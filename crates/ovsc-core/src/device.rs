//! Starting, observing and stopping a device.

use std::net::Ipv4Addr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use ovsc_clock::MediaClock;
use ovsc_proto::arc::SubscriptionStatus;
use ovsc_proto::audio::{Sample, SampleFormat};

use crate::buffer::TimedRing;
use crate::config::DeviceConfig;
use crate::control;
use crate::directory::{Directory, ResolvedChannel, StaticDirectory};
use crate::info::{BOARD_NAME, DeviceInfo, MANUFACTURER, MODEL_ID};
use crate::mdns::{self, Advertisement, Mdns};
use crate::net::{Interface, bind_udp, into_tokio};
use crate::persist::{self, SavedState};
use crate::rx::{RxManager, own_channel_txt};
use crate::state::{FormatRequest, Shared, State, Subscription};
use crate::tx::{self, FlowControl, TxCommand};
use crate::{Error, Result};

/// How long [`Device::shutdown`] waits for the device's tasks and receive
/// threads to stop once told to. They normally take well under a receive
/// thread's keepalive interval (250 ms).
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// A running Dante-compatible device.
pub struct Device {
    shared: Arc<Shared>,
    mdns: Option<Mdns>,
    tasks: Vec<JoinHandle<()>>,
    rx_shutdown: Option<oneshot::Sender<oneshot::Sender<()>>>,
    /// Closes once the receive manager and all its threads have exited (see
    /// [`RxManager::new`]).
    rx_threads: mpsc::Receiver<()>,
    tx_commands: std::sync::mpsc::Sender<TxCommand>,
    tx_thread: Option<std::thread::JoinHandle<()>>,
    /// Where the device saves its state; saved once more on shutdown.
    state_file: Option<std::path::PathBuf>,
}

/// Status of a receive channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RxChannelStatus {
    pub name: String,
    pub subscription: Option<(String, String)>,
    pub status: SubscriptionStatus,
}

/// A transmit flow some receiver requested from us.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxFlowStatus {
    pub id: u16,
    pub receiver: Option<String>,
    pub destination: std::net::SocketAddrV4,
    pub channels: Vec<u16>,
}

/// Channel rings the device uses instead of allocating its own, for example
/// rings in memory shared with an audio driver. There must be one ring per
/// channel, each of exactly [`DeviceConfig::ring_capacity`] samples.
#[derive(Clone, Debug)]
pub struct ExternalRings {
    pub rx: Vec<Arc<TimedRing>>,
    pub tx: Vec<Arc<TimedRing>>,
}

/// Optional parts of [`Device::start_with_options`].
#[derive(Clone, Default)]
pub struct StartOptions {
    /// Resolve subscriptions from this directory instead of mDNS.
    pub directory: Option<StaticDirectory>,
    /// Use these rings instead of allocating them.
    pub rings: Option<ExternalRings>,
    /// Let controllers change the sample rate and the bit depth: the
    /// caller then applies [`Device::format_requests`], restarting the
    /// device with them.
    pub format_configurable: bool,
}

/// Packet counters since the device started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeviceStats {
    /// Audio packets sent.
    pub tx_packets: u64,
    /// Packets sent while a channel they carry had no audio from the backend.
    pub tx_underruns: u64,
    /// Audio packets received.
    pub rx_packets: u64,
    /// Received packets that were already older than the receive latency.
    pub rx_late_packets: u64,
}

impl Device {
    /// Starts a device on the configured interface. With `discovery` on (the
    /// default) it advertises itself and finds transmitters over mDNS;
    /// otherwise it resolves subscriptions from an empty
    /// [`StaticDirectory`] (see [`Device::start_with_directory`]).
    pub async fn start(config: DeviceConfig, clock: MediaClock) -> Result<Self> {
        Self::start_with_options(config, clock, StartOptions::default()).await
    }

    /// Starts a device that resolves subscriptions from `directory` instead
    /// of mDNS. mDNS advertisement still follows `config.discovery`.
    pub async fn start_with_directory(
        config: DeviceConfig,
        clock: MediaClock,
        directory: StaticDirectory,
    ) -> Result<Self> {
        let options = StartOptions { directory: Some(directory), ..Default::default() };
        Self::start_with_options(config, clock, options).await
    }

    /// Starts a device with the given [`StartOptions`]. Fails with
    /// [`Error::Config`] if external rings don't match the configuration.
    pub async fn start_with_options(
        config: DeviceConfig,
        clock: MediaClock,
        options: StartOptions,
    ) -> Result<Self> {
        config.validate()?;
        let saved = config.state_file.as_deref().and_then(SavedState::load).unwrap_or_default();
        let config = with_saved_latency(config, &saved);
        let iface = Interface::find(&config.interface)?;
        let info = DeviceInfo::new(&config, iface)?;
        let state = initial_state(&config, &info, saved);
        let ip = info.iface.ip;
        let ports = info.ports;
        let (shared, notify_rx) = Shared::new(
            info,
            state,
            clock,
            config.ring_capacity,
            options.rings,
            options.format_configurable,
        )?;

        // Bind everything first so that port conflicts fail the start.
        let arc_socket = into_tokio(bind_udp(ip, ports.arc)?)?;
        // Dante Controller sends ARC to a device on its own host at
        // 127.0.0.1, not at the device's address (seen on macOS), so answer
        // there too. Without it the controller lists the device but never
        // its channels.
        let arc_loopback = if ip == Ipv4Addr::LOCALHOST {
            None
        } else {
            match bind_udp(Ipv4Addr::LOCALHOST, ports.arc) {
                Ok(s) => Some(into_tokio(s)?),
                Err(e) => {
                    warn!(
                        "not answering ARC on 127.0.0.1 ({e}): Dante Controller on this host \
                         will not list the device's channels"
                    );
                    None
                }
            }
        };
        let cmc_socket = into_tokio(bind_udp(ip, ports.cmc)?)?;
        let fc_socket = into_tokio(bind_udp(ip, ports.flow_control)?)?;
        let settings_socket = if shared.info.discovery {
            Some(into_tokio(bind_udp(ip, ports.settings)?)?)
        } else {
            None
        };
        let mdns = if shared.info.discovery { Some(Mdns::start(ip)?) } else { None };

        let directory = match (options.directory, &mdns) {
            (Some(d), _) => Directory::Static(d),
            (None, Some(m)) => Directory::Mdns(m.clone()),
            (None, None) => Directory::Static(StaticDirectory::new()),
        };

        let mut tasks = Vec::new();
        tasks.push(tokio::spawn(control::run_arc(shared.clone(), arc_socket)));
        if let Some(s) = arc_loopback {
            tasks.push(tokio::spawn(control::run_arc(shared.clone(), s)));
        }
        tasks.push(tokio::spawn(control::run_cmc(shared.clone(), cmc_socket)));
        tasks.push(match settings_socket {
            Some(s) => tokio::spawn(control::run_conmon(shared.clone(), s, notify_rx)),
            None => tokio::spawn(control::drain_notifications(notify_rx)),
        });

        let (tx_commands, tx_events, tx_thread) = tx::spawn_engine(shared.clone());
        let fc = FlowControl::new(shared.clone(), tx_commands.clone());
        tasks.push(tokio::spawn(fc.run(fc_socket, tx_events)));

        let (rx_shutdown, rx_shutdown_rx) = oneshot::channel();
        let (rx_threads_tx, rx_threads) = mpsc::channel(1);
        let rx = RxManager::new(shared.clone(), directory, rx_threads_tx);
        tasks.push(tokio::spawn(rx.run(rx_shutdown_rx)));

        if let Some(m) = &mdns {
            tasks.push(tokio::spawn(advertise(shared.clone(), m.clone())));
        }
        if let Some(path) = config.state_file.clone() {
            tasks.push(tokio::spawn(persist::run(shared.clone(), path)));
        }

        info!(
            "device {:?} running on {} ({}): {} tx / {} rx channels, {} Hz, {}-bit, {} ms latency",
            shared.name(),
            shared.info.iface.name,
            ip,
            shared.info.tx_channels.len(),
            shared.info.rx_channels.len(),
            shared.info.sample_rate,
            shared.info.format.bits(),
            shared.info.latency_ns as f64 / 1e6,
        );
        Ok(Self {
            shared,
            mdns,
            tasks,
            rx_shutdown: Some(rx_shutdown),
            rx_threads,
            tx_commands,
            tx_thread: Some(tx_thread),
            state_file: config.state_file,
        })
    }

    /// Current device name.
    pub fn name(&self) -> String {
        self.shared.name()
    }

    /// Sets the receive latency to `ns`, saved for the next start of the
    /// device; [`Device::latency_requests`] listeners restart it to apply it.
    pub fn request_latency(&self, ns: u32) -> Result<()> {
        self.shared.request_latency(ns)
    }

    /// The receive latency from the next start of the device on, in ns:
    /// the running one unless a controller or the app set another.
    pub fn configured_latency_ns(&self) -> u32 {
        self.shared.configured_latency_ns()
    }

    /// Wakes up whenever a controller or the app asks for a new latency.
    pub fn latency_requests(&self) -> watch::Receiver<Option<u32>> {
        self.shared.latency_requests()
    }

    /// Wakes up whenever a controller asks for a new sample rate or bit
    /// depth; see [`StartOptions::format_configurable`].
    pub fn format_requests(&self) -> watch::Receiver<FormatRequest> {
        self.shared.format_requests()
    }

    /// Renames the device, as a controller would.
    pub fn rename(&self, name: &str) -> Result<()> {
        ovsc_proto::discovery::validate_device_name(name)
            .map_err(|e| Error::Config(format!("invalid device name {name:?}: {e}")))?;
        self.shared.modify(|s| s.name = name.to_owned());
        Ok(())
    }

    pub fn info(&self) -> &DeviceInfo {
        &self.shared.info
    }

    /// The media clock the device runs on.
    pub fn clock(&self) -> MediaClock {
        self.shared.clock.clone()
    }

    /// Notifies every change of the device name, channel names or
    /// subscriptions (the value is a change counter).
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.shared.watch()
    }

    /// Current receive and transmit channel names, as `(rx, tx)`.
    pub fn channel_names(&self) -> (Vec<String>, Vec<String>) {
        channel_names(&self.shared)
    }

    /// Packet counters since the device started.
    pub fn stats(&self) -> DeviceStats {
        stats(&self.shared)
    }

    /// A handle to the device's names and counters for tasks that outlive a
    /// borrow of the device. It does not keep the device alive.
    pub fn observer(&self) -> DeviceObserver {
        DeviceObserver { shared: Arc::downgrade(&self.shared) }
    }

    /// Handle for audio backends.
    /// Feed playback into the same rings the native driver exposes as inputs.
    /// Live receive workers keep their flows alive but suppress their writes.
    pub fn override_receive(&self) -> Result<(AudioIo, ReceiveOverride)> {
        let mut active = self.shared.rx_override.write().unwrap_or_else(|e| e.into_inner());
        if *active {
            return Err(Error::Config("receive audio is already overridden".into()));
        }
        *active = true;
        drop(active);
        for ring in &self.shared.rx_rings {
            ring.clear();
        }
        let mut io = self.audio();
        io.tx = self.shared.rx_rings.clone();
        Ok((io, ReceiveOverride { shared: self.shared.clone() }))
    }

    /// Exclusively replace the network transmit source. Core Audio keeps
    /// writing its own rings; it cannot overwrite this source's samples.
    pub fn override_transmit(&self) -> Result<(AudioIo, TransmitOverride)> {
        let mut io = self.audio();
        io.tx =
            self.shared.tx_rings.iter().map(|r| Arc::new(TimedRing::new(r.capacity()))).collect();
        let rings = Arc::new(io.tx.clone());
        let previous = self
            .shared
            .tx_override
            .compare_and_swap(&None::<Arc<Vec<Arc<TimedRing>>>>, Some(rings.clone()));
        if previous.is_some() {
            return Err(Error::Config("the transmit source is already overridden".into()));
        }
        Ok((io, TransmitOverride { shared: self.shared.clone(), rings }))
    }

    pub fn audio(&self) -> AudioIo {
        AudioIo {
            clock: self.shared.clock.clone(),
            sample_rate: self.shared.info.sample_rate,
            format: self.shared.info.format,
            latency_samples: self.shared.info.latency_samples,
            rx: self.shared.rx_rings.clone(),
            tx: self.shared.tx_rings.clone(),
            rx_names: self.shared.info.rx_channels.clone(),
            tx_names: self.shared.info.tx_channels.clone(),
        }
    }

    /// Subscribes receive channel `rx_channel` (1-based) to
    /// `tx_channel@tx_device`, exactly as a controller would.
    pub fn subscribe(&self, rx_channel: u16, tx_channel: &str, tx_device: &str) -> Result<()> {
        let index = self.rx_index(rx_channel)?;
        self.shared.modify(|s| {
            s.subscriptions[index] = Some(Subscription {
                tx_channel: tx_channel.to_owned(),
                tx_device: tx_device.to_owned(),
                status: SubscriptionStatus::Unresolved,
            })
        });
        Ok(())
    }

    /// Removes the subscription of receive channel `rx_channel` (1-based).
    pub fn unsubscribe(&self, rx_channel: u16) -> Result<()> {
        let index = self.rx_index(rx_channel)?;
        self.shared.modify(|s| s.subscriptions[index] = None);
        Ok(())
    }

    fn rx_index(&self, rx_channel: u16) -> Result<usize> {
        (rx_channel as usize)
            .checked_sub(1)
            .filter(|&i| i < self.shared.info.rx_channels.len())
            .ok_or_else(|| Error::NotFound(format!("rx channel {rx_channel}")))
    }

    pub fn rx_channels(&self) -> Vec<RxChannelStatus> {
        let s = self.shared.state();
        s.rx_names
            .iter()
            .zip(&s.subscriptions)
            .map(|(name, sub)| RxChannelStatus {
                name: name.clone(),
                subscription: sub.as_ref().map(|s| (s.tx_channel.clone(), s.tx_device.clone())),
                status: sub.as_ref().map_or(SubscriptionStatus::None, |s| s.status),
            })
            .collect()
    }

    pub fn tx_flows(&self) -> Vec<TxFlowStatus> {
        self.shared
            .state()
            .tx_flows
            .iter()
            .map(|f| TxFlowStatus {
                id: f.id,
                receiver: f.remote.as_ref().map(|(dev, _)| dev.clone()),
                destination: std::net::SocketAddrV4::new(f.dst_addr, f.dst_port),
                channels: f.channels.clone(),
            })
            .collect()
    }

    /// Directory entries describing our transmit channels, for feeding a
    /// [`StaticDirectory`] of another device (tests, multicast-less setups).
    pub fn directory_entries(&self) -> Vec<ResolvedChannel> {
        let info = &self.shared.info;
        let s = self.shared.state();
        let mut out = Vec::new();
        for (i, (factory, current)) in info.tx_channels.iter().zip(&s.tx_names).enumerate() {
            for name in [factory, current] {
                out.push(ResolvedChannel {
                    device: s.name.clone(),
                    channel: name.clone(),
                    addr: info.iface.ip,
                    flow_control_port: info.ports.flow_control,
                    txt: own_channel_txt(info, i as u16 + 1),
                });
            }
        }
        out
    }

    /// Stops all flows, withdraws the mDNS advertisement and shuts down.
    ///
    /// When it returns, the device's tasks and threads have exited and
    /// dropped what they shared, the channel rings included: only
    /// [`AudioIo`] handles still hold those. (Should one fail to stop
    /// within a couple of seconds, it logs a warning and returns anyway.)
    pub async fn shutdown(mut self) {
        if let Some(rx) = self.rx_shutdown.take() {
            let (done_tx, done_rx) = oneshot::channel();
            if rx.send(done_tx).is_ok() {
                let _ = tokio::time::timeout(Duration::from_secs(3), done_rx).await;
            }
        }
        if let Some(m) = &self.mdns {
            m.goodbye().await;
            m.stop();
        }
        let tasks = self.stop_now();
        // Aborting a task only marks it: the runtime drops its future, and
        // the `Arc<Shared>` and sockets in it, when it next polls the task,
        // on whichever worker gets to it. Receive threads notice they were
        // stopped within a keepalive interval. Wait for both, so that the
        // device holds nothing once this returns.
        let rx_threads = &mut self.rx_threads;
        let stopped = tokio::time::timeout(STOP_TIMEOUT, async {
            for t in tasks {
                let _ = t.await;
            }
            while rx_threads.recv().await.is_some() {}
        });
        if stopped.await.is_err() {
            warn!(
                "device tasks or receive threads still running {} s after shutdown",
                STOP_TIMEOUT.as_secs()
            );
        }
        // The saving task may not have caught up with the last change (a
        // latency set just before a restart, say).
        if let Some(path) = &self.state_file {
            persist::save(&self.shared, path);
        }
    }

    /// Aborts the tasks, stops the transmit thread and mDNS, and returns the
    /// aborted tasks' handles.
    fn stop_now(&mut self) -> Vec<JoinHandle<()>> {
        for t in &self.tasks {
            t.abort();
        }
        let _ = self.tx_commands.send(TxCommand::Shutdown);
        if let Some(t) = self.tx_thread.take() {
            let _ = t.join();
        }
        if let Some(m) = &self.mdns {
            m.stop();
        }
        std::mem::take(&mut self.tasks)
    }
}

impl Drop for Device {
    /// Stops everything without waiting for the tasks and receive threads,
    /// which release the device's state shortly after (see
    /// [`Device::shutdown`]).
    fn drop(&mut self) {
        self.stop_now();
    }
}

/// A device's current names and packet counters, readable from tasks that
/// outlive a borrow of the [`Device`] (status reporters, configuration
/// watchers). It does not keep the device alive: every accessor returns
/// `None` once the device has shut down.
#[derive(Clone, Debug)]
pub struct DeviceObserver {
    shared: Weak<Shared>,
}

impl DeviceObserver {
    /// Current device name.
    pub fn name(&self) -> Option<String> {
        self.shared.upgrade().map(|s| s.name())
    }

    /// Current receive and transmit channel names, as `(rx, tx)`.
    pub fn channel_names(&self) -> Option<(Vec<String>, Vec<String>)> {
        self.shared.upgrade().map(|s| channel_names(&s))
    }

    /// Packet counters since the device started.
    pub fn stats(&self) -> Option<DeviceStats> {
        self.shared.upgrade().map(|s| stats(&s))
    }
}

fn channel_names(shared: &Shared) -> (Vec<String>, Vec<String>) {
    let s = shared.state();
    (s.rx_names.clone(), s.tx_names.clone())
}

fn stats(shared: &Shared) -> DeviceStats {
    let c = &shared.counters;
    DeviceStats {
        tx_packets: c.tx_packets.load(Ordering::Relaxed),
        tx_underruns: c.tx_underruns.load(Ordering::Relaxed),
        rx_packets: c.rx_packets.load(Ordering::Relaxed),
        rx_late_packets: c.rx_late_packets.load(Ordering::Relaxed),
    }
}

/// `config` with the receive latency a controller or the app saved, if it
/// is valid.
fn with_saved_latency(mut config: DeviceConfig, saved: &SavedState) -> DeviceConfig {
    if let Some(ns) = saved.latency_ns {
        let ms = ns as f64 / 1e6;
        if (crate::config::MIN_LATENCY_MS..=crate::config::MAX_LATENCY_MS).contains(&ms) {
            info!("receive latency {ms} ms, as set by a controller or the app");
            config.latency_ms = ms;
        } else {
            warn!("ignoring the saved latency of {ms} ms");
        }
    }
    config
}

fn initial_state(config: &DeviceConfig, info: &DeviceInfo, saved: SavedState) -> State {
    let pick = |saved: &[String], factory: &[String]| -> Vec<String> {
        factory
            .iter()
            .enumerate()
            .map(|(i, f)| saved.get(i).filter(|s| !s.is_empty()).unwrap_or(f).clone())
            .collect()
    };
    let name = saved
        .name
        .clone()
        .filter(|n| ovsc_proto::discovery::validate_device_name(n).is_ok())
        .unwrap_or_else(|| {
            if config.name.is_empty() { info.factory_name.clone() } else { config.name.clone() }
        });
    let mut subscriptions = vec![None; info.rx_channels.len()];
    let initial = config.subscriptions.iter().map(|s| (s.rx_channel, &s.tx_channel, &s.tx_device));
    let restored = saved.subscriptions.iter().map(|s| (s.rx_channel, &s.tx_channel, &s.tx_device));
    for (rx, tx_channel, tx_device) in initial.chain(restored) {
        match (rx as usize).checked_sub(1).and_then(|i| subscriptions.get_mut(i)) {
            Some(slot) => {
                *slot = Some(Subscription {
                    tx_channel: tx_channel.clone(),
                    tx_device: tx_device.clone(),
                    status: SubscriptionStatus::Unresolved,
                })
            }
            None => warn!("ignoring subscription for unknown rx channel {rx}"),
        }
    }
    State {
        name,
        tx_names: pick(&saved.tx_names, &info.tx_channels),
        rx_names: pick(&saved.rx_names, &info.rx_channels),
        subscriptions,
        tx_flows: Vec::new(),
        rx_flows: Vec::new(),
        rx_flow_stats: Vec::new(),
        latency_override_ns: saved.latency_ns.filter(|&ns| ns == info.latency_ns),
    }
}

/// Keeps the mDNS records in line with the device name and channel names.
async fn advertise(shared: Arc<Shared>, mdns: Mdns) {
    let mut watch = shared.watch();
    let mut last = None;
    loop {
        let records = {
            let s = shared.state();
            let info = &shared.info;
            mdns::device_records(&Advertisement {
                name: &s.name,
                ip: info.iface.ip,
                arc_port: info.ports.arc,
                cmc_port: info.ports.cmc,
                flow_control_port: info.ports.flow_control,
                device_id: &info.device_id,
                process_id: info.process_id,
                manufacturer: MANUFACTURER,
                board_name: BOARD_NAME,
                model: MODEL_ID,
                tx_channels: info
                    .tx_channels
                    .iter()
                    .zip(&s.tx_names)
                    .enumerate()
                    .map(|(i, (f, c))| {
                        (f.as_str(), c.as_str(), own_channel_txt(info, i as u16 + 1))
                    })
                    .collect(),
            })
        };
        if last.as_ref() != Some(&records) {
            mdns.set_records(records.clone()).await;
            last = Some(records);
        }
        if watch.changed().await.is_err() {
            return;
        }
    }
}

/// Exclusive receive ownership, released when dropped.
pub struct ReceiveOverride {
    shared: Arc<Shared>,
}

impl Drop for ReceiveOverride {
    fn drop(&mut self) {
        for ring in &self.shared.rx_rings {
            ring.clear();
        }
        *self.shared.rx_override.write().unwrap_or_else(|e| e.into_inner()) = false;
    }
}

/// Exclusive transmit ownership, released when dropped.
pub struct TransmitOverride {
    shared: Arc<Shared>,
    rings: Arc<Vec<Arc<TimedRing>>>,
}

impl Drop for TransmitOverride {
    fn drop(&mut self) {
        self.shared.tx_override.compare_and_swap(&Some(self.rings.clone()), None);
    }
}

/// What an audio backend needs: the media clock and the channel rings.
///
/// * Receive: read ring `rx[ch]` at `now - latency` (never later: newer
///   samples may not have arrived yet).
/// * Transmit: write ring `tx[ch]` ahead of `now` (at least your callback
///   period plus the transmit guard), so samples are there when the network
///   thread picks them up.
#[derive(Clone)]
pub struct AudioIo {
    pub clock: MediaClock,
    pub sample_rate: u32,
    pub format: SampleFormat,
    pub latency_samples: u64,
    pub rx: Vec<Arc<TimedRing>>,
    pub tx: Vec<Arc<TimedRing>>,
    pub rx_names: Vec<String>,
    pub tx_names: Vec<String>,
}

impl AudioIo {
    /// Current media time in samples, if the clock is available.
    pub fn now(&self) -> Option<u64> {
        self.clock.now_samples(self.sample_rate)
    }

    /// Reads receive channel `ch` for timestamps `ts..ts + out.len()`.
    pub fn read_rx(&self, ch: usize, ts: u64, out: &mut [Sample]) -> usize {
        self.rx[ch].read(ts, out)
    }

    /// Writes transmit channel `ch` at timestamps `ts..`.
    pub fn write_tx(&self, ch: usize, ts: u64, samples: &[Sample]) {
        self.tx[ch].write(ts, samples)
    }
}
