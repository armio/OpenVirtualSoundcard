//! macOS real-time threads: the time-constraint policy the audio threads
//! get, and how precisely the transmit thread's absolute-deadline sleep
//! wakes up.
//!
//! Run with `--nocapture` to see the measured lateness.

#![cfg(target_os = "macos")]

use mach2::boolean::boolean_t;
use mach2::kern_return::KERN_SUCCESS;
use mach2::mach_init::mach_thread_self;
use mach2::mach_port::mach_port_deallocate;
use mach2::mach_time::{mach_timebase_info, mach_timebase_info_data_t};
use mach2::thread_policy::{
    THREAD_TIME_CONSTRAINT_POLICY, THREAD_TIME_CONSTRAINT_POLICY_COUNT, thread_policy_get,
    thread_time_constraint_policy_data_t,
};
use mach2::traps::mach_task_self;

use ovsc_clock::local_now_ns;
use ovsc_core::rt::{RtClass, raise_priority, sleep_until};

/// Nanoseconds to Mach absolute-time ticks.
fn ticks(ns: u64) -> u32 {
    let mut tb = mach_timebase_info_data_t { numer: 0, denom: 0 };
    // SAFETY: `tb` is a valid out-pointer.
    assert_eq!(unsafe { mach_timebase_info(&mut tb) }, KERN_SUCCESS);
    u32::try_from(ns * u64::from(tb.denom) / u64::from(tb.numer)).unwrap()
}

/// Reads the calling thread's own time-constraint policy, without falling
/// back to the defaults. Returns None when the thread has none.
fn current_time_constraint() -> Option<thread_time_constraint_policy_data_t> {
    let mut policy = thread_time_constraint_policy_data_t {
        period: 0,
        computation: 0,
        constraint: 0,
        preemptible: 0,
    };
    let mut count = THREAD_TIME_CONSTRAINT_POLICY_COUNT;
    let mut get_default: boolean_t = 0;
    // SAFETY: `policy` holds `count` integers; the thread port is released
    // after use.
    let kr = unsafe {
        let thread = mach_thread_self();
        let kr = thread_policy_get(
            thread,
            THREAD_TIME_CONSTRAINT_POLICY,
            (&raw mut policy).cast(),
            &mut count,
            &mut get_default,
        );
        mach_port_deallocate(mach_task_self(), thread);
        kr
    };
    assert_eq!(kr, KERN_SUCCESS, "thread_policy_get");
    (get_default == 0).then_some(policy)
}

#[test]
fn time_constraint_policy_is_set() {
    // (class, period, constraint) in nanoseconds.
    let cases = [(RtClass::Transmit, 1_000_000u64, 1_000_000u64), (RtClass::Receive, 0, 1_000_000)];
    for (class, period_ns, constraint_ns) in cases {
        let policy = std::thread::spawn(move || {
            assert!(current_time_constraint().is_none(), "a new thread is not real-time");
            raise_priority("test", class);
            current_time_constraint()
        })
        .join()
        .unwrap();
        let policy = policy.unwrap_or_else(|| panic!("{class:?}: no time-constraint policy"));
        println!(
            "{class:?}: period {} computation {} constraint {} preemptible {} (ticks)",
            policy.period, policy.computation, policy.constraint, policy.preemptible
        );
        // The kernel may raise the computation (to half the constraint on
        // recent XNU), so only the period and the constraint must match.
        assert_eq!(policy.period, ticks(period_ns), "{class:?} period");
        assert_eq!(policy.constraint, ticks(constraint_ns), "{class:?} constraint");
        assert_ne!(policy.preemptible, 0, "{class:?} preemptible");
    }
}

#[test]
fn pacing_lateness() {
    const DEADLINES: u64 = 2_000;
    const PERIOD_NS: u64 = 1_000_000;
    let mut late = std::thread::spawn(|| {
        raise_priority("test", RtClass::Transmit);
        let mut late = Vec::with_capacity(DEADLINES as usize);
        let start = local_now_ns() + PERIOD_NS;
        for i in 0..DEADLINES {
            let deadline = start + i * PERIOD_NS;
            sleep_until(deadline);
            let now = local_now_ns();
            assert!(now >= deadline, "deadline {i}: woke {} ns early", deadline - now);
            late.push(now - deadline);
        }
        late
    })
    .join()
    .unwrap();
    late.sort_unstable();
    let pick = |q: usize| late[(late.len() * q / 100).min(late.len() - 1)];
    let (p50, p99, max) = (pick(50), pick(99), late[late.len() - 1]);
    println!(
        "lateness over {DEADLINES} x 1 ms deadlines: p50 {:.1} us, p99 {:.1} us, max {:.1} us",
        p50 as f64 / 1e3,
        p99 as f64 / 1e3,
        max as f64 / 1e3
    );
    // Lateness only delays packets into the receivers' latency budget
    // (4 ms by default, of which the 0.5 ms transmit guard is already
    // spent); audio waiting in the TX rings loses nothing. So the bound is
    // what that budget can afford, not what bare metal achieves (a few
    // microseconds). GitHub's arm64 runners are VMs whose timers fire about
    // 0.4 ms late (p50 360 us, p99 467 us, max 560 us measured on macOS 26).
    // Informational on Intel, whose runners are slower and noisier.
    if cfg!(target_arch = "aarch64") {
        assert!(p99 < 1_000_000, "p99 lateness {p99} ns is not below 1 ms");
    }
}
