//! Watching the streams from outside the audio callbacks, which can't log.
//!
//! A [`Watch`] compares successive [`DirectionStats`] snapshots of one
//! direction and says what is worth a log line: state changes (device lost,
//! stalled, rerouted, lock lost or regained, callback size), real-time
//! priority refused, and growing counters (xruns, underruns, jumps), the
//! latter at most once per [`REPEAT_SECONDS`] so that a burst of xruns is
//! one warning, not a flood. The bridge runs it on a small thread; the
//! logic is pure so the tests drive it with synthetic snapshots.

use crate::stats::DirectionStats;

/// Shortest time between two reports of the same counter, seconds.
pub(crate) const REPEAT_SECONDS: f64 = 5.0;

/// How important a report is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Level {
    Info,
    Warn,
}

/// A counter of [`DirectionStats`] that only grows.
#[derive(Clone, Copy, Debug)]
enum Counter {
    DeviceErrors,
    Underruns,
    Overruns,
    Realigns,
}

impl Counter {
    const ALL: [Counter; 4] =
        [Counter::DeviceErrors, Counter::Underruns, Counter::Overruns, Counter::Realigns];

    fn get(self, s: &DirectionStats) -> u64 {
        match self {
            Counter::DeviceErrors => s.device_errors,
            Counter::Underruns => s.underruns,
            Counter::Overruns => s.overruns,
            Counter::Realigns => s.realigns,
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Counter::DeviceErrors => "device errors (xruns: the device lost audio)",
            Counter::Underruns => {
                "underruns (audio touched too close to the network's edge; raise margin_ms if \
                 this keeps growing)"
            }
            Counter::Overruns => "overruns (audio too far from the network's edge)",
            Counter::Realigns => "jumps back onto the timeline (each a dip of a few ms)",
        }
    }
}

/// The last report of one counter.
#[derive(Clone, Copy, Debug, Default)]
struct Reported {
    total: u64,
    at: Option<f64>,
}

/// Turns snapshots of one direction into log reports.
#[derive(Debug)]
pub(crate) struct Watch {
    what: &'static str,
    last: Option<DirectionStats>,
    reported: [Reported; 4],
}

impl Watch {
    /// `what` names the direction in reports ("playback", "capture").
    pub(crate) fn new(what: &'static str) -> Self {
        Watch { what, last: None, reported: [Reported::default(); 4] }
    }

    /// Compares `s`, taken at `now` (seconds, any origin), with the
    /// previous snapshot and returns what to log.
    pub(crate) fn check(&mut self, now: f64, s: &DirectionStats) -> Vec<(Level, String)> {
        let mut out = Vec::new();
        let who = format!("{} {:?}", self.what, s.device);
        let prev = self.last.take().unwrap_or_default();
        if s.device_lost && !prev.device_lost {
            out.push((Level::Warn, format!("{who}: the device is gone; its stream has stopped")));
        }
        if s.stalled && !prev.stalled {
            out.push((
                Level::Warn,
                format!(
                    "{who}: no callback for {:.0} ms: the device stopped without reporting an \
                     error, and this direction is silent",
                    s.since_callback_ms
                ),
            ));
        } else if !s.stalled && prev.stalled {
            out.push((Level::Info, format!("{who}: callbacks resumed")));
        }
        if s.device_changed && !prev.device_changed {
            out.push((
                Level::Warn,
                format!(
                    "{who}: the system moved this stream to another device (it follows the \
                     system default); name the device explicitly for live use"
                ),
            ));
        }
        if s.realtime != prev.realtime {
            match s.realtime {
                Some(false) => out.push((
                    Level::Warn,
                    format!(
                        "{who}: callbacks run at normal priority (real-time scheduling was \
                         refused), so a busy machine will cause xruns. On Linux, grant \
                         CAP_SYS_NICE or run as root"
                    ),
                )),
                Some(true) => {
                    out.push((Level::Info, format!("{who}: callbacks run with real-time priority")))
                }
                None => {}
            }
        }
        if s.buffer_frames != prev.buffer_frames && s.buffer_frames > 0 {
            let level = if prev.buffer_frames > 0 && s.buffer_frames > prev.buffer_frames {
                Level::Warn
            } else {
                Level::Info
            };
            out.push((
                level,
                format!(
                    "{who}: callbacks of {} frames; the bridge adds {:.2} ms",
                    s.buffer_frames, s.latency_ms
                ),
            ));
        }
        if s.locked && !prev.locked {
            out.push((Level::Info, format!("{who}: locked, {:+.1} ppm", s.ratio_ppm)));
        } else if prev.locked && !s.locked && s.running {
            out.push((
                Level::Warn,
                format!("{who}: lost lock ({:+.1} samples off); re-acquiring", s.error_samples),
            ));
        }
        for (i, counter) in Counter::ALL.into_iter().enumerate() {
            let total = counter.get(s);
            let r = &mut self.reported[i];
            let due = r.at.is_none_or(|at| now - at >= REPEAT_SECONDS);
            if total > r.total && due {
                let since = match r.at {
                    Some(at) => format!("in the last {:.0} s", now - at),
                    None => "so far".to_owned(),
                };
                out.push((
                    Level::Warn,
                    format!(
                        "{who}: {} {} {since} ({total} in all)",
                        total - r.total,
                        counter.describe()
                    ),
                ));
                *r = Reported { total, at: Some(now) };
            }
        }
        self.last = Some(s.clone());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy() -> DirectionStats {
        DirectionStats {
            device: "Loopback".into(),
            buffer_frames: 128,
            latency_ms: 4.33,
            running: true,
            locked: true,
            realtime: Some(true),
            callbacks: 1000,
            ..Default::default()
        }
    }

    fn warnings(reports: &[(Level, String)]) -> Vec<&str> {
        reports.iter().filter(|(l, _)| *l == Level::Warn).map(|(_, m)| m.as_str()).collect()
    }

    #[test]
    fn a_healthy_stream_reports_once_then_stays_quiet() {
        let mut w = Watch::new("capture");
        let first = w.check(0.0, &healthy());
        assert!(warnings(&first).is_empty(), "{first:?}");
        assert!(first.iter().any(|(_, m)| m.contains("128 frames") && m.contains("4.33 ms")));
        assert!(first.iter().any(|(_, m)| m.contains("locked")));
        for t in 1..100 {
            assert!(w.check(t as f64 * 0.25, &healthy()).is_empty());
        }
    }

    #[test]
    fn state_changes_are_reported() {
        let mut w = Watch::new("playback");
        w.check(0.0, &healthy());
        let stalled = DirectionStats {
            stalled: true,
            running: false,
            locked: false,
            since_callback_ms: 340.0,
            ..healthy()
        };
        let r = w.check(1.0, &stalled);
        assert_eq!(warnings(&r).len(), 1, "{r:?}");
        assert!(warnings(&r)[0].contains("no callback for 340 ms"));
        assert!(w.check(1.25, &stalled).is_empty());
        let r = w.check(2.0, &healthy());
        assert!(r.iter().any(|(l, m)| *l == Level::Info && m.contains("resumed")), "{r:?}");

        let lost = DirectionStats { device_lost: true, ..healthy() };
        assert!(warnings(&w.check(3.0, &lost))[0].contains("gone"));
        let moved = DirectionStats { device_changed: true, ..lost.clone() };
        assert!(warnings(&w.check(3.5, &moved))[0].contains("another device"));

        let unlocked = DirectionStats { locked: false, error_samples: 30.0, ..moved };
        assert!(warnings(&w.check(4.0, &unlocked))[0].contains("lost lock"));

        let bigger = DirectionStats { buffer_frames: 1024, latency_ms: 23.0, ..unlocked };
        assert!(warnings(&w.check(5.0, &bigger))[0].contains("1024 frames"));
    }

    #[test]
    fn refused_priority_is_a_warning() {
        let mut w = Watch::new("capture");
        let r = w.check(0.0, &DirectionStats { realtime: Some(false), ..healthy() });
        assert!(warnings(&r).iter().any(|m| m.contains("normal priority")), "{r:?}");
    }

    #[test]
    fn counters_are_rate_limited() {
        let mut w = Watch::new("capture");
        w.check(0.0, &healthy());
        // An xrun: reported at once.
        let s = DirectionStats { device_errors: 1, realigns: 1, ..healthy() };
        let r = w.check(0.25, &s);
        let warned = warnings(&r);
        assert_eq!(warned.len(), 2, "{r:?}");
        assert!(warned[0].contains("1 device errors") && warned[0].contains("so far"));
        // A burst over the next seconds: one summary after five seconds.
        let mut reports = 0;
        for i in 2..40 {
            let s = DirectionStats { device_errors: i, realigns: 1, ..healthy() };
            let r = w.check(0.25 * i as f64, &s);
            reports += r.len();
            if !r.is_empty() {
                assert!(r[0].1.contains("in the last 5 s"), "{r:?}");
            }
        }
        assert_eq!(reports, 1);
    }
}
