//! Mutable device state shared by the servers, and change notification.

use std::sync::atomic::{AtomicU32, AtomicU64};
use std::sync::{Arc, Mutex, MutexGuard};

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
}

/// When one receive flow's packets arrive, written by its receive thread.
#[derive(Debug, Default)]
pub struct FlowStats {
    /// The longest a packet took, from its timestamp to its arrival, in
    /// samples, since the heartbeat last took it.
    pub max_arrival: AtomicU32,
    /// Packets that arrived later than the receive latency.
    pub late_packets: AtomicU64,
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
        let ms = ns as f64 / 1e6;
        if !(crate::config::MIN_LATENCY_MS..=crate::config::MAX_LATENCY_MS).contains(&ms) {
            return Err(crate::Error::Config(format!(
                "latency {ms} ms is outside {} to {} ms",
                crate::config::MIN_LATENCY_MS,
                crate::config::MAX_LATENCY_MS
            )));
        }
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
