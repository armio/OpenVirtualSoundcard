//! Mutable device state shared by the servers, and change notification.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

use tokio::sync::{mpsc, watch};

use ovsc_clock::MediaClock;
use ovsc_proto::arc::{RxFlowInfo, SubscriptionStatus, TxFlowInfo};

use crate::buffer::TimedRing;
use crate::device::ExternalRings;
use crate::info::DeviceInfo;
use crate::{Error, Result};

/// A receive channel's subscription.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subscription {
    pub tx_channel: String,
    pub tx_device: String,
    pub status: SubscriptionStatus,
}

/// State that controllers can change or observe.
#[derive(Clone, Debug)]
pub struct State {
    /// Current device name (user-assigned or factory).
    pub name: String,
    /// Another device using the device name, as mDNS shows it. Status only:
    /// setting it doesn't go through [`Shared::modify`].
    pub name_conflict: Option<crate::mdns::NameConflict>,
    /// Current transmit channel names (user-assigned or factory).
    pub tx_names: Vec<String>,
    /// Current receive channel names.
    pub rx_names: Vec<String>,
    /// Per receive channel.
    pub subscriptions: Vec<Option<Subscription>>,
    /// For ARC flow listings.
    pub tx_flows: Vec<TxFlowInfo>,
    pub rx_flows: Vec<RxFlowInfo>,
    /// Timing of each receive flow, by flow id, for the heartbeat.
    pub rx_flow_stats: Vec<(u16, Arc<FlowStats>)>,
    /// A receive latency set by a controller or the app, in ns, in place of
    /// the configured one. The running device keeps its latency
    /// ([`DeviceInfo::latency_ns`]); a new one takes effect when the device
    /// restarts, which [`Shared::latency_requests`] asks for.
    pub latency_override_ns: Option<u32>,
    /// The show lock: while it is on, controllers may not change the
    /// latency, the sample rate or the encoding, which interrupt audio both
    /// ways, nor the device or channel names, which other devices'
    /// subscriptions depend on. Subscriptions stay open, and the app's
    /// changes still apply. See [`Shared::lock_remote_settings`].
    pub remote_settings_locked: bool,
    /// The lock as the app last set it, saved with the state so that it
    /// survives restarts; `None` until then, when the configuration's
    /// `lock_remote_settings` applies.
    pub remote_settings_lock_set: Option<bool>,
}

impl State {
    /// Index of the transmit channel called `name` (current or factory name).
    pub fn tx_channel_index(&self, info: &DeviceInfo, name: &str) -> Option<usize> {
        self.tx_names
            .iter()
            .position(|n| n.eq_ignore_ascii_case(name))
            .or_else(|| info.tx_channels.iter().position(|n| n.eq_ignore_ascii_case(name)))
    }
}

/// Messages to the conmon task.
#[derive(Debug)]
pub enum Notify {
    /// Receive channels (0-based) whose name or subscription changed.
    RxChannelsChanged(Vec<usize>),
}

/// Packet counters, written by the transmit thread and the receive threads
/// (relaxed increments) and read for status displays.
#[derive(Debug, Default)]
pub struct Counters {
    /// Audio packets sent.
    pub tx_packets: AtomicU64,
    /// Packets sent while a channel they carry had no audio from the backend.
    pub tx_underruns: AtomicU64,
    /// Audio packets received.
    pub rx_packets: AtomicU64,
    /// Received packets older than the receive latency on arrival.
    pub rx_late_packets: AtomicU64,
    /// Audio packets the network stack refused to send (a full queue,
    /// no route, ...) or reported undeliverable (ICMP host or port
    /// unreachable).
    pub tx_send_errors: AtomicU64,
    /// The OS error code of the latest failed send, 0 if none or unknown.
    pub tx_last_send_error: AtomicI32,
    /// Audio packets sent more than the transmit thread's late allowance
    /// (250 µs) after their send time, timestamp + guard. The first packet
    /// of a flow, and the first after a resync, are not counted: they are
    /// up to one packet late by design.
    pub tx_late_packets: AtomicU64,
    /// The latest any audio packet left after its send time, in ns, since
    /// the device started (the packets [`Counters::tx_late_packets`] skips
    /// excluded).
    pub tx_max_lateness_ns: AtomicU64,
    /// Audio packets never sent: overdue by more than any receiver can
    /// play after a stall or a media clock step, or (never expected) too
    /// large to encode.
    pub tx_skipped_packets: AtomicU64,
    /// Times a flow's timeline restarted after a stall or a media clock
    /// step.
    pub tx_resyncs: AtomicU64,
    /// Unicast flows removed after 4 s without keepalives.
    pub tx_expired_flows: AtomicU64,
    /// Set when the transmit thread runs without real-time scheduling.
    pub tx_not_realtime: AtomicBool,
    /// Set once a receive thread has started without real-time scheduling,
    /// until the device restarts.
    pub rx_not_realtime: AtomicBool,
    /// Set when the transmit thread has stopped on a panic: no flow sends
    /// audio any more, and flow requests are refused until the device
    /// restarts.
    pub tx_engine_failed: AtomicBool,
    /// Packets missing from received flows: gaps in their timestamps.
    pub rx_lost_packets: AtomicU64,
    /// Received packets stamped ahead of our clock by more than a packet,
    /// a sign that the transmitter's clock disagrees with ours.
    pub rx_early_packets: AtomicU64,
    /// Datagrams dropped by receive flows: not from the flow's transmitter,
    /// or not whole frames of the flow's slots.
    pub rx_rejected_packets: AtomicU64,
}

/// What one receive flow's packets look like, written by its receive thread
/// (and [`FlowStats::degraded`] by the subscription manager).
#[derive(Debug, Default)]
pub struct FlowStats {
    /// The longest a packet took, from its timestamp to its arrival, in
    /// samples, since the heartbeat last took it.
    pub max_arrival: AtomicU32,
    /// Packets that arrived later than the flow's latency.
    pub late_packets: AtomicU64,
    /// Packets accepted and written into the rings.
    pub packets: AtomicU64,
    /// Packets missing from the timestamp sequence.
    pub lost_packets: AtomicU64,
    /// Timestamp jumps too large to be packet loss (a transmitter restart,
    /// a clock step).
    pub discontinuities: AtomicU64,
    /// Packets stamped more than a packet ahead of our clock.
    pub early_packets: AtomicU64,
    /// The furthest ahead of our clock a packet was stamped, in samples,
    /// since someone last took it.
    pub max_early: AtomicU32,
    /// Datagrams dropped: from another address or source port than the
    /// flow's transmitter, or not whole frames of the flow's slots.
    pub rejected_packets: AtomicU64,
    /// The last dropped datagram, for diagnostics: source IPv4 address
    /// (high 32 bits), source port (next 16) and length (low 16).
    pub last_rejected: AtomicU64,
    /// Set while the flow recently lost packets or received them too late
    /// to play: audible trouble, though the flow still counts as receiving.
    pub degraded: AtomicBool,
}

/// A change a controller made, or tried to make, to a setting that
/// interrupts audio or that other devices depend on: the latency, the sample
/// rate, the encoding or a name. Kept so that the app can show who did what
/// once the audio dropped, or why a controller's change did not take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteChange {
    /// When, by the system clock.
    pub at: SystemTime,
    /// The controller's address.
    pub from: SocketAddr,
    /// What it asked for, in words: "the receive latency to 2 ms".
    pub what: String,
    /// Whether it was refused because remote settings are locked.
    pub refused: bool,
}

/// The last few [`RemoteChange`]s, oldest first. Clones share the log: a
/// daemon hands it to the device it restarts
/// ([`crate::StartOptions::remote_changes`]), so that a latency change that
/// restarted the device is still listed afterwards.
#[derive(Clone, Debug, Default)]
pub struct RemoteChanges(Arc<Mutex<VecDeque<RemoteChange>>>);

impl RemoteChanges {
    /// How many changes the log keeps.
    pub const KEPT: usize = 8;

    /// Adds `change`, dropping the oldest beyond [`RemoteChanges::KEPT`].
    pub fn push(&self, change: RemoteChange) {
        let mut log = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if log.len() == Self::KEPT {
            log.pop_front();
        }
        log.push_back(change);
    }

    /// The changes kept, oldest first.
    pub fn recent(&self) -> Vec<RemoteChange> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect()
    }
}

/// A sample rate or bit depth that a controller asked for, applied when the
/// device next starts; see [`Shared::request_format`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FormatRequest {
    pub sample_rate: Option<u32>,
    pub bits_per_sample: Option<u16>,
}

/// What every task of a device shares.
pub struct Shared {
    pub info: DeviceInfo,
    state: Mutex<State>,
    /// Bumped whenever configuration-like state (names, subscriptions)
    /// changes. Status-only updates don't bump it.
    generation: watch::Sender<u64>,
    pub clock: MediaClock,
    pub rx_rings: Vec<Arc<TimedRing>>,
    pub tx_rings: Vec<Arc<TimedRing>>,
    /// Shared with the receive threads, which don't hold the rest.
    pub counters: Arc<Counters>,
    notify: mpsc::UnboundedSender<Notify>,
    /// The latest latency asked for, in ns; see [`Shared::request_latency`].
    latency_request: watch::Sender<Option<u32>>,
    /// Whether controllers may change the sample rate and the bit depth:
    /// only when something listens to [`Shared::format_requests`].
    pub format_configurable: bool,
    /// The latest format asked for; see [`Shared::request_format`].
    format_request: watch::Sender<FormatRequest>,
    /// What controllers changed or tried to change; see [`RemoteChange`].
    pub remote_changes: RemoteChanges,
}

impl Shared {
    /// Creates the shared state, with rings of `ring_capacity` samples on the
    /// heap, or with `rings` when given. Those must match the channel counts
    /// and have exactly `ring_capacity` samples each.
    pub fn new(
        info: DeviceInfo,
        state: State,
        clock: MediaClock,
        ring_capacity: usize,
        rings: Option<ExternalRings>,
        format_configurable: bool,
        remote_changes: RemoteChanges,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<Notify>)> {
        let (rx_rings, tx_rings) = match rings {
            Some(r) => {
                check_rings("rx", &r.rx, info.rx_channels.len(), ring_capacity)?;
                check_rings("tx", &r.tx, info.tx_channels.len(), ring_capacity)?;
                (r.rx, r.tx)
            }
            None => {
                let rings = |n: usize| (0..n).map(|_| Arc::new(TimedRing::new(ring_capacity)));
                (rings(info.rx_channels.len()).collect(), rings(info.tx_channels.len()).collect())
            }
        };
        let (notify, notify_rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Self {
            rx_rings,
            tx_rings,
            info,
            state: Mutex::new(state),
            generation: watch::channel(0).0,
            clock,
            counters: Arc::default(),
            notify,
            latency_request: watch::channel(None).0,
            format_configurable,
            format_request: watch::channel(FormatRequest::default()).0,
            remote_changes,
        });
        Ok((shared, notify_rx))
    }

    /// Read access to the state. Keep the guard short-lived.
    pub fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Changes names or subscriptions and wakes everyone who reacts to them.
    pub fn modify<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        let r = f(&mut self.state());
        self.generation.send_modify(|g| *g += 1);
        r
    }

    /// Sets the receive latency to `ns` from the next start of the device on,
    /// saves it with the other state, and tells [`Shared::latency_requests`]
    /// listeners, who restart the device to apply it.
    pub fn request_latency(&self, ns: u32) -> crate::Result<()> {
        // The rule the device checks when it restarts with this latency.
        let guard_us = (self.info.tx_guard_ns / 1000) as u32;
        crate::config::check_latency_ms("the latency", ns as f64 / 1e6, guard_us)?;
        self.modify(|s| s.latency_override_ns = Some(ns));
        self.latency_request.send_replace(Some(ns));
        Ok(())
    }

    /// Wakes up whenever [`Shared::request_latency`] asks for a latency.
    pub fn latency_requests(&self) -> watch::Receiver<Option<u32>> {
        self.latency_request.subscribe()
    }

    /// The latency the device will run at from its next start, in ns.
    pub fn configured_latency_ns(&self) -> u32 {
        self.state().latency_override_ns.unwrap_or(self.info.latency_ns)
    }

    /// Asks for a new sample rate or bit depth, for [`Shared::format_requests`]
    /// listeners to apply (the device restarts). Fails unless the device is
    /// [`Shared::format_configurable`] and the values are valid.
    pub fn request_format(&self, request: FormatRequest) -> crate::Result<()> {
        if !self.format_configurable {
            return Err(Error::Config("the format of this device is fixed".into()));
        }
        if let Some(rate) = request.sample_rate {
            if !crate::config::SAMPLE_RATES.contains(&rate) {
                return Err(Error::Config(format!("unsupported sample rate {rate}")));
            }
        }
        if let Some(bits) = request.bits_per_sample {
            if !crate::config::BITS_PER_SAMPLE.contains(&bits) {
                return Err(Error::Config(format!("unsupported bit depth {bits}")));
            }
        }
        self.format_request.send_modify(|r| {
            r.sample_rate = request.sample_rate.or(r.sample_rate);
            r.bits_per_sample = request.bits_per_sample.or(r.bits_per_sample);
        });
        Ok(())
    }

    /// Wakes up whenever [`Shared::request_format`] asks for a format.
    pub fn format_requests(&self) -> watch::Receiver<FormatRequest> {
        self.format_request.subscribe()
    }

    /// The format asked for and not applied yet.
    pub fn pending_format(&self) -> FormatRequest {
        *self.format_request.borrow()
    }

    /// Whether the show lock is on: see [`State::remote_settings_locked`].
    pub fn remote_settings_locked(&self) -> bool {
        self.state().remote_settings_locked
    }

    /// Turns the show lock on or off, and saves that with the state: from
    /// then on it replaces the configuration's `lock_remote_settings`. Only
    /// the device's owner (the app) calls this; controllers cannot.
    pub fn lock_remote_settings(&self, locked: bool) {
        self.modify(|s| {
            s.remote_settings_locked = locked;
            s.remote_settings_lock_set = Some(locked);
        });
    }

    /// Notes a controller's change, or refused attempt, in
    /// [`Shared::remote_changes`].
    pub fn record_remote_change(&self, from: SocketAddr, what: String, refused: bool) {
        self.remote_changes.push(RemoteChange { at: SystemTime::now(), from, what, refused });
    }

    /// Subscribes to [`Shared::modify`] notifications.
    pub fn watch(&self) -> watch::Receiver<u64> {
        self.generation.subscribe()
    }

    pub fn notify(&self, n: Notify) {
        let _ = self.notify.send(n);
    }

    /// Current device name.
    pub fn name(&self) -> String {
        self.state().name.clone()
    }
}

fn check_rings(
    what: &str,
    rings: &[Arc<TimedRing>],
    channels: usize,
    capacity: usize,
) -> Result<()> {
    if rings.len() != channels {
        return Err(Error::Config(format!(
            "{} external {what} rings for {channels} {what} channels",
            rings.len()
        )));
    }
    if let Some(r) = rings.iter().find(|r| r.capacity() != capacity) {
        return Err(Error::Config(format!(
            "external {what} ring of {} samples, ring_capacity is {capacity}",
            r.capacity()
        )));
    }
    Ok(())
}
