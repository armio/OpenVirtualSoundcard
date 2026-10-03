//! A [`Platform`] for tests and for builds on systems without Core Audio.
//!
//! CoreFoundation objects are boxed Rust values that carry a magic number,
//! so any code holding one (the driver, a [`crate::testing::FakeHost`], a
//! test) can read and free it with the functions in this module. The clock
//! is either set by hand or follows the host's monotonic clock, and the
//! timebase can be changed to exercise tick conversion (1/1 by default,
//! 125/3 as on Apple silicon).

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use super::{Platform, Timebase};
use crate::abi::{CFStringRef, CFUUIDRef};

const CF_MAGIC: u64 = u64::from_le_bytes(*b"odstubCF");
const MAX_LOG_LINES: usize = 10_000;

/// The value inside a stub CoreFoundation object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CfValue {
    String(String),
    Uuid([u8; 16]),
    Data(Vec<u8>),
}

#[repr(C)]
struct CfBox {
    magic: u64,
    value: CfValue,
}

static LIVE_OBJECTS: AtomicUsize = AtomicUsize::new(0);

fn cf_new(value: CfValue) -> *const c_void {
    LIVE_OBJECTS.fetch_add(1, Ordering::Relaxed);
    Box::into_raw(Box::new(CfBox { magic: CF_MAGIC, value })) as *const c_void
}

/// A new stub CFString (+1).
pub fn cf_string(s: &str) -> CFStringRef {
    cf_new(CfValue::String(s.to_owned()))
}

/// A new stub CFUUID (+1).
pub fn cf_uuid(bytes: [u8; 16]) -> CFUUIDRef {
    cf_new(CfValue::Uuid(bytes))
}

/// A new stub CFData (+1).
pub fn cf_data(bytes: &[u8]) -> *const c_void {
    cf_new(CfValue::Data(bytes.to_vec()))
}

/// The value of a stub object, or `None` for null or a foreign pointer.
///
/// # Safety
/// `o` must be null or point to readable memory at least as large as a
/// stub object header; a live stub object always qualifies.
pub unsafe fn cf_value(o: *const c_void) -> Option<CfValue> {
    let b = unsafe { (o as *const CfBox).as_ref()? };
    (b.magic == CF_MAGIC).then(|| b.value.clone())
}

/// The text of a stub CFString, or `None`.
///
/// # Safety
/// As for [`cf_value`].
pub unsafe fn read_string(o: *const c_void) -> Option<String> {
    match unsafe { cf_value(o) }? {
        CfValue::String(s) => Some(s),
        _ => None,
    }
}

/// The bytes of a stub CFData, or `None`.
///
/// # Safety
/// As for [`cf_value`].
pub unsafe fn read_data(o: *const c_void) -> Option<Vec<u8>> {
    match unsafe { cf_value(o) }? {
        CfValue::Data(d) => Some(d),
        _ => None,
    }
}

/// Frees a stub object; null and foreign pointers are ignored.
///
/// # Safety
/// `o` must be null or a live stub object that nothing uses afterwards.
pub unsafe fn cf_free(o: *const c_void) {
    let p = o as *mut CfBox;
    // SAFETY: the caller hands over a live stub object or null.
    if unsafe { p.as_ref() }.is_some_and(|b| b.magic == CF_MAGIC) {
        unsafe {
            (*p).magic = 0;
            drop(Box::from_raw(p));
        }
        LIVE_OBJECTS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Stub objects created and not yet freed, in this process.
pub fn live_objects() -> usize {
    LIVE_OBJECTS.load(Ordering::Relaxed)
}

/// Nanoseconds on the host's monotonic clock: `CLOCK_UPTIME_RAW` on Apple
/// systems, `CLOCK_MONOTONIC` on other Unix systems, and time since the
/// first call elsewhere. This matches `ovsc_clock::local_now_ns`.
pub fn host_now_ns() -> u64 {
    #[cfg(unix)]
    {
        #[cfg(target_vendor = "apple")]
        const CLOCK: libc::clockid_t = libc::CLOCK_UPTIME_RAW;
        #[cfg(not(target_vendor = "apple"))]
        const CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC;
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: `ts` is a valid out-pointer and the clock always exists.
        unsafe { libc::clock_gettime(CLOCK, &mut ts) };
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    }
    #[cfg(not(unix))]
    {
        use std::sync::OnceLock;
        use std::time::Instant;
        static EPOCH: OnceLock<Instant> = OnceLock::new();
        let epoch = *EPOCH.get_or_init(Instant::now);
        Instant::now().saturating_duration_since(epoch).as_nanos() as u64
    }
}

/// Where [`StubPlatform::inject_panic`] makes the next call panic.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanicAt {
    NowTicks = 1,
    CfStringCreate = 2,
    CfStringRead = 3,
    UuidBytes = 4,
}

enum Clock {
    /// Ticks set by the test.
    Manual(AtomicU64),
    /// Nanoseconds from a function, converted with the timebase.
    Host(fn() -> u64),
}

/// A [`Platform`] whose clock, timebase and failures are under test control.
pub struct StubPlatform {
    clock: Clock,
    numer: AtomicU32,
    denom: AtomicU32,
    random: AtomicU64,
    pid: AtomicI32,
    panic_at: AtomicU8,
    logs: Mutex<VecDeque<(u8, String)>>,
}

impl Default for StubPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl StubPlatform {
    /// A manual clock at tick 0 with a 1/1 timebase.
    pub fn new() -> Self {
        Self::with(Clock::Manual(AtomicU64::new(0)))
    }

    /// A clock that follows [`host_now_ns`].
    pub fn with_host_clock() -> Self {
        Self::with_clock(host_now_ns)
    }

    /// A clock that reads nanoseconds from `now_ns`.
    pub fn with_clock(now_ns: fn() -> u64) -> Self {
        Self::with(Clock::Host(now_ns))
    }

    fn with(clock: Clock) -> Self {
        Self {
            clock,
            numer: AtomicU32::new(1),
            denom: AtomicU32::new(1),
            random: AtomicU64::new(0x0123_4567_89AB_CDEF),
            pid: AtomicI32::new(4242),
            panic_at: AtomicU8::new(0),
            logs: Mutex::new(VecDeque::new()),
        }
    }

    /// Moves this platform to the heap for the life of the process, as the
    /// driver needs a `&'static dyn Platform`.
    pub fn leak(self) -> &'static StubPlatform {
        Box::leak(Box::new(self))
    }

    /// Sets the manual clock. No effect on a host clock.
    pub fn set_now_ticks(&self, ticks: u64) {
        if let Clock::Manual(t) = &self.clock {
            t.store(ticks, Ordering::SeqCst);
        }
    }

    /// Advances the manual clock. No effect on a host clock.
    pub fn advance_ticks(&self, ticks: u64) {
        if let Clock::Manual(t) = &self.clock {
            t.fetch_add(ticks, Ordering::SeqCst);
        }
    }

    /// Advances the manual clock by at least `ns` nanoseconds.
    pub fn advance_ns(&self, ns: u64) {
        self.advance_ticks(self.timebase().ns_to_ticks_ceil(ns));
    }

    /// Sets the timebase (`ns = ticks * numer / denom`).
    pub fn set_timebase(&self, numer: u32, denom: u32) {
        self.numer.store(numer, Ordering::SeqCst);
        self.denom.store(denom, Ordering::SeqCst);
    }

    pub fn set_pid(&self, pid: i32) {
        self.pid.store(pid, Ordering::SeqCst);
    }

    /// Makes the next call of `at` panic, once.
    pub fn inject_panic(&self, at: PanicAt) {
        self.panic_at.store(at as u8, Ordering::SeqCst);
    }

    /// Every line logged so far (the most recent 10,000).
    pub fn logs(&self) -> Vec<(u8, String)> {
        let logs = self.logs.lock().unwrap_or_else(|e| e.into_inner());
        logs.iter().cloned().collect()
    }

    /// Whether a logged line contains `needle`.
    pub fn logged(&self, needle: &str) -> bool {
        let logs = self.logs.lock().unwrap_or_else(|e| e.into_inner());
        logs.iter().any(|(_, line)| line.contains(needle))
    }

    fn maybe_panic(&self, at: PanicAt) {
        let armed = at as u8;
        if self.panic_at.compare_exchange(armed, 0, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            panic!("injected panic in {at:?}");
        }
    }
}

// The Platform interface passes CoreFoundation references by value, as
// CoreFoundation does; the driver only hands back objects a platform made.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
impl Platform for StubPlatform {
    fn now_ticks(&self) -> u64 {
        self.maybe_panic(PanicAt::NowTicks);
        match &self.clock {
            Clock::Manual(t) => t.load(Ordering::SeqCst),
            Clock::Host(now_ns) => {
                // ticks = floor(ns * denom / numer), the inverse of ticks_to_ns.
                let tb = self.timebase();
                let ticks = now_ns() as u128 * tb.denom.max(1) as u128 / tb.numer.max(1) as u128;
                ticks.min(u64::MAX as u128) as u64
            }
        }
    }

    fn timebase(&self) -> Timebase {
        Timebase {
            numer: self.numer.load(Ordering::SeqCst),
            denom: self.denom.load(Ordering::SeqCst),
        }
    }

    fn cfstring_create(&self, s: &str) -> CFStringRef {
        self.maybe_panic(PanicAt::CfStringCreate);
        cf_string(s)
    }

    fn cfstring_read(&self, s: CFStringRef) -> Option<String> {
        self.maybe_panic(PanicAt::CfStringRead);
        // SAFETY: the driver only passes null or objects this module made.
        match unsafe { cf_value(s) }? {
            CfValue::String(s) => Some(s),
            CfValue::Data(d) => String::from_utf8(d).ok(),
            CfValue::Uuid(_) => None,
        }
    }

    fn cf_release(&self, o: *const c_void) {
        // SAFETY: as for cfstring_read; the driver releases each object once.
        unsafe { cf_free(o) }
    }

    fn uuid_bytes(&self, u: CFUUIDRef) -> [u8; 16] {
        self.maybe_panic(PanicAt::UuidBytes);
        // SAFETY: as for cfstring_read.
        match unsafe { cf_value(u) } {
            Some(CfValue::Uuid(b)) => b,
            _ => [0; 16],
        }
    }

    fn log(&self, level: u8, msg: &str) {
        let mut logs = self.logs.lock().unwrap_or_else(|e| e.into_inner());
        if logs.len() == MAX_LOG_LINES {
            logs.pop_front();
        }
        logs.push_back((level, msg.to_owned()));
    }

    fn random_u64(&self) -> u64 {
        // splitmix64 over a Weyl sequence: deterministic, lock-free.
        let mut z = self.random.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn pid(&self) -> i32 {
        self.pid.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cf_objects_round_trip() {
        let p = StubPlatform::new();
        let s = p.cfstring_create("OpenVirtualSoundcard");
        assert_eq!(p.cfstring_read(s).as_deref(), Some("OpenVirtualSoundcard"));
        assert_eq!(unsafe { read_string(s) }.as_deref(), Some("OpenVirtualSoundcard"));
        let u = cf_uuid([7; 16]);
        assert_eq!(p.uuid_bytes(u), [7; 16]);
        assert_eq!(p.uuid_bytes(s), [0; 16]);
        assert_eq!(p.cfstring_read(u), None);
        let d = cf_data(b"a=1\n");
        assert_eq!(p.cfstring_read(d).as_deref(), Some("a=1\n"));
        assert_eq!(unsafe { read_data(d) }.as_deref(), Some(&b"a=1\n"[..]));
        assert_eq!(p.cfstring_read(std::ptr::null()), None);
        for o in [s, u, d] {
            p.cf_release(o);
        }
        p.cf_release(std::ptr::null());
    }

    #[test]
    fn manual_clock_and_timebase() {
        let p = StubPlatform::new();
        assert_eq!(p.now_ticks(), 0);
        p.advance_ns(1000);
        assert_eq!(p.now_ticks(), 1000);
        p.set_timebase(125, 3);
        p.set_now_ticks(0);
        p.advance_ns(1000);
        assert_eq!(p.now_ticks(), 24);
        assert_eq!(p.timebase().ticks_to_ns(p.now_ticks()), 1000);
    }

    #[test]
    fn host_clock_converts_to_ticks() {
        fn fixed() -> u64 {
            1_000_000_000
        }
        let p = StubPlatform::with_clock(fixed);
        assert_eq!(p.now_ticks(), 1_000_000_000);
        p.set_timebase(125, 3);
        assert_eq!(p.now_ticks(), 24_000_000);
        let real = StubPlatform::with_host_clock();
        let a = real.now_ticks();
        assert!(real.now_ticks() >= a);
    }

    #[test]
    fn injected_panic_fires_once() {
        let p = StubPlatform::new();
        p.inject_panic(PanicAt::NowTicks);
        assert!(std::panic::catch_unwind(|| p.now_ticks()).is_err());
        assert_eq!(p.now_ticks(), 0);
    }

    #[test]
    fn random_values_differ() {
        let p = StubPlatform::new();
        assert_ne!(p.random_u64(), p.random_u64());
    }
}
