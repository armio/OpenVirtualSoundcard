//! Transmit side: the flow-control server receivers ask for audio, and the
//! real-time thread that sends it.
//!
//! Packets are paced by the media clock: the packet stamped `t` (carrying
//! frames `t .. t + fpp`) is sent once media time reaches `t + guard`. The
//! guard keeps receivers from seeing timestamps from their future when our
//! clock runs slightly ahead of theirs.

use std::collections::{BTreeMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket as StdUdpSocket};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use ovsc_clock::{ClockSnapshot, local_now_ns, ns_to_samples, samples_to_ns};
use ovsc_proto::arc::TxFlowInfo;
use ovsc_proto::audio::{self, SampleFormat};
use ovsc_proto::dbcp::{self, FlowHandle, FlowRequest};
use ovsc_proto::frame::{Frame, Header, respond, result};

use crate::config::MAX_LATENCY_MS;
use crate::info::{FPP_MAX, FPP_MIN, MAX_CHANNELS_PER_TX_FLOW, MAX_TX_FLOWS};
use crate::net::{bind_udp, mark_media};
use crate::state::{Counters, Shared};

/// Unicast flows without keepalives for this long are dropped.
const KEEPALIVE_TIMEOUT_NS: u64 = 4_000_000_000;
/// Largest UDP payload we send.
const MAX_DATAGRAM: usize = 1472;
/// Longest the transmit thread sleeps, so it sees commands and keepalives
/// promptly.
const MAX_WAIT_NS: u64 = 20_000_000;
/// How long the transmit thread sleeps while the media clock has no time.
const NO_CLOCK_WAIT_NS: u64 = 50_000_000;
/// A packet sent more than this after its send time (timestamp + guard)
/// counts as late. A real-time thread wakes within tens of µs; at a 1 ms
/// receive latency the 0.5 ms guard leaves about 0.4 ms for scheduling and
/// the network, so a packet this late has used more than half of that.
const LATE_ALLOWANCE_NS: u64 = 250_000;
/// A media clock that steps back by more than this restarts the flows'
/// timelines; after a smaller step they wait for the clock to catch up.
const BACKWARD_STEP_NS: u64 = 100_000_000;
/// How often the flow-control task reports what the transmit thread
/// counted. The thread itself never logs: a log write can block it.
const REPORT_INTERVAL: Duration = Duration::from_secs(5);
/// How often flow control looks for the flows the transmit thread found
/// silent ([`Expiries`]) and takes back what the thread is done with
/// ([`Retired`]). The thread tells it neither through a channel that wakes
/// a task: that can allocate, and takes the runtime's lock. Next to
/// `KEEPALIVE_TIMEOUT_NS`, the delay is nothing.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Commands the transmit thread's queue holds: a removal and an addition
/// for every flow. Flow control keeps any more until there is room.
const COMMAND_QUEUE: usize = 2 * MAX_TX_FLOWS;
/// The most flows and channel lists flow control has handed to the
/// transmit thread and not had back: every flow the thread can run, and as
/// many again on their way in or out. The queue that hands them back holds
/// as many, so that the thread never finds it full.
const IN_FLIGHT_MAX: usize = 2 * MAX_TX_FLOWS;

pub enum TxCommand {
    Add(TxFlow),
    Remove(u16),
    /// New channels for flow `id`, promised to its receiver as
    /// `generation` (see [`Expiries`]).
    SetChannels {
        id: u16,
        channels: Vec<Option<usize>>,
        generation: u32,
    },
    Shutdown,
}

pub enum TxEvent {
    /// The transmit thread panicked and stopped: no flow sends any more.
    Failed,
}

/// What the transmit thread is done with: a flow removed or replaced, or
/// the channel list a flow had before an update (or an update for a flow it
/// no longer has). It hands them back to flow control, which drops them:
/// dropping a flow closes its socket and frees memory, which must not
/// happen on the thread.
enum Retired {
    // Never read: held only to be dropped where that does no harm.
    #[allow(dead_code)]
    Flow(TxFlow),
    #[allow(dead_code)]
    Channels(Vec<Option<usize>>),
}

/// Per flow id, the latest promise (generation) of a flow that the transmit
/// thread found silent for `KEEPALIVE_TIMEOUT_NS`, for flow control to
/// poll. The flow stops sending; flow control removes it only if it has
/// not promised it again (or replaced it) since, because a later promise
/// revives the flow in the thread. A store is all the thread does.
pub struct Expiries([AtomicU64; MAX_TX_FLOWS]);

/// Marks an [`Expiries`] slot that holds a generation (which can be 0).
const EXPIRED: u64 = 1 << 32;

impl Expiries {
    fn new() -> Self {
        Self(std::array::from_fn(|_| AtomicU64::new(0)))
    }

    /// Notes that flow `id` expired at `generation`. This replaces an
    /// earlier expiry of the same id that flow control has not taken yet,
    /// which a later promise made stale.
    fn report(&self, id: u16, generation: u32) {
        if let Some(slot) = usize::from(id).checked_sub(1).and_then(|i| self.0.get(i)) {
            slot.store(EXPIRED | u64::from(generation), Ordering::Release);
        }
    }

    /// The expiries reported since the last call, as (id, generation).
    fn take(&self) -> impl Iterator<Item = (u16, u32)> + '_ {
        self.0.iter().zip(1u16..).filter_map(|(slot, id)| {
            let value = slot.swap(0, Ordering::Acquire);
            (value & EXPIRED != 0).then_some((id, value as u32))
        })
    }
}

/// Flow control's end of what the transmit thread reports back: see
/// [`spawn_engine`] and [`FlowControl::run`].
pub struct TxLink {
    events: mpsc::UnboundedReceiver<TxEvent>,
    retired: std_mpsc::Receiver<Retired>,
    expiries: Arc<Expiries>,
}

/// A flow as owned by the transmit thread.
pub struct TxFlow {
    id: u16,
    /// The latest promise of this flow to its receiver.
    generation: u32,
    socket: StdUdpSocket,
    /// Local transmit channel per slot in the packet.
    channels: Vec<Option<usize>>,
    fpp: u64,
    format: SampleFormat,
    unicast: bool,
    next_ts: Option<u64>,
    /// Whether the flow's timeline is established, so that the lateness of
    /// its packets means something. The first packet after a (re)start is
    /// up to `fpp - 1` frames past its send time by construction: the
    /// timeline starts at the newest packet already due.
    steady: bool,
    last_heard_ns: u64,
    expired: bool,
}

impl TxFlow {
    fn new(
        id: u16,
        generation: u32,
        socket: StdUdpSocket,
        channels: Vec<Option<usize>>,
        fpp: u16,
        format: SampleFormat,
        unicast: bool,
    ) -> Self {
        Self {
            id,
            generation,
            socket,
            channels,
            fpp: fpp as u64,
            format,
            unicast,
            next_ts: None,
            steady: false,
            last_heard_ns: local_now_ns(),
            expired: false,
        }
    }
}

/// Starts the transmit thread.
///
/// Its command queue and the queue that hands flows back are bounded
/// (`std::sync::mpsc::sync_channel`): their buffers are allocated here, and
/// taking from or adding to them neither allocates nor frees, nor locks
/// while nobody blocks on them. Flow control never does (`try_send`); a
/// blocking send to the command queue, as for [`TxCommand::Shutdown`],
/// waits at most until the thread's next wake.
pub fn spawn_engine(
    shared: Arc<Shared>,
) -> (std_mpsc::SyncSender<TxCommand>, TxLink, std::thread::JoinHandle<()>) {
    let (cmd_tx, cmd_rx) = std_mpsc::sync_channel(COMMAND_QUEUE);
    let (retired_tx, retired_rx) = std_mpsc::sync_channel(IN_FLIGHT_MAX);
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    let expiries = Arc::new(Expiries::new());
    let thread_expiries = expiries.clone();
    let thread = std::thread::Builder::new()
        .name("ovsc-tx".into())
        .spawn(move || {
            supervise(&shared.counters, &ev_tx, || {
                engine(&shared, cmd_rx, &retired_tx, &thread_expiries)
            })
        })
        .expect("spawn transmit thread");
    (cmd_tx, TxLink { events: ev_rx, retired: retired_rx, expiries }, thread)
}

/// Runs the transmit thread's `body` and reports a panic in it, which would
/// otherwise end the thread silently: every flow would stop while flow
/// control kept listing them and accepting new ones.
fn supervise(counters: &Counters, events: &mpsc::UnboundedSender<TxEvent>, body: impl FnOnce()) {
    if catch_unwind(AssertUnwindSafe(body)).is_err() {
        counters.tx_engine_failed.store(true, Ordering::Relaxed);
        let _ = events.send(TxEvent::Failed);
    }
}

/// How old (now - timestamp, in samples) the oldest overdue packet of a flow
/// may be for the transmit thread to catch up rather than resynchronise.
///
/// A receiver plays our frame `ts` at `ts + latency`, with a latency of at
/// least the one we advertise and at most its own maximum (PROTOCOL.md 8.4,
/// Playout): 40 ms for OpenVirtualSoundcard (`MAX_LATENCY_MS`), about 20 ms
/// for Dante hardware (PROTOCOL.md 4.6.2, property 0x8302). A packet older
/// than that is useless to every receiver, so after a longer stall the
/// thread restarts at the newest due packet instead of queueing stale ones
/// ahead of the packets that still count; the few overdue packets that a
/// receiver at a high latency could still have played are skipped with
/// them. A tighter window, such as our own latency, would also skip
/// packets after short stalls that a receiver set to a higher latency than
/// ours still plays.
fn catch_up_window(latency_samples: u64, rate: u32) -> u64 {
    let max_latency_ns = (MAX_LATENCY_MS * 1e6) as u64;
    ns_to_samples(max_latency_ns, rate).max(latency_samples)
}

/// What one flow sends when the transmit thread wakes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Plan {
    /// Timestamp of the first packet to send.
    first: u64,
    /// Packets to send now, from `first` on, `fpp` frames apart.
    count: u64,
    /// Overdue packets passed over, never to be sent.
    skipped: u64,
    /// Whether the flow's timeline (re)starts at `first`.
    restarted: bool,
}

/// Plans a flow's packets at media time `now` (in samples). The packet
/// stamped `ts` is due once `now >= ts + guard`; `next` is the flow's next
/// timestamp, `None` for a new flow.
///
/// Overdue packets are sent oldest first, as long as the oldest is at most
/// `window` old. Beyond that (a long stall, or the media clock stepping
/// forward), and after the clock stepped back by more than `backward`, the
/// timeline restarts at the newest due packet, aligned to `fpp` as
/// receivers expect (PROTOCOL.md 8.4, Alignment).
fn plan(now: u64, next: Option<u64>, guard: u64, fpp: u64, window: u64, backward: u64) -> Plan {
    let fpp = fpp.max(1);
    let start = (now.saturating_sub(guard) / fpp) * fpp;
    let (first, skipped, restarted) = match next {
        Some(n) if n > now.saturating_add(backward) => (start, 0, true),
        Some(n) if now.saturating_sub(n) > window => (start, start.saturating_sub(n) / fpp, true),
        Some(n) => (n, 0, false),
        None => (start, 0, true),
    };
    let count = match now.checked_sub(guard) {
        Some(due) if first <= due => (due - first) / fpp + 1,
        _ => 0,
    };
    Plan { first, count, skipped, restarted }
}

/// Hands `value` back to flow control, from the transmit thread.
fn retire(retired: &std_mpsc::SyncSender<Retired>, value: Retired) {
    // The queue is never full: flow control keeps no more values out than
    // it holds (IN_FLIGHT_MAX). Only once flow control has gone, with the
    // device, does this fail, and the value is dropped here then.
    let _ = retired.try_send(value);
}

fn engine(
    shared: &Shared,
    commands: std_mpsc::Receiver<TxCommand>,
    retired: &std_mpsc::SyncSender<Retired>,
    expiries: &Expiries,
) {
    let counters = &*shared.counters;
    let realtime = crate::rt::raise_priority("transmit", crate::rt::RtClass::Transmit);
    counters.tx_not_realtime.store(!realtime, Ordering::Relaxed);
    let rate = shared.info.sample_rate;
    let guard = ns_to_samples(shared.info.tx_guard_ns, rate);
    let window = catch_up_window(shared.info.latency_samples, rate);
    let backward = ns_to_samples(BACKWARD_STEP_NS, rate);
    // Room for every flow from the start: growing would allocate.
    let mut flows: Vec<TxFlow> = Vec::with_capacity(MAX_TX_FLOWS);
    let mut packet = vec![0u8; MAX_DATAGRAM];
    let mut scratch = [0u8; 64];
    // Local time (ovsc_clock::local_now_ns) of the next wake-up.
    let mut wake_ns = local_now_ns() + 10_000_000;

    // Nothing in this loop logs: a log write is file I/O that can block
    // every flow. Problems go to `counters`, which flow control reports.
    // Nor does anything in it allocate, free, close a socket or wake a
    // task, any of which can wait on a lock held by a thread without
    // real-time priority: what the thread is done with goes back to flow
    // control (`retire`), and the flows it finds silent go to `expiries`.
    loop {
        // Every turn blocks here until the next packet is due, at most
        // MAX_WAIT_NS. The thread must keep blocking: XNU demotes a
        // real-time thread that runs for about a second without doing so.
        crate::rt::sleep_until(wake_ns);
        loop {
            match commands.try_recv() {
                Ok(TxCommand::Add(flow)) => match flows.iter().position(|f| f.id == flow.id) {
                    Some(i) => {
                        let old = std::mem::replace(&mut flows[i], flow);
                        retire(retired, Retired::Flow(old));
                    }
                    None if flows.len() < flows.capacity() => flows.push(flow),
                    // Flow control runs at most MAX_TX_FLOWS flows, so this
                    // does not happen; growing `flows` would allocate.
                    None => retire(retired, Retired::Flow(flow)),
                },
                Ok(TxCommand::Remove(id)) => {
                    if let Some(i) = flows.iter().position(|f| f.id == id) {
                        retire(retired, Retired::Flow(flows.remove(i)));
                    }
                }
                Ok(TxCommand::SetChannels { id, channels, generation }) => {
                    match flows.iter_mut().find(|f| f.id == id) {
                        Some(f) => {
                            let old = std::mem::replace(&mut f.channels, channels);
                            retire(retired, Retired::Channels(old));
                            f.generation = generation;
                            // Promised to the receiver again. This revives
                            // a flow this thread found silent after flow
                            // control made the promise, and flow control
                            // then ignores the expiry as stale.
                            f.last_heard_ns = local_now_ns();
                            f.expired = false;
                        }
                        None => retire(retired, Retired::Channels(channels)),
                    }
                }
                Ok(TxCommand::Shutdown) | Err(std_mpsc::TryRecvError::Disconnected) => return,
                Err(std_mpsc::TryRecvError::Empty) => break,
            }
        }

        let Some(snap) = shared.clock.snapshot() else {
            wake_ns = local_now_ns() + NO_CLOCK_WAIT_NS;
            continue;
        };
        let local = local_now_ns();
        let now = ns_to_samples(snap.media_ns_at(local), rate);
        let mut next_due: Option<u64> = None;

        for flow in flows.iter_mut().filter(|f| !f.expired) {
            // Anything the receiver sends us counts as a keepalive.
            loop {
                match flow.socket.recv(&mut scratch) {
                    Ok(_) => flow.last_heard_ns = local,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    // An earlier packet bounced (ICMP: port or host
                    // unreachable), reported on whichever call comes next.
                    Err(e) => {
                        count_send_error(counters, &e);
                        break;
                    }
                }
            }
            if flow.unicast && local.saturating_sub(flow.last_heard_ns) > KEEPALIVE_TIMEOUT_NS {
                flow.expired = true;
                expiries.report(flow.id, flow.generation);
                continue;
            }

            let plan = plan(now, flow.next_ts, guard, flow.fpp, window, backward);
            if plan.restarted {
                if flow.next_ts.is_some() {
                    counters.tx_resyncs.fetch_add(1, Ordering::Relaxed);
                }
                flow.steady = false;
            }
            if plan.skipped > 0 {
                counters.tx_skipped_packets.fetch_add(plan.skipped, Ordering::Relaxed);
            }
            let mut ts = plan.first;
            for _ in 0..plan.count {
                send_packet(shared, flow, &mut packet, ts, guard, &snap);
                ts += flow.fpp;
            }
            flow.next_ts = Some(ts);
            next_due = Some(next_due.map_or(ts + guard, |d: u64| d.min(ts + guard)));
        }

        // An absolute deadline: on macOS the thread waits for it with
        // mach_wait_until, which wakes it more precisely than a relative sleep.
        let latest = local_now_ns() + MAX_WAIT_NS;
        wake_ns = match next_due {
            Some(ts) => snap.local_ns_at(samples_to_ns(ts, rate)).min(latest),
            None => latest,
        };
    }
}

/// Encodes and sends `flow`'s packet stamped `ts` (due at `ts + guard`),
/// and counts how that went. Real-time safe: no allocation, no lock, no
/// log, and no panic.
fn send_packet(
    shared: &Shared,
    flow: &mut TxFlow,
    packet: &mut [u8],
    ts: u64,
    guard: u64,
    snap: &ClockSnapshot,
) {
    let counters = &*shared.counters;
    let rate = shared.info.sample_rate;
    let rings = &shared.tx_rings;
    let fpp = flow.fpp;
    let missing = flow.channels.iter().flatten().any(|&ch| {
        rings.get(ch).is_none_or(|r| r.read_one(ts).is_none() || r.read_one(ts + fpp - 1).is_none())
    });
    if missing {
        counters.tx_underruns.fetch_add(1, Ordering::Relaxed);
    }
    let channels = &flow.channels;
    let encoded = audio::encode_packet(
        packet,
        ts,
        rate,
        flow.format,
        channels.len(),
        fpp as usize,
        |frame, slot| {
            channels
                .get(slot)
                .copied()
                .flatten()
                .and_then(|ch| rings.get(ch))
                .and_then(|r| r.read_one(ts + frame as u64))
                .unwrap_or(0)
        },
    );
    // Flow control refuses flows whose packets would not fit; should one
    // slip through, it is counted rather than allowed to stop every flow.
    let Some(packet) = encoded.and_then(|len| packet.get(..len)) else {
        counters.tx_skipped_packets.fetch_add(1, Ordering::Relaxed);
        return;
    };
    match flow.socket.send(packet) {
        Ok(_) => {
            counters.tx_packets.fetch_add(1, Ordering::Relaxed);
            let sent = snap.media_ns_at(local_now_ns());
            let late_ns = sent.saturating_sub(samples_to_ns(ts + guard, rate));
            if flow.steady {
                counters.tx_max_lateness_ns.fetch_max(late_ns, Ordering::Relaxed);
                if late_ns > LATE_ALLOWANCE_NS {
                    counters.tx_late_packets.fetch_add(1, Ordering::Relaxed);
                }
            }
            flow.steady = true;
        }
        Err(e) => count_send_error(counters, &e),
    }
}

fn count_send_error(counters: &Counters, e: &std::io::Error) {
    counters.tx_send_errors.fetch_add(1, Ordering::Relaxed);
    counters.tx_last_send_error.store(e.raw_os_error().unwrap_or(0), Ordering::Relaxed);
}

/// The transmit thread's problem counters at one moment, to report what
/// changed between two of them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TxHealth {
    send_errors: u64,
    last_send_error: i32,
    late: u64,
    max_lateness_ns: u64,
    skipped: u64,
    resyncs: u64,
    underruns: u64,
}

impl TxHealth {
    fn read(c: &Counters) -> Self {
        let r = Ordering::Relaxed;
        Self {
            send_errors: c.tx_send_errors.load(r),
            last_send_error: c.tx_last_send_error.load(r),
            late: c.tx_late_packets.load(r),
            max_lateness_ns: c.tx_max_lateness_ns.load(r),
            skipped: c.tx_skipped_packets.load(r),
            resyncs: c.tx_resyncs.load(r),
            underruns: c.tx_underruns.load(r),
        }
    }

    /// One line about the transmit problems counted since `before`, if
    /// there were any. Underruns are left out: they count every packet sent
    /// while no application plays into the device, which is normal.
    fn problems_since(&self, before: &Self) -> Option<String> {
        let mut parts = Vec::new();
        let errors = self.send_errors.saturating_sub(before.send_errors);
        if errors > 0 {
            let why = match self.last_send_error {
                0 => String::new(),
                code => format!(" (the last: {})", std::io::Error::from_raw_os_error(code)),
            };
            parts.push(format!("{errors} send errors{why}"));
        }
        let late = self.late.saturating_sub(before.late);
        if late > 0 {
            parts.push(format!(
                "{late} packets left more than {} µs late (the latest so far {:.2} ms)",
                LATE_ALLOWANCE_NS / 1000,
                self.max_lateness_ns as f64 / 1e6
            ));
        }
        let resyncs = self.resyncs.saturating_sub(before.resyncs);
        if resyncs > 0 {
            parts.push(format!("{resyncs} flow resyncs after a stall or a media clock step"));
        }
        let skipped = self.skipped.saturating_sub(before.skipped);
        if skipped > 0 {
            parts.push(format!("{skipped} overdue packets skipped"));
        }
        (!parts.is_empty()).then(|| format!("transmit problems: {}", parts.join(", ")))
    }
}

/// Book-keeping for a flow on the control side.
struct ServerFlow {
    cookie: u32,
    /// The latest promise of this flow to its receiver: a new flow, a
    /// channel update or a re-request (see [`Expiries`]).
    generation: u32,
    dst: SocketAddrV4,
    fpp: u16,
    format: SampleFormat,
    info: TxFlowInfo,
}

fn handle_of(id: u16, cookie: u32) -> FlowHandle {
    let mut h = [0u8; 6];
    h[..2].copy_from_slice(&id.to_be_bytes());
    h[2..].copy_from_slice(&cookie.to_be_bytes());
    h
}

fn split_handle(h: FlowHandle) -> (u16, u32) {
    (u16::from_be_bytes([h[0], h[1]]), u32::from_be_bytes([h[2], h[3], h[4], h[5]]))
}

/// The flow-control server (port 4455 by default).
pub struct FlowControl {
    shared: Arc<Shared>,
    commands: std_mpsc::SyncSender<TxCommand>,
    /// Commands for the transmit thread that wait, oldest first, for room
    /// in its queue or for values to come back ([`FlowControl::flush`]).
    backlog: VecDeque<TxCommand>,
    /// Flows and channel lists handed to the transmit thread and not had
    /// back ([`Retired`]): at most `IN_FLIGHT_MAX`.
    in_flight: usize,
    flows: BTreeMap<u16, ServerFlow>,
    /// The last generation handed out, across all flows, so that an expiry
    /// can never match a flow promised since, whatever its id.
    generation: u32,
    /// Whether the transmit thread has stopped ([`TxEvent::Failed`]).
    engine_failed: bool,
}

impl FlowControl {
    pub fn new(shared: Arc<Shared>, commands: std_mpsc::SyncSender<TxCommand>) -> Self {
        Self {
            shared,
            commands,
            backlog: VecDeque::new(),
            in_flight: 0,
            flows: BTreeMap::new(),
            generation: 0,
            engine_failed: false,
        }
    }

    pub async fn run(mut self, socket: UdpSocket, link: TxLink) {
        let TxLink { mut events, retired, expiries } = link;
        let mut buf = vec![0u8; 2048];
        let mut poll = tokio::time::interval(POLL_INTERVAL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut report = tokio::time::interval(REPORT_INTERVAL);
        report.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut reported = TxHealth::read(&self.shared.counters);
        loop {
            tokio::select! {
                r = socket.recv_from(&mut buf) => {
                    let Ok((len, src)) = r else { continue };
                    let Ok(frame) = Frame::parse(&buf[..len]) else {
                        debug!("malformed flow-control packet from {src}");
                        continue;
                    };
                    if let Some(resp) = self.handle(&frame) {
                        let _ = socket.send_to(&resp, src).await;
                    }
                }
                // After a failure the thread is gone; keep answering
                // receivers, with refusals, rather than let them time out.
                ev = events.recv(), if !self.engine_failed => match ev {
                    Some(TxEvent::Failed) => self.engine_stopped(),
                    // The thread was shut down with the device.
                    None => return,
                },
                _ = poll.tick() => self.poll(&retired, &expiries),
                _ = report.tick() => {
                    let health = TxHealth::read(&self.shared.counters);
                    if let Some(problems) = health.problems_since(&reported) {
                        warn!("{problems} in the last {} s", REPORT_INTERVAL.as_secs());
                    }
                    let underruns = health.underruns.saturating_sub(reported.underruns);
                    if underruns > 0 {
                        debug!("tx underruns: {underruns} packets sent without audio from the backend");
                    }
                    reported = health;
                }
            }
        }
    }

    fn publish(&self) {
        let infos = self.flows.values().map(|f| f.info.clone()).collect();
        self.shared.state().tx_flows = infos;
    }

    /// Takes back what the transmit thread is done with, removes the flows
    /// it found silent, and hands it the commands that waited.
    fn poll(&mut self, retired: &std_mpsc::Receiver<Retired>, expiries: &Expiries) {
        while let Ok(value) = retired.try_recv() {
            // Here, off the transmit thread, this closes a flow's socket
            // and frees its channel list.
            drop(value);
            self.in_flight = self.in_flight.saturating_sub(1);
        }
        for (id, generation) in expiries.take() {
            self.expired(id, generation);
        }
        self.flush();
    }

    /// Queues `command` for the transmit thread, behind any that wait.
    /// False if the thread is gone.
    fn send(&mut self, command: TxCommand) -> bool {
        self.backlog.push_back(command);
        self.flush()
    }

    /// Moves the commands that wait into the transmit thread's queue, in
    /// order, while it has room and while the thread could hand back all
    /// they give it without finding that queue full (`IN_FLIGHT_MAX`). The
    /// rest wait for [`FlowControl::poll`]: flow control never blocks on
    /// the thread, nor drops a command. False if the thread is gone.
    fn flush(&mut self) -> bool {
        while let Some(command) = self.backlog.pop_front() {
            let gives =
                usize::from(matches!(command, TxCommand::Add(_) | TxCommand::SetChannels { .. }));
            if self.in_flight + gives > IN_FLIGHT_MAX {
                self.backlog.push_front(command);
                break;
            }
            match self.commands.try_send(command) {
                Ok(()) => self.in_flight += gives,
                Err(std_mpsc::TrySendError::Full(command)) => {
                    self.backlog.push_front(command);
                    break;
                }
                Err(std_mpsc::TrySendError::Disconnected(_)) => {
                    self.backlog.clear();
                    return false;
                }
            }
        }
        true
    }

    fn remove(&mut self, id: u16) {
        if self.flows.remove(&id).is_some() {
            self.send(TxCommand::Remove(id));
            self.publish();
        }
    }

    fn next_generation(&mut self) -> u32 {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    /// The transmit thread found flow `id` silent at `generation`.
    fn expired(&mut self, id: u16, generation: u32) {
        let Some(f) = self.flows.get(&id).filter(|f| f.generation == generation) else {
            // Promised again since, which revived it in the transmit
            // thread, or replaced or stopped, which removed it there.
            debug!("tx flow {id}: ignoring the expiry of an earlier promise");
            return;
        };
        let receiver = f.info.remote.as_ref().map_or("its receiver", |(device, _)| device);
        warn!(
            "tx flow {id} to {receiver} at {} expired: no keepalives for {} s",
            f.dst,
            KEEPALIVE_TIMEOUT_NS / 1_000_000_000
        );
        self.shared.counters.tx_expired_flows.fetch_add(1, Ordering::Relaxed);
        self.remove(id);
    }

    /// The transmit thread stopped (it already set `tx_engine_failed`).
    fn engine_stopped(&mut self) {
        self.engine_failed = true;
        error!(
            "the transmit thread stopped: {} flows to receivers send no audio, and flow \
             requests are refused until the device restarts",
            self.flows.len()
        );
        self.flows.clear();
        self.publish();
    }

    /// Gives flow `id` new channels and promises it to its receiver again.
    fn set_channels(&mut self, id: u16, ids: Vec<u16>, channels: Vec<Option<usize>>) {
        let generation = self.next_generation();
        if let Some(f) = self.flows.get_mut(&id) {
            f.info.channels = ids;
            f.generation = generation;
            self.send(TxCommand::SetChannels { id, channels, generation });
            self.publish();
        }
    }

    fn find(&self, handle: FlowHandle) -> Option<u16> {
        let (id, cookie) = split_handle(handle);
        self.flows.get(&id).filter(|f| f.cookie == cookie).map(|_| id)
    }

    /// Validates 1-based channel ids and maps them to ring indices.
    fn map_channels(&self, ids: &[u16]) -> Option<Vec<Option<usize>>> {
        let n = self.shared.info.tx_channels.len();
        ids.iter()
            .map(|&id| match id {
                0 => Some(None),
                id if (id as usize) <= n => Some(Some(id as usize - 1)),
                _ => None,
            })
            .collect()
    }

    pub fn handle(&mut self, frame: &Frame<'_>) -> Option<Vec<u8>> {
        let h = &frame.header;
        if h.result != result::REQUEST {
            return None;
        }
        Some(match h.opcode {
            dbcp::opcode::REQUEST_FLOW => match FlowRequest::decode(frame) {
                Ok(req) => self.request_flow(h, req),
                Err(e) => {
                    warn!("malformed flow request: {e}");
                    respond(h, dbcp::error::INVALID_PARAMETER, &[])
                }
            },
            dbcp::opcode::STOP_FLOW => {
                let handle = dbcp::decode_stop_flow(frame).ok()?;
                match self.find(handle) {
                    Some(id) => {
                        info!("tx flow {id} stopped by receiver");
                        self.remove(id);
                        respond(h, result::SUCCESS, &[])
                    }
                    None => respond(h, dbcp::error::FLOW_NOT_FOUND, &[]),
                }
            }
            dbcp::opcode::UPDATE_FLOW => {
                let (handle, ids) = dbcp::decode_update_flow(frame).ok()?;
                let Some(id) = self.find(handle) else {
                    return Some(respond(h, dbcp::error::FLOW_NOT_FOUND, &[]));
                };
                let Some(channels) = self.map_channels(&ids) else {
                    return Some(respond(h, dbcp::error::INVALID_PARAMETER, &[]));
                };
                let f = self.flows.get(&id)?;
                if let Err(why) = check_flow(f.fpp, channels.len(), f.format) {
                    warn!("refusing to change tx flow {id}: {why}");
                    return Some(respond(h, dbcp::error::INVALID_PARAMETER, &[]));
                }
                info!("tx flow {id} now carries channels {ids:?}");
                self.set_channels(id, ids, channels);
                respond(h, result::SUCCESS, &[])
            }
            other => {
                debug!("unhandled flow-control opcode {other:#06x}");
                return None;
            }
        })
    }

    fn request_flow(&mut self, h: &Header, req: FlowRequest) -> Vec<u8> {
        let shared = self.shared.clone();
        let info = &shared.info;
        info!(
            "{} requests flow {:?}: channels {:?} at {} Hz/{} bit, {} fpp, to {}:{}",
            req.rx_device,
            req.flow_name,
            req.channels,
            req.sample_rate,
            req.bits_per_sample,
            req.fpp,
            req.rx_addr,
            req.rx_port
        );
        if self.engine_failed {
            warn!("refusing flow: the transmit thread has stopped");
            return respond(h, result::FAILURE, &[]);
        }
        if req.sample_rate != info.sample_rate {
            warn!("refusing flow: sample rate {} != {}", req.sample_rate, info.sample_rate);
            return respond(h, dbcp::error::SAMPLE_RATE_MISMATCH, &[]);
        }
        let Some(format) = SampleFormat::from_bits(req.bits_per_sample) else {
            warn!("refusing flow: {} bits per sample", req.bits_per_sample);
            return respond(h, dbcp::error::INVALID_PARAMETER, &[]);
        };
        let Some(channels) = self.map_channels(&req.channels) else {
            warn!("refusing flow: channel ids {:?} out of range", req.channels);
            return respond(h, dbcp::error::INVALID_PARAMETER, &[]);
        };
        if let Err(why) = check_flow(req.fpp, channels.len(), format) {
            warn!("refusing flow: {why}");
            return respond(h, dbcp::error::INVALID_PARAMETER, &[]);
        }

        let dst = SocketAddrV4::new(req.rx_addr, req.rx_port);
        let existing = self.flows.iter().find(|(_, f)| f.dst == dst).map(|(&id, f)| (id, f));
        if let Some((id, f)) = existing {
            if f.fpp == req.fpp && f.format == format {
                // A receiver asking again for a flow it has gets it back
                // with the new channels, under the same handle (PROTOCOL.md
                // 6.7, Same destination).
                let handle = handle_of(id, f.cookie);
                if let Some(f) = self.flows.get_mut(&id) {
                    f.info.remote = Some((req.rx_device, req.flow_name));
                }
                self.set_channels(id, req.channels, channels);
                return dbcp::encode_flow_created(h, handle);
            }
            // Other packets than the flow sends: a receiver that changed its
            // latency, say. Answering with the old flow would send packets
            // it did not ask for, so it gets a new flow, with a new handle
            // that a late stop for the old one cannot match.
            info!(
                "tx flow {id} to {dst} re-requested at {} fpp/{} bit, was {} fpp/{} bit: \
                 replacing it",
                req.fpp,
                format.bits(),
                f.fpp,
                f.format.bits()
            );
            self.remove(id);
        }
        let Some(id) = (1..=MAX_TX_FLOWS as u16).find(|id| !self.flows.contains_key(id)) else {
            warn!("refusing flow: all {MAX_TX_FLOWS} transmit flows in use");
            return respond(h, dbcp::error::TOO_MANY_FLOWS, &[]);
        };
        let socket = match open_flow_socket(info.iface.ip, dst) {
            Ok(s) => s,
            Err(e) => {
                warn!("cannot open flow socket to {dst}: {e}");
                return respond(h, dbcp::error::INVALID_PARAMETER, &[]);
            }
        };
        let cookie: u32 = rand::random();
        let generation = self.next_generation();
        let unicast = !req.rx_addr.is_multicast();
        let flow = TxFlow::new(id, generation, socket, channels, req.fpp, format, unicast);
        if !self.send(TxCommand::Add(flow)) {
            return respond(h, result::FAILURE, &[]);
        }
        self.flows.insert(
            id,
            ServerFlow {
                cookie,
                generation,
                dst,
                fpp: req.fpp,
                format,
                info: TxFlowInfo {
                    id,
                    local_name: format!("{id}_{}", info.process_id),
                    remote: Some((req.rx_device, req.flow_name)),
                    dst_addr: req.rx_addr,
                    dst_port: req.rx_port,
                    channels: req.channels,
                    fpp: req.fpp,
                },
            },
        );
        self.publish();
        dbcp::encode_flow_created(h, handle_of(id, cookie))
    }
}

fn packet_size(fpp: u16, channels: usize, format: SampleFormat) -> usize {
    audio::HEADER_LEN + fpp as usize * channels * format.bytes()
}

/// Why a flow of `slots` slots, `fpp` frames per packet and `format` is
/// refused, if it is. Receivers choose within what we advertise
/// (PROTOCOL.md 6.2 and 8.3): `fpp` within TXT `fpp=32,2`, and at most
/// `MAX_CHANNELS_PER_TX_FLOW` slots (TXT `nchan` and ARC 0x1000 advertise
/// no more). The output safety offset of the macOS device covers packets of
/// up to `FPP_MAX` frames only: the frames of a larger one would not be
/// written yet when it leaves, and would go out as silence.
fn check_flow(fpp: u16, slots: usize, format: SampleFormat) -> Result<(), String> {
    if !(FPP_MIN..=FPP_MAX).contains(&fpp) {
        return Err(format!(
            "{fpp} frames per packet, outside the advertised {FPP_MIN} to {FPP_MAX}"
        ));
    }
    if !(1..=MAX_CHANNELS_PER_TX_FLOW as usize).contains(&slots) {
        return Err(format!("{slots} channel slots, outside 1 to {MAX_CHANNELS_PER_TX_FLOW}"));
    }
    let size = packet_size(fpp, slots, format);
    if size > MAX_DATAGRAM {
        return Err(format!("{size}-byte packets, more than {MAX_DATAGRAM}"));
    }
    Ok(())
}

fn open_flow_socket(local_ip: Ipv4Addr, dst: SocketAddrV4) -> std::io::Result<StdUdpSocket> {
    let socket = bind_udp(local_ip, 0)?;
    if let Err(e) = mark_media(&socket) {
        // Unmarked audio still flows, only without QoS priority.
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            warn!("cannot mark audio packets for QoS (DSCP EF): {e}");
        }
    }
    socket.connect(SocketAddr::V4(dst))?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Instant;

    use ovsc_clock::free_running_clock;

    use crate::config::{Channels, DeviceConfig};
    use crate::info::DeviceInfo;
    use crate::net::Interface;
    use crate::state::State;

    fn test_shared(tx_channels: u16) -> Arc<Shared> {
        let config = DeviceConfig {
            interface: "127.0.0.1".into(),
            tx_channels: Channels::Count(tx_channels),
            discovery: false,
            ..Default::default()
        };
        let info = DeviceInfo::new(&config, Interface::find("127.0.0.1").unwrap()).unwrap();
        let state = State {
            name: "tx-test".into(),
            tx_names: info.tx_channels.clone(),
            rx_names: info.rx_channels.clone(),
            subscriptions: vec![None; info.rx_channels.len()],
            tx_flows: Vec::new(),
            rx_flows: Vec::new(),
            rx_flow_stats: Vec::new(),
            latency_override_ns: None,
            remote_settings_locked: false,
            remote_settings_lock_set: None,
            name_conflict: None,
        };
        // One fixed mapping from the monotonic clock, so that the transmit
        // thread and a test reading the time agree exactly (the system
        // clock maps afresh on every read, off by any preemption between
        // its two clock reads).
        let clock = free_running_clock();
        Shared::new(info, state, clock, config.ring_capacity, None, false, Default::default())
            .unwrap()
            .0
    }

    fn flow_request(port: u16, fpp: u16, bits: u32, channels: &[u16]) -> FlowRequest {
        FlowRequest {
            rx_device: "console".into(),
            flow_name: "1".into(),
            sample_rate: 48_000,
            bits_per_sample: bits,
            fpp,
            channels: channels.to_vec(),
            rx_addr: Ipv4Addr::LOCALHOST,
            rx_port: port,
        }
    }

    /// Hands `packet` to `fc` and returns the response's result code and
    /// handle (if it carries one).
    fn call(fc: &mut FlowControl, packet: &[u8]) -> (u16, Option<FlowHandle>) {
        let resp = fc.handle(&Frame::parse(packet).unwrap()).expect("a response");
        let frame = Frame::parse(&resp).unwrap();
        (frame.header.result, dbcp::decode_flow_created(&frame).ok())
    }

    fn request(fc: &mut FlowControl, req: &FlowRequest) -> (u16, Option<FlowHandle>) {
        call(fc, &req.encode(7))
    }

    /// What the transmit thread was told, by flow id: (generation, fpp) for
    /// an add, the generation for a channel update, and removals.
    #[derive(Debug, PartialEq, Eq)]
    enum Sent {
        Add { id: u16, generation: u32, fpp: u64, format: SampleFormat },
        Remove(u16),
        SetChannels { id: u16, generation: u32, slots: usize },
    }

    fn sent(commands: &std_mpsc::Receiver<TxCommand>) -> Vec<Sent> {
        commands
            .try_iter()
            .map(|c| match c {
                TxCommand::Add(f) => {
                    Sent::Add { id: f.id, generation: f.generation, fpp: f.fpp, format: f.format }
                }
                TxCommand::Remove(id) => Sent::Remove(id),
                TxCommand::SetChannels { id, generation, channels } => {
                    Sent::SetChannels { id, generation, slots: channels.len() }
                }
                TxCommand::Shutdown => panic!("shutdown"),
            })
            .collect()
    }

    #[test]
    fn flows_beyond_what_we_advertise_are_refused() {
        let shared = test_shared(16);
        let (cmd_tx, commands) = std_mpsc::sync_channel(COMMAND_QUEUE);
        let mut fc = FlowControl::new(shared.clone(), cmd_tx);
        let invalid = dbcp::error::INVALID_PARAMETER;
        // fpp outside the advertised 2..=32, too many slots, no slots, an
        // unknown encoding, a channel we do not have.
        for req in [
            flow_request(40_001, 64, 24, &[1, 2]),
            flow_request(40_001, 256, 24, &[1]),
            flow_request(40_001, 1, 24, &[1]),
            flow_request(40_001, 0, 24, &[1]),
            flow_request(40_001, 16, 24, &[1, 2, 3, 4, 5, 6, 7, 8, 9]),
            flow_request(40_001, 16, 24, &[]),
            flow_request(40_001, 16, 20, &[1]),
            flow_request(40_001, 16, 24, &[17]),
        ] {
            assert_eq!(request(&mut fc, &req), (invalid, None), "{req:?}");
        }
        assert!(sent(&commands).is_empty());
        assert!(shared.state().tx_flows.is_empty());

        // The largest flow we advertise is accepted...
        let (code, handle) = request(&mut fc, &flow_request(40_001, 32, 32, &[1; 8]));
        assert_eq!(code, result::SUCCESS);
        let handle = handle.unwrap();
        // ...but cannot grow beyond 8 slots through an update.
        let update = dbcp::encode_update_flow(8, handle, &[1; 9]);
        assert_eq!(call(&mut fc, &update).0, invalid);
        let update = dbcp::encode_update_flow(9, handle, &[]);
        assert_eq!(call(&mut fc, &update).0, invalid);
        let update = dbcp::encode_update_flow(10, handle, &[2; 8]);
        assert_eq!(call(&mut fc, &update).0, result::SUCCESS);
    }

    #[test]
    fn a_re_request_with_other_packets_gets_a_new_flow() {
        let shared = test_shared(8);
        let (cmd_tx, commands) = std_mpsc::sync_channel(COMMAND_QUEUE);
        let mut fc = FlowControl::new(shared.clone(), cmd_tx);
        fn fpp_of(shared: &Shared) -> Vec<u16> {
            shared.state().tx_flows.iter().map(|f| f.fpp).collect()
        }

        let (code, first) = request(&mut fc, &flow_request(40_002, 16, 24, &[1, 2]));
        assert_eq!(code, result::SUCCESS);
        let first = first.unwrap();
        let [Sent::Add { id, generation: g1, fpp: 16, format: SampleFormat::S24 }] =
            sent(&commands)[..]
        else {
            panic!("no add");
        };

        // The same packets again, with other channels: the same flow and
        // handle, promised again.
        let (code, again) = request(&mut fc, &flow_request(40_002, 16, 24, &[3, 4, 5]));
        assert_eq!((code, again), (result::SUCCESS, Some(first)));
        let [Sent::SetChannels { id: same, generation: g2, slots: 3 }] = sent(&commands)[..] else {
            panic!("no channel update");
        };
        assert_eq!(same, id);
        assert_ne!(g2, g1);
        assert_eq!(fpp_of(&shared), vec![16]);

        // Another fpp: a new flow (the old one removed first), with a new
        // handle carrying the new fpp.
        let (code, second) = request(&mut fc, &flow_request(40_002, 8, 24, &[1, 2]));
        assert_eq!(code, result::SUCCESS);
        let second = second.unwrap();
        assert_ne!(second, first);
        let cmds = sent(&commands);
        assert_eq!(cmds[0], Sent::Remove(id));
        assert!(
            matches!(cmds[1..], [Sent::Add { fpp: 8, format: SampleFormat::S24, .. }]),
            "{cmds:?}"
        );
        assert_eq!(fpp_of(&shared), vec![8]);
        assert_eq!(fc.flows.len(), 1);

        // Another encoding: replaced again.
        let (code, third) = request(&mut fc, &flow_request(40_002, 8, 32, &[1, 2]));
        assert_eq!(code, result::SUCCESS);
        assert_ne!(third.unwrap(), second);
        let cmds = sent(&commands);
        assert!(
            matches!(
                cmds[..],
                [Sent::Remove(_), Sent::Add { fpp: 8, format: SampleFormat::S32, .. }]
            ),
            "{cmds:?}"
        );

        // A late stop for a replaced handle does not stop the new flow.
        let stop = dbcp::encode_stop_flow(11, first);
        assert_eq!(call(&mut fc, &stop).0, dbcp::error::FLOW_NOT_FOUND);
        let stop = dbcp::encode_stop_flow(12, second);
        assert_eq!(call(&mut fc, &stop).0, dbcp::error::FLOW_NOT_FOUND);
        assert_eq!(shared.state().tx_flows.len(), 1);
        assert!(sent(&commands).is_empty());
    }

    #[test]
    fn a_stale_expiry_does_not_remove_a_flow_promised_again() {
        let shared = test_shared(4);
        let (cmd_tx, commands) = std_mpsc::sync_channel(COMMAND_QUEUE);
        let mut fc = FlowControl::new(shared.clone(), cmd_tx);
        let (_, handle) = request(&mut fc, &flow_request(40_003, 16, 24, &[1]));
        let [Sent::Add { id, generation: g1, .. }] = sent(&commands)[..] else { panic!() };

        // The receiver re-requests while the transmit thread reports the
        // flow silent: the expiry is for the earlier promise.
        assert_eq!(request(&mut fc, &flow_request(40_003, 16, 24, &[1, 2])).1, handle);
        let [Sent::SetChannels { generation: g2, .. }] = sent(&commands)[..] else { panic!() };
        fc.expired(id, g1);
        assert_eq!(shared.state().tx_flows.len(), 1, "the flow survives");
        assert!(sent(&commands).is_empty());
        let update = dbcp::encode_update_flow(3, handle.unwrap(), &[2]);
        assert_eq!(call(&mut fc, &update).0, result::SUCCESS);
        let [Sent::SetChannels { generation: g3, .. }] = sent(&commands)[..] else { panic!() };
        fc.expired(id, g2);
        assert_eq!(shared.state().tx_flows.len(), 1, "the flow survives an update too");
        assert_eq!(shared.counters.tx_expired_flows.load(Ordering::Relaxed), 0);

        // The expiry of the latest promise removes it.
        fc.expired(id, g3);
        assert!(shared.state().tx_flows.is_empty());
        assert_eq!(sent(&commands), [Sent::Remove(id)]);
        assert_eq!(shared.counters.tx_expired_flows.load(Ordering::Relaxed), 1);
        let update = dbcp::encode_update_flow(4, handle.unwrap(), &[2]);
        assert_eq!(call(&mut fc, &update).0, dbcp::error::FLOW_NOT_FOUND);

        // An expiry for a flow id now used by another flow is stale too.
        let (_, other) = request(&mut fc, &flow_request(40_004, 16, 24, &[1]));
        let [Sent::Add { id: reused, .. }] = sent(&commands)[..] else { panic!() };
        assert_eq!(reused, id);
        fc.expired(id, g3);
        assert_eq!(shared.state().tx_flows.len(), 1);
        assert_ne!(other, handle);
    }

    #[test]
    fn flow_control_polls_for_expiries_and_waits_for_room() {
        let shared = test_shared(4);
        // A queue of one command, to see the next wait for room.
        let (cmd_tx, commands) = std_mpsc::sync_channel(1);
        let (_retire, retired) = std_mpsc::sync_channel(IN_FLIGHT_MAX);
        let expiries = Expiries::new();
        let mut fc = FlowControl::new(shared.clone(), cmd_tx);

        // Both flows are granted and listed at once; the second goes to the
        // thread once its queue has room.
        assert_eq!(request(&mut fc, &flow_request(40_010, 16, 24, &[1])).0, result::SUCCESS);
        assert_eq!(request(&mut fc, &flow_request(40_011, 16, 24, &[2])).0, result::SUCCESS);
        assert_eq!(shared.state().tx_flows.len(), 2);
        let [Sent::Add { id: 1, generation: g1, .. }] = sent(&commands)[..] else { panic!() };
        fc.poll(&retired, &expiries);
        let [Sent::Add { id: 2, generation: g2, .. }] = sent(&commands)[..] else { panic!() };

        // The thread found both flows silent; flow 2's receiver asked again
        // before the poll, which makes its expiry stale.
        expiries.report(1, g1);
        expiries.report(2, g2);
        assert_eq!(request(&mut fc, &flow_request(40_011, 16, 24, &[2, 3])).0, result::SUCCESS);
        let [Sent::SetChannels { id: 2, slots: 2, .. }] = sent(&commands)[..] else { panic!() };
        assert_eq!(shared.state().tx_flows.len(), 2, "nothing removed before the poll");
        fc.poll(&retired, &expiries);
        assert_eq!(sent(&commands), [Sent::Remove(1)]);
        let listed: Vec<u16> = shared.state().tx_flows.iter().map(|f| f.id).collect();
        assert_eq!(listed, [2]);
        assert_eq!(shared.counters.tx_expired_flows.load(Ordering::Relaxed), 1);
        // Each expiry is taken once.
        fc.poll(&retired, &expiries);
        assert!(sent(&commands).is_empty());
    }

    #[test]
    fn flow_control_hands_out_no_more_than_the_thread_can_hand_back() {
        let shared = test_shared(4);
        let (cmd_tx, commands) = std_mpsc::sync_channel(4 * IN_FLIGHT_MAX);
        let (retire, retired) = std_mpsc::sync_channel(IN_FLIGHT_MAX);
        let expiries = Expiries::new();
        let mut fc = FlowControl::new(shared.clone(), cmd_tx);
        let handle = request(&mut fc, &flow_request(40_012, 16, 24, &[1])).1.unwrap();
        let update = |fc: &mut FlowControl, seq| {
            call(fc, &dbcp::encode_update_flow(seq, handle, &[seq % 4 + 1])).0
        };

        // The flow and IN_FLIGHT_MAX - 1 channel lists go to the thread...
        for seq in 1..IN_FLIGHT_MAX as u16 {
            assert_eq!(update(&mut fc, seq), result::SUCCESS);
        }
        assert_eq!(sent(&commands).len(), IN_FLIGHT_MAX);
        // ...and the next update, answered all the same, waits until the
        // thread hands something back, and a stop behind it waits too.
        assert_eq!(update(&mut fc, 100), result::SUCCESS);
        assert_eq!(call(&mut fc, &dbcp::encode_stop_flow(101, handle)).0, result::SUCCESS);
        assert!(sent(&commands).is_empty());
        fc.poll(&retired, &expiries);
        assert!(sent(&commands).is_empty(), "nothing came back yet");
        retire.send(Retired::Channels(vec![Some(0)])).unwrap();
        fc.poll(&retired, &expiries);
        let cmds = sent(&commands);
        assert!(
            matches!(cmds[..], [Sent::SetChannels { id: 1, slots: 1, .. }, Sent::Remove(1)]),
            "{cmds:?}"
        );
        assert_eq!(fc.in_flight, IN_FLIGHT_MAX);
    }

    /// Counts the allocations and frees of the threads that ask for it while
    /// armed, as the driver's test of its real-time paths does
    /// (crates/ovsc-hal/tests/io_noalloc.rs).
    mod counting {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

        pub static ALLOCS: AtomicU64 = AtomicU64::new(0);
        pub static FREES: AtomicU64 = AtomicU64::new(0);
        static ARMED: AtomicBool = AtomicBool::new(false);

        thread_local! {
            static WATCHED: Cell<bool> = const { Cell::new(false) };
        }

        /// Counts what the calling thread allocates and frees while armed.
        pub fn watch_this_thread() {
            WATCHED.with(|w| w.set(true));
        }

        pub fn arm(on: bool) {
            ARMED.store(on, Ordering::SeqCst);
        }

        fn count(counter: &AtomicU64) {
            if ARMED.load(Ordering::Relaxed) && WATCHED.try_with(Cell::get).unwrap_or(false) {
                counter.fetch_add(1, Ordering::Relaxed);
            }
        }

        struct Counting;

        // SAFETY: every call goes straight to the system allocator.
        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                count(&ALLOCS);
                unsafe { System.alloc(layout) }
            }

            unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
                count(&ALLOCS);
                unsafe { System.alloc_zeroed(layout) }
            }

            unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
                count(&ALLOCS);
                count(&FREES);
                unsafe { System.realloc(ptr, layout, new_size) }
            }

            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                count(&FREES);
                unsafe { System.dealloc(ptr, layout) }
            }
        }

        #[global_allocator]
        static ALLOCATOR: Counting = Counting;
    }

    /// Takes `n` values the transmit thread handed back, as flow control
    /// does: without blocking, which would make the thread wake this one.
    fn take_back(retired: &std_mpsc::Receiver<Retired>, n: usize) -> Vec<Retired> {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut back = Vec::new();
        while back.len() < n {
            match retired.try_recv() {
                Ok(value) => back.push(value),
                Err(_) => {
                    assert!(Instant::now() < deadline, "{} of {n} values handed back", back.len());
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
        back
    }

    /// Flows come and go mid-show (a receiver subscribes, is re-patched,
    /// disappears): the transmit thread does all of it without allocating,
    /// freeing or closing anything, and reports silent flows without a
    /// channel that wakes a task.
    #[test]
    fn flow_changes_neither_allocate_nor_free_on_the_transmit_thread() {
        let shared = test_shared(4);
        let (commands, queue) = std_mpsc::sync_channel(COMMAND_QUEUE);
        let (retire, retired) = std_mpsc::sync_channel(IN_FLIGHT_MAX);
        let expiries = Arc::new(Expiries::new());
        let thread = {
            let (shared, expiries) = (shared.clone(), expiries.clone());
            std::thread::spawn(move || {
                counting::watch_this_thread();
                engine(&shared, queue, &retire, &expiries);
            })
        };
        let mut buf = [0u8; 2048];
        let (first_rx, first) = flow_to_new_receiver(1, 1, 16, vec![Some(0), Some(1)]);
        commands.send(TxCommand::Add(first)).unwrap();
        // Once it sends, the thread is past its setup, which allocates.
        first_rx.recv(&mut buf).expect("packets from flow 1");
        counting::arm(true);

        // A flow whose receiver has been silent for longer than the
        // keepalive timeout: the thread reports it at once.
        let (_silent_rx, mut silent) = flow_to_new_receiver(2, 2, 16, vec![Some(0)]);
        silent.last_heard_ns = 0;
        commands.send(TxCommand::Add(silent)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !expiries.take().any(|e| e == (2, 2)) {
            assert!(Instant::now() < deadline, "flow 2 not reported silent");
            std::thread::sleep(Duration::from_millis(5));
        }
        // New channels for flow 1, and for a flow the thread does not have;
        // flow 1 replaced by a flow to another receiver; flow 2 removed.
        let update = |id, channels, generation| TxCommand::SetChannels { id, channels, generation };
        commands.send(update(1, vec![Some(2)], 3)).unwrap();
        commands.send(update(9, vec![Some(3)], 4)).unwrap();
        let (second_rx, second) = flow_to_new_receiver(1, 5, 16, vec![Some(1)]);
        commands.send(TxCommand::Add(second)).unwrap();
        commands.send(TxCommand::Remove(2)).unwrap();
        second_rx.recv(&mut buf).expect("packets from the new flow 1");
        // Everything it is done with comes back, in order.
        let back = take_back(&retired, 4);
        counting::arm(false);

        let [
            Retired::Channels(before_update),
            Retired::Channels(unknown_flow),
            Retired::Flow(replaced),
            Retired::Flow(removed),
        ] = &back[..]
        else {
            panic!("not handed back in order");
        };
        assert_eq!(before_update, &[Some(0), Some(1)]);
        assert_eq!(unknown_flow, &[Some(3)]);
        assert_eq!((replaced.id, replaced.generation), (1, 3));
        assert_eq!(replaced.channels, [Some(2)]);
        assert_eq!((removed.id, removed.generation, removed.expired), (2, 2, true));
        commands.send(TxCommand::Shutdown).unwrap();
        thread.join().unwrap();
        let allocs = counting::ALLOCS.load(Ordering::Relaxed);
        let frees = counting::FREES.load(Ordering::Relaxed);
        assert_eq!((allocs, frees), (0, 0), "allocations and frees on the transmit thread");
    }

    #[test]
    fn plans_catch_up_within_the_window_and_resync_beyond() {
        let guard = 24;
        let window = 1920; // 40 ms
        let backward = 4800;
        let p = |now, next, fpp| plan(now, next, guard, fpp, window, backward);
        // A new flow starts at the newest due packet, aligned to fpp.
        assert_eq!(
            p(100_000, None, 16),
            Plan { first: 99_968, count: 1, skipped: 0, restarted: true }
        );
        // On time: the one packet due.
        assert_eq!(p(100_000, Some(99_968), 16).count, 1);
        assert_eq!(p(100_000, Some(99_984), 16).count, 0);
        // Before the guard has passed: nothing.
        assert_eq!(p(10, None, 16).count, 0);
        // A 20 ms stall: every overdue packet, oldest first.
        let late = p(100_000, Some(99_968 - 960), 16);
        assert_eq!(late, Plan { first: 99_008, count: 61, skipped: 0, restarted: false });
        // A 50 ms stall: older than any receiver plays, so resync to the
        // newest due packet and skip the rest.
        let stalled = p(100_000, Some(100_000 - 2400), 16);
        assert_eq!(stalled, Plan { first: 99_968, count: 1, skipped: 148, restarted: true });
        // A clock step forward by seconds: the same.
        let stepped = p(10_000_000, Some(100_000), 32);
        assert!(stepped.restarted && stepped.count == 1 && stepped.first % 32 == 0);
        // A small step back: wait for the clock; a large one: restart.
        assert_eq!(p(100_000, Some(100_400), 16).count, 0);
        assert!(!p(100_000, Some(100_400), 16).restarted);
        assert_eq!(p(100_000, Some(200_000), 16).first, 99_968);
    }

    #[test]
    fn the_catch_up_window_covers_every_receiver_latency() {
        // 40 ms, whatever latency we run at ourselves.
        assert_eq!(catch_up_window(48, 48_000), 1920);
        assert_eq!(catch_up_window(192, 48_000), 1920);
        assert_eq!(catch_up_window(0, 96_000), 3840);
    }

    #[test]
    fn transmit_problems_are_reported_as_they_change() {
        let before = TxHealth::default();
        assert_eq!(before.problems_since(&before), None);
        // Underruns alone are normal while nothing plays.
        let idle = TxHealth { underruns: 5000, ..before };
        assert_eq!(idle.problems_since(&before), None);
        let bad = TxHealth {
            send_errors: 3,
            last_send_error: 111,
            late: 2,
            max_lateness_ns: 1_250_000,
            skipped: 148,
            resyncs: 1,
            underruns: 5000,
        };
        let msg = bad.problems_since(&idle).unwrap();
        for part in [
            "3 send errors (the last: ",
            "2 packets left more than 250 µs late (the latest so far 1.25 ms)",
            "1 flow resyncs",
            "148 overdue packets skipped",
        ] {
            assert!(msg.contains(part), "{part:?} in {msg:?}");
        }
        assert_eq!(bad.problems_since(&bad), None);
    }

    #[test]
    fn a_panic_in_the_transmit_thread_is_reported() {
        let counters = Counters::default();
        let (events, mut rx) = mpsc::unbounded_channel();
        supervise(&counters, &events, || {});
        assert!(rx.try_recv().is_err());
        assert!(!counters.tx_engine_failed.load(Ordering::Relaxed));
        supervise(&counters, &events, || panic!("transmit test panic"));
        assert!(matches!(rx.try_recv(), Ok(TxEvent::Failed)));
        assert!(counters.tx_engine_failed.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn flow_control_refuses_flows_once_the_transmit_thread_stopped() {
        let shared = test_shared(4);
        let (cmd_tx, _commands) = std_mpsc::sync_channel(COMMAND_QUEUE);
        let mut fc = FlowControl::new(shared.clone(), cmd_tx);
        assert_eq!(request(&mut fc, &flow_request(40_005, 16, 24, &[1])).0, result::SUCCESS);
        assert_eq!(shared.state().tx_flows.len(), 1);

        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = socket.local_addr().unwrap();
        let (events, events_rx) = mpsc::unbounded_channel();
        let (_retire, retired) = std_mpsc::sync_channel(IN_FLIGHT_MAX);
        let link = TxLink { events: events_rx, retired, expiries: Arc::new(Expiries::new()) };
        let server = tokio::spawn(fc.run(socket, link));
        events.send(TxEvent::Failed).unwrap();
        drop(events);
        // Let it handle the failure before asking for a flow.
        let deadline = Instant::now() + Duration::from_secs(2);
        while !shared.state().tx_flows.is_empty() {
            assert!(Instant::now() < deadline, "the failure was not handled");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let packet = flow_request(40_006, 16, 24, &[1]).encode(crate::client::next_seq());
        let resp = crate::client::transact(
            Ipv4Addr::LOCALHOST,
            addr,
            &packet,
            Duration::from_millis(500),
            3,
        )
        .await
        .expect("an answer, not a timeout");
        assert_eq!(Frame::parse(&resp).unwrap().header.result, result::FAILURE);
        assert!(shared.state().tx_flows.is_empty(), "no flow is listed");
        assert!(!server.is_finished(), "still answering");
        server.abort();
    }

    /// Runs the transmit thread with one flow to a local socket and returns
    /// the socket, the flow's source address and the command sender.
    fn start_engine(
        shared: &Arc<Shared>,
        fpp: u16,
        slots: usize,
    ) -> (StdUdpSocket, SocketAddr, std_mpsc::SyncSender<TxCommand>, std::thread::JoinHandle<()>)
    {
        let (receiver, flow) = flow_to_new_receiver(1, 1, fpp, (0..slots).map(Some).collect());
        let source = flow.socket.local_addr().unwrap();
        let (commands, _link, thread) = spawn_engine(shared.clone());
        commands.send(TxCommand::Add(flow)).unwrap();
        (receiver, source, commands, thread)
    }

    /// A unicast flow `id`, promised as `generation`, to a new local socket
    /// (returned with it), which waits up to 2 s for each packet.
    fn flow_to_new_receiver(
        id: u16,
        generation: u32,
        fpp: u16,
        channels: Vec<Option<usize>>,
    ) -> (StdUdpSocket, TxFlow) {
        let receiver = StdUdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        receiver.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let SocketAddr::V4(dst) = receiver.local_addr().unwrap() else { unreachable!() };
        let socket = open_flow_socket(Ipv4Addr::LOCALHOST, dst).unwrap();
        let flow = TxFlow::new(id, generation, socket, channels, fpp, SampleFormat::S24, true);
        (receiver, flow)
    }

    #[test]
    fn the_transmit_thread_paces_marked_packets_and_counts_lateness() {
        let shared = test_shared(4);
        let (receiver, source, commands, thread) = start_engine(&shared, 16, 2);
        let guard = ns_to_samples(shared.info.tx_guard_ns, 48_000);
        let mut buf = [0u8; 2048];
        let mut timestamps = Vec::new();
        while timestamps.len() < 200 {
            let (len, from) = receiver.recv_from(&mut buf).expect("packets keep coming");
            let arrived = ns_to_samples(shared.clock.now_ns().unwrap(), 48_000);
            assert_eq!(from, source);
            // 16 frames of 2 slots at 24 bit.
            assert_eq!(len, audio::HEADER_LEN + 16 * 2 * 3);
            let p = audio::AudioPacket::parse(&buf[..len], 2, SampleFormat::S24).unwrap();
            let ts = p.timestamp(48_000);
            assert_eq!(ts % 16, 0, "aligned to fpp");
            assert!(timestamps.last().is_none_or(|&last| ts > last), "in order");
            // Paced by the media clock, not sent ahead in a burst: no packet
            // leaves before its send time. This holds however late the test
            // thread reads it (a load-dependent gap between the first and
            // the last read would not); the extra sample covers rounding.
            assert!(arrived + 1 >= ts + guard, "packet {ts} arrived at {arrived}, guard {guard}");
            timestamps.push(ts);
        }
        // The thread counts a packet once send() returns, which can be
        // after the packet was read here: stop it before reading counters.
        commands.send(TxCommand::Shutdown).unwrap();
        thread.join().unwrap();
        let c = &shared.counters;
        assert!(!c.tx_engine_failed.load(Ordering::Relaxed));
        assert!(c.tx_packets.load(Ordering::Relaxed) >= 200);
        assert_eq!(c.tx_send_errors.load(Ordering::Relaxed), 0);
        // Without a stall of more than 40 ms (which a loaded host can
        // cause), the packets follow each other without a gap.
        if c.tx_resyncs.load(Ordering::Relaxed) == 0 {
            assert!(timestamps.windows(2).all(|w| w[1] == w[0] + 16), "{timestamps:?}");
            assert_eq!(c.tx_skipped_packets.load(Ordering::Relaxed), 0);
        }
        // No audio was written: every packet is an underrun.
        assert!(c.tx_underruns.load(Ordering::Relaxed) >= 200);
        // Lateness is measured; how much depends on the host, so only check
        // that it is plausible and consistent.
        let max = c.tx_max_lateness_ns.load(Ordering::Relaxed);
        let late = c.tx_late_packets.load(Ordering::Relaxed);
        assert!(max < 2_000_000_000, "{max} ns");
        assert!(late == 0 || max > LATE_ALLOWANCE_NS, "{late} late, max {max} ns");
        // Whether the thread got real-time scheduling is what this thread
        // gets too (same process, same privileges).
        let realtime =
            std::thread::spawn(|| crate::rt::raise_priority("test", crate::rt::RtClass::Transmit))
                .join()
                .unwrap();
        assert_eq!(c.tx_not_realtime.load(Ordering::Relaxed), !realtime);
    }

    #[test]
    fn flow_sockets_mark_audio_expedited_forwarding() {
        let dst = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9);
        let socket = open_flow_socket(Ipv4Addr::LOCALHOST, dst).unwrap();
        #[cfg(any(target_os = "linux", target_vendor = "apple"))]
        assert_eq!(socket2::SockRef::from(&socket).tos_v4().unwrap(), 0xb8);
        let _ = socket;
    }

    /// A connected socket learns from ICMP that nobody listens, and its
    /// next send fails: Linux reports it on the socket (macOS does not).
    #[cfg(target_os = "linux")]
    #[test]
    fn send_errors_are_counted() {
        let shared = test_shared(2);
        let (receiver, _, commands, thread) = start_engine(&shared, 16, 1);
        drop(receiver);
        let deadline = Instant::now() + Duration::from_secs(2);
        while shared.counters.tx_send_errors.load(Ordering::Relaxed) == 0 {
            assert!(Instant::now() < deadline, "no send error counted");
            std::thread::sleep(Duration::from_millis(10));
        }
        // The error code is stored just after the count: stop the thread
        // before reading it.
        commands.send(TxCommand::Shutdown).unwrap();
        thread.join().unwrap();
        let code = shared.counters.tx_last_send_error.load(Ordering::Relaxed);
        assert_eq!(code, libc::ECONNREFUSED);
    }
}
