//! The link's connection state machine (design section 10.6).
//!
//! The machine only decides; the link carries its [`Plan`]s out on the
//! transport. It moves through four phases:
//!
//! ```text
//! Idle --start--> AwaitReply --welcome--> Attached { generation }
//!                     |  ^                    |
//!            reject   |  | timer      interrupted, bye
//!                     v  |                    v
//!                 Incompatible            AwaitReply (fast schedule)
//! ```
//!
//! How it retries depends on why it is not attached:
//!
//! * after an interruption, a bye or at start, hellos go out at 0, 0.25,
//!   0.5, 1, 2 and 5 s, then every 5 s, without waiting for the previous
//!   one's reply (a daemon that comes back answers the first hello it gets);
//! * when the service does not exist (the connection is invalid), the
//!   connection is cancelled and made again every 5 s;
//! * when the daemon refused the plug-in, or its region is unusable, the
//!   hello is repeated every 30 s, so a daemon upgrade heals on its own.
//!
//! Every timer carries the epoch it was armed in; a transition that makes
//! pending timers pointless moves to a new epoch, so they do nothing when
//! they fire.

use std::time::Duration;

/// How long a hello waits for its reply.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(2);

/// When the hellos of the fast schedule go out, counted from its start;
/// after the last, every [`FAST_EVERY`].
const FAST_AT: [Duration; 6] = [
    Duration::ZERO,
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
];

/// The fast schedule's period once its steps are used up.
pub const FAST_EVERY: Duration = Duration::from_secs(5);

/// How often a connection to a service that does not exist is made again.
pub const INVALID_EVERY: Duration = Duration::from_secs(5);

/// How often a refused plug-in says hello again.
pub const REFUSED_EVERY: Duration = Duration::from_secs(30);

/// What the link is doing about its connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Not started, or waiting to make the connection again.
    Idle,
    /// A hello is out. A welcome from any hello still out is taken.
    AwaitReply,
    /// Welcomed by the daemon of this generation.
    Attached { generation: u64 },
    /// Refused by the daemon, or its region is unusable here; a hello goes
    /// out again after [`REFUSED_EVERY`].
    Incompatible,
}

/// Why the link is retrying, which sets the pace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// The fast schedule; `sent` hellos have gone out on it.
    Fast { sent: usize },
    /// The service does not exist.
    Invalid,
    /// The daemon refused the plug-in.
    Refused,
}

/// What the link must do after a transition, in this order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Cancel the connection.
    pub cancel: bool,
    /// Make a new connection.
    pub connect: bool,
    /// The link was attached and no longer is: start counting down to
    /// releasing the region.
    pub detached: bool,
    /// Send a hello, numbered so.
    pub hello: Option<u64>,
    /// Call [`Machine::on_timer`] with this epoch after this delay.
    pub timer: Option<(Duration, u64)>,
}

impl Plan {
    /// Whether the plan does nothing: the event did not change the state.
    pub fn is_empty(&self) -> bool {
        *self == Plan::default()
    }
}

/// The connection state machine.
#[derive(Debug)]
pub struct Machine {
    phase: Phase,
    mode: Mode,
    epoch: u64,
    /// The number of the last hello sent.
    hello: u64,
    started: bool,
}

impl Default for Machine {
    fn default() -> Self {
        Self::new()
    }
}

impl Machine {
    pub const fn new() -> Self {
        Self {
            phase: Phase::Idle,
            mode: Mode::Fast { sent: 0 },
            epoch: 0,
            hello: 0,
            started: false,
        }
    }

    #[cfg(test)]
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// The generation of the daemon the link is attached to.
    pub fn attached(&self) -> Option<u64> {
        match self.phase {
            Phase::Attached { generation } => Some(generation),
            _ => None,
        }
    }

    /// Whether a welcome now would be taken: whenever the link is not
    /// attached.
    pub fn takes_welcome(&self) -> bool {
        self.attached().is_none()
    }

    /// The number of the last hello sent.
    #[cfg(test)]
    pub fn last_hello(&self) -> u64 {
        self.hello
    }

    /// The link starts: connect and say hello on the fast schedule. Only the
    /// first call does anything.
    pub fn start(&mut self) -> Plan {
        if self.started {
            return Plan::default();
        }
        self.started = true;
        Plan { connect: true, ..self.fast() }
    }

    /// The daemon went away. When attached, or refused (the daemon may come
    /// back upgraded), the fast schedule starts over; otherwise the current
    /// schedule goes on, so a daemon that keeps refusing the connection
    /// never makes the hellos faster.
    pub fn interrupted(&mut self) -> Plan {
        match (self.phase, self.mode) {
            (Phase::Attached { .. }, _) => Plan { detached: true, ..self.fast() },
            (_, Mode::Refused) => self.fast(),
            _ => Plan::default(),
        }
    }

    /// The daemon said goodbye: it is about to exit, and the link reconnects
    /// as after an interruption.
    pub fn bye(&mut self) -> Plan {
        match self.phase {
            Phase::Attached { .. } => Plan { detached: true, ..self.fast() },
            _ => Plan::default(),
        }
    }

    /// The connection is invalid: the handler said so (`hello` is `None`),
    /// or the reply to hello `hello` did. Cancel it and connect again after
    /// [`INVALID_EVERY`]. Ignored for a hello other than the last and while
    /// already waiting to reconnect.
    pub fn invalid(&mut self, hello: Option<u64>) -> Plan {
        if !self.started || hello.is_some_and(|h| h != self.hello) {
            return Plan::default();
        }
        if self.mode == Mode::Invalid && self.phase == Phase::Idle {
            return Plan::default();
        }
        let detached = matches!(self.phase, Phase::Attached { .. });
        self.phase = Phase::Idle;
        self.mode = Mode::Invalid;
        let epoch = self.next_epoch();
        Plan { cancel: true, detached, timer: Some((INVALID_EVERY, epoch)), ..Plan::default() }
    }

    /// A welcome from the daemon of `generation` was taken: attached. Pending
    /// timers lapse.
    pub fn welcomed(&mut self, generation: u64) {
        self.phase = Phase::Attached { generation };
        self.mode = Mode::Fast { sent: 0 };
        self.next_epoch();
    }

    /// The daemon refused the plug-in, or the region or reply it sent is
    /// unusable: say hello again after [`REFUSED_EVERY`]. Ignored while
    /// attached.
    pub fn refused(&mut self) -> Plan {
        if !self.started || self.attached().is_some() {
            return Plan::default();
        }
        self.phase = Phase::Incompatible;
        self.mode = Mode::Refused;
        let epoch = self.next_epoch();
        Plan { timer: Some((REFUSED_EVERY, epoch)), ..Plan::default() }
    }

    /// A timer armed in `epoch` fired.
    pub fn on_timer(&mut self, epoch: u64) -> Plan {
        if epoch != self.epoch || self.attached().is_some() {
            return Plan::default();
        }
        match self.mode {
            Mode::Fast { sent } => {
                let hello = self.next_hello();
                self.phase = Phase::AwaitReply;
                self.mode = Mode::Fast { sent: sent.saturating_add(1) };
                let delay = fast_delay(sent.saturating_add(1));
                Plan { hello: Some(hello), timer: Some((delay, epoch)), ..Plan::default() }
            }
            Mode::Invalid => {
                let hello = self.next_hello();
                self.phase = Phase::AwaitReply;
                Plan {
                    connect: true,
                    hello: Some(hello),
                    timer: Some((INVALID_EVERY, epoch)),
                    ..Plan::default()
                }
            }
            Mode::Refused => {
                let hello = self.next_hello();
                self.phase = Phase::AwaitReply;
                Plan { hello: Some(hello), timer: Some((REFUSED_EVERY, epoch)), ..Plan::default() }
            }
        }
    }

    /// Starts the fast schedule: a hello now, the next one timed.
    fn fast(&mut self) -> Plan {
        let epoch = self.next_epoch();
        let hello = self.next_hello();
        self.phase = Phase::AwaitReply;
        self.mode = Mode::Fast { sent: 1 };
        Plan { hello: Some(hello), timer: Some((fast_delay(1), epoch)), ..Plan::default() }
    }

    fn next_epoch(&mut self) -> u64 {
        self.epoch = self.epoch.wrapping_add(1);
        self.epoch
    }

    fn next_hello(&mut self) -> u64 {
        self.hello = self.hello.wrapping_add(1);
        self.hello
    }
}

/// The wait between the fast schedule's hello number `sent` (1-based) and
/// the next one.
fn fast_delay(sent: usize) -> Duration {
    match (FAST_AT.get(sent.wrapping_sub(1)), FAST_AT.get(sent)) {
        (Some(prev), Some(next)) => next.saturating_sub(*prev),
        _ => FAST_EVERY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An event at a time: something that happens to the machine.
    type Event = (Duration, fn(&mut Machine) -> Plan);

    /// Runs the machine's timers in time order until `end`, recording when
    /// each hello and connection happens. Events in `at` (time, event) are
    /// applied when their time comes.
    fn run(
        m: &mut Machine,
        first: Plan,
        end: Duration,
        at: &mut Vec<Event>,
    ) -> (Vec<Duration>, Vec<Duration>) {
        let mut now = Duration::ZERO;
        let (mut hellos, mut connects) = (Vec::new(), Vec::new());
        let mut timers: Vec<(Duration, u64)> = Vec::new();
        let mut take = |plan: Plan,
                        now: Duration,
                        timers: &mut Vec<(Duration, u64)>,
                        hellos: &mut Vec<Duration>| {
            if plan.connect {
                connects.push(now);
            }
            if plan.hello.is_some() {
                hellos.push(now);
            }
            if let Some((d, e)) = plan.timer {
                timers.push((now + d, e));
            }
        };
        take(first, now, &mut timers, &mut hellos);
        at.sort_by_key(|(t, _)| *t);
        loop {
            timers.sort_by_key(|(t, _)| *t);
            let next_timer = timers.first().map(|(t, _)| *t);
            let next_event = at.first().map(|(t, _)| *t);
            let event_first = match (next_timer, next_event) {
                (_, None) => false,
                (None, Some(_)) => true,
                (Some(t), Some(e)) => e <= t,
            };
            if event_first {
                let (t, f) = at.remove(0);
                if t > end {
                    break;
                }
                now = t;
                let plan = f(m);
                take(plan, now, &mut timers, &mut hellos);
            } else if let Some((t, e)) = timers.first().copied() {
                if t > end {
                    break;
                }
                timers.remove(0);
                now = t;
                let plan = m.on_timer(e);
                take(plan, now, &mut timers, &mut hellos);
            } else {
                break;
            }
        }
        (hellos, connects)
    }

    fn secs(s: &[f64]) -> Vec<Duration> {
        s.iter().map(|&s| Duration::from_secs_f64(s)).collect()
    }

    #[test]
    fn the_fast_schedule_backs_off_to_every_5_s() {
        let mut m = Machine::new();
        let plan = m.start();
        assert!(plan.connect);
        assert_eq!(m.phase(), Phase::AwaitReply);
        let (hellos, connects) = run(&mut m, plan, Duration::from_secs(20), &mut Vec::new());
        assert_eq!(hellos, secs(&[0.0, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 20.0]));
        assert_eq!(connects, secs(&[0.0]));
        // Starting again does nothing.
        assert!(m.start().is_empty());
    }

    #[test]
    fn a_welcome_stops_the_hellos_and_an_interruption_starts_them_over() {
        let mut m = Machine::new();
        let plan = m.start();
        let mut at: Vec<Event> = vec![
            (Duration::from_millis(600), |m| {
                m.welcomed(7);
                Plan::default()
            }),
            (Duration::from_secs(3), |m| {
                let p = m.interrupted();
                assert!(p.detached);
                p
            }),
            // Interruptions while reconnecting change nothing.
            (Duration::from_millis(3100), Machine::interrupted),
        ];
        let (hellos, _) = run(&mut m, plan, Duration::from_secs(9), &mut at);
        assert_eq!(hellos, secs(&[0.0, 0.25, 0.5, 3.0, 3.25, 3.5, 4.0, 5.0, 8.0]));
        assert_eq!(m.phase(), Phase::AwaitReply);
        assert_eq!(m.attached(), None);
    }

    #[test]
    fn an_invalid_service_is_reconnected_every_5_s() {
        let mut m = Machine::new();
        let plan = m.start();
        let mut at: Vec<Event> = vec![
            (Duration::from_millis(1), |m| {
                let p = m.invalid(Some(m.last_hello()));
                assert!(p.cancel && !p.detached);
                p
            }),
            // The handler's own notice of the same failure.
            (Duration::from_millis(2), |m| m.invalid(None)),
            (Duration::from_millis(5002), |m| m.invalid(Some(m.last_hello()))),
            (Duration::from_millis(10_002), |m| m.invalid(None)),
        ];
        let (hellos, connects) = run(&mut m, plan, Duration::from_secs(21), &mut at);
        // After an invalid connection the reconnects keep a 5 s cadence,
        // whether or not their hellos fail.
        assert_eq!(connects, secs(&[0.0, 5.001, 10.002, 15.002, 20.002]));
        assert_eq!(hellos, connects);
    }

    #[test]
    fn errors_from_older_hellos_are_ignored() {
        let mut m = Machine::new();
        m.start();
        let first = m.last_hello();
        let p = m.on_timer(m.epoch);
        assert!(p.hello.is_some());
        assert!(m.invalid(Some(first)).is_empty());
        assert!(!m.invalid(Some(m.last_hello())).is_empty());
    }

    #[test]
    fn a_refused_plugin_says_hello_every_30_s() {
        let mut m = Machine::new();
        let plan = m.start();
        let mut at: Vec<Event> = vec![
            (Duration::from_millis(1), Machine::refused),
            (Duration::from_millis(30_002), Machine::refused),
        ];
        let (hellos, _) = run(&mut m, plan, Duration::from_secs(95), &mut at);
        assert_eq!(hellos, secs(&[0.0, 30.001, 60.002, 90.002]));
        assert_eq!(m.phase(), Phase::AwaitReply);

        // Refusals do not apply once attached; an interruption after a
        // refusal starts the fast schedule.
        let mut m = Machine::new();
        m.start();
        m.welcomed(1);
        assert!(m.refused().is_empty());
        let mut m = Machine::new();
        m.start();
        m.refused();
        assert_eq!(m.phase(), Phase::Incompatible);
        let p = m.interrupted();
        assert!(p.hello.is_some() && !p.detached);
    }

    #[test]
    fn bye_reconnects_only_when_attached() {
        let mut m = Machine::new();
        m.start();
        assert!(m.bye().is_empty());
        m.welcomed(3);
        assert!(!m.takes_welcome());
        let p = m.bye();
        assert!(p.detached && p.hello.is_some());
        assert!(m.takes_welcome());
        // Timers of the attached epoch are gone.
        assert!(m.on_timer(m.epoch - 1).is_empty());
    }

    #[test]
    fn nothing_happens_before_start() {
        let mut m = Machine::new();
        assert!(m.interrupted().is_empty());
        assert!(m.invalid(None).is_empty());
        assert!(m.refused().is_empty());
        assert!(m.bye().is_empty());
        assert_eq!(m.phase(), Phase::Idle);
    }

    #[test]
    fn fast_delays() {
        let d: Vec<Duration> = (1..=8).map(fast_delay).collect();
        assert_eq!(d, secs(&[0.25, 0.25, 0.5, 1.0, 3.0, 5.0, 5.0, 5.0]));
    }
}
