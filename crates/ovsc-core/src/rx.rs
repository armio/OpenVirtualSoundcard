//! Receive side: turns subscriptions into flows and flows into samples.
//!
//! The [`RxManager`] is a reconcile loop. Whenever subscriptions change, a
//! lookup or request it started finishes, and four times a second anyway, it
//! compares what controllers asked for with the flows it has, and fixes the
//! difference:
//!
//! 1. routes that no longer match a subscription are removed (a
//!    subscription that only changed its spelling, but still names the same
//!    transmit channel, keeps its route);
//! 2. flows that stopped delivering packets are stopped, and so are flows
//!    that have carried nothing for a few seconds;
//! 3. subscribed channels already carried by a flow are routed to it;
//! 4. the remaining ones are resolved (mDNS), grouped per transmitter and
//!    requested, in free slots of existing flows or as new unicast flows;
//! 5. per-channel statuses are published for ARC listings.
//!
//! A pass never waits for the network. Lookups and requests run as
//! background jobs, at most one request job per transmitter at a time, and
//! the loop applies their results when they finish, so a transmitter that
//! doesn't answer holds up neither the others nor the detection of dead
//! flows. A failed attempt is retried after a delay counted from the failure:
//! short when nobody answered or our own network failed, so that audio comes
//! back soon after the network does.
//!
//! Each flow has a real-time thread that receives its packets, keeps only
//! those from its transmitter that have its size, writes the samples into
//! the receive rings at their media timestamp and sends keepalives back to
//! the transmitter. It reads its routing from an immutable table that the
//! manager replaces as a whole, so it never waits for the manager.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinError, JoinSet};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use ovsc_clock::{MediaClock, local_now_ns};
use ovsc_proto::arc::{RxFlowInfo, SubscriptionStatus};
use ovsc_proto::audio::{self, AudioPacket, Sample, SampleFormat};
use ovsc_proto::dbcp::{self, FlowHandle, FlowRequest};
use ovsc_proto::discovery::ChannelTxt;

use crate::buffer::TimedRing;
use crate::client;
use crate::directory::{Directory, ResolvedChannel};
use crate::info::{FPP_MAX, FPP_MIN, MAX_CHANNELS_PER_TX_FLOW, MAX_RX_FLOWS, PCM_TYPE};
use crate::net::bind_udp;
use crate::state::{Counters, FlowStats, Notify, Shared};
use crate::{Error, Result};

const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(250);
/// A flow that delivers nothing for this long is torn down and re-requested.
/// Transmitters give up on a flow 4 s after its last keepalive
/// (PROTOCOL.md §8.5).
const FLOW_TIMEOUT_NS: u64 = 3_000_000_000;
/// How often the manager looks at its flows when nothing else wakes it: how
/// soon a dead flow is noticed and a failed request is retried.
const TICK: Duration = Duration::from_millis(250);
/// How long a flow left without routes is kept before it is stopped. A
/// channel subscribed again meanwhile (or under another spelling) reuses it,
/// without asking the transmitter for a new flow.
const EMPTY_LINGER: Duration = Duration::from_secs(3);
/// How long after a channel loses its last route its ring is cleared: long
/// enough for a receive thread that was writing a packet with the old
/// routing to have finished.
const CLEAR_DELAY: Duration = Duration::from_millis(100);
/// How long a replaced routing table is kept, so that a receive thread still
/// reading it never holds the last reference and frees it.
const RETIRE_AFTER: Duration = Duration::from_secs(1);
/// How long a lookup result waits for its transmitter to be free.
const RESOLVED_TTL: Duration = Duration::from_secs(10);
/// A flow's problems are logged at most this often.
const REPORT_INTERVAL: Duration = Duration::from_secs(10);
/// A flow counts as degraded for this long after it last lost a packet or
/// received one too late.
const DEGRADED_HOLD: Duration = Duration::from_secs(5);
/// A receive thread that follows one source port of its transmitter moves to
/// another only after the first has been quiet this long.
const SOURCE_HOLD_NS: u64 = KEEPALIVE_INTERVAL.as_nanos() as u64;

/// Subscription statuses without a variant of their own (PROTOCOL.md
/// §4.14).
mod status {
    use ovsc_proto::arc::SubscriptionStatus;

    /// 0x000F NO_CONNECTION: the transmitter doesn't answer.
    pub const NO_CONNECTION: SubscriptionStatus = SubscriptionStatus::Other(0x000F);
    /// 0x0010 CHANNEL_FORMAT: its sample rate or encoding isn't ours.
    pub const CHANNEL_FORMAT: SubscriptionStatus = SubscriptionStatus::Other(0x0010);
    /// 0x0012 NO_RX: we have no receive flow left.
    pub const NO_RX: SubscriptionStatus = SubscriptionStatus::Other(0x0012);
    /// 0x0013 RX_FAIL: receiving failed on our side (our address or
    /// interface), not the transmitter's.
    pub const RX_FAIL: SubscriptionStatus = SubscriptionStatus::Other(0x0013);
    /// 0x001A CHANNEL_LATENCY: the transmitter demands more latency than we
    /// can play at.
    pub const CHANNEL_LATENCY: SubscriptionStatus = SubscriptionStatus::Other(0x001A);
}

/// A subscription as a lookup key: transmit channel and device, lowercase.
type Key = (String, String);

fn key(channel: &str, device: &str) -> Key {
    (channel.to_ascii_lowercase(), device.to_ascii_lowercase())
}

/// How soon a failed lookup or request is tried again: `first`, doubling
/// with each further failure up to `max`, counted from the failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Retry {
    first: Duration,
    max: Duration,
}

impl Retry {
    /// Nobody answered, or our own network failed: try again soon, so that
    /// audio comes back soon after the network does. (A request that times
    /// out has already waited 2.1 s, sending three times.)
    const FAST: Retry = Retry { first: Duration::from_millis(250), max: Duration::from_secs(1) };
    /// Nothing answered a lookup, which itself waits 2 s.
    const LOOKUP: Retry = Retry { first: Duration::from_millis(500), max: Duration::from_secs(2) };
    /// The transmitter refused, or the channel can't be received as it is:
    /// asking again right away won't help.
    const REFUSED: Retry = Retry { first: Duration::from_secs(1), max: Duration::from_secs(5) };

    /// The delay after the `failures`th failure in a row.
    fn delay(self, failures: u32) -> Duration {
        let doublings = failures.saturating_sub(1).min(8);
        self.first.saturating_mul(1u32 << doublings).min(self.max)
    }
}

/// What a failed lookup or request shows, and how soon it is retried.
fn classify(e: &Error) -> (SubscriptionStatus, Retry) {
    match e {
        Error::Timeout(_) => (status::NO_CONNECTION, Retry::FAST),
        Error::Refused(dbcp::error::TOO_MANY_FLOWS) => {
            (SubscriptionStatus::TxNoFlows, Retry::REFUSED)
        }
        Error::Refused(dbcp::error::SAMPLE_RATE_MISMATCH) => {
            (status::CHANNEL_FORMAT, Retry::REFUSED)
        }
        Error::Refused(_) | Error::Protocol(_) | Error::Config(_) => {
            (SubscriptionStatus::TxFail, Retry::REFUSED)
        }
        Error::Io(e) if is_local(e) => (status::RX_FAIL, Retry::FAST),
        // Unreachable host (no ARP answer) and the like.
        Error::Io(_) => (status::NO_CONNECTION, Retry::FAST),
        Error::NotFound(_) => (SubscriptionStatus::Unresolved, Retry::LOOKUP),
    }
}

/// Whether a socket error is about our side: our address is gone (an IP
/// change, the interface went away) or our network is down.
fn is_local(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(
        e.kind(),
        AddrNotAvailable | AddrInUse | NetworkDown | NetworkUnreachable | PermissionDenied
    )
}

/// What a receive thread routes packets by. The manager replaces it as a
/// whole and never changes it in place.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Routes {
    /// Slot → local receive channels fed by that slot.
    slots: Vec<Vec<usize>>,
    /// Per slot, the earliest packet timestamp it may write. A slot that
    /// just took another transmit channel still carries the previous one in
    /// packets the transmitter sent before it switched.
    fresh_from: Vec<u64>,
}

impl Routes {
    fn new(slots: usize) -> Self {
        Self { slots: vec![Vec::new(); slots], fresh_from: vec![0; slots] }
    }

    fn feeds(&self, ch: usize) -> bool {
        self.slots.iter().any(|t| t.contains(&ch))
    }

    fn is_empty(&self) -> bool {
        self.slots.iter().all(Vec::is_empty)
    }
}

/// The routing a receive thread reads, swapped by the manager.
type Routing = Arc<ArcSwap<Routes>>;

struct RxFlow {
    id: u16,
    tx_device: String,
    /// The transmitter's flow-control address; also where its audio comes
    /// from.
    control: SocketAddrV4,
    handle: FlowHandle,
    /// A spelling of the transmit channel requested for each slot.
    slot_names: Vec<String>,
    /// Transmit channel id carried in each slot (0: none).
    tx_ids: Vec<u16>,
    /// Bits per sample on the wire.
    bits: u16,
    /// What the receive thread reads, and the table last stored in it.
    routing: Routing,
    routes: Arc<Routes>,
    /// The subscription each routed local channel was routed for.
    spelled: HashMap<usize, Key>,
    /// Slots a request in flight is changing.
    changing: HashSet<usize>,
    /// The transmitter refused to change the flow's channels, as some Dante
    /// devices do (PROTOCOL.md §6.5): new channels get flows of their own.
    grow_refused: bool,
    /// Since when the flow has had no routes.
    empty_since: Option<Instant>,
    port: u16,
    /// The latency the flow plays at, in ns: ours or more (see
    /// [`flow_latency`]).
    latency_ns: u32,
    /// Local time of the last packet, 0 if none yet.
    last_packet_ns: Arc<AtomicU64>,
    /// What its packets look like.
    stats: Arc<FlowStats>,
    health: Health,
    created_ns: u64,
    worker: Worker,
}

impl RxFlow {
    fn is_receiving(&self, now_ns: u64) -> bool {
        let last = self.last_packet_ns.load(Ordering::Relaxed);
        last != 0 && now_ns.saturating_sub(last) < 1_000_000_000
    }

    fn is_dead(&self, now_ns: u64) -> bool {
        let last = self.last_packet_ns.load(Ordering::Relaxed).max(self.created_ns);
        now_ns.saturating_sub(last) > FLOW_TIMEOUT_NS
    }

    /// Whether `slot` carries the transmit channel `r` (and no request is
    /// changing it).
    fn carries(&self, r: &ResolvedChannel, slot: usize) -> bool {
        r.txt.id != 0
            && SocketAddrV4::new(r.addr, r.flow_control_port) == self.control
            && self.tx_ids.get(slot) == Some(&r.txt.id)
            && !self.changing.contains(&slot)
    }

    /// Hands the receive thread `routes` if they differ from the current
    /// ones. The replaced table goes to `retired` for a while: the receive
    /// thread may be reading it, and must not be the one that frees it.
    fn set_routes(&mut self, routes: Routes, retired: &mut Vec<(Instant, Arc<Routes>)>) {
        if *self.routes == routes {
            return;
        }
        let routes = Arc::new(routes);
        self.routing.store(routes.clone());
        retired.push((Instant::now(), std::mem::replace(&mut self.routes, routes)));
    }

    /// Routes local channel `ch`, subscribed as `key`, to `slot`.
    fn route(
        &mut self,
        slot: usize,
        ch: usize,
        key: &Key,
        retired: &mut Vec<(Instant, Arc<Routes>)>,
    ) {
        let mut routes = (*self.routes).clone();
        routes.slots[slot].push(ch);
        self.spelled.insert(ch, key.clone());
        self.set_routes(routes, retired);
    }

    /// A summary of `t`, the problems since the last report.
    fn report(&self, t: Trouble, since: Duration, sample_rate: u32) -> String {
        let ms = |samples: u64| samples as f64 * 1000.0 / sample_rate.max(1) as f64;
        let mut parts = Vec::new();
        if t.late > 0 {
            parts.push(format!(
                "{} packets arrived after their play time ({:.2} ms)",
                t.late,
                self.latency_ns as f64 / 1e6
            ));
        }
        if t.lost > 0 {
            parts.push(format!("{} packets lost", t.lost));
        }
        if t.jumps > 0 {
            parts.push(format!("{} timestamp jumps", t.jumps));
        }
        if t.early > 0 {
            let lead = self.stats.max_early.swap(0, Ordering::Relaxed);
            parts.push(format!(
                "{} packets stamped up to {:.2} ms ahead of our clock",
                t.early,
                ms(lead as u64)
            ));
        }
        if t.rejected > 0 {
            let last = self.stats.last_rejected.load(Ordering::Relaxed);
            let from = SocketAddrV4::new(Ipv4Addr::from((last >> 32) as u32), (last >> 16) as u16);
            parts.push(format!(
                "{} datagrams dropped (the last from {from}, {} bytes; the flow's packets come \
                 from {} and have {} bytes, or another whole number of {}-byte frames)",
                t.rejected,
                last & 0xffff,
                self.control.ip(),
                self.worker.params.packet_len(),
                self.worker.params.stride(),
            ));
        }
        format!(
            "rx flow {} from {} in the last {:.0} s: {}",
            self.id,
            self.tx_device,
            since.as_secs_f64(),
            parts.join(", ")
        )
    }
}

/// A flow's problem counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Trouble {
    late: u64,
    lost: u64,
    jumps: u64,
    early: u64,
    rejected: u64,
}

impl Trouble {
    fn read(stats: &FlowStats) -> Self {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        Self {
            late: get(&stats.late_packets),
            lost: get(&stats.lost_packets),
            jumps: get(&stats.discontinuities),
            early: get(&stats.early_packets),
            rejected: get(&stats.rejected_packets),
        }
    }

    fn since(self, before: Self) -> Self {
        Self {
            late: self.late.saturating_sub(before.late),
            lost: self.lost.saturating_sub(before.lost),
            jumps: self.jumps.saturating_sub(before.jumps),
            early: self.early.saturating_sub(before.early),
            rejected: self.rejected.saturating_sub(before.rejected),
        }
    }

    fn is_empty(self) -> bool {
        self == Self::default()
    }

    /// Problems you can hear: samples that never made it to the rings.
    fn audible(self) -> u64 {
        self.late + self.lost
    }
}

/// Rate-limits a flow's problem reports, which the receive thread only
/// counts, and tracks whether the flow is degraded.
#[derive(Debug)]
struct Health {
    /// The counters at the last report.
    reported: Trouble,
    /// Since when the next report counts, and when the last one was made.
    window: Instant,
    reported_at: Option<Instant>,
    /// Audible problems last seen, and when they last grew.
    audible: u64,
    audible_at: Option<Instant>,
}

impl Health {
    fn new(start: Instant) -> Self {
        Self {
            reported: Trouble::default(),
            window: start,
            reported_at: None,
            audible: 0,
            audible_at: None,
        }
    }

    /// Takes a flow's counters `now`: whether the flow is degraded, and what
    /// to report if a report is due, with the time it covers.
    fn update(&mut self, counters: Trouble, now: Instant) -> (bool, Option<(Trouble, Duration)>) {
        if counters.audible() > self.audible {
            self.audible = counters.audible();
            self.audible_at = Some(now);
        }
        let degraded = self.audible_at.is_some_and(|t| now.duration_since(t) < DEGRADED_HOLD);
        let new = counters.since(self.reported);
        let due = self.reported_at.is_none_or(|t| now.duration_since(t) >= REPORT_INTERVAL);
        if new.is_empty() || !due {
            return (degraded, None);
        }
        let covers = now.duration_since(self.window);
        self.reported = counters;
        self.window = now;
        self.reported_at = Some(now);
        (degraded, Some((new, covers)))
    }
}

/// A channel that failed: why, and when to try again.
#[derive(Clone, Debug)]
struct Failure {
    /// The subscription that failed; forgotten when the channel's changes.
    key: Key,
    status: SubscriptionStatus,
    retry_at: Instant,
    /// Failures in a row.
    count: u32,
}

/// A transmit channel put in a flow slot, and the local channels that want
/// it.
struct Placed {
    slot: usize,
    resolved: ResolvedChannel,
    key: Key,
    chans: Vec<usize>,
}

/// Transmit channels to put in free slots of an existing flow (DBCP 0x0102).
struct Grow {
    flow: u16,
    handle: FlowHandle,
    ids: Vec<u16>,
    placed: Vec<Placed>,
}

/// A new flow to request (DBCP 0x0100).
struct Create {
    id: u16,
    socket: std::net::UdpSocket,
    request: FlowRequest,
    placed: Vec<Placed>,
    format: SampleFormat,
    /// The latency it will play at, in samples.
    latency: u64,
}

/// What a background job was about, to clean up after it.
enum Job {
    Lookup(Key),
    Request { control: SocketAddrV4, channels: Vec<(usize, Key)>, ids: Vec<u16> },
}

/// What a background job found.
enum Done {
    Lookup {
        key: Key,
        result: Result<ResolvedChannel>,
    },
    Request {
        control: SocketAddrV4,
        grows: Vec<(Grow, Result<()>)>,
        creates: Vec<(Create, Result<FlowHandle>)>,
    },
}

pub struct RxManager {
    shared: Arc<Shared>,
    directory: Directory,
    flows: Vec<RxFlow>,
    /// Failed channels.
    failures: HashMap<usize, Failure>,
    /// Cloned into every receive thread, which keeps it until it exits; see
    /// [`RxManager::new`].
    threads: mpsc::Sender<()>,
    /// Lookups and requests in flight, and what each is about.
    jobs: JoinSet<Done>,
    job_info: HashMap<tokio::task::Id, Job>,
    /// Subscriptions being looked up.
    resolving: HashSet<Key>,
    /// Lookups done, waiting for their transmitter to be free.
    resolved: HashMap<Key, (ResolvedChannel, Instant)>,
    /// Transmitters with a request job in flight.
    busy: HashSet<SocketAddrV4>,
    /// Local channels in a request in flight, and what they were requested
    /// as.
    requesting: HashMap<usize, Key>,
    /// Flow ids of requests in flight.
    reserved: HashSet<u16>,
    /// Flows to stop, per transmitter, sent before its next requests.
    stops: BTreeMap<SocketAddrV4, Vec<FlowHandle>>,
    /// Routing tables replaced recently; see [`RxFlow::set_routes`].
    retired: Vec<(Instant, Arc<Routes>)>,
    /// Channels routed at the end of the last pass.
    routed: HashSet<usize>,
    /// Channels that lost their last route, and since when.
    unrouted: HashMap<usize, Instant>,
}

impl RxManager {
    /// `threads` is never sent on: its receiver sees the channel close once
    /// the manager and every receive thread it started are gone, and with
    /// them every reference they held to the receive rings.
    pub fn new(shared: Arc<Shared>, directory: Directory, threads: mpsc::Sender<()>) -> Self {
        Self {
            shared,
            directory,
            flows: Vec::new(),
            failures: HashMap::new(),
            threads,
            jobs: JoinSet::new(),
            job_info: HashMap::new(),
            resolving: HashSet::new(),
            resolved: HashMap::new(),
            busy: HashSet::new(),
            requesting: HashMap::new(),
            reserved: HashSet::new(),
            stops: BTreeMap::new(),
            retired: Vec::new(),
            routed: HashSet::new(),
            unrouted: HashMap::new(),
        }
    }

    pub async fn run(mut self, mut shutdown: oneshot::Receiver<oneshot::Sender<()>>) {
        let mut watch = self.shared.watch();
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = watch.changed() => {
                    // Controllers often send related changes back to back;
                    // batch them into one pass so they can share flows.
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    watch.borrow_and_update();
                }
                _ = tick.tick() => {}
                Some(joined) = self.jobs.join_next_with_id() => self.finished(joined),
                done = &mut shutdown => {
                    self.stop_all().await;
                    if let Ok(done) = done {
                        let _ = done.send(());
                    }
                    return;
                }
            }
            self.reconcile().await;
        }
    }

    async fn stop_all(&mut self) {
        // Requests in flight are dropped; a flow one of them obtained
        // expires on its transmitter for lack of keepalives.
        self.jobs.shutdown().await;
        let ip = self.shared.info.iface.ip;
        let mut stopping = JoinSet::new();
        let mut written = Vec::new();
        let Self { flows, retired, stops, unrouted, .. } = self;
        for mut flow in flows.drain(..) {
            written.extend(flow.routes.slots.iter().flatten().copied());
            flow.set_routes(Routes::new(flow.tx_ids.len()), retired);
            flow.worker.stop();
            stopping.spawn(client::stop_flow(ip, flow.control.into(), flow.handle));
        }
        for (control, handles) in std::mem::take(stops) {
            for handle in handles {
                stopping.spawn(client::stop_flow(ip, control.into(), handle));
            }
        }
        while stopping.join_next().await.is_some() {}
        written.extend(unrouted.drain().map(|(ch, _)| ch));
        if !written.is_empty() {
            // See `housekeeping`: forget what the rings last held, once the
            // receive threads are done writing.
            tokio::time::sleep(CLEAR_DELAY).await;
            for ch in written {
                if let Some(ring) = self.shared.rx_rings.get(ch) {
                    ring.clear();
                }
            }
        }
        self.shared.state().rx_flows.clear();
    }

    /// The subscriptions controllers asked for, per receive channel.
    fn desired(&self) -> Vec<Option<(String, String)>> {
        self.shared
            .state()
            .subscriptions
            .iter()
            .map(|s| s.as_ref().map(|s| (s.tx_channel.clone(), s.tx_device.clone())))
            .collect()
    }

    pub async fn reconcile(&mut self) {
        let desired = self.desired();
        let keys: Vec<Option<Key>> =
            desired.iter().map(|d| d.as_ref().map(|(c, v)| key(c, v))).collect();
        let now = Instant::now();

        // 1. Remove stale routes.
        self.unroute_stale(&desired, &keys).await;
        // 2. Stop flows that died or have carried nothing for a while.
        self.stop_flows(now);
        // 3. Route channels to existing flows where possible.
        let wanted = self.route_existing(&desired, &keys).await;
        // 4. Resolve and request the others.
        self.request(&desired, wanted, now);
        // 5. Publish statuses and flow info.
        self.housekeeping(now);
        self.publish(&keys);
    }

    /// Step 1: drops routes of channels that are no longer subscribed, or
    /// subscribed to another transmit channel. A subscription spelled
    /// differently (a label instead of the factory name, a renamed device)
    /// that names the same transmit channel keeps its route: re-routing it
    /// would interrupt the audio.
    async fn unroute_stale(&mut self, desired: &[Option<(String, String)>], keys: &[Option<Key>]) {
        let mut respelled = Vec::new();
        for (f, flow) in self.flows.iter().enumerate() {
            for (slot, targets) in flow.routes.slots.iter().enumerate() {
                for &ch in targets {
                    if keys[ch].as_ref().is_some_and(|k| flow.spelled.get(&ch) != Some(k)) {
                        respelled.push((f, slot, ch));
                    }
                }
            }
        }
        let mut same = HashSet::new();
        for (f, slot, ch) in respelled {
            let Some((channel, device)) = &desired[ch] else { continue };
            if let Some(r) = self.cached(channel, device).await {
                if self.flows[f].carries(&r, slot) {
                    same.insert((f, slot, ch));
                }
            }
        }

        let Self { flows, retired, .. } = self;
        for (f, flow) in flows.iter_mut().enumerate() {
            let mut routes = (*flow.routes).clone();
            for (slot, targets) in routes.slots.iter_mut().enumerate() {
                targets.retain(|&ch| {
                    keys[ch].as_ref().is_some_and(|k| {
                        flow.spelled.get(&ch) == Some(k) || same.contains(&(f, slot, ch))
                    })
                });
                for &ch in targets.iter() {
                    let (Some(k), Some((channel, device))) = (&keys[ch], &desired[ch]) else {
                        continue;
                    };
                    if flow.spelled.get(&ch) != Some(k) {
                        info!(
                            "rx {} now subscribed as {channel}@{device}, the transmit channel it \
                             already receives: keeping its route",
                            ch + 1
                        );
                        flow.slot_names[slot] = channel.clone();
                        flow.tx_device = device.clone();
                        flow.spelled.insert(ch, k.clone());
                    }
                }
            }
            flow.spelled.retain(|&ch, _| routes.feeds(ch));
            flow.set_routes(routes, retired);
        }
    }

    /// Step 2.
    fn stop_flows(&mut self, now: Instant) {
        let now_ns = local_now_ns();
        let Self { flows, retired, stops, .. } = self;
        flows.retain_mut(|flow| {
            let empty = flow.routes.is_empty() && flow.changing.is_empty();
            let since = if empty { *flow.empty_since.get_or_insert(now) } else { now };
            if !empty {
                flow.empty_since = None;
            }
            let dead = flow.is_dead(now_ns);
            if !dead && now.duration_since(since) < EMPTY_LINGER {
                return true;
            }
            if dead {
                warn!(
                    "rx flow {} from {} delivered nothing for {} s: requesting it again",
                    flow.id,
                    flow.tx_device,
                    FLOW_TIMEOUT_NS / 1_000_000_000
                );
            } else {
                info!("rx flow {} from {} no longer needed", flow.id, flow.tx_device);
            }
            // Nothing more for its thread to write, should a packet still
            // arrive before it exits.
            flow.set_routes(Routes::new(flow.tx_ids.len()), retired);
            flow.worker.stop();
            stops.entry(flow.control).or_default().push(flow.handle);
            false
        });
    }

    /// Step 3: routes subscribed channels that a flow already carries.
    /// Returns the others that may be requested, by subscription.
    async fn route_existing(
        &mut self,
        desired: &[Option<(String, String)>],
        keys: &[Option<Key>],
    ) -> BTreeMap<Key, Vec<usize>> {
        let mut routed: HashSet<usize> =
            self.flows.iter().flat_map(|f| f.routes.slots.iter().flatten().copied()).collect();
        // A failure belongs to the subscription that failed.
        self.failures.retain(|&ch, f| keys[ch].as_ref() == Some(&f.key));
        let mut wanted: BTreeMap<Key, Vec<usize>> = BTreeMap::new();
        for (ch, k) in keys.iter().enumerate() {
            let (Some(k), Some((channel, device))) = (k, &desired[ch]) else { continue };
            if routed.contains(&ch) {
                self.failures.remove(&ch);
                continue;
            }
            if let Some((f, slot)) = self.find_slot(k, channel, device).await {
                self.flows[f].route(slot, ch, k, &mut self.retired);
                routed.insert(ch);
                self.failures.remove(&ch);
                continue;
            }
            wanted.entry(k.clone()).or_default().push(ch);
        }
        wanted
    }

    /// A flow slot that carries `channel@device` (subscribed as `k`): one
    /// feeding a channel subscribed the same way, one requested under that
    /// name, or one that carries what the directory says it is.
    async fn find_slot(&self, k: &Key, channel: &str, device: &str) -> Option<(usize, usize)> {
        for (f, flow) in self.flows.iter().enumerate() {
            for slot in 0..flow.tx_ids.len() {
                if flow.tx_ids[slot] == 0 || flow.changing.contains(&slot) {
                    continue;
                }
                let same_spelling =
                    flow.routes.slots[slot].iter().any(|ch| flow.spelled.get(ch) == Some(k));
                let same_name = flow.slot_names[slot].eq_ignore_ascii_case(channel)
                    && flow.tx_device.eq_ignore_ascii_case(device);
                if same_spelling || same_name {
                    return Some((f, slot));
                }
            }
        }
        let r = self.cached(channel, device).await?;
        self.carrier(&r)
    }

    /// The flow slot carrying `r`, if any.
    fn carrier(&self, r: &ResolvedChannel) -> Option<(usize, usize)> {
        self.flows.iter().enumerate().find_map(|(f, flow)| {
            (0..flow.tx_ids.len()).find(|&slot| flow.carries(r, slot)).map(|slot| (f, slot))
        })
    }

    /// What `channel@device` is, as far as known without waiting.
    async fn cached(&self, channel: &str, device: &str) -> Option<ResolvedChannel> {
        if device.eq_ignore_ascii_case(&self.shared.name()) {
            self.resolve_self(channel).ok()
        } else {
            self.directory.cached(channel, device).await
        }
    }

    /// Step 4: starts lookups for subscriptions not looked up yet, and
    /// requests for those looked up, per transmitter.
    fn request(
        &mut self,
        desired: &[Option<(String, String)>],
        wanted: BTreeMap<Key, Vec<usize>>,
        now: Instant,
    ) {
        let own = self.shared.name();
        self.resolved
            .retain(|k, (_, at)| wanted.contains_key(k) && now.duration_since(*at) < RESOLVED_TTL);
        let mut per_tx: BTreeMap<SocketAddrV4, Vec<Placed>> = BTreeMap::new();
        for (k, chans) in wanted {
            // Not those already being requested as this, nor those waiting
            // to be retried.
            let chans: Vec<usize> = chans
                .into_iter()
                .filter(|ch| {
                    self.requesting.get(ch) != Some(&k)
                        && !self.failures.get(ch).is_some_and(|f| f.retry_at > now)
                })
                .collect();
            if chans.is_empty() || self.resolving.contains(&k) {
                continue;
            }
            let resolved = match self.resolved.get(&k) {
                Some((r, _)) => r.clone(),
                None => {
                    // Use the spelling from the subscription for lookups.
                    let Some((channel, device)) = desired[chans[0]].clone() else { continue };
                    if !device.eq_ignore_ascii_case(&own) {
                        self.lookup(k, channel, device);
                        continue;
                    }
                    if self.foreign_possible() {
                        self.lookup_self(k, channel, device);
                        continue;
                    }
                    match self.resolve_self(&channel) {
                        Ok(r) => r,
                        Err(e) => {
                            debug!("cannot subscribe to {channel}@{device}: {e}");
                            self.fail(&chans, &k, SubscriptionStatus::Unresolved, Retry::LOOKUP);
                            continue;
                        }
                    }
                }
            };
            let control = SocketAddrV4::new(resolved.addr, resolved.flow_control_port);
            per_tx.entry(control).or_default().push(Placed { slot: 0, resolved, key: k, chans });
        }

        for (control, entries) in per_tx {
            // One request at a time per transmitter: it changes the flows
            // they share. The lookups wait in `resolved`.
            if self.busy.contains(&control) {
                continue;
            }
            for p in &entries {
                self.resolved.remove(&p.key);
            }
            self.plan(control, entries);
        }
        // Flows to stop on transmitters we ask nothing else of.
        let idle: Vec<SocketAddrV4> =
            self.stops.keys().filter(|c| !self.busy.contains(c)).copied().collect();
        for control in idle {
            self.dispatch(control, Vec::new(), Vec::new());
        }
    }

    /// Whether another device may use our name: a static directory may list
    /// one, and mDNS knows of one only while it reports a name conflict.
    fn foreign_possible(&self) -> bool {
        match &self.directory {
            Directory::Mdns(m) => m.has_conflicts(),
            Directory::Static(_) => true,
        }
    }

    /// Starts looking up `channel@device`, where `device` is our own name:
    /// our own channel, unless another device uses our name too and has it.
    /// That is a name conflict, logged and reported while it lasts. A
    /// subscription then can't tell the two devices apart, and looping it
    /// back here would hide that the other device was the one meant.
    fn lookup_self(&mut self, k: Key, channel: String, device: String) {
        let ours = self.resolve_self(&channel);
        let directory = self.directory.clone();
        let us = (self.shared.info.iface.ip, self.shared.info.ports.flow_control);
        let key = k.clone();
        let job = self.jobs.spawn(async move {
            let result = match foreign_channel(&directory, &channel, &device, us).await {
                Some(theirs) => {
                    warn!(
                        "subscribing to {channel}@{device} at {}, another device named like \
                         this one",
                        theirs.addr
                    );
                    Ok(theirs)
                }
                None => ours,
            };
            Done::Lookup { key, result }
        });
        self.resolving.insert(k.clone());
        self.job_info.insert(job.id(), Job::Lookup(k));
    }

    /// Starts looking up `channel@device`, subscribed as `k`.
    fn lookup(&mut self, k: Key, channel: String, device: String) {
        let directory = self.directory.clone();
        let key = k.clone();
        let job = self.jobs.spawn(async move {
            let result = directory.resolve(&channel, &device).await;
            Done::Lookup { key, result }
        });
        self.resolving.insert(k.clone());
        self.job_info.insert(job.id(), Job::Lookup(k));
    }

    /// Decides how to get transmit channels from the transmitter at
    /// `control`: from slots that already carry them, in free slots of its
    /// flows, or in new flows; and starts the requests.
    fn plan(&mut self, control: SocketAddrV4, entries: Vec<Placed>) {
        let rate = self.shared.info.sample_rate;
        let mut left = Vec::new();
        for p in entries {
            let txt = &p.resolved.txt;
            if txt.sample_rate != rate
                || SampleFormat::from_bits(txt.bits_per_sample as u32).is_none()
            {
                let changed = self.fail(&p.chans, &p.key, status::CHANNEL_FORMAT, Retry::REFUSED);
                if changed {
                    warn!(
                        "{}@{} is {} Hz/{} bit; we run at {} Hz",
                        p.resolved.channel,
                        p.resolved.device,
                        txt.sample_rate,
                        txt.bits_per_sample,
                        rate
                    );
                }
                continue;
            }
            if let Some((f, slot)) = self.carrier(&p.resolved) {
                for &ch in &p.chans {
                    self.flows[f].route(slot, ch, &p.key, &mut self.retired);
                    self.failures.remove(&ch);
                }
                continue;
            }
            left.push(p);
        }
        let grows = self.plan_grows(control, &mut left);
        let mut creates = Vec::new();
        while !left.is_empty() {
            let per_flow = self.slots_per_flow(&left[0].resolved.txt);
            let chunk: Vec<Placed> = left.drain(..per_flow.min(left.len())).collect();
            if let Some(c) = self.plan_create(chunk, per_flow) {
                creates.push(c);
            }
        }
        if !grows.is_empty() || !creates.is_empty() {
            self.dispatch(control, grows, creates);
        }
    }

    /// Puts entries of `left` in free slots of flows from the transmitter
    /// at `control`, so that transmitters (which may only have a few flows)
    /// aren't asked for a new flow per channel. Entries placed are removed.
    fn plan_grows(&mut self, control: SocketAddrV4, left: &mut Vec<Placed>) -> Vec<Grow> {
        let mut grows = Vec::new();
        for flow in self.flows.iter_mut().filter(|f| f.control == control && !f.grow_refused) {
            if left.is_empty() {
                break;
            }
            // Slots that carry nothing come first. One that still carries
            // an unsubscribed channel keeps it, so that subscribing that
            // channel again needs no request.
            let mut free: Vec<usize> = (0..flow.tx_ids.len())
                .filter(|&s| flow.routes.slots[s].is_empty() && !flow.changing.contains(&s))
                .collect();
            free.sort_by_key(|&s| flow.tx_ids[s] != 0);
            let mut ids = flow.tx_ids.clone();
            let mut placed = Vec::new();
            let mut i = 0;
            while i < left.len() && placed.len() < free.len() {
                if left[i].resolved.txt.bits_per_sample != flow.bits {
                    i += 1;
                    continue;
                }
                let mut p = left.remove(i);
                p.slot = free[placed.len()];
                ids[p.slot] = p.resolved.txt.id;
                placed.push(p);
            }
            if placed.is_empty() {
                continue;
            }
            flow.changing.extend(placed.iter().map(|p| p.slot));
            grows.push(Grow { flow: flow.id, handle: flow.handle, ids, placed });
        }
        grows
    }

    /// Prepares a new flow for `chunk`: its id, socket, frames per packet
    /// and latency. On failure the channels are marked failed.
    fn plan_create(&mut self, mut chunk: Vec<Placed>, max_slots: usize) -> Option<Create> {
        let shared = self.shared.clone();
        let info = &shared.info;
        let txt = chunk[0].resolved.txt.clone();
        let Some(id) = (1..=MAX_RX_FLOWS as u16)
            .find(|id| self.flows.iter().all(|f| f.id != *id) && !self.reserved.contains(id))
        else {
            // Logged once, not on every retry.
            if self.fail_chunk(&chunk, status::NO_RX, Retry::REFUSED) {
                warn!("all {MAX_RX_FLOWS} receive flows in use");
            }
            return None;
        };
        let Some(format) = SampleFormat::from_bits(txt.bits_per_sample as u32) else {
            self.fail_chunk(&chunk, status::CHANNEL_FORMAT, Retry::REFUSED);
            return None;
        };
        // Sized for every slot it may hold, so packets always fit.
        let fpp = choose_fpp(info.latency_samples, &txt, max_slots, format);
        let latency = flow_latency(info.latency_samples, &txt, fpp, info.sample_rate);
        let most = max_latency_samples(info.sample_rate);
        if latency > most {
            if self.fail_chunk(&chunk, status::CHANNEL_LATENCY, Retry::REFUSED) {
                warn!(
                    "{} asks for {:.2} ms of latency, more than the {:.0} ms we can play at",
                    chunk[0].resolved.device,
                    latency as f64 * 1000.0 / info.sample_rate as f64,
                    crate::config::MAX_LATENCY_MS
                );
            }
            return None;
        }
        // Every slot the flow may hold, unused ones 0, as Dante receivers
        // request them (1, 0, 0, 0): a transmitter fixes a flow's slot count
        // when it creates the flow, and 0x0102 then fills those slots.
        let slots = max_slots.max(chunk.len());
        let mut channels = vec![0u16; slots];
        for (slot, p) in chunk.iter_mut().enumerate() {
            p.slot = slot;
            channels[slot] = p.resolved.txt.id;
        }
        let socket = match bind_rx_port(info.iface.ip) {
            Ok(s) => s,
            Err(e) => {
                let changed = self.fail_chunk(&chunk, status::RX_FAIL, Retry::FAST);
                if changed {
                    warn!(
                        "cannot receive on {} ({}): {e}; is the interface up, and does it still \
                         have this address?",
                        info.iface.name, info.iface.ip
                    );
                }
                return None;
            }
        };
        let port = socket.local_addr().map(|a| a.port()).unwrap_or(0);
        let request = FlowRequest {
            rx_device: shared.name(),
            flow_name: format!("{id}_{}", info.process_id),
            sample_rate: info.sample_rate,
            bits_per_sample: txt.bits_per_sample as u32,
            fpp,
            channels,
            rx_addr: info.iface.ip,
            rx_port: port,
        };
        Some(Create { id, socket, request, placed: chunk, format, latency })
    }

    fn fail_chunk(&mut self, chunk: &[Placed], status: SubscriptionStatus, retry: Retry) -> bool {
        let mut changed = false;
        for p in chunk {
            changed |= self.fail(&p.chans, &p.key, status, retry);
        }
        changed
    }

    /// Starts a request job for the transmitter at `control`, preceded by
    /// the flows to stop there.
    fn dispatch(&mut self, control: SocketAddrV4, grows: Vec<Grow>, creates: Vec<Create>) {
        let ip = self.shared.info.iface.ip;
        let stops = self.stops.remove(&control).unwrap_or_default();
        let mut channels = Vec::new();
        let placed =
            grows.iter().flat_map(|g| &g.placed).chain(creates.iter().flat_map(|c| &c.placed));
        for p in placed {
            channels.extend(p.chans.iter().map(|&ch| (ch, p.key.clone())));
        }
        let ids: Vec<u16> = creates.iter().map(|c| c.id).collect();
        for (ch, k) in &channels {
            self.requesting.insert(*ch, k.clone());
        }
        self.reserved.extend(&ids);
        self.busy.insert(control);
        let job = self.jobs.spawn(request_job(ip, control, stops, grows, creates));
        self.job_info.insert(job.id(), Job::Request { control, channels, ids });
    }

    /// Applies what a job found.
    fn finished(&mut self, joined: std::result::Result<(tokio::task::Id, Done), JoinError>) {
        let (id, done) = match joined {
            Ok((id, done)) => (id, Some(done)),
            Err(e) => {
                warn!("subscription job failed: {e}");
                (e.id(), None)
            }
        };
        let job = self.job_info.remove(&id);
        match &job {
            Some(Job::Lookup(k)) => {
                self.resolving.remove(k);
            }
            Some(Job::Request { control, channels, ids }) => {
                self.busy.remove(control);
                // Only this job could change the transmitter's flows.
                for flow in self.flows.iter_mut().filter(|f| f.control == *control) {
                    flow.changing.clear();
                }
                for (ch, k) in channels {
                    if self.requesting.get(ch) == Some(k) {
                        self.requesting.remove(ch);
                    }
                }
                for id in ids {
                    self.reserved.remove(id);
                }
            }
            None => {}
        }
        match (done, job) {
            (Some(Done::Lookup { key, result }), _) => self.looked_up(key, result),
            (Some(Done::Request { control, grows, creates }), _) => {
                self.grown(grows);
                self.created(control, creates);
            }
            (None, Some(Job::Request { channels, .. })) => {
                for (ch, k) in channels {
                    self.fail(&[ch], &k, SubscriptionStatus::TxFail, Retry::REFUSED);
                }
            }
            (None, _) => {}
        }
    }

    fn looked_up(&mut self, k: Key, result: Result<ResolvedChannel>) {
        match result {
            Ok(r) => {
                self.resolved.insert(k, (r, Instant::now()));
            }
            Err(e) => {
                let chans = self.subscribed_as(&k);
                if self.fail(&chans, &k, SubscriptionStatus::Unresolved, Retry::LOOKUP) {
                    info!("cannot find {}@{}: {e}", k.0, k.1);
                } else {
                    debug!("cannot find {}@{}: {e}", k.0, k.1);
                }
            }
        }
    }

    /// Local channels currently subscribed as `k`.
    fn subscribed_as(&self, k: &Key) -> Vec<usize> {
        let state = self.shared.state();
        let subscribed = |s: &crate::state::Subscription| key(&s.tx_channel, &s.tx_device) == *k;
        state
            .subscriptions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.as_ref().is_some_and(subscribed))
            .map(|(ch, _)| ch)
            .collect()
    }

    /// Of `chans`, requested as `k`, those still subscribed as `k` and not
    /// routed meanwhile.
    fn still_wanted(&self, chans: &[usize], k: &Key) -> Vec<usize> {
        let subscribed = self.subscribed_as(k);
        chans
            .iter()
            .copied()
            .filter(|ch| subscribed.contains(ch) && !self.flows.iter().any(|f| f.routes.feeds(*ch)))
            .collect()
    }

    fn grown(&mut self, grows: Vec<(Grow, Result<()>)>) {
        let media_now = self.shared.clock.now_samples(self.shared.info.sample_rate).unwrap_or(0);
        for (g, result) in grows {
            let Some(f) = self.flows.iter().position(|f| f.id == g.flow && f.handle == g.handle)
            else {
                // Stopped meanwhile: its channels are requested again.
                continue;
            };
            match result {
                Ok(()) => {
                    let wanted: Vec<_> =
                        g.placed.iter().map(|p| self.still_wanted(&p.chans, &p.key)).collect();
                    let flow = &mut self.flows[f];
                    flow.tx_ids = g.ids;
                    let mut routes = (*flow.routes).clone();
                    for (p, chans) in g.placed.iter().zip(wanted) {
                        routes.slots[p.slot] = chans.clone();
                        // The transmitter has switched by the time it
                        // answers: from now on the slot carries the new
                        // channel.
                        routes.fresh_from[p.slot] = media_now;
                        flow.slot_names[p.slot] = p.resolved.channel.clone();
                        for ch in chans {
                            flow.spelled.insert(ch, p.key.clone());
                            self.failures.remove(&ch);
                        }
                    }
                    info!(
                        "rx flow {} from {} now carries {:?}",
                        flow.id, flow.tx_device, flow.tx_ids
                    );
                    flow.set_routes(routes, &mut self.retired);
                }
                Err(Error::Refused(code)) => {
                    let flow = &mut self.flows[f];
                    warn!(
                        "{} refused to change rx flow {} ({code:#06x}); asking for a new flow",
                        flow.tx_device, flow.id
                    );
                    flow.grow_refused = true;
                    let now = Instant::now();
                    for p in g.placed {
                        self.resolved.insert(p.key, (p.resolved, now));
                    }
                }
                Err(e) => {
                    let (status, retry) = classify(&e);
                    let mut changed = false;
                    for p in &g.placed {
                        changed |= self.fail(&p.chans, &p.key, status, retry);
                    }
                    if changed {
                        let flow = &self.flows[f];
                        warn!("changing rx flow {} from {} failed: {e}", flow.id, flow.tx_device);
                    }
                }
            }
        }
    }

    fn created(&mut self, control: SocketAddrV4, creates: Vec<(Create, Result<FlowHandle>)>) {
        for (c, result) in creates {
            match result {
                Ok(handle) => {
                    if let Err((placed, e)) = self.start_flow(control, c, handle) {
                        warn!("cannot start a receive thread: {e}");
                        self.stops.entry(control).or_default().push(handle);
                        self.fail_chunk(&placed, status::RX_FAIL, Retry::FAST);
                    }
                }
                Err(e) => {
                    let (status, retry) = classify(&e);
                    if self.fail_chunk(&c.placed, status, retry) {
                        if status == status::RX_FAIL {
                            let iface = &self.shared.info.iface;
                            warn!(
                                "flow request to {control} failed on our side, from {} ({}): \
                                 {e}; is the interface up, and does it still have this address?",
                                iface.name, iface.ip
                            );
                        } else {
                            warn!("flow request to {control} failed: {e}");
                        }
                    }
                }
            }
        }
    }

    /// Starts receiving a flow the transmitter granted.
    fn start_flow(
        &mut self,
        control: SocketAddrV4,
        c: Create,
        handle: FlowHandle,
    ) -> std::result::Result<(), (Vec<Placed>, std::io::Error)> {
        let shared = self.shared.clone();
        let info = &shared.info;
        let Create { id, socket, request, placed, format, latency } = c;
        let slots = request.channels.len();
        let mut routes = Routes::new(slots);
        let mut spelled = HashMap::new();
        let mut slot_names = vec![String::new(); slots];
        for p in &placed {
            let chans = self.still_wanted(&p.chans, &p.key);
            for &ch in &chans {
                spelled.insert(ch, p.key.clone());
                self.failures.remove(&ch);
            }
            routes.slots[p.slot] = chans;
            slot_names[p.slot] = p.resolved.channel.clone();
        }
        let params = FlowParams {
            tx_ip: *control.ip(),
            slots,
            fpp: request.fpp as usize,
            format,
            sample_rate: info.sample_rate,
            latency,
            offset: latency.saturating_sub(info.latency_samples),
        };
        let routes = Arc::new(routes);
        let routing: Routing = Arc::new(ArcSwap::new(routes.clone()));
        let last_packet_ns = Arc::new(AtomicU64::new(0));
        let stats = Arc::new(FlowStats::default());
        let receiver = Receiver::new(
            params,
            routing.clone(),
            shared.rx_rings.clone(),
            Probes {
                last_packet_ns: last_packet_ns.clone(),
                counters: shared.counters.clone(),
                stats: stats.clone(),
            },
        );
        let worker =
            match Worker::spawn(id, socket, shared.clock.clone(), receiver, self.threads.clone()) {
                Ok(w) => w,
                Err(e) => return Err((placed, e)),
            };
        let latency_ns = (latency as u128 * 1_000_000_000 / info.sample_rate.max(1) as u128)
            .min(u32::MAX as u128) as u32;
        let flow = RxFlow {
            id,
            tx_device: placed[0].resolved.device.clone(),
            control,
            handle,
            slot_names,
            tx_ids: request.channels,
            bits: request.bits_per_sample as u16,
            routing,
            routes,
            spelled,
            changing: HashSet::new(),
            grow_refused: false,
            empty_since: None,
            port: request.rx_port,
            latency_ns,
            last_packet_ns,
            stats,
            health: Health::new(Instant::now()),
            created_ns: local_now_ns(),
            worker,
        };
        info!(
            "rx flow {} from {} ({} channels) established",
            flow.id,
            flow.tx_device,
            flow.slot_names.iter().filter(|n| !n.is_empty()).count()
        );
        if params.offset > 0 {
            warn!(
                "rx flow {} from {}: the transmitter needs {:.2} ms of latency, we run at {:.2} \
                 ms; its channels play {:.2} ms later than the others",
                flow.id,
                flow.tx_device,
                latency_ns as f64 / 1e6,
                info.latency_ns as f64 / 1e6,
                params.offset as f64 * 1000.0 / info.sample_rate as f64
            );
        }
        self.flows.push(flow);
        Ok(())
    }

    /// Records that `chans`, subscribed as `k`, failed: they show `status`
    /// and are tried again after `retry`'s delay, counted from now. Returns
    /// whether that changed what any of them shows, for logging.
    fn fail(&mut self, chans: &[usize], k: &Key, status: SubscriptionStatus, retry: Retry) -> bool {
        let now = Instant::now();
        let mut changed = false;
        for &ch in chans {
            let count = self.failures.get(&ch).filter(|f| f.key == *k).map_or(0, |f| f.count) + 1;
            let failure =
                Failure { key: k.clone(), status, retry_at: now + retry.delay(count), count };
            changed |= self.failures.insert(ch, failure).is_none_or(|old| old.status != status);
        }
        changed
    }

    /// Clears the rings of channels that lost their route, forgets old
    /// routing tables, and looks at the flows' problem counters.
    fn housekeeping(&mut self, now: Instant) {
        let routed: HashSet<usize> =
            self.flows.iter().flat_map(|f| f.routes.slots.iter().flatten().copied()).collect();
        for &ch in self.routed.difference(&routed) {
            self.unrouted.entry(ch).or_insert(now);
        }
        let rings = &self.shared.rx_rings;
        self.unrouted.retain(|&ch, since| {
            if routed.contains(&ch) {
                return false;
            }
            if now.duration_since(*since) < CLEAR_DELAY {
                return true;
            }
            // Nothing writes this ring any more. Its slots are tagged with
            // the low 32 bits of their timestamps, so what it holds would
            // play again 2^32 samples later (25 h at 48 kHz): forget it.
            if let Some(ring) = rings.get(ch) {
                ring.clear();
            }
            false
        });
        self.routed = routed;
        self.retired.retain(|(at, _)| now.duration_since(*at) < RETIRE_AFTER);

        let rate = self.shared.info.sample_rate;
        for flow in &mut self.flows {
            let (degraded, report) = flow.health.update(Trouble::read(&flow.stats), now);
            flow.stats.degraded.store(degraded, Ordering::Relaxed);
            if let Some((trouble, since)) = report {
                warn!("{}", flow.report(trouble, since, rate));
            }
        }
    }

    fn slots_per_flow(&self, txt: &ChannelTxt) -> usize {
        txt.nchan.max(1).min(self.shared.info.max_channels_per_rx_flow) as usize
    }

    /// Describes one of our own transmit channels, for self-subscriptions
    /// (see [`foreign_channel`]).
    fn resolve_self(&self, channel: &str) -> Result<ResolvedChannel> {
        let info = &self.shared.info;
        let state = self.shared.state();
        let index = state
            .tx_channel_index(info, channel)
            .ok_or_else(|| Error::NotFound(format!("own channel {channel}")))?;
        Ok(ResolvedChannel {
            device: state.name.clone(),
            channel: channel.to_owned(),
            addr: info.iface.ip,
            flow_control_port: info.ports.flow_control,
            txt: own_channel_txt(info, index as u16 + 1),
        })
    }

    fn publish(&self, keys: &[Option<Key>]) {
        let now = local_now_ns();
        let mut status = vec![SubscriptionStatus::None; keys.len()];
        for (ch, k) in keys.iter().enumerate() {
            let Some(k) = k else { continue };
            status[ch] = match self.failures.get(&ch) {
                // A failure shows until the channel receives, through the
                // retries.
                Some(f) => f.status,
                None if self.requesting.get(&ch) == Some(k) => SubscriptionStatus::InProgress,
                None => SubscriptionStatus::Unresolved,
            };
        }
        let mut flows_info = Vec::new();
        for flow in &self.flows {
            let receiving = flow.is_receiving(now);
            for targets in &flow.routes.slots {
                for &ch in targets {
                    status[ch] = if receiving {
                        SubscriptionStatus::ReceivingUnicast
                    } else {
                        SubscriptionStatus::InProgress
                    };
                }
            }
            flows_info.push(RxFlowInfo {
                id: flow.id,
                addr: self.shared.info.iface.ip,
                port: flow.port,
                latency_ns: flow.latency_ns,
                status: if receiving {
                    SubscriptionStatus::ReceivingUnicast
                } else {
                    SubscriptionStatus::InProgress
                },
                slots: flow
                    .routes
                    .slots
                    .iter()
                    .map(|t| t.iter().map(|&c| c as u16).collect())
                    .collect(),
            });
        }

        let mut changed = Vec::new();
        {
            let mut state = self.shared.state();
            for (ch, sub) in state.subscriptions.iter_mut().enumerate() {
                if let Some(sub) = sub {
                    if sub.status != status[ch] {
                        debug!("rx {} status {:?} -> {:?}", ch + 1, sub.status, status[ch]);
                        sub.status = status[ch];
                        changed.push(ch);
                    }
                }
            }
            state.rx_flows = flows_info;
            state.rx_flow_stats = self.flows.iter().map(|f| (f.id, f.stats.clone())).collect();
        }
        if !changed.is_empty() {
            self.shared.notify(Notify::RxChannelsChanged(changed));
        }
    }
}

/// A request job: stops flows the transmitter at `control` should no
/// longer send, then changes and requests flows there, all at once.
async fn request_job(
    ip: Ipv4Addr,
    tx: SocketAddrV4,
    stops: Vec<FlowHandle>,
    grows: Vec<Grow>,
    creates: Vec<Create>,
) -> Done {
    let control = SocketAddr::V4(tx);
    // Stopped first: a transmitter at its flow limit would refuse a new
    // flow while the one it replaces still counts. All at once: a
    // transmitter back from a reboot may not answer stops for handles it no
    // longer knows, and each unanswered stop waits 1 s.
    let mut stopping = JoinSet::new();
    for handle in stops {
        stopping.spawn(client::stop_flow(ip, control, handle));
    }
    while stopping.join_next().await.is_some() {}
    enum One {
        Grow(Grow, Result<()>),
        Create(Create, Result<FlowHandle>),
    }
    let mut set = JoinSet::new();
    for g in grows {
        set.spawn(async move {
            let r = client::update_flow(ip, control, g.handle, &g.ids).await;
            One::Grow(g, r)
        });
    }
    for c in creates {
        set.spawn(async move {
            let r = client::request_flow(ip, control, &c.request).await;
            One::Create(c, r)
        });
    }
    let (mut grown, mut created) = (Vec::new(), Vec::new());
    while let Some(one) = set.join_next().await {
        match one {
            Ok(One::Grow(g, r)) => grown.push((g, r)),
            Ok(One::Create(c, r)) => created.push((c, r)),
            Err(_) => {}
        }
    }
    Done::Request { control: tx, grows: grown, creates: created }
}

/// How long a subscription to our own name asks mDNS for another device
/// that has the name too, while one is known to (see [`foreign_channel`]).
const FOREIGN_LOOKUP: Duration = Duration::from_secs(1);

/// The transmit channel `channel@device`, where `device` is our own name, of
/// another device that has the name too, if there is one: an mDNS name
/// conflict, or an entry of a static directory that isn't ours (`us`: our
/// address and flow-control port).
async fn foreign_channel(
    directory: &Directory,
    channel: &str,
    device: &str,
    us: (Ipv4Addr, u16),
) -> Option<ResolvedChannel> {
    match directory {
        Directory::Mdns(m) => m.resolve_foreign_channel(channel, device, FOREIGN_LOOKUP).await,
        Directory::Static(s) => s.get(channel, device),
    }
    .filter(|r| (r.addr, r.flow_control_port) != us)
}

/// Binds a receive socket in the unicast port range real receivers report
/// (ARC 0x3300), falling back to an ephemeral port.
fn bind_rx_port(ip: Ipv4Addr) -> std::io::Result<std::net::UdpSocket> {
    let (first, last) = ovsc_proto::arc::RX_PORT_RANGES[0];
    let span = (last - first + 1) as u32;
    let offset = rand::random::<u32>() % span;
    for i in 0..span {
        let port = first + ((offset + i) % span) as u16;
        if let Ok(s) = bind_udp(ip, port) {
            return Ok(s);
        }
    }
    bind_udp(ip, 0)
}

/// The latency a flow plays at, in samples: ours, or more if the
/// transmitter demands more (TXT `latency_ns`, PROTOCOL.md §2.2; receivers
/// play at the larger of the two, §8.4), or if its packets are longer than
/// our latency, since a packet can't arrive before its last sample exists.
fn flow_latency(own: u64, txt: &ChannelTxt, fpp: u16, sample_rate: u32) -> u64 {
    let demanded = (txt.latency_ns as u64 * sample_rate as u64).div_ceil(1_000_000_000);
    own.max(demanded).max(fpp as u64)
}

/// The most latency a flow may play at: the most the device can be set to.
fn max_latency_samples(sample_rate: u32) -> u64 {
    (crate::config::MAX_LATENCY_MS * sample_rate as f64 / 1000.0) as u64
}

/// Frames per packet: about a quarter of our latency, within what the
/// transmitter supports and what fits in one datagram.
fn choose_fpp(
    latency_samples: u64,
    txt: &ChannelTxt,
    channels: usize,
    format: SampleFormat,
) -> u16 {
    let target = (latency_samples / 4).clamp(1, u16::MAX as u64) as u16;
    let mtu_limit = audio::max_fpp(channels, format).min(u16::MAX as usize) as u16;
    let max = txt.fpp_max.max(1).min(mtu_limit);
    let min = txt.fpp_min.max(1).min(max);
    target.clamp(min, max)
}

/// The TXT record we advertise for our transmit channel `id`.
pub fn own_channel_txt(info: &crate::info::DeviceInfo, id: u16) -> ChannelTxt {
    ChannelTxt {
        id,
        sample_rate: info.sample_rate,
        bits_per_sample: info.format.bits(),
        pcm_type: PCM_TYPE,
        latency_ns: info.latency_ns,
        fpp_max: FPP_MAX,
        fpp_min: FPP_MIN,
        nchan: MAX_CHANNELS_PER_TX_FLOW.min(info.tx_channels.len() as u16).max(1),
        dbcp1: ovsc_proto::frame::protocol::DBCP,
        is_default_name: true,
        multicast: None,
    }
}

/// The real-time thread receiving one flow.
///
/// Packets are written into the rings by timestamp, so receiving them late
/// only matters once they are older than the receive latency. That makes
/// scheduling delays the main cause of dropouts, hence a dedicated
/// high-priority thread per flow rather than an async task.
///
/// The thread blocks in `recv` with a timeout (the keepalive interval) on
/// every turn and must keep doing so: XNU demotes a real-time thread that
/// runs for about a second without blocking.
struct Worker {
    stop: Arc<AtomicBool>,
    params: FlowParams,
}

/// What a receive thread reports about its flow.
struct Probes {
    /// Local time of the last packet.
    last_packet_ns: Arc<AtomicU64>,
    /// The device's packet counters.
    counters: Arc<Counters>,
    /// The flow's.
    stats: Arc<FlowStats>,
}

impl Worker {
    fn spawn(
        id: u16,
        socket: std::net::UdpSocket,
        clock: MediaClock,
        receiver: Receiver,
        alive: mpsc::Sender<()>,
    ) -> std::io::Result<Self> {
        socket.set_read_timeout(Some(KEEPALIVE_INTERVAL))?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let params = receiver.params;
        std::thread::Builder::new().name(format!("ovsc-rx{id}")).spawn(move || {
            // Dropped last, once `receive` has dropped the rings.
            let _alive = alive;
            crate::rt::raise_priority("receive", crate::rt::RtClass::Receive);
            receive(socket, clock, receiver, flag)
        })?;
        Ok(Self { stop, params })
    }

    /// Asks the thread to exit; it does within one keepalive interval.
    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What a receive thread knows about its flow.
#[derive(Clone, Copy, Debug)]
struct FlowParams {
    /// The transmitter's address: datagrams from any other are dropped.
    tx_ip: Ipv4Addr,
    /// Slots per packet, fixed when the flow was requested.
    slots: usize,
    fpp: usize,
    format: SampleFormat,
    sample_rate: u32,
    /// The flow's latency in samples: packets older than this on arrival
    /// are late.
    latency: u64,
    /// How much later than their timestamps plus our latency the flow's
    /// samples play, in samples: what the flow needs beyond our latency
    /// (see [`flow_latency`]). Samples are written that much later.
    offset: u64,
}

impl FlowParams {
    /// The size of the flow's packets as requested (PROTOCOL.md §8.2).
    fn packet_len(&self) -> usize {
        audio::HEADER_LEN + self.slots * self.fpp * self.format.bytes()
    }

    /// The bytes of one frame: a sample for every slot.
    fn stride(&self) -> usize {
        (self.slots * self.format.bytes()).max(1)
    }
}

/// The receive buffer's size: more than any audio packet, which stays
/// within an Ethernet frame.
const RECV_BUFFER: usize = 2048;

/// The most frames a received packet can carry for a flow.
fn max_frames(params: &FlowParams) -> usize {
    ((RECV_BUFFER - audio::HEADER_LEN) / params.stride()).max(1)
}

/// Receives one flow's packets into the rings and keeps the flow alive.
fn receive(
    socket: std::net::UdpSocket,
    clock: MediaClock,
    mut rx: Receiver,
    stop: Arc<AtomicBool>,
) {
    let mut buf = vec![0u8; RECV_BUFFER];
    // The first load on a thread registers it with arc-swap, which may
    // allocate: do it now rather than on the first packet.
    drop(rx.routing.load());
    let mut next_keepalive = local_now_ns();
    let interval = KEEPALIVE_INTERVAL.as_nanos() as u64;
    while !stop.load(Ordering::Relaxed) {
        if let Ok((len, from)) = socket.recv_from(&mut buf) {
            rx.datagram(&buf[..len], from, local_now_ns(), &clock);
        }
        let now = local_now_ns();
        if now >= next_keepalive {
            // Only to the transmitter's source we follow (PROTOCOL.md §8.5):
            // a keepalive to any other would keep a stray flow alive.
            if let Some(source) = rx.source {
                let _ = socket.send_to(&audio::KEEPALIVE, source);
            }
            next_keepalive = now + interval;
        }
    }
}

/// The per-packet work of a receive thread. It runs on the real-time thread
/// and must not allocate, lock or log: it counts what goes wrong, and the
/// manager reports it.
struct Receiver {
    params: FlowParams,
    routing: Routing,
    rings: Vec<Arc<TimedRing>>,
    probes: Probes,
    samples: Vec<Sample>,
    /// Where the transmitter sends the flow from, once known.
    source: Option<SocketAddr>,
    /// Local time of the last packet accepted from `source`.
    source_ns: u64,
    /// Where the next packet should start.
    expected: Option<u64>,
}

impl Receiver {
    fn new(
        params: FlowParams,
        routing: Routing,
        rings: Vec<Arc<TimedRing>>,
        probes: Probes,
    ) -> Self {
        Self {
            params,
            routing,
            rings,
            probes,
            // Room for any packet that fits the receive buffer: a
            // transmitter may send another fpp than the one asked for.
            samples: vec![0; max_frames(&params)],
            source: None,
            source_ns: 0,
            expected: None,
        }
    }

    /// Handles one datagram received at local time `now_ns`.
    fn datagram(&mut self, data: &[u8], from: SocketAddr, now_ns: u64, clock: &MediaClock) {
        if !self.accept(data.len(), from, now_ns) {
            let Probes { counters, stats, .. } = &self.probes;
            counters.rx_rejected_packets.fetch_add(1, Ordering::Relaxed);
            stats.rejected_packets.fetch_add(1, Ordering::Relaxed);
            let ip = match from.ip() {
                IpAddr::V4(ip) => u32::from(ip),
                IpAddr::V6(_) => 0,
            };
            let last =
                (ip as u64) << 32 | (from.port() as u64) << 16 | data.len().min(0xffff) as u64;
            stats.last_rejected.store(last, Ordering::Relaxed);
            return;
        }
        let p = self.params;
        let Ok(packet) = AudioPacket::parse(data, p.slots, p.format) else { return };
        let ts = packet.timestamp(p.sample_rate);
        let frames = packet.frames() as u64;
        self.sequence(ts, frames);
        let Probes { last_packet_ns, counters, stats } = &self.probes;
        counters.rx_packets.fetch_add(1, Ordering::Relaxed);
        stats.packets.fetch_add(1, Ordering::Relaxed);
        if let Some(now) = clock.now_samples(p.sample_rate) {
            // Its first sample plays `latency` samples after its timestamp,
            // so the packet must be here by then.
            let arrival = now.saturating_sub(ts);
            stats.max_arrival.fetch_max(arrival.min(u64::from(u32::MAX)) as u32, Ordering::Relaxed);
            if arrival > p.latency {
                counters.rx_late_packets.fetch_add(1, Ordering::Relaxed);
                stats.late_packets.fetch_add(1, Ordering::Relaxed);
            }
            // Stamped in our future: the transmitter's clock is ahead of
            // ours, which delays its audio by as much.
            let lead = ts.saturating_sub(now);
            if lead > frames {
                counters.rx_early_packets.fetch_add(1, Ordering::Relaxed);
                stats.early_packets.fetch_add(1, Ordering::Relaxed);
                stats.max_early.fetch_max(lead.min(u64::from(u32::MAX)) as u32, Ordering::Relaxed);
            }
        }
        let routes = self.routing.load();
        for (slot, targets) in routes.slots.iter().enumerate().take(p.slots) {
            if targets.is_empty() || ts < routes.fresh_from.get(slot).copied().unwrap_or(0) {
                continue;
            }
            let n = packet.read_channel(slot, &mut self.samples);
            for &ch in targets {
                if let Some(ring) = self.rings.get(ch) {
                    ring.write(ts + p.offset, &self.samples[..n]);
                }
            }
        }
        last_packet_ns.store(now_ns, Ordering::Relaxed);
    }

    /// Whether a datagram is one of the flow's packets: from the
    /// transmitter's address and the source port its packets have been
    /// coming from, and whole frames of the flow's slots. Anything else (a
    /// flow that a previous receiver on this port left behind, a second flow
    /// created by a retried request) would be mixed into this flow's
    /// channels, or decoded with the wrong layout.
    ///
    /// The frame count is not checked against the fpp we asked for:
    /// receivers derive it from the length (PROTOCOL.md §8.2), so a
    /// transmitter that picks another fpp still plays rather than going
    /// silent for good.
    fn accept(&mut self, len: usize, from: SocketAddr, now_ns: u64) -> bool {
        let payload = len.saturating_sub(audio::HEADER_LEN);
        let whole = len > audio::HEADER_LEN && payload % self.params.stride() == 0;
        if from.ip() != IpAddr::V4(self.params.tx_ip) || !whole {
            return false;
        }
        match self.source {
            Some(source) if source == from => {}
            // Another port: only once the one we follow has gone quiet.
            Some(_) if now_ns.saturating_sub(self.source_ns) < SOURCE_HOLD_NS => return false,
            _ => self.source = Some(from),
        }
        self.source_ns = now_ns;
        true
    }

    /// Counts packets missing from the timestamp sequence, in packets of
    /// the size received.
    fn sequence(&mut self, ts: u64, frames: u64) {
        let Probes { counters, stats, .. } = &self.probes;
        let window = u64::from(self.params.sample_rate);
        let next = match self.expected {
            Some(exp) if ts > exp && ts - exp <= window => {
                let lost = (ts - exp).div_ceil(frames.max(1));
                counters.rx_lost_packets.fetch_add(lost, Ordering::Relaxed);
                stats.lost_packets.fetch_add(lost, Ordering::Relaxed);
                ts + frames
            }
            // A late duplicate or reordered packet: the sequence goes on
            // where it was.
            Some(exp) if ts < exp && exp - ts <= window => exp,
            Some(exp) if ts != exp => {
                stats.discontinuities.fetch_add(1, Ordering::Relaxed);
                ts + frames
            }
            _ => ts + frames,
        };
        self.expected = Some(next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fpp_choice() {
        let txt = ChannelTxt {
            id: 1,
            sample_rate: 48_000,
            bits_per_sample: 24,
            pcm_type: 0x0e,
            latency_ns: 1_000_000,
            fpp_max: 32,
            fpp_min: 4,
            nchan: 8,
            dbcp1: 0x1102,
            is_default_name: true,
            multicast: None,
        };
        // 4 ms at 48 kHz → 48, capped by the transmitter's 32.
        assert_eq!(choose_fpp(192, &txt, 2, SampleFormat::S24), 32);
        // 0.25 ms → 3, raised to the transmitter's minimum.
        assert_eq!(choose_fpp(12, &txt, 2, SampleFormat::S24), 4);
        // 1 ms → 12.
        assert_eq!(choose_fpp(48, &txt, 2, SampleFormat::S24), 12);
        // Wide flows are limited by the datagram size.
        let wide = ChannelTxt { fpp_max: 256, ..txt };
        assert_eq!(choose_fpp(4800, &wide, 64, SampleFormat::S32), 5);
    }

    fn txt(id: u16, latency_ns: u32, fpp_max: u16, fpp_min: u16) -> ChannelTxt {
        ChannelTxt {
            id,
            sample_rate: 48_000,
            bits_per_sample: 24,
            pcm_type: 0x0e,
            latency_ns,
            fpp_max,
            fpp_min,
            nchan: 8,
            dbcp1: 0x1102,
            is_default_name: true,
            multicast: None,
        }
    }

    /// A flow plays at the latency its transmitter demands when that is more
    /// than ours (PROTOCOL.md §8.4), and never below its packet length.
    #[test]
    fn flow_latency_honours_the_transmitter() {
        // Dante Virtual Soundcard: 6 ms, 48 frames per packet, to a device
        // at 1 ms: 288 samples, 240 more than ours.
        let dvs = txt(1, 6_000_000, 48, 48);
        let fpp = choose_fpp(48, &dvs, 8, SampleFormat::S24);
        assert_eq!(fpp, 48);
        assert_eq!(flow_latency(48, &dvs, fpp, 48_000), 288);
        // A transmitter asking for less than ours: ours.
        assert_eq!(flow_latency(480, &txt(1, 1_000_000, 32, 4), 32, 48_000), 480);
        // Packets longer than our latency: at least one packet.
        assert_eq!(flow_latency(12, &txt(1, 0, 32, 16), 16, 48_000), 16);
        // Nothing advertised: ours.
        assert_eq!(flow_latency(96, &txt(1, 0, 32, 2), 24, 96_000), 96);
        assert_eq!(max_latency_samples(48_000), 1920);
    }

    #[test]
    fn retries_back_off_from_the_failure() {
        let delays: Vec<u128> = (1..=6).map(|n| Retry::FAST.delay(n).as_millis()).collect();
        assert_eq!(delays, [250, 500, 1000, 1000, 1000, 1000]);
        assert_eq!(Retry::LOOKUP.delay(1), Duration::from_millis(500));
        assert_eq!(Retry::LOOKUP.delay(9), Duration::from_secs(2));
        assert_eq!(Retry::REFUSED.delay(100), Duration::from_secs(5));
    }

    /// Timeouts and our own network failing are not the transmitter
    /// refusing.
    #[test]
    fn failures_are_told_apart() {
        use std::io::{Error as IoError, ErrorKind};
        let io = |kind| Error::Io(IoError::from(kind));
        assert_eq!(classify(&Error::Timeout("x".into())), (status::NO_CONNECTION, Retry::FAST));
        assert_eq!(classify(&io(ErrorKind::AddrNotAvailable)), (status::RX_FAIL, Retry::FAST));
        assert_eq!(classify(&io(ErrorKind::NetworkDown)), (status::RX_FAIL, Retry::FAST));
        assert_eq!(classify(&io(ErrorKind::HostUnreachable)), (status::NO_CONNECTION, Retry::FAST));
        assert_eq!(
            classify(&Error::Refused(dbcp::error::TOO_MANY_FLOWS)),
            (SubscriptionStatus::TxNoFlows, Retry::REFUSED)
        );
        assert_eq!(
            classify(&Error::Refused(dbcp::error::SAMPLE_RATE_MISMATCH)),
            (status::CHANNEL_FORMAT, Retry::REFUSED)
        );
        assert_eq!(
            classify(&Error::Refused(dbcp::error::INVALID_PARAMETER)),
            (SubscriptionStatus::TxFail, Retry::REFUSED)
        );
        // The codes Dante Controller knows (PROTOCOL.md §4.14).
        assert_eq!(status::NO_CONNECTION.code(), 0x0f);
        assert_eq!(status::RX_FAIL.code(), 0x13);
        assert_eq!(status::CHANNEL_LATENCY.code(), 0x1a);
    }

    const TX: Ipv4Addr = Ipv4Addr::LOCALHOST;
    const RATE: u32 = 48_000;

    /// A receiver for a flow of `slots` slots of `fpp` 24-bit frames from
    /// [`TX`], slot k feeding ring k.
    struct TestRx {
        rx: Receiver,
        rings: Vec<Arc<TimedRing>>,
        routing: Routing,
        stats: Arc<FlowStats>,
        counters: Arc<Counters>,
        clock: MediaClock,
    }

    fn test_rx(slots: usize, fpp: usize, offset: u64) -> TestRx {
        let params = FlowParams {
            tx_ip: TX,
            slots,
            fpp,
            format: SampleFormat::S24,
            sample_rate: RATE,
            latency: 480 + offset,
            offset,
        };
        let routes =
            Routes { slots: (0..slots).map(|s| vec![s]).collect(), fresh_from: vec![0; slots] };
        let routing: Routing = Arc::new(ArcSwap::from_pointee(routes));
        let rings: Vec<_> = (0..slots).map(|_| Arc::new(TimedRing::new(1 << 15))).collect();
        let stats = Arc::new(FlowStats::default());
        let counters = Arc::new(Counters::default());
        let probes = Probes {
            last_packet_ns: Arc::new(AtomicU64::new(0)),
            counters: counters.clone(),
            stats: stats.clone(),
        };
        let rx = Receiver::new(params, routing.clone(), rings.clone(), probes);
        TestRx { rx, rings, routing, stats, counters, clock: ovsc_clock::system_clock() }
    }

    impl TestRx {
        fn now(&self) -> u64 {
            self.clock.now_samples(RATE).unwrap()
        }

        /// Sends a packet stamped `ts` whose slot k carries `value + k`.
        fn send(&mut self, from: SocketAddr, at_ns: u64, ts: u64, value: i32) {
            let p = self.rx.params;
            let data = packet(ts, p.slots, p.fpp, value);
            self.rx.datagram(&data, from, at_ns, &self.clock);
        }
    }

    fn packet(ts: u64, slots: usize, fpp: usize, value: i32) -> Vec<u8> {
        let mut out = vec![0u8; audio::HEADER_LEN + slots * fpp * 3];
        let n = audio::encode_packet(&mut out, ts, RATE, SampleFormat::S24, slots, fpp, |_, c| {
            (value + c as i32) << 8
        });
        out.truncate(n.expect("packet fits"));
        out
    }

    fn get(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }

    fn from(ip: Ipv4Addr, port: u16) -> SocketAddr {
        SocketAddr::from((ip, port))
    }

    /// Only the transmitter's packets of whole frames, from the port they
    /// have been coming from, reach the rings.
    #[test]
    fn packets_from_elsewhere_are_dropped() {
        let mut t = test_rx(2, 12, 0);
        let ts = t.now();
        let a = from(TX, 5000);
        let b = from(TX, 5001);
        // Another address.
        t.send(from(Ipv4Addr::new(127, 0, 0, 2), 5000), 1_000, ts, 1);
        // The transmitter: followed from now on.
        t.send(a, 1_000, ts, 100);
        // Another port of the same transmitter, while the first is busy.
        t.send(b, 2_000, ts + 12, 200);
        // The right source, the wrong size: not whole frames of 2 slots.
        let mut wrong = packet(ts + 24, 2, 6, 300);
        wrong.pop();
        t.rx.datagram(&wrong, a, 3_000, &t.clock);
        assert_eq!(get(&t.stats.rejected_packets), 3);
        assert_eq!(get(&t.counters.rx_rejected_packets), 3);
        assert_eq!(get(&t.stats.packets), 1);
        let last = get(&t.stats.last_rejected);
        assert_eq!(last >> 32, u32::from(TX) as u64);
        assert_eq!((last >> 16) & 0xffff, 5000);
        assert_eq!(last & 0xffff, wrong.len() as u64);
        assert_eq!(t.rx.source, Some(a));
        // Only the accepted packet was written.
        assert_eq!(t.rings[1].read_one(ts), Some(101 << 8));
        assert_eq!(t.rings[0].read_one(ts + 12), None);
        assert_eq!(t.rings[0].read_one(ts + 24), None);

        // Once the followed port has been quiet a while, another may take
        // over: a transmitter that moved its flow to a new socket.
        t.send(b, 1_000 + SOURCE_HOLD_NS + 1, ts + 36, 400);
        assert_eq!(t.rx.source, Some(b));
        assert_eq!(t.rings[0].read_one(ts + 36), Some(400 << 8));
        t.send(a, 1_000 + SOURCE_HOLD_NS + 2, ts + 48, 500);
        assert_eq!(t.rings[0].read_one(ts + 48), None);
        assert_eq!(get(&t.stats.rejected_packets), 4);
    }

    /// Receivers derive fpp from a packet's length (PROTOCOL.md §8.2): a
    /// transmitter that sends another fpp than the one requested still
    /// plays, every frame of it, and its gaps count in its own packets.
    #[test]
    fn packets_of_another_fpp_play_whole() {
        let mut t = test_rx(2, 12, 0);
        let src = from(TX, 5000);
        let ts = t.now();
        for (i, fpp) in [(0u64, 16usize), (1, 16), (3, 16)] {
            let at = ts + i * fpp as u64;
            let data = packet(at, 2, fpp, 10 * i as i32);
            t.rx.datagram(&data, src, 1_000 + i, &t.clock);
        }
        assert_eq!(get(&t.stats.rejected_packets), 0);
        assert_eq!(get(&t.stats.packets), 3);
        for i in [0u64, 1, 3] {
            for f in 0..16 {
                let v = (10 * i as i32 + 1) << 8;
                assert_eq!(t.rings[1].read_one(ts + i * 16 + f), Some(v), "packet {i} frame {f}");
            }
        }
        assert_eq!(t.rings[0].read_one(ts + 2 * 16), None);
        // One packet of 16 frames is missing, not two of the 12 asked for.
        assert_eq!(get(&t.stats.lost_packets), 1);

        // The largest packet that fits the receive buffer is read whole.
        let big = max_frames(&t.rx.params);
        let at = ts + 4 * 16;
        let data = packet(at, 2, big, 7);
        assert!(data.len() <= RECV_BUFFER);
        t.rx.datagram(&data, src, 2_000, &t.clock);
        assert_eq!(t.rings[0].read_one(at + big as u64 - 1), Some(7 << 8));
    }

    #[test]
    fn gaps_count_as_lost_packets() {
        let mut t = test_rx(1, 12, 0);
        let src = from(TX, 5000);
        let ts = t.now();
        t.send(src, 1, ts, 1);
        t.send(src, 2, ts + 12, 1);
        // Two packets missing.
        t.send(src, 3, ts + 48, 1);
        // One of them arrives after all: not a new gap.
        t.send(src, 4, ts + 24, 1);
        t.send(src, 5, ts + 60, 1);
        assert_eq!(get(&t.stats.lost_packets), 2);
        assert_eq!(get(&t.counters.rx_lost_packets), 2);
        assert_eq!(get(&t.stats.discontinuities), 0);
        // A jump of seconds is a restart or a clock step, not loss.
        t.send(src, 6, ts + 10 * RATE as u64, 1);
        assert_eq!(get(&t.stats.lost_packets), 2);
        assert_eq!(get(&t.stats.discontinuities), 1);
        assert_eq!(get(&t.stats.packets), 6);
    }

    #[test]
    fn late_and_early_packets_are_counted() {
        let mut t = test_rx(1, 12, 0);
        let src = from(TX, 5000);
        let now = t.now();
        // On time.
        t.send(src, 1, now - 100, 1);
        // Older than the latency (480).
        t.send(src, 2, now - 10_000, 1);
        // Stamped 100 ms in our future: the transmitter's clock is ahead.
        t.send(src, 3, now + 4_800, 1);
        assert_eq!(get(&t.stats.late_packets), 1);
        assert_eq!(get(&t.counters.rx_late_packets), 1);
        assert_eq!(get(&t.stats.early_packets), 1);
        assert_eq!(get(&t.counters.rx_early_packets), 1);
        let lead = t.stats.max_early.load(Ordering::Relaxed);
        assert!((4_000..=4_800).contains(&lead), "{lead}");
        assert!(t.stats.max_arrival.load(Ordering::Relaxed) >= 10_000);
    }

    /// A flow that needs more latency than ours plays that much later.
    #[test]
    fn samples_are_written_after_the_offset() {
        let mut t = test_rx(1, 12, 240);
        let src = from(TX, 5000);
        let ts = t.now() - 500;
        t.send(src, 1, ts, 7);
        assert_eq!(t.rings[0].read_one(ts), None);
        assert_eq!(t.rings[0].read_one(ts + 240), Some(7 << 8));
        assert_eq!(t.rings[0].read_one(ts + 251), Some(7 << 8));
        // 500 samples old: late for our 480, not for the flow's 720.
        assert_eq!(get(&t.stats.late_packets), 0);
    }

    /// A slot that just took a new transmit channel ignores packets stamped
    /// before the switch, which may still carry the old one.
    #[test]
    fn a_new_slot_skips_packets_from_before_the_switch() {
        let mut t = test_rx(2, 12, 0);
        let src = from(TX, 5000);
        let switch = t.now();
        t.routing
            .store(Arc::new(Routes { slots: vec![vec![0], vec![1]], fresh_from: vec![switch, 0] }));
        t.send(src, 1, switch - 24, 10);
        t.send(src, 2, switch, 20);
        assert_eq!(t.rings[0].read_one(switch - 24), None);
        assert_eq!(t.rings[1].read_one(switch - 24), Some(11 << 8));
        assert_eq!(t.rings[0].read_one(switch), Some(20 << 8));
    }

    /// The receive thread never logs, however much goes wrong: it counts,
    /// and the manager reports at most every few seconds.
    #[test]
    fn the_receive_path_does_not_log() {
        use std::sync::atomic::AtomicUsize;

        #[derive(Clone)]
        struct Lines(Arc<AtomicUsize>);
        impl std::io::Write for Lines {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.fetch_add(buf.iter().filter(|&&b| b == b'\n').count(), Ordering::Relaxed);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let lines = Lines(Arc::new(AtomicUsize::new(0)));
        let writer = lines.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let mut t = test_rx(2, 12, 0);
            let src = from(TX, 5000);
            let now = t.now();
            for i in 0..1000u64 {
                // Late, with gaps, interleaved with strays.
                t.send(src, i, now - 100_000 + i * 36, 1);
                t.send(from(Ipv4Addr::new(127, 0, 0, 3), 6000), i, now, 1);
            }
            assert_eq!(get(&t.stats.late_packets), 1000);
            assert!(get(&t.stats.lost_packets) > 0);
            assert_eq!(get(&t.stats.rejected_packets), 1000);
            // The subscriber works: a log line here is counted.
            tracing::debug!("done");
        });
        assert_eq!(lines.0.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn problems_are_reported_at_most_every_few_seconds() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let late = |n: u64| Trouble { late: n, ..Trouble::default() };
        let mut h = Health::new(t0);
        assert_eq!(h.update(late(0), at(1)), (false, None));
        // The first problem is reported at once.
        assert_eq!(h.update(late(3), at(2)), (true, Some((late(3), Duration::from_secs(2)))));
        // More within the interval: counted, not reported yet.
        assert_eq!(h.update(late(5), at(3)), (true, None));
        // Degraded until the problems have stopped for a while.
        assert_eq!(h.update(late(5), at(7)), (true, None));
        assert_eq!(h.update(late(5), at(8)), (false, None));
        // Then what accumulated since the last report.
        assert_eq!(h.update(late(5), at(13)), (false, Some((late(2), Duration::from_secs(11)))));
        assert_eq!(h.update(late(5), at(30)), (false, None));
        // Rejected datagrams are reported but don't degrade the audio.
        let rejected = Trouble { late: 5, rejected: 4, ..Trouble::default() };
        let (degraded, report) = h.update(rejected, at(31));
        assert!(!degraded);
        assert_eq!(report.map(|(t, _)| t.rejected), Some(4));
    }

    /// Keepalives go to the transmitter's source we follow, never to a
    /// stray sender, which would keep its flow alive.
    #[test]
    fn keepalives_go_to_the_followed_source_only() {
        let socket = std::net::UdpSocket::bind((TX, 0)).unwrap();
        let addr = socket.local_addr().unwrap();
        let t = test_rx(1, 12, 0);
        let (alive, _) = mpsc::channel(1);
        let worker = Worker::spawn(99, socket, t.clock.clone(), t.rx, alive).unwrap();
        let tx = std::net::UdpSocket::bind((TX, 0)).unwrap();
        let stray = std::net::UdpSocket::bind((TX, 0)).unwrap();
        for s in [&tx, &stray] {
            s.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        }
        let start = t.clock.now_samples(RATE).unwrap();
        let mut keepalives = [0, 0];
        let mut buf = [0u8; 64];
        for i in 0..40u64 {
            tx.send_to(&packet(start + i * 12, 1, 12, 1), addr).unwrap();
            stray.send_to(&packet(start + i * 12, 1, 12, 2), addr).unwrap();
            for (n, s) in keepalives.iter_mut().zip([&tx, &stray]) {
                while let Ok(len) = s.recv(&mut buf) {
                    assert_eq!(&buf[..len], &audio::KEEPALIVE);
                    *n += 1;
                }
            }
        }
        drop(worker);
        assert!(keepalives[0] >= 2, "{keepalives:?}");
        assert_eq!(keepalives[1], 0);
        assert_eq!(get(&t.stats.rejected_packets), 40);
        assert_eq!(t.rings[0].read_one(start), Some(1 << 8));
    }

    mod manager {
        use super::*;
        use crate::config::{Channels, DeviceConfig, Ports};
        use crate::device::Device;
        use crate::directory::StaticDirectory;
        use crate::info::DeviceInfo;
        use crate::net::Interface;
        use crate::state::{State, Subscription};

        fn config(name: &str, base: u16) -> DeviceConfig {
            DeviceConfig {
                name: name.into(),
                interface: "127.0.0.1".into(),
                tx_channels: Channels::Count(4),
                rx_channels: Channels::Count(4),
                latency_ms: 10.0,
                ports: Ports {
                    arc: base,
                    cmc: base + 1,
                    flow_control: base + 2,
                    settings: base + 3,
                },
                discovery: false,
                process_id: base,
                ..Default::default()
            }
        }

        fn manager(
            config: &DeviceConfig,
            iface: Interface,
            directory: StaticDirectory,
        ) -> RxManager {
            let info = DeviceInfo::new(config, iface).unwrap();
            let state = State {
                name: config.name.clone(),
                tx_names: info.tx_channels.clone(),
                rx_names: info.rx_channels.clone(),
                subscriptions: vec![None; info.rx_channels.len()],
                tx_flows: Vec::new(),
                rx_flows: Vec::new(),
                rx_flow_stats: Vec::new(),
                latency_override_ns: None,
                name_conflict: None,
            };
            let clock = ovsc_clock::system_clock();
            let ring_capacity = crate::buffer::DEFAULT_CAPACITY;
            let (shared, _) = Shared::new(info, state, clock, ring_capacity, None, false).unwrap();
            let (threads, _) = mpsc::channel(1);
            RxManager::new(shared, Directory::Static(directory), threads)
        }

        fn subscribe(mgr: &RxManager, rx: usize, channel: &str, device: &str) {
            mgr.shared.modify(|s| {
                s.subscriptions[rx] = Some(Subscription {
                    tx_channel: channel.into(),
                    tx_device: device.into(),
                    status: SubscriptionStatus::Unresolved,
                })
            });
        }

        fn status(mgr: &RxManager, rx: usize) -> SubscriptionStatus {
            mgr.shared.state().subscriptions[rx].as_ref().unwrap().status
        }

        /// Runs passes, applying what jobs find, until `done`.
        async fn drive(mgr: &mut RxManager, mut done: impl FnMut(&RxManager) -> bool) {
            for _ in 0..200 {
                mgr.reconcile().await;
                if done(mgr) {
                    return;
                }
                tokio::select! {
                    Some(joined) = mgr.jobs.join_next_with_id() => mgr.finished(joined),
                    _ = tokio::time::sleep(Duration::from_millis(25)) => {}
                }
            }
            panic!("timed out");
        }

        /// The receive thread's routing is replaced only when the routes
        /// change, not on every pass.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn routing_is_republished_only_when_it_changes() {
            let tx =
                Device::start(config("unit-tx", 24760), ovsc_clock::system_clock()).await.unwrap();
            let directory = StaticDirectory::new();
            for entry in tx.directory_entries() {
                directory.insert(entry);
            }
            let rx_config = config("unit-rx", 24770);
            let mut mgr = manager(&rx_config, Interface::find("127.0.0.1").unwrap(), directory);
            subscribe(&mgr, 0, "01", "unit-tx");
            drive(&mut mgr, |m| m.flows.len() == 1 && m.flows[0].routes.feeds(0)).await;

            let table = mgr.flows[0].routing.load_full();
            for _ in 0..5 {
                mgr.reconcile().await;
            }
            assert!(Arc::ptr_eq(&table, &mgr.flows[0].routing.load_full()));
            assert!(Arc::ptr_eq(&table, &mgr.flows[0].routes));

            subscribe(&mgr, 1, "02", "unit-tx");
            drive(&mut mgr, |m| m.flows[0].routes.feeds(1)).await;
            assert!(!Arc::ptr_eq(&table, &mgr.flows[0].routing.load_full()));
            // The table the thread may still be reading is kept for a while.
            assert!(mgr.retired.iter().any(|(_, t)| Arc::ptr_eq(t, &table)));
            mgr.stop_all().await;
            tx.shutdown().await;
        }

        /// When our own address is gone, the subscription says so (RX_FAIL),
        /// not that the transmitter refused, and is retried soon.
        #[tokio::test]
        async fn a_local_address_problem_is_not_the_transmitters() {
            let directory = StaticDirectory::new();
            directory.insert(ResolvedChannel {
                device: "far-tx".into(),
                channel: "01".into(),
                addr: Ipv4Addr::LOCALHOST,
                flow_control_port: 24799,
                txt: txt(1, 1_000_000, 32, 4),
            });
            // TEST-NET-1: never assigned to this host.
            let gone = Interface {
                name: "test0".into(),
                ip: Ipv4Addr::new(192, 0, 2, 1),
                netmask: Ipv4Addr::new(255, 255, 255, 0),
                gateway: Ipv4Addr::UNSPECIFIED,
                mac: [0; 6],
                link_speed_mbps: 0,
            };
            let mut mgr = manager(&config("unit-gone", 24780), gone, directory);
            subscribe(&mgr, 0, "01", "far-tx");
            drive(&mut mgr, |m| status(m, 0) == status::RX_FAIL).await;
            let retry = mgr.failures[&0].retry_at.saturating_duration_since(Instant::now());
            assert!(retry <= Retry::FAST.first, "{retry:?}");
            assert!(mgr.flows.is_empty());
        }

        /// A subscription that keeps failing the same way is logged once,
        /// not on every retry for the rest of the show.
        #[tokio::test]
        async fn a_lasting_failure_is_logged_once() {
            use std::sync::atomic::AtomicUsize;

            #[derive(Clone)]
            struct Lines(Arc<AtomicUsize>);
            impl std::io::Write for Lines {
                fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                    let lines = buf.iter().filter(|&&b| b == b'\n').count();
                    self.0.fetch_add(lines, Ordering::Relaxed);
                    Ok(buf.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            let warnings = Lines(Arc::new(AtomicUsize::new(0)));
            let writer = warnings.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::WARN)
                .with_writer(move || writer.clone())
                .finish();
            let _log = tracing::subscriber::set_default(subscriber);

            // More latency than any device may run at.
            let directory = StaticDirectory::new();
            directory.insert(ResolvedChannel {
                device: "slow-tx".into(),
                channel: "01".into(),
                addr: Ipv4Addr::LOCALHOST,
                flow_control_port: 24799,
                txt: txt(1, 50_000_000, 32, 4),
            });
            let iface = Interface::find("127.0.0.1").unwrap();
            let mut mgr = manager(&config("unit-slow", 24790), iface, directory);
            subscribe(&mgr, 0, "01", "slow-tx");
            drive(&mut mgr, |m| status(m, 0) == status::CHANNEL_LATENCY).await;
            // Retried, here at once rather than after 1, 2 and 4 s: still
            // failing.
            for tries in 2..=4 {
                mgr.failures.get_mut(&0).unwrap().retry_at = Instant::now();
                drive(&mut mgr, |m| m.failures.get(&0).is_some_and(|f| f.count == tries)).await;
            }
            assert_eq!(status(&mgr, 0), status::CHANNEL_LATENCY);
            assert_eq!(warnings.0.load(Ordering::Relaxed), 1);
        }
    }
}
