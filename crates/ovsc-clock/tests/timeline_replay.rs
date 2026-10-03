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

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ovsc_clock::ptp::servo::{Servo, ServoConfig, ServoState};
use ovsc_clock::{
    ClockMirror, ClockSnapshot, ClockState, ClockStatus, ClockWriter, MasterInfo, MediaClock,
};
use ovsc_shm::clock::{ClockBlock, ClockRead, ClockRecord, READ_TRIES};
use ovsc_shm::timeline::{ClockInput, DeviceTimeline, Regime, TimelineParams, Zts};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

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
const DRIFTS_PPM: [f64; 5] = [-100.0, -30.0, 0.0, 45.0, 100.0];

/// The event scenario: the master steps at 60 s, the daemon dies at 85 s
/// and a new one starts at 91 s.
const STEP_AT: u64 = START + 60 * SEC;
const DIES_AT: u64 = START + 85 * SEC;
const RESTARTS_AT: u64 = START + 91 * SEC;

const MASTER: MasterInfo = MasterInfo {
    uuid: [0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0c],
    port_id: 1,
    addr: std::net::Ipv4Addr::new(192, 168, 1, 10),
};

/// Timestamping delay: exponential (mean 50 us) or uniform, capped at
/// 200 us, plus a 2 ms scheduling hiccup 3 % of the time.
fn jitter(rng: &mut StdRng, exponential: bool) -> u64 {
    let u: f64 = rng.random();
    let base =
        if exponential { (-50_000.0 * (1.0 - u).ln()).min(200_000.0) } else { 200_000.0 * u };
    let base = base as u64;
    if rng.random::<f64>() < 0.03 { base + 2 * MS } else { base }
}

/// The daemon's shared-memory clock mirror (design section 7.2): copies
/// every change into a clock block and counts discontinuities in step_gen.
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
        let (mut step_gen, was_valid, old_state, old_gm) = match *prev {
            Some(p) => (p.step_gen, p.valid, p.state, p.grandmaster),
            None => (0, false, ClockState::Unlocked, 0),
        };
        let jumped = prev.is_some_and(|p| {
            p.valid && valid && {
                let expected = p.snapshot.media_ns_at(snapshot.local_ref_ns);
                (snapshot.media_ref_ns as i128 - expected as i128).abs() > US as i128
            }
        });
        let into_lock = matches!(status.state, ClockState::Locked | ClockState::FreeRunning)
            && status.state != old_state;
        if jumped || (valid && !was_valid) || grandmaster != old_gm || into_lock {
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
/// clock block.
struct Daemon {
    servo: Servo,
    writer: ClockWriter,
    _clock: MediaClock,
    mirror: Arc<Mirror>,
    generation: u64,
    started: u64,
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
        }
    }

    /// A Sync arrived: run the servo and publish, as the PTP follower does.
    fn sync(&mut self, t1: u64, t2: u64, now: u64) {
        self.servo.sync(t1, t2, now);
        if let Some(s) = self.servo.snapshot() {
            self.writer.publish(s);
        }
        let state = match self.servo.state() {
            ServoState::Locked => ClockState::Locked,
            _ => ClockState::Locking,
        };
        self.writer.set_status(ClockStatus {
            state,
            master: Some(MASTER),
            offset_ns: self.servo.offset_ns(),
            mean_path_delay_ns: self.servo.path_delay_ns().unwrap_or(0),
            freq_offset_ppb: self.servo.freq_offset_ppb(),
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
}

fn run(seed: u64, drift_ppm: f64, exponential: bool, events: bool) -> RunStats {
    let rate_m = 1.0 + drift_ppm * 1e-6;
    let media_at = |l: u64| {
        let step = if events && l > STEP_AT { 10 * MS } else { 0 };
        MEDIA_START + ((l as i128 - START as i128) as f64 * rate_m) as u64 + step
    };
    let mut rng = StdRng::seed_from_u64(seed);
    let mut j = StdRng::seed_from_u64(seed ^ 0x5eed);
    let mut daemon = Daemon::new(1, START);
    let mut restarted = false;
    let mut sync_sent = START;
    let mut next_dr = START + 300 * MS;
    let mut lock_t: Option<u64> = None;

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
    let mut max_e = 0.0f64;

    while sync_sent < END {
        let t1 = media_at(sync_sent);
        let t2 = sync_sent + 60 * US + jitter(&mut j, exponential);
        let now_s = t2 + 20 * US;
        let dead = events && now_s > DIES_AT && !restarted;
        // IO cycles up to the Sync's processing.
        while next_io < now_s {
            let now = next_io;
            let alive = !(events && now > DIES_AT && !restarted);
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
            tl.gate(now, input.as_ref());
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
            let in_events = events && now > START + 59 * SEC && now < START + 100 * SEC;
            if tl.status().regime == Regime::Following
                && lock_t.is_some_and(|l| now > l + 3 * SEC)
                && !in_events
            {
                if let Some(r) = record {
                    max_e = max_e.max(media_error(&tl, &r.snapshot, now).abs());
                }
            }
            next_io += IO_FRAMES * SEC / FS as u64 + (rng.random::<f64>() * 300_000.0) as u64;
        }
        if events && !restarted && now_s > RESTARTS_AT {
            // A new daemon process maps a new region; the driver attaches it.
            daemon = Daemon::new(2, now_s);
            last_good = None;
            restarted = true;
        }
        if !dead {
            daemon.sync(t1, t2, now_s);
            if daemon.servo.state() == ServoState::Locked && lock_t.is_none() {
                lock_t = Some(now_s);
            }
        }
        let next_sync = sync_sent + rng.random_range(125 * MS..=250 * MS);
        while next_dr < next_sync {
            let t3 = next_dr;
            let arrival = t3 + jitter(&mut j, exponential) / 4 + 60 * US;
            let t4 = media_at(arrival + jitter(&mut j, exponential));
            if !dead {
                daemon.servo.delay_measurement(t1, t2, t3, t4);
            }
            let iv = if daemon.servo.wants_fast_delay_requests() { 250 * MS } else { SEC };
            next_dr += (rng.random_range(0.75..1.25) * iv as f64) as u64;
        }
        sync_sent = next_sync;
    }

    // Knot analysis, as in the simulator.
    let lt = lock_t.unwrap_or(END) as f64;
    let mut stats = RunStats { max_e, knots: knots.len(), ..RunStats::default() };
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
        let in_events = events
            && ((h2 > (START + 59 * SEC) as f64 && h0 < (START + 66 * SEC) as f64)
                || (h2 > (START + 84 * SEC) as f64 && h0 < (START + 100 * SEC) as f64));
        if in_events {
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

/// Every combination of seed, drift and jitter kind, spread over the
/// available cores.
fn sweep(events: bool) -> Vec<(String, RunStats)> {
    let mut cases = Vec::new();
    for seed in 0..SEEDS {
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
                    let stats = run(seed, drift, exponential, events);
                    let name = format!("seed {seed}, {drift} ppm, exponential {exponential}");
                    results.lock().unwrap().push((name, stats));
                }
            });
        }
    });
    let results = results.into_inner().unwrap();
    assert_eq!(results.len(), 600);
    results
}

/// The worst case of each figure, with the run that produced it.
fn check(results: &[(String, RunStats)], max_absorbs: u64) {
    let worst = |f: fn(&RunStats) -> f64| {
        results
            .iter()
            .map(|(n, s)| (f(s), n.as_str()))
            .fold((0.0, ""), |a, b| if b.0 > a.0 { b } else { a })
    };
    let steady = worst(|s| s.steady_us);
    let transition = worst(|s| s.transition_us);
    let e = worst(|s| s.max_e);
    let rate = worst(|s| s.rate_err_ppm.abs());
    let absorbs = results.iter().map(|(_, s)| s.absorbs).max().unwrap_or(0);
    let bumps: u64 = results.iter().map(|(_, s)| s.seed_bumps).sum();
    let nonconsecutive: u64 = results.iter().map(|(_, s)| s.nonconsecutive).sum();
    println!(
        "steady knot 2nd difference {:.1} us ({}); transition {:.1} us ({}); |e| {:.3} frames ({}); \
         HAL rate error {:.3} ppm ({}); absorbs max {absorbs}; seed bumps {bumps}; \
         non-consecutive knots {nonconsecutive}",
        steady.0, steady.1, transition.0, transition.1, e.0, e.1, rate.0, rate.1
    );
    assert!(results.iter().all(|(_, s)| s.knots > 300));
    assert!(steady.0 <= 30.0, "steady knot second difference {:.1} us in {}", steady.0, steady.1);
    assert!(transition.0 <= 30.0, "transition {:.1} us in {}", transition.0, transition.1);
    assert!(e.0 <= 2.0, "|e| {:.3} frames in {}", e.0, e.1);
    assert!(rate.0 <= 1.0, "HAL rate {:.3} ppm off in {}", rate.0, rate.1);
    assert_eq!(bumps, 0);
    assert_eq!(nonconsecutive, 0);
    assert!(absorbs <= max_absorbs, "{absorbs} absorbs in one run");
}

#[test]
fn replay_through_lock() {
    check(&sweep(false), 1);
}

#[test]
fn replay_through_a_step_and_a_daemon_restart() {
    check(&sweep(true), 4);
}
