//! Scenario tests of the device timeline (design section 7.4), ported from
//! the synthesis simulator: constant-rate daemons, steps, daemon restarts,
//! outages, rate jumps, resets and IO stalls.
//!
//! [`Sim`] drives a timeline the way the driver does, one GetZeroTimeStamp
//! per IO cycle, and checks on every call what the HAL relies on: sample
//! times are multiples of the period, a reported zero time stamp never
//! changes, the next one is exactly one period later with a later host
//! time, and host times are never in the future.

use ovsc_shm::clock::ClockRecord;
use ovsc_shm::time::{ClockSnapshot, ClockState};
use ovsc_shm::timeline::{ClockInput, DeviceTimeline, Regime, TimelineParams, TimelineStatus, Zts};

const US: u64 = 1_000;
const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;
/// Host time at which the simulations start.
const START: u64 = 3 * SEC;
/// Media time at `START`.
const MEDIA_START: u64 = 1_700_000_000_000_000_000;
/// ZeroTimeStampPeriod.
const P: u64 = 16384;

/// xorshift64*, so the scenarios are reproducible without dependencies.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, n].
    fn upto(&mut self, n: u64) -> u64 {
        self.next() % (n + 1)
    }

    /// Uniform in [-1, 1).
    fn signed(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 52) as f64 - 1.0
    }
}

/// What the driver reads from the daemon's region.
#[derive(Clone, Copy, Debug)]
struct Daemon {
    snapshot: ClockSnapshot,
    valid: bool,
    state: ClockState,
    step_gen: u64,
    generation: u64,
    /// A frozen heartbeat (the daemon died); `None` beats at every read.
    heartbeat: Option<u64>,
    rate: u32,
    running: bool,
}

impl Daemon {
    /// A locked daemon at `fs` whose media clock runs `ppm` fast, at
    /// `MEDIA_START` at host time `START`.
    fn locked(fs: u32, ppm: f64) -> Self {
        Daemon {
            snapshot: ClockSnapshot {
                local_ref_ns: START,
                media_ref_ns: MEDIA_START,
                rate: 1.0 + ppm * 1e-6,
            },
            valid: true,
            state: ClockState::Locked,
            step_gen: 0,
            generation: 1,
            heartbeat: None,
            rate: fs,
            running: true,
        }
    }

    fn input(&self, now: u64) -> ClockInput {
        ClockInput {
            record: ClockRecord {
                snapshot: self.snapshot,
                valid: self.valid,
                state: self.state,
                step_gen: self.step_gen,
                grandmaster: 0x0011_2233_4455_0001,
                publish_ns: self.snapshot.local_ref_ns,
            },
            generation: self.generation,
            heartbeat_ns: self.heartbeat.unwrap_or(now),
            daemon_rate: self.rate,
            engine_running: self.running,
        }
    }

    /// Changes the rate at `now`, keeping the media clock continuous.
    fn set_rate(&mut self, now: u64, ppm: f64) {
        self.snapshot = ClockSnapshot {
            local_ref_ns: now,
            media_ref_ns: self.snapshot.media_ns_at(now),
            rate: 1.0 + ppm * 1e-6,
        };
    }

    /// Steps the media clock by `ns` at `now`, as a discontinuity.
    fn step(&mut self, now: u64, ns: i64) {
        self.snapshot = ClockSnapshot {
            local_ref_ns: now,
            media_ref_ns: self.snapshot.media_ns_at(now).checked_add_signed(ns).unwrap(),
            rate: self.snapshot.rate,
        };
        self.step_gen += 1;
    }
}

/// How far the daemon's clock is ahead of the device, in frames:
/// `M(s, now) - off - T(now)`, from the timeline's public view.
fn media_error(tl: &DeviceTimeline, s: &ClockSnapshot, now: u64) -> f64 {
    let p = s.media_ns_at(now) as u128 * tl.sample_rate() as u128;
    let (mw, mf) = ((p / 1_000_000_000) as i128, (p % 1_000_000_000) as f64 / 1e9);
    let (tw, tf) = tl.device_time_at(now);
    (mw - tl.media_offset() as i128 - tw as i128) as f64 + (mf - tf)
}

/// Drives a timeline one IO cycle per call and checks every zero time
/// stamp.
struct Sim {
    tl: DeviceTimeline,
    now: u64,
    rng: Rng,
    /// The IO cycle; each cycle is up to `jitter_ns` longer.
    cycle_ns: u64,
    jitter_ns: u64,
    last: Option<Zts>,
    /// Every distinct zero time stamp, in order.
    knots: Vec<Zts>,
    record: bool,
}

impl Sim {
    /// Initialize at `fs`, IO started at `START`, 512-frame cycles with up
    /// to 0.3 ms of scheduling jitter.
    fn new(fs: u32, seed: u64) -> Self {
        Self::with_params(fs, seed, TimelineParams::DEFAULT)
    }

    fn with_params(fs: u32, seed: u64, params: TimelineParams) -> Self {
        let mut tl = DeviceTimeline::new(params);
        tl.reset(fs, START - 10 * MS);
        tl.start(START, None);
        Sim {
            tl,
            now: START,
            rng: Rng::new(seed),
            cycle_ns: 512 * SEC / fs as u64,
            jitter_ns: 300 * US,
            last: None,
            knots: Vec::new(),
            record: true,
        }
    }

    /// One GetZeroTimeStamp at the current time, then the time advances by
    /// one IO cycle.
    fn step(&mut self, d: Option<&Daemon>) -> Zts {
        let input = d.map(|d| d.input(self.now));
        self.tl.update(self.now, input.as_ref());
        let z = self.tl.zero_timestamp(self.now);
        self.check(z);
        self.now += self.cycle_ns + self.rng.upto(self.jitter_ns);
        z
    }

    fn run_until(&mut self, t: u64, d: Option<&Daemon>) {
        while self.now < t {
            self.step(d);
        }
    }

    fn check(&mut self, z: Zts) {
        assert_eq!(z.sample_time % P, 0, "{z:?}");
        assert!(z.host_ns <= self.now, "{z:?} is after now {}", self.now);
        if self.last == Some(z) {
            return;
        }
        if let Some(l) = self.last {
            if z.seed == l.seed {
                assert_eq!(z.sample_time, l.sample_time + P, "{l:?} -> {z:?}");
                assert!(z.host_ns > l.host_ns, "{l:?} -> {z:?}");
            } else {
                assert!(z.seed > l.seed, "{l:?} -> {z:?}");
            }
        }
        self.last = Some(z);
        if self.record {
            self.knots.push(z);
        }
    }

    fn status(&self) -> TimelineStatus {
        self.tl.status()
    }

    fn error(&self, d: &Daemon) -> f64 {
        media_error(&self.tl, &d.snapshot, self.now)
    }

    /// Distinct zero time stamps of `seed` reported at host times in
    /// [from, to).
    fn knots_between(&self, seed: u64, from: u64, to: u64) -> Vec<Zts> {
        self.knots
            .iter()
            .filter(|k| k.seed == seed && k.host_ns >= from && k.host_ns < to)
            .copied()
            .collect()
    }
}

/// Largest change between consecutive knot intervals with the same seed,
/// microseconds.
fn max_second_difference(knots: &[Zts]) -> f64 {
    knots
        .windows(3)
        .filter(|w| w[0].seed == w[2].seed)
        .map(|w| {
            let d1 = w[1].host_ns as f64 - w[0].host_ns as f64;
            let d2 = w[2].host_ns as f64 - w[1].host_ns as f64;
            (d2 - d1).abs() / 1e3
        })
        .fold(0.0, f64::max)
}

/// The rate the HAL sees over `knots` (one seed), frames per second.
fn hal_rate(knots: &[Zts]) -> f64 {
    let (first, last) = (knots.first().unwrap(), knots.last().unwrap());
    (last.sample_time - first.sample_time) as f64 / ((last.host_ns - first.host_ns) as f64 / 1e9)
}

fn ppm_error(measured: f64, expected: f64) -> f64 {
    (measured / expected - 1.0) * 1e6
}

#[test]
fn knots_are_consecutive_multiples_of_the_period() {
    for (seed, ppm) in [(1, 45.0), (2, -100.0), (3, 0.0)] {
        let mut sim = Sim::new(48_000, seed);
        let mut d = Daemon::locked(48_000, ppm);
        sim.run_until(START + 2 * SEC, None);
        assert_eq!(sim.status().regime, Regime::Synthetic);
        sim.run_until(START + 30 * SEC, Some(&d));
        assert_eq!(sim.status().regime, Regime::Following);
        d.step(sim.now, 2 * MS as i64);
        sim.run_until(START + 60 * SEC, Some(&d));
        // Every call was checked; there are no gaps and a single timeline.
        let (first, last) = (sim.knots[0], *sim.knots.last().unwrap());
        assert_eq!(first.sample_time, 0);
        assert!(sim.knots.len() > 170, "{}", sim.knots.len());
        assert_eq!((last.sample_time - first.sample_time) / P + 1, sim.knots.len() as u64);
        assert!(sim.knots.iter().all(|k| k.seed == 1));
        assert_eq!(sim.status().seed_bumps, 0);
    }
}

#[test]
fn large_and_irregular_io_cycles_keep_the_knots_consecutive() {
    // 4096-frame buffers with up to 60 ms of extra delay: some calls see
    // more than one period go by and catch up one period per call.
    let mut sim = Sim::new(48_000, 11);
    sim.cycle_ns = 4096 * SEC / 48_000;
    sim.jitter_ns = 60 * MS;
    let d = Daemon::locked(48_000, -30.0);
    sim.run_until(START + 60 * SEC, Some(&d));
    assert_eq!(sim.status().seed_bumps, 0);
    let late = sim.knots_between(1, START + 10 * SEC, START + 60 * SEC);
    assert!(ppm_error(hal_rate(&late), 48_000.0 * (1.0 - 30e-6)).abs() < 0.5);
}

#[test]
fn constant_rate_models_give_exact_periods() {
    for ppm in [300.0, -250.0, 0.0] {
        let mut sim = Sim::new(48_000, 2);
        let d = Daemon::locked(48_000, ppm);
        sim.run_until(START + SEC, None);
        sim.run_until(START + 31 * SEC, Some(&d));
        let st = sim.status();
        // Beyond 200 ppm from nominal, following the daemon starts a new
        // timeline instead of ramping.
        let bumps = if (-200.0..=200.0).contains(&ppm) { 0 } else { 1 };
        assert_eq!(st.seed_bumps, bumps, "{ppm}");
        assert_eq!(st.seed, 1 + bumps);
        let window = sim.knots_between(st.seed, START + 11 * SEC, START + 31 * SEC);
        assert!(window.len() >= 55, "{}", window.len());
        for w in window.windows(2) {
            assert_eq!(w[1].sample_time - w[0].sample_time, P);
        }
        let err = ppm_error(hal_rate(&window), 48_000.0 * (1.0 + ppm * 1e-6));
        assert!(err.abs() < 0.5, "{ppm} ppm: HAL rate off by {err} ppm");
        assert!(sim.error(&d).abs() < 0.01, "{}", sim.error(&d));
        assert!((st.device_rate_ppm_milli as f64 - ppm * 1e3).abs() <= 500.0, "{st:?}");
        assert!(sim.tl.gate(sim.now, Some(&d.input(sim.now))));
    }
}

#[test]
fn a_step_moves_the_offset_once_and_keeps_t_continuous() {
    let mut sim = Sim::new(48_000, 3);
    let mut d = Daemon::locked(48_000, 20.0);
    sim.run_until(START + SEC, None);
    sim.run_until(START + 10 * SEC, Some(&d));
    let st0 = sim.status();
    assert_eq!(st0.absorbs, 1, "{st0:?}");
    let off0 = sim.tl.media_offset();

    d.step(sim.now, 10 * MS as i64);
    let now = sim.now;
    let before = sim.tl.device_time_at(now);
    sim.step(Some(&d));
    let after = sim.tl.device_time_at(now);
    assert_eq!(before.0, after.0);
    assert!((before.1 - after.1).abs() < 1e-6, "{before:?} {after:?}");

    let mut changes = 0;
    let mut off = sim.tl.media_offset();
    assert_eq!(off - off0, 480);
    while sim.now < START + 40 * SEC {
        sim.step(Some(&d));
        if sim.tl.media_offset() != off {
            changes += 1;
            off = sim.tl.media_offset();
        }
        assert!(sim.error(&d).abs() <= 0.5);
    }
    assert_eq!(changes, 0);
    let st = sim.status();
    assert_eq!(st.absorbs, st0.absorbs + 1);
    assert_eq!(st.seed_bumps, 0);
    // The HAL does not see the step at all.
    let knots = sim.knots_between(1, START + 5 * SEC, START + 40 * SEC);
    let dev = max_second_difference(&knots);
    assert!(dev < 1.0, "{dev} us");
}

#[test]
fn locking_and_unlocked_inputs_are_ignored() {
    let mut sim = Sim::new(48_000, 4);
    let d = Daemon::locked(48_000, -15.0);
    sim.run_until(START + 10 * SEC, Some(&d));
    let st0 = sim.status();
    let (off0, rho0) = (sim.tl.media_offset(), sim.tl.model().rho);

    // The daemon re-locks somewhere else: 5 ms off, 80 ppm faster.
    let mut locking = d;
    locking.state = ClockState::Locking;
    locking.set_rate(sim.now, 65.0);
    locking.step(sim.now, 5 * MS as i64);
    sim.run_until(START + 15 * SEC, Some(&locking));
    let st = sim.status();
    assert_eq!(st.regime, Regime::Holdover);
    assert_eq!((st.absorbs, st.seed_bumps), (st0.absorbs, 0));
    assert_eq!(sim.tl.media_offset(), off0);
    assert_eq!(sim.tl.model().rho, rho0);

    let mut unlocked = locking;
    unlocked.state = ClockState::Unlocked;
    unlocked.valid = false;
    sim.run_until(START + 17 * SEC, Some(&unlocked));
    assert_eq!(sim.status().regime, Regime::Holdover);
    assert_eq!(sim.tl.model().rho, rho0);
    assert_eq!(sim.tl.media_offset(), off0);
    // Holdover extrapolates the old clock, which the daemon no longer runs.
    assert!(sim.error(&d).abs() < 0.5);

    // Locked again: the whole error goes into the offset at once, then it
    // follows.
    let mut relocked = locking;
    relocked.state = ClockState::Locked;
    relocked.step_gen += 1;
    let e = sim.error(&relocked);
    assert!(e > 240.0, "{e}");
    sim.step(Some(&relocked));
    assert_eq!(sim.tl.media_offset() - off0, e.round() as i64);
    sim.run_until(START + 40 * SEC, Some(&relocked));
    let st = sim.status();
    assert_eq!(st.regime, Regime::Following);
    assert_eq!((st.absorbs, st.seed_bumps), (st0.absorbs + 1, 0));
    assert!(sim.error(&relocked).abs() < 0.5);
    // A free-running daemon is followed too.
    let mut free = relocked;
    free.state = ClockState::FreeRunning;
    free.step_gen += 1;
    sim.step(Some(&free));
    assert_eq!(sim.status().regime, Regime::Following);
}

#[test]
fn a_stale_heartbeat_holds_over_and_a_new_generation_is_absorbed() {
    let mut sim = Sim::new(48_000, 5);
    let d = Daemon::locked(48_000, 60.0);
    sim.run_until(START + 10 * SEC, Some(&d));
    let st0 = sim.status();

    // The daemon dies: its heartbeat stops.
    let died = sim.now;
    let mut dead = d;
    dead.heartbeat = Some(died);
    sim.run_until(died + 900 * MS, Some(&dead));
    assert_eq!(sim.status().regime, Regime::Following);
    assert!(sim.tl.gate(sim.now, Some(&dead.input(sim.now))));
    sim.run_until(died + 1100 * MS, Some(&dead));
    assert_eq!(sim.status().regime, Regime::Holdover);
    assert!(!sim.tl.gate(sim.now, Some(&dead.input(sim.now))));
    sim.run_until(died + 6 * SEC, Some(&dead));

    // A new daemon: a fresh servo lands 3.3 ms away, 2 ppm off, with the
    // same step_gen as the old one.
    let mut fresh = d;
    fresh.generation += 1;
    fresh.set_rate(sim.now, 62.0);
    fresh.step(sim.now, 3_300 * US as i64);
    fresh.step_gen = d.step_gen;
    let off = sim.tl.media_offset();
    sim.run_until(died + 30 * SEC, Some(&fresh));
    let st = sim.status();
    assert_eq!(st.regime, Regime::Following);
    assert_eq!((st.absorbs, st.seed_bumps), (st0.absorbs + 1, 0));
    assert_eq!(sim.tl.media_offset() - off, 158);
    assert!(sim.error(&fresh).abs() < 0.5);
    assert!(sim.tl.gate(sim.now, Some(&fresh.input(sim.now))));

    // A restart faster than the stale time is absorbed the same way.
    let mut quick = fresh;
    quick.generation += 1;
    quick.step(sim.now, -(700 * US as i64));
    quick.step_gen = 0;
    sim.run_until(died + 40 * SEC, Some(&quick));
    let st = sim.status();
    assert_eq!((st.absorbs, st.seed_bumps), (st0.absorbs + 2, 0));
    assert!(sim.error(&quick).abs() < 0.5);
    let knots = sim.knots_between(1, START, sim.now);
    let dev = max_second_difference(&knots);
    assert!(dev < 30.0, "{dev} us");
}

#[test]
fn a_rate_jump_beyond_200_ppm_bumps_the_seed_once() {
    let mut sim = Sim::new(48_000, 6);
    let mut d = Daemon::locked(48_000, 0.0);
    sim.run_until(START + 10 * SEC, Some(&d));
    assert_eq!(sim.status().seed_bumps, 0);

    d.set_rate(sim.now, 250.0);
    d.step_gen += 1;
    sim.run_until(START + 30 * SEC, Some(&d));
    let st = sim.status();
    assert_eq!((st.seed, st.seed_bumps), (2, 1));
    assert_eq!(st.regime, Regime::Following);
    let window = sim.knots_between(2, START + 15 * SEC, START + 30 * SEC);
    let err = ppm_error(hal_rate(&window), 48_000.0 * (1.0 + 250e-6));
    assert!(err.abs() < 0.5, "{err} ppm");

    // 150 ppm at a discontinuity ramps instead.
    d.set_rate(sim.now, 400.0);
    d.step_gen += 1;
    sim.run_until(START + 60 * SEC, Some(&d));
    assert_eq!(sim.status().seed_bumps, 1);
    assert!(sim.error(&d).abs() < 0.5);

    // So does any rate change of a continuous clock.
    d.set_rate(sim.now, 100.0);
    sim.run_until(START + 110 * SEC, Some(&d));
    assert_eq!(sim.status().seed_bumps, 1);
    assert!(sim.error(&d).abs() < 0.5, "{}", sim.error(&d));
    // The ramp keeps the knots smooth.
    let knots = sim.knots_between(2, START, sim.now);
    let dev = max_second_difference(&knots);
    assert!(dev < 25.0, "{dev} us");
}

#[test]
fn reset_changes_the_seed_except_the_first() {
    let mut tl = DeviceTimeline::new(TimelineParams::DEFAULT);
    assert_eq!(tl.status().seed, 1);
    tl.reset(48_000, START);
    let st = tl.status();
    assert_eq!((st.seed, st.seed_bumps, st.regime), (1, 0, Regime::Synthetic));

    // Follow a daemon for a while.
    let d = Daemon::locked(48_000, 10.0);
    let mut now = START;
    tl.start(now, Some(&d.input(now)));
    while now < START + 5 * SEC {
        tl.update(now, Some(&d.input(now)));
        tl.zero_timestamp(now);
        now += 10 * MS;
    }
    assert_eq!(tl.status().regime, Regime::Following);
    assert_ne!(tl.media_offset(), 0);

    tl.reset(96_000, now);
    let st = tl.status();
    assert_eq!((st.seed, st.seed_bumps, st.regime), (2, 1, Regime::Synthetic));
    assert_eq!((tl.media_offset(), tl.sample_rate()), (0, 96_000));
    assert_eq!(tl.zero_timestamp(now), Zts { sample_time: 0, host_ns: now, seed: 2 });
    // The daemon still runs at 48 kHz: not followed, and no audio.
    tl.update(now + 10 * MS, Some(&d.input(now + 10 * MS)));
    assert_eq!(tl.status().regime, Regime::Synthetic);
    assert!(!tl.gate(now + 10 * MS, Some(&d.input(now + 10 * MS))));
    let mut d96 = d;
    d96.rate = 96_000;
    tl.update(now + 20 * MS, Some(&d96.input(now + 20 * MS)));
    assert_eq!(tl.status().regime, Regime::Following);
    assert!(tl.gate(now + 20 * MS, Some(&d96.input(now + 20 * MS))));

    tl.reset(96_000, now + 30 * MS);
    assert_eq!((tl.status().seed, tl.status().seed_bumps), (3, 2));
}

#[test]
fn a_stall_of_five_periods_resyncs_with_a_new_seed() {
    let mut sim = Sim::new(48_000, 8);
    let d = Daemon::locked(48_000, -45.0);
    sim.run_until(START + 10 * SEC, Some(&d));
    let before = sim.last.unwrap();
    let period_ns = P * SEC / 48_000;

    // Three periods without a call: catch up one period per call.
    sim.now += 3 * period_ns + 20 * MS;
    let caught_up = |sim: &Sim| {
        let (t, _) = sim.tl.device_time_at(sim.now);
        sim.last.unwrap().sample_time == t as u64 / P * P
    };
    let mut calls = 0;
    sim.step(Some(&d));
    while !caught_up(&sim) {
        sim.step(Some(&d));
        calls += 1;
    }
    assert!((2..=4).contains(&calls), "{calls}");
    assert_eq!(sim.status().seed_bumps, 0);
    assert!(sim.last.unwrap().sample_time >= before.sample_time + 3 * P);

    // Five periods: jump to the present on a new timeline.
    sim.run_until(sim.now + SEC, Some(&d));
    let before = sim.last.unwrap();
    sim.now += 5 * period_ns + 20 * MS;
    let z = sim.step(Some(&d));
    assert_eq!(z.seed, before.seed + 1);
    assert!(z.sample_time >= before.sample_time + 5 * P);
    assert_eq!(sim.status().seed_bumps, 1);
    sim.run_until(sim.now + 10 * SEC, Some(&d));
    assert_eq!(sim.status().seed_bumps, 1);
    assert!(sim.error(&d).abs() < 0.5);
}

#[test]
fn starting_io_again_absorbs_what_drifted_while_it_was_stopped() {
    // While IO is stopped nothing updates the timeline, and the daemon's
    // clock moves on at its own rate: continuous, with the same step_gen.
    for (seed, idle, drift_ppm) in [(13, SEC, 0.0), (14, 600 * SEC, 2.0), (15, 3600 * SEC, -3.0)] {
        let master_ppm = 45.0;
        let mut nd = NoisyDaemon {
            d: Daemon::locked(48_000, master_ppm),
            master_ppm,
            master_media: MEDIA_START,
            next_publish: START,
            rng: Rng::new(seed + 100),
        };
        let mut sim = Sim::new(48_000, seed);
        while sim.now < START + 40 * SEC {
            nd.publish(sim.now);
            sim.step(Some(&nd.d));
        }
        let st0 = sim.status();
        assert_eq!(st0.regime, Regime::Following);
        let tag = format!("idle {} s, {drift_ppm} ppm", idle / SEC);

        // StopIO, and StartIO `idle` later.
        let mut d = nd.d;
        d.set_rate(sim.now, master_ppm + drift_ppm);
        sim.now += idle;
        let e = sim.error(&d);
        if idle > SEC {
            assert!(e.abs() > 10.0, "{tag}: {e}");
        }
        sim.tl.start(sim.now, Some(&d.input(sim.now)));
        // A new IO session: the device time restarts near 0, same seed.
        sim.last = None;
        assert!(sim.error(&d).abs() < 0.5, "{tag}: {} after {e}", sim.error(&d));
        let st = sim.status();
        let absorbed = u64::from(e.abs() >= 0.5);
        assert_eq!((st.absorbs, st.seed_bumps), (st0.absorbs + absorbed, 0), "{tag}");
        assert_eq!(st.regime, Regime::Following, "{tag}");

        // Audio flows from the first zero time stamp on, and the content
        // does not slip again.
        let off = sim.tl.media_offset();
        let mut first = true;
        while sim.now < START + 40 * SEC + idle + 30 * SEC {
            let input = d.input(sim.now);
            sim.tl.update(sim.now, Some(&input));
            let z = sim.tl.zero_timestamp(sim.now);
            sim.check(z);
            if first {
                assert_eq!((z.sample_time, z.seed), (0, st0.seed), "{tag}");
                first = false;
            }
            assert!(sim.tl.gate(sim.now, Some(&input)), "{tag} at {}", sim.now);
            assert!(sim.error(&d).abs() < 0.5, "{tag}: {}", sim.error(&d));
            sim.now += sim.cycle_ns + sim.rng.upto(sim.jitter_ns);
        }
        assert_eq!(sim.tl.media_offset(), off, "{tag}");

        // The daemon dies, then IO stops and starts again: nothing to follow
        // yet, so the timeline holds over with the gate closed, and the
        // daemon's return is absorbed at once.
        d.heartbeat = Some(sim.now - SEC);
        d.set_rate(sim.now, master_ppm - drift_ppm);
        sim.now += idle;
        sim.tl.start(sim.now, Some(&d.input(sim.now)));
        sim.last = None;
        let st1 = sim.status();
        assert_eq!((st1.regime, st1.absorbs), (Regime::Holdover, st.absorbs), "{tag}");
        assert!(!sim.tl.gate(sim.now, Some(&d.input(sim.now))), "{tag}");
        sim.run_until(sim.now + 2 * SEC, Some(&d));
        assert!(!sim.tl.gate(sim.now, Some(&d.input(sim.now))), "{tag}");
        d.heartbeat = None;
        let e = sim.error(&d);
        let input = d.input(sim.now);
        sim.tl.update(sim.now, Some(&input));
        assert!(sim.tl.gate(sim.now, Some(&input)), "{tag}");
        assert!(sim.error(&d).abs() < 0.5, "{tag}");
        let st2 = sim.status();
        let absorbed = u64::from(e.abs() >= 0.5);
        assert_eq!((st2.absorbs, st2.seed_bumps), (st1.absorbs + absorbed, 0), "{tag}");
        assert_eq!(st2.regime, Regime::Following, "{tag}");
    }
}

#[test]
fn the_gate_needs_a_live_coherent_daemon() {
    let mut sim = Sim::new(48_000, 9);
    let d = Daemon::locked(48_000, 25.0);
    let input = |d: &Daemon, now| Some(d.input(now));

    // Synthetic: no audio, even with a perfect daemon.
    assert!(!sim.tl.gate(sim.now, input(&d, sim.now).as_ref()));
    sim.run_until(START + SEC, None);
    assert!(!sim.tl.gate(sim.now, input(&d, sim.now).as_ref()));

    sim.run_until(START + 15 * SEC, Some(&d));
    let now = sim.now;
    let gate = |d: &Daemon| sim.tl.gate(now, Some(&d.input(now)));
    assert!(gate(&d));
    assert!(!sim.tl.gate(now, None));

    let mut x = d;
    x.heartbeat = Some(now - 1100 * MS);
    assert!(!gate(&x));
    x.heartbeat = Some(now - 900 * MS);
    assert!(gate(&x));
    // A heartbeat just ahead of our clock is a race, far ahead is garbage.
    x.heartbeat = Some(now + 500 * MS);
    assert!(gate(&x));
    x.heartbeat = Some(now + 2 * SEC);
    assert!(!gate(&x));

    for (shift_us, open) in [(300, false), (-300, false), (200, true), (-200, true)] {
        let mut x = d;
        x.snapshot.media_ref_ns =
            x.snapshot.media_ref_ns.checked_add_signed(shift_us * 1000).unwrap();
        assert_eq!(gate(&x), open, "{shift_us} us");
    }

    let mut x = d;
    x.valid = false;
    assert!(!gate(&x));
    let mut x = d;
    x.running = false;
    assert!(!gate(&x));
    let mut x = d;
    x.rate = 44_100;
    assert!(!gate(&x));
    for rate in [f64::NAN, f64::INFINITY, 2.0, 0.0] {
        let mut x = d;
        x.snapshot.rate = rate;
        assert!(!gate(&x), "{rate}");
    }
    // Whatever the daemon's state, as long as the clocks agree.
    let mut x = d;
    x.state = ClockState::Locking;
    assert!(gate(&x));
    sim.step(Some(&x));
    assert_eq!(sim.status().regime, Regime::Holdover);
    assert!(sim.tl.gate(sim.now, Some(&x.input(sim.now))));

    // In holdover, only the clock held over: not a step of it, and not a
    // new daemon still locking, however close (a CI run saw one land 3
    // frames away, then step when it locked).
    let now = sim.now;
    let mut stepped = x;
    stepped.step_gen += 1;
    assert!(!sim.tl.gate(now, Some(&stepped.input(now))));
    let mut fresh = x;
    fresh.generation += 1;
    fresh.step_gen = 1;
    assert!(!sim.tl.gate(now, Some(&fresh.input(now))));
    // Restarting IO keeps what is held over.
    sim.tl.start(now, Some(&x.input(now)));
    sim.last = None;
    assert!(sim.tl.gate(now, Some(&x.input(now))));
    // Following the new daemon once it locks opens the gate at once.
    fresh.state = ClockState::Locked;
    fresh.step_gen += 1;
    sim.step(Some(&fresh));
    assert_eq!(sim.status().regime, Regime::Following);
    assert!(sim.tl.gate(sim.now, Some(&fresh.input(sim.now))));
    // A reset forgets it.
    sim.tl.reset(48_000, sim.now);
    assert!(!sim.tl.gate(sim.now, Some(&fresh.input(sim.now))));
}

#[test]
fn a_day_long_run_stays_within_a_sample() {
    // Long cycles keep this fast; they stay under one period.
    for (fs, ppm, cycle) in [(48_000u32, 37.5, 60 * MS), (192_000, -91.0, 40 * MS)] {
        let mut sim = Sim::new(fs, 10);
        sim.cycle_ns = cycle;
        sim.jitter_ns = 2 * MS;
        sim.record = false;
        let d = Daemon::locked(fs, ppm);
        sim.run_until(START + 30 * SEC, Some(&d));
        let st0 = sim.status();
        assert_eq!(st0.absorbs, 1);
        let mut worst = 0.0f64;
        while sim.now < START + 24 * 3600 * SEC {
            sim.step(Some(&d));
            worst = worst.max(sim.error(&d).abs());
        }
        let st = sim.status();
        assert!(worst < 1.0, "{fs}: drifted {worst} frames");
        assert_eq!((st.absorbs, st.seed, st.media_offset), (st0.absorbs, 1, st0.media_offset));
        // The last zero time stamp is a day of frames at the daemon's rate.
        let z = sim.last.unwrap();
        let frames = (z.host_ns - START) as f64 * fs as f64 * (1.0 + ppm * 1e-6) / 1e9;
        assert!((z.sample_time as f64 - frames).abs() < 2.0, "{fs}: {z:?} vs {frames}");
        assert!((st.device_rate_ppm_milli as f64 - ppm * 1e3).abs() < 50.0, "{st:?}");
    }
}

#[test]
fn debug_jitter_alternates_around_the_model() {
    let params = TimelineParams { debug_zts_jitter_ns: 1000, ..TimelineParams::DEFAULT };
    let mut sim = Sim::with_params(48_000, 12, params);
    let d = Daemon::locked(48_000, 10.0);
    sim.run_until(START + 30 * SEC, Some(&d));
    let knots = sim.knots_between(1, START + 15 * SEC, START + 30 * SEC);
    let mut n = 0;
    for w in knots.windows(3) {
        let d1 = w[1].host_ns as i64 - w[0].host_ns as i64;
        let d2 = w[2].host_ns as i64 - w[1].host_ns as i64;
        // +j then -j: intervals alternate 2j short and 2j long.
        let expect = if (w[1].sample_time / P) % 2 == 0 { -4000 } else { 4000 };
        assert!((d2 - d1 - expect).abs() <= 3, "{d1} {d2} at {:?}", w[1]);
        n += 1;
    }
    assert!(n > 40);
}

/// A daemon that publishes a continuous clock every 125-250 ms with the
/// rate wander of a servo: it steers towards the master and adds noise.
struct NoisyDaemon {
    d: Daemon,
    master_ppm: f64,
    /// Media time of the master at `START`.
    master_media: u64,
    next_publish: u64,
    rng: Rng,
}

impl NoisyDaemon {
    fn master_at(&self, now: u64) -> u64 {
        let dl = now as i128 - START as i128;
        (self.master_media as i128 + (dl as f64 * (1.0 + self.master_ppm * 1e-6)) as i128) as u64
    }

    fn publish(&mut self, now: u64) {
        while self.next_publish <= now {
            let t = self.next_publish;
            let media = self.d.snapshot.media_ns_at(t);
            let err = self.master_at(t) as f64 - media as f64;
            let rate = 1.0 + self.master_ppm * 1e-6 + err / 2e9 + self.rng.signed() * 0.3e-6;
            self.d.snapshot = ClockSnapshot { local_ref_ns: t, media_ref_ns: media, rate };
            self.d.heartbeat = None;
            self.next_publish += 125 * MS + self.rng.upto(125 * MS);
        }
    }
}

#[test]
fn the_simulator_event_scenario() {
    // IO starts before lock; Locking, then Locked; a 10 ms step at 60 s; the
    // daemon dies at 85 s and a new one starts at 91 s, locking at 94 s.
    for seed in 0..12u64 {
        for master_ppm in [-100.0, -30.0, 0.0, 45.0, 100.0] {
            let mut rng = Rng::new(seed);
            let lock_phase = (rng.signed() * 400.0) as i64 * US as i64;
            let first = Daemon {
                snapshot: ClockSnapshot {
                    local_ref_ns: START + SEC,
                    media_ref_ns: (MEDIA_START + SEC).checked_add_signed(lock_phase).unwrap(),
                    rate: 1.0 + (master_ppm + rng.signed() * 20.0) * 1e-6,
                },
                state: ClockState::Locking,
                ..Daemon::locked(48_000, 0.0)
            };
            let mut nd = NoisyDaemon {
                d: first,
                master_ppm,
                master_media: MEDIA_START,
                next_publish: START + SEC,
                rng,
            };
            let mut sim = Sim::new(48_000, seed);
            sim.run_until(START + SEC, None);
            let mut worst_e = 0.0f64;
            let mut gate_in_outage = false;
            while sim.now < START + 120 * SEC {
                let now = sim.now;
                let t = now - START;
                if (85 * SEC..91 * SEC).contains(&t) {
                    // Dead: the region keeps the last record and heartbeat.
                    nd.d.heartbeat.get_or_insert(START + 85 * SEC);
                    nd.next_publish = START + 91 * SEC;
                } else {
                    if nd.d.state == ClockState::Locking && t >= 4 * SEC && nd.d.generation == 1 {
                        nd.d.state = ClockState::Locked;
                        nd.d.step_gen += 1;
                    }
                    if t >= 60 * SEC && nd.master_media == MEDIA_START {
                        nd.master_media += 10 * MS;
                        nd.d.step(now, 10 * MS as i64);
                    }
                    if t >= 91 * SEC && nd.d.generation == 1 {
                        // A fresh servo: unlocked, then locking 300 us away.
                        nd.d.generation = 2;
                        nd.d.step_gen = 0;
                        nd.d.valid = false;
                        nd.d.state = ClockState::Unlocked;
                        nd.d.heartbeat = None;
                    }
                    if nd.d.generation == 2 && !nd.d.valid && t >= 92 * SEC {
                        nd.d.valid = true;
                        nd.d.state = ClockState::Locking;
                        let m = nd.master_at(now).checked_add_signed(300 * US as i64).unwrap();
                        nd.d.snapshot = ClockSnapshot {
                            local_ref_ns: now,
                            media_ref_ns: m,
                            rate: 1.0 + (master_ppm + 3.0) * 1e-6,
                        };
                        nd.d.step_gen += 1;
                        nd.next_publish = now;
                    }
                    if nd.d.generation == 2 && nd.d.state == ClockState::Locking && t >= 94 * SEC {
                        nd.d.state = ClockState::Locked;
                        nd.d.step_gen += 1;
                    }
                    if nd.d.valid {
                        nd.publish(now);
                    }
                }
                sim.step(Some(&nd.d));
                let following = sim.status().regime == Regime::Following;
                let settled = (7 * SEC..59 * SEC).contains(&t)
                    || (66 * SEC..84 * SEC).contains(&t)
                    || t >= 100 * SEC;
                if following && settled {
                    worst_e = worst_e.max(sim.error(&nd.d).abs());
                }
                if (86 * SEC + 100 * MS..91 * SEC).contains(&t) {
                    gate_in_outage |= sim.tl.gate(sim.now, Some(&nd.d.input(sim.now)));
                }
            }
            let st = sim.status();
            let tag = format!("seed {seed}, master {master_ppm} ppm: {st:?}");
            assert_eq!(st.seed_bumps, 0, "{tag}");
            assert!(st.absorbs >= 3 && st.absorbs <= 4, "{tag}");
            assert_eq!(st.regime, Regime::Following, "{tag}");
            assert!(worst_e <= 2.0, "{tag}: |e| {worst_e}");
            assert!(!gate_in_outage, "{tag}");
            assert!(sim.tl.gate(sim.now, Some(&nd.d.input(sim.now))), "{tag}");
            let dev = max_second_difference(&sim.knots);
            assert!(dev <= 30.0, "{tag}: knot second difference {dev} us");
            let late = sim.knots_between(1, START + 100 * SEC, START + 120 * SEC);
            let err = ppm_error(hal_rate(&late), 48_000.0 * (1.0 + master_ppm * 1e-6));
            assert!(err.abs() < 1.0, "{tag}: HAL rate off by {err} ppm");
        }
    }
}
