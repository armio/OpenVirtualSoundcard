//! Real-time scheduling for the threads that move audio.
//!
//! Every audio thread blocks on each turn of its loop: the transmit thread
//! sleeps until its next packet is due ([`sleep_until`]), and each receive
//! thread blocks in `recv` with a timeout. Keep it that way. Besides wasting a
//! core, a loop that spins is punished on macOS: XNU demotes a real-time
//! thread that runs for about a second without blocking, and the thread then
//! competes with everything else again.

use tracing::{debug, info};

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

/// Asks the OS to schedule the calling thread as real-time (best effort).
///
/// Without it, a busy machine can delay the audio threads long enough for
/// receivers to drop audio. On macOS the thread gets the time-constraint
/// policy for its `class`, which needs no privileges; if that fails, and on
/// every other OS, it asks for the highest priority instead. Linux needs
/// `CAP_SYS_NICE` (or rtkit) for that; macOS and Windows allow raised
/// priorities for normal users.
pub fn raise_priority(what: &str, class: RtClass) {
    #[cfg(target_os = "macos")]
    match macos::set_time_constraint(class.time_constraint()) {
        Ok(()) => {
            debug!("{what} thread runs with the real-time time-constraint policy");
            return;
        }
        Err(kr) => {
            use std::sync::atomic::{AtomicBool, Ordering};
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    "cannot give the {what} thread the real-time time-constraint policy \
                     (thread_policy_set returned {kr}); raising its priority instead"
                );
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = class;

    use thread_priority::{ThreadPriority, set_current_thread_priority};
    #[cfg(unix)]
    {
        use thread_priority::{
            RealtimeThreadSchedulePolicy, ThreadSchedulePolicy, set_thread_priority_and_policy,
            thread_native_id,
        };
        let fifo = ThreadSchedulePolicy::Realtime(RealtimeThreadSchedulePolicy::Fifo);
        if set_thread_priority_and_policy(thread_native_id(), ThreadPriority::Max, fifo).is_ok() {
            debug!("{what} thread runs with real-time priority");
            return;
        }
    }
    match set_current_thread_priority(ThreadPriority::Max) {
        Ok(()) => debug!("{what} thread runs with raised priority"),
        Err(e) => {
            use std::sync::atomic::{AtomicBool, Ordering};
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                info!(
                    "audio threads run at normal priority ({e:?}); on a busy host this can \
                     cause dropouts. Grant real-time scheduling (e.g. CAP_SYS_NICE) to avoid it."
                );
            }
        }
    }
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

    #[test]
    fn raise_priority_is_best_effort() {
        for class in [RtClass::Transmit, RtClass::Receive] {
            std::thread::spawn(move || {
                raise_priority("test", class);
                sleep_until(local_now_ns() + 1_000_000);
            })
            .join()
            .unwrap();
        }
    }
}
