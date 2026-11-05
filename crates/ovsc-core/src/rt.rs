//! Real-time scheduling for the threads that move audio.
//!
//! Every audio thread blocks on each turn of its loop: the transmit thread
//! sleeps until its next packet is due ([`sleep_until`]), and each receive
//! thread blocks in `recv` with a timeout. Keep it that way. Besides wasting a
//! core, a loop that spins is punished on macOS: XNU demotes a real-time
//! thread that runs for about a second without blocking, and the thread then
//! competes with everything else again.

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::{debug, warn};

/// What a real-time thread does. On macOS it selects the thread's
/// time-constraint contract with the scheduler; elsewhere every class gets
/// the same treatment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RtClass {
    /// The transmit thread: wakes about every millisecond to send the
    /// packets that are due.
    Transmit,
    /// A receive thread: wakes whenever a packet arrives, with no fixed
    /// period.
    Receive,
}

/// The parameters of macOS's `THREAD_TIME_CONSTRAINT_POLICY`, in nanoseconds.
#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TimeConstraint {
    /// Nominal interval between wake-ups; 0 when there is none.
    period_ns: u64,
    /// CPU time needed per wake-up.
    computation_ns: u64,
    /// Time from wake-up by which the computation must be done.
    constraint_ns: u64,
}

#[cfg(any(target_os = "macos", test))]
impl RtClass {
    const fn time_constraint(self) -> TimeConstraint {
        match self {
            RtClass::Transmit => TimeConstraint {
                period_ns: 1_000_000,
                computation_ns: 250_000,
                constraint_ns: 1_000_000,
            },
            RtClass::Receive => {
                TimeConstraint { period_ns: 0, computation_ns: 100_000, constraint_ns: 1_000_000 }
            }
        }
    }
}

/// Asks the OS to schedule the calling thread as real-time (best effort),
/// and returns whether it did.
///
/// Without it, a busy machine can delay the audio threads long enough for
/// receivers to drop audio. On macOS the thread gets the time-constraint
/// policy for its `class`, which needs no privileges; if that fails (it is
/// not expected to), the thread gets a raised priority that is not
/// real-time. Other Unix systems get `SCHED_FIFO`, which Linux grants only
/// with `CAP_SYS_NICE` (or through rtkit); failing that, the thread gets
/// the highest normal priority it may have. Elsewhere (Windows) the highest
/// priority is the best there is, and counts as real-time.
///
/// Whenever the thread ends up without real-time scheduling, on any
/// platform, the function returns false, so that the caller can record it
/// where the status shows it, and logs one warning that says "the {what}
/// thread is not real-time", what it got instead and why, so that a single
/// search of the log finds every case. It warns once per process for each
/// [`RtClass`], since every thread of a kind fares the same.
pub fn raise_priority(what: &str, class: RtClass) -> bool {
    // Why the thread is not real-time, should it end up so.
    #[cfg(target_os = "macos")]
    let why: Cow<'static, str> = match macos::set_time_constraint(class.time_constraint()) {
        Ok(()) => {
            debug!("{what} thread runs with the real-time time-constraint policy");
            return true;
        }
        Err(kr) => Cow::Owned(format!(
            "macOS refused it the time-constraint policy (thread_policy_set returned {kr}), \
             which needs no privileges. Restart the daemon, and report it if it happens again."
        )),
    };
    #[cfg(not(target_os = "macos"))]
    let why: Cow<'static, str> =
        Cow::Borrowed("Grant real-time scheduling (e.g. CAP_SYS_NICE) to avoid it.");

    use thread_priority::{ThreadPriority, set_current_thread_priority};
    #[cfg(unix)]
    let fifo = {
        use thread_priority::{
            RealtimeThreadSchedulePolicy, ThreadSchedulePolicy, set_thread_priority_and_policy,
            thread_native_id,
        };
        let fifo = ThreadSchedulePolicy::Realtime(RealtimeThreadSchedulePolicy::Fifo);
        set_thread_priority_and_policy(thread_native_id(), ThreadPriority::Max, fifo).is_ok()
    };
    #[cfg(not(unix))]
    let fifo = false;
    // What the thread got instead.
    let got = if fifo {
        // On macOS only the time-constraint policy is real-time: SCHED_FIFO
        // there is a fixed priority below the real-time band that Core
        // Audio's threads run in.
        if cfg!(not(target_os = "macos")) {
            debug!("{what} thread runs with the SCHED_FIFO policy");
            return true;
        }
        "a raised fixed priority (SCHED_FIFO), below Core Audio's real-time threads".to_owned()
    } else {
        match set_current_thread_priority(ThreadPriority::Max) {
            Ok(()) if cfg!(not(unix)) => {
                debug!("{what} thread runs with the highest priority");
                return true;
            }
            Ok(()) => "a raised but not real-time priority".to_owned(),
            Err(e) => format!("normal priority ({e:?})"),
        }
    };
    if first_miss(class) {
        warn!("{}", not_realtime_warning(what, &got, &why));
    }
    false
}

/// Whether no `class` thread has missed real-time scheduling before in this
/// process: each kind is warned about once, not once per flow.
fn first_miss(class: RtClass) -> bool {
    static MISSED: [AtomicBool; 2] = [AtomicBool::new(false), AtomicBool::new(false)];
    let kind = match class {
        RtClass::Transmit => 0,
        RtClass::Receive => 1,
    };
    !MISSED[kind].swap(true, Ordering::Relaxed)
}

/// The warning for a `what` thread that runs at `got` rather than
/// real-time, for the reason or remedy `why`. Every such outcome, on every
/// platform, says "the {what} thread is not real-time" in the same words.
fn not_realtime_warning(what: &str, got: &str, why: &str) -> String {
    format!(
        "the {what} thread is not real-time: it runs at {got}, and on a busy host this can \
         cause dropouts. {why}"
    )
}

/// Blocks the calling thread until [`ovsc_clock::local_now_ns`]
/// reaches `deadline_ns`, and returns at once if it already has.
///
/// On macOS this waits for the absolute deadline with `mach_wait_until`,
/// which never wakes before the deadline and, for a time-constraint thread,
/// wakes within microseconds on real hardware (about 0.4 ms late on
/// virtualized CI runners). Elsewhere it is a `std::thread::sleep` for the time
/// left, which is high-resolution on every platform, unlike timed waits on
/// channels.
///
/// Real-time safe once the Mach timebase is cached, which [`raise_priority`]
/// does on macOS: no allocation, no locks, and on macOS no system call other
/// than the wait.
pub fn sleep_until(deadline_ns: u64) {
    #[cfg(target_os = "macos")]
    macos::sleep_until(deadline_ns);
    #[cfg(not(target_os = "macos"))]
    std::thread::sleep(std::time::Duration::from_nanos(
        deadline_ns.saturating_sub(ovsc_clock::local_now_ns()),
    ));
}

/// Converts `ns` nanoseconds to Mach absolute-time ticks (`ns * denom /
/// numer`), rounding up so that a deadline in ticks is never earlier than
/// the one in nanoseconds.
#[cfg(any(target_os = "macos", test))]
fn ns_to_ticks(ns: u64, numer: u32, denom: u32) -> u64 {
    let ticks = (u128::from(ns) * u128::from(denom)).div_ceil(u128::from(numer.max(1)));
    u64::try_from(ticks).unwrap_or(u64::MAX)
}

#[cfg(target_os = "macos")]
mod macos {
    use std::sync::OnceLock;

    use mach2::kern_return::{KERN_ABORTED, KERN_SUCCESS, kern_return_t};
    use mach2::mach_init::mach_thread_self;
    use mach2::mach_port::mach_port_deallocate;
    use mach2::mach_time::{mach_timebase_info, mach_wait_until};
    use mach2::thread_policy::{
        THREAD_TIME_CONSTRAINT_POLICY, THREAD_TIME_CONSTRAINT_POLICY_COUNT, thread_policy_set,
        thread_time_constraint_policy_data_t,
    };
    use mach2::traps::mach_task_self;

    use super::TimeConstraint;

    /// The Mach timebase as (numer, denom): 125/3 on Apple silicon, 1/1 on
    /// Intel.
    fn timebase() -> (u32, u32) {
        static TIMEBASE: OnceLock<(u32, u32)> = OnceLock::new();
        *TIMEBASE.get_or_init(|| {
            let mut info = mach_timebase_info { numer: 0, denom: 0 };
            // SAFETY: `info` is a valid out-pointer.
            let kr = unsafe { mach_timebase_info(&mut info) };
            if kr == KERN_SUCCESS && info.numer != 0 && info.denom != 0 {
                (info.numer, info.denom)
            } else {
                (1, 1)
            }
        })
    }

    fn ticks(ns: u64) -> u64 {
        let (numer, denom) = timebase();
        super::ns_to_ticks(ns, numer, denom)
    }

    fn ticks32(ns: u64) -> u32 {
        u32::try_from(ticks(ns)).unwrap_or(u32::MAX)
    }

    /// Gives the calling thread `THREAD_TIME_CONSTRAINT_POLICY` (preemptible).
    pub(super) fn set_time_constraint(tc: TimeConstraint) -> Result<(), kern_return_t> {
        let mut policy = thread_time_constraint_policy_data_t {
            period: ticks32(tc.period_ns),
            computation: ticks32(tc.computation_ns),
            constraint: ticks32(tc.constraint_ns),
            preemptible: 1,
        };
        // SAFETY: `mach_thread_self` returns a send right to the calling
        // thread, released below; `policy` is a valid time-constraint policy
        // of THREAD_TIME_CONSTRAINT_POLICY_COUNT integers.
        let kr = unsafe {
            let thread = mach_thread_self();
            let kr = thread_policy_set(
                thread,
                THREAD_TIME_CONSTRAINT_POLICY,
                (&raw mut policy).cast(),
                THREAD_TIME_CONSTRAINT_POLICY_COUNT,
            );
            mach_port_deallocate(mach_task_self(), thread);
            kr
        };
        if kr == KERN_SUCCESS { Ok(()) } else { Err(kr) }
    }

    pub(super) fn sleep_until(deadline_ns: u64) {
        let deadline = ticks(deadline_ns);
        // Checked against the same clock the caller uses (CLOCK_UPTIME_RAW
        // is mach_absolute_time in nanoseconds, read without a system call),
        // so a wake-up is never early by its reckoning.
        while ovsc_clock::local_now_ns() < deadline_ns {
            // SAFETY: no pointers involved.
            let kr = unsafe { mach_wait_until(deadline) };
            // KERN_ABORTED: woken by a signal or thread_abort; wait again.
            // Anything else is not expected: return rather than spin.
            if kr != KERN_SUCCESS && kr != KERN_ABORTED {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ovsc_clock::local_now_ns;

    #[test]
    fn ticks_round_up() {
        // Apple silicon: 24 MHz.
        assert_eq!(ns_to_ticks(1_000_000, 125, 3), 24_000);
        assert_eq!(ns_to_ticks(250_000, 125, 3), 6_000);
        assert_eq!(ns_to_ticks(1, 125, 3), 1);
        assert_eq!(ns_to_ticks(0, 125, 3), 0);
        // Intel: nanoseconds.
        assert_eq!(ns_to_ticks(1_000_000, 1, 1), 1_000_000);
        assert_eq!(ns_to_ticks(u64::MAX, 1, 1), u64::MAX);
        assert_eq!(ns_to_ticks(u64::MAX, 1, 3), u64::MAX);
        // The deadline in ticks converts back (rounding down, like
        // CLOCK_UPTIME_RAW) to no earlier than the one asked for.
        for ns in (0..10_000_000u64).step_by(997) {
            let t = ns_to_ticks(ns, 125, 3);
            assert!(t * 125 / 3 >= ns, "{ns} ns -> {t} ticks");
            assert!(t == 0 || (t - 1) * 125 / 3 < ns, "{ns} ns -> {t} ticks");
        }
    }

    #[test]
    fn time_constraints() {
        let tx = RtClass::Transmit.time_constraint();
        assert_eq!(
            (tx.period_ns, tx.computation_ns, tx.constraint_ns),
            (1_000_000, 250_000, 1_000_000)
        );
        let rx = RtClass::Receive.time_constraint();
        assert_eq!((rx.period_ns, rx.computation_ns, rx.constraint_ns), (0, 100_000, 1_000_000));
        for tc in [tx, rx] {
            assert!(tc.computation_ns <= tc.constraint_ns);
            assert!(tc.period_ns == 0 || tc.constraint_ns <= tc.period_ns);
        }
    }

    #[cfg(unix)]
    #[test]
    fn sleep_until_is_never_early() {
        let start = local_now_ns();
        for i in 1..=10 {
            let deadline = start + i * 1_000_000;
            sleep_until(deadline);
            let now = local_now_ns();
            assert!(now >= deadline, "woke {} ns early", deadline - now);
        }
    }

    #[test]
    fn sleep_until_past_deadline_returns() {
        let start = std::time::Instant::now();
        sleep_until(0);
        sleep_until(local_now_ns().saturating_sub(1_000_000));
        sleep_until(local_now_ns());
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    /// Every way a thread misses real-time scheduling is logged in the same
    /// words, which one search of the log finds.
    #[test]
    fn not_realtime_is_logged_in_the_same_words() {
        for got in [
            "a raised fixed priority (SCHED_FIFO), below Core Audio's real-time threads",
            "a raised but not real-time priority",
            "normal priority (OS(1))",
        ] {
            let w = not_realtime_warning("receive", got, "Why.");
            assert!(w.starts_with("the receive thread is not real-time: it runs at "), "{w}");
            assert!(w.contains(got) && w.ends_with(" Why."), "{w}");
        }
    }

    /// Each kind of thread is warned about once per process, not once per
    /// thread.
    #[test]
    fn each_kind_warns_once() {
        for class in [RtClass::Transmit, RtClass::Receive] {
            // Another test's thread may have missed first.
            first_miss(class);
            assert!(!first_miss(class), "{class:?}");
        }
    }

    #[test]
    fn raise_priority_is_best_effort() {
        for class in [RtClass::Transmit, RtClass::Receive] {
            std::thread::spawn(move || {
                let realtime = raise_priority("test", class);
                // What it reports is what the thread got: SCHED_FIFO with
                // CAP_SYS_NICE (root in a container), normal scheduling
                // without.
                #[cfg(target_os = "linux")]
                {
                    // SAFETY: no pointers; 0 is the calling thread.
                    let policy = unsafe { libc::sched_getscheduler(0) };
                    let policy = policy & !libc::SCHED_RESET_ON_FORK;
                    assert_eq!(realtime, policy == libc::SCHED_FIFO, "policy {policy}");
                }
                let _ = realtime;
                sleep_until(local_now_ns() + 1_000_000);
            })
            .join()
            .unwrap();
        }
    }
}
