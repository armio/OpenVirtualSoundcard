//! The device timeline: the sample clock the driver reports to Core Audio
//! through its zero time stamps (design section 7.4).
//!
//! Core Audio extrapolates a device's sample clock from the (sample time,
//! host time, seed) triples that `GetZeroTimeStamp` returns, one every
//! `period` frames, and re-anchors, which is audible, when consecutive
//! triples disagree too much. The daemon's media clock, on the other hand,
//! can jump: the PTP servo locks or steps, the master changes, the daemon
//! restarts. So the device does not report the media clock directly. It runs
//! its own continuous model of device frames against host time,
//!
//! ```text
//! T(h) = t_whole + t_frac + (h - h_a) * rho
//! ```
//!
//! and device frame `t` lives at ring index `t + off`. The model follows the
//! daemon's clock smoothly: its rate `rho` moves towards the daemon's rate by
//! at most `ramp_ppm_per_s` per second, plus a phase correction of at most
//! `slew_max_ppm`. A discontinuity of the daemon's clock, seen as a new
//! `(generation, step_gen)` key, leaves `T` alone and moves the integer
//! offset `off` by the whole frames of the error instead: the audio content
//! slips once, but the timeline the HAL sees does not move. Only a rate jump
//! beyond `seed_rate_jump_ppm`, a reset (sample-rate change) or an IO stall
//! gives the HAL a new timeline, that is, a new seed.
//!
//! Absolute times are u64 nanoseconds and only differences are f64. Media
//! positions come from exact integer maths. Nothing here allocates, locks or
//! panics, so the driver can call it on its real-time threads.

#![forbid(unsafe_code)]
#![deny(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects
)]

use crate::clock::ClockRecord;
use crate::time::{ClockSnapshot, ClockState, NANOS_PER_SEC};

/// Nanoseconds per second, as a float.
const NS_PER_SEC: f64 = NANOS_PER_SEC as f64;
/// Nanoseconds per second, for exact media positions.
const NS_PER_SEC_U128: u128 = NANOS_PER_SEC as u128;
/// The sample rate of a timeline that was never reset.
const INITIAL_RATE: u32 = 48_000;
/// The widest band around nominal the device rate may ever use, whatever
/// the parameters say, so that it stays positive.
const MAX_RATE_DEV: f64 = 0.5;

/// Tuning of a [`DeviceTimeline`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimelineParams {
    /// Frames between two zero time stamps (ZeroTimeStampPeriod).
    pub period: u32,
    /// A discontinuity whose error reaches this many thousandths of a frame
    /// moves the ring offset.
    pub absorb_threshold_millisamples: u32,
    /// Fastest change of the device rate, ppm per second.
    pub ramp_ppm_per_s: u32,
    /// Time constant of the phase correction, milliseconds.
    pub slew_tau_ms: u32,
    /// Largest phase correction, ppm.
    pub slew_max_ppm: u32,
    /// A rate change larger than this at a discontinuity starts a new
    /// timeline (seed) instead of ramping, ppm.
    pub seed_rate_jump_ppm: u32,
    /// Largest deviation from nominal of the device rate, and of a daemon
    /// rate worth following, ppm.
    pub max_rate_dev_ppm: u32,
    /// A daemon heartbeat further than this from now is stale, nanoseconds.
    pub stale_ns: u64,
    /// Audio flows only while the daemon's clock and the ring mapping agree
    /// to within this, nanoseconds.
    pub coherence_ns: u64,
    /// Zero time stamps that fall behind catch up one period per call; when
    /// they are further behind than this many periods, the timeline jumps
    /// to the present with a new seed.
    pub max_catch_up_periods: u32,
    /// For tests of the HAL's tolerance only: added to the host time of
    /// even periods and subtracted from odd ones, nanoseconds.
    pub debug_zts_jitter_ns: u32,
}

impl TimelineParams {
    /// The tuning of design section 7.4, checked against the PTP servo in
    /// simulation.
    pub const DEFAULT: TimelineParams = TimelineParams {
        period: 16384,
        absorb_threshold_millisamples: 500,
        ramp_ppm_per_s: 200,
        slew_tau_ms: 2000,
        slew_max_ppm: 20,
        seed_rate_jump_ppm: 200,
        max_rate_dev_ppm: 1000,
        stale_ns: 1_000_000_000,
        coherence_ns: 250_000,
        max_catch_up_periods: 4,
        debug_zts_jitter_ns: 0,
    };
}

impl Default for TimelineParams {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// What the driver read from the daemon's region for one update.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClockInput {
    /// The last good record of the clock block.
    pub record: ClockRecord,
    /// The daemon's generation, which changes with every daemon process.
    pub generation: u64,
    /// The daemon's last heartbeat, host nanoseconds.
    pub heartbeat_ns: u64,
    /// The sample rate the daemon's engine runs at.
    pub daemon_rate: u32,
    /// Whether the daemon's engine is running.
    pub engine_running: bool,
}

/// What the timeline currently follows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Regime {
    /// Its own clock at the nominal rate: no daemon clock was followed since
    /// the last reset.
    Synthetic,
    /// The daemon's clock.
    Following,
    /// Nothing: it extrapolates the clock it last followed.
    Holdover,
}

impl Regime {
    /// The regime's code in the plug-in status block (`REGIME_*`).
    pub const fn code(self) -> u64 {
        match self {
            Regime::Synthetic => 0,
            Regime::Following => 1,
            Regime::Holdover => 2,
        }
    }
}

/// A zero time stamp: device frame `sample_time` happened at host time
/// `host_ns`, on the timeline `seed`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Zts {
    pub sample_time: u64,
    pub host_ns: u64,
    pub seed: u64,
}

/// Diagnostics of a [`DeviceTimeline`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimelineStatus {
    pub regime: Regime,
    /// Discontinuities that moved the ring offset.
    pub absorbs: u64,
    /// Seed changes since creation. The first reset keeps seed 1.
    pub seed_bumps: u64,
    pub seed: u64,
    /// Ring index minus device sample time.
    pub media_offset: i64,
    /// Where the daemon's clock was ahead of the device at the last update
    /// that followed it, nanoseconds.
    pub phase_error_ns: i64,
    /// Device rate relative to nominal, thousandths of a ppm.
    pub device_rate_ppm_milli: i64,
}

/// The device's sample clock: `T(h) = t_whole + t_frac + (h - h_a) * rho`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeviceModel {
    /// Host time of the anchor, nanoseconds.
    pub h_a: u64,
    /// Whole device frames at the anchor.
    pub t_whole: i64,
    /// Fraction of a frame at the anchor, in [0, 1).
    pub t_frac: f64,
    /// Device frames per host nanosecond.
    pub rho: f64,
}

impl DeviceModel {
    /// The device time at host time `now_ns`, as whole frames and a
    /// fraction in [0, 1).
    pub fn device_time_at(&self, now_ns: u64) -> (i64, f64) {
        let dh = i128::from(now_ns).wrapping_sub(i128::from(self.h_a)) as f64;
        let (w, f) = split(self.t_frac + dh * self.rho);
        (self.t_whole.saturating_add(w), f)
    }

    /// The host time at which the device reaches frame `sample`.
    fn host_ns_of(&self, sample: u64) -> i128 {
        let frames = i128::from(sample).wrapping_sub(i128::from(self.t_whole)) as f64 - self.t_frac;
        i128::from(self.h_a).saturating_add(i128::from(round_to_i64(frames / self.rho)))
    }
}

/// The last zero time stamp reported, and its period number.
#[derive(Clone, Copy, Debug)]
struct Cache {
    k: u64,
    zts: Zts,
}

/// The device timeline. See the [module documentation](self).
///
/// The driver calls [`reset`](Self::reset) at Initialize and on every
/// sample-rate change, [`start`](Self::start) when IO starts, and
/// [`update`](Self::update), [`zero_timestamp`](Self::zero_timestamp) and
/// [`gate`](Self::gate) on every GetZeroTimeStamp.
#[derive(Clone, Debug)]
pub struct DeviceTimeline {
    params: TimelineParams,
    fs: u32,
    model: DeviceModel,
    /// Ring index minus device frame.
    off: i64,
    regime: Regime,
    /// `(generation, step_gen)` of the daemon clock being followed.
    key: Option<(u64, u64)>,
    /// `(generation, step_gen)` of the daemon clock the timeline last
    /// followed: what it continues in holdover. Kept when IO starts again,
    /// cleared on reset.
    followed: Option<(u64, u64)>,
    last_update_ns: u64,
    seed: u64,
    resets: u64,
    cache: Option<Cache>,
    absorbs: u64,
    seed_bumps: u64,
    /// The error at the last update that followed the daemon, frames.
    phase_error: f64,
}

impl DeviceTimeline {
    /// A timeline at 48 kHz anchored at host time 0, with seed 1. The driver
    /// resets it before use.
    pub const fn new(params: TimelineParams) -> Self {
        Self {
            params,
            fs: INITIAL_RATE,
            model: DeviceModel {
                h_a: 0,
                t_whole: 0,
                t_frac: 0.0,
                rho: INITIAL_RATE as f64 / NS_PER_SEC,
            },
            off: 0,
            regime: Regime::Synthetic,
            key: None,
            followed: None,
            last_update_ns: 0,
            seed: 1,
            resets: 0,
            cache: None,
            absorbs: 0,
            seed_bumps: 0,
            phase_error: 0.0,
        }
    }

    /// Restarts the timeline at `sample_rate` from frame 0 at `now_ns`, on
    /// its own clock. Every reset but the first changes the seed.
    pub fn reset(&mut self, sample_rate: u32, now_ns: u64) {
        if self.resets > 0 {
            self.bump_seed();
        }
        self.resets = self.resets.saturating_add(1);
        self.fs = sample_rate.max(1);
        self.model = DeviceModel { h_a: now_ns, t_whole: 0, t_frac: 0.0, rho: self.nominal() };
        self.off = 0;
        self.regime = Regime::Synthetic;
        self.key = None;
        self.followed = None;
        self.cache = None;
        self.last_update_ns = now_ns;
        self.phase_error = 0.0;
    }

    /// IO starts: updates as at a discontinuity, then restarts the device
    /// time near frame 0 with the same seed (as Apple's NullAudio does). The
    /// ring offset absorbs the restart, so the ring mapping does not change.
    ///
    /// While IO is stopped the model is updated rarely if at all, so it may
    /// have drifted from the daemon's clock by however much their rates
    /// differed, for as long as it went without an update. Only the slew could remove that error while IO
    /// runs, and the gate would stay closed until it had. No client hears
    /// a slip before IO starts, so the whole error goes into the offset
    /// here instead, just as for a new daemon clock.
    pub fn start(&mut self, now_ns: u64, input: Option<&ClockInput>) {
        self.key = None;
        self.update(now_ns, input);
        let (w, f) = self.model.device_time_at(now_ns);
        self.off = self.off.saturating_add(w);
        self.model.h_a = now_ns;
        self.model.t_whole = 0;
        self.model.t_frac = f;
        self.cache = None;
    }

    /// Follows the daemon's clock, if `input` is worth following; otherwise
    /// keeps extrapolating at the current rate.
    pub fn update(&mut self, now_ns: u64, input: Option<&ClockInput>) {
        let Some(i) = input.filter(|i| self.usable(now_ns, i)) else {
            if self.regime == Regime::Following {
                self.regime = Regime::Holdover;
            }
            self.reanchor(now_ns);
            self.last_update_ns = now_ns;
            return;
        };
        let s = i.record.snapshot;
        let key = (i.generation, i.record.step_gen);
        let nominal = self.nominal();
        let rho_m = nominal * s.rate;
        if self.regime != Regime::Following || self.key != Some(key) {
            // A discontinuity. T stays continuous; the content slips once.
            let e0 = self.error(&s, now_ns);
            let threshold = f64::from(self.params.absorb_threshold_millisamples) * 1e-3;
            let x = e0.frames();
            if x >= threshold || x <= -threshold {
                let slip = e0.rounded();
                if slip != 0 {
                    self.off = self.off.saturating_add(slip);
                    self.absorbs = self.absorbs.saturating_add(1);
                }
            }
            if !within(rho_m / self.model.rho - 1.0, ppm(self.params.seed_rate_jump_ppm)) {
                self.reanchor(now_ns);
                self.model.rho = rho_m;
                self.bump_seed();
            }
            self.key = Some(key);
            self.followed = Some(key);
            self.regime = Regime::Following;
        }
        self.reanchor(now_ns);
        let e = self.error(&s, now_ns).frames();
        self.phase_error = e;
        let tau_frames = f64::from(self.params.slew_tau_ms) * 1e-3 * f64::from(self.fs);
        let corr = clamp_sym(e / tau_frames, ppm(self.params.slew_max_ppm));
        let target = rho_m * (1.0 + corr);
        let dt = now_ns.saturating_sub(self.last_update_ns) as f64 / NS_PER_SEC;
        let step = ppm(self.params.ramp_ppm_per_s) * dt * nominal;
        let rho = self.model.rho + clamp_sym(target - self.model.rho, step);
        let dev = self.max_rate_dev();
        self.model.rho = clamp(rho, nominal * (1.0 - dev), nominal * (1.0 + dev));
        self.last_update_ns = now_ns;
    }

    /// The zero time stamp to report at `now_ns`.
    ///
    /// Sample times are multiples of the period. With an unchanged seed they
    /// advance by exactly one period, at most once per call, and their host
    /// times strictly increase and are never later than `now_ns`. A reported
    /// time stamp is reported again, unchanged, until the next one is due.
    pub fn zero_timestamp(&mut self, now_ns: u64) -> Zts {
        let period = u64::from(self.params.period.max(1));
        let (t, _) = self.model.device_time_at(now_ns);
        let k_now = u64::try_from(t).unwrap_or(0).checked_div(period).unwrap_or(0);
        let k = match self.cache {
            Some(c) if k_now <= c.k => return c.zts,
            Some(c) if k_now.saturating_sub(c.k) > u64::from(self.params.max_catch_up_periods) => {
                // Too far behind to catch up: jump to the present.
                self.bump_seed();
                k_now
            }
            Some(c) => c.k.saturating_add(1),
            None => k_now,
        };
        let sample_time = k.saturating_mul(period);
        let mut h = self.model.host_ns_of(sample_time).saturating_add(self.jitter(k));
        match self.cache {
            Some(c) => {
                h = h.max(i128::from(c.zts.host_ns).saturating_add(1));
                if h > i128::from(now_ns) {
                    // Not due yet; report it on a later call.
                    return c.zts;
                }
            }
            None => h = h.min(i128::from(now_ns)).max(0),
        }
        let zts = Zts { sample_time, host_ns: u64::try_from(h).unwrap_or(0), seed: self.seed };
        self.cache = Some(Cache { k, zts });
        zts
    }

    /// Whether audio may flow: the daemon is alive, running at this rate,
    /// its clock is the one the timeline follows or continues in holdover,
    /// and it agrees with the ring mapping to within `coherence_ns`.
    ///
    /// Another daemon's clock, or this one's after a step, stays shut out
    /// until the timeline follows it, however close it is: a clock still
    /// locking may step when it locks, and the content would slip under
    /// audio already flowing.
    pub fn gate(&self, now_ns: u64, input: Option<&ClockInput>) -> bool {
        let Some(i) = input else {
            return false;
        };
        let coherence = self.params.coherence_ns as f64 * f64::from(self.fs) / NS_PER_SEC;
        self.regime != Regime::Synthetic
            && self.followed == Some((i.generation, i.record.step_gen))
            && i.record.valid
            && self.sane_rate(i.record.snapshot.rate)
            && self.alive(now_ns, i)
            && self.error(&i.record.snapshot, now_ns).within(coherence)
    }

    /// The device time at `now_ns`, as whole frames and a fraction in [0, 1).
    pub fn device_time_at(&self, now_ns: u64) -> (i64, f64) {
        self.model.device_time_at(now_ns)
    }

    /// Ring index minus device sample time.
    pub fn media_offset(&self) -> i64 {
        self.off
    }

    /// The current device model, for a copy the IO path can extrapolate.
    pub fn model(&self) -> DeviceModel {
        self.model
    }

    /// The nominal sample rate of the last reset.
    pub fn sample_rate(&self) -> u32 {
        self.fs
    }

    /// The tuning.
    pub fn params(&self) -> TimelineParams {
        self.params
    }

    /// Changes the tuning (the driver's configuration carries
    /// `debug_zts_jitter_ns`). The state, seed included, is kept; the new
    /// values apply from the next update and zero time stamp.
    pub fn set_params(&mut self, params: TimelineParams) {
        self.params = params;
    }

    /// Diagnostics for the plug-in status block.
    pub fn status(&self) -> TimelineStatus {
        TimelineStatus {
            regime: self.regime,
            absorbs: self.absorbs,
            seed_bumps: self.seed_bumps,
            seed: self.seed,
            media_offset: self.off,
            phase_error_ns: round_to_i64(self.phase_error * NS_PER_SEC / f64::from(self.fs)),
            device_rate_ppm_milli: round_to_i64((self.model.rho / self.nominal() - 1.0) * 1e9),
        }
    }

    /// Device frames per host nanosecond at the nominal rate.
    fn nominal(&self) -> f64 {
        f64::from(self.fs) / NS_PER_SEC
    }

    fn max_rate_dev(&self) -> f64 {
        ppm(self.params.max_rate_dev_ppm).min(MAX_RATE_DEV)
    }

    fn sane_rate(&self, rate: f64) -> bool {
        within(rate - 1.0, self.max_rate_dev())
    }

    /// Whether the daemon is alive and its engine runs at our rate.
    fn alive(&self, now_ns: u64, i: &ClockInput) -> bool {
        i.engine_running
            && i.daemon_rate == self.fs
            && now_ns.abs_diff(i.heartbeat_ns) <= self.params.stale_ns
    }

    /// Whether the daemon's clock is worth following.
    fn usable(&self, now_ns: u64, i: &ClockInput) -> bool {
        let r = &i.record;
        r.valid
            && matches!(r.state, ClockState::Locked | ClockState::FreeRunning)
            && self.sane_rate(r.snapshot.rate)
            && self.alive(now_ns, i)
    }

    /// `e(s, h)`: how far the daemon's clock `s` is ahead of the device at
    /// host time `h`, in frames: `M(s, h) - off - T(h)`.
    fn error(&self, s: &ClockSnapshot, h: u64) -> Phase {
        let (mw, mf) = media_position(s, h, self.fs);
        let (tw, tf) = self.model.device_time_at(h);
        Phase::new(mw.saturating_sub(self.off).saturating_sub(tw), mf - tf)
    }

    /// Moves the anchor to `now_ns`, leaving `T` unchanged.
    fn reanchor(&mut self, now_ns: u64) {
        let (w, f) = self.model.device_time_at(now_ns);
        self.model.h_a = now_ns;
        self.model.t_whole = w;
        self.model.t_frac = f;
    }

    fn bump_seed(&mut self) {
        self.seed = self.seed.wrapping_add(1).max(1);
        self.seed_bumps = self.seed_bumps.saturating_add(1);
        self.cache = None;
    }

    fn jitter(&self, k: u64) -> i128 {
        let j = i128::from(self.params.debug_zts_jitter_ns);
        if k & 1 == 0 { j } else { j.wrapping_neg() }
    }
}

impl Default for DeviceTimeline {
    fn default() -> Self {
        Self::new(TimelineParams::DEFAULT)
    }
}

/// A phase error in frames, `whole + frac`, with `frac` in (-1, 1) and of
/// the same sign as `whole`.
#[derive(Clone, Copy, Debug)]
struct Phase {
    whole: i64,
    frac: f64,
}

impl Phase {
    /// `whole + frac` for `frac` in (-1, 1).
    fn new(whole: i64, frac: f64) -> Self {
        if whole > 0 && frac < 0.0 {
            Phase { whole: whole.wrapping_sub(1), frac: frac + 1.0 }
        } else if whole < 0 && frac > 0.0 {
            Phase { whole: whole.wrapping_add(1), frac: frac - 1.0 }
        } else {
            Phase { whole, frac }
        }
    }

    fn frames(self) -> f64 {
        self.whole as f64 + self.frac
    }

    /// The nearest whole frame, halves away from zero.
    fn rounded(self) -> i64 {
        let r = if self.frac >= 0.5 {
            1
        } else if self.frac <= -0.5 {
            -1
        } else {
            0
        };
        self.whole.saturating_add(r)
    }

    fn within(self, limit: f64) -> bool {
        within(self.frames(), limit)
    }
}

/// `M(s, h)`: the media sample position of `s` at host time `h`, as whole
/// frames and a fraction in [0, 1), from exact integer maths.
fn media_position(s: &ClockSnapshot, h: u64, fs: u32) -> (i64, f64) {
    let p = u128::from(s.media_ns_at(h)).saturating_mul(u128::from(fs));
    let whole = i64::try_from(p / NS_PER_SEC_U128).unwrap_or(i64::MAX);
    let frac = (p % NS_PER_SEC_U128) as f64 / NS_PER_SEC;
    (whole, frac)
}

/// Splits `x` into its floor and a fraction in [0, 1). Saturates, and NaN
/// gives 0.
fn split(x: f64) -> (i64, f64) {
    // `as` truncates towards zero, saturates and maps NaN to 0.
    let mut w = x as i64;
    if w as f64 > x {
        w = w.saturating_sub(1);
    }
    let f = x - w as f64;
    if f >= 1.0 {
        // Rounding of a tiny negative x.
        (w.saturating_add(1), 0.0)
    } else if f >= 0.0 {
        (w, f)
    } else {
        (w, 0.0)
    }
}

/// The nearest integer, halves away from zero. Saturates, and NaN gives 0.
fn round_to_i64(x: f64) -> i64 {
    if x >= 0.0 { (x + 0.5) as i64 } else { (x - 0.5) as i64 }
}

/// `x` limited to [-limit, limit]; NaN gives 0.
fn clamp_sym(x: f64, limit: f64) -> f64 {
    clamp(x, -limit, limit)
}

/// `x` limited to [lo, hi]; NaN gives the middle. Never panics.
fn clamp(x: f64, lo: f64, hi: f64) -> f64 {
    if x > hi {
        hi
    } else if x < lo {
        lo
    } else if x.is_nan() {
        (lo + hi) * 0.5
    } else {
        x
    }
}

/// Whether `|x| <= limit`. False for NaN.
fn within(x: f64, limit: f64) -> bool {
    x <= limit && x >= -limit
}

fn ppm(v: u32) -> f64 {
    f64::from(v) * 1e-6
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    #[test]
    fn split_floors() {
        assert_eq!(split(0.0), (0, 0.0));
        assert_eq!(split(2.25), (2, 0.25));
        assert_eq!(split(-2.25), (-3, 0.75));
        assert_eq!(split(-3.0), (-3, 0.0));
        assert_eq!(split(-1e-300), (0, 0.0));
        assert_eq!(split(f64::NAN), (0, 0.0));
        assert_eq!(split(f64::INFINITY).0, i64::MAX);
        assert_eq!(split(f64::NEG_INFINITY).0, i64::MIN);
        for x in [1e-17, 0.999_999_999_999_999_9, 1e15 + 0.5, -1e15 - 0.5] {
            let (w, f) = split(x);
            assert!((0.0..1.0).contains(&f), "{x}: {f}");
            assert_eq!(w as f64 + f, x);
        }
    }

    #[test]
    fn rounding() {
        assert_eq!(round_to_i64(0.49), 0);
        assert_eq!(round_to_i64(0.5), 1);
        assert_eq!(round_to_i64(-0.5), -1);
        assert_eq!(round_to_i64(-2.4), -2);
        assert_eq!(round_to_i64(f64::NAN), 0);
        assert_eq!(round_to_i64(1e300), i64::MAX);
        let p = |w, f| Phase::new(w, f);
        assert_eq!(p(1, -0.5).rounded(), 1);
        assert_eq!(p(1, -0.7).rounded(), 0);
        assert_eq!(p(-1, 0.5).rounded(), -1);
        assert_eq!(p(-1, 0.2).rounded(), -1);
        assert_eq!(p(0, -0.5).rounded(), -1);
        assert_eq!(p(480, 0.25).rounded(), 480);
        assert_eq!(p(3, -0.25).frames(), 2.75);
    }

    #[test]
    fn clamps_never_pass_nan() {
        assert_eq!(clamp_sym(f64::NAN, 1.0), 0.0);
        assert_eq!(clamp_sym(f64::INFINITY, 1.0), 1.0);
        assert_eq!(clamp_sym(-5.0, 1.0), -1.0);
        assert_eq!(clamp(f64::NAN, 1.0, 3.0), 2.0);
        assert!(!within(f64::NAN, 1.0));
        assert!(within(-1.0, 1.0));
    }

    #[test]
    fn media_positions_are_exact() {
        let s =
            ClockSnapshot { local_ref_ns: 0, media_ref_ns: 1_700_000_000_000_020_833, rate: 1.0 };
        let (w, f) = media_position(&s, 0, 48_000);
        assert_eq!(w, 81_600_000_000_000);
        assert!(f > 0.999_983_999 && f < 0.999_984_001, "{f}");
        let max = ClockSnapshot { local_ref_ns: 0, media_ref_ns: u64::MAX, rate: 1.0 };
        assert_eq!(media_position(&max, 0, u32::MAX).0, i64::MAX);
    }

    #[test]
    fn regime_codes_match_the_status_block() {
        #[cfg(target_has_atomic = "64")]
        {
            use crate::status::{REGIME_FOLLOWING, REGIME_HOLDOVER, REGIME_SYNTHETIC};
            assert_eq!(Regime::Synthetic.code(), REGIME_SYNTHETIC);
            assert_eq!(Regime::Following.code(), REGIME_FOLLOWING);
            assert_eq!(Regime::Holdover.code(), REGIME_HOLDOVER);
        }
    }
}
