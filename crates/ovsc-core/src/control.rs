//! Control-plane servers: ARC (4440), CMC (8800) and conmon (8700 + the
//! 8702/8708 multicasts).

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};

use ovsc_clock::ClockState;
use ovsc_proto::arc::{self, ChannelCounts, DeviceNames, RxChannelInfo, opcode};
use ovsc_proto::cmc::{self, DeviceAdvertisement};
use ovsc_proto::conmon::{self, ConmonHeader, Heartbeat, InterfaceTraffic, notification};
use ovsc_proto::discovery;
use ovsc_proto::frame::{Frame, respond, result};

use crate::buffer::TimedRing;
use crate::info::{
    BOARD_NAME, MANUFACTURER, MAX_CHANNELS_PER_TX_FLOW, MAX_RX_FLOWS, MAX_TX_FLOWS, MODEL_NAME,
    REVISION,
};
use crate::net::{InterfaceCounters, counter_delta, interface_counters};
use crate::state::{FlowStats, FormatRequest, Notify, Shared, Subscription};

const MTU: usize = 2048;

/// The smallest receive latency offered to controllers: their latency menus
/// list the values between this and [`MAX_LATENCY_NS`].
const MIN_LATENCY_NS: u32 = 1_000_000;
const MAX_LATENCY_NS: u32 = (crate::config::MAX_LATENCY_MS * 1e6) as u32;

// ---------------------------------------------------------------------------
// ARC
// ---------------------------------------------------------------------------

pub async fn run_arc(shared: Arc<Shared>, socket: UdpSocket) {
    let mut buf = vec![0u8; MTU];
    loop {
        let (len, src) = match socket.recv_from(&mut buf).await {
            Ok(r) => r,
            Err(e) => {
                debug!("ARC receive error: {e}");
                continue;
            }
        };
        let Ok(frame) = Frame::parse(&buf[..len]) else {
            debug!("malformed ARC packet from {src}");
            continue;
        };
        if let Some(resp) = handle_arc(&shared, &frame, src) {
            if let Err(e) = socket.send_to(&resp, src).await {
                debug!("ARC send to {src} failed: {e}");
            }
        }
    }
}

/// The device properties a controller reads ([`opcode::PROPERTIES_1100`]):
/// the ones `asked` for, in that order, or all known ones.
fn read_properties(shared: &Shared, asked: Option<&[u16]>) -> Vec<(u16, Option<u32>)> {
    use arc::property::*;
    let info = &shared.info;
    let known = [
        (SAMPLE_RATE, info.sample_rate),
        (DEFAULT_LATENCY, shared.configured_latency_ns()),
        (CONFIGURED_LATENCY, shared.configured_latency_ns()),
        (RX_LATENCY, info.latency.ns()),
        (MAX_LATENCY, MAX_LATENCY_NS),
        (MIN_LATENCY, MIN_LATENCY_NS),
    ];
    let value = |id: u16| known.iter().find(|(k, _)| *k == id).map(|(_, v)| *v);
    match asked {
        Some(ids) => ids.iter().map(|&id| (id, value(id))).collect(),
        None => known.iter().map(|&(id, v)| (id, Some(v))).collect(),
    }
}

/// Whether the show lock refuses the controller at `from` the change `what`
/// (see [`crate::state::State::remote_settings_locked`]). A refusal is
/// logged and listed with the device's remote changes, so that the owner
/// learns why a controller's change did not take.
fn refused_by_lock(shared: &Shared, from: SocketAddr, what: impl FnOnce() -> String) -> bool {
    if !shared.remote_settings_locked() {
        return false;
    }
    let what = what();
    warn!("refusing {what} from the controller at {from}: remote settings are locked");
    shared.record_remote_change(from, what, true);
    true
}

/// Notes a change the controller at `from` made.
fn changed_by(shared: &Shared, from: SocketAddr, what: String) {
    info!("the controller at {from} changed {what}");
    shared.record_remote_change(from, what, false);
}

/// A controller writing device properties ([`opcode::PROPERTIES_1101`]):
/// the configured or receive latency is saved, for the device's owner to
/// apply to the receive side ([`Shared::restart_rx`]), unless the show lock
/// refuses it. The answer echoes the records applied; a refusal is an error
/// result, as for a value out of range (docs/PROTOCOL.md §4.6.3: the real
/// reply to 0x1101 is unknown).
fn write_properties(shared: &Shared, frame: &Frame<'_>, from: SocketAddr) -> Vec<u8> {
    use arc::property::*;
    let h = &frame.header;
    let Ok(records) = arc::decode_write_properties_request(frame) else {
        return respond(h, result::ERROR, &[]);
    };
    let latency = records
        .iter()
        .find(|(id, v)| matches!(*id, CONFIGURED_LATENCY | RX_LATENCY) && v.is_some())
        .and_then(|(_, v)| *v);
    let Some(ns) = latency else {
        debug!("unhandled property write {records:?}");
        return respond(h, result::ERROR, &[]);
    };
    if !(MIN_LATENCY_NS..=MAX_LATENCY_NS).contains(&ns) {
        warn!("refusing a latency of {} ms", ns as f64 / 1e6);
        return respond(h, result::ERROR, &[]);
    }
    let applied: Vec<(u16, Option<u32>)> = records
        .iter()
        .filter_map(|&(id, v)| match id {
            CONFIGURED_LATENCY | RX_LATENCY => Some((id, Some(ns))),
            UNICAST_FPP | RX_FPP => Some((id, v)),
            _ => None,
        })
        .collect();
    // Writing the latency the device is already set to changes nothing, so
    // the lock lets it through: a controller applying a page of settings at
    // once then gets no error for a field nobody touched.
    let unchanged = ns == shared.configured_latency_ns();
    if unchanged && shared.remote_settings_locked() {
        return arc::encode_properties_response(h, &applied);
    }
    let what = || format!("the receive latency to {} ms", ns as f64 / 1e6);
    if refused_by_lock(shared, from, what) {
        return respond(h, result::ERROR, &[]);
    }
    if let Err(e) = shared.request_latency(ns) {
        warn!("refusing a latency of {} ms: {e}", ns as f64 / 1e6);
        return respond(h, result::ERROR, &[]);
    }
    if !unchanged {
        // Applied by the daemon to the receive side only: the receive flows
        // are requested again, the transmit flows other devices receive
        // from this one go on (docs/MACOS.md, "Setting the latency from
        // Dante Controller or the app").
        changed_by(shared, from, what());
    }
    arc::encode_properties_response(h, &applied)
}

/// Handles one ARC request from `from` and returns the response, if any.
pub fn handle_arc(shared: &Shared, frame: &Frame<'_>, from: SocketAddr) -> Option<Vec<u8>> {
    let h = &frame.header;
    if h.result != result::REQUEST {
        trace!("ignoring ARC packet with result code {:#x}", h.result);
        return None;
    }
    let info = &shared.info;
    let fmt = info.channel_format();
    let paged_start = || arc::decode_paged_request(frame).ok();
    let resp = match h.opcode {
        opcode::CHANNEL_COUNTS => ChannelCounts {
            tx_channels: info.tx_channels.len() as u16,
            rx_channels: info.rx_channels.len() as u16,
            max_channels_per_flow: MAX_CHANNELS_PER_TX_FLOW
                .min(info.tx_channels.len() as u16)
                .max(1),
            max_tx_flows: MAX_TX_FLOWS as u16,
            max_rx_flows: MAX_RX_FLOWS as u16,
            supports_tx_rename: true,
            supports_tx_multicast: false,
            network_interfaces: 1 + info.secondary.is_some() as u16,
        }
        .encode_response(h),
        opcode::DEVICE_NAME => arc::encode_device_name_response(h, &shared.name()),
        opcode::DEVICE_INFO => DeviceNames {
            friendly_name: shared.name(),
            factory_name: info.factory_name.clone(),
            board_name: BOARD_NAME.into(),
            revision: REVISION.into(),
        }
        .encode_response(h),
        opcode::SET_DEVICE_NAME => {
            let name = arc::decode_set_device_name_request(frame).ok()?;
            let name = name.unwrap_or_else(|| info.factory_name.clone());
            // Receivers subscribe by `channel@device`: a new device name
            // leaves their subscriptions to this device unresolved.
            let what = || format!("the device name to {name}");
            if discovery::validate_device_name(&name).is_err() {
                warn!("refusing invalid device name {name:?}");
                respond(h, result::ERROR, &[])
            } else if name == shared.name() {
                arc::encode_device_name_response(h, &name)
            } else if refused_by_lock(shared, from, what) {
                respond(h, result::ERROR, &[])
            } else {
                changed_by(shared, from, what());
                shared.modify(|s| s.name = name.clone());
                arc::encode_device_name_response(h, &name)
            }
        }
        opcode::TX_CHANNELS => {
            arc::encode_tx_channels_response(h, paged_start()?, fmt, &info.tx_channels)
        }
        opcode::TX_CHANNEL_NAMES => {
            let names = shared.state().tx_names.clone();
            arc::encode_tx_channel_names_response(h, paged_start()?, &names)
        }
        opcode::RX_CHANNELS => {
            let channels = rx_channel_list(shared);
            arc::encode_rx_channels_response(h, paged_start()?, fmt, &channels)
        }
        opcode::RENAME_TX_CHANNELS => {
            let renames = arc::decode_rename_tx_channels_request(frame).ok()?;
            // Subscriptions made by a channel's previous name stop
            // resolving (those by its factory name keep working).
            let renaming = renames_anything(shared, &renames, true);
            if renaming && refused_by_lock(shared, from, || renames_words(&renames, true)) {
                return Some(respond(h, result::ERROR, &[]));
            }
            let changed = rename(shared, &renames, true);
            // Peers learn about transmit renames from mDNS.
            if changed.is_empty() {
                respond(h, result::ERROR, &[])
            } else {
                if renaming {
                    changed_by(shared, from, renames_words(&renames, true));
                }
                respond(h, result::SUCCESS, &[0, 0])
            }
        }
        opcode::RENAME_RX_CHANNELS => {
            let renames = arc::decode_rename_rx_channels_request(frame).ok()?;
            // Harmless to audio, but locked with the other names: a show's
            // input labels stay as set up.
            if renames_anything(shared, &renames, false)
                && refused_by_lock(shared, from, || renames_words(&renames, false))
            {
                return Some(respond(h, result::ERROR, &[]));
            }
            let changed = rename(shared, &renames, false);
            if changed.is_empty() {
                respond(h, result::ERROR, &[])
            } else {
                shared.notify(Notify::RxChannelsChanged(changed));
                respond(h, result::SUCCESS, &[])
            }
        }
        opcode::SET_SUBSCRIPTIONS => {
            let subs = arc::decode_set_subscriptions_request(frame).ok()?;
            let own_name = shared.name();
            let mut changed = Vec::new();
            shared.modify(|s| {
                for sub in subs {
                    let Some(slot) = (sub.rx_channel as usize)
                        .checked_sub(1)
                        .and_then(|i| s.subscriptions.get_mut(i))
                    else {
                        warn!("subscription for unknown rx channel {}", sub.rx_channel);
                        continue;
                    };
                    *slot = sub.source.map(|(tx_channel, tx_device)| {
                        let tx_device = if tx_device == "." { own_name.clone() } else { tx_device };
                        info!("rx {} <- {tx_channel}@{tx_device}", sub.rx_channel);
                        Subscription {
                            tx_channel,
                            tx_device,
                            status: arc::SubscriptionStatus::Unresolved,
                        }
                    });
                    changed.push(sub.rx_channel as usize - 1);
                }
            });
            shared.notify(Notify::RxChannelsChanged(changed));
            respond(h, result::SUCCESS, &[])
        }
        opcode::REMOVE_SUBSCRIPTIONS => {
            let channels = arc::decode_remove_subscriptions_request(frame).ok()?;
            let changed: Vec<usize> = channels
                .iter()
                .filter_map(|&c| (c as usize).checked_sub(1))
                .filter(|&i| i < info.rx_channels.len())
                .collect();
            shared.modify(|s| {
                for &i in &changed {
                    info!("rx {} unsubscribed", i + 1);
                    s.subscriptions[i] = None;
                }
            });
            shared.notify(Notify::RxChannelsChanged(changed));
            respond(h, result::SUCCESS, &[])
        }
        opcode::TX_FLOWS => {
            let flows = shared.state().tx_flows.clone();
            arc::encode_tx_flows_response(
                h,
                paged_start()?,
                info.sample_rate,
                info.format.bits(),
                &flows,
            )
        }
        opcode::RX_FLOWS => {
            let flows = shared.state().rx_flows.clone();
            arc::encode_rx_flows_response(
                h,
                paged_start()?,
                info.sample_rate,
                info.format.bits(),
                info.rx_channels.len(),
                &flows,
            )
        }
        opcode::CREATE_MULTICAST_TX_FLOW | opcode::DELETE_TX_FLOWS => {
            // Multicast transmit flows are not implemented yet; the channel
            // count response says so, but answer anyway.
            respond(h, result::ERROR, &[])
        }
        opcode::UNKNOWN_2320 => respond(h, result::FRONTEND_UNAVAILABLE, &[]),
        // No transmit flow has a label: an empty page.
        opcode::TX_FLOW_LABELS => respond(h, result::SUCCESS, &[2, 0]),
        opcode::PROPERTIES_1100 => {
            let asked = arc::decode_read_properties_request(frame).ok()?;
            arc::encode_properties_response(h, &read_properties(shared, asked.as_deref()))
        }
        opcode::PROPERTIES_1101 => write_properties(shared, frame, from),
        opcode::PROPERTIES_1102 => arc::encode_properties_1102_response(h),
        opcode::RX_PORT_RANGES => arc::encode_rx_port_ranges_response(h),
        other => {
            debug!(
                "unhandled ARC opcode {other:#06x} (protocol {:#06x}): {}",
                h.protocol,
                hex::encode(frame.packet)
            );
            return None;
        }
    };
    Some(resp)
}

fn rx_channel_list(shared: &Shared) -> Vec<RxChannelInfo> {
    let s = shared.state();
    s.rx_names
        .iter()
        .zip(&s.subscriptions)
        .enumerate()
        .map(|(i, (name, sub))| RxChannelInfo {
            id: i as u16 + 1,
            name: name.clone(),
            subscription: sub.as_ref().map(|s| (s.tx_channel.clone(), s.tx_device.clone())),
            status: sub.as_ref().map_or(arc::SubscriptionStatus::None, |s| s.status),
        })
        .collect()
}

/// Whether `renames` would change a channel's name; an empty name restores
/// the factory name. Entries for unknown channels change nothing.
fn renames_anything(shared: &Shared, renames: &[(u16, String)], tx: bool) -> bool {
    let factory = if tx { &shared.info.tx_channels } else { &shared.info.rx_channels };
    let s = shared.state();
    let names = if tx { &s.tx_names } else { &s.rx_names };
    renames.iter().any(|(id, name)| {
        (*id as usize).checked_sub(1).filter(|&i| i < factory.len()).is_some_and(|i| {
            let name = if name.is_empty() { &factory[i] } else { name };
            names[i] != *name
        })
    })
}

/// Channel renames in words, for the log and the app.
fn renames_words(renames: &[(u16, String)], tx: bool) -> String {
    let kind = if tx { "transmit" } else { "receive" };
    match renames {
        [(id, name)] if name.is_empty() => {
            format!("the name of {kind} channel {id} back to its factory name")
        }
        [(id, name)] => format!("the name of {kind} channel {id} to {name}"),
        _ => format!("the names of {} {kind} channels", renames.len()),
    }
}

/// Applies channel renames; an empty name restores the factory name.
/// Returns the 0-based indices that changed.
fn rename(shared: &Shared, renames: &[(u16, String)], tx: bool) -> Vec<usize> {
    let factory = if tx { &shared.info.tx_channels } else { &shared.info.rx_channels };
    let mut changed = Vec::new();
    shared.modify(|s| {
        for (id, name) in renames {
            let Some(i) = (*id as usize).checked_sub(1).filter(|&i| i < factory.len()) else {
                warn!("rename of unknown channel {id}");
                continue;
            };
            let name = if name.is_empty() { factory[i].clone() } else { name.clone() };
            let names = if tx { &mut s.tx_names } else { &mut s.rx_names };
            let duplicate = names.iter().enumerate().any(|(j, n)| j != i && n == &name);
            if discovery::validate_channel_name(&name).is_err() || duplicate {
                warn!("refusing channel name {name:?}");
                continue;
            }
            info!("{} channel {id} renamed to {name}", if tx { "tx" } else { "rx" });
            names[i] = name;
            changed.push(i);
        }
    });
    changed
}

// ---------------------------------------------------------------------------
// CMC
// ---------------------------------------------------------------------------

pub async fn run_cmc(shared: Arc<Shared>, socket: UdpSocket) {
    let mut buf = vec![0u8; MTU];
    loop {
        let Ok((len, src)) = socket.recv_from(&mut buf).await else { continue };
        let Ok(frame) = Frame::parse(&buf[..len]) else { continue };
        let resp = match frame.header.opcode {
            cmc::opcode::DEVICE_ADVERTISEMENT => DeviceAdvertisement {
                process_id: shared.info.process_id,
                device_id: shared.info.device_id,
                ip: shared.info.iface.ip,
                info_port: shared.info.ports.settings,
            }
            .encode_response(&frame.header),
            other => {
                debug!("unhandled CMC opcode {other:#06x}: {}", hex::encode(frame.packet));
                continue;
            }
        };
        let _ = socket.send_to(&resp, src).await;
    }
}

// ---------------------------------------------------------------------------
// Conmon
// ---------------------------------------------------------------------------

struct Conmon {
    shared: Arc<Shared>,
    socket: UdpSocket,
    /// Where statuses go: the info multicast group (a local socket in
    /// tests).
    info_dest: SocketAddr,
    seq: u16,
    /// The clock state last reported, to report changes.
    sync: Option<conmon::Sync>,
    /// Each interface's counters at the last heartbeat, and when: the
    /// primary's, then the secondary's.
    traffic: [Option<(InterfaceCounters, std::time::Instant)>; 2],
    /// Each interface's error counters when the device started.
    errors_base: [Option<InterfaceCounters>; 2],
}

impl Conmon {
    async fn send(&mut self, start_code: u16, opcode: [u8; 8], content: &[u8], dest: SocketAddr) {
        let header = ConmonHeader {
            start_code,
            seq: self.seq,
            // Always 0 on real devices; netaudio drops messages with any
            // other value.
            process_id: 0,
            device_id: self.shared.info.device_id,
            vendor: conmon::VENDOR_ID,
            opcode,
        };
        self.seq = self.seq.wrapping_add(1);
        if let Err(e) = self.socket.send_to(&header.encode(content), dest).await {
            trace!("conmon send to {dest} failed: {e}");
        }
    }

    async fn status(&mut self, id: u16, content: &[u8]) {
        let dest = self.info_dest;
        self.send(conmon::START_INFO, conmon::status_opcode(id), content, dest).await;
    }

    /// Answers request `id` with content `body`, from the controller at
    /// `from` (`None`: the device announcing itself).
    async fn answer(&mut self, id: u16, body: &[u8], from: Option<SocketAddr>) {
        let info = self.shared.info.clone();
        let iface = &info.iface;
        // While locked, the statuses say the format is not writable, and the
        // capabilities that it is not configurable, so that a controller
        // shows it as fixed rather than as a change that failed.
        let writable = self.shared.format_configurable && !self.shared.remote_settings_locked();
        match id {
            notification::VERSIONS_QUERY => {
                let mut capabilities = conmon::capability::MANUFACTURER_NAME;
                if writable {
                    capabilities |= conmon::capability::SAMPLE_RATE | conmon::capability::ENCODING;
                }
                let c = conmon::versions_status(BOARD_NAME, capabilities);
                self.status(notification::VERSIONS_STATUS, &c).await
            }
            notification::SAMPLE_RATE_QUERY => {
                if let (Some(rate), Some(from)) = (conmon::decode_configurable_request(body), from)
                {
                    let request = FormatRequest { sample_rate: Some(rate), ..Default::default() };
                    self.request_format(request, from, info.sample_rate != rate);
                }
                let pending = self.shared.pending_format().sample_rate;
                let c = conmon::configurable_status(
                    info.sample_rate,
                    pending.filter(|&r| r != info.sample_rate).unwrap_or(0),
                    writable,
                    &crate::config::SAMPLE_RATES,
                );
                self.status(notification::SAMPLE_RATE_STATUS, &c).await
            }
            notification::ENCODING_QUERY => {
                let current = u32::from(info.format.bits());
                if let (Some(bits), Some(from)) = (conmon::decode_configurable_request(body), from)
                {
                    let request = FormatRequest {
                        bits_per_sample: Some(bits.min(u32::from(u16::MAX)) as u16),
                        ..Default::default()
                    };
                    self.request_format(request, from, bits != current);
                }
                let pending = self.shared.pending_format().bits_per_sample.map(u32::from);
                let values: Vec<u32> =
                    crate::config::BITS_PER_SAMPLE.iter().map(|&b| u32::from(b)).collect();
                let c = conmon::configurable_status(
                    current,
                    pending.filter(|&b| b != current).unwrap_or(0),
                    writable,
                    &values,
                );
                self.status(notification::ENCODING_STATUS, &c).await
            }
            notification::MANUFACTURER_VERSIONS_QUERY => {
                let c = conmon::manufacturer_versions_status(
                    MANUFACTURER,
                    BOARD_NAME,
                    MODEL_NAME,
                    info.version,
                );
                self.status(notification::MANUFACTURER_VERSIONS_STATUS, &c).await
            }
            notification::INTERFACE_QUERY => {
                let addrs = |i: &crate::net::Interface| conmon::InterfaceAddrs {
                    mac: i.mac,
                    ip: i.ip,
                    netmask: i.netmask,
                    gateway: i.gateway,
                };
                let all: Vec<_> =
                    std::iter::once(iface).chain(&info.secondary).map(addrs).collect();
                let c = conmon::interface_status(iface.link_speed_mbps, &all);
                self.status(notification::INTERFACE_STATUS, &c).await
            }
            notification::CLOCKING_QUERY => self.clocking().await,
            notification::CLEAR_CONFIG_QUERY => {
                self.status(notification::CLEAR_CONFIG_STATUS, &conmon::clear_config_status()).await
            }
            other => debug!("unhandled conmon query {other:#06x}"),
        }
    }

    /// Tells controllers what another task of the device changed.
    async fn notified(&mut self, n: Notify) {
        match n {
            Notify::RxChannelsChanged(channels) => {
                self.status(notification::RX_CHANNEL_CHANGE, &conmon::channel_change(channels))
                    .await;
            }
            // Controllers ask for these statuses when they first see the
            // device, not when the lock changes what they say. Statuses
            // double as notifications, multicast to every controller: send
            // them unasked, as at start-up (docs/PROTOCOL.md §7.2, §7.3).
            Notify::RemoteSettingsLockChanged => {
                for query in [
                    notification::VERSIONS_QUERY,
                    notification::SAMPLE_RATE_QUERY,
                    notification::ENCODING_QUERY,
                ] {
                    self.answer(query, &[], None).await;
                }
            }
        }
    }

    /// Applies the format request of the controller at `from`, or says why
    /// not. `changes` says whether it differs from the running format: the
    /// lock lets a request for the running format through, as it changes
    /// nothing. A refusal has no reply of its own (conmon answers with the
    /// status, which shows the format unchanged).
    fn request_format(&self, request: FormatRequest, from: SocketAddr, changes: bool) {
        let what = || match request {
            FormatRequest { sample_rate: Some(rate), .. } => {
                format!("the sample rate to {rate} Hz")
            }
            FormatRequest { bits_per_sample: Some(bits), .. } => {
                format!("the encoding to {bits}-bit")
            }
            _ => format!("the format to {request:?}"),
        };
        if !changes && self.shared.remote_settings_locked() {
            return;
        }
        if refused_by_lock(&self.shared, from, what) {
            return;
        }
        match self.shared.request_format(request) {
            Ok(()) if changes => changed_by(&self.shared, from, what()),
            Ok(()) => info!("the controller at {from} asked for {request:?}"),
            Err(e) => warn!("refusing {request:?} from the controller at {from}: {e}"),
        }
    }

    /// Interface `net`'s traffic (0 the primary, 1 the secondary) since the
    /// last call, per second, and its errors since the device started.
    fn traffic(&mut self, net: usize) -> Option<InterfaceTraffic> {
        let info = &self.shared.info;
        let iface = if net == 0 { Some(&info.iface) } else { info.secondary.as_ref() }?;
        let now = interface_counters(&iface.name)?;
        let at = std::time::Instant::now();
        let base = *self.errors_base[net].get_or_insert(now);
        let previous = self.traffic[net].replace((now, at));
        let (before, then) = previous?;
        let secs = at.duration_since(then).as_secs_f64().max(0.001);
        let rate =
            |b: u64, n: u64| (counter_delta(b, n) as f64 / secs).min(f64::from(u32::MAX)) as u32;
        let since = |b: u64, n: u64| counter_delta(b, n).min(u64::from(u32::MAX)) as u32;
        Some(InterfaceTraffic {
            tx_bytes_per_s: rate(before.tx_bytes, now.tx_bytes),
            rx_bytes_per_s: rate(before.rx_bytes, now.rx_bytes),
            tx_errors: since(base.tx_errors, now.tx_errors),
            rx_errors: since(base.rx_errors, now.rx_errors),
        })
    }

    /// Reports the clock's state (`CLOCKING_STATUS`).
    async fn clocking(&mut self) {
        let status = self.shared.clock.status();
        let sync = clock_sync(status.state);
        self.sync = Some(sync);
        let master = status.master.map_or([0; 8], |m| conmon::eui64_from_mac(m.uuid));
        let c = conmon::clocking_status(
            sync,
            status.freq_offset_ppb.round() as i32,
            self.shared.info.iface.mac,
            master,
        );
        self.status(notification::CLOCKING_STATUS, &c).await
    }

    async fn heartbeat(&mut self) {
        // Controllers show the clock's state as it changes, not only when
        // they ask.
        let sync = clock_sync(self.shared.clock.status().state);
        if self.sync.is_some_and(|s| s != sync) {
            self.clocking().await;
        }
        let seq = self.seq;
        let rate = self.shared.info.sample_rate;
        let mut hb = Heartbeat::new();
        let snap = self.shared.clock.snapshot();
        if let Some(snap) = &snap {
            hb = hb.clock(seq, snap.freq_offset_ppb().round() as i32);
        }
        if let Some(primary) = self.traffic(0) {
            // A device on two networks reports both, the secondary as zeros
            // while its counters are unavailable, as an LX-DANTE with its
            // secondary link down does.
            hb = match self.shared.info.secondary {
                Some(_) => {
                    hb.interface_traffic(seq, &[primary, self.traffic(1).unwrap_or_default()])
                }
                None => hb.interface_traffic(seq, &[primary]),
            };
        }
        let now = self.shared.clock.now_samples(rate);
        let window = (rate / 4) as usize;
        hb = hb.levels(
            seq,
            &peak_levels(&self.shared.tx_rings, now, window),
            &peak_levels(&self.shared.rx_rings, now, window),
        );
        if snap.is_some() && !self.shared.rx_rings.is_empty() {
            let stats = self.shared.state().rx_flow_stats.clone();
            let (arrivals, late) = match self.shared.info.secondary {
                None => {
                    (flow_arrivals(&stats, MAX_RX_FLOWS), flow_late_packets(&stats, MAX_RX_FLOWS))
                }
                // Each flow on each network: the primary's flows, then the
                // secondary's (PROTOCOL.md §7.6, 0x8003).
                Some(_) => (0..2).fold((Vec::new(), Vec::new()), |(mut a, mut l), net| {
                    a.extend(network_arrivals(&stats, MAX_RX_FLOWS, net));
                    l.extend(network_late_packets(&stats, MAX_RX_FLOWS, net));
                    (a, l)
                }),
            };
            hb = hb.rx_latency(seq, rate, &arrivals).late_packets(seq, &late);
        }
        let dest = SocketAddr::from((conmon::HEARTBEAT_GROUP, conmon::HEARTBEAT_PORT));
        self.send(conmon::START_HEARTBEAT, conmon::HEARTBEAT_OPCODE, &hb.into_content(), dest)
            .await;
    }
}

/// The peak level of each ring over the `window` samples up to `now`, for
/// the heartbeat's meters; silence without a clock.
fn peak_levels(rings: &[Arc<TimedRing>], now: Option<u64>, window: usize) -> Vec<u8> {
    let Some(now) = now else { return vec![conmon::level_byte(0); rings.len()] };
    let mut buf = vec![0; window];
    rings
        .iter()
        .map(|ring| {
            ring.read(now.saturating_sub(window as u64), &mut buf);
            conmon::level_byte(buf.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0))
        })
        .collect()
}

/// The clock's state as conmon reports it.
fn clock_sync(state: ClockState) -> conmon::Sync {
    match state {
        ClockState::Locked => conmon::Sync::Locked,
        ClockState::Locking => conmon::Sync::Locking,
        ClockState::Unlocked => conmon::Sync::Unlocked,
        ClockState::FreeRunning => conmon::Sync::FreeRunning,
    }
}

/// The longest arrival time of each receive flow's packets since the last
/// call, in samples, indexed by flow id - 1 for all `capacity` flows (0 for
/// no flow or no packet).
fn flow_arrivals(stats: &[(u16, Arc<FlowStats>)], capacity: usize) -> Vec<u32> {
    per_flow(stats, capacity, |s| s.max_arrival.swap(0, Ordering::Relaxed))
}

/// The late packets each receive flow counted so far, indexed like
/// [`flow_arrivals`].
fn flow_late_packets(stats: &[(u16, Arc<FlowStats>)], capacity: usize) -> Vec<u32> {
    per_flow(stats, capacity, |s| {
        s.late_packets.load(Ordering::Relaxed).min(u64::from(u32::MAX)) as u32
    })
}

/// [`flow_arrivals`] on network `net` alone.
fn network_arrivals(stats: &[(u16, Arc<FlowStats>)], capacity: usize, net: usize) -> Vec<u32> {
    per_flow(stats, capacity, |s| s.networks[net].max_arrival.swap(0, Ordering::Relaxed))
}

/// [`flow_late_packets`] on network `net` alone.
fn network_late_packets(stats: &[(u16, Arc<FlowStats>)], capacity: usize, net: usize) -> Vec<u32> {
    per_flow(stats, capacity, |s| {
        s.networks[net].late_packets.load(Ordering::Relaxed).min(u64::from(u32::MAX)) as u32
    })
}

fn per_flow(
    stats: &[(u16, Arc<FlowStats>)],
    capacity: usize,
    value: impl Fn(&FlowStats) -> u32,
) -> Vec<u32> {
    let mut out = vec![0; capacity];
    for (id, s) in stats {
        if let Some(slot) = usize::from(*id).checked_sub(1).and_then(|i| out.get_mut(i)) {
            *slot = value(s);
        }
    }
    out
}

pub async fn run_conmon(
    shared: Arc<Shared>,
    socket: UdpSocket,
    mut notify: mpsc::UnboundedReceiver<Notify>,
) {
    let mut c = Conmon {
        shared,
        socket,
        info_dest: SocketAddr::from((conmon::INFO_GROUP, conmon::INFO_PORT)),
        seq: 1,
        sync: None,
        traffic: [None; 2],
        errors_base: [None; 2],
    };
    c.answer(notification::VERSIONS_QUERY, &[], None).await;
    c.answer(notification::MANUFACTURER_VERSIONS_QUERY, &[], None).await;
    let mut heartbeat = tokio::time::interval(Duration::from_secs(1));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut buf = vec![0u8; MTU];
    loop {
        tokio::select! {
            r = c.socket.recv_from(&mut buf) => {
                let Ok((len, src)) = r else { continue };
                match ConmonHeader::decode(&buf[..len]) {
                    Ok((h, body)) => match conmon::opcode_id(&h.opcode) {
                        Some(id) => c.answer(id, body, Some(src)).await,
                        None => debug!("conmon request with unknown opcode {} from {src}",
                                       hex::encode(h.opcode)),
                    },
                    Err(e) => debug!("malformed conmon packet from {src}: {e}"),
                }
            }
            n = notify.recv() => match n {
                Some(n) => c.notified(n).await,
                None => return,
            },
            _ = heartbeat.tick() => c.heartbeat().await,
        }
    }
}

/// Discards notifications when conmon isn't running.
pub async fn drain_notifications(mut notify: mpsc::UnboundedReceiver<Notify>) {
    while notify.recv().await.is_some() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_latencies_follow_flow_ids_and_restart_each_beat() {
        let a = Arc::new(FlowStats::default());
        let b = Arc::new(FlowStats::default());
        a.max_arrival.store(40, Ordering::Relaxed);
        b.max_arrival.store(55, Ordering::Relaxed);
        a.late_packets.store(3, Ordering::Relaxed);
        // Flows 1 and 3, of 4 the device could have.
        let stats = vec![(1, a.clone()), (3, b.clone())];
        assert_eq!(flow_arrivals(&stats, 4), [40, 0, 55, 0]);
        // Each heartbeat reports the longest since the previous one, and the
        // late packets so far.
        assert_eq!(flow_arrivals(&stats, 4), [0, 0, 0, 0]);
        assert_eq!(flow_late_packets(&stats, 4), [3, 0, 0, 0]);
        a.max_arrival.fetch_max(12, Ordering::Relaxed);
        assert_eq!(flow_arrivals(&stats, 4), [12, 0, 0, 0]);
        assert_eq!(flow_arrivals(&[], 2), [0, 0]);
        // A flow beyond the capacity has no entry.
        assert_eq!(flow_arrivals(&[(9, a)], 2), [0, 0]);
    }

    #[test]
    fn meters_show_the_recent_peak_of_each_ring() {
        let loud = Arc::new(TimedRing::new(1024));
        let quiet = Arc::new(TimedRing::new(1024));
        let silent = Arc::new(TimedRing::new(1024));
        loud.write(900, &[1 << 30, -(1 << 30) - 5, 7]);
        quiet.write(990, &[1 << 20]);
        // Older than the window: not shown.
        silent.write(100, &[i32::MAX]);
        let rings = [loud, quiet, silent];
        assert_eq!(peak_levels(&rings, Some(1000), 200), [12, 132, 0xff]);
        assert_eq!(peak_levels(&rings, None, 200), [0xff; 3]);
    }

    #[test]
    fn clock_states_map_to_conmon() {
        assert_eq!(clock_sync(ClockState::Locked), conmon::Sync::Locked);
        assert_eq!(clock_sync(ClockState::Unlocked), conmon::Sync::Unlocked);
    }

    use std::net::Ipv4Addr;

    use crate::config::{Channels, DeviceConfig};
    use crate::info::DeviceInfo;
    use crate::net::Interface;
    use crate::state::{RemoteChanges, State};

    /// A device's shared state at 4 ms, 2 + 2 channels, named `desk-mac`,
    /// with the show lock `locked`, without its servers.
    fn shared(locked: bool) -> Arc<Shared> {
        shared_with_notifications(locked).0
    }

    /// [`shared`], and the notifications meant for the conmon task.
    fn shared_with_notifications(locked: bool) -> (Arc<Shared>, mpsc::UnboundedReceiver<Notify>) {
        let config = DeviceConfig {
            tx_channels: Channels::Count(2),
            rx_channels: Channels::Count(2),
            latency_ms: 4.0,
            lock_remote_settings: locked,
            ..Default::default()
        };
        let iface = Interface {
            name: "lo".into(),
            ip: Ipv4Addr::LOCALHOST,
            netmask: Ipv4Addr::new(255, 0, 0, 0),
            gateway: Ipv4Addr::UNSPECIFIED,
            mac: [0; 6],
            link_speed_mbps: 1000,
        };
        let info = DeviceInfo::new(&config, iface).unwrap();
        let state = State {
            name: "desk-mac".into(),
            tx_names: info.tx_channels.clone(),
            rx_names: info.rx_channels.clone(),
            subscriptions: vec![None; 2],
            tx_flows: Vec::new(),
            rx_flows: Vec::new(),
            rx_flow_stats: Vec::new(),
            latency_override_ns: None,
            remote_settings_locked: locked,
            remote_settings_lock_set: None,
            name_conflict: None,
            announced: None,
        };
        let clock = ovsc_clock::system_clock();
        Shared::new(info, state, clock, 1024, None, true, RemoteChanges::default()).unwrap()
    }

    /// A conmon server for `shared` that sends its statuses to the returned
    /// socket rather than to the multicast group.
    async fn local_conmon(shared: &Arc<Shared>) -> (Conmon, std::net::UdpSocket) {
        let listener = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let c = Conmon {
            shared: shared.clone(),
            socket: UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap(),
            info_dest: listener.local_addr().unwrap(),
            seq: 1,
            sync: None,
            traffic: [None; 2],
            errors_base: [None; 2],
        };
        (c, listener)
    }

    /// Whether each of the next `n` statuses `listener` receives says that
    /// controllers may set the format, by status id: the capability bits of
    /// 0x0060, the update mode of 0x0080 and 0x0082 (docs/PROTOCOL.md
    /// §7.4.5, §7.4.7).
    fn format_settable(listener: &std::net::UdpSocket, n: usize) -> Vec<(u16, bool)> {
        let mut buf = [0u8; MTU];
        let format = conmon::capability::SAMPLE_RATE | conmon::capability::ENCODING;
        (0..n)
            .map(|_| {
                let len = listener.recv(&mut buf).expect("a status");
                let (h, c) = ConmonHeader::decode(&buf[..len]).unwrap();
                let id = conmon::opcode_id(&h.opcode).unwrap();
                let settable = match id {
                    notification::VERSIONS_STATUS => {
                        let caps = u32::from_be_bytes(c[0x14..0x18].try_into().unwrap());
                        assert!(caps & format == 0 || caps & format == format, "{caps:#x}");
                        caps & format == format
                    }
                    _ => u16::from_be_bytes([c[12], c[13]]) == 2,
                };
                (id, settable)
            })
            .collect()
    }

    /// Dante Controller's address in these tests.
    fn controller() -> SocketAddr {
        SocketAddr::from(([192, 168, 1, 20], 51_000))
    }

    /// The result code of the ARC answer to `request`.
    fn ask(shared: &Shared, request: Vec<u8>) -> u16 {
        let frame = Frame::parse(&request).unwrap();
        let answer = handle_arc(shared, &frame, controller()).expect("an answer");
        Frame::parse(&answer).unwrap().header.result
    }

    /// The remote changes noted so far, as `(refused, what)`.
    fn noted(shared: &Shared) -> Vec<(bool, String)> {
        shared.remote_changes.recent().into_iter().map(|c| (c.refused, c.what)).collect()
    }

    #[test]
    fn the_show_lock_refuses_a_controllers_latency() {
        let shared = shared(true);
        let requests = shared.latency_requests();
        // Dante Controller's own 0x1101 packet for 2 ms: refused with an
        // error result, and nothing is asked of the daemon.
        let set = |ns| arc::encode_set_latency_request(1, ns, 16);
        assert_eq!(ask(&shared, set(2_000_000)), result::ERROR);
        assert_eq!(shared.configured_latency_ns(), 4_000_000);
        assert!(!requests.has_changed().unwrap());
        assert_eq!(noted(&shared), [(true, "the receive latency to 2 ms".to_owned())]);
        assert_eq!(shared.remote_changes.recent()[0].from, controller());
        // The latency it already has changes nothing: no error, no request.
        assert_eq!(ask(&shared, set(4_000_000)), result::SUCCESS);
        assert!(!requests.has_changed().unwrap());
        assert_eq!(noted(&shared).len(), 1);

        // Unlocked, the controller sets it, for the daemon to apply to the
        // receive side.
        shared.lock_remote_settings(false);
        assert_eq!(ask(&shared, set(2_000_000)), result::SUCCESS);
        assert_eq!(shared.configured_latency_ns(), 2_000_000);
        assert!(requests.has_changed().unwrap());
        // Reported as the receive latency once applied.
        let rx_latency = |s: &Shared| read_properties(s, Some(&[arc::property::RX_LATENCY]));
        assert_eq!(rx_latency(&shared), [(arc::property::RX_LATENCY, Some(4_000_000))]);
        assert!(shared.restart_rx());
        assert_eq!(rx_latency(&shared), [(arc::property::RX_LATENCY, Some(2_000_000))]);
        assert_eq!(shared.info.latency.get(), (2_000_000, 96));
        // Applying it again changes nothing.
        assert!(!shared.restart_rx());
        assert_eq!(noted(&shared)[1], (false, "the receive latency to 2 ms".to_owned()));
        // Saved with the state, as the app's choice.
        assert_eq!(shared.state().remote_settings_lock_set, Some(false));
    }

    #[test]
    fn the_show_lock_refuses_renames_but_not_routing() {
        let shared = shared(true);
        let name = |n| arc::encode_set_device_name_request(1, Some(n));
        assert_eq!(ask(&shared, name("vox-fx")), result::ERROR);
        assert_eq!(shared.name(), "desk-mac");
        // Its own name again changes nothing.
        assert_eq!(ask(&shared, name("desk-mac")), result::SUCCESS);
        assert_eq!(
            ask(&shared, arc::encode_rename_tx_channels_request(1, &[(1, "Vox")])),
            result::ERROR
        );
        assert_eq!(
            ask(&shared, arc::encode_rename_rx_channels_request(1, &[(2, "Mic")])),
            result::ERROR
        );
        assert_eq!(shared.state().tx_names, ["01", "02"]);
        assert_eq!(shared.state().rx_names, ["01", "02"]);
        assert_eq!(
            noted(&shared),
            [
                (true, "the device name to vox-fx".to_owned()),
                (true, "the name of transmit channel 1 to Vox".to_owned()),
                (true, "the name of receive channel 2 to Mic".to_owned()),
            ]
        );
        // Subscriptions stay open: patching is the console's job mid-show.
        let sub = arc::SubscriptionRequest {
            rx_channel: 1,
            source: Some(("01".into(), "stagebox".into())),
        };
        assert_eq!(ask(&shared, arc::encode_set_subscriptions_request(1, &[sub])), result::SUCCESS);
        assert!(shared.state().subscriptions[0].is_some());

        shared.lock_remote_settings(false);
        assert_eq!(ask(&shared, name("vox-fx")), result::SUCCESS);
        assert_eq!(shared.name(), "vox-fx");
        assert_eq!(
            ask(&shared, arc::encode_rename_tx_channels_request(1, &[(1, "Vox")])),
            result::SUCCESS
        );
        assert_eq!(shared.state().tx_names, ["Vox", "02"]);
        let last = noted(&shared).split_off(3);
        assert_eq!(
            last,
            [
                (false, "the device name to vox-fx".to_owned()),
                (false, "the name of transmit channel 1 to Vox".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn the_show_lock_refuses_a_controllers_format() {
        let shared = shared(true);
        let (mut c, _statuses) = local_conmon(&shared).await;
        let mut formats = shared.format_requests();
        // Dante Controller's "set" body: mode 1, then the value
        // (docs/PROTOCOL.md §7.5).
        let set = |value: u32| [1u32.to_be_bytes(), value.to_be_bytes()].concat();
        c.answer(notification::SAMPLE_RATE_QUERY, &set(96_000), Some(controller())).await;
        c.answer(notification::ENCODING_QUERY, &set(32), Some(controller())).await;
        assert!(!formats.has_changed().unwrap());
        assert_eq!(
            noted(&shared),
            [
                (true, "the sample rate to 96000 Hz".to_owned()),
                (true, "the encoding to 32-bit".to_owned()),
            ]
        );
        // The running format changes nothing, so it is not refused either.
        c.answer(notification::ENCODING_QUERY, &set(24), Some(controller())).await;
        assert!(!formats.has_changed().unwrap());
        assert_eq!(noted(&shared).len(), 2);

        shared.lock_remote_settings(false);
        c.answer(notification::SAMPLE_RATE_QUERY, &set(96_000), Some(controller())).await;
        assert!(formats.has_changed().unwrap());
        assert_eq!(formats.borrow_and_update().sample_rate, Some(96_000));
        assert_eq!(noted(&shared)[2], (false, "the sample rate to 96000 Hz".to_owned()));
    }

    #[tokio::test]
    async fn conmon_shows_the_format_fixed_while_the_show_lock_is_on() {
        use notification::{ENCODING_STATUS, SAMPLE_RATE_STATUS, VERSIONS_STATUS};
        let (shared, mut notify) = shared_with_notifications(false);
        let (mut c, statuses) = local_conmon(&shared).await;
        let all = |settable| {
            [
                (VERSIONS_STATUS, settable),
                (SAMPLE_RATE_STATUS, settable),
                (ENCODING_STATUS, settable),
            ]
        };
        // Unlocked, a device whose owner applies a new format says so.
        for query in [
            notification::VERSIONS_QUERY,
            notification::SAMPLE_RATE_QUERY,
            notification::ENCODING_QUERY,
        ] {
            c.answer(query, &[], Some(controller())).await;
        }
        assert_eq!(format_settable(&statuses, 3), all(true));

        // The lock tells conmon, which says again, unasked, that it is not.
        shared.lock_remote_settings(true);
        c.notified(notify.try_recv().expect("a notification")).await;
        assert_eq!(format_settable(&statuses, 3), all(false));
        // So does every answer while it is locked.
        c.answer(notification::VERSIONS_QUERY, &[], Some(controller())).await;
        assert_eq!(format_settable(&statuses, 1), [(VERSIONS_STATUS, false)]);
        // Locking it again changes nothing, and says nothing.
        shared.lock_remote_settings(true);
        assert!(notify.try_recv().is_err());

        shared.lock_remote_settings(false);
        let n = notify.try_recv().expect("a notification");
        assert!(matches!(n, Notify::RemoteSettingsLockChanged), "{n:?}");
        c.notified(n).await;
        assert_eq!(format_settable(&statuses, 3), all(true));
        // A locked change is still refused with the ARC error result
        // (0x0022), as a value out of range is (docs/PROTOCOL.md §3.3).
        shared.lock_remote_settings(true);
        let set = arc::encode_set_latency_request(1, 2_000_000, 16);
        assert_eq!(ask(&shared, set), 0x0022);
    }

    #[test]
    fn the_remote_change_log_keeps_the_latest() {
        let log = RemoteChanges::default();
        let shared_log = log.clone();
        for i in 0..RemoteChanges::KEPT + 3 {
            log.push(crate::state::RemoteChange {
                at: std::time::SystemTime::UNIX_EPOCH,
                from: controller(),
                what: i.to_string(),
                refused: false,
            });
        }
        let kept: Vec<String> = shared_log.recent().into_iter().map(|c| c.what).collect();
        assert_eq!(kept.len(), RemoteChanges::KEPT);
        assert_eq!(kept.first().unwrap(), "3");
        assert_eq!(kept.last().unwrap(), &(RemoteChanges::KEPT + 2).to_string());
    }
}
