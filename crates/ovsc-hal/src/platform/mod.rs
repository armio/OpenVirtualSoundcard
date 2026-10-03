//! What the driver needs from the operating system.
//!
//! Everything Apple-specific the driver touches (host time, CoreFoundation
//! objects, logging, randomness) goes through [`Platform`], so the whole
//! driver, extern "C" entry points included, runs under `cargo test` on any
//! OS with [`stub::StubPlatform`]. `macos::MacPlatform` is the real one.

use std::ffi::c_void;

use crate::abi::{CFStringRef, CFUUIDRef};

#[cfg(target_os = "macos")]
pub mod macos;
pub mod stub;

/// Debug detail, normally not persisted.
pub const LOG_DEBUG: u8 = 0;
/// Information, normally not persisted.
pub const LOG_INFO: u8 = 1;
/// Notable events (the os_log default level).
pub const LOG_DEFAULT: u8 = 2;
/// Errors.
pub const LOG_ERROR: u8 = 3;
/// Faults in the driver itself.
pub const LOG_FAULT: u8 = 4;

// The host clock's tick rate. The shared region's header carries the
// daemon's, which the driver compares with its own, so both sides use the
// ovsc-shm type.
pub use ovsc_shm::time::Timebase;

/// The operating-system services the driver uses.
///
/// `now_ticks` and `timebase` are called on real-time threads and must not
/// block, allocate or make system calls beyond reading the clock.
pub trait Platform: Send + Sync {
    /// Host time in ticks (`mach_absolute_time`).
    fn now_ticks(&self) -> u64;
    /// The tick rate of [`Platform::now_ticks`].
    fn timebase(&self) -> Timebase;
    /// A new CFString holding `s`, retained once (+1); the caller releases it.
    fn cfstring_create(&self, s: &str) -> CFStringRef;
    /// The text of a CFString, or `None` if `s` is null or not text.
    fn cfstring_read(&self, s: CFStringRef) -> Option<String>;
    /// Releases a CoreFoundation object; null is ignored.
    fn cf_release(&self, o: *const c_void);
    /// The 16 bytes of a CFUUID, all zero if `u` is null or not a UUID.
    fn uuid_bytes(&self, u: CFUUIDRef) -> [u8; 16];
    /// Logs one line at `level` (one of the `LOG_*` constants). Never called
    /// on real-time paths.
    fn log(&self, level: u8, msg: &str);
    /// A random number, for instance identifiers.
    fn random_u64(&self) -> u64;
    /// This process's ID.
    fn pid(&self) -> i32;
}

/// The platform the factory uses: `macos::MacPlatform` on macOS. Elsewhere
/// there is no Core Audio; a [`stub::StubPlatform`] on the host's monotonic
/// clock lets the factory run in tests.
pub fn production() -> &'static dyn Platform {
    #[cfg(target_os = "macos")]
    {
        macos::MacPlatform::get()
    }
    #[cfg(not(target_os = "macos"))]
    {
        use std::sync::OnceLock;
        static PLATFORM: OnceLock<stub::StubPlatform> = OnceLock::new();
        PLATFORM.get_or_init(stub::StubPlatform::with_host_clock)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timebase_conversions() {
        let arm = Timebase { numer: 125, denom: 3 };
        assert_eq!(arm.ticks_to_ns(24), 1000);
        assert_eq!(arm.ticks_to_ns(1), 41);
        assert_eq!(arm.ns_to_ticks_ceil(1000), 24);
        assert_eq!(arm.ns_to_ticks_ceil(1001), 25);
        assert_eq!(arm.ns_to_ticks_ceil(0), 0);
        assert_eq!(Timebase::NANOS.ticks_to_ns(12345), 12345);
        assert_eq!(Timebase::NANOS.ns_to_ticks_ceil(12345), 12345);
        assert_eq!(Timebase { numer: 0, denom: 0 }.ticks_to_ns(7), 7);
        assert_eq!(arm.ticks_to_ns(u64::MAX), u64::MAX);
        // ceil then floor never loses time.
        for ns in [0, 1, 40, 41, 42, 999, 1_000_000_007] {
            assert!(arm.ticks_to_ns(arm.ns_to_ticks_ceil(ns)) >= ns);
        }
    }
}
