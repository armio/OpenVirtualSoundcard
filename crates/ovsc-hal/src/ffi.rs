//! Panic containment for the extern "C" entry points.
//!
//! Since Rust 1.81, a panic that unwinds out of an `extern "C"` function
//! aborts the process, which here would be the Core Audio driver helper.
//! Every entry point therefore runs its body under [`ffi_guard`]: a panic
//! becomes the entry's fallback result and marks the driver faulted, after
//! which IO is silent (design D13).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

/// Runs `f`. If it panics, sets `faulted` and returns `default` instead.
///
/// Costs nothing when `f` does not panic: no allocation, no lock.
pub(crate) fn ffi_guard<T>(faulted: &AtomicBool, default: T, f: impl FnOnce() -> T) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(payload) => {
            faulted.store(true, Ordering::Release);
            // A payload whose destructor panics must not escape either.
            let _ = catch_unwind(AssertUnwindSafe(move || drop(payload)));
            default
        }
    }
}

/// Replaces the panic hook with one that does nothing, once per process.
///
/// The default hook writes to stderr and may take locks, which a real-time
/// thread must not do; inside coreaudiod nobody reads stderr anyway. Tests
/// keep the default hook so their panics stay visible.
pub(crate) fn install_quiet_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| std::panic::set_hook(Box::new(|_| {})));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_passes_values_through() {
        let faulted = AtomicBool::new(false);
        assert_eq!(ffi_guard(&faulted, 0, || 7), 7);
        assert!(!faulted.load(Ordering::Acquire));
    }

    #[test]
    fn guard_turns_a_panic_into_the_default() {
        let faulted = AtomicBool::new(false);
        let r = ffi_guard(&faulted, -1, || -> i32 { panic!("boom") });
        assert_eq!(r, -1);
        assert!(faulted.load(Ordering::Acquire));
    }
}
