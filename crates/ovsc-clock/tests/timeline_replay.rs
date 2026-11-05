//! Replays the real PTP servo into the macOS driver's device timeline
//! (design sections 7.2 to 7.5).
//!
//! The daemon side is a [`Servo`] fed by a simulated master, publishing
//! through a [`MediaClock`]'s writer to a [`ClockMirror`] that fills a
//! [`ClockBlock`] the way the daemon's shared-memory mirror does. The driver
//! side reads that block with a bounded number of tries on every IO cycle
//! and drives a [`DeviceTimeline`], like GetZeroTimeStamp does.
//!
//! The setup is the synthesis simulator's: 60 seeds, masters at -100, -30,
//! 0, +45 and +100 ppm, exponential and uniform timestamp jitter with 3 % of
//! 2 ms outliers, IO starting before lock with 512-frame cycles and up to
//! 0.3 ms of scheduling jitter. 600 runs of 120 s each, once with a steady
//! lock and once with a 10 ms master step at 60 s and a daemon restart (dead
//! from 85 s, a fresh servo from 91 s).
//!
//! Further scenarios, over 20 seeds each, are the faults of a live show
//! that must not stop the audio (see [`Scenario`]): a leader change, Syncs
//! queued behind traffic, and a master lost soon after the servo re-locked.
//! A leader whose time really differs is stepped to, and must be absorbed
//! once.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ovsc_clock::ptp::servo::{Servo, ServoConfig, ServoState};
use ovsc_clock::{
    ClockMirror, ClockSnapshot, ClockState, ClockStatus, ClockWriter, MasterInfo, MediaClock,
};
use ovsc_shm::clock::{ClockBlock, ClockRead, ClockRecord, READ_TRIES};
use ovsc_shm::timeline::{ClockInput, DeviceTimeline, Regime, TimelineParams, Zts};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

const US: u64 = 1_000;
const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;
/// Local time at which the simulation starts.
const START: u64 = 3 * SEC;
/// Master time at `START`.
const MEDIA_START: u64 = 1_700_000_000_000_000_000;
const END: u64 = START + 120 * SEC;
const FS: u32 = 48_000;
/// ZeroTimeStampPeriod.
const P: u64 = 16384;
const IO_FRAMES: u64 = 512;

const SEEDS: u64 = 60;
/// Seeds of the fault scenarios.
const FAULT_SEEDS: u64 = 20;
const DRIFTS_PPM: [f64; 5] = [-100.0, -30.0, 0.0, 45.0, 100.0];

/// The event scenario: the master steps at 60 s, the daemon dies at 85 s
/// and a new one starts at 91 s.
const STEP_AT: u64 = START + 60 * SEC;
const DIES_AT: u64 = START + 85 * SEC;
const RESTARTS_AT: u64 = START + 91 * SEC;

/// The follower's master timeout (`PtpConfig::master_timeout`).
const MASTER_TIMEOUT: u64 = 5 * SEC;

const MASTER: MasterInfo = MasterInfo {
    uuid: [0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0c],
    port_id: 1,
    addr: std::net::Ipv4Addr::new(192, 168, 1, 10),
};

/// The device that takes over as leader.
const LEADER_B: MasterInfo = MasterInfo {
    uuid: [0x00, 0x1d, 0xc1, 0x0d, 0x0e, 0x0f],
    port_id: 1,
    addr: std::net::Ipv4Addr::new(192, 168, 1, 11),
};

/// What happens during a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scenario {
    /// A steady lock.
    Steady,
    /// The master steps by 10 ms at 60 s; the daemon dies at 85 s and a
    /// new one starts at 91 s.
    StepAndRestart,
    /// The leader reboots at 55 s. Another device, which was synchronised
    /// to it, takes over: 2 us and 0.05 ppm away from it. Its Syncs start
    /// at 60 s, when the silent leader also times out, and the follower
    /// switches then.
    LeaderChange,
    /// At 60 s a preferred leader whose time is 10 ms off takes over.
    DistantLeader,
    /// From 60 s, every Sync is queued 300 us longer for 3 s: a network
    /// without strict-priority QoS for PTP, or a loaded 100 Mbit hop.
    Congestion,
    /// Syncs are queued 400 us longer from 40 s to 50 s, so the servo
    /// unlocks and re-locks. Then the master is silent from 57 s to 77 s,
    /// so the clock holds over for 15 s (after the 5 s master timeout).
    HoldoverAfterRelock,
}

impl Scenario {
    /// Which master's Syncs reach the servo at `rel` (time since `START`):
    /// 0 for the first master, 1 for leader B, `None` for none.
    fn source(self, rel: u64) -> Option<usize> {
        match self {
            Scenario::LeaderChange if rel >= 60 * SEC => Some(1),
            Scenario::LeaderChange if rel >= 55 * SEC => None,
            Scenario::DistantLeader if rel >= 60 * SEC => Some(1),
            Scenario::HoldoverAfterRelock if (57 * SEC..77 * SEC).contains(&rel) => None,
            _ => Some(0),
        }
    }

    /// Extra queueing of the Sync sent at `rel`.
    fn queueing(self, rel: u64) -> u64 {
        match self {
            Scenario::Congestion if (60 * SEC..63 * SEC).contains(&rel) => 300 * US,
            Scenario::HoldoverAfterRelock if (40 * SEC..50 * SEC).contains(&rel) => 400 * US,
            _ => 0,
        }
    }

    /// Leader B's time minus the first master's, and its frequency offset
    /// from it, from 60 s on.
    fn leader_b(self) -> (i64, f64) {
        match self {
            Scenario::DistantLeader => (10 * MS as i64, 0.0),
            _ => (2 * US as i64, 0.05e-6),
        }
    }

    /// Where the clock and the timeline go through transitions, times since
    /// `START`. Outside them the run must look like a steady lock.
    fn transitions(self) -> &'static [(u64, u64)] {
        match self {
            Scenario::Steady => &[],
            Scenario::StepAndRestart => &[(59 * SEC, 66 * SEC), (84 * SEC, 100 * SEC)],
            Scenario::LeaderChange | Scenario::Congestion => &[(55 * SEC, 75 * SEC)],
            Scenario::DistantLeader => &[(59 * SEC, 70 * SEC)],
            Scenario::HoldoverAfterRelock => &[(40 * SEC, 95 * SEC)],
        }
    }
}

/// Timestamping delay: exponential (mean 50 us) or uniform, capped at
/// 200 us, plus a 2 ms scheduling hiccup 3 % of the time.
fn jitter(rng: &mut StdRng, exponential: bool) -> u64 {
    let u: f64 = rng.random();
    let base =
        if exponential { (-50_000.0 * (1.0 - u).ln()).min(200_000.0) } else { 200_000.0 * u };
    let base = base as u64;
    if rng.random::<f64>() < 0.03 { base + 2 * MS } else { base }
}

/// The daemon's shared-memory clock mirror (design section 7.2, and
/// `ovsc-hal-server/src/mirror.rs`): copies every change into a clock block
/// and counts discontinuities in step_gen: a jump of the mapping, the clock
/// becoming valid, and a change into FreeRunning. A change of master or of
/// lock state is none.
struct Mirror {
    block: ClockBlock,
    prev: Mutex<Option<ClockRecord>>,
}

impl ClockMirror for Mirror {
    fn publish(&self, snapshot: Option<ClockSnapshot>, status: &ClockStatus) {
        let mut prev = self.prev.lock().unwrap();
        let valid = snapshot.is_some();
        let snapshot =
            snapshot.unwrap_or(ClockSnapshot { local_ref_ns: 0, media_ref_ns: 0, rate: 1.0 });
        let grandmaster = status.master.map_or(0, |m| {
            let uuid = m.uuid.iter().fold(0u64, |a, &b| a << 8 | b as u64);
            uuid << 16 | m.port_id as u64
        });
        let (mut step_gen, was_valid, old_state) = match *prev {
            Some(p) => (p.step_gen, p.valid, p.state),
            None => (0, false, ClockState::Unlocked),
        };
        let jumped = prev.is_some_and(|p| {
            p.valid && valid && {
                let expected = p.snapshot.media_ns_at(snapshot.local_ref_ns);
                (snapshot.media_ref_ns as i128 - expected as i128).abs() > US as i128
            }
        });
        let free_running =
            status.state == ClockState::FreeRunning && old_state != ClockState::FreeRunning;
        if jumped || (valid && !was_valid) || free_running {
            step_gen += 1;
        }
        let record = ClockRecord {
            snapshot,
            valid,
            state: status.state,
            step_gen,
            grandmaster,
            publish_ns: snapshot.local_ref_ns,
        };
        self.block.publish(&record);
        *prev = Some(record);
    }
}

/// One daemon process: a servo, the media clock it drives, and the region's
/// clock block. It reacts to Syncs and to changes of master as the PTP
/// follower does (`ptp/follower.rs`).
struct Daemon {
    servo: Servo,
    writer: ClockWriter,
    _clock: MediaClock,
    mirror: Arc<Mirror>,
    generation: u64,
    started: u64,
    master: Option<MasterInfo>,
}

impl Daemon {
    fn new(generation: u64, now: u64) -> Self {
        let (clock, writer) = MediaClock::new();
        let mirror = Arc::new(Mirror { block: ClockBlock::new(), prev: Mutex::new(None) });
        clock.set_mirror(Some(mirror.clone())).unwrap();
        Daemon {
            servo: Servo::new(ServoConfig::default()),
            writer,
            _clock: clock,
            mirror,
            generation,
            started: now,
            master: Some(MASTER),
        }
    }

    /// A Sync arrived: run the servo and publish.
    fn sync(&mut self, t1: u64, t2: u64, now: u64) {
        self.servo.sync(t1, t2, now);
        if let Some(s) = self.servo.snapshot() {
            self.writer.publish(s);
        }
        self.set_status();
    }

    /// The selector moved to another master (`Core::on_master_change`).
    fn change_master(&mut self, now: u64, master: MasterInfo) {
        if let Some(s) = self.servo.change_master(now) {
            self.writer.publish(s);
        }
        self.master = Some(master);
        self.set_status();
    }

    /// The master timed out with no other to take over.
    fn lose_master(&mut self, now: u64) {
        if let Some(s) = self.servo.enter_holdover(now) {
            self.writer.publish(s);
        }
        self.master = None;
        self.set_status();
    }

    fn set_status(&self) {
        let state = match (self.master, self.servo.state()) {
            (None, _) => ClockState::Unlocked,
            (Some(_), ServoState::Locked) => ClockState::Locked,
            (Some(_), _) => ClockState::Locking,
        };
        self.writer.set_status(ClockStatus {
            state,
            master: self.master,
            offset_ns: self.servo.offset_ns(),
            mean_path_delay_ns: self.servo.path_delay_ns().unwrap_or(0),
            freq_offset_ppb: self.servo.freq_offset_ppb(),
            ..ClockStatus::default()
        });
    }

    /// The last 10 Hz heartbeat at or before `now`.
    fn heartbeat(&self, now: u64) -> u64 {
        now - (now - self.started) % (100 * MS)
    }
}

/// How far the daemon's clock is ahead of the device, in frames.
fn media_error(tl: &DeviceTimeline, s: &ClockSnapshot, now: u64) -> f64 {
    let p = s.media_ns_at(now) as u128 * FS as u128;
    let (mw, mf) = ((p / 1_000_000_000) as i128, (p % 1_000_000_000) as f64 / 1e9);
    let (tw, tf) = tl.device_time_at(now);
    (mw - tl.media_offset() as i128 - tw as i128) as f64 + (mf - tf)
}

/// What one run showed.
#[derive(Clone, Copy, Debug, Default)]
struct RunStats {
    /// Largest knot second difference more than 3 s after lock, us.
    steady_us: f64,
    /// Largest knot second difference around the lock and the events, us.
    transition_us: f64,
    /// Largest |e| while following, from 3 s after lock, outside the
    /// events, frames.
    max_e: f64,
    /// Largest |e| while following during the events, frames.
    event_e: f64,
    absorbs: u64,
    seed_bumps: u64,
    /// Knot intervals more than 1 % away from a period.
    nonconsecutive: u64,
    /// The HAL-visible rate over the last 60 s against the master's. With
    /// the events, the fresh servo is still converging then, so instead the
    /// rate over the last 20 s against that of the daemon clock it follows.
    /// ppm.
    rate_err_ppm: f64,
    knots: usize,
    /// Time the gate was shut after it first opened, and its longest
    /// stretch, ns.
    gate_shut_ns: u64,
    longest_shut_ns: u64,
    /// Largest |daemon clock - network| during the events, us.
    event_net_err_us: f64,
    /// Times the servo fell from Locked to Locking without a step.
    unlocks: u64,
}

fn run(seed: u64, drift_ppm: f64, exponential: bool, sc: Scenario) -> RunStats {
    let events = sc != Scenario::Steady;
    let rate_m = 1.0 + drift_ppm * 1e-6;
    let (b_offset, b_ppm) = sc.leader_b();
    // Network time of master `src` at local time `l`.
    let media_at = |src: usize, l: u64| {
        let step = if sc == Scenario::StepAndRestart && l > STEP_AT { 10 * MS } else { 0 };
        let a = MEDIA_START as i128 + ((l as i128 - START as i128) as f64 * rate_m) as i128;
        let b = if src == 1 {
            b_offset as i128 + ((l as i128 - (START + 60 * SEC) as i128) as f64 * b_ppm) as i128
        } else {
            0
        };
        (a + b) as u64 + step
    };
    // Which master's time the network runs on at `l`.
    let network_at = |l: u64| {
        let leader_b =
            matches!(sc, Scenario::LeaderChange | Scenario::DistantLeader) && l >= START + 60 * SEC;
        media_at(usize::from(leader_b), l)
    };
    // From the start of the first transition to the end of the last.
    let in_transition = |l: u64| {
        let rel = l.saturating_sub(START);
        let t = sc.transitions();
        t.first().is_some_and(|f| rel > f.0) && t.last().is_some_and(|l| rel < l.1)
    };
    let mut rng = StdRng::seed_from_u64(seed);
    let mut j = StdRng::seed_from_u64(seed ^ 0x5eed);
    let mut daemon = Daemon::new(1, START);
    let mut restarted = false;
    let mut sync_sent = START;
    let mut next_dr = START + 300 * MS;
    let mut lock_t: Option<u64> = None;
    // The follower's view of the master: when it last sent a Sync, and
    // which one is selected.
    let mut last_sync_from_current = START;
    let mut current: Option<usize> = Some(0);

    // The driver: Initialize, then IO starts before the servo locks.
    let io_start = START + 500 * MS;
    let mut tl = DeviceTimeline::new(TimelineParams::DEFAULT);
    tl.reset(FS, START);
    tl.start(io_start, None);
    let mut next_io = io_start;
    let mut last_good: Option<ClockRecord> = None;
    let mut knots: Vec<Zts> = Vec::new();
    // The daemon clock's media position at each knot, frames from
    // MEDIA_START.
    let mut knot_media: Vec<Option<f64>> = Vec::new();
    let mut stats = RunStats::default();
    let mut first_open: Option<u64> = None;
    let mut shut_since: Option<u64> = None;

    while sync_sent < END {
        let rel = sync_sent - START;
        let source = sc.source(rel);
        let t1 = media_at(source.unwrap_or(0), sync_sent);
        let t2 = sync_sent + 60 * US + jitter(&mut j, exponential) + sc.queueing(rel);
        let now_s = t2 + 20 * US;
        let dead = sc == Scenario::StepAndRestart && now_s > DIES_AT && !restarted;
        // IO cycles up to the Sync's processing.
        while next_io < now_s {
            let now = next_io;
            let alive = !(sc == Scenario::StepAndRestart && now > DIES_AT && !restarted);
            let heartbeat = daemon.heartbeat(if alive { now } else { DIES_AT });
            let record = match daemon.mirror.block.read_bounded(READ_TRIES) {
                ClockRead::Record(r) => {
                    last_good = Some(r);
                    Some(r)
                }
                ClockRead::NeverWritten => None,
                ClockRead::Contended => last_good,
            };
            let input = record.map(|record| ClockInput {
                record,
                generation: daemon.generation,
                heartbeat_ns: heartbeat,
                daemon_rate: FS,
                engine_running: true,
            });
            tl.update(now, input.as_ref());
            let z = tl.zero_timestamp(now);
            let open = tl.gate(now, input.as_ref());
            assert!(z.host_ns <= now, "{z:?} after {now}");
            assert_eq!(z.sample_time % P, 0);
            if let Some(&l) = knots.last() {
                if z != l && z.seed == l.seed {
                    assert_eq!(z.sample_time, l.sample_time + P, "{l:?} -> {z:?}");
                    assert!(z.host_ns > l.host_ns, "{l:?} -> {z:?}");
                }
            }
            if knots.last() != Some(&z) {
                knots.push(z);
                knot_media.push(record.filter(|r| r.valid).map(|r| {
                    let m = r.snapshot.media_ns_at(z.host_ns) as i128 - MEDIA_START as i128;
                    m as f64 * FS as f64 / 1e9
                }));
            }
            let following = tl.status().regime == Regime::Following;
            if let Some(r) =
                record.filter(|_| following && lock_t.is_some_and(|l| now > l + 3 * SEC))
            {
                let e = media_error(&tl, &r.snapshot, now).abs();
                if in_transition(now) {
                    stats.event_e = stats.event_e.max(e);
                } else {
                    stats.max_e = stats.max_e.max(e);
                }
            }
            if let Some(r) = record.filter(|r| r.valid && in_transition(now)) {
                let err = r.snapshot.media_ns_at(now) as f64 - network_at(now) as f64;
                stats.event_net_err_us = stats.event_net_err_us.max(err.abs() / 1e3);
            }
            let cycle = IO_FRAMES * SEC / FS as u64 + (rng.random::<f64>() * 300_000.0) as u64;
            if open {
                first_open.get_or_insert(now);
                shut_since = None;
            } else if first_open.is_some() {
                stats.gate_shut_ns += cycle;
                let since = *shut_since.get_or_insert(now);
                stats.longest_shut_ns = stats.longest_shut_ns.max(now + cycle - since);
            }
            next_io += cycle;
        }
        if sc == Scenario::StepAndRestart && !restarted && now_s > RESTARTS_AT {
            // A new daemon process maps a new region; the driver attaches it.
            daemon = Daemon::new(2, now_s);
            last_good = None;
            restarted = true;
            last_sync_from_current = now_s;
        }
        // The follower's master selection (ptp/bmc.rs): a silent master is
        // dropped after the timeout, and the leader taking over is then
        // selected, or a preferred one at once.
        if !dead && current.is_some() && now_s > last_sync_from_current + MASTER_TIMEOUT {
            match sc {
                Scenario::LeaderChange => {
                    daemon.change_master(last_sync_from_current + MASTER_TIMEOUT, LEADER_B);
                    current = Some(1);
                }
                _ => {
                    daemon.lose_master(last_sync_from_current + MASTER_TIMEOUT);
                    current = None;
                }
            }
        }
        if sc == Scenario::DistantLeader && source == Some(1) && current == Some(0) {
            daemon.change_master(now_s, LEADER_B);
            current = Some(1);
        }
        if let (None, Some(s)) = (current, source) {
            // The first master heard again: selected at once, and the same
            // master the servo followed.
            current = Some(s);
            daemon.master = Some(MASTER);
        }
        let used = !dead && source.is_some() && source == current;
        if used {
            last_sync_from_current = t2;
            let was_locked = daemon.servo.state() == ServoState::Locked;
            daemon.sync(t1, t2, now_s);
            if was_locked && daemon.servo.state() == ServoState::Locking {
                stats.unlocks += 1;
            }
            if daemon.servo.state() == ServoState::Locked && lock_t.is_none() {
                lock_t = Some(now_s);
            }
        }
        let next_sync = sync_sent + rng.random_range(125 * MS..=250 * MS);
        while next_dr < next_sync {
            let t3 = next_dr;
            let arrival = t3 + jitter(&mut j, exponential) / 4 + 60 * US;
            let t4 = media_at(source.unwrap_or(0), arrival + jitter(&mut j, exponential));
            if used {
                daemon.servo.delay_measurement(t1, t2, t3, t4);
            }
            let iv = if daemon.servo.wants_fast_delay_requests() { 250 * MS } else { SEC };
            next_dr += (rng.random_range(0.75..1.25) * iv as f64) as u64;
        }
        sync_sent = next_sync;
    }

    // Knot analysis, as in the simulator.
    let lt = lock_t.unwrap_or(END) as f64;
    stats.knots = knots.len();
    let expected = P as f64 / FS as f64 * 1e9 / rate_m;
    for w in knots.windows(3) {
        if w[0].seed != w[2].seed {
            continue;
        }
        let d1 = w[1].host_ns as f64 - w[0].host_ns as f64;
        let d2 = w[2].host_ns as f64 - w[1].host_ns as f64;
        let dev = (d2 - d1).abs() / 1e3;
        if (d1 / expected - 1.0).abs() > 0.01 {
            stats.nonconsecutive += 1;
        }
        let (h0, h2) = (w[0].host_ns as f64, w[2].host_ns as f64);
        let touches = |&(a, b): &(u64, u64)| h2 > (START + a) as f64 && h0 < (START + b) as f64;
        if sc.transitions().iter().any(touches) {
            stats.transition_us = stats.transition_us.max(dev);
        } else if h0 > lt + 3e9 {
            stats.steady_us = stats.steady_us.max(dev);
        } else if h2 > lt - 1e9 {
            stats.transition_us = stats.transition_us.max(dev);
        }
    }
    let window = if events { 20 * SEC } else { 60 * SEC };
    let first = knots.iter().position(|k| k.host_ns > END - window).unwrap();
    let last = knots.len() - 1;
    let (k0, k1) = (knots[first], knots[last]);
    assert_eq!(k0.seed, k1.seed);
    let frames = (k1.sample_time - k0.sample_time) as f64;
    stats.rate_err_ppm = if events {
        let daemon_frames = knot_media[last].unwrap() - knot_media[first].unwrap();
        (frames / daemon_frames - 1.0) * 1e6
    } else {
        let span = (k1.host_ns - k0.host_ns) as f64 / 1e9;
        (frames / span / (FS as f64 * rate_m) - 1.0) * 1e6
    };
    let st = tl.status();
    stats.absorbs = st.absorbs;
    stats.seed_bumps = st.seed_bumps;
    assert_eq!(st.regime, Regime::Following);
    stats
}

/// Every combination of `seeds`, drift and jitter kind, spread over the
/// available cores.
fn sweep(sc: Scenario, seeds: u64) -> Vec<(String, RunStats)> {
    let mut cases = Vec::new();
    for seed in 0..seeds {
        for drift in DRIFTS_PPM {
            for exponential in [true, false] {
                cases.push((seed, drift, exponential));
            }
        }
    }
    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::with_capacity(cases.len()));
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&(seed, drift, exponential)) = cases.get(i) else { break };
                    let stats = run(seed, drift, exponential, sc);
                    let name = format!("seed {seed}, {drift} ppm, exponential {exponential}");
                    results.lock().unwrap().push((name, stats));
                }
            });
        }
    });
    let results = results.into_inner().unwrap();
    assert_eq!(results.len(), cases.len());
    results
}

/// The worst case of a figure, with the run that produced it.
fn worst(results: &[(String, RunStats)], f: fn(&RunStats) -> f64) -> (f64, &str) {
    results
        .iter()
        .map(|(n, s)| (f(s), n.as_str()))
        .fold((0.0, ""), |a, b| if b.0 > a.0 { b } else { a })
}

/// Checks what every scenario must keep: smooth, consecutive knots on one
/// timeline that tracks the daemon's clock. Returns the largest number of
/// absorbs in one run.
fn check(sc: Scenario, results: &[(String, RunStats)], max_absorbs: u64) -> u64 {
    let steady = worst(results, |s| s.steady_us);
    let transition = worst(results, |s| s.transition_us);
    let e = worst(results, |s| s.max_e);
    let event_e = worst(results, |s| s.event_e);
    let rate = worst(results, |s| s.rate_err_ppm.abs());
    let shut = worst(results, |s| s.gate_shut_ns as f64 / 1e9);
    let longest = worst(results, |s| s.longest_shut_ns as f64 / 1e9);
    let net = worst(results, |s| s.event_net_err_us);
    let absorbs = results.iter().map(|(_, s)| s.absorbs).max().unwrap_or(0);
    let bumps: u64 = results.iter().map(|(_, s)| s.seed_bumps).sum();
    let nonconsecutive: u64 = results.iter().map(|(_, s)| s.nonconsecutive).sum();
    println!(
        "{sc:?}: steady knot 2nd difference {:.1} us ({}); transition {:.1} us ({}); \
         |e| {:.3} frames ({}), during events {:.3} ({}); HAL rate error {:.3} ppm ({}); \
         gate shut {:.2} s ({}), longest {:.2} s ({}); network error during events {:.0} us \
         ({}); absorbs max {absorbs}; seed bumps {bumps}; non-consecutive knots {nonconsecutive}",
        steady.0,
        steady.1,
        transition.0,
        transition.1,
        e.0,
        e.1,
        event_e.0,
        event_e.1,
        rate.0,
        rate.1,
        shut.0,
        shut.1,
        longest.0,
        longest.1,
        net.0,
        net.1,
    );
    assert!(results.iter().all(|(_, s)| s.knots > 300));
    assert!(steady.0 <= 30.0, "steady knot second difference {:.1} us in {}", steady.0, steady.1);
    assert!(transition.0 <= 30.0, "transition {:.1} us in {}", transition.0, transition.1);
    assert!(e.0 <= 2.0, "|e| {:.3} frames in {}", e.0, e.1);
    assert!(rate.0 <= 1.0, "HAL rate {:.3} ppm off in {}", rate.0, rate.1);
    assert_eq!(bumps, 0);
    assert_eq!(nonconsecutive, 0);
    assert!(absorbs <= max_absorbs, "{absorbs} absorbs in one run");
    absorbs
}

/// Asserts that the gate never shut once it had opened, and that the
/// timeline stayed well inside the 250 us coherence window (12 frames) of
/// the daemon's clock throughout.
fn check_gate_never_shut(results: &[(String, RunStats)]) {
    for (name, s) in results {
        assert_eq!(s.gate_shut_ns, 0, "gate shut for {} ms in {name}", s.gate_shut_ns / MS);
        assert!(s.event_e <= 6.0, "|e| {:.3} frames during the events in {name}", s.event_e);
    }
}

/// Prints the distribution of gate closures of one scenario over all 600
/// runs; for tuning: `SC=distant cargo test -p ovsc-clock --release --test
/// timeline_replay fault_sweep -- --ignored --nocapture` (`SC` is one of
/// `leader`, `distant`, `congestion`, `holdover`).
#[test]
#[ignore]
fn fault_sweep() {
    let sc = match std::env::var("SC").as_deref() {
        Ok("distant") => Scenario::DistantLeader,
        Ok("congestion") => Scenario::Congestion,
        Ok("holdover") => Scenario::HoldoverAfterRelock,
        _ => Scenario::LeaderChange,
    };
    let mut results = sweep(sc, SEEDS);
    check(sc, &results, 2);
    results.sort_by_key(|(_, s)| s.longest_shut_ns);
    let n = results.len();
    for (label, q) in [("p50", n / 2), ("p90", n * 9 / 10), ("max", n - 1)] {
        let (name, s) = &results[q];
        println!("longest gate closure {label} {:.2} s ({name})", s.longest_shut_ns as f64 / 1e9);
    }
}

#[test]
fn replay_through_lock() {
    let results = sweep(Scenario::Steady, SEEDS);
    check(Scenario::Steady, &results, 1);
    check_gate_never_shut(&results);
}

#[test]
fn replay_through_a_step_and_a_daemon_restart() {
    check(Scenario::StepAndRestart, &sweep(Scenario::StepAndRestart, SEEDS), 4);
}

/// Finding clock#1/driver#1: a leader change used to reset the servo and
/// shut the gate for about 4 s. A new leader that was synchronised to the
/// old one is now followed without a gap, and nothing slips.
#[test]
fn replay_through_a_leader_change() {
    let results = sweep(Scenario::LeaderChange, FAULT_SEEDS);
    // The single absorb is the first lock.
    assert_eq!(check(Scenario::LeaderChange, &results, 1), 1);
    check_gate_never_shut(&results);
    for (name, s) in &results {
        // 5 s of coasting on the last rate, then the new leader: the
        // clock stays within tens of microseconds of the network.
        assert!(s.event_net_err_us < 150.0, "{:.0} us off in {name}", s.event_net_err_us);
    }
}

/// A leader whose time differs by 10 ms: the servo steps to it, the gate
/// shuts until it has locked, and the driver realigns once.
#[test]
fn replay_through_a_distant_leader() {
    let results = sweep(Scenario::DistantLeader, FAULT_SEEDS);
    check(Scenario::DistantLeader, &results, 2);
    let mut shut: Vec<u64> = results.iter().map(|(_, s)| s.longest_shut_ns).collect();
    shut.sort();
    for (name, s) in &results {
        assert_eq!(s.absorbs, 2, "{name}: {s:?}");
        assert_eq!(s.gate_shut_ns, s.longest_shut_ns, "{name}: shut more than once");
        // Two Syncs to confirm the step, then at least eight to lock again,
        // in place of a full acquisition (which took 3.6 s at p50 and up to
        // 7.6 s in the servo simulation).
        assert!(s.longest_shut_ns < 6 * SEC, "{name}: shut {} ms", s.longest_shut_ns / MS);
    }
    let median = shut[shut.len() / 2];
    assert!(median < 5 * SEC / 2, "median {} ms", median / MS);
}

/// Finding clock#2: 300 us of extra Sync queueing for 3 s unlocked the
/// servo, which then re-fitted its frequency to the disturbance, and the
/// driver slipped several frames at the re-lock. Now the clock slews gently
/// and the driver follows it throughout.
#[test]
fn replay_through_sync_congestion() {
    let results = sweep(Scenario::Congestion, FAULT_SEEDS);
    assert_eq!(check(Scenario::Congestion, &results, 1), 1);
    check_gate_never_shut(&results);
    // The episode does unlock the servo, and it rides it out.
    let unlocked = results.iter().filter(|(_, s)| s.unlocks > 0).count();
    assert!(unlocked * 2 > results.len(), "only {unlocked} runs unlocked");
}

/// Finding clock#3: a master lost soon after a re-lock held over at the raw
/// frequency estimate, and the driver at its own rate, until the two drifted
/// apart and the gate shut. Now the unlock keeps the frequency estimate
/// instead of re-fitting it to the disturbance, holdover runs at that
/// estimate, and the driver follows the daemon's holdover.
#[test]
fn replay_through_holdover_after_a_relock() {
    let results = sweep(Scenario::HoldoverAfterRelock, FAULT_SEEDS);
    assert_eq!(check(Scenario::HoldoverAfterRelock, &results, 1), 1);
    check_gate_never_shut(&results);
    assert!(results.iter().all(|(_, s)| s.unlocks > 0));
}
