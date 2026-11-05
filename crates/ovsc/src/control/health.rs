//! The daemon's health tracker: turns the counters and states of the
//! driver, the device and the clock into warnings an audio engineer can act
//! on, before or as problems become audible.
//!
//! The counters run from the start of the device, the clock or the driver,
//! so a total says nothing about now. The tracker keeps the readings of the
//! last [`WINDOW`], taken every second by the daemon's engine loop
//! ([`super::watch`]) and at each status request, and warns about what rose
//! within it: such a warning holds for the window after its counter last
//! rose, then clears. States (a failed driver, a closed gate while an
//! application plays, a lost clock) warn while they hold.
//!
//! The first [`ENGINE_SETTLE`] after the engine (re)starts, and the first
//! [`IO_SETTLE`] after an application starts IO, are expected to be rough:
//! flows connect, the clock settles, the gate opens. What counters rise
//! then never counts.
//!
//! All of this runs on the daemon's control side and reads atomics through
//! the device, the clock and the driver's status: nothing here touches a
//! real-time thread.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::{Duration, Instant};

use tracing::{info, warn};

use ovsc_clock::{ClockState, ClockStatus};
use ovsc_core::{DeviceStats, NameConflict, RxChannelStatus};
use ovsc_hal_server::HalStatus;

/// How far back counters are compared: a warning about a counter holds
/// this long after the counter last rose.
pub const WINDOW: Duration = Duration::from_secs(30);
/// How long after the engine (re)starts rising counters are expected.
pub const ENGINE_SETTLE: Duration = Duration::from_secs(10);
/// How long after an application starts IO the driver's counters are
/// expected to rise: the gate opens, the first cycles find their place.
pub const IO_SETTLE: Duration = Duration::from_secs(3);
/// How long a closed gate is normal while IO runs: at the start of IO, or
/// while the clock settles.
const GATE_GRACE: Duration = Duration::from_secs(2);
/// The driver's IO thread counts as stalled once its last operation is this
/// old while IO runs. A cycle lasts a few milliseconds; the margin covers
/// the driver publishing the end of IO (from its IPC queue) a little after
/// the last operation.
const IO_STALL: Duration = Duration::from_secs(2);
/// How long a subscribed channel may go without audio before it warns: a
/// transmitter's dead flow takes 3 s to notice and a new request a few more.
const RX_GRACE: Duration = Duration::from_secs(10);
/// How long the clock may take to lock before the app is told why it may
/// not.
const LOCK_WARNING_AFTER: Duration = Duration::from_secs(20);
/// Readings closer together than this replace the last one, so that a
/// client asking many times a second does not grow the window.
const MIN_SPACING: Duration = Duration::from_millis(250);
/// A warning is logged as cleared once it has been gone this long, so that
/// one that comes and goes is logged once.
const CLEAR_HOLD: Duration = Duration::from_secs(10);
/// Fewer early packets, dropped datagrams or late sends than these within
/// the window are noise (a leader change, a stray packet). Each lost or late
/// received packet is a dropout, so those warn from the first.
const EARLY_MIN: u64 = 5;
const REJECTED_MIN: u64 = 5;
const TX_LATE_MIN: u64 = 5;
/// The most channels a warning names.
const NAMED_CHANNELS: usize = 4;

/// What the tracker looks at, at one moment.
#[derive(Clone, Debug, Default)]
pub struct Reading {
    /// The Core Audio driver's status, when the daemon serves the driver.
    pub hal: Option<HalStatus>,
    /// The engine, while it runs.
    pub engine: Option<EngineReading>,
}

/// The running engine at one moment.
#[derive(Clone, Debug)]
pub struct EngineReading {
    pub stats: DeviceStats,
    pub clock: ClockStatus,
    /// Whether the clock follows the network (PTP) rather than this
    /// computer.
    pub ptp: bool,
    /// How long the clock has run. A clock younger than at the last
    /// reading is a new one.
    pub clock_age: Duration,
    /// The local time of the reading (`ovsc_clock::local_now_ns`), which
    /// the clock's holdover deadline is in.
    pub local_ns: u64,
    /// The receive latency, ms.
    pub latency_ms: f64,
    /// The network interface's name.
    pub interface: String,
    pub rx: Vec<RxChannelStatus>,
    /// A part of the device that has stopped ([`ovsc_core::Device::stopped_part`]).
    pub stopped_part: Option<&'static str>,
    pub name_conflict: Option<NameConflict>,
}

/// What a warning is about. At most one of each holds at a time; the order
/// is the order shown, after the urgent ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    DriverFailed,
    IoStalled,
    GateClosed,
    TransmitStopped,
    ClockNotLocked,
    ClockLost,
    Holdover,
    ClockRelocking,
    ClockStepped,
    LeaderChanged,
    RxNotReceiving,
    RxLost,
    RxLate,
    RxEarly,
    RxRejected,
    TxSendErrors,
    TxSkipped,
    TxLate,
    TxUnderruns,
    TxExpired,
    TxNotRealtime,
    LateOutput,
    EarlyInput,
    Silenced,
    Slips,
    NonFinite,
    NameConflict,
}

impl Kind {
    /// The warning in a few words, for the log line that says it cleared.
    pub fn label(self) -> &'static str {
        match self {
            Kind::DriverFailed => "the driver failed",
            Kind::IoStalled => "Core Audio stopped calling the driver",
            Kind::GateClosed => "the driver holds audio back while an application plays",
            Kind::TransmitStopped => "a part of the device stopped",
            Kind::ClockNotLocked => "the clock has not locked",
            Kind::ClockLost => "the clock lost its leader",
            Kind::Holdover => "the clock runs in holdover",
            Kind::ClockRelocking => "the clock lost lock",
            Kind::ClockStepped => "the clock jumped",
            Kind::LeaderChanged => "the clock leader changed",
            Kind::RxNotReceiving => "subscribed receive channels without audio",
            Kind::RxLost => "received packets lost",
            Kind::RxLate => "received packets too late to play",
            Kind::RxEarly => "received packets ahead of our clock",
            Kind::RxRejected => "stray datagrams dropped",
            Kind::TxSendErrors => "packets that could not be sent",
            Kind::TxSkipped => "the transmit thread fell behind",
            Kind::TxLate => "packets sent late",
            Kind::TxUnderruns => "packets sent without the application's audio",
            Kind::TxExpired => "flows to other devices closed",
            Kind::TxNotRealtime => "the transmit thread lacks real-time priority",
            Kind::LateOutput => "late output cycles",
            Kind::EarlyInput => "early input cycles",
            Kind::Silenced => "audio held back while an application played",
            Kind::Slips => "audio slips",
            Kind::NonFinite => "invalid samples from the application",
            Kind::NameConflict => "another device uses this name",
        }
    }
}

/// A warning for the app and the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    pub kind: Kind,
    /// Audio is down now in at least one direction.
    pub urgent: bool,
    /// It is, or was within the window, audible: dropouts, clicks, silence.
    pub audible: bool,
    /// What happened, what it affects and what to do, in plain words.
    pub text: String,
    /// A few words for the status line: `12 received packets lost`.
    pub short: String,
}

/// What the tracker makes of a reading.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Assessment {
    /// The warnings that hold, the urgent first.
    pub warnings: Vec<Warning>,
    /// Whether the driver has failed: its status says so, and the value is
    /// not a dead driver instance's leftover.
    pub driver_failed: bool,
    /// Whether Core Audio stopped calling the driver while IO runs.
    pub io_stalled: bool,
}

impl Assessment {
    /// Whether any warning is about something audible.
    pub fn audible(&self) -> bool {
        self.warnings.iter().any(|w| w.audible)
    }
}

/// The daemon's health tracker; see the module documentation.
#[derive(Debug, Default)]
pub struct Health {
    /// The readings of the window, oldest first, and one before it.
    samples: VecDeque<Sample>,
    /// Until when the engine settles after its last (re)start.
    settle_until: Option<Instant>,
    /// Since when IO runs, while it does.
    io_since: Option<Instant>,
    /// Since when the gate is closed while IO runs.
    gate_closed_since: Option<Instant>,
    /// The clock's age at the last reading, and whether it has locked.
    clock_age: Option<Duration>,
    clock_locked: bool,
    /// The driver instance whose failure the status shows. The driver
    /// writes `faulted` only with its IO counters, so a driver loaded since
    /// leaves the failed one's value until its own IO starts.
    failed_instance: Option<u64>,
    /// Subscribed receive channels without audio, by index and
    /// subscription, and since when.
    not_receiving: HashMap<(usize, String), Instant>,
    /// For the log: the warnings logged as present, and when each was last
    /// seen.
    logged: BTreeMap<Kind, Instant>,
    /// The warnings of the last reading, for the status line.
    last: Vec<Warning>,
}

/// One reading, as the window keeps it.
#[derive(Clone, Copy, Debug)]
struct Sample {
    at: Instant,
    c: Counts,
    /// A driver is connected: its counters are its own.
    hal: bool,
    /// The engine runs: the device's and clock's counters are there.
    engine: bool,
    /// An application runs IO through the driver.
    io: bool,
    /// IO has run for [`IO_SETTLE`].
    io_settled: bool,
    /// IO runs through an open gate and a healthy driver: audio is live.
    live: bool,
}

/// The counters a sample keeps.
#[derive(Clone, Copy, Debug, Default)]
struct Counts {
    rx_late: u64,
    rx_lost: u64,
    rx_early: u64,
    rx_rejected: u64,
    tx_send_errors: u64,
    tx_late: u64,
    tx_skipped: u64,
    tx_resyncs: u64,
    tx_expired: u64,
    tx_underruns: u64,
    late_out: u64,
    early_in: u64,
    silenced: u64,
    absorbs: u64,
    non_finite: u64,
    min_out_margin: i64,
    min_in_margin: i64,
    unlocks: u64,
    steps: u64,
    master_changes: u64,
}

impl Counts {
    fn of(hal: Option<&HalStatus>, engine: Option<&EngineReading>) -> Counts {
        let mut c = Counts::default();
        if let Some(h) = hal {
            c.late_out = h.late_output_cycles;
            c.early_in = h.early_input_cycles;
            c.silenced = h.silenced_cycles;
            c.absorbs = h.absorbs;
            c.non_finite = h.non_finite_out;
            c.min_out_margin = h.min_output_margin;
            c.min_in_margin = h.min_input_margin;
        }
        if let Some(e) = engine {
            let s = &e.stats;
            c.rx_late = s.rx_late_packets;
            c.rx_lost = s.rx_lost_packets;
            c.rx_early = s.rx_early_packets;
            c.rx_rejected = s.rx_rejected_packets;
            c.tx_send_errors = s.tx_send_errors;
            c.tx_late = s.tx_late_packets;
            c.tx_skipped = s.tx_skipped_packets;
            c.tx_resyncs = s.tx_resyncs;
            c.tx_expired = s.tx_expired_flows;
            c.tx_underruns = s.tx_underruns;
            c.unlocks = u64::from(e.clock.unlocks);
            c.steps = u64::from(e.clock.steps);
            c.master_changes = u64::from(e.clock.master_changes);
        }
        c
    }
}

impl Health {
    pub fn new() -> Health {
        Health::default()
    }

    /// The engine (re)started at `now`: its device counts from zero, and
    /// the next [`ENGINE_SETTLE`] are expected to be rough.
    pub fn restarted(&mut self, now: Instant) {
        self.samples.clear();
        self.not_receiving.clear();
        self.settle_until = Some(now + ENGINE_SETTLE);
    }

    /// Takes `reading`, made at `now`, into the window and says what holds.
    pub fn observe(&mut self, now: Instant, reading: &Reading) -> Assessment {
        // A driver that is not connected left its last values behind.
        let hal = reading.hal.as_ref().filter(|h| h.peers > 0);
        let engine = reading.engine.as_ref();

        let failed = match hal {
            Some(h) if h.faulted => {
                // A driver doing IO writes `faulted` itself; otherwise the
                // value is the one first seen, by the instance then.
                if h.io_clients > 0 || self.failed_instance.is_none() {
                    self.failed_instance = Some(h.plugin_instance);
                }
                self.failed_instance == Some(h.plugin_instance)
            }
            Some(_) => {
                self.failed_instance = None;
                false
            }
            None => false,
        };
        let io = hal.is_some_and(|h| h.io_clients > 0);
        if io {
            self.io_since.get_or_insert(now);
        } else {
            self.io_since = None;
        }
        let io_settled = self.io_since.is_some_and(|t| now.duration_since(t) >= IO_SETTLE);
        let gate = hal.is_some_and(|h| h.gate);
        if io && !gate && !failed {
            self.gate_closed_since.get_or_insert(now);
        } else {
            self.gate_closed_since = None;
        }
        let stalled = io
            && io_settled
            && !failed
            && hal.is_some_and(|h| h.io_heartbeat_age.is_none_or(|age| age >= IO_STALL));
        if let Some(e) = engine {
            if self.clock_age.is_some_and(|age| e.clock_age < age) {
                // A new clock: whether the old one locked says nothing.
                self.clock_locked = false;
            }
            self.clock_age = Some(e.clock_age);
            if e.clock.state == ClockState::Locked || e.clock.holdover_until_ns.is_some() {
                self.clock_locked = true;
            }
        }

        let sample = Sample {
            at: now,
            c: Counts::of(hal, engine),
            hal: hal.is_some(),
            engine: engine.is_some(),
            io,
            io_settled,
            live: io && gate && !failed && !stalled,
        };
        // The newest reading stands in for the time since the one before it
        // until that is MIN_SPACING old; the ones before stay put.
        let n = self.samples.len();
        if n >= 2 && now.saturating_duration_since(self.samples[n - 2].at) < MIN_SPACING {
            self.samples[n - 1] = sample;
        } else {
            self.samples.push_back(sample);
        }
        if let Some(start) = now.checked_sub(WINDOW) {
            // Keep one reading from before the window: the base of the
            // first rise within it.
            while self.samples.len() > 2 && self.samples[1].at <= start {
                self.samples.pop_front();
            }
        }

        let mut warnings = Vec::new();
        if let Some(h) = hal {
            self.driver_warnings(now, h, engine, failed, stalled, &mut warnings);
        }
        if let Some(e) = engine {
            self.engine_warnings(now, e, &mut warnings);
        }
        warnings.sort_by_key(|w| (!w.urgent, w.kind));
        self.last = warnings.clone();
        Assessment { warnings, driver_failed: failed, io_stalled: stalled }
    }

    /// Logs the warnings that appeared since the last call at WARN, and
    /// those gone for [`CLEAR_HOLD`] once at INFO. The daemon calls it
    /// every second with that second's warnings, not on status requests.
    pub fn log(&mut self, now: Instant, warnings: &[Warning]) {
        let (appeared, cleared) = self.changes(now, warnings);
        for w in appeared {
            warn!("health: {}", w.text);
        }
        for kind in cleared {
            info!("health: cleared: {}", kind.label());
        }
    }

    /// The warnings that appeared since the last call, and the kinds gone
    /// for [`CLEAR_HOLD`].
    fn changes<'w>(
        &mut self,
        now: Instant,
        warnings: &'w [Warning],
    ) -> (Vec<&'w Warning>, Vec<Kind>) {
        let appeared =
            warnings.iter().filter(|w| self.logged.insert(w.kind, now).is_none()).collect();
        let mut cleared = Vec::new();
        self.logged.retain(|&kind, &mut seen| {
            let gone = now.saturating_duration_since(seen) >= CLEAR_HOLD;
            if gone {
                cleared.push(kind);
            }
            !gone
        });
        (appeared, cleared)
    }

    /// The health in a few words for the status line: `healthy`, or
    /// `warnings: ` and the warnings of the last reading.
    pub fn summary(&self) -> String {
        if self.last.is_empty() {
            return "healthy".to_owned();
        }
        let shorts: Vec<&str> = self.last.iter().map(|w| w.short.as_str()).collect();
        format!("warnings: {}", shorts.join(", "))
    }

    /// Whether `s` was taken after the engine settled.
    fn settled(&self, s: &Sample) -> bool {
        self.settle_until.is_none_or(|t| s.at >= t)
    }

    /// How much `get` rose within the window, over the pairs of
    /// consecutive readings `pair` accepts. A counter that went back
    /// restarted, and counts nothing for that pair.
    fn rise(&self, pair: impl Fn(&Sample, &Sample) -> bool, get: impl Fn(&Counts) -> u64) -> u64 {
        self.pairs()
            .filter(|(a, b)| pair(a, b))
            .map(|(a, b)| get(&b.c).saturating_sub(get(&a.c)))
            .sum()
    }

    /// The lowest a margin (`get`, a minimum since IO started) fell to,
    /// within the window, among the falls to zero or below.
    fn margin_fell(&self, get: impl Fn(&Counts) -> i64) -> Option<i64> {
        self.pairs()
            .filter(|(a, b)| self.driver_pair(a, b))
            .map(|(a, b)| (get(&a.c), get(&b.c)))
            .filter(|&(before, after)| after < before && after <= 0)
            .map(|(_, after)| after)
            .min()
    }

    fn pairs(&self) -> impl Iterator<Item = (&Sample, &Sample)> {
        self.samples.iter().zip(self.samples.iter().skip(1))
    }

    /// Readings of the device and its clock, after the engine settled.
    fn engine_pair(&self, a: &Sample, b: &Sample) -> bool {
        a.engine && b.engine && self.settled(a)
    }

    /// Readings of the driver's IO, once it settled.
    fn driver_pair(&self, a: &Sample, b: &Sample) -> bool {
        a.hal && b.hal && a.io_settled && b.io && self.settled(a)
    }

    fn driver_warnings(
        &self,
        now: Instant,
        h: &HalStatus,
        engine: Option<&EngineReading>,
        failed: bool,
        stalled: bool,
        out: &mut Vec<Warning>,
    ) {
        let window = WINDOW.as_secs();
        if failed {
            out.push(Warning {
                kind: Kind::DriverFailed,
                urgent: true,
                audible: true,
                text: "The Core Audio driver has failed (it caught an internal error) and is \
                       silent: every input and every return stays silent until Core Audio \
                       restarts. Restart it with `sudo killall coreaudiod` (all audio on this Mac \
                       stops for a few seconds), then restart the audio of the application."
                    .to_owned(),
                short: "driver failed".to_owned(),
            });
            return;
        }
        if stalled {
            let age = h
                .io_heartbeat_age
                .map_or("never".to_owned(), |a| format!("{:.1} s ago", a.as_secs_f64()));
            out.push(Warning {
                kind: Kind::IoStalled,
                urgent: true,
                audible: true,
                text: format!(
                    "Core Audio stopped calling the OpenVirtualSoundcard driver while an \
                     application uses it (last call: {age}): inputs and returns are silent. \
                     Restart the application's audio; if that does not help, restart Core Audio \
                     with `sudo killall coreaudiod`."
                ),
                short: "driver IO stalled".to_owned(),
            });
        }
        if let Some(since) = self.gate_closed_since
            && now.saturating_duration_since(since) >= GATE_GRACE
        {
            let why = match engine {
                None => "The network engine is not running; it retries on its own, and its \
                         last error says why."
                    .to_owned(),
                Some(e)
                    if e.clock.state != ClockState::Locked
                        && e.clock.state != ClockState::FreeRunning =>
                {
                    format!(
                        "The clock is {} and audio starts once it locks.",
                        clock_state_words(&e.clock)
                    )
                }
                Some(_) => "This lasts seconds after a jump of the clock or a restart of the \
                            engine; if it goes on, restart the daemon."
                    .to_owned(),
            };
            out.push(Warning {
                kind: Kind::GateClosed,
                urgent: true,
                audible: true,
                text: format!(
                    "An application is using OpenVirtualSoundcard, but the driver holds the \
                     audio back: inputs are silent and the returns carry silence. {why}"
                ),
                short: "audio held back".to_owned(),
            });
        }

        let late = self.rise(|a, b| self.driver_pair(a, b), |c| c.late_out);
        let late_margin = self.margin_fell(|c| c.min_out_margin);
        if late > 0 || late_margin.is_some() {
            let worst = margin_words(h.min_output_margin);
            let what = if late > 0 {
                format!(
                    "{} from the application came too late to be sent in the last {window} s \
                     ({worst}): dropouts on the returns.",
                    count(late, "output cycle", "output cycles")
                )
            } else {
                format!(
                    "Output from the application ran out of margin in the last {window} s \
                     ({worst}): one cycle later is a dropout on the returns."
                )
            };
            out.push(Warning {
                kind: Kind::LateOutput,
                urgent: false,
                audible: late > 0,
                text: format!(
                    "{what} The application misses its deadlines: raise its buffer size, lower \
                     its load, or raise `[coreaudio] output_margin_us`."
                ),
                short: if late > 0 {
                    format!("{late} late output cycles")
                } else {
                    "output margin ran out".to_owned()
                },
            });
        }
        let early = self.rise(|a, b| self.driver_pair(a, b), |c| c.early_in);
        let early_margin = self.margin_fell(|c| c.min_in_margin);
        if early > 0 || early_margin.is_some() {
            let worst = margin_words(h.min_input_margin);
            let what = if early > 0 {
                format!(
                    "{} read before their audio had arrived in the last {window} s ({worst}): \
                     dropouts on the inputs.",
                    count(early, "input cycle was", "input cycles were")
                )
            } else {
                format!(
                    "Input was read with no margin left in the last {window} s ({worst}): one \
                     cycle earlier is a dropout on the inputs."
                )
            };
            out.push(Warning {
                kind: Kind::EarlyInput,
                urgent: false,
                audible: early > 0,
                text: format!("{what} Raise the latency, or `[coreaudio] input_margin_us`."),
                short: if early > 0 {
                    format!("{early} early input cycles")
                } else {
                    "input margin ran out".to_owned()
                },
            });
        }
        let silenced = self.rise(|a, b| self.driver_pair(a, b), |c| c.silenced);
        if silenced > 0 && self.gate_closed_since.is_none() {
            out.push(Warning {
                kind: Kind::Silenced,
                urgent: false,
                audible: true,
                text: format!(
                    "The driver held the audio back for {} in the last {window} s while an \
                     application was playing: dropouts on inputs and returns. It does so while \
                     the clock settles after a jump, or while the engine restarts.",
                    count(silenced, "IO cycle", "IO cycles")
                ),
                short: format!("{silenced} silenced IO cycles"),
            });
        }
        let slips = self.rise(|a, b| self.driver_pair(a, b), |c| c.absorbs);
        if slips > 0 {
            out.push(Warning {
                kind: Kind::Slips,
                urgent: false,
                audible: true,
                text: format!(
                    "The audio slipped {} in the last {window} s to follow a jump of the clock: \
                     a click on inputs and returns each time. The clock warnings and the log say \
                     why it jumped.",
                    count(slips, "time", "times")
                ),
                short: format!("{slips} audio slips"),
            });
        }
        let non_finite = self.rise(|a, b| a.hal && b.hal, |c| c.non_finite);
        if non_finite > 0 {
            out.push(Warning {
                kind: Kind::NonFinite,
                urgent: false,
                audible: true,
                text: format!(
                    "The application sent invalid samples (NaN or infinite) to \
                     OpenVirtualSoundcard's outputs in the last {window} s ({non_finite} \
                     channel-frames): they went out as silence. A plug-in has probably blown \
                     up: bypass it, or reload it."
                ),
                short: format!("{non_finite} invalid output samples"),
            });
        }
    }

    fn engine_warnings(&mut self, now: Instant, e: &EngineReading, out: &mut Vec<Warning>) {
        let window = WINDOW.as_secs();
        let s = &e.stats;
        if s.tx_engine_failed || e.stopped_part.is_some() {
            let what = match e.stopped_part {
                Some(part) if part != "transmit thread" => format!(
                    "The device's {part} has stopped: the device no longer works as a whole \
                     (subscriptions, flows or controllers may be stuck)."
                ),
                _ => "The transmit engine has stopped: nothing is sent to other devices, so the \
                      returns to the console are down."
                    .to_owned(),
            };
            out.push(Warning {
                kind: Kind::TransmitStopped,
                urgent: true,
                audible: true,
                text: format!(
                    "{what} The daemon restarts itself to recover (audio stops for 10–20 s); if \
                     this warning stays, restart it (on a Mac: `sudo launchctl kickstart -k \
                     system/org.openvirtualsoundcard.daemon`)."
                ),
                short: e.stopped_part.unwrap_or("transmit engine").to_owned() + " stopped",
            });
        }
        if e.ptp {
            self.clock_warnings(e, out);
        }

        // Receive.
        let mut silent = Vec::new();
        let mut not_receiving = HashMap::new();
        for (i, c) in e.rx.iter().enumerate() {
            let Some((channel, device)) = &c.subscription else { continue };
            if c.status.is_receiving() {
                continue;
            }
            let source = format!("{channel}@{device}");
            let key = (i, source.clone());
            let since = self.not_receiving.get(&key).copied().unwrap_or(now);
            not_receiving.insert(key, since);
            if now.saturating_duration_since(since) >= RX_GRACE {
                silent.push(format!(
                    "{} {} ← {source} ({})",
                    i + 1,
                    c.name,
                    super::subscription_words(c.status)
                ));
            }
        }
        self.not_receiving = not_receiving;
        if !silent.is_empty() {
            let n = silent.len();
            let mut named = silent[..n.min(NAMED_CHANNELS)].join("; ");
            if n > NAMED_CHANNELS {
                named.push_str(&format!("; and {} more", n - NAMED_CHANNELS));
            }
            let (subject, effect) = if n == 1 {
                ("A subscribed receive channel gets no audio", "that input is silent")
            } else {
                ("Subscribed receive channels get no audio", "those inputs are silent")
            };
            out.push(Warning {
                kind: Kind::RxNotReceiving,
                urgent: false,
                audible: true,
                text: format!(
                    "{subject}, so {effect}: {named}. Check the transmitters and the \
                     subscriptions in Dante Controller."
                ),
                short: format!("{n} subscribed channels silent"),
            });
        }
        let lost = self.rise(|a, b| self.engine_pair(a, b), |c| c.rx_lost);
        if lost > 0 {
            out.push(Warning {
                kind: Kind::RxLost,
                urgent: false,
                audible: true,
                text: format!(
                    "{} lost in the last {window} s: short dropouts on the inputs. Check the \
                     cables and switches between the transmitters and this computer.",
                    count(lost, "received packet was", "received packets were")
                ),
                short: format!("{lost} received packets lost"),
            });
        }
        let late = self.rise(|a, b| self.engine_pair(a, b), |c| c.rx_late);
        if late > 0 {
            out.push(Warning {
                kind: Kind::RxLate,
                urgent: false,
                audible: true,
                text: format!(
                    "{} too late to play in the last {window} s: short dropouts on the inputs. \
                     If it goes on, raise the latency (now {} ms), or look for congestion \
                     between the transmitters and this computer.",
                    count(late, "received packet arrived", "received packets arrived"),
                    decimal(e.latency_ms)
                ),
                short: format!("{late} received packets late"),
            });
        }
        let early = self.rise(|a, b| self.engine_pair(a, b), |c| c.rx_early);
        if early >= EARLY_MIN {
            out.push(Warning {
                kind: Kind::RxEarly,
                urgent: false,
                audible: false,
                text: format!(
                    "{early} received packets were stamped ahead of this computer's clock in the last \
                     {window} s: a transmitter's clock disagrees with ours (two clock leaders, \
                     or a transmitter that is not locked), and its audio may drop out. Check the \
                     clock status in Dante Controller."
                ),
                short: format!("{early} received packets early"),
            });
        }
        let rejected = self.rise(|a, b| self.engine_pair(a, b), |c| c.rx_rejected);
        if rejected >= REJECTED_MIN {
            out.push(Warning {
                kind: Kind::RxRejected,
                urgent: false,
                audible: false,
                text: format!(
                    "{rejected} datagrams sent to this device's receive flows were dropped in the \
                     last {window} s: they came from another sender than the flow's \
                     transmitter, or had the wrong size. A stray or misconfigured flow reaches \
                     this computer; the log names its sender."
                ),
                short: format!("{rejected} stray datagrams"),
            });
        }

        // Transmit.
        let errors = self.rise(|a, b| self.engine_pair(a, b), |c| c.tx_send_errors);
        if errors > 0 {
            let last = match s.tx_last_send_error {
                0 => String::new(),
                errno => format!(" (the last: {})", std::io::Error::from_raw_os_error(errno)),
            };
            out.push(Warning {
                kind: Kind::TxSendErrors,
                urgent: false,
                audible: true,
                text: format!(
                    "{} in the last {window} s{last}: the devices receiving from this one, such \
                     as the console's returns, heard dropouts. Check this computer's network \
                     interface and cable.",
                    count(
                        errors,
                        "packet to other devices could not be sent",
                        "packets to other devices could not be sent"
                    )
                ),
                short: format!("{errors} send errors"),
            });
        }
        let skipped = self.rise(|a, b| self.engine_pair(a, b), |c| c.tx_skipped);
        let resyncs = self.rise(|a, b| self.engine_pair(a, b), |c| c.tx_resyncs);
        if skipped > 0 || resyncs > 0 {
            // A flow's timeline can restart without a packet skipped (the
            // clock went back): say only what happened.
            let (what, short) = if skipped > 0 {
                (
                    format!(
                        "The transmit thread fell behind in the last {window} s: {} to other \
                         devices never sent, {} of a flow's timing",
                        count(skipped, "packet", "packets"),
                        count(resyncs, "restart", "restarts")
                    ),
                    format!("{skipped} packets skipped"),
                )
            } else {
                (
                    format!(
                        "The transmit thread restarted a flow's timing {} in the last {window} s",
                        count(resyncs, "time", "times")
                    ),
                    format!("{resyncs} transmit restarts"),
                )
            };
            out.push(Warning {
                kind: Kind::TxSkipped,
                urgent: false,
                audible: true,
                text: format!(
                    "{what}: a dropout on the returns. This computer was too busy for a \
                     moment, or the clock jumped."
                ),
                short,
            });
        }
        let tx_late = self.rise(|a, b| self.engine_pair(a, b), |c| c.tx_late);
        if tx_late >= TX_LATE_MIN {
            out.push(Warning {
                kind: Kind::TxLate,
                urgent: false,
                audible: false,
                text: format!(
                    "{tx_late} packets to other devices left more than 0.25 ms late in the last \
                     {window} s (the latest so far: {} ms after its time): receivers with a \
                     short latency may drop them. This computer is busy, or the transmit thread lacks \
                     real-time priority.",
                    decimal(s.tx_max_lateness_ns as f64 / 1e6)
                ),
                short: format!("{tx_late} packets sent late"),
            });
        }
        // Without an application playing, every packet goes out without
        // its audio: only a live audio path counts.
        let underruns = self.rise(
            |a, b| self.engine_pair(a, b) && a.live && b.live && a.io_settled,
            |c| c.tx_underruns,
        );
        if underruns > 0 {
            out.push(Warning {
                kind: Kind::TxUnderruns,
                urgent: false,
                audible: true,
                text: format!(
                    "{} without audio from the application in the last {window} s, while it \
                     was playing: dropouts on the returns. The application does not deliver \
                     its output in time: raise its buffer size, or lower its load.",
                    count(
                        underruns,
                        "packet to other devices was sent",
                        "packets to other devices were sent"
                    )
                ),
                short: format!("{underruns} packets without audio"),
            });
        }
        let expired = self.rise(|a, b| self.engine_pair(a, b), |c| c.tx_expired);
        if expired > 0 {
            out.push(Warning {
                kind: Kind::TxExpired,
                urgent: false,
                audible: false,
                text: format!(
                    "{} in the last {window} s: the receivers stopped confirming them for 4 s (a \
                     cable pulled, a device restarted). They come back when the receivers ask \
                     again; if a console lost its returns, check its subscriptions.",
                    count(
                        expired,
                        "flow to another device closed",
                        "flows to other devices closed"
                    )
                ),
                short: format!("{expired} flows expired"),
            });
        }
        if s.tx_not_realtime {
            out.push(Warning {
                kind: Kind::TxNotRealtime,
                urgent: false,
                audible: false,
                text: "The transmit thread runs without real-time priority, so the returns may \
                       leave late while this computer is busy. Restart the daemon; if it stays, the \
                       log says why."
                    .to_owned(),
                short: "transmit thread not real-time".to_owned(),
            });
        }
        if let Some(conflict) = &e.name_conflict {
            out.push(Warning {
                kind: Kind::NameConflict,
                urgent: false,
                audible: false,
                text: format!(
                    "Name conflict: {conflict}. Give each device its own name (Settings here, or \
                     Dante Controller for the other), then check the subscriptions to this \
                     device."
                ),
                short: "name conflict".to_owned(),
            });
        }
    }

    fn clock_warnings(&self, e: &EngineReading, out: &mut Vec<Warning>) {
        let window = WINDOW.as_secs();
        let c = &e.clock;
        if !self.clock_locked {
            if c.state != ClockState::Locked && e.clock_age > LOCK_WARNING_AFTER {
                let why = match c.master {
                    None => {
                        format!(
                            "no Dante clock leader is reaching this computer on {}",
                            e.interface
                        )
                    }
                    Some(m) => format!(
                        "it hears the leader {} on {} but cannot follow it, as if its messages \
                         were delayed",
                        m.addr, e.interface
                    ),
                };
                out.push(Warning {
                    kind: Kind::ClockNotLocked,
                    urgent: true,
                    audible: true,
                    text: format!(
                        "The clock has not locked: {why}. Until it locks, inputs and returns are \
                         silent. Check the cable and the interface; \"The clock does not lock\" in \
                         docs/MACOS.md lists the usual causes."
                    ),
                    short: "clock not locked".to_owned(),
                });
            }
            return;
        }
        let leader = c.master.map(|m| format!(" {}", m.addr)).unwrap_or_default();
        match (c.state, c.holdover_until_ns) {
            (ClockState::Locked, _) | (ClockState::FreeRunning, _) => {}
            (_, Some(until)) if c.master.is_none() => {
                let left = until.saturating_sub(e.local_ns) / 1_000_000_000;
                out.push(Warning {
                    kind: Kind::Holdover,
                    urgent: false,
                    audible: false,
                    text: format!(
                        "No clock leader is heard any more: the clock runs on by itself \
                         (holdover) for another {left} s. Inputs and returns keep playing, \
                         drifting slowly from the network; if no leader is heard by then, audio \
                         stops both ways. Check the leader (console or stagebox) and the \
                         network between it and this computer."
                    ),
                    short: format!("clock in holdover, {left} s left"),
                });
            }
            (ClockState::Unlocked, _) => out.push(Warning {
                kind: Kind::ClockLost,
                urgent: true,
                audible: true,
                text: "The clock lost its leader and its holdover ran out: inputs and returns are \
                       silent until a Dante clock leader is heard again. Check the leader \
                       (console or stagebox) and the network between it and this computer."
                    .to_owned(),
                short: "clock lost".to_owned(),
            }),
            (_, _) => out.push(Warning {
                kind: Kind::ClockRelocking,
                urgent: false,
                audible: false,
                text: format!(
                    "The clock is locking to its leader{leader} again, after the leader's \
                     messages were delayed or a new leader took over: audio keeps flowing \
                     unless the clock has to jump. If this happens often, give PTP priority \
                     (QoS) on the switches and turn off Energy-Efficient Ethernet."
                ),
                short: "clock relocking".to_owned(),
            }),
        }
        let unlocks = self.rise(|a, b| self.engine_pair(a, b), |c| c.unlocks);
        if unlocks > 0 && c.state == ClockState::Locked {
            out.push(Warning {
                kind: Kind::ClockRelocking,
                urgent: false,
                audible: false,
                text: format!(
                    "The clock lost lock {} in the last {window} s and pulled back in: its \
                     leader's messages are being delayed. Audio kept flowing; give PTP priority \
                     (QoS) on the switches and turn off Energy-Efficient Ethernet.",
                    count(unlocks, "time", "times")
                ),
                short: format!("clock lost lock {unlocks}×"),
            });
        }
        let steps = self.rise(|a, b| self.engine_pair(a, b), |c| c.steps);
        if steps > 0 {
            out.push(Warning {
                kind: Kind::ClockStepped,
                urgent: false,
                audible: true,
                text: format!(
                    "The clock jumped {} in the last {window} s (the last by {} ms): the \
                     leader's time moved, or a new leader's time differed. Audio stopped until \
                     the clock locked again, then slipped once.",
                    count(steps, "time", "times"),
                    decimal(c.last_step_ns as f64 / 1e6)
                ),
                short: format!("clock jumped {steps}×"),
            });
        }
        let changes = self.rise(|a, b| self.engine_pair(a, b), |c| c.master_changes);
        if changes > 0 {
            let now = if leader.is_empty() { String::new() } else { format!(", now{leader}") };
            out.push(Warning {
                kind: Kind::LeaderChanged,
                urgent: false,
                audible: false,
                text: format!(
                    "The clock leader changed {} in the last {window} s{now}. Audio keeps \
                     flowing when the new leader keeps the old one's time. Check why it \
                     changed: a device restarted, a cable, the Preferred Leader setting.",
                    count(changes, "time", "times")
                ),
                short: format!("clock leader changed {changes}×"),
            });
        }
    }
}

/// The clock's state in words, for the gate's warning.
fn clock_state_words(c: &ClockStatus) -> &'static str {
    match (c.state, c.holdover_until_ns) {
        (ClockState::Locking, _) => "locking",
        (ClockState::Unlocked, Some(_)) => "in holdover",
        (ClockState::Unlocked, None) => "not locked",
        (ClockState::Locked, _) => "locked",
        (ClockState::FreeRunning, _) => "free running",
    }
}

/// The worst margin since IO started, in words.
fn margin_words(frames: i64) -> String {
    if frames == i64::MAX {
        "no margin measured yet".to_owned()
    } else {
        format!("worst margin since IO started: {frames} frames")
    }
}

/// `n` and the noun phrase for that many: `1 packet was`, `3 packets were`.
fn count(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// `x` with at most two decimals and no trailing zeros: `4`, `0.25`.
fn decimal(x: f64) -> String {
    let s = format!("{x:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use ovsc_clock::MasterInfo;
    use ovsc_proto::arc::SubscriptionStatus;

    use super::*;

    /// Readings at `t0 + s` seconds.
    struct Clock(Instant);

    impl Clock {
        fn at(&self, s: f64) -> Instant {
            self.0 + Duration::from_secs_f64(s)
        }
    }

    fn clock() -> Clock {
        Clock(Instant::now())
    }

    /// A connected driver: IO running or not, gate open, a fresh heartbeat.
    fn hal(io: bool) -> HalStatus {
        HalStatus {
            peers: 1,
            engine_running: true,
            io_clients: u64::from(io),
            gate: true,
            min_output_margin: if io { 40 } else { i64::MAX },
            min_input_margin: if io { 40 } else { i64::MAX },
            io_heartbeat_age: io.then_some(Duration::from_millis(2)),
            plugin_instance: 7,
            ..Default::default()
        }
    }

    fn engine() -> EngineReading {
        EngineReading {
            stats: DeviceStats::default(),
            clock: ClockStatus {
                state: ClockState::Locked,
                master: Some(MasterInfo {
                    uuid: [0; 6],
                    port_id: 1,
                    addr: Ipv4Addr::new(192, 168, 1, 2),
                }),
                ..Default::default()
            },
            ptp: true,
            clock_age: Duration::from_secs(100),
            local_ns: 1_000_000_000_000,
            latency_ms: 2.0,
            interface: "en7".into(),
            rx: Vec::new(),
            stopped_part: None,
            name_conflict: None,
        }
    }

    fn reading(hal: HalStatus, engine: EngineReading) -> Reading {
        Reading { hal: Some(hal), engine: Some(engine) }
    }

    fn kinds(a: &Assessment) -> Vec<Kind> {
        a.warnings.iter().map(|w| w.kind).collect()
    }

    fn find(a: &Assessment, kind: Kind) -> &Warning {
        a.warnings.iter().find(|w| w.kind == kind).unwrap_or_else(|| panic!("no {kind:?}: {a:?}"))
    }

    /// Feeds one reading a second from `from` to `to` (inclusive), made by
    /// `make` from the second; returns the last assessment.
    fn feed(
        h: &mut Health,
        c: &Clock,
        from: u32,
        to: u32,
        make: impl Fn(u32) -> Reading,
    ) -> Assessment {
        let mut last = Assessment::default();
        for s in from..=to {
            last = h.observe(c.at(f64::from(s)), &make(s));
        }
        last
    }

    #[test]
    fn a_healthy_device_has_no_warnings() {
        let c = clock();
        let mut h = Health::new();
        let a = feed(&mut h, &c, 0, 40, |_| reading(hal(true), engine()));
        assert_eq!(a, Assessment::default());
        assert_eq!(h.summary(), "healthy");
    }

    #[test]
    fn a_rising_counter_warns_for_the_window_then_clears() {
        let c = clock();
        let mut h = Health::new();
        let lost = |s: u32| {
            let mut e = engine();
            // Three packets lost at second 20, two more at 25.
            e.stats.rx_lost_packets = 100
                + if s >= 25 {
                    5
                } else if s >= 20 {
                    3
                } else {
                    0
                };
            reading(hal(true), e)
        };
        assert!(feed(&mut h, &c, 0, 19, lost).warnings.is_empty());
        let a = feed(&mut h, &c, 20, 20, lost);
        let w = find(&a, Kind::RxLost);
        assert!(w.text.starts_with("3 received packets were lost in the last 30 s"), "{}", w.text);
        assert!(w.audible && !w.urgent);
        assert_eq!(h.summary(), "warnings: 3 received packets lost");
        // The window counts both rises, and holds 30 s after the last.
        let a = feed(&mut h, &c, 21, 54, lost);
        assert_eq!(find(&a, Kind::RxLost).short, "2 received packets lost");
        let a = feed(&mut h, &c, 55, 56, lost);
        assert!(a.warnings.is_empty(), "{a:?}");
        // One lost packet is a dropout: it warns too.
        let a = h.observe(c.at(57.0), &{
            let mut r = lost(57);
            r.engine.as_mut().unwrap().stats.rx_lost_packets += 1;
            r
        });
        assert!(find(&a, Kind::RxLost).text.starts_with("1 received packet was lost"));
    }

    #[test]
    fn rises_while_the_engine_settles_never_count() {
        let c = clock();
        let mut h = Health::new();
        feed(&mut h, &c, 0, 5, |_| reading(hal(true), engine()));
        h.restarted(c.at(5.5));
        // The new device's flows connect: late and lost packets.
        let rough = |s: u32| {
            let mut e = engine();
            e.stats.rx_late_packets = u64::from(s.min(14)) * 3;
            e.stats.rx_lost_packets = u64::from(s.min(14));
            reading(hal(true), e)
        };
        let a = feed(&mut h, &c, 6, 30, rough);
        assert!(a.warnings.is_empty(), "{a:?}");
        // After the settle, the same rise warns.
        let a = h.observe(c.at(31.0), &{
            let mut r = rough(31);
            r.engine.as_mut().unwrap().stats.rx_late_packets += 1;
            r
        });
        assert_eq!(kinds(&a), [Kind::RxLate]);
        assert!(find(&a, Kind::RxLate).text.contains("raise the latency (now 2 ms)"));
    }

    #[test]
    fn noise_thresholds() {
        let c = clock();
        let mut h = Health::new();
        let early = |n: u64| {
            move |_: u32| {
                let mut e = engine();
                e.stats.rx_early_packets = n;
                e.stats.rx_rejected_packets = n;
                e.stats.tx_late_packets = n;
                reading(hal(true), e)
            }
        };
        feed(&mut h, &c, 0, 10, early(0));
        assert!(feed(&mut h, &c, 11, 11, early(4)).warnings.is_empty());
        let a = feed(&mut h, &c, 12, 12, early(5));
        assert_eq!(kinds(&a), [Kind::RxEarly, Kind::RxRejected, Kind::TxLate]);
        assert!(!a.audible());
    }

    #[test]
    fn a_failed_driver_is_urgent_until_a_new_driver_clears_it() {
        let c = clock();
        let mut h = Health::new();
        let failed = HalStatus { faulted: true, ..hal(true) };
        let a = h.observe(c.at(0.0), &reading(failed, engine()));
        assert!(a.driver_failed);
        let w = find(&a, Kind::DriverFailed);
        assert!(w.urgent && w.text.contains("`sudo killall coreaudiod`"), "{}", w.text);
        assert_eq!(a.warnings[0].kind, Kind::DriverFailed);
        // Core Audio restarted: a new driver instance, without IO yet, has
        // not rewritten its predecessor's flag.
        let reloaded = HalStatus { faulted: true, plugin_instance: 8, ..hal(false) };
        let a = h.observe(c.at(5.0), &reading(reloaded, engine()));
        assert!(!a.driver_failed && a.warnings.is_empty(), "{a:?}");
        // Once it does IO, the flag is its own.
        let again = HalStatus { faulted: true, plugin_instance: 8, ..hal(true) };
        assert!(h.observe(c.at(6.0), &reading(again, engine())).driver_failed);
        let fine = HalStatus { plugin_instance: 8, ..hal(true) };
        assert!(!h.observe(c.at(7.0), &reading(fine, engine())).driver_failed);
        // A driver that is not connected says nothing.
        let gone = HalStatus { peers: 0, faulted: true, ..hal(true) };
        assert_eq!(h.observe(c.at(8.0), &reading(gone, engine())), Assessment::default());
    }

    #[test]
    fn a_closed_gate_while_an_application_plays_warns_after_a_grace() {
        let c = clock();
        let mut h = Health::new();
        let closed = HalStatus { gate: false, ..hal(true) };
        feed(&mut h, &c, 0, 9, |_| reading(hal(true), engine()));
        assert!(h.observe(c.at(10.0), &reading(closed, engine())).warnings.is_empty());
        let a = h.observe(c.at(11.5), &reading(closed, engine()));
        assert!(a.warnings.is_empty(), "{a:?}");
        let a = h.observe(c.at(12.0), &reading(closed, engine()));
        let w = find(&a, Kind::GateClosed);
        assert!(w.urgent && w.text.contains("inputs are silent"), "{}", w.text);
        assert!(w.text.contains("if it goes on, restart the daemon"), "{}", w.text);
        // Without an engine, or a locked clock, it says so.
        let a = h.observe(c.at(13.0), &Reading { hal: Some(closed), engine: None });
        assert!(find(&a, Kind::GateClosed).text.contains("network engine is not running"));
        let mut locking = engine();
        locking.clock.state = ClockState::Locking;
        let a = h.observe(c.at(14.0), &reading(closed, locking));
        assert!(find(&a, Kind::GateClosed).text.contains("The clock is locking"));
        // No application, no warning: nothing is held back.
        let idle = HalStatus { gate: false, ..hal(false) };
        assert!(h.observe(c.at(15.0), &reading(idle, engine())).warnings.is_empty());
    }

    #[test]
    fn a_stalled_io_thread_is_urgent_once_io_settled() {
        let c = clock();
        let mut h = Health::new();
        let stale = HalStatus { io_heartbeat_age: Some(Duration::from_secs(4)), ..hal(true) };
        // Just started: the first operation may not have come yet.
        let a = h.observe(c.at(0.0), &reading(stale, engine()));
        assert!(!a.io_stalled && a.warnings.is_empty());
        let a = h.observe(c.at(3.0), &reading(stale, engine()));
        assert!(a.io_stalled);
        assert!(find(&a, Kind::IoStalled).text.contains("last call: 4.0 s ago"));
        let a = h.observe(c.at(4.0), &reading(hal(true), engine()));
        assert!(!a.io_stalled && a.warnings.is_empty());
        // Stopped IO has no heartbeat to keep.
        let idle = HalStatus { io_heartbeat_age: Some(Duration::from_secs(60)), ..hal(false) };
        assert!(!h.observe(c.at(5.0), &reading(idle, engine())).io_stalled);
    }

    #[test]
    fn transmit_underruns_count_only_while_an_application_plays() {
        let c = clock();
        let mut h = Health::new();
        let underruns = |io: bool| {
            move |s: u32| {
                let mut e = engine();
                e.stats.tx_underruns = u64::from(s) * 1000;
                reading(hal(io), e)
            }
        };
        // No application: every packet is an underrun, and that is fine.
        assert!(feed(&mut h, &c, 0, 20, underruns(false)).warnings.is_empty());
        // An application starts: its first seconds settle.
        assert!(feed(&mut h, &c, 21, 23, underruns(true)).warnings.is_empty());
        let a = feed(&mut h, &c, 24, 25, underruns(true));
        assert!(find(&a, Kind::TxUnderruns).text.starts_with("1000 packets to other devices"));
        // A closed gate drops the output on purpose: not the application's
        // fault.
        let mut h = Health::new();
        let held = |s: u32| {
            let mut r = underruns(true)(s);
            r.hal.as_mut().unwrap().gate = false;
            r
        };
        let a = feed(&mut h, &c, 0, 10, held);
        assert_eq!(kinds(&a), [Kind::GateClosed]);
    }

    #[test]
    fn driver_cycles_warn_after_io_settles() {
        let c = clock();
        let mut h = Health::new();
        let cycles = |late: u64, margin: i64| {
            move |_: u32| {
                let hal = HalStatus {
                    late_output_cycles: late,
                    min_output_margin: margin,
                    absorbs: late,
                    non_finite_out: late * 10,
                    ..hal(true)
                };
                reading(hal, engine())
            }
        };
        // Rises in the first seconds of IO are the gate opening.
        feed(&mut h, &c, 0, 1, cycles(0, 40));
        assert!(
            feed(&mut h, &c, 2, 2, cycles(2, -3))
                .warnings
                .iter()
                .all(|w| w.kind == Kind::NonFinite)
        );
        feed(&mut h, &c, 3, 10, cycles(2, -3));
        let a = feed(&mut h, &c, 11, 11, cycles(3, -7));
        assert_eq!(kinds(&a), [Kind::LateOutput, Kind::Slips, Kind::NonFinite]);
        let w = find(&a, Kind::LateOutput);
        assert!(w.text.starts_with("1 output cycle from the application came too late"));
        assert!(w.text.contains("worst margin since IO started: -7 frames"), "{}", w.text);
        assert!(a.audible());
        // A margin that falls to zero warns without a late cycle, as not
        // yet audible.
        let mut h = Health::new();
        feed(&mut h, &c, 0, 5, |_| reading(hal(true), engine()));
        let a = h.observe(c.at(6.0), &cycles(0, 0)(6));
        let w = find(&a, Kind::LateOutput);
        assert!(w.text.starts_with("Output from the application ran out of margin"), "{}", w.text);
        assert!(w.text.contains("(worst margin since IO started: 0 frames)"), "{}", w.text);
        assert!(!w.audible);
        // The status line does not count zero late cycles.
        assert_eq!(h.summary(), "warnings: output margin ran out");
    }

    #[test]
    fn subscribed_channels_without_audio_warn_after_a_grace() {
        let c = clock();
        let mut h = Health::new();
        let rx = |status| RxChannelStatus {
            name: "Vox".into(),
            subscription: Some(("lead".into(), "stagebox".into())),
            status,
        };
        let channels = |s: u32| {
            let mut e = engine();
            e.rx = vec![
                rx(SubscriptionStatus::ReceivingUnicast),
                rx(if s < 30 {
                    SubscriptionStatus::Unresolved
                } else {
                    SubscriptionStatus::ReceivingUnicast
                }),
                RxChannelStatus {
                    name: "Spare".into(),
                    subscription: None,
                    status: SubscriptionStatus::None,
                },
            ];
            reading(hal(true), e)
        };
        assert!(feed(&mut h, &c, 0, 9, channels).warnings.is_empty());
        let a = feed(&mut h, &c, 10, 10, channels);
        let w = find(&a, Kind::RxNotReceiving);
        assert_eq!(
            w.text,
            "A subscribed receive channel gets no audio, so that input is silent: 2 Vox ← \
             lead@stagebox (transmitter not found). Check the transmitters and the subscriptions \
             in Dante Controller."
        );
        assert!(feed(&mut h, &c, 11, 30, channels).warnings.is_empty());
    }

    #[test]
    fn the_clock_warns_once_it_had_locked() {
        let c = clock();
        let mut h = Health::new();
        let with = |state, holdover: Option<u64>, master: bool, age: u64| {
            let mut e = engine();
            e.clock.state = state;
            e.clock.holdover_until_ns = holdover;
            if !master {
                e.clock.master = None;
            }
            e.clock_age = Duration::from_secs(age);
            reading(hal(true), e)
        };
        // Never locked: after 20 s, why not.
        let a = h.observe(c.at(0.0), &with(ClockState::Unlocked, None, false, 10));
        assert!(a.warnings.is_empty());
        let a = h.observe(c.at(15.0), &with(ClockState::Unlocked, None, false, 25));
        let w = find(&a, Kind::ClockNotLocked);
        assert!(
            w.urgent
                && w.text.starts_with(
                    "The clock has not locked: no Dante clock leader is reaching this computer on en7."
                )
        );
        // Locked, then the leader goes: holdover, with the time left.
        assert!(
            h.observe(c.at(16.0), &with(ClockState::Locked, None, true, 26)).warnings.is_empty()
        );
        let until = engine().local_ns + 12_500_000_000;
        let a = h.observe(c.at(17.0), &with(ClockState::Unlocked, Some(until), false, 27));
        let w = find(&a, Kind::Holdover);
        assert!(!w.urgent && w.text.contains("for another 12 s"), "{}", w.text);
        // The holdover ran out: silent.
        let a = h.observe(c.at(30.0), &with(ClockState::Unlocked, None, false, 40));
        assert!(find(&a, Kind::ClockLost).urgent);
        // Relocking to a leader.
        let a = h.observe(c.at(31.0), &with(ClockState::Locking, None, true, 41));
        assert!(find(&a, Kind::ClockRelocking).text.contains("leader 192.168.1.2 again"));
        // A new clock (the engine restarted) has to lock again first.
        let a = h.observe(c.at(32.0), &with(ClockState::Unlocked, None, false, 1));
        assert!(a.warnings.is_empty(), "{a:?}");
        // A free-running clock never warns.
        let mut free = with(ClockState::FreeRunning, None, false, 100);
        free.engine.as_mut().unwrap().ptp = false;
        assert!(h.observe(c.at(33.0), &free).warnings.is_empty());
    }

    #[test]
    fn clock_events_within_the_window() {
        let c = clock();
        let mut h = Health::new();
        let events = |s: u32| {
            let mut e = engine();
            if s >= 10 {
                e.clock.unlocks = 1;
                e.clock.steps = 1;
                e.clock.last_step_ns = -12_340_000;
                e.clock.master_changes = 1;
            }
            reading(hal(true), e)
        };
        let a = feed(&mut h, &c, 0, 10, events);
        assert_eq!(kinds(&a), [Kind::ClockRelocking, Kind::ClockStepped, Kind::LeaderChanged]);
        assert!(find(&a, Kind::ClockStepped).text.contains("(the last by -12.34 ms)"));
        assert!(find(&a, Kind::LeaderChanged).text.contains("now 192.168.1.2"));
        assert!(feed(&mut h, &c, 11, 41, events).warnings.is_empty());
    }

    #[test]
    fn transmit_problems() {
        let c = clock();
        let mut h = Health::new();
        let tx = |s: u32| {
            let mut e = engine();
            if s >= 5 {
                e.stats.tx_send_errors = 3;
                e.stats.tx_last_send_error = libc::ENOBUFS;
                e.stats.tx_skipped_packets = 20;
                e.stats.tx_resyncs = 1;
                e.stats.tx_expired_flows = 1;
                e.stats.tx_not_realtime = true;
                e.name_conflict = Some(NameConflict {
                    name: "ovsc".into(),
                    peer: Ipv4Addr::new(192, 168, 1, 9),
                    renamed_to: None,
                });
            }
            reading(hal(true), e)
        };
        let a = feed(&mut h, &c, 0, 5, tx);
        assert_eq!(
            kinds(&a),
            [
                Kind::TxSendErrors,
                Kind::TxSkipped,
                Kind::TxExpired,
                Kind::TxNotRealtime,
                Kind::NameConflict
            ]
        );
        let errors = &find(&a, Kind::TxSendErrors).text;
        assert!(errors.starts_with("3 packets to other devices could not be sent"), "{errors}");
        assert!(errors.contains("(the last: No buffer space available"), "{errors}");
        assert!(
            find(&a, Kind::TxSkipped)
                .text
                .contains("20 packets to other devices never sent, 1 restart")
        );
        assert!(find(&a, Kind::NameConflict).text.contains("192.168.1.9 also uses the name ovsc"));
        // A stopped transmit engine is urgent and comes first.
        let mut r = tx(6);
        r.engine.as_mut().unwrap().stats.tx_engine_failed = true;
        let a = h.observe(c.at(6.0), &r);
        let w = &a.warnings[0];
        assert!(w.urgent && w.text.starts_with("The transmit engine has stopped"), "{}", w.text);
        r.engine.as_mut().unwrap().stopped_part = Some("rx manager");
        let a = h.observe(c.at(7.0), &r);
        assert!(a.warnings[0].text.starts_with("The device's rx manager has stopped"));

        // A flow's timing restarted without a packet skipped: no "0
        // packets" in the words.
        let mut h = Health::new();
        let resync = |s: u32| {
            let mut e = engine();
            e.stats.tx_resyncs = u64::from(s >= 5) * 2;
            reading(hal(true), e)
        };
        let a = feed(&mut h, &c, 0, 5, resync);
        let w = find(&a, Kind::TxSkipped);
        assert!(
            w.text.starts_with("The transmit thread restarted a flow's timing 2 times in the last"),
            "{}",
            w.text
        );
        assert_eq!(w.short, "2 transmit restarts");
    }

    #[test]
    fn fast_readings_do_not_grow_the_window() {
        let c = clock();
        let mut h = Health::new();
        for i in 0..2000 {
            h.observe(c.at(f64::from(i) * 0.01), &reading(hal(true), engine()));
        }
        // 20 s at 100 readings a second: about four a second are kept (one
        // per 250 ms, give or take a reading), not a hundred.
        assert!((75..=90).contains(&h.samples.len()), "{}", h.samples.len());
        for i in 0..100 {
            h.observe(c.at(20.0 + f64::from(i)), &reading(hal(true), engine()));
        }
        // A window's worth, and one before it.
        assert_eq!(h.samples.len(), 31);
    }

    #[test]
    fn the_log_says_once_when_a_warning_appears_and_clears() {
        let c = clock();
        let mut h = Health::new();
        let w = |kind| Warning {
            kind,
            urgent: false,
            audible: true,
            text: String::new(),
            short: String::new(),
        };
        let lost = [w(Kind::RxLost)];
        let (appeared, cleared) = h.changes(c.at(0.0), &lost);
        assert_eq!((appeared.len(), cleared.len()), (1, 0));
        // Still there, or back within the hold: nothing new to say.
        assert_eq!(h.changes(c.at(1.0), &lost).0.len(), 0);
        assert_eq!(h.changes(c.at(5.0), &[]), (vec![], vec![]));
        assert_eq!(h.changes(c.at(9.0), &lost).0.len(), 0);
        // Gone for the hold: cleared, once.
        assert_eq!(h.changes(c.at(19.0), &[]).1, [Kind::RxLost]);
        assert_eq!(h.changes(c.at(30.0), &[]), (vec![], vec![]));
        // And it may appear again.
        assert_eq!(h.changes(c.at(31.0), &lost).0.len(), 1);
    }
}
