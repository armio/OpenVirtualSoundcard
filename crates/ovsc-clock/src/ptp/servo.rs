//! Clock servo: turns PTP timestamps into a continuous media clock.
//!
//! The servo is pure and deterministic. It never reads a clock or touches
//! the network; all times are passed in. That makes it possible to test it
//! against simulated masters with arbitrary drift and jitter.
//!
//! # Terms
//!
//! * `t1`: master time when a Sync left the master (from the Sync itself, or
//!   from its Follow_Up for two-step masters).
//! * `t2`: local time ([`crate::local_now_ns`]) when we received that Sync.
//! * `t3`: local time when we sent a Delay_Req.
//! * `t4`: master time when the master received it (from the Delay_Resp).
//! * `d`: the one-way path delay, assumed symmetric.
//!
//! Every Sync tells us that at local time `t2` the master's clock read
//! `t1 + d`. The servo's job is to keep a published [`ClockSnapshot`], a
//! line `media = media_ref + (local - local_ref) * rate`, as close as
//! possible to those measurements *without ever jumping*, except when the
//! error is too large to slew away (initial lock, master change, or a jump of
//! the master's clock).
//!
//! # Why the filtering looks like this
//!
//! With software timestamps the dominant error is not network jitter but
//! scheduling: `t2` is read after the kernel has delivered the packet and
//! the task has been woken up, which adds anywhere from a few microseconds to
//! milliseconds. That error is one-sided: a timestamp can be late, never
//! early. So instead of averaging (which would turn the mean latency into a
//! bias and let outliers through), the servo keeps the *least delayed*
//! measurements ("lucky packets"):
//!
//! * The Sync measurements of a short recent window are each projected to
//!   the newest sample's local time using the frequency estimate, and the
//!   largest projection is taken. Each projection is a lower bound of the
//!   master's time (a late `t2` makes it smaller), so the maximum is the
//!   tightest one, and outliers are discarded automatically.
//! * The path delay is the minimum over a window of recent measurements,
//!   each paired with that lucky estimate rather than with a single Sync
//!   (see [`Servo::delay_measurement`]).
//! * Frequency fits are robust (Theil–Sen) and then refined on the less
//!   delayed half of the points (see `estimate_drift`).
//!
//! # Control loop
//!
//! The filtered error `e = estimate - snapshot(t2)` (master minus us) is
//! corrected by changing the snapshot's rate:
//!
//! ```text
//! rate = freq + kp * e
//! ```
//!
//! where `freq` is the frequency estimate. After every update the snapshot
//! is re-anchored at the current local time on the media time it already
//! shows and only its rate changes, so the published clock is continuous
//! and, because the rate stays within `1 ± max_freq_offset`, strictly
//! increasing. A constant error is slewed away with time constant `1 / kp`.
//! Errors beyond the step threshold (confirmed by consecutive samples) are
//! stepped instead.
//!
//! Where `freq` comes from depends on how far along the servo is:
//!
//! 1. **Acquiring**: Syncs are collected for
//!    [`ServoConfig::acquire_min_span_ns`]; then the frequency is fitted by
//!    least squares and the clock is *stepped* to the lucky estimate.
//! 2. **Frequency-locked loop** (all of Locking and the start of Locked): a
//!    fit over a couple of seconds of jittery timestamps is only good to
//!    tens of ppm, so the frequency keeps being re-fitted over all samples
//!    since the step. Its error shrinks roughly as `T^-1.5` with the
//!    baseline `T`: much faster than an integrator would learn it, and
//!    without the windup an integrator suffers after a bad start. The
//!    proportional gain is wide here so that the remaining frequency error
//!    costs little phase.
//! 3. **Phase-locked loop**: once locked and the fit spans
//!    [`ServoConfig::fll_handover_span_ns`], a PI controller takes over to
//!    track slow changes of the master's frequency (temperature, ageing):
//!
//!    ```text
//!    freq <- freq + ki * e * dt          (integral term)
//!    rate  = freq + kp * e               (proportional term)
//!    ```
//!
//!    This is a classic type-2 PLL: with `kp = 2ζωn` and `ki = ωn²` it has
//!    natural frequency `ωn` and damping `ζ`. Its narrow default bandwidth
//!    keeps the timestamp noise out of the clock.
//!
//! The servo reports Locked once `|e|` has stayed below
//! [`ServoConfig::lock_threshold_ns`] for [`ServoConfig::lock_samples`]
//! updates.

use std::collections::VecDeque;

use crate::ClockSnapshot;

/// Gains of the PI controller.
///
/// `kp` is in 1/s (rate correction per second of phase error), `ki` in 1/s².
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PiGains {
    pub kp: f64,
    pub ki: f64,
}

impl PiGains {
    /// Gains for a loop with natural frequency `omega_n` (rad/s) and damping
    /// `zeta`.
    pub fn from_loop(omega_n: f64, zeta: f64) -> PiGains {
        PiGains { kp: 2.0 * zeta * omega_n, ki: omega_n * omega_n }
    }
}

/// Tuning of the [`Servo`].
#[derive(Clone, Debug, PartialEq)]
pub struct ServoConfig {
    /// Errors larger than this are corrected by stepping the clock instead
    /// of slewing it.
    pub step_threshold_ns: i64,
    /// How many consecutive updates must exceed the step threshold before
    /// the clock is stepped (guards against a single bad sample).
    pub step_confirmations: u32,
    /// The servo is locked once the error stays below this...
    pub lock_threshold_ns: i64,
    /// ...for this many consecutive updates.
    pub lock_samples: u32,
    /// A locked servo falls back to locking when the error exceeds this...
    pub unlock_threshold_ns: i64,
    /// ...for this many consecutive updates.
    pub unlock_samples: u32,
    /// Largest allowed `|rate - 1|`. Real oscillators are within ±100 ppm.
    pub max_freq_offset: f64,
    /// Minimum number of Sync samples before the initial step.
    pub acquire_min_samples: usize,
    /// Minimum time spanned by those samples, so the initial frequency
    /// estimate has a usable baseline.
    pub acquire_min_span_ns: u64,
    /// Path delay measurements needed before the initial step, so that a
    /// single bad measurement cannot offset the clock...
    pub acquire_min_delay_samples: usize,
    /// ...unless the acquisition samples already span this long (the master
    /// may not answer Delay_Req at all).
    pub acquire_max_span_ns: u64,
    /// The frequency fit (FLL) uses at most this much history.
    pub fll_max_span_ns: u64,
    /// Once locked, the FLL hands over to the PI controller when its fit
    /// spans this long (by then it is accurate to well below a ppm).
    pub fll_handover_span_ns: u64,
    /// Samples older than this (relative to the newest) are not used by the
    /// lucky-packet filter under the PI controller.
    pub lucky_window_ns: u64,
    /// The same while the FLL runs. It must be short: projecting a sample
    /// with a wrong frequency estimate errs by `frequency error x age`, and
    /// when the estimate is too high the oldest sample always looks best,
    /// hiding the very error the loop needs to see.
    pub locking_lucky_window_ns: u64,
    /// Number of path delay measurements the minimum is taken over.
    pub delay_window: usize,
    /// Until this many path delay measurements exist, the follower should
    /// send Delay_Reqs faster than its configured interval (see
    /// [`Servo::wants_fast_delay_requests`]): early delay estimates set the
    /// initial phase, and a minimum over few samples is still biased.
    pub fast_delay_samples: usize,
    /// Proportional gain (1/s) while the FLL runs.
    pub locking_kp: f64,
    /// Gains of the PI controller that takes over from the FLL.
    pub locked_gains: PiGains,
    /// When locked, the integral (frequency) term only learns from errors
    /// smaller than this. Larger errors are transients (a changed path
    /// delay, a disturbance) that the proportional term slews away;
    /// integrating them would wind the frequency estimate up.
    pub integrate_below_ns: i64,
}

impl Default for ServoConfig {
    fn default() -> Self {
        ServoConfig {
            step_threshold_ns: 2_000_000,
            step_confirmations: 2,
            lock_threshold_ns: 50_000,
            lock_samples: 8,
            unlock_threshold_ns: 250_000,
            unlock_samples: 4,
            max_freq_offset: 500e-6,
            acquire_min_samples: 4,
            acquire_min_span_ns: 2_000_000_000,
            acquire_min_delay_samples: 2,
            acquire_max_span_ns: 6_000_000_000,
            fll_max_span_ns: 30_000_000_000,
            fll_handover_span_ns: 20_000_000_000,
            lucky_window_ns: 2_000_000_000,
            locking_lucky_window_ns: 1_000_000_000,
            delay_window: 32,
            fast_delay_samples: 8,
            locking_kp: 0.7,
            locked_gains: PiGains::from_loop(0.07, 1.0),
            integrate_below_ns: 250_000,
        }
    }
}

/// Coarse state of the servo.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ServoState {
    /// Collecting samples for the initial estimate; no clock yet.
    #[default]
    Acquiring,
    /// The clock is set and converging.
    Locking,
    /// The error has stayed within the lock threshold.
    Locked,
}

/// What a Sync sample did to the clock.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ServoEvent {
    /// Still acquiring; there is no clock to publish yet.
    Collecting,
    /// The clock was set. `offset_ns` is the error that was removed, or
    /// `None` for the initial step.
    Stepped { offset_ns: Option<i64> },
    /// The clock is being slewed; `offset_ns` is the filtered error
    /// (master minus us).
    Slewed { offset_ns: i64 },
    /// The sample was not used: it was out of order, or the error exceeded
    /// the step threshold but has not been confirmed yet. The clock was
    /// left alone.
    Held { offset_ns: i64 },
}

/// One Sync measurement.
#[derive(Clone, Copy, Debug)]
struct Sample {
    /// Master time of transmission.
    t1: u64,
    /// Local time of reception.
    t2: u64,
}

/// One Delay_Req/Delay_Resp exchange, with the Sync that preceded it.
#[derive(Clone, Copy, Debug)]
struct DelayExchange {
    t1: u64,
    t2: u64,
    t3: u64,
    t4: u64,
}

/// Longest interval the integral term will integrate over in one update, so
/// that a gap in Sync messages cannot wind the frequency up.
const MAX_DT_S: f64 = 2.0;

/// Acquisition and frequency fits keep at most this many samples (the
/// newest).
const MAX_FIT_SAMPLES: usize = 256;

/// Raw path delays outside this range are treated as garbage.
const DELAY_SANITY_NS: std::ops::RangeInclusive<i64> = -1_000_000..=1_000_000_000;

/// The clock servo. See the [module documentation](self).
#[derive(Clone, Debug)]
pub struct Servo {
    config: ServoConfig,
    state: ServoState,
    /// Whether the frequency comes from the fit (FLL) rather than from the
    /// PI controller's integral.
    fll: bool,
    /// Samples for frequency fitting: everything since acquisition started
    /// (while acquiring) or since the last step (while the FLL runs).
    fit: VecDeque<Sample>,
    /// Recent samples for the lucky-packet filter.
    window: VecDeque<Sample>,
    /// Recent delay exchanges and the raw path delay computed from each.
    delays: VecDeque<(DelayExchange, i64)>,
    /// The clock being published.
    snapshot: Option<ClockSnapshot>,
    /// Frequency estimate: media nanoseconds per local nanosecond.
    freq: f64,
    /// Local time of the last controller update.
    last_update_ns: u64,
    /// Last filtered error.
    offset_ns: i64,
    /// Consecutive updates within the lock threshold (while locking).
    good_samples: u32,
    /// Consecutive updates beyond the unlock threshold (while locked).
    bad_samples: u32,
    /// Consecutive updates beyond the step threshold.
    step_samples: u32,
}

impl Servo {
    pub fn new(config: ServoConfig) -> Servo {
        Servo {
            config,
            state: ServoState::Acquiring,
            fll: true,
            fit: VecDeque::new(),
            window: VecDeque::new(),
            delays: VecDeque::new(),
            snapshot: None,
            freq: 1.0,
            last_update_ns: 0,
            offset_ns: 0,
            good_samples: 0,
            bad_samples: 0,
            step_samples: 0,
        }
    }

    pub fn config(&self) -> &ServoConfig {
        &self.config
    }

    /// Forgets everything, e.g. because the master changed.
    pub fn reset(&mut self) {
        *self = Servo::new(self.config.clone());
    }

    pub fn state(&self) -> ServoState {
        self.state
    }

    /// The clock to publish, once acquired.
    pub fn snapshot(&self) -> Option<ClockSnapshot> {
        self.snapshot
    }

    /// Estimated frequency of the master relative to the local clock (media
    /// nanoseconds per local nanosecond). This excludes the transient phase
    /// correction contained in the snapshot's rate.
    pub fn frequency(&self) -> f64 {
        self.freq
    }

    /// [`frequency`](Self::frequency) as an offset in parts per billion.
    pub fn freq_offset_ppb(&self) -> f64 {
        (self.freq - 1.0) * 1e9
    }

    /// Last filtered error (master minus us), nanoseconds.
    pub fn offset_ns(&self) -> i64 {
        self.offset_ns
    }

    /// Filtered one-way path delay, once measured.
    pub fn path_delay_ns(&self) -> Option<i64> {
        self.delays.iter().map(|&(_, d)| d).min().map(|d| d.max(0))
    }

    /// Number of path delay measurements currently in the filter window.
    pub fn delay_samples(&self) -> usize {
        self.delays.len()
    }

    /// Whether more path delay measurements are urgently needed, so
    /// Delay_Reqs should be sent faster than usual (a few per second).
    pub fn wants_fast_delay_requests(&self) -> bool {
        self.delays.len() < self.config.fast_delay_samples
    }

    /// Records a delay request/response exchange and returns the raw path
    /// delay it implies.
    ///
    /// `t1`/`t2` come from the latest Sync before `t3`; `t3`/`t4` from the
    /// Delay_Req and its Delay_Resp. IEEE 1588 computes
    /// `((t2 - t1) + (t4 - t3)) / 2`, which assumes the clock offset did not
    /// change between `t2` and `t3`. With up to a second between them and a
    /// frequency offset of tens of ppm, that assumption costs tens of
    /// microseconds, so the local interval `t3 - t2` is converted to master
    /// time with the frequency estimate first:
    ///
    /// ```text
    /// t4 - t1 = 2d + freq * (t3 - t2)   =>   d = ((t4 - t1) - freq * (t3 - t2)) / 2
    /// ```
    ///
    /// That still carries the full receive jitter of that one Sync. So once
    /// there is a frequency estimate, the Sync is replaced by the lucky-packet
    /// estimate of the master's transmit time projected to `t3`, `L(t3)`,
    /// using only Syncs close to `t3` so that the frequency error hardly
    /// matters: `d = (t4 - L(t3)) / 2`. Measurements taken while acquiring
    /// are recomputed that way when the frequency becomes known.
    pub fn delay_measurement(&mut self, t1: u64, t2: u64, t3: u64, t4: u64) -> i64 {
        let exchange = DelayExchange { t1, t2, t3, t4 };
        let raw = self.exchange_delay(&exchange, self.window.iter());
        if DELAY_SANITY_NS.contains(&raw) {
            self.delays.push_back((exchange, raw));
            while self.delays.len() > self.config.delay_window.max(1) {
                self.delays.pop_front();
            }
        }
        raw
    }

    /// The path delay implied by `x`, paired with the lucky estimate from
    /// `samples` near `t3` (or, while acquiring, with its own Sync).
    fn exchange_delay<'a>(
        &self,
        x: &DelayExchange,
        samples: impl Iterator<Item = &'a Sample>,
    ) -> i64 {
        let lucky = match self.state {
            ServoState::Acquiring => None,
            _ => self.lucky_origin_near(samples, x.t3, self.lucky_window() / 2),
        };
        match lucky {
            Some(origin) => ((x.t4 as i128 - origin) as f64 / 2.0).round() as i64,
            None => {
                let master = x.t4 as i128 - x.t1 as i128;
                let local = (x.t3 as i128 - x.t2 as i128) as f64 * self.freq;
                ((master as f64 - local) / 2.0).round() as i64
            }
        }
    }

    /// Processes a Sync measurement: the master sent it at `t1` (master
    /// time) and we received it at `t2` (local time). `now_ns` is the local
    /// time at which the resulting snapshot takes effect (normally the time
    /// of the call; it must not be earlier than `t2`).
    pub fn sync(&mut self, t1: u64, t2: u64, now_ns: u64) -> ServoEvent {
        let now_ns = now_ns.max(t2);
        let sample = Sample { t1, t2 };
        if self.fit.back().is_some_and(|last| t2 <= last.t2)
            || self.window.back().is_some_and(|last| t2 <= last.t2)
        {
            // Out of order or duplicate.
            return match self.snapshot {
                None => ServoEvent::Collecting,
                Some(_) => ServoEvent::Held { offset_ns: self.offset_ns },
            };
        }
        match self.snapshot {
            None => self.acquire_sample(sample, now_ns),
            Some(snapshot) => self.track_sample(snapshot, sample, now_ns),
        }
    }

    /// Prepares for running without a master: the proportional correction
    /// is dropped so the clock free-runs at the best frequency estimate.
    /// Returns the snapshot to publish, if there is a clock.
    pub fn enter_holdover(&mut self, now_ns: u64) -> Option<ClockSnapshot> {
        let snap = self.snapshot?;
        let media = snap.media_ns_at(now_ns);
        self.set_snapshot(now_ns, media, self.freq);
        // When the master comes back, re-converge from the current estimate.
        self.restart_locking(now_ns);
        self.snapshot
    }

    fn acquire_sample(&mut self, sample: Sample, now_ns: u64) -> ServoEvent {
        self.push_fit(sample, u64::MAX);
        let span = sample.t2 - self.fit[0].t2;
        let enough_syncs = self.fit.len() >= self.config.acquire_min_samples.max(2)
            && span >= self.config.acquire_min_span_ns;
        let enough_delays = self.delays.len() >= self.config.acquire_min_delay_samples
            || span >= self.config.acquire_max_span_ns;
        if !(enough_syncs && enough_delays) {
            return ServoEvent::Collecting;
        }

        let drift = estimate_drift(self.fit.make_contiguous());
        self.freq = self.clamp_freq(1.0 + drift);
        self.state = ServoState::Locking;
        // With a frequency estimate, the delay measurements can be paired
        // with lucky Syncs instead of their own.
        let delays = self
            .delays
            .iter()
            .map(|(x, _)| (*x, self.exchange_delay(x, self.fit.iter())))
            .filter(|(_, d)| DELAY_SANITY_NS.contains(d))
            .collect();
        self.delays = delays;
        // The newest acquisition samples seed the lucky-packet filter and
        // give the initial phase; all of them stay in the frequency fit.
        self.window = self.fit.clone();
        self.trim_window(sample.t2);
        let estimate = self.lucky_estimate(sample.t2);
        self.step_to(estimate, sample.t2, now_ns);
        self.offset_ns = 0;
        ServoEvent::Stepped { offset_ns: None }
    }

    fn track_sample(&mut self, snapshot: ClockSnapshot, sample: Sample, now_ns: u64) -> ServoEvent {
        self.window.push_back(sample);
        self.trim_window(sample.t2);
        if self.fll {
            self.push_fit(sample, self.config.fll_max_span_ns);
        }

        let estimate = self.lucky_estimate(sample.t2);
        let e = clamp_i64(estimate - snapshot.media_ns_at(sample.t2) as i128);
        self.offset_ns = e;

        if e.abs() > self.config.step_threshold_ns {
            self.step_samples += 1;
            if self.step_samples < self.config.step_confirmations {
                return ServoEvent::Held { offset_ns: e };
            }
            // The master's clock jumped (or we lost track). Keep the
            // frequency, which is still the best guess, and re-converge.
            self.restart_locking(now_ns);
            self.window.retain(|s| s.t2 == sample.t2);
            self.fit.push_back(sample);
            self.step_to(estimate, sample.t2, now_ns);
            return ServoEvent::Stepped { offset_ns: Some(e) };
        }
        self.step_samples = 0;

        let e_s = e as f64 * 1e-9;
        let dt = (now_ns.saturating_sub(self.last_update_ns) as f64 * 1e-9).min(MAX_DT_S);
        let kp = if self.fll {
            // FLL: re-fit the frequency over the growing baseline.
            let span = sample.t2 - self.fit[0].t2;
            if self.fit.len() >= 3 && span >= self.config.acquire_min_span_ns {
                let drift = estimate_drift(self.fit.make_contiguous());
                self.freq = self.clamp_freq(1.0 + drift);
            }
            if self.state == ServoState::Locked && span >= self.config.fll_handover_span_ns {
                // The fit is now better than the loop could do; from here
                // on the PI controller tracks slow changes of the master.
                self.fll = false;
                self.fit.clear();
            }
            self.config.locking_kp
        } else {
            // PLL: the integral term tracks the master's frequency.
            if e.abs() < self.config.integrate_below_ns {
                self.freq = self.clamp_freq(self.freq + self.config.locked_gains.ki * e_s * dt);
            }
            self.config.locked_gains.kp
        };
        let rate = self.clamp_freq(self.freq + kp * e_s);
        // Re-anchor at `now` on the media time already being shown, so the
        // published clock never jumps.
        self.set_snapshot(now_ns, snapshot.media_ns_at(now_ns), rate);
        self.last_update_ns = now_ns;

        self.update_lock_state(e, now_ns);
        ServoEvent::Slewed { offset_ns: e }
    }

    fn update_lock_state(&mut self, e: i64, now_ns: u64) {
        let e = e.abs();
        match self.state {
            ServoState::Locking => {
                if e < self.config.lock_threshold_ns {
                    self.good_samples += 1;
                    if self.good_samples >= self.config.lock_samples {
                        // The frequency fit keeps going until its baseline
                        // is long enough to hand over to the PI controller.
                        self.state = ServoState::Locked;
                        self.bad_samples = 0;
                    }
                } else {
                    self.good_samples = 0;
                }
            }
            ServoState::Locked => {
                if e > self.config.unlock_threshold_ns {
                    self.bad_samples += 1;
                    if self.bad_samples >= self.config.unlock_samples {
                        self.restart_locking(now_ns);
                    }
                } else {
                    self.bad_samples = 0;
                }
            }
            ServoState::Acquiring => {}
        }
    }

    /// Enters the locking state with a fresh frequency fit (old samples may
    /// reflect a master that has since changed).
    fn restart_locking(&mut self, now_ns: u64) {
        self.state = ServoState::Locking;
        self.fll = true;
        self.fit.clear();
        self.last_update_ns = now_ns;
        self.good_samples = 0;
        self.bad_samples = 0;
        self.step_samples = 0;
    }

    /// Sets the clock so that it reads `estimate` at local time `at`.
    fn step_to(&mut self, estimate: i128, at: u64, now_ns: u64) {
        let media_now = estimate + (self.freq * (now_ns - at) as f64).round() as i128;
        self.set_snapshot(now_ns, clamp_u64(media_now), self.freq);
        self.last_update_ns = now_ns;
    }

    /// The tightest lower bound of the master's time at local time `t_ref`
    /// implied by the lucky window: each sample says the master read
    /// `t1 + d` at local time `t2`; projected to `t_ref` with the frequency
    /// estimate that is `t1 + d + freq * (t_ref - t2)`. A late receive
    /// timestamp can only make the projection smaller, so the largest
    /// projection is the best.
    fn lucky_estimate(&self, t_ref: u64) -> i128 {
        let delay = self.path_delay_ns().unwrap_or(0) as i128;
        self.lucky_origin_near(self.window.iter(), t_ref, u64::MAX).unwrap_or(0) + delay
    }

    /// Like [`lucky_estimate`](Self::lucky_estimate) without the path delay
    /// (the master's time at `t_ref` minus `d`), from those of `samples`
    /// received within `radius_ns` of `t_ref`.
    fn lucky_origin_near<'a>(
        &self,
        samples: impl Iterator<Item = &'a Sample>,
        t_ref: u64,
        radius_ns: u64,
    ) -> Option<i128> {
        samples
            .filter(|s| s.t2.abs_diff(t_ref) <= radius_ns)
            .map(|s| {
                let dt = (t_ref as i128 - s.t2 as i128) as f64;
                s.t1 as i128 + (self.freq * dt).round() as i128
            })
            .max()
    }

    /// Length of the lucky-packet window in the current state.
    fn lucky_window(&self) -> u64 {
        if self.fll { self.config.locking_lucky_window_ns } else { self.config.lucky_window_ns }
    }

    fn trim_window(&mut self, newest_t2: u64) {
        let horizon = newest_t2.saturating_sub(self.lucky_window());
        while self.window.len() > 1 && self.window.front().is_some_and(|s| s.t2 < horizon) {
            self.window.pop_front();
        }
    }

    /// Adds a sample to the frequency fit, keeping at most `max_span_ns`
    /// of history.
    fn push_fit(&mut self, sample: Sample, max_span_ns: u64) {
        self.fit.push_back(sample);
        let horizon = sample.t2.saturating_sub(max_span_ns);
        while self.fit.len() > MAX_FIT_SAMPLES || self.fit.front().is_some_and(|s| s.t2 < horizon) {
            self.fit.pop_front();
        }
    }

    fn set_snapshot(&mut self, local_ref_ns: u64, media_ref_ns: u64, rate: f64) {
        self.snapshot = Some(ClockSnapshot { local_ref_ns, media_ref_ns, rate });
    }

    fn clamp_freq(&self, f: f64) -> f64 {
        let m = self.config.max_freq_offset;
        f.clamp(1.0 - m, 1.0 + m)
    }
}

/// Estimates `rate - 1` from Sync samples.
///
/// The offsets `t1 - t2` over local time form a line whose slope is the
/// frequency offset, with points pulled down by late timestamps. A plain
/// least-squares fit is ruined by a single 2 ms scheduling hiccup near the
/// end of the baseline (it has the most leverage), so:
///
/// 1. a robust first line comes from the Theil–Sen estimator (median of
///    the pairwise slopes, intercept the median residual);
/// 2. only the less delayed half of the points (residual at or above the
///    median) is kept, which also drops all outliers;
/// 3. the slope is refitted to those points by least squares.
fn estimate_drift(samples: &[Sample]) -> f64 {
    let Some(first) = samples.first() else { return 0.0 };
    let base_offset = first.t1 as i128 - first.t2 as i128;
    let points: Vec<(f64, f64)> = samples
        .iter()
        .map(|s| {
            let x = (s.t2 - first.t2) as f64;
            let y = (s.t1 as i128 - s.t2 as i128 - base_offset) as f64;
            (x, y)
        })
        .collect();
    if points.len() < 5 {
        return least_squares(&points).map_or(0.0, |(_, b)| b);
    }
    let Some(b) = theil_sen_slope(&points) else { return 0.0 };
    let mut residuals: Vec<f64> = points.iter().map(|&(x, y)| y - b * x).collect();
    let median = median(&mut residuals.clone());
    let upper: Vec<(f64, f64)> = points
        .iter()
        .zip(residuals.drain(..))
        .filter(|&(_, r)| r >= median)
        .map(|(&p, _)| p)
        .collect();
    if upper.len() >= 3 { least_squares(&upper).map_or(b, |(_, b)| b) } else { b }
}

/// Median of the slopes between all pairs of points with distinct `x`.
fn theil_sen_slope(points: &[(f64, f64)]) -> Option<f64> {
    let mut slopes = Vec::with_capacity(points.len() * (points.len() - 1) / 2);
    for (i, &(x1, y1)) in points.iter().enumerate() {
        for &(x2, y2) in &points[i + 1..] {
            if x2 != x1 {
                slopes.push((y2 - y1) / (x2 - x1));
            }
        }
    }
    (!slopes.is_empty()).then(|| median(&mut slopes))
}

/// The median (upper median for even lengths). `values` is reordered.
fn median(values: &mut [f64]) -> f64 {
    let mid = values.len() / 2;
    *values.select_nth_unstable_by(mid, f64::total_cmp).1
}

/// Least-squares line `y = a + b x`; `None` if all `x` are equal.
fn least_squares(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    if points.len() < 2 {
        return None;
    }
    let n = points.len() as f64;
    let mx = points.iter().map(|p| p.0).sum::<f64>() / n;
    let my = points.iter().map(|p| p.1).sum::<f64>() / n;
    let sxx: f64 = points.iter().map(|p| (p.0 - mx) * (p.0 - mx)).sum();
    let sxy: f64 = points.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    if sxx <= 0.0 {
        return None;
    }
    let b = sxy / sxx;
    Some((my - b * mx, b))
}

fn clamp_u64(v: i128) -> u64 {
    v.clamp(0, u64::MAX as i128) as u64
}

fn clamp_i64(v: i128) -> i64 {
    v.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;
    const MEDIA0: u64 = 1_700_000_000_000_000_000;

    /// A perfect master running at `rate` relative to local time, with
    /// symmetric path delay `delay`.
    struct Ideal {
        rate: f64,
        delay: u64,
        offset: u64,
    }

    impl Ideal {
        fn media_at(&self, local: u64) -> u64 {
            self.offset + (local as f64 * self.rate) as u64
        }
        /// (t1, t2) of a Sync sent at local time `sent`.
        fn sync(&self, sent: u64) -> (u64, u64) {
            (self.media_at(sent), sent + self.delay)
        }
    }

    fn run_ideal(servo: &mut Servo, m: &Ideal, from: u64, until: u64) -> u64 {
        let mut t = from;
        while t < until {
            let (t1, t2) = m.sync(t);
            // A delay exchange 100 ms after each Sync.
            let t3 = t2 + 100 * MS;
            let t4 = m.media_at(t3 + m.delay);
            servo.delay_measurement(t1, t2, t3, t4);
            servo.sync(t1, t2, t2);
            t += 250 * MS;
        }
        t
    }

    #[test]
    fn acquires_then_locks_to_ideal_master() {
        let m = Ideal { rate: 1.0 + 40e-6, delay: 70_000, offset: MEDIA0 };
        let mut servo = Servo::new(ServoConfig::default());
        let (t1, t2) = m.sync(1_000 * MS);
        assert_eq!(servo.sync(t1, t2, t2), ServoEvent::Collecting);
        assert!(servo.snapshot().is_none());

        run_ideal(&mut servo, &m, 1_250 * MS, 20_000 * MS);
        assert_eq!(servo.state(), ServoState::Locked);
        let snap = servo.snapshot().unwrap();
        let now = 20_000 * MS;
        let err = snap.media_ns_at(now) as i64 - m.media_at(now) as i64;
        assert!(err.abs() < 1_000, "error {err} ns");
        assert!((servo.freq_offset_ppb() - 40_000.0).abs() < 500.0, "{}", servo.freq_offset_ppb());
        assert!((servo.path_delay_ns().unwrap() - 70_000).abs() < 100);
    }

    #[test]
    fn steps_when_master_jumps() {
        let mut m = Ideal { rate: 1.0 - 20e-6, delay: 50_000, offset: MEDIA0 };
        let mut servo = Servo::new(ServoConfig::default());
        let t = run_ideal(&mut servo, &m, 1_000 * MS, 15_000 * MS);
        assert_eq!(servo.state(), ServoState::Locked);

        // The master's clock jumps forward by 10 ms.
        m.offset += 10 * MS;
        let (t1, t2) = m.sync(t);
        assert!(matches!(servo.sync(t1, t2, t2), ServoEvent::Held { .. }));
        let (t1, t2) = m.sync(t + 250 * MS);
        match servo.sync(t1, t2, t2) {
            ServoEvent::Stepped { offset_ns: Some(e) } => assert!((e - 10_000_000).abs() < 5_000),
            other => panic!("expected a step, got {other:?}"),
        }
        assert_eq!(servo.state(), ServoState::Locking);
        run_ideal(&mut servo, &m, t + 500 * MS, t + 10_000 * MS);
        assert_eq!(servo.state(), ServoState::Locked);
    }

    #[test]
    fn holdover_keeps_frequency_estimate() {
        let m = Ideal { rate: 1.0 + 25e-6, delay: 10_000, offset: MEDIA0 };
        let mut servo = Servo::new(ServoConfig::default());
        let t = run_ideal(&mut servo, &m, 1_000 * MS, 30_000 * MS);
        let snap = servo.enter_holdover(t).unwrap();
        assert!((snap.rate - (1.0 + 25e-6)).abs() < 0.5e-6);
        // Ten seconds later, the free-running clock is still close.
        let later = t + 10_000 * MS;
        let err = snap.media_ns_at(later) as i64 - m.media_at(later) as i64;
        assert!(err.abs() < 10_000, "holdover error {err} ns");
    }

    #[test]
    fn delay_measurement_corrects_for_drift() {
        let m = Ideal { rate: 1.0 + 100e-6, delay: 30_000, offset: MEDIA0 };
        let mut servo = Servo::new(ServoConfig::default());
        servo.freq = m.rate;
        let (t1, t2) = m.sync(1_000 * MS);
        let t3 = t2 + 900 * MS;
        let t4 = m.media_at(t3 + m.delay);
        let raw = servo.delay_measurement(t1, t2, t3, t4);
        assert!((raw - 30_000).abs() <= 10, "raw delay {raw}");
        // Garbage is not used.
        servo.delay_measurement(t1, t2, t3, t4 - 10 * MS);
        assert_eq!(servo.path_delay_ns(), Some(raw));
    }

    #[test]
    fn rate_is_clamped() {
        let m = Ideal { rate: 1.0 + 2_000e-6, delay: 0, offset: MEDIA0 };
        let mut servo = Servo::new(ServoConfig::default());
        run_ideal(&mut servo, &m, 1_000 * MS, 5_000 * MS);
        let snap = servo.snapshot().unwrap();
        assert!(snap.rate <= 1.0 + 500e-6 + 1e-12);
        assert!(servo.frequency() <= 1.0 + 500e-6 + 1e-12);
    }

    #[test]
    fn acquires_without_delay_measurements_eventually() {
        let m = Ideal { rate: 1.0, delay: 0, offset: MEDIA0 };
        let mut servo = Servo::new(ServoConfig::default());
        let mut t = 1_000 * MS;
        // A master that never answers Delay_Req: the servo waits for
        // `acquire_max_span_ns` instead of `acquire_min_span_ns`.
        while servo.snapshot().is_none() {
            let (t1, t2) = m.sync(t);
            servo.sync(t1, t2, t2);
            t += 250 * MS;
        }
        assert_eq!(t - 1_250 * MS, ServoConfig::default().acquire_max_span_ns);
        assert!(servo.wants_fast_delay_requests());
    }

    #[test]
    fn hands_over_from_fll_to_pi() {
        let m = Ideal { rate: 1.0 - 60e-6, delay: 20_000, offset: MEDIA0 };
        let mut servo = Servo::new(ServoConfig::default());
        let t = run_ideal(&mut servo, &m, 1_000 * MS, 10_000 * MS);
        assert_eq!(servo.state(), ServoState::Locked);
        assert!(servo.fll, "the frequency fit runs on after lock");
        run_ideal(&mut servo, &m, t, 30_000 * MS);
        assert!(!servo.fll, "the PI controller has taken over");
        assert!((servo.freq_offset_ppb() + 60_000.0).abs() < 50.0, "{}", servo.freq_offset_ppb());
        assert!(!servo.wants_fast_delay_requests());
    }

    #[test]
    fn early_delay_measurements_are_repaired_at_acquisition() {
        // While acquiring, each exchange is paired with a Sync whose receive
        // timestamp was 2 ms late; once the frequency is known they are
        // re-paired with the lucky estimate instead.
        let m = Ideal { rate: 1.0 + 10e-6, delay: 40_000, offset: MEDIA0 };
        let mut servo = Servo::new(ServoConfig::default());
        let mut t = 1_000 * MS;
        while servo.snapshot().is_none() {
            let (t1, t2) = m.sync(t);
            if servo.delay_samples() < 3 {
                let late_t2 = t2 + 2 * MS;
                let t3 = t2 + 50 * MS;
                let raw = servo.delay_measurement(t1, late_t2, t3, m.media_at(t3 + m.delay));
                assert!((raw - 1_040_000).abs() < 10_000, "raw delay {raw}");
            }
            servo.sync(t1, t2, t2);
            t += 250 * MS;
        }
        let delay = servo.path_delay_ns().unwrap();
        assert!((delay - 40_000).abs() < 1_000, "path delay {delay}");
    }

    #[test]
    fn theil_sen_ignores_outliers() {
        // Offsets on a 25 ppm line, two of them 2 ms late.
        let samples: Vec<Sample> = (0..20u64)
            .map(|i| {
                let t2 = 1_000 * MS + i * 200 * MS;
                let late = if i == 17 || i == 19 { 2 * MS } else { 0 };
                Sample { t1: MEDIA0 + (t2 as f64 * (1.0 + 25e-6)) as u64, t2: t2 + late }
            })
            .collect();
        let drift = estimate_drift(&samples);
        assert!((drift - 25e-6).abs() < 0.1e-6, "{drift}");
        let plain = least_squares(
            &samples.iter().map(|s| (s.t2 as f64, s.t1 as f64 - s.t2 as f64)).collect::<Vec<_>>(),
        )
        .unwrap()
        .1;
        assert!((plain - 25e-6).abs() > 50e-6, "plain least squares would be ruined: {plain}");
    }
}
