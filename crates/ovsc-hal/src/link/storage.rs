//! Configurations from the daemon (design section 12): how one differs from
//! what the device publishes, and the configuration change a structural
//! difference needs.
//!
//! The HAL changes a device's structure only in
//! PerformDeviceConfigurationChange, after the driver has asked for it with
//! RequestDeviceConfigurationChange. [`Staging`] keeps those requests in
//! line:
//!
//! * at most one is outstanding, until the HAL performs or aborts it;
//! * none goes out before the link has run for [`REQUEST_DELAY_NS`] (the
//!   HAL may still be bringing the device up right after Initialize);
//! * after an abort, or a request the host refused, the next waits
//!   [`REQUEST_RETRY_NS`].
//!
//! What a change applied is persisted by the link through its sink, under
//! `ovsc_ipc::protocol::STORAGE_KEY`, after every Perform and every
//! names-only change, so the device comes back with it at the next boot.

use std::time::Duration;

use super::ConfigPlan;
use crate::model::DriverConfig;

/// How long after the link starts the first request may go out.
pub const REQUEST_DELAY_NS: u64 = 2_000_000_000;

/// How long after an abort, or a failed request, the next may go out.
pub const REQUEST_RETRY_NS: u64 = 1_000_000_000;

/// How `cfg` differs from the `published` configuration:
///
/// * [`ConfigPlan::Same`]: in nothing but, perhaps, its generation;
/// * [`ConfigPlan::NamesOnly`]: in channel or device names only, which can
///   be published at once;
/// * [`ConfigPlan::Structural`]: in anything else, which needs a HAL
///   configuration change.
pub fn plan(published: &DriverConfig, cfg: &DriverConfig) -> ConfigPlan {
    if !cfg.structural_eq(published) {
        ConfigPlan::Structural
    } else if cfg.input_names == published.input_names
        && cfg.output_names == published.output_names
        && cfg.device_name == published.device_name
    {
        ConfigPlan::Same
    } else {
        ConfigPlan::NamesOnly
    }
}

/// What to do about a configuration change request now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    /// Nothing: none is wanted, one is outstanding, or a timer will ask
    /// again.
    Nothing,
    /// Call RequestDeviceConfigurationChange now.
    Now,
    /// Ask again after this delay ([`Staging::timer_fired`] first).
    Later(Duration),
}

/// The state of the structural changes the link asks the HAL for.
#[derive(Debug)]
pub struct Staging {
    /// The structural configuration waiting for Perform. Kept whole:
    /// generations restart with every daemon, so only the configuration
    /// itself tells whether a Perform applied it.
    wanted: Option<DriverConfig>,
    /// A request is out, not yet performed or aborted.
    outstanding: bool,
    /// No request before this host time.
    not_before_ns: u64,
    /// A timer will ask again.
    timer_armed: bool,
    /// Consecutive requests the host refused.
    failures: u32,
    /// The generation of the last invalid configuration, logged once.
    rejected: Option<u64>,
}

impl Staging {
    /// Staging for a link that started at `start_ns`.
    pub fn new(start_ns: u64) -> Self {
        Self {
            wanted: None,
            outstanding: false,
            not_before_ns: start_ns.saturating_add(REQUEST_DELAY_NS),
            timer_armed: false,
            failures: 0,
            rejected: None,
        }
    }

    /// The structural configuration `cfg` waits for Perform, replacing any
    /// that waited before.
    pub fn want(&mut self, cfg: DriverConfig) {
        self.wanted = Some(cfg);
    }

    /// The newest configuration needs no change: whatever was waiting is
    /// superseded.
    pub fn settle(&mut self) {
        self.wanted = None;
    }

    /// Whether a structural configuration waits.
    pub fn wanted(&self) -> bool {
        self.wanted.is_some()
    }

    /// Whether a request is out.
    #[cfg(test)]
    pub fn outstanding(&self) -> bool {
        self.outstanding
    }

    /// What to do about a request at `now_ns`.
    pub fn next(&mut self, now_ns: u64) -> Request {
        if self.wanted.is_none() || self.outstanding || self.timer_armed {
            return Request::Nothing;
        }
        if now_ns < self.not_before_ns {
            self.timer_armed = true;
            return Request::Later(Duration::from_nanos(self.not_before_ns - now_ns));
        }
        Request::Now
    }

    /// The timer of a [`Request::Later`] fired.
    pub fn timer_fired(&mut self) {
        self.timer_armed = false;
    }

    /// A request went to the host at `now_ns`, which returned `ok`. Returns
    /// whether this is the first of a run of refusals, worth a log line.
    pub fn requested(&mut self, ok: bool, now_ns: u64) -> bool {
        if ok {
            self.outstanding = true;
            self.failures = 0;
            false
        } else {
            self.not_before_ns = now_ns.saturating_add(REQUEST_RETRY_NS);
            self.failures = self.failures.saturating_add(1);
            self.failures == 1
        }
    }

    /// The HAL performed a change that applied `cfg`. Whatever else
    /// arrived meanwhile is still wanted.
    pub fn performed(&mut self, cfg: &DriverConfig) {
        self.outstanding = false;
        if self.wanted.as_ref() == Some(cfg) {
            self.wanted = None;
        }
    }

    /// The HAL aborted the request at `now_ns`, or performed it with nothing
    /// pending; whatever is wanted is asked for again after
    /// [`REQUEST_RETRY_NS`].
    pub fn aborted(&mut self, now_ns: u64) {
        self.outstanding = false;
        self.not_before_ns = self.not_before_ns.max(now_ns.saturating_add(REQUEST_RETRY_NS));
    }

    /// An invalid configuration of generation `config_gen` arrived. Returns
    /// whether it is new, so worth logging.
    pub fn rejected(&mut self, config_gen: u64) -> bool {
        self.rejected.replace(config_gen) != Some(config_gen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    fn cfg(config_gen: u64) -> DriverConfig {
        DriverConfig { config_gen, input_channels: 2, ..DriverConfig::fallback() }
    }

    #[test]
    fn plans_follow_what_differs() {
        let a = DriverConfig::fallback();
        assert_eq!(plan(&a, &a), ConfigPlan::Same);
        assert_eq!(plan(&a, &DriverConfig { config_gen: 9, ..a.clone() }), ConfigPlan::Same);
        let mut names = a.clone();
        names.input_names[3] = "vox".into();
        assert_eq!(plan(&a, &names), ConfigPlan::NamesOnly);
        let device = DriverConfig { device_name: "studio".into(), ..a.clone() };
        assert_eq!(plan(&a, &device), ConfigPlan::NamesOnly);
        let rate = DriverConfig { sample_rate: 96_000, ..names };
        assert_eq!(plan(&a, &rate), ConfigPlan::Structural);
        let latency = DriverConfig { output_latency: 7, ..a.clone() };
        assert_eq!(plan(&a, &latency), ConfigPlan::Structural);
    }

    #[test]
    fn requests_wait_for_the_start_delay() {
        let mut s = Staging::new(10 * S);
        assert_eq!(s.next(10 * S), Request::Nothing);
        s.want(cfg(1));
        assert_eq!(s.next(10 * S), Request::Later(Duration::from_secs(2)));
        // The timer is armed: no second one.
        assert_eq!(s.next(11 * S), Request::Nothing);
        s.timer_fired();
        assert_eq!(s.next(11 * S), Request::Later(Duration::from_secs(1)));
        s.timer_fired();
        assert_eq!(s.next(12 * S), Request::Now);
    }

    #[test]
    fn one_request_is_outstanding_until_performed() {
        let mut s = Staging::new(0);
        s.want(cfg(5));
        assert_eq!(s.next(3 * S), Request::Now);
        assert!(!s.requested(true, 3 * S));
        assert!(s.outstanding());
        // A restarted daemon counts generations from 1 again.
        s.want(cfg(1));
        assert_eq!(s.next(3 * S), Request::Nothing);
        // Perform applied generation 5; generation 1 is still wanted.
        s.performed(&cfg(5));
        assert!(s.wanted());
        assert_eq!(s.next(3 * S), Request::Now);
        s.requested(true, 3 * S);
        s.performed(&cfg(1));
        assert!(!s.wanted());
        assert_eq!(s.next(4 * S), Request::Nothing);
    }

    #[test]
    fn aborts_and_refusals_retry_after_1_s() {
        let mut s = Staging::new(0);
        s.want(cfg(1));
        assert_eq!(s.next(5 * S), Request::Now);
        s.requested(true, 5 * S);
        s.aborted(5 * S);
        assert_eq!(s.next(5 * S), Request::Later(Duration::from_secs(1)));
        s.timer_fired();
        assert_eq!(s.next(6 * S), Request::Now);
        // The host refuses: logged once, retried every second.
        assert!(s.requested(false, 6 * S));
        assert_eq!(s.next(6 * S), Request::Later(Duration::from_secs(1)));
        s.timer_fired();
        assert_eq!(s.next(7 * S), Request::Now);
        assert!(!s.requested(false, 7 * S));
        assert_eq!(s.next(7 * S), Request::Later(Duration::from_secs(1)));
        s.timer_fired();
        assert_eq!(s.next(8 * S), Request::Now);
        assert!(!s.requested(true, 8 * S));
        // A configuration that needs no change supersedes the wanted one.
        s.settle();
        s.aborted(8 * S);
        s.timer_fired();
        assert_eq!(s.next(10 * S), Request::Nothing);
    }

    #[test]
    fn invalid_configurations_are_reported_once() {
        let mut s = Staging::new(0);
        assert!(s.rejected(4));
        assert!(!s.rejected(4));
        assert!(s.rejected(5));
    }
}
