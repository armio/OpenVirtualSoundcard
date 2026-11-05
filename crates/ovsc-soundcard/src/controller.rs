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
//! * **Jumps.** A running loop does not slew large errors: at ±1000 ppm it
//!   would take a second per millisecond of error. Instead the stream jumps
//!   back onto its target ([`Steer::shift`], without a mute; the stream fades
//!   out and in around it):
//!   - when the stream is losing audio (playback reading samples that
//!     haven't arrived, capture writing samples after they were sent) and
//!     three callbacks in a row agree on how far off it is (a device gap,
//!     an xrun, early callbacks), or at once if the stream announced a step
//!     ([`DriftController::expect_step`]: a reported xrun, a larger
//!     callback). Late callbacks, alone or catching up, never cause a jump;
//!   - after 50 ms on the other side, when the stream has fallen further
//!     behind (playback) or ahead (capture) than one callback plus the
//!     margin: a stall or a media-clock step, whose backlog would otherwise
//!     add latency for tens of seconds;
//!   - at once for an error beyond the jump limit (≈ 50 ms) once locked.
//!
//!   The jump moves by the smallest of the errors that triggered it (late
//!   callbacks only ever add to the error), and the frequency estimate and
//!   lock survive it. Errors beyond the limits feed the loop clamped to the
//!   limit, so a discontinuity cannot wind up the integrator.
//! * **Realignment.** Without a trustworthy frequency estimate (not locked
//!   yet), an error beyond the jump limit resets the loop and settles again.
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
/// The raw error must stay beyond a limit for this long (seconds) and this
/// many callbacks before the controller jumps...
const PERSIST_SECONDS: f64 = 0.05;
const PERSIST_CALLBACKS: u32 = 3;
/// ...unless the stream is losing audio and this many callbacks in a row
/// agree on the error.
const AGREE_CALLBACKS: u32 = 3;
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

/// A side of the target a stream's position can be on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// `target - position < 0`.
    Ahead,
    /// `target - position > 0`.
    Behind,
}

/// When the controller stops slewing and jumps or realigns. All distances
/// are in media samples and positive.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limits {
    /// Largest raw error (either sign) the loop handles without starting
    /// over (once locked: without jumping at once).
    pub jump: f64,
    /// Largest persistent error with the position *ahead* of its target
    /// (`target - position < -ahead`) left to the loop. For playback this
    /// is the safety margin: further ahead, it reads samples that haven't
    /// arrived.
    pub ahead: f64,
    /// Largest persistent error with the position *behind* its target
    /// (`target - position > behind`) left to the loop. For capture this is
    /// the safety margin: further behind, it writes samples after they were
    /// sent.
    pub behind: f64,
    /// The side on which the stream loses audio: [`Side::Ahead`] for
    /// playback, [`Side::Behind`] for capture.
    pub lossy: Side,
}

impl Limits {
    fn on(&self, side: Side) -> f64 {
        match side {
            Side::Ahead => self.ahead,
            Side::Behind => self.behind,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    /// Muted, measuring the offset between position and target.
    Settling { frames: u64, callbacks: u32, sum: f64, count: u32 },
    /// Tracking; `time` is the time since the loop (re)started acquiring.
    Running { time: f64 },
}

const SETTLING: Phase = Phase::Settling { frames: 0, callbacks: 0, sum: 0.0, count: 0 };

/// Consecutive callbacks whose raw error lay beyond the limit on one side.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Excursion {
    side: Side,
    /// Device time spent beyond the limit, seconds.
    time: f64,
    callbacks: u32,
    /// The smallest raw error seen: late callbacks only ever add to the
    /// error, so this is the best estimate of how far off the stream is.
    min: f64,
    /// The raw errors of the previous two callbacks, newest first.
    recent: [f64; 2],
}

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
    excursion: Option<Excursion>,
    /// The stream announced a step: trust the next error beyond a limit.
    expect_step: bool,
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
            excursion: None,
            expect_step: false,
            realigns: 0,
        }
    }

    /// Replaces the limits, e.g. when the callback size changes.
    pub fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
    }

    /// The limits in effect.
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Announces that the stream's timing has just stepped (the device
    /// reported an xrun, the target moved): if the next error is beyond a
    /// limit, jump at once instead of waiting for a second callback to
    /// confirm it.
    pub fn expect_step(&mut self) {
        self.expect_step = true;
    }

    /// Feeds one callback: `error` is `target - position` in media samples,
    /// measured at the start of the callback, and `frames` the number of
    /// device frames the callback processes.
    pub fn update(&mut self, error: f64, frames: usize) -> Steer {
        let expect_step = std::mem::take(&mut self.expect_step);
        let dt = frames as f64 / self.rate;
        if !error.is_finite() {
            return self.realign(error);
        }
        if error.abs() > self.limits.jump {
            // A long stall or a media-clock step. A locked loop knows the
            // clocks' ratio, so the stream can carry on from the new
            // position at once; otherwise start over.
            return if self.locked { self.jump(error) } else { self.realign(error) };
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

        if let Some(shift) = self.excursion(error, frames, expect_step) {
            return self.jump(shift);
        }
        // Beyond a limit the loop sees the error clamped to that limit: it
        // keeps pushing in the right direction, but a discontinuity that is
        // about to be jumped over can't wind up the integrator.
        let error = error.clamp(-self.limits.ahead, self.limits.behind);

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

    /// Tracks runs of callbacks beyond a limit and decides when the stream
    /// should jump, and by how much.
    fn excursion(&mut self, error: f64, frames: usize, expect_step: bool) -> Option<f64> {
        let side = if error < -self.limits.ahead {
            Side::Ahead
        } else if error > self.limits.behind {
            Side::Behind
        } else {
            self.excursion = None;
            return None;
        };
        if expect_step {
            return Some(error);
        }
        let mut x = match self.excursion {
            Some(x) if x.side == side => x,
            _ => Excursion { side, time: 0.0, callbacks: 0, min: error, recent: [error; 2] },
        };
        let [previous, before] = x.recent;
        x.time += frames as f64 / self.rate;
        x.callbacks += 1;
        x.min = x.min.min(error);
        x.recent = [error, previous];
        self.excursion = Some(x);
        // Losing audio: as soon as three callbacks in a row agree on how far
        // off the stream is, it is a step, not jitter. Late callbacks are
        // rarely late three times running, and the back-to-back callbacks
        // that catch up after one change the error by a whole callback
        // each, hence the tolerance of half a callback.
        let tolerance = self.limits.on(side).min(frames as f64 / 2.0);
        let (lo, hi) = (error.min(previous).min(before), error.max(previous).max(before));
        if side == self.limits.lossy && x.callbacks >= AGREE_CALLBACKS && hi - lo <= tolerance {
            return Some(lo);
        }
        if x.time >= PERSIST_SECONDS && x.callbacks >= PERSIST_CALLBACKS {
            return Some(x.min);
        }
        None
    }

    /// Moves the stream by `shift` onto its target without settling: the
    /// frequency estimate (and lock) stay, the filters restart from zero.
    fn jump(&mut self, shift: f64) -> Steer {
        self.realigns += 1;
        self.excursion = None;
        self.lp1 = 0.0;
        self.lp2 = 0.0;
        self.ratio = 1.0 + self.integral;
        Steer { shift, ratio: self.ratio, mute: false }
    }

    /// Forgets the phase and goes back to settling: the stream realigns and
    /// stays muted for a moment. Call when the media clock disappears.
    /// Calling it again while waiting to settle changes nothing, so a locked
    /// loop's frequency estimate survives a clock outage of any length.
    pub fn reset(&mut self) {
        if self.phase == SETTLING {
            return;
        }
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
        self.excursion = None;
        self.expect_step = false;
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

    /// How many times the stream jumped or realigned after it had started
    /// running: each is a short fade out and in (or, before the loop
    /// locked, a settling mute).
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

    /// The margin of the simulated streams, samples (1 ms).
    const MARGIN: f64 = 0.001 * RATE;

    /// Playback-style limits for callbacks of `period` frames: losing audio
    /// ahead of the target, a callback plus the margin of slack behind it.
    fn limits(period: usize) -> Limits {
        let jump = 0.05 * RATE;
        Limits {
            jump,
            ahead: MARGIN,
            behind: (period as f64 + MARGIN).min(jump),
            lossy: Side::Ahead,
        }
    }

    /// The mirror image, for capture.
    fn capture_limits(period: usize) -> Limits {
        let Limits { jump, ahead, behind, .. } = limits(period);
        Limits { jump, ahead: behind, behind: ahead, lossy: Side::Behind }
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

    /// A simulation: a stream whose device clock runs `drift_ppm` fast
    /// relative to media time. Callbacks of `period` frames are measured
    /// late by a uniformly distributed delay in `[0, jitter)` seconds (one
    /// in a hundred three times that); the first `burst` callbacks arrive
    /// back to back. `clock_step` (time, samples) shifts the media clock
    /// once; `gap` (time, frames) makes the device lose that many frames
    /// (an xrun), so that every later callback comes that much later, once
    /// or, with `gap_every`, every that many seconds from then on.
    #[derive(Clone, Copy)]
    struct Sim {
        drift_ppm: f64,
        period: usize,
        jitter: f64,
        seconds: f64,
        burst: usize,
        clock_step: Option<(f64, f64)>,
        gap: Option<(f64, usize)>,
        gap_every: Option<f64>,
        limits: Limits,
    }

    impl Sim {
        fn new(drift_ppm: f64, period: usize, jitter: f64, seconds: f64) -> Self {
            Sim {
                drift_ppm,
                period,
                jitter,
                seconds,
                burst: 3,
                clock_step: None,
                gap: None,
                gap_every: None,
                limits: limits(period),
            }
        }

        fn run(self) -> (DriftController, Vec<Step>) {
            let Sim {
                drift_ppm,
                period,
                jitter,
                seconds,
                burst,
                clock_step,
                gap,
                gap_every,
                limits,
            } = self;
            let mut c = DriftController::new(RATE, limits);
            let mut rng = Rng(drift_ppm.to_bits() ^ period as u64);
            let device_rate = RATE * (1.0 + drift_ppm * 1e-6);
            let origin = 1.0e9; // arbitrary media-time origin, samples
            let mut pos = 0.0f64;
            let mut trace = Vec::new();
            let callbacks = (seconds * device_rate / period as f64) as usize;
            for i in 0..callbacks {
                // When the device asks for callback i, in seconds of media
                // time.
                let mut due = (i.saturating_sub(burst) * period) as f64 / device_rate;
                if let Some((at, frames)) = gap
                    && due >= at
                {
                    let gaps = gap_every.map_or(1.0, |every| ((due - at) / every).floor() + 1.0);
                    due += gaps * frames as f64 / device_rate;
                }
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
                trace.push(Step {
                    time: t,
                    steer,
                    error: c.error(),
                    raw_error,
                    locked: c.locked(),
                });
            }
            (c, trace)
        }
    }

    /// A playback simulation without gaps.
    fn simulate(
        drift_ppm: f64,
        period: usize,
        jitter: f64,
        seconds: f64,
        burst: usize,
        clock_step: Option<(f64, f64)>,
    ) -> (DriftController, Vec<Step>) {
        Sim { burst, clock_step, ..Sim::new(drift_ppm, period, jitter, seconds) }.run()
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

    /// The first discontinuity after `from` seconds, if any.
    fn first_jump(trace: &[Step], from: f64) -> Option<&Step> {
        trace.iter().find(|s| s.time >= from && s.steer.is_discontinuous())
    }

    /// Mean raw error of the callbacks in `[from, to)`.
    fn mean_error(trace: &[Step], from: f64, to: f64) -> f64 {
        let errors: Vec<f64> =
            trace.iter().filter(|s| s.time >= from && s.time < to).map(|s| s.raw_error).collect();
        errors.iter().sum::<f64>() / errors.len() as f64
    }

    /// Checks that after a disturbance at `at`, the stream jumped exactly
    /// once, without muting, within `within` seconds, stayed locked, and
    /// was back where it was before (on average, jitter included) half a
    /// second later.
    fn check_one_jump(c: &DriftController, trace: &[Step], at: f64, within: f64) {
        assert_eq!(c.realigns(), 1);
        assert!(trace.iter().filter(|s| s.time > 1.0).all(|s| !s.steer.mute), "muted");
        let jump = first_jump(trace, 1.0).expect("never jumped");
        let delay = jump.time - at;
        assert!((0.0..within).contains(&delay), "jumped after {delay:.4} s");
        let lock = trace.iter().position(|s| s.locked).unwrap();
        assert!(trace[lock..].iter().all(|s| s.locked), "lost lock");
        let before = mean_error(trace, at - 2.0, at);
        let after = mean_error(trace, at + 0.5, at + 2.5);
        assert!(
            (after - before).abs() < MARGIN / 2.0,
            "error {before:.1} before, {after:.1} after"
        );
    }

    #[test]
    fn clock_step_jumps_once_locked() {
        // 200 ms: far beyond what the loop pulls in. Locked, the stream
        // jumps at once and keeps its frequency estimate and lock.
        let (c, trace) = simulate(30.0, 256, 0.5e-3, 90.0, 3, Some((60.0, 0.2 * RATE)));
        check_one_jump(&c, &trace, 60.0, 0.006);
        assert!((first_jump(&trace, 1.0).unwrap().steer.shift - 0.2 * RATE).abs() < MARGIN);
    }

    #[test]
    fn clock_step_before_lock_settles_again() {
        let (c, trace) = simulate(30.0, 256, 0.5e-3, 30.0, 3, Some((1.0, 0.2 * RATE)));
        assert_eq!(c.realigns(), 1);
        let at = trace.iter().position(|s| s.time >= 1.0).unwrap();
        assert!(trace[at].steer.mute);
        assert!(trace.last().unwrap().locked);
    }

    #[test]
    fn backwards_step_on_lossy_side_jumps_promptly() {
        // A 5 ms step back puts playback 5 ms ahead of what has arrived:
        // beyond the 1 ms margin. Three callbacks confirm it; the stream
        // jumps back without muting.
        let (c, trace) = simulate(30.0, 256, 0.5e-3, 60.0, 3, Some((40.0, -0.005 * RATE)));
        check_one_jump(&c, &trace, 40.0, 3.0 * 256.0 / RATE + 1e-3);
    }

    #[test]
    fn capture_gap_jumps_within_three_callbacks() {
        // A capture stream whose device loses one 128-frame buffer: from
        // then on it is 128 samples behind, writing audio after it was
        // sent. It must not wait 50 ms, nor settle for 100 ms.
        for period in [64, 128, 256] {
            let sim = Sim {
                gap: Some((20.0, 128)),
                limits: capture_limits(period),
                ..Sim::new(30.0, period, 0.3e-3, 40.0)
            };
            let (c, trace) = sim.run();
            // The callbacks after the gap come 128 frames late; the third
            // confirms the step.
            check_one_jump(&c, &trace, 20.0, (128 + 3 * period) as f64 / RATE + 1e-3);
            let jump = first_jump(&trace, 1.0).unwrap();
            // Smallest consistent shift: the gap, give or take the jitter.
            assert!((jump.steer.shift - 128.0).abs() < 0.3e-3 * RATE, "{}", jump.steer.shift);
        }
    }

    #[test]
    fn announced_step_jumps_at_once() {
        let sim = Sim { limits: capture_limits(128), ..Sim::new(30.0, 128, 0.3e-3, 20.0) };
        let (mut c, _) = sim.run();
        assert!(c.locked());
        // A device-reported xrun: the very next error is trusted.
        c.expect_step();
        let steer = c.update(140.0, 128);
        assert_eq!(steer.shift, 140.0);
        assert!(!steer.mute);
        assert!(c.locked());
        // An announcement doesn't make a stream jump that is on target.
        c.expect_step();
        assert!(!c.update(3.0, 128).is_discontinuous());
    }

    #[test]
    fn late_callbacks_never_jump() {
        let sim = Sim { limits: capture_limits(128), ..Sim::new(30.0, 128, 0.3e-3, 20.0) };
        let (mut c, _) = sim.run();
        // One callback 4 ms late, then the backlog back to back: the error
        // drops by a callback each time. Not a step.
        for error in [192.0, 64.0, 0.0, 192.0, 64.0, 2.0] {
            assert!(!c.update(error, 128).is_discontinuous(), "jumped at {error}");
        }
        // Late callbacks beyond the margin that don't agree either.
        for error in [100.0, 2.0, 150.0, 40.0, 60.0] {
            assert!(!c.update(error, 128).is_discontinuous(), "jumped at {error}");
        }
        assert!(c.locked());
        assert_eq!(c.realigns(), 0);
    }

    #[test]
    fn forward_step_drops_the_backlog() {
        // 20 ms behind is on the safe side for playback, but slewing it
        // back at 1000 ppm would take 20 s of extra latency. Jump after
        // 50 ms instead.
        let (c, trace) = simulate(30.0, 256, 0.5e-3, 60.0, 3, Some((30.0, 0.02 * RATE)));
        check_one_jump(&c, &trace, 30.0, 0.07);
    }

    #[test]
    fn playback_stall_drops_the_backlog() {
        // The device stalls for 20 ms: playback falls 20 ms behind.
        let sim = Sim { gap: Some((30.0, 960)), ..Sim::new(30.0, 128, 0.3e-3, 60.0) };
        let (c, trace) = sim.run();
        check_one_jump(&c, &trace, 30.0, 0.07);
    }

    #[test]
    fn repeated_stalls_dont_ratchet_the_latency_up() {
        // One lost 128-frame buffer every second: each leaves playback a
        // buffer further behind. Slewing alone (at most 48 samples per
        // second) would let the latency climb to the jump limit.
        let sim = Sim {
            gap: Some((10.0, 128)),
            gap_every: Some(1.0),
            ..Sim::new(30.0, 128, 0.3e-3, 60.0)
        };
        let (c, trace) = sim.run();
        let worst = trace.iter().filter(|s| s.time > 10.0).fold(0f64, |w, s| w.max(s.raw_error));
        // At most a callback plus the margin of slack, one more gap and
        // the jitter.
        assert!(worst < 128.0 + MARGIN + 128.0 + 0.3e-3 * RATE, "{worst:.1} samples behind");
        assert!(c.realigns() < 50, "{} jumps for 50 gaps", c.realigns());
        assert!(trace.iter().filter(|s| s.time > 1.0).all(|s| !s.steer.mute));
    }

    #[test]
    fn small_forward_step_is_pulled_in_without_jumping() {
        // 2 ms behind is within a callback plus the margin on the safe
        // side for playback: slew, don't jump.
        let (c, trace) = simulate(30.0, 256, 0.5e-3, 60.0, 3, Some((30.0, 0.002 * RATE)));
        assert_eq!(c.realigns(), 0);
        let after = trace.iter().filter(|s| s.time > 30.0);
        assert!(after.clone().all(|s| !s.steer.is_discontinuous()));
        let relock = trace.iter().filter(|s| s.time > 30.0).find(|s| s.locked).unwrap();
        assert!(relock.time < 40.0, "relocked at {:.1} s", relock.time);
    }

    #[test]
    fn clock_outage_keeps_the_frequency_estimate() {
        let (mut c, _) = simulate(-900.0, 256, 0.3e-3, 60.0, 3, None);
        assert!(c.locked());
        let ratio = c.ratio();
        // The media clock is gone for a while: the stream resets at every
        // callback.
        for _ in 0..100 {
            c.reset();
        }
        assert!(!c.running());
        assert_eq!(c.realigns(), 1);
        // The integral (the frequency estimate) survives; only the
        // proportional part of the correction goes.
        assert!((ppm(c.ratio()) - ppm(ratio)).abs() < 10.0, "{} vs {ratio}", c.ratio());
        assert!(ppm(c.ratio()) > 850.0, "{}", ppm(c.ratio()));
    }

    #[test]
    fn settles_before_running() {
        let mut c = DriftController::new(RATE, limits(4800));
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
