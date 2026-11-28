//! Starting, observing and stopping a device.

use std::net::Ipv4Addr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use ovsc_clock::MediaClock;
use ovsc_proto::arc::SubscriptionStatus;
use ovsc_proto::audio::{Sample, SampleFormat};
use ovsc_proto::dns::Record;

use crate::buffer::{SWEEP_INTERVAL, Sweeper, TimedRing};
use crate::config::DeviceConfig;
use crate::control;
use crate::directory::{Directory, ResolvedChannel, StaticDirectory};
use crate::info::{BOARD_NAME, DeviceInfo, MANUFACTURER, MODEL_ID};
use crate::mdns::{self, Advertisement, Mdns, NameConflict};
use crate::net::{Interface, bind_udp, into_tokio};
use crate::persist::{self, SavedState};
use crate::rx::{RxManager, own_channel_txt};
use crate::state::{
    FormatRequest, NetworkStats, RemoteChanges, RxLatency, Shared, State, Subscription,
};
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
    /// The device's tasks, each with what it does, for [`Device::stopped_part`].
    tasks: Vec<(&'static str, JoinHandle<()>)>,
    rx_shutdown: Option<oneshot::Sender<oneshot::Sender<()>>>,
    /// Closes once the receive manager and all its threads have exited (see
    /// [`RxManager::new`]).
    rx_threads: mpsc::Receiver<()>,
    tx_commands: std::sync::mpsc::SyncSender<TxCommand>,
    tx_thread: Option<std::thread::JoinHandle<()>>,
    /// Forgets samples in the rings whose writer stopped, before their
    /// 32-bit tags could match again (see [`Sweeper`]).
    sweeper: Sweeper,
    /// Where the device saves its state; saved once more on shutdown.
    state_file: Option<std::path::PathBuf>,
    /// Why the saved state could not be used, if it could not.
    state_problem: Option<String>,
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
    /// The receiver's address on the secondary network, for a flow sent on
    /// both networks of a redundant system.
    pub secondary: Option<std::net::SocketAddrV4>,
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
    /// Note controllers' changes in this log, rather than a new one, to keep
    /// them listed across a restart of the device (see
    /// [`Device::remote_changes`]).
    pub remote_changes: Option<RemoteChanges>,
    /// Start from this state instead of the state file's: the
    /// [`Device::saved_state`] of the device this one replaces, so that a
    /// restart keeps the names and subscriptions made since the start
    /// even without a state file. It is saved to the state file, if any.
    pub state: Option<SavedState>,
    /// The name the device this one replaces in this process announced
    /// ([`Device::announced_name`]). Starting with that name, the device
    /// announces it again without probing for it first: the name is
    /// already its own on the network, and a probe would be answered by
    /// any other device that announces it too (two devices given the same
    /// name, say), which would rename this one mid-show, breaking every
    /// subscription to it. That conflict is reported instead
    /// ([`Device::name_conflict`], `renamed_to: None`). Any other name is
    /// probed for, as at a first start.
    ///
    /// Without it, the device takes the name over in the same way from a
    /// device that ran before it on this computer, in an earlier process:
    /// when the state (the state file, or [`StartOptions::state`]) holds
    /// that device's claim of the name, made on this interface (see
    /// [`crate::SavedClaim`]).
    pub announced: Option<String>,
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
    /// Audio packets the network stack refused to send or reported
    /// undeliverable.
    pub tx_send_errors: u64,
    /// The OS error code of the latest send error, 0 if none.
    pub tx_last_send_error: i32,
    /// Audio packets sent more than 250 µs after their send time
    /// (timestamp + guard).
    pub tx_late_packets: u64,
    /// The latest an audio packet left after its send time, in ns.
    pub tx_max_lateness_ns: u64,
    /// Audio packets never sent: too old for any receiver after a stall of
    /// the transmit thread or a media clock step.
    pub tx_skipped_packets: u64,
    /// Times a transmit flow's timeline restarted after such a stall or
    /// step.
    pub tx_resyncs: u64,
    /// Transmit flows removed after 4 s without keepalives.
    pub tx_expired_flows: u64,
    /// The transmit thread runs without real-time scheduling.
    pub tx_not_realtime: bool,
    /// A receive thread started without real-time scheduling since the
    /// device started.
    pub rx_not_realtime: bool,
    /// The transmit thread has stopped: no flow sends audio until the
    /// device restarts.
    pub tx_engine_failed: bool,
    /// Packets missing from received flows: gaps in their timestamps.
    pub rx_lost_packets: u64,
    /// Received packets stamped ahead of our clock by more than a packet.
    pub rx_early_packets: u64,
    /// Datagrams receive flows dropped: from another sender than the
    /// flow's transmitter, or not whole frames of the flow's slots.
    pub rx_rejected_packets: u64,
    /// Each network's packets, the primary's then the secondary's: see
    /// [`crate::state::Counters`]. All zero for the secondary of a device
    /// without one.
    pub networks: [NetworkStats; 2],
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
        let recovered = match options.state {
            Some(state) => persist::Recovered { state, intact: false, problem: None },
            None => config.state_file.as_deref().map(SavedState::recover).unwrap_or_default(),
        };
        let saved = recovered.state.clone();
        let on_disk = recovered.intact.then_some(recovered.state);
        // Also a problem found by a device this one replaces.
        let state_problem = recovered
            .problem
            .or_else(|| config.state_file.as_deref().and_then(persist::problem_at_start));
        let config = config.with_saved(&saved);
        if let Some(ns) = saved.latency_ns {
            let ms = ns as f64 / 1e6;
            if config.latency_ns() == ns {
                info!("receive latency {ms} ms, as set by a controller or the app");
            } else {
                warn!("ignoring the saved latency of {ms} ms");
            }
        }
        let iface = Interface::find(&config.interface)?;
        // A secondary network that is down (its cable out, so its interface
        // has no address) must not keep the device off the primary one: the
        // device then runs on the primary network alone, and says so.
        let (secondary, missing) = match config.secondary_interface.as_str() {
            "" => (None, None),
            spec => match Interface::find(spec) {
                Ok(s) => (Some(s), None),
                Err(e) => {
                    warn!(
                        "secondary_interface {spec}: {e}; running on the primary network alone, \
                         without redundancy, until the device restarts"
                    );
                    (None, Some(e.to_string()))
                }
            },
        };
        check_secondary(&iface, secondary.as_ref())?;
        let mut info = DeviceInfo::new(&config, iface)?;
        info.secondary = secondary;
        info.secondary_missing = missing;
        let state = initial_state(&config, &info, saved, options.announced);
        let ip = info.iface.ip;
        let ports = info.ports;
        let (shared, notify_rx) = Shared::new(
            info,
            state,
            clock,
            config.ring_capacity,
            options.rings,
            options.format_configurable,
            options.remote_changes.unwrap_or_default(),
        )?;
        // Rings in a region shared with a driver outlive the device, so the
        // sweeper's first pass, at once, also forgets what an earlier device
        // left behind.
        let rings = shared.rx_rings.iter().chain(&shared.tx_rings).cloned().collect();
        let (clock, rate) = (shared.clock.clone(), shared.info.sample_rate);
        let sweeper = Sweeper::spawn(rings, SWEEP_INTERVAL, move || clock.now_samples(rate))?;

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
        tasks.push(("ARC server", tokio::spawn(control::run_arc(shared.clone(), arc_socket))));
        if let Some(s) = arc_loopback {
            tasks.push((
                "ARC server on 127.0.0.1",
                tokio::spawn(control::run_arc(shared.clone(), s)),
            ));
        }
        tasks.push(("CMC server", tokio::spawn(control::run_cmc(shared.clone(), cmc_socket))));
        tasks.push(match settings_socket {
            Some(s) => {
                ("settings server", tokio::spawn(control::run_conmon(shared.clone(), s, notify_rx)))
            }
            None => ("notifications", tokio::spawn(control::drain_notifications(notify_rx))),
        });

        let (tx_commands, tx_events, tx_thread) = tx::spawn_engine(shared.clone());
        let fc = FlowControl::new(shared.clone(), tx_commands.clone());
        tasks.push(("flow control server", tokio::spawn(fc.run(fc_socket, tx_events))));

        let (rx_shutdown, rx_shutdown_rx) = oneshot::channel();
        let (rx_threads_tx, rx_threads) = mpsc::channel(1);
        let rx = RxManager::new(shared.clone(), directory, rx_threads_tx);
        tasks.push(("receive manager", tokio::spawn(rx.run(rx_shutdown_rx))));

        if let Some(m) = &mdns {
            tasks.push(("mDNS advertiser", tokio::spawn(advertise(shared.clone(), m.clone()))));
        }
        if let Some(path) = config.state_file.clone() {
            tasks.push(("state saver", tokio::spawn(persist::run(shared.clone(), path, on_disk))));
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
            shared.info.latency.ns() as f64 / 1e6,
        );
        Ok(Self {
            shared,
            mdns,
            tasks,
            rx_shutdown: Some(rx_shutdown),
            rx_threads,
            tx_commands,
            tx_thread: Some(tx_thread),
            sweeper,
            state_file: config.state_file,
            state_problem,
        })
    }

    /// The device name this device announces on the network, once it has
    /// claimed it (probed for it, RFC 6762 §8.1), or from the start if it
    /// holds it already: taken over from the device it replaces in this
    /// process, or claimed on this computer before (see
    /// [`StartOptions::announced`]). `None` before that, and without
    /// discovery. A device that replaces this one in the same process takes
    /// it over through [`StartOptions::announced`]; one in a later process,
    /// through the state file, which saves it with the interface's MAC
    /// address ([`crate::SavedClaim`]).
    pub fn announced_name(&self) -> Option<String> {
        self.shared.state().announced.clone()
    }

    /// The names, subscriptions and latency a controller or the app set,
    /// and the device's claim of the name it announces, as they would be
    /// saved; see [`StartOptions::state`].
    pub fn saved_state(&self) -> SavedState {
        SavedState::from_state(&self.shared.state(), &self.shared.info)
    }

    /// Why the saved state (names, subscriptions, latency) could not be
    /// used at start, if it could not: the device then started from the
    /// backup or without it (see [`SavedState::recover`]). It stays for
    /// the life of the process, through restarts of the device.
    pub fn saved_state_problem(&self) -> Option<&str> {
        self.state_problem.as_deref()
    }

    /// A part of the running device that has stopped on its own, if one
    /// has: the transmit thread or one of the tasks. They only end when
    /// the device shuts down, so this means one failed (a panic, say) and
    /// the device no longer works as a whole.
    pub fn stopped_part(&self) -> Option<&'static str> {
        if self.tx_thread.as_ref().is_some_and(|t| t.is_finished()) {
            return Some("transmit thread");
        }
        self.tasks.iter().find(|(_, t)| t.is_finished()).map(|(what, _)| *what)
    }

    /// Current device name.
    pub fn name(&self) -> String {
        self.shared.name()
    }

    /// Another device on the network that uses this device's name, or did
    /// when this device took another one; see [`NameConflict`].
    pub fn name_conflict(&self) -> Option<NameConflict> {
        self.shared.state().name_conflict.clone()
    }

    /// Sets the receive latency to `ns`, saved for the next start of the
    /// device; [`Device::latency_requests`] listeners apply it with
    /// [`Device::restart_rx`].
    pub fn request_latency(&self, ns: u32) -> Result<()> {
        self.shared.request_latency(ns)
    }

    /// The receive latency asked for last, in ns: the running one unless a
    /// controller or the app set another that [`Device::restart_rx`] has
    /// not applied yet. The device starts with it next time.
    pub fn configured_latency_ns(&self) -> u32 {
        self.shared.configured_latency_ns()
    }

    /// The receive latency the device runs at, in ns.
    pub fn latency_ns(&self) -> u32 {
        self.shared.info.latency.ns()
    }

    /// The receive latency the device runs at, in samples.
    pub fn latency_samples(&self) -> u64 {
        self.shared.info.latency.samples()
    }

    /// Restarts the receive side at the receive latency asked for last
    /// ([`Device::request_latency`]), if the device does not run at it yet.
    /// Returns whether it changed.
    ///
    /// Only what the receive latency is about follows it, at once:
    ///
    /// * every receive flow is stopped and requested again with frames per
    ///   packet and a latency of its own that suit the new latency; the
    ///   transmitter is told to stop the old flow before it is asked for
    ///   the new one, which comes back about as fast as the transmitter
    ///   answers (and the subscriptions show `InProgress` meanwhile);
    /// * [`AudioIo::latency_samples`], how far behind the media clock audio
    ///   backends read the receive rings, and the latency reported to
    ///   controllers (ARC) and in [`DeviceInfo::latency`].
    ///
    /// Nothing else stops or changes: the transmit flows, which other
    /// devices receive from this one, keep their handles and keep sending;
    /// the flow-control server keeps answering; the mDNS records stay
    /// announced as they are, the transmit latency they advertise
    /// included.
    pub fn restart_rx(&self) -> bool {
        let changed = self.shared.restart_rx();
        if changed {
            info!(
                "receive latency {} ms: requesting the receive flows again; the transmit flows \
                 go on",
                self.latency_ns() as f64 / 1e6
            );
        }
        changed
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

    /// Whether the show lock keeps controllers from changing the latency,
    /// the format and the names; see [`DeviceConfig::lock_remote_settings`].
    pub fn remote_settings_locked(&self) -> bool {
        self.shared.remote_settings_locked()
    }

    /// Turns the show lock on or off, saved with the device's state.
    pub fn lock_remote_settings(&self, locked: bool) {
        self.shared.lock_remote_settings(locked)
    }

    /// What controllers changed, or tried to change while locked, recently.
    pub fn remote_changes(&self) -> RemoteChanges {
        self.shared.remote_changes.clone()
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
    pub fn audio(&self) -> AudioIo {
        AudioIo {
            clock: self.shared.clock.clone(),
            sample_rate: self.shared.info.sample_rate,
            format: self.shared.info.format,
            latency: self.shared.info.latency.clone(),
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

    /// The receive flows, as controllers see them (ARC 0x3200): for a flow
    /// on both networks, its secondary address and which networks bring it.
    pub fn rx_flows(&self) -> Vec<ovsc_proto::arc::RxFlowInfo> {
        self.shared.state().rx_flows.clone()
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
                secondary: f.secondary,
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
                    addrs: vec![info.iface.ip],
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

    /// Aborts the tasks, stops the transmit thread, the ring sweeper and
    /// mDNS, and returns the aborted tasks' handles.
    fn stop_now(&mut self) -> Vec<JoinHandle<()>> {
        for (_, t) in &self.tasks {
            t.abort();
        }
        let _ = self.tx_commands.send(TxCommand::Shutdown);
        if let Some(t) = self.tx_thread.take() {
            let _ = t.join();
        }
        self.sweeper.stop();
        if let Some(m) = &self.mdns {
            m.stop();
        }
        std::mem::take(&mut self.tasks).into_iter().map(|(_, t)| t).collect()
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

    /// Another device that uses this device's name, as
    /// [`Device::name_conflict`]: `Some(None)` when there is none.
    pub fn name_conflict(&self) -> Option<Option<NameConflict>> {
        self.shared.upgrade().map(|s| s.state().name_conflict.clone())
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

/// A secondary interface must be another interface, on another network:
/// Dante's two networks are separate, and an address on the primary's
/// network would send the secondary's traffic through either interface.
/// Loopback addresses are allowed together, for tests.
fn check_secondary(primary: &Interface, secondary: Option<&Interface>) -> Result<()> {
    let Some(s) = secondary else { return Ok(()) };
    // A flow on both networks is received by one thread waiting on both
    // sockets with poll(2).
    if cfg!(not(unix)) {
        return Err(Error::Config(
            "secondary_interface: Dante redundancy needs macOS or Linux".into(),
        ));
    }
    // Two loopback addresses stand in for two interfaces in tests.
    let loopback = s.ip.is_loopback() && primary.ip.is_loopback();
    if s.ip == primary.ip || (s.name == primary.name && !loopback) {
        return Err(Error::Config(format!(
            "secondary_interface {} ({}) is the primary interface",
            s.name, s.ip
        )));
    }
    if loopback {
        return Ok(());
    }
    let on_network_of = |a: &Interface, ip: Ipv4Addr| {
        let mask = u32::from(a.netmask);
        mask != 0 && u32::from(a.ip) & mask == u32::from(ip) & mask
    };
    if on_network_of(primary, s.ip) || on_network_of(s, primary.ip) {
        return Err(Error::Config(format!(
            "secondary_interface {} ({}) is on the network of the primary interface {} ({}): \
             the secondary network of a redundant Dante system is a separate one",
            s.name, s.ip, primary.name, primary.ip
        )));
    }
    Ok(())
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
        tx_send_errors: c.tx_send_errors.load(Ordering::Relaxed),
        tx_last_send_error: c.tx_last_send_error.load(Ordering::Relaxed),
        tx_late_packets: c.tx_late_packets.load(Ordering::Relaxed),
        tx_max_lateness_ns: c.tx_max_lateness_ns.load(Ordering::Relaxed),
        tx_skipped_packets: c.tx_skipped_packets.load(Ordering::Relaxed),
        tx_resyncs: c.tx_resyncs.load(Ordering::Relaxed),
        tx_expired_flows: c.tx_expired_flows.load(Ordering::Relaxed),
        tx_not_realtime: c.tx_not_realtime.load(Ordering::Relaxed),
        rx_not_realtime: c.rx_not_realtime.load(Ordering::Relaxed),
        tx_engine_failed: c.tx_engine_failed.load(Ordering::Relaxed),
        rx_lost_packets: c.rx_lost_packets.load(Ordering::Relaxed),
        rx_early_packets: c.rx_early_packets.load(Ordering::Relaxed),
        rx_rejected_packets: c.rx_rejected_packets.load(Ordering::Relaxed),
        networks: [c.networks[0].read(), c.networks[1].read()],
    }
}

/// The state a device starts in. `config` already holds the saved name and
/// latency ([`DeviceConfig::with_saved`]). `predecessor` is the name the
/// device this one replaces in this process announced
/// ([`StartOptions::announced`]).
fn initial_state(
    config: &DeviceConfig,
    info: &DeviceInfo,
    saved: SavedState,
    predecessor: Option<String>,
) -> State {
    let name = if config.name.is_empty() { info.factory_name.clone() } else { config.name.clone() };
    // A name the device holds already is announced again without a probe
    // (see [`advertise`]): the one its predecessor in this process
    // announced, or else one claimed on this computer before, as saved
    // ([`SavedState::claimed_name`]). Without discovery the device
    // announces nothing, so it holds no name.
    let held = predecessor.as_deref().or_else(|| saved.claimed_name(&info.iface));
    let holds = info.discovery && held.is_some_and(|h| h.eq_ignore_ascii_case(&name));
    let announced = holds.then(|| name.clone());
    let (rx_names, tx_names) = saved.channel_names(&info.rx_channels, &info.tx_channels);
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
        name_conflict: None,
        announced,
        tx_names,
        rx_names,
        subscriptions,
        tx_flows: Vec::new(),
        rx_flows: Vec::new(),
        rx_flow_stats: Vec::new(),
        latency_override_ns: saved.latency_ns.filter(|&ns| ns == info.latency.ns()),
        remote_settings_locked: saved.lock_remote_settings.unwrap_or(config.lock_remote_settings),
        remote_settings_lock_set: saved.lock_remote_settings,
    }
}

/// How many other names [`advertise`] tries when probing finds the device's
/// name taken, before it announces one anyway and reports the conflict.
/// RFC 6762 §8.1 slows probing down after 15 conflicts.
const MAX_RENAMES: u32 = 15;

/// A device name that probing found taken.
struct Renaming {
    /// The name asked for (by the configuration, a controller or the app),
    /// which the alternatives are numbered after.
    wanted: String,
    /// The device that holds it.
    peer: Ipv4Addr,
    /// The alternative taken last, and how many were tried.
    picked: String,
    tries: u32,
}

/// Keeps the mDNS records in line with the device name and channel names,
/// and [`State::name_conflict`] up to date.
///
/// Each new device name is probed for before it is announced (RFC 6762
/// §8.1). When another device answers for it, this device takes the first
/// free name of `<name>-2`, `<name>-3`, ... and keeps it: it is saved like a
/// rename from a controller, so the device that held the name first keeps
/// it, and neither changes name at its next start. A conflict that only
/// shows once a name is announced renames nothing (see [`crate::mdns`]).
///
/// A name the device holds already, [`State::announced`] at the start
/// (taken over from the device it replaces in this process, or claimed on
/// this computer before, as saved: see [`StartOptions::announced`]), is
/// announced again without a probe, which another device announcing it too
/// would answer. Such a device is a conflict reported with `renamed_to:
/// None`, as one that appears while the name is announced. The device name
/// whose records are announced goes to [`State::announced`]
/// ([`Device::announced_name`]), and so to the state file.
async fn advertise(shared: Arc<Shared>, mdns: Mdns) {
    let mut watch = shared.watch();
    let mut conflicts = mdns.conflicts();
    let mut renaming: Option<Renaming> = None;
    let mut last = None;
    let mut claimed = shared.state().announced.clone();
    loop {
        let (name, records) = advertisement(&shared);
        if !claimed.as_ref().is_some_and(|c| c.eq_ignore_ascii_case(&name)) {
            let taken = mdns.probe(&records).await;
            if !shared.name().eq_ignore_ascii_case(&name) {
                // Renamed meanwhile: probe for the new name instead.
                continue;
            }
            if let Some(conflict) = taken {
                let mut r = match renaming.take() {
                    Some(r) if r.picked.eq_ignore_ascii_case(&name) => r,
                    _ => Renaming {
                        wanted: name.clone(),
                        peer: conflict.peer,
                        picked: name.clone(),
                        tries: 0,
                    },
                };
                r.tries += 1;
                if r.tries <= MAX_RENAMES {
                    let new = mdns::alternative_name(&r.wanted, r.tries + 1);
                    error!(
                        "the device at {} already uses the name {name}: this device takes the \
                         name {new}. Rename one of them to choose their names.",
                        conflict.peer
                    );
                    shared.modify(|s| {
                        if s.name.eq_ignore_ascii_case(&name) {
                            s.name = new.clone();
                        }
                    });
                    r.picked = new;
                    renaming = Some(r);
                    continue;
                }
                error!(
                    "{MAX_RENAMES} names after {} are taken; announcing {name} although the \
                     device at {} uses it",
                    r.wanted, conflict.peer
                );
            }
            claimed = Some(name);
        }
        if last.as_ref() != Some(&records) {
            mdns.set_records(records.clone()).await;
            last = Some(records);
            // Saved, so that a new process on this computer keeps the
            // name. Only when it changes (not when channels are renamed):
            // `modify` wakes every task that follows the state.
            if shared.state().announced != claimed {
                shared.modify(|s| s.announced = claimed.clone());
            }
        }
        report_name_conflict(&shared, renaming.as_ref(), &conflicts.borrow_and_update());
        tokio::select! {
            changed = watch.changed() => if changed.is_err() {
                return;
            },
            changed = conflicts.changed() => if changed.is_err() {
                return;
            },
        }
    }
}

/// Sets [`State::name_conflict`]: a device announcing our name right now,
/// or else the name that probing found taken, while the device keeps the
/// one it took instead. Status only: it doesn't wake [`Shared::watch`].
fn report_name_conflict(shared: &Shared, renaming: Option<&Renaming>, live: &[mdns::Conflict]) {
    let mut s = shared.state();
    s.name_conflict = match live.first() {
        Some(c) => Some(NameConflict { name: s.name.clone(), peer: c.peer, renamed_to: None }),
        None => renaming.filter(|r| r.picked.eq_ignore_ascii_case(&s.name)).map(|r| NameConflict {
            name: r.wanted.clone(),
            peer: r.peer,
            renamed_to: Some(r.picked.clone()),
        }),
    };
}

/// The device name and the mDNS records of the device as it is now.
fn advertisement(shared: &Shared) -> (String, Vec<Record>) {
    let s = shared.state();
    let info = &shared.info;
    let records = mdns::device_records(&Advertisement {
        name: &s.name,
        host: &info.factory_name,
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
            .map(|(i, (f, c))| (f.as_str(), c.as_str(), own_channel_txt(info, i as u16 + 1)))
            .collect(),
    });
    (s.name.clone(), records)
}

/// What an audio backend needs: the media clock and the channel rings.
///
/// * Receive: read ring `rx[ch]` at `now - latency` (never later: newer
///   samples may not have arrived yet). The latency changes while the
///   device runs ([`Device::restart_rx`]): read [`AudioIo::latency_samples`]
///   anew for each period, as it costs one atomic load.
/// * Transmit: write ring `tx[ch]` ahead of `now` (at least your callback
///   period plus the transmit guard), so samples are there when the network
///   thread picks them up.
#[derive(Clone)]
pub struct AudioIo {
    pub clock: MediaClock,
    pub sample_rate: u32,
    pub format: SampleFormat,
    /// The device's receive latency, shared with it.
    pub latency: RxLatency,
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

    /// The receive latency now, in samples: read the receive rings this far
    /// behind the media clock. Neither locks nor allocates.
    pub fn latency_samples(&self) -> u64 {
        self.latency.samples()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Channels, Ports};

    #[cfg(unix)]
    #[test]
    fn the_secondary_interface_is_another_one_on_another_network() {
        let iface = |name: &str, ip: [u8; 4], mask: [u8; 4]| Interface {
            name: name.into(),
            ip: ip.into(),
            netmask: mask.into(),
            gateway: Ipv4Addr::UNSPECIFIED,
            mac: [0; 6],
            link_speed_mbps: 1000,
        };
        let primary = iface("en0", [192, 168, 1, 20], [255, 255, 255, 0]);
        let ok = |s: &Interface| check_secondary(&primary, Some(s)).is_ok();
        assert!(check_secondary(&primary, None).is_ok());
        // The usual secondary network of a Dante system.
        assert!(ok(&iface("en7", [172, 31, 1, 20], [255, 255, 0, 0])));
        // The primary again, by name or address, or the primary's network.
        assert!(!ok(&iface("en0", [172, 31, 1, 20], [255, 255, 0, 0])));
        assert!(!ok(&iface("en7", [192, 168, 1, 20], [255, 255, 255, 0])));
        assert!(!ok(&iface("en7", [192, 168, 1, 21], [255, 255, 255, 0])));
        // A wider netmask that covers the primary.
        assert!(!ok(&iface("en7", [192, 168, 2, 20], [255, 255, 0, 0])));
        // Two loopback addresses, as the tests use.
        let lo = iface("lo", [127, 0, 0, 1], [255, 0, 0, 0]);
        assert!(check_secondary(&lo, Some(&iface("lo", [127, 0, 0, 2], [255, 0, 0, 0]))).is_ok());
    }

    /// A device's rings that held audio from before it started (rings in a
    /// region shared with a driver outlive the device) are swept when it
    /// starts: nothing written a day ago can come back when the 32-bit tags
    /// match again.
    #[tokio::test]
    async fn a_device_sweeps_its_rings() {
        let clock = ovsc_clock::system_clock();
        let rate = 48_000;
        let ring = || Arc::new(TimedRing::new(crate::buffer::DEFAULT_CAPACITY));
        let rings = ExternalRings { rx: vec![ring(), ring()], tx: vec![ring(), ring()] };
        let day_ago = clock.now_samples(rate).unwrap() - u64::from(rate) * 86_400;
        for r in rings.rx.iter().chain(&rings.tx) {
            r.write(day_ago, &[0x7fff_ff00; 64]);
        }
        let config = DeviceConfig {
            name: "sweep-dev".into(),
            interface: "127.0.0.1".into(),
            sample_rate: rate,
            tx_channels: Channels::Count(2),
            rx_channels: Channels::Count(2),
            ports: Ports { arc: 25130, cmc: 25131, flow_control: 25132, settings: 25133 },
            discovery: false,
            process_id: 41,
            ..Default::default()
        };
        let options = StartOptions { rings: Some(rings.clone()), ..Default::default() };
        let device = Device::start_with_options(config, clock, options).await.unwrap();
        let all = || rings.rx.iter().chain(&rings.tx);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while all().any(|r| r.read_one(day_ago).is_some()) {
            assert!(tokio::time::Instant::now() < deadline, "rings not swept");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(all().all(|r| r.read_one(day_ago + 63).is_none()));
        // Shutting down stops the sweeper, which then holds no ring.
        device.shutdown().await;
        assert!(all().all(|r| Arc::strong_count(r) == 1), "only the test holds the rings");
    }
}
