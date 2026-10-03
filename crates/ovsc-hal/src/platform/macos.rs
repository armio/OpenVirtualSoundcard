//! The real [`Platform`]: mach time, CoreFoundation, libSystem.

use std::ffi::{c_int, c_void};
use std::sync::OnceLock;

use ovsc_ipc::log;

use super::{LOG_DEBUG, LOG_ERROR, LOG_FAULT, LOG_INFO, Platform, Timebase};
use crate::abi::{
    Boolean, CFAllocatorRef, CFStringRef, CFUUIDBytes, CFUUIDRef, kCFStringEncodingUTF8,
};

/// The unified log's subsystem and category of the driver's messages
/// (design section 13).
const LOG_SUBSYSTEM: &str = "org.openvirtualsoundcard";
const LOG_CATEGORY: &str = "driver";

type CFIndex = isize;
type CFTypeID = usize;

#[repr(C)]
#[derive(Clone, Copy)]
struct CFRange {
    location: CFIndex,
    length: CFIndex,
}

#[repr(C)]
#[derive(Default)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> c_int;
    fn arc4random_buf(buf: *mut c_void, nbytes: usize);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFGetTypeID(cf: *const c_void) -> CFTypeID;
    fn CFRelease(cf: *const c_void);
    fn CFStringGetTypeID() -> CFTypeID;
    fn CFStringCreateWithBytes(
        alloc: CFAllocatorRef,
        bytes: *const u8,
        num_bytes: CFIndex,
        encoding: u32,
        is_external_representation: Boolean,
    ) -> CFStringRef;
    fn CFStringGetLength(s: CFStringRef) -> CFIndex;
    fn CFStringGetBytes(
        s: CFStringRef,
        range: CFRange,
        encoding: u32,
        loss_byte: u8,
        is_external_representation: Boolean,
        buffer: *mut u8,
        max_buf_len: CFIndex,
        used_buf_len: *mut CFIndex,
    ) -> CFIndex;
    fn CFDataGetTypeID() -> CFTypeID;
    fn CFDataGetLength(d: *const c_void) -> CFIndex;
    fn CFDataGetBytePtr(d: *const c_void) -> *const u8;
    fn CFUUIDGetTypeID() -> CFTypeID;
    fn CFUUIDGetUUIDBytes(uuid: CFUUIDRef) -> CFUUIDBytes;
}

/// The macOS platform. The timebase is read once.
pub struct MacPlatform {
    timebase: Timebase,
}

impl MacPlatform {
    /// The process-wide instance.
    pub fn get() -> &'static MacPlatform {
        static PLATFORM: OnceLock<MacPlatform> = OnceLock::new();
        PLATFORM.get_or_init(|| {
            log::log_init(LOG_SUBSYSTEM, LOG_CATEGORY);
            let mut info = MachTimebaseInfo::default();
            // SAFETY: `info` is a valid out-pointer.
            unsafe { mach_timebase_info(&mut info) };
            MacPlatform { timebase: Timebase { numer: info.numer, denom: info.denom } }
        })
    }

    /// The UTF-8 text of a CFString.
    ///
    /// # Safety
    /// `s` must be a live CFString.
    unsafe fn string_text(s: CFStringRef) -> Option<String> {
        let range = CFRange { location: 0, length: unsafe { CFStringGetLength(s) } };
        let mut needed: CFIndex = 0;
        // SAFETY: a null buffer asks only for the length in bytes.
        let converted = unsafe {
            CFStringGetBytes(
                s,
                range,
                kCFStringEncodingUTF8,
                0,
                0,
                std::ptr::null_mut(),
                0,
                &mut needed,
            )
        };
        if converted != range.length || needed < 0 {
            return None;
        }
        let mut buf = vec![0u8; needed as usize];
        let mut used: CFIndex = 0;
        // SAFETY: `buf` holds `needed` bytes.
        unsafe {
            CFStringGetBytes(
                s,
                range,
                kCFStringEncodingUTF8,
                0,
                0,
                buf.as_mut_ptr(),
                needed,
                &mut used,
            )
        };
        buf.truncate(used.clamp(0, needed) as usize);
        String::from_utf8(buf).ok()
    }

    /// The bytes of a CFData, read as UTF-8 text.
    ///
    /// # Safety
    /// `d` must be a live CFData.
    unsafe fn data_text(d: *const c_void) -> Option<String> {
        let len = unsafe { CFDataGetLength(d) };
        let ptr = unsafe { CFDataGetBytePtr(d) };
        if len <= 0 || ptr.is_null() {
            return Some(String::new());
        }
        // SAFETY: CFData owns `len` readable bytes at `ptr` while it lives.
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
        String::from_utf8(bytes.to_vec()).ok()
    }
}

// The Platform interface passes CoreFoundation references by value, as
// CoreFoundation does.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
impl Platform for MacPlatform {
    fn now_ticks(&self) -> u64 {
        // SAFETY: no preconditions; a commpage read.
        unsafe { mach_absolute_time() }
    }

    fn timebase(&self) -> Timebase {
        self.timebase
    }

    fn cfstring_create(&self, s: &str) -> CFStringRef {
        // SAFETY: `s` is valid UTF-8 of the given length; CF copies it.
        unsafe {
            CFStringCreateWithBytes(
                std::ptr::null(),
                s.as_ptr(),
                s.len() as CFIndex,
                kCFStringEncodingUTF8,
                0,
            )
        }
    }

    /// Reads a CFString. Host storage values are read through here too, so
    /// a CFData holding UTF-8 text is accepted as well.
    fn cfstring_read(&self, s: CFStringRef) -> Option<String> {
        if s.is_null() {
            return None;
        }
        // SAFETY: the HAL hands the driver live CF objects; the type is
        // checked before any type-specific call.
        unsafe {
            let ty = CFGetTypeID(s);
            if ty == CFStringGetTypeID() {
                Self::string_text(s)
            } else if ty == CFDataGetTypeID() {
                Self::data_text(s)
            } else {
                None
            }
        }
    }

    fn cf_release(&self, o: *const c_void) {
        if !o.is_null() {
            // SAFETY: the caller owns one reference to `o`.
            unsafe { CFRelease(o) };
        }
    }

    fn uuid_bytes(&self, u: CFUUIDRef) -> [u8; 16] {
        if u.is_null() {
            return [0; 16];
        }
        // SAFETY: `u` is a live CF object; its type is checked first.
        unsafe {
            if CFGetTypeID(u) != CFUUIDGetTypeID() {
                return [0; 16];
            }
            CFUUIDGetUUIDBytes(u).0
        }
    }

    /// Logs to the unified log (os_log), subsystem `org.openvirtualsoundcard`,
    /// category `driver`.
    fn log(&self, level: u8, msg: &str) {
        let level = match level {
            LOG_DEBUG => log::Level::Debug,
            LOG_INFO => log::Level::Info,
            LOG_ERROR => log::Level::Error,
            LOG_FAULT => log::Level::Fault,
            _ => log::Level::Default,
        };
        log::log(level, &format!("OpenVirtualSoundcard driver: {msg}"));
    }

    fn random_u64(&self) -> u64 {
        let mut bytes = [0u8; 8];
        // SAFETY: `bytes` holds 8 writable bytes.
        unsafe { arc4random_buf(bytes.as_mut_ptr() as *mut c_void, bytes.len()) };
        u64::from_ne_bytes(bytes)
    }

    fn pid(&self) -> i32 {
        // SAFETY: no preconditions.
        unsafe { libc::getpid() }
    }
}
