//! The drift-tracking controller: a slow, gear-shifted PI phase-locked loop.
//!
//! # The problem
//!
//! A sound card consumes (or produces) frames at the rate of its own crystal,
//! while the network rings are indexed by PTP media time. The two clocks
//! differ by tens of ppm, so a stream that simply takes one media sample per
//! device frame slides through the ring by a few milliseconds per minute
//! until it reads audio that hasn't arrived yet (or writes audio after it was
//! sent).
//!
//! # The loop
//!
//! Each stream keeps a fractional *position* on the media timeline that
//! advances by `ratio` media samples per device frame (the resampler turns
//! that into audio). Once per callback the stream reads the media clock,
//! derives where the position *should* be (the *target*: for playback
//! `now - latency - safety`, for capture `now + safety`), and hands the
//! controller the error `target - position` in media samples. The
//! controller answers with a [`Steer`]: normally just the ratio to use until
//! the next callback,
//!
//! ```text
//! raw error ──► 2 × one-pole low-pass ──► e ──► ratio = 1 + Kp·e + ∫Ki·e dt
//! ```
//!
//! Position integrates the ratio, so with the PI controller this is a
//! type-2 loop: it follows a constant frequency offset with zero steady-state
//! phase error, and the integrator converges to the true clock ratio. The
//! gains follow the standard second-order design, `Kp = 2ζω`, `Ki = ω²`
//! (errors measured in seconds), critically damped (`ζ = 1`) so the error
//! never overshoots.
//!
//! * **Jitter.** Callbacks are not evenly spaced (scheduling delays of
//!   hundreds of µs are normal), so each reading of the media clock is
//!   noisy. The two low-pass stages (poles at 10 × ω, keeping ≈ 50° of
//!   phase margin) and the narrow loop bandwidth average that noise out.
//! * **Gear shifting.** A narrow loop rejects jitter well but takes a long
//!   time to pull in. The loop therefore starts wide (ω = 1 rad/s, locking
//!   within a few seconds even to a 900 ppm offset), then narrows as `1/t`
//!   (the least-squares rate at which a frequency estimate improves) down
//!   to its tracking bandwidth, ω = 0.025 rad/s, a 40 s time constant:
//!   crystals wander far more slowly than that. The narrowing only advances
//!   while the filtered error is under 200 µs, so a loop that hasn't caught
//!   the frequency yet stays wide. Within about 40 s the ratio stays within
//!   ±2 ppm of the true clock ratio at every callback, even with a
//!   millisecond of callback jitter: a pitch modulation of a few
//!   thousandths of a cent. If the filtered error ever exceeds 0.5 ms (a
//!   frequency step, a short stall), the loop widens again. The ratio is
//!   clamped to ±1000 ppm (1.7 cents) in every state, with integrator
//!   anti-windup.
//! * **Settling.** After a reset the stream stays muted for at least 100 ms
//!   and 8 callbacks while the controller watches where callbacks land. This
//!   rides out the burst of back-to-back callbacks most backends issue when
//!   a stream starts, and lets the loop start from the *average* callback
//!   timing instead of a single jittery reading.
//! * **Realignment.** A running loop gives up and settles again if the raw
//!   error exceeds the jump limit (≈ 50 ms: start-up, a stall, a media-clock
//!   step), or if it stays on the side where audio gets lost (playback
//!   reading samples that haven't arrived, capture writing samples after
//!   they were sent) for 50 ms. Isolated late callbacks never trigger this.
//!   If the loop was locked, its frequency estimate survives, so it relocks
//!   within seconds.
//!
//! The controller is pure and deterministic: it only sees the errors and
//! frame counts it is given, which is how the tests at the bottom of this
//! file exercise it with simulated clocks.

/// Largest correction the controller ever applies, in ppm.
pub const MAX_PPM: f64 = 1000.0;
/// Initial (acquisition) loop bandwidth, rad/s.
const ACQUIRE_BW: f64 = 1.0;
/// Final (tracking) loop bandwidth, rad/s.
const TRACK_BW: f64 = 0.025;
/// Time spent at the acquisition bandwidth before narrowing, seconds. After
/// that the bandwidth falls as `1/t` down to `TRACK_BW`.
const ACQUIRE_SECONDS: f64 = 2.0;
/// The narrowing only progresses while the filtered error is within this
/// many seconds: a loop that hasn't caught the frequency yet stays wide.
const NARROW_TOLERANCE: f64 = 200e-6;
/// Damping ratio of the loop.
const DAMPING: f64 = 1.0;
/// Error low-pass poles, as a multiple of the loop bandwidth.
const FILTER_RATIO: f64 = 10.0;
/// Minimum settling time after a reset, seconds of device frames.
const SETTLE_SECONDS: f64 = 0.1;
/// Minimum number of settling callbacks after a reset.
const SETTLE_CALLBACKS: u32 = 8;
/// A filtered error beyond this (seconds) widens the loop again.
const REACQUIRE: f64 = 500e-6;
/// The raw error must stay on the unsafe side for this long (seconds) and
/// this many callbacks before the controller realigns.
const UNSAFE_SECONDS: f64 = 0.05;
const UNSAFE_CALLBACKS: u32 = 3;
/// The loop counts as locked once the filtered error has stayed within this
/// many seconds (of media time) for `LOCK_HOLD_SECONDS`.
const LOCK_TOLERANCE: f64 = 100e-6;
const LOCK_HOLD_SECONDS: f64 = 1.0;

/// What a stream should do with one callback: move its position by `shift`
/// media samples, output silence if `mute`, then advance by `ratio` media
/// samples per device frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Steer {
    pub shift: f64,
    pub ratio: f64,
    pub mute: bool,
}

impl Steer {
    /// Whether this callback breaks the continuity of the stream.
    pub fn is_discontinuous(&self) -> bool {
        self.mute || self.shift != 0.0
    }
}

/// When the controller gives up tracking and realigns. All values are in
/// media samples and positive.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limits {
    /// Largest raw error (either sign) the loop tries to pull in.
    pub jump: f64,
    /// Largest persistent error with the position *ahead* of its target
    /// (`target - position < -ahead`). For playback this is the safety
    /// margin: further ahead, it reads samples that haven't arrived.
    pub ahead: f64,
    /// Largest persistent error with the position *behind* its target
    /// (`target - position > behind`). For capture this is the safety
    /// margin: further behind, it writes samples after they were sent.
    pub behind: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    /// Muted, measuring the offset between position and target.
    Settling { frames: u64, callbacks: u32, sum: f64, count: u32 },
    /// Tracking; `time` is the time since the loop (re)started acquiring.
    Running { time: f64 },
}

const SETTLING: Phase = Phase::Settling { frames: 0, callbacks: 0, sum: 0.0, count: 0 };

/// Steers a stream's resampling ratio so that its position tracks a target
/// on the media timeline. See the [module documentation](self).
#[derive(Clone, Debug)]
pub struct DriftController {
    rate: f64,
    limits: Limits,
    phase: Phase,
    lp1: f64,
    lp2: f64,
    /// Integral part of the correction (the frequency estimate), as a ratio
    /// offset.
    integral: f64,
    ratio: f64,
    locked: bool,
    in_tolerance: f64,
    unsafe_time: f64,
    unsafe_callbacks: u32,
    realigns: u64,
}

impl DriftController {
    /// A controller for a device running at nominally `rate` frames per
    /// second, starting in the settling state.
    pub fn new(rate: f64, limits: Limits) -> Self {
        DriftController {
            rate,
            limits,
            phase: SETTLING,
            lp1: 0.0,
            lp2: 0.0,
            integral: 0.0,
            ratio: 1.0,
            locked: false,
            in_tolerance: 0.0,
            unsafe_time: 0.0,
            unsafe_callbacks: 0,
            realigns: 0,
        }
    }

    /// Feeds one callback: `error` is `target - position` in media samples,
    /// measured at the start of the callback, and `frames` the number of
    /// device frames the callback processes.
    pub fn update(&mut self, error: f64, frames: usize) -> Steer {
        let dt = frames as f64 / self.rate;
        if !error.is_finite() || error.abs() > self.limits.jump {
            return self.realign(error);
        }
        let settle_frames = SETTLE_SECONDS * self.rate;
        let mut time = match &mut self.phase {
            Phase::Settling { frames: total, callbacks, sum, count } => {
                *total += frames as u64;
                *callbacks += 1;
                if *callbacks == 1 {
                    // Snap onto the target, then watch where the following
                    // callbacks land relative to it.
                    return Steer { shift: error, ratio: self.ratio, mute: true };
                }
                // Skip the first half (start-up bursts), average the rest.
                if *callbacks > SETTLE_CALLBACKS / 2 && *total as f64 >= settle_frames / 2.0 {
                    *sum += error;
                    *count += 1;
                }
                if *callbacks < SETTLE_CALLBACKS || (*total as f64) < settle_frames {
                    return Steer { shift: 0.0, ratio: self.ratio, mute: true };
                }
                // Start from the average offset, so that the loop is centred
                // on the typical callback timing rather than on whichever
                // callback happened to come first.
                let shift = *sum / (*count).max(1) as f64;
                self.phase = Phase::Running { time: 0.0 };
                self.lp1 = 0.0;
                self.lp2 = 0.0;
                return Steer { shift, ratio: self.ratio, mute: false };
            }
            Phase::Running { time } => *time,
        };

        // Persistently on the side where audio gets lost: give up.
        if error < -self.limits.ahead || error > self.limits.behind {
            self.unsafe_time += dt;
            self.unsafe_callbacks += 1;
            if self.unsafe_time >= UNSAFE_SECONDS && self.unsafe_callbacks >= UNSAFE_CALLBACKS {
                return self.realign(error);
            }
        } else {
            self.unsafe_time = 0.0;
            self.unsafe_callbacks = 0;
        }

        let a = 1.0 - (-dt * FILTER_RATIO * bandwidth(time)).exp();
        self.lp1 += a * (error - self.lp1);
        self.lp2 += a * (self.lp1 - self.lp2);
        if time > ACQUIRE_SECONDS && self.lp2.abs() > REACQUIRE * self.rate {
            // Far off (a frequency step, a short stall): pull in at full
            // speed again.
            time = 0.0;
        } else if self.lp2.abs() <= NARROW_TOLERANCE * self.rate {
            time += dt;
        }
        self.phase = Phase::Running { time };

        // PI on the filtered error, in seconds of media time.
        let bw = bandwidth(time);
        let e = self.lp2 / self.rate;
        let kp = 2.0 * DAMPING * bw;
        let ki = bw * bw;
        let max = MAX_PPM * 1e-6;
        let integral = (self.integral + ki * e * dt).clamp(-max, max);
        let unclamped = kp * e + integral;
        // Anti-windup: while the output is saturated, only let the integral
        // move back towards the linear range.
        if unclamped.abs() <= max || integral.abs() < self.integral.abs() {
            self.integral = integral;
        }
        self.ratio = 1.0 + (kp * e + self.integral).clamp(-max, max);

        let tolerance = LOCK_TOLERANCE * self.rate;
        if time >= ACQUIRE_SECONDS && self.lp2.abs() < tolerance {
            self.in_tolerance += dt;
        } else {
            self.in_tolerance = 0.0;
        }
        if self.in_tolerance >= LOCK_HOLD_SECONDS {
            self.locked = true;
        } else if self.lp2.abs() > 4.0 * tolerance {
            self.locked = false;
        }
        Steer { shift: 0.0, ratio: self.ratio, mute: false }
    }

    /// Forgets the phase and goes back to settling: the stream realigns and
    /// stays muted for a moment. Call when the media clock disappears or the
    /// stream's timing changes.
    pub fn reset(&mut self) {
        if self.running() {
            self.realigns += 1;
        }
        if !self.locked {
            self.integral = 0.0;
        }
        self.phase = SETTLING;
        self.lp1 = 0.0;
        self.lp2 = 0.0;
        self.ratio = 1.0 + self.integral;
        self.locked = false;
        self.in_tolerance = 0.0;
        self.unsafe_time = 0.0;
        self.unsafe_callbacks = 0;
    }

    /// Resets and treats this callback as the first one of the new settling
    /// period.
    fn realign(&mut self, error: f64) -> Steer {
        self.reset();
        self.phase = Phase::Settling { frames: 0, callbacks: 1, sum: 0.0, count: 0 };
        let shift = if error.is_finite() { error } else { 0.0 };
        Steer { shift, ratio: self.ratio, mute: true }
    }

    /// The ratio in effect: media samples per device frame.
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// The filtered error (`target - position`), in media samples.
    pub fn error(&self) -> f64 {
        self.lp2
    }

    /// Whether the loop has converged and is tracking.
    pub fn locked(&self) -> bool {
        self.locked
    }

    /// Whether the controller is past settling (the stream is audible).
    pub fn running(&self) -> bool {
        matches!(self.phase, Phase::Running { .. })
    }

    /// How many times the loop lost track and had to realign after it had
    /// started running.
    pub fn realigns(&self) -> u64 {
        self.realigns
    }
}

/// Loop bandwidth (rad/s) `time` seconds after the loop started acquiring.
fn bandwidth(time: f64) -> f64 {
    if time < ACQUIRE_SECONDS {
        ACQUIRE_BW
    } else {
        (ACQUIRE_BW * ACQUIRE_SECONDS / time).max(TRACK_BW)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 48_000.0;

    fn limits() -> Limits {
        Limits { jump: 0.05 * RATE, ahead: 0.001 * RATE, behind: 0.05 * RATE }
    }

    /// Small deterministic PRNG (SplitMix64) for reproducible jitter.
    struct Rng(u64);
    impl Rng {
        fn next_f64(&mut self) -> f64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^= z >> 31;
            (z >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    struct Step {
        time: f64,
        steer: Steer,
        error: f64,
        raw_error: f64,
        locked: bool,
    }

    /// Simulates a playback stream whose device clock runs `drift_ppm` fast
    /// relative to media time. Callbacks of `period` frames are measured
    /// late by a uniformly distributed delay in `[0, jitter)` seconds (one
    /// in a hundred three times that); the first `burst` callbacks arrive
    /// back to back. `clock_step` (time, samples) shifts the media clock
    /// once.
    fn simulate(
        drift_ppm: f64,
        period: usize,
        jitter: f64,
        seconds: f64,
        burst: usize,
        clock_step: Option<(f64, f64)>,
    ) -> (DriftController, Vec<Step>) {
        let mut c = DriftController::new(RATE, limits());
        let mut rng = Rng(drift_ppm.to_bits() ^ period as u64);
        let device_rate = RATE * (1.0 + drift_ppm * 1e-6);
        let origin = 1.0e9; // arbitrary media-time origin, samples
        let mut pos = 0.0f64;
        let mut trace = Vec::new();
        let callbacks = (seconds * device_rate / period as f64) as usize;
        for i in 0..callbacks {
            // When the device asks for callback i, in seconds of media time.
            let due = (i.saturating_sub(burst) * period) as f64 / device_rate;
            let mut late = rng.next_f64() * jitter;
            if rng.next_f64() < 0.01 {
                late *= 3.0;
            }
            let t = due + late;
            let mut now = origin + t * RATE;
            if let Some((at, step)) = clock_step
                && t >= at
            {
                now += step;
            }
            let target = now - 192.0 - 400.0;
            let raw_error = target - pos;
            let steer = c.update(raw_error, period);
            pos += steer.shift + steer.ratio * period as f64;
            trace.push(Step { time: t, steer, error: c.error(), raw_error, locked: c.locked() });
        }
        (c, trace)
    }

    fn ppm(ratio: f64) -> f64 {
        (ratio - 1.0) * 1e6
    }

    /// What a tracking simulation must achieve.
    struct Bounds {
        /// Locked within this many seconds.
        lock_by: f64,
        /// From this time on, at every callback...
        settle: f64,
        /// ...the ratio is within this many ppm of the true clock ratio...
        max_ppm: f64,
        /// ...and the filtered error within this many seconds.
        max_error: f64,
        /// Realignments tolerated before lock.
        max_realigns: u64,
    }

    /// The bounds for ordinary callback timing.
    const STRICT: Bounds =
        Bounds { lock_by: 10.0, settle: 40.0, max_ppm: 2.0, max_error: 50e-6, max_realigns: 0 };

    /// Runs a 150 s simulation and checks the loop locks in time, never
    /// realigns or loses lock afterwards, and converges within `bounds`.
    fn check_tracking(drift_ppm: f64, period: usize, jitter: f64, bounds: Bounds) {
        let Bounds { lock_by, settle, max_ppm, max_error, max_realigns } = bounds;
        let (c, trace) = simulate(drift_ppm, period, jitter, 150.0, 3, None);
        // The ratio converges to media samples per device frame.
        let true_ppm = ppm(1.0 / (1.0 + drift_ppm * 1e-6));
        let lock_at = trace.iter().position(|s| s.locked).expect("never locked");
        let lock_time = trace[lock_at].time;
        assert!(lock_time < lock_by, "locked only after {lock_time:.1} s");
        // Once locked: never another discontinuity, never loses lock.
        for s in &trace[lock_at..] {
            assert!(!s.steer.is_discontinuous(), "realigned at {:.2} s after lock", s.time);
            assert!(s.locked, "lost lock at {:.2} s", s.time);
        }
        assert!(c.realigns() <= max_realigns, "{} realignments", c.realigns());
        let mut worst_ppm = 0f64;
        let mut worst_err = 0f64;
        for s in trace.iter().filter(|s| s.time > settle) {
            worst_ppm = worst_ppm.max((ppm(s.steer.ratio) - true_ppm).abs());
            worst_err = worst_err.max(s.error.abs());
        }
        assert!(worst_ppm < max_ppm, "ratio off by up to {worst_ppm:.3} ppm");
        assert!(worst_err < max_error * RATE, "error up to {worst_err:.3} samples");
        // The raw error, jitter included, averages out to zero.
        let tail: Vec<f64> = trace.iter().filter(|s| s.time > 75.0).map(|s| s.raw_error).collect();
        let mean = tail.iter().sum::<f64>() / tail.len() as f64;
        assert!(mean.abs() < max_error * RATE, "mean raw error {mean:.3} samples");
    }

    /// Prints how the loop converges (`cargo test -p ovsc-soundcard
    /// convergence_report -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn convergence_report() {
        let cases = [
            (50.0, 256, 1e-3),
            (-80.0, 256, 1e-3),
            (-80.0, 32, 0.3e-3),
            (50.0, 2048, 2e-3),
            (-900.0, 256, 0.5e-3),
        ];
        for (drift, period, jitter) in cases {
            let (_, trace) = simulate(drift, period, jitter, 180.0, 3, None);
            let true_ppm = ppm(1.0 / (1.0 + drift * 1e-6));
            println!("drift {drift} ppm, {period} frames, jitter {} ms", jitter * 1e3);
            for w in 0..18 {
                let from = w as f64 * 10.0;
                let win: Vec<&Step> =
                    trace.iter().filter(|s| s.time >= from && s.time < from + 10.0).collect();
                let dev: Vec<f64> = win.iter().map(|s| ppm(s.steer.ratio) - true_ppm).collect();
                let worst = dev.iter().fold(0f64, |a, &d| a.max(d.abs()));
                let mean = dev.iter().sum::<f64>() / dev.len() as f64;
                let err = win.iter().fold(0f64, |a, s| a.max(s.error.abs()));
                println!(
                    "  {from:3}-{:3} s: ratio error worst {worst:7.3} mean {mean:7.3} ppm, \
                     |error| ≤ {:6.1} µs",
                    from + 10.0,
                    err / RATE * 1e6
                );
            }
        }
    }

    #[test]
    fn tracks_fast_device_with_jitter() {
        check_tracking(50.0, 256, 1e-3, STRICT);
    }

    #[test]
    fn tracks_slow_device_with_jitter() {
        check_tracking(-80.0, 256, 1e-3, STRICT);
    }

    #[test]
    fn tracks_with_small_and_large_buffers() {
        check_tracking(-80.0, 32, 0.3e-3, STRICT);
        // 43 ms callbacks with up to 6 ms of jitter: ten times fewer and much
        // noisier measurements. Pulling in takes longer and the ratio
        // wanders more, though still far below audibility.
        let harsh = Bounds {
            lock_by: 40.0,
            settle: 80.0,
            max_ppm: 8.0,
            max_error: 150e-6,
            max_realigns: 3,
        };
        check_tracking(50.0, 2048, 2e-3, harsh);
    }

    #[test]
    fn large_drift_saturates_without_winding_up() {
        // 5000 ppm cannot be followed: the ratio pins at the limit and the
        // loop keeps realigning instead of drifting off.
        let (c, trace) = simulate(5000.0, 256, 0.5e-3, 20.0, 0, None);
        for s in &trace {
            assert!(ppm(s.steer.ratio).abs() <= MAX_PPM + 1e-6);
        }
        assert!(c.realigns() > 0);
        // 900 ppm is within range: it locks.
        let (c, trace) = simulate(-900.0, 256, 0.5e-3, 60.0, 0, None);
        assert!(trace.last().unwrap().locked);
        assert!((ppm(c.ratio()) - 900.81).abs() < 2.0, "{}", ppm(c.ratio()));
    }

    #[test]
    fn clock_step_realigns_and_relocks_quickly() {
        let (c, trace) = simulate(30.0, 256, 0.5e-3, 90.0, 3, Some((60.0, 0.2 * RATE)));
        assert_eq!(c.realigns(), 1);
        let jump = trace.iter().position(|s| s.time >= 60.0).unwrap();
        assert!(trace[jump].steer.mute);
        assert!(trace[..jump].iter().filter(|s| s.time > 1.0).all(|s| !s.steer.mute));
        // The frequency estimate survived: relocked within a few seconds.
        let relock = trace[jump..].iter().find(|s| s.locked).expect("no relock");
        assert!(relock.time < 66.0, "relocked at {:.1} s", relock.time);
        assert!(trace.last().unwrap().locked);
    }

    #[test]
    fn backwards_step_on_unsafe_side_realigns_promptly() {
        // A 5 ms step back puts playback 5 ms ahead of what has arrived:
        // beyond the 1 ms margin, so the controller realigns at once.
        let (c, trace) = simulate(30.0, 256, 0.5e-3, 60.0, 3, Some((40.0, -0.005 * RATE)));
        assert_eq!(c.realigns(), 1);
        let at = trace.iter().position(|s| s.steer.mute && s.time > 1.0).unwrap();
        let delay = trace[at].time - 40.0;
        assert!((0.0..0.1).contains(&delay), "realigned after {delay:.3} s");
        assert!(trace.last().unwrap().locked);
    }

    #[test]
    fn small_forward_step_is_pulled_in_without_realigning() {
        // 2 ms behind is on the safe side for playback: slew, don't mute.
        let (c, trace) = simulate(30.0, 256, 0.5e-3, 60.0, 3, Some((30.0, 0.002 * RATE)));
        assert_eq!(c.realigns(), 0);
        let after = trace.iter().filter(|s| s.time > 30.0);
        assert!(after.clone().all(|s| !s.steer.is_discontinuous()));
        let relock = trace.iter().filter(|s| s.time > 30.0).find(|s| s.locked).unwrap();
        assert!(relock.time < 40.0, "relocked at {:.1} s", relock.time);
    }

    #[test]
    fn settles_before_running() {
        let mut c = DriftController::new(RATE, limits());
        let first = c.update(123.0, 4800);
        assert_eq!(first, Steer { shift: 123.0, ratio: 1.0, mute: true });
        for _ in 2..SETTLE_CALLBACKS {
            assert_eq!(c.update(10.0, 4800), Steer { shift: 0.0, ratio: 1.0, mute: true });
        }
        // Running starts by moving onto the average offset.
        assert_eq!(c.update(10.0, 4800), Steer { shift: 10.0, ratio: 1.0, mute: false });
        assert!(c.running());
        assert_eq!(c.update(0.0, 4800), Steer { shift: 0.0, ratio: 1.0, mute: false });
        c.reset();
        assert!(!c.running());
        assert!(c.update(0.0, 64).mute);
    }
}
