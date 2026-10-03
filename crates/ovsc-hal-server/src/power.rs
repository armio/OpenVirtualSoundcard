//! Keeps the Mac from idle-sleeping while audio streams: an IOKit power
//! assertion (`kIOPMAssertPreventUserIdleSystemSleep`), held while the
//! engine runs (design section 14.3). On other systems it does nothing.

use std::io;

/// A held power assertion, released on drop.
#[derive(Debug)]
pub(crate) struct PowerAssertion {
    #[cfg(target_os = "macos")]
    id: u32,
}

impl PowerAssertion {
    /// Asks the system to stay awake, for the reason `name` (shown by
    /// `pmset -g assertions`).
    #[cfg(target_os = "macos")]
    pub(crate) fn take(name: &str) -> io::Result<PowerAssertion> {
        use std::ffi::CString;

        let kind = CfString::new(c"PreventUserIdleSystemSleep")?;
        let name = CString::new(name.replace('\0', " ")).unwrap_or_default();
        let name = CfString::new(&name)?;
        let mut id = 0u32;
        // SAFETY: both strings are live CFStrings and `id` is a valid
        // out-pointer.
        let r = unsafe {
            ffi::IOPMAssertionCreateWithName(kind.0, ffi::ASSERTION_LEVEL_ON, name.0, &mut id)
        };
        if r != 0 {
            return Err(io::Error::other(format!("IOPMAssertionCreateWithName failed: {r:#x}")));
        }
        Ok(PowerAssertion { id })
    }

    /// Nothing to hold on this system.
    #[cfg(not(target_os = "macos"))]
    pub(crate) fn take(_name: &str) -> io::Result<PowerAssertion> {
        Ok(PowerAssertion {})
    }
}

#[cfg(target_os = "macos")]
impl Drop for PowerAssertion {
    fn drop(&mut self) {
        // SAFETY: releases the assertion created in `take`, once.
        unsafe { ffi::IOPMAssertionRelease(self.id) };
    }
}

/// An owned CFString.
#[cfg(target_os = "macos")]
struct CfString(ffi::CFStringRef);

#[cfg(target_os = "macos")]
impl CfString {
    fn new(s: &std::ffi::CStr) -> io::Result<CfString> {
        // SAFETY: `s` is NUL-terminated; a null allocator is the default.
        let r = unsafe {
            ffi::CFStringCreateWithCString(std::ptr::null(), s.as_ptr(), ffi::STRING_ENCODING_UTF8)
        };
        if r.is_null() {
            return Err(io::Error::other("CFStringCreateWithCString failed"));
        }
        Ok(CfString(r))
    }
}

#[cfg(target_os = "macos")]
impl Drop for CfString {
    fn drop(&mut self) {
        // SAFETY: releases the reference created in `new`.
        unsafe { ffi::CFRelease(self.0) };
    }
}

#[cfg(target_os = "macos")]
mod ffi {
    use std::ffi::{c_char, c_void};

    pub type CFStringRef = *const c_void;
    /// `kCFStringEncodingUTF8`.
    pub const STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    /// `kIOPMAssertionLevelOn`.
    pub const ASSERTION_LEVEL_ON: u32 = 255;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub fn CFStringCreateWithCString(
            alloc: *const c_void,
            c_str: *const c_char,
            encoding: u32,
        ) -> CFStringRef;
        pub fn CFRelease(cf: *const c_void);
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        /// Returns an `IOReturn`, 0 on success.
        pub fn IOPMAssertionCreateWithName(
            assertion_type: CFStringRef,
            level: u32,
            name: CFStringRef,
            id: *mut u32,
        ) -> i32;
        pub fn IOPMAssertionRelease(id: u32) -> i32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assertions_are_taken_and_released() {
        // Two at once, released when they go out of scope.
        let _a = PowerAssertion::take("OpenVirtualSoundcard test").unwrap();
        let _b = PowerAssertion::take("OpenVirtualSoundcard\0test").unwrap();
    }
}
