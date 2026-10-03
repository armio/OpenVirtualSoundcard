//! Simulated-time tests of the PTP servo against a drifting master, with
//! the kind of jitter software timestamps suffer from.
//!
//! For tuning, the ignored `sweep` test prints worst cases over hundreds of
//! seeds and `trace` prints every update of one run (`SERVO_TRACE=1` does
//! that for any test); `SERVO_<FIELD>` environment variables override the
//! servo configuration (see `config_from_env`).

use ovsc_clock::ClockSnapshot;
use ovsc_clock::ptp::servo::{Servo, ServoConfig, ServoEvent, ServoState};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

const US: u64 = 1_000;
const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;

/// Local time at which the simulation starts.
const START: u64 = 3 * SEC;
/// Master time at `START`.
const MEDIA_START: u64 = 1_700_000_000_000_000_000;

/// How receive/send timestamps are delayed relative to the wire.
#[derive(Clone, Copy, Debug)]
enum JitterKind {
    /// Exponentially distributed (mean 50 µs), capped at 200 µs.
    Exponential,
    /// Uniform between 0 and 200 µs.
    Uniform,
}

struct Jitter {
    rng: StdRng,
    kind: JitterKind,
    /// Probability of an extra 2 ms delay (a scheduling hiccup).
    outlier_prob: f64,
}

impl Jitter {
    fn sample(&mut self) -> u64 {
        let u: f64 = self.rng.random();
        let base = match self.kind {
            JitterKind::Exponential => (-50_000.0 * (1.0 - u).ln()).min(200_000.0),
            JitterKind::Uniform => 200_000.0 * u,
        } as u64;
        if self.rng.random::<f64>() < self.outlier_prob { base + 2 * MS } else { base }
    }
}

struct Master {
    /// Media nanoseconds per local nanosecond.
    rate: f64,
}

impl Master {
    fn media_at(&self, local: u64) -> u64 {
        MEDIA_START + ((local as i128 - START as i128) as f64 * self.rate) as u64
    }
}

#[derive(Debug, Default)]
struct Outcome {
    /// Local time from start until the servo first reported Locked.
    lock_after: Option<u64>,
    /// Largest |published - master| over the last ten seconds, ns.
    final_max_error: i64,
    /// Frequency estimate error at the end, ppm.
    final_freq_error_ppm: f64,
    /// Largest discontinuity of the published clock after lock, ns.
    max_jump_after_lock: i64,
    /// Whether the published clock ever went backwards after lock.
    went_backwards_after_lock: bool,
    steps_after_lock: u32,
    /// Frequency estimate error when the servo declared lock, ppm.
    freq_error_at_lock_ppm: f64,
    /// Largest |published - master| at any time after lock, ns.
    max_error_after_lock: i64,
}

/// Runs the servo for `duration` of local time and checks the published
/// clock every millisecond.
///
/// The master sends a Sync every 125–250 ms; we send a Delay_Req every
/// second ±25% (every 250 ms until four delay measurements exist, like
/// the follower). Every timestamp we take is late by some jitter, and so is
/// the master's receive timestamp of our Delay_Req.
fn simulate(seed: u64, drift_ppm: f64, kind: JitterKind, duration: u64) -> Outcome {
    let trace = std::env::var_os("SERVO_TRACE").is_some();
    let master = Master { rate: 1.0 + drift_ppm * 1e-6 };
    let mut rng = StdRng::seed_from_u64(seed);
    let mut jitter = Jitter { rng: StdRng::seed_from_u64(seed ^ 0x5eed), kind, outlier_prob: 0.03 };
    let path_delay = 60 * US;
    let mut servo = Servo::new(config_from_env());

    let mut out = Outcome::default();
    let end = START + duration;
    let mut sync_sent = START;
    let mut next_delay_req = START + 300 * MS;
    let mut published: Option<ClockSnapshot> = None;
    let mut probe = START;
    let mut last_probe_media: Option<u64> = None;
    let mut locked = false;

    while sync_sent < end {
        // A Sync leaves the master; we timestamp it late by some jitter and
        // process it a little later still.
        let t1 = master.media_at(sync_sent);
        let t2 = sync_sent + path_delay + jitter.sample();
        let now = t2 + 20 * US;

        // Watch the published clock up to `now`.
        while probe <= now {
            if let Some(snap) = published {
                let media = snap.media_ns_at(probe);
                let error = media as i64 - master.media_at(probe) as i64;
                if probe + 10 * SEC >= end {
                    out.final_max_error = out.final_max_error.max(error.abs());
                }
                if locked {
                    out.max_error_after_lock = out.max_error_after_lock.max(error.abs());
                    if let Some(prev) = last_probe_media {
                        out.went_backwards_after_lock |= media < prev;
                        let jump = media as i64 - prev as i64 - MS as i64;
                        out.max_jump_after_lock = out.max_jump_after_lock.max(jump.abs());
                    }
                }
                last_probe_media = Some(media);
            }
            probe += MS;
        }

        let event = servo.sync(t1, t2, now);
        if let (Some(old), Some(new)) = (published, servo.snapshot()) {
            if locked {
                let jump = new.media_ns_at(now) as i64 - old.media_ns_at(now) as i64;
                out.max_jump_after_lock = out.max_jump_after_lock.max(jump.abs());
                if matches!(event, ServoEvent::Stepped { .. }) {
                    out.steps_after_lock += 1;
                }
            }
        }
        published = servo.snapshot();
        if servo.state() == ServoState::Locked && !locked {
            locked = true;
            out.lock_after = Some(now - START);
            out.freq_error_at_lock_ppm = (servo.frequency() - master.rate) * 1e6;
        }
        if let (true, Some(s)) = (trace, published) {
            let err = s.media_ns_at(now) as i64 - master.media_at(now) as i64;
            println!(
                "{:8.3}s err {:8.1}us e {:8.1}us delay {:6.1}us freq err {:8.3}ppm {:?} {:?}",
                (now - START) as f64 / 1e9,
                err as f64 / 1e3,
                servo.offset_ns() as f64 / 1e3,
                servo.path_delay_ns().unwrap_or(0) as f64 / 1e3,
                (servo.frequency() - master.rate) * 1e6,
                servo.state(),
                event
            );
        }

        // Delay measurements until the next Sync, paired with this one.
        let next_sync = sync_sent + rng.random_range(125 * MS..=250 * MS);
        while next_delay_req < next_sync {
            let t3 = next_delay_req;
            // The request leaves after we timestamped it, and the master
            // timestamps its arrival late as well.
            let arrival = t3 + jitter.sample() / 4 + path_delay;
            let t4 = master.media_at(arrival + jitter.sample());
            servo.delay_measurement(t1, t2, t3, t4);
            let interval = if servo.wants_fast_delay_requests() { 250 * MS } else { SEC };
            next_delay_req += (rng.random_range(0.75..1.25) * interval as f64) as u64;
        }
        sync_sent = next_sync;
    }
    out.final_freq_error_ppm = (servo.frequency() - master.rate) * 1e6;
    out
}

/// The default servo configuration, with overrides from the environment
/// for tuning experiments (`SERVO_<FIELD>=value`, durations in ms).
fn config_from_env() -> ServoConfig {
    let mut c = ServoConfig::default();
    let env =
        |k: &str| std::env::var(format!("SERVO_{k}")).ok().and_then(|v| v.parse::<f64>().ok());
    if let Some(v) = env("LOCKED_WINDOW") {
        c.lucky_window_ns = (v * 1e6) as u64;
    }
    if let Some(v) = env("LOCKING_WINDOW") {
        c.locking_lucky_window_ns = (v * 1e6) as u64;
    }
    if let Some(v) = env("ACQUIRE_SPAN") {
        c.acquire_min_span_ns = (v * 1e6) as u64;
    }
    if let Some(v) = env("DELAY_WINDOW") {
        c.delay_window = v as usize;
    }
    if let Some(v) = env("FAST_DELAY") {
        c.fast_delay_samples = v as usize;
    }
    if let Some(v) = env("HANDOVER") {
        c.fll_handover_span_ns = (v * 1e6) as u64;
    }
    if let Some(v) = env("LOCK_SAMPLES") {
        c.lock_samples = v as u32;
    }
    if let Some(v) = env("LOCKING_KP") {
        c.locking_kp = v;
    }
    if let Some(v) = env("LOCKED_WN") {
        c.locked_gains = ovsc_clock::ptp::servo::PiGains::from_loop(v, 1.0);
    }
    c
}

fn check(seed: u64, drift_ppm: f64, kind: JitterKind) {
    let o = simulate(seed, drift_ppm, kind, 120 * SEC);
    let ctx = format!("seed {seed}, drift {drift_ppm} ppm, {kind:?} jitter: {o:?}");
    let lock_after = o.lock_after.unwrap_or_else(|| panic!("never locked; {ctx}"));
    assert!(lock_after <= 10 * SEC, "locked too late; {ctx}");
    assert!(o.final_max_error < 50 * US as i64, "final error too large; {ctx}");
    assert!(o.final_freq_error_ppm.abs() < 2.0, "frequency estimate off; {ctx}");
    assert!(!o.went_backwards_after_lock, "clock went backwards; {ctx}");
    assert!(o.max_jump_after_lock < 2 * MS as i64, "clock jumped; {ctx}");
    assert_eq!(o.steps_after_lock, 0, "unexpected step; {ctx}");
    eprintln!("{ctx}");
}

#[test]
fn locks_to_fast_master_with_exponential_jitter() {
    for seed in 0..6 {
        check(seed, 40.0, JitterKind::Exponential);
    }
}

#[test]
fn locks_to_fast_master_with_uniform_jitter() {
    for seed in 100..106 {
        check(seed, 40.0, JitterKind::Uniform);
    }
}

#[test]
fn locks_to_slow_master() {
    for seed in 200..203 {
        check(seed, -75.0, JitterKind::Exponential);
    }
}

/// Worst cases over many seeds; for tuning:
/// `cargo test -p ovsc-clock --release --test servo_sim sweep -- --ignored --nocapture`.
#[test]
#[ignore]
fn sweep() {
    for (kind, drift) in [
        (JitterKind::Exponential, 40.0),
        (JitterKind::Uniform, 40.0),
        (JitterKind::Exponential, -100.0),
        (JitterKind::Uniform, 100.0),
    ] {
        let runs: Vec<Outcome> =
            (0..200).map(|seed| simulate(seed, drift, kind, 120 * SEC)).collect();
        let mut locks: Vec<(u64, usize)> = runs
            .iter()
            .enumerate()
            .map(|(seed, o)| (o.lock_after.unwrap_or(u64::MAX), seed))
            .collect();
        locks.sort();
        println!(
            "  lock time p50 {:.2} s, p90 {:.2} s, slowest seeds {:?}",
            locks[100].0 as f64 / 1e9,
            locks[180].0 as f64 / 1e9,
            &locks[195..]
        );
        let mut errs: Vec<(i64, usize)> =
            runs.iter().enumerate().map(|(seed, o)| (o.final_max_error, seed)).collect();
        errs.sort();
        println!(
            "  final error p50 {} ns, p90 {} ns, worst seeds {:?}",
            errs[100].0,
            errs[180].0,
            &errs[197..]
        );
        let lock = runs.iter().map(|o| o.lock_after.unwrap_or(u64::MAX)).max().unwrap();
        let err = runs.iter().map(|o| o.final_max_error).max().unwrap();
        let freq = runs.iter().map(|o| o.final_freq_error_ppm.abs()).fold(0.0, f64::max);
        let (jump, jump_seed) =
            runs.iter().enumerate().map(|(s, o)| (o.max_jump_after_lock, s)).max().unwrap();
        let (after, after_seed) =
            runs.iter().enumerate().map(|(s, o)| (o.max_error_after_lock, s)).max().unwrap();
        let at_lock = runs.iter().map(|o| o.freq_error_at_lock_ppm.abs()).fold(0.0, f64::max);
        println!(
            "  largest jump {jump} ns (seed {jump_seed}); worst error after lock {:.1} us \
             (seed {after_seed}); worst freq error at lock {at_lock:.1} ppm",
            after as f64 / 1e3
        );
        let back = runs.iter().filter(|o| o.went_backwards_after_lock).count();
        let steps: u32 = runs.iter().map(|o| o.steps_after_lock).sum();
        println!(
            "{kind:?} {drift:+} ppm: worst lock {:.2} s, error {:.1} us, freq {:.2} ppm, \
             jump {jump} ns, backwards {back}, steps {steps}",
            lock as f64 / 1e9,
            err as f64 / 1e3,
            freq,
        );
    }
}

/// Prints one run; for tuning: `SEED=3 UNIFORM=1 DRIFT=40 SERVO_TRACE=1
/// cargo test -p ovsc-clock --test servo_sim trace -- --ignored --nocapture`.
#[test]
#[ignore]
fn trace() {
    let env = |k: &str| std::env::var(k).ok();
    let seed = env("SEED").and_then(|s| s.parse().ok()).unwrap_or(0);
    let drift = env("DRIFT").and_then(|s| s.parse().ok()).unwrap_or(40.0);
    let kind = if env("UNIFORM").is_some() { JitterKind::Uniform } else { JitterKind::Exponential };
    println!("{:?}", simulate(seed, drift, kind, 120 * SEC));
}
