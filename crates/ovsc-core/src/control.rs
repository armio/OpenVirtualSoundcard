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
        if let Some(resp) = handle_arc(&shared, &frame) {
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
        (RX_LATENCY, info.latency_ns),
        (MAX_LATENCY, MAX_LATENCY_NS),
        (MIN_LATENCY, MIN_LATENCY_NS),
    ];
    let value = |id: u16| known.iter().find(|(k, _)| *k == id).map(|(_, v)| *v);
    match asked {
        Some(ids) => ids.iter().map(|&id| (id, value(id))).collect(),
        None => known.iter().map(|&(id, v)| (id, Some(v))).collect(),
    }
}

/// A controller writing device properties ([`opcode::PROPERTIES_1101`]):
/// the configured or receive latency is saved and applied by restarting the
/// device. The answer echoes the records applied.
fn write_properties(shared: &Shared, frame: &Frame<'_>) -> Vec<u8> {
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
    if let Err(e) = shared.request_latency(ns) {
        warn!("refusing a latency of {} ms: {e}", ns as f64 / 1e6);
        return respond(h, result::ERROR, &[]);
    }
    info!("receive latency set to {} ms by a controller", ns as f64 / 1e6);
    let applied: Vec<(u16, Option<u32>)> = records
        .iter()
        .filter_map(|&(id, v)| match id {
            CONFIGURED_LATENCY | RX_LATENCY => Some((id, Some(ns))),
            UNICAST_FPP | RX_FPP => Some((id, v)),
            _ => None,
        })
        .collect();
    arc::encode_properties_response(h, &applied)
}

/// Handles one ARC request and returns the response, if any.
pub fn handle_arc(shared: &Shared, frame: &Frame<'_>) -> Option<Vec<u8>> {
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
            if discovery::validate_device_name(&name).is_err() {
                warn!("refusing invalid device name {name:?}");
                respond(h, result::ERROR, &[])
            } else {
                info!("device renamed to {name}");
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
            let changed = rename(shared, &renames, true);
            // Peers learn about transmit renames from mDNS.
            if changed.is_empty() {
                respond(h, result::ERROR, &[])
            } else {
                respond(h, result::SUCCESS, &[0, 0])
            }
        }
        opcode::RENAME_RX_CHANNELS => {
            let renames = arc::decode_rename_rx_channels_request(frame).ok()?;
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
        opcode::PROPERTIES_1101 => write_properties(shared, frame),
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
    seq: u16,
    /// The clock state last reported, to report changes.
    sync: Option<conmon::Sync>,
    /// The interface's counters at the last heartbeat, and when.
    traffic: Option<(InterfaceCounters, std::time::Instant)>,
    /// The interface's error counters when the device started.
    errors_base: Option<InterfaceCounters>,
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
        let dest = SocketAddr::from((conmon::INFO_GROUP, conmon::INFO_PORT));
        self.send(conmon::START_INFO, conmon::status_opcode(id), content, dest).await;
    }

    /// Answers request `id` with content `body`.
    async fn answer(&mut self, id: u16, body: &[u8]) {
        let info = self.shared.info.clone();
        let iface = &info.iface;
        match id {
            notification::VERSIONS_QUERY => {
                let mut capabilities = conmon::capability::MANUFACTURER_NAME;
                if self.shared.format_configurable {
                    capabilities |= conmon::capability::SAMPLE_RATE | conmon::capability::ENCODING;
                }
                let c = conmon::versions_status(BOARD_NAME, capabilities);
                self.status(notification::VERSIONS_STATUS, &c).await
            }
            notification::SAMPLE_RATE_QUERY => {
                if let Some(rate) = conmon::decode_configurable_request(body) {
                    self.request_format(FormatRequest {
                        sample_rate: Some(rate),
                        ..Default::default()
                    });
                }
                let pending = self.shared.pending_format().sample_rate;
                let c = conmon::configurable_status(
                    info.sample_rate,
                    pending.filter(|&r| r != info.sample_rate).unwrap_or(0),
                    self.shared.format_configurable,
                    &crate::config::SAMPLE_RATES,
                );
                self.status(notification::SAMPLE_RATE_STATUS, &c).await
            }
            notification::ENCODING_QUERY => {
                if let Some(bits) = conmon::decode_configurable_request(body) {
                    self.request_format(FormatRequest {
                        bits_per_sample: Some(bits.min(u32::from(u16::MAX)) as u16),
                        ..Default::default()
                    });
                }
                let current = u32::from(info.format.bits());
                let pending = self.shared.pending_format().bits_per_sample.map(u32::from);
                let values: Vec<u32> =
                    crate::config::BITS_PER_SAMPLE.iter().map(|&b| u32::from(b)).collect();
                let c = conmon::configurable_status(
                    current,
                    pending.filter(|&b| b != current).unwrap_or(0),
                    self.shared.format_configurable,
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
                let c = conmon::interface_status(
                    iface.link_speed_mbps,
                    iface.mac,
                    iface.ip,
                    iface.netmask,
                    iface.gateway,
                );
                self.status(notification::INTERFACE_STATUS, &c).await
            }
            notification::CLOCKING_QUERY => self.clocking().await,
            notification::CLEAR_CONFIG_QUERY => {
                self.status(notification::CLEAR_CONFIG_STATUS, &conmon::clear_config_status()).await
            }
            other => debug!("unhandled conmon query {other:#06x}"),
        }
    }

    /// Applies a controller's format request, or says why not.
    fn request_format(&self, request: FormatRequest) {
        match self.shared.request_format(request) {
            Ok(()) => info!("a controller asked for {request:?}"),
            Err(e) => warn!("refusing {request:?} from a controller: {e}"),
        }
    }

    /// The interface's traffic since the last call, per second, and its
    /// errors since the device started.
    fn traffic(&mut self) -> Option<InterfaceTraffic> {
        let now = interface_counters(&self.shared.info.iface.name)?;
        let at = std::time::Instant::now();
        let base = *self.errors_base.get_or_insert(now);
        let previous = self.traffic.replace((now, at));
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
        if let Some(traffic) = self.traffic() {
            hb = hb.interface_traffic(seq, &[traffic]);
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
            hb = hb
                .rx_latency(seq, rate, &flow_arrivals(&stats, MAX_RX_FLOWS))
                .late_packets(seq, &flow_late_packets(&stats, MAX_RX_FLOWS));
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
    let mut c = Conmon { shared, socket, seq: 1, sync: None, traffic: None, errors_base: None };
    c.answer(notification::VERSIONS_QUERY, &[]).await;
    c.answer(notification::MANUFACTURER_VERSIONS_QUERY, &[]).await;
    let mut heartbeat = tokio::time::interval(Duration::from_secs(1));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut buf = vec![0u8; MTU];
    loop {
        tokio::select! {
            r = c.socket.recv_from(&mut buf) => {
                let Ok((len, src)) = r else { continue };
                match ConmonHeader::decode(&buf[..len]) {
                    Ok((h, body)) => match conmon::opcode_id(&h.opcode) {
                        Some(id) => c.answer(id, body).await,
                        None => debug!("conmon request with unknown opcode {} from {src}",
                                       hex::encode(h.opcode)),
                    },
                    Err(e) => debug!("malformed conmon packet from {src}: {e}"),
                }
            }
            n = notify.recv() => match n {
                Some(Notify::RxChannelsChanged(channels)) => {
                    c.status(notification::RX_CHANNEL_CHANGE, &conmon::channel_change(channels)).await;
                }
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
}
