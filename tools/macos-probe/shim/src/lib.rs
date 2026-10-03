//! XPC, Mach and sandbox helpers shared by the probe plug-in and daemon.

#![cfg(target_os = "macos")]
#![allow(non_camel_case_types)]

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;

pub type xpc_object_t = *mut c_void;
pub type xpc_connection_t = *mut c_void;

pub const DICTIONARY: c_int = 1;
pub const INTERRUPTED: c_int = 2;
pub const INVALID: c_int = 3;
pub const TERMINATION_IMMINENT: c_int = 4;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MachTimebase {
    pub numer: u32,
    pub denom: u32,
}

pub fn timebase() -> MachTimebase {
    let mut tb = MachTimebase::default();
    unsafe { mach_timebase_info(&mut tb) };
    tb
}

pub type EventFn =
    unsafe extern "C" fn(ctx: *mut c_void, conn: xpc_connection_t, event: xpc_object_t);

unsafe extern "C" {
    pub fn ovshim_kind(object: xpc_object_t) -> c_int;
    pub fn ovshim_connect(service: *const c_char, f: EventFn, ctx: *mut c_void)
    -> xpc_connection_t;
    pub fn ovshim_listen(service: *const c_char, f: EventFn, ctx: *mut c_void) -> xpc_connection_t;
    pub fn ovshim_bootstrap_look_up(service: *const c_char) -> c_int;
    pub fn ovshim_sandboxed() -> c_int;
    pub fn ovshim_sandbox_allows_name(operation: *const c_char, name: *const c_char) -> c_int;
    pub fn ovshim_sandbox_allows_path(operation: *const c_char, path: *const c_char) -> c_int;
    pub fn ovshim_make_realtime(period_ns: u32, computation_ns: u32, constraint_ns: u32) -> c_int;
    pub fn ovshim_progname() -> *const c_char;

    pub fn xpc_dictionary_create(
        keys: *const *const c_char,
        values: *const xpc_object_t,
        count: usize,
    ) -> xpc_object_t;
    pub fn xpc_dictionary_create_reply(original: xpc_object_t) -> xpc_object_t;
    pub fn xpc_dictionary_set_string(d: xpc_object_t, key: *const c_char, value: *const c_char);
    pub fn xpc_dictionary_set_uint64(d: xpc_object_t, key: *const c_char, value: u64);
    pub fn xpc_dictionary_set_int64(d: xpc_object_t, key: *const c_char, value: i64);
    pub fn xpc_dictionary_set_bool(d: xpc_object_t, key: *const c_char, value: bool);
    pub fn xpc_dictionary_set_value(d: xpc_object_t, key: *const c_char, value: xpc_object_t);
    pub fn xpc_dictionary_get_string(d: xpc_object_t, key: *const c_char) -> *const c_char;
    pub fn xpc_dictionary_get_uint64(d: xpc_object_t, key: *const c_char) -> u64;
    pub fn xpc_dictionary_get_int64(d: xpc_object_t, key: *const c_char) -> i64;
    pub fn xpc_dictionary_get_bool(d: xpc_object_t, key: *const c_char) -> bool;
    pub fn xpc_dictionary_get_value(d: xpc_object_t, key: *const c_char) -> xpc_object_t;
    pub fn xpc_shmem_create(region: *mut c_void, length: usize) -> xpc_object_t;
    pub fn xpc_shmem_map(xshmem: xpc_object_t, region: *mut *mut c_void) -> usize;
    pub fn xpc_connection_send_message(conn: xpc_connection_t, message: xpc_object_t);
    pub fn xpc_connection_send_message_with_reply_sync(
        conn: xpc_connection_t,
        message: xpc_object_t,
    ) -> xpc_object_t;
    pub fn xpc_connection_get_euid(conn: xpc_connection_t) -> libc::uid_t;
    pub fn xpc_connection_get_pid(conn: xpc_connection_t) -> libc::pid_t;
    pub fn xpc_connection_cancel(conn: xpc_connection_t);
    pub fn xpc_copy_description(object: xpc_object_t) -> *mut c_char;
    pub fn xpc_release(object: xpc_object_t);

    pub fn mach_absolute_time() -> u64;
    pub fn mach_timebase_info(info: *mut MachTimebase) -> c_int;
    pub fn clock_gettime_nsec_np(clock: libc::clockid_t) -> u64;
    pub fn proc_pidpath(pid: libc::pid_t, buffer: *mut c_void, size: u32) -> c_int;
}

/// A NUL-terminated copy of `s` (panics on interior NUL, which never occurs
/// for the literals used here).
pub fn cstr(s: &str) -> CString {
    CString::new(s).expect("no interior NUL")
}

/// An owned XPC dictionary.
pub struct Dict(pub xpc_object_t);

impl Dict {
    pub fn new() -> Self {
        Dict(unsafe { xpc_dictionary_create(ptr::null(), ptr::null(), 0) })
    }

    /// A reply to `request`, or `None` if the request expects no reply.
    ///
    /// # Safety
    /// `request` must be a live XPC dictionary received from a connection.
    pub unsafe fn reply_to(request: xpc_object_t) -> Option<Self> {
        let d = unsafe { xpc_dictionary_create_reply(request) };
        (!d.is_null()).then_some(Dict(d))
    }

    pub fn set_str(&self, key: &str, value: &str) -> &Self {
        let (k, v) = (cstr(key), cstr(&value.replace('\0', " ")));
        unsafe { xpc_dictionary_set_string(self.0, k.as_ptr(), v.as_ptr()) };
        self
    }

    pub fn set_u64(&self, key: &str, value: u64) -> &Self {
        let k = cstr(key);
        unsafe { xpc_dictionary_set_uint64(self.0, k.as_ptr(), value) };
        self
    }

    pub fn set_i64(&self, key: &str, value: i64) -> &Self {
        let k = cstr(key);
        unsafe { xpc_dictionary_set_int64(self.0, k.as_ptr(), value) };
        self
    }

    pub fn set_bool(&self, key: &str, value: bool) -> &Self {
        let k = cstr(key);
        unsafe { xpc_dictionary_set_bool(self.0, k.as_ptr(), value) };
        self
    }

    /// Stores `value` (retained by the dictionary; the caller keeps its own
    /// reference).
    ///
    /// # Safety
    /// `value` must be a live XPC object.
    pub unsafe fn set_value(&self, key: &str, value: xpc_object_t) -> &Self {
        let k = cstr(key);
        unsafe { xpc_dictionary_set_value(self.0, k.as_ptr(), value) };
        self
    }
}

impl Default for Dict {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Dict {
    fn drop(&mut self) {
        unsafe { xpc_release(self.0) };
    }
}

/// Reads a string from a borrowed dictionary.
///
/// # Safety
/// `d` must be a live XPC dictionary.
pub unsafe fn get_str(d: xpc_object_t, key: &str) -> Option<String> {
    let k = cstr(key);
    let p = unsafe { xpc_dictionary_get_string(d, k.as_ptr()) };
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

/// # Safety
/// `d` must be a live XPC dictionary.
pub unsafe fn get_u64(d: xpc_object_t, key: &str) -> u64 {
    let k = cstr(key);
    unsafe { xpc_dictionary_get_uint64(d, k.as_ptr()) }
}

/// # Safety
/// `d` must be a live XPC dictionary.
pub unsafe fn get_i64(d: xpc_object_t, key: &str) -> i64 {
    let k = cstr(key);
    unsafe { xpc_dictionary_get_int64(d, k.as_ptr()) }
}

/// # Safety
/// `d` must be a live XPC dictionary.
pub unsafe fn get_bool(d: xpc_object_t, key: &str) -> bool {
    let k = cstr(key);
    unsafe { xpc_dictionary_get_bool(d, k.as_ptr()) }
}

/// A borrowed value (not retained).
///
/// # Safety
/// `d` must be a live XPC dictionary.
pub unsafe fn get_value(d: xpc_object_t, key: &str) -> xpc_object_t {
    let k = cstr(key);
    unsafe { xpc_dictionary_get_value(d, k.as_ptr()) }
}

/// `xpc_copy_description` as a String.
///
/// # Safety
/// `object` must be a live XPC object.
pub unsafe fn describe(object: xpc_object_t) -> String {
    let p = unsafe { xpc_copy_description(object) };
    if p.is_null() {
        return "<null>".into();
    }
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    unsafe { libc::free(p.cast()) };
    s
}

pub fn progname() -> String {
    let p = unsafe { ovshim_progname() };
    if p.is_null() {
        return "?".into();
    }
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

pub fn pid_path(pid: libc::pid_t) -> String {
    let mut buf = vec![0u8; 4096];
    let n = unsafe { proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return format!("<proc_pidpath failed: {}>", std::io::Error::last_os_error());
    }
    String::from_utf8_lossy(&buf[..n as usize]).into_owned()
}

/// mach_absolute_time in nanoseconds.
pub fn host_ns() -> u64 {
    host_ticks_to_ns(unsafe { mach_absolute_time() })
}

pub fn host_ticks_to_ns(ticks: u64) -> u64 {
    let tb = timebase();
    (ticks as u128 * tb.numer as u128 / tb.denom as u128) as u64
}

/// sysctlbyname as a string.
pub fn sysctl_string(name: &str) -> Option<String> {
    let n = cstr(name);
    let mut len = 0usize;
    if unsafe { libc::sysctlbyname(n.as_ptr(), ptr::null_mut(), &mut len, ptr::null_mut(), 0) } != 0
    {
        return None;
    }
    let mut buf = vec![0u8; len];
    if unsafe {
        libc::sysctlbyname(n.as_ptr(), buf.as_mut_ptr().cast(), &mut len, ptr::null_mut(), 0)
    } != 0
    {
        return None;
    }
    buf.truncate(len);
    while buf.last() == Some(&0) {
        buf.pop();
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// sysctlbyname as an i32.
pub fn sysctl_i32(name: &str) -> Option<i32> {
    let n = cstr(name);
    let mut v: i32 = 0;
    let mut len = std::mem::size_of::<i32>();
    let r = unsafe {
        libc::sysctlbyname(n.as_ptr(), (&mut v as *mut i32).cast(), &mut len, ptr::null_mut(), 0)
    };
    (r == 0).then_some(v)
}

/// Shared names and layout used by both sides of the probe.
pub mod layout {
    /// Listed in the plug-in's AudioServerPlugIn_MachServices.
    pub const SERVICE: &str = "org.openvirtualsoundcard.probe";
    /// Registered by the daemon but not listed in the plug-in's Info.plist.
    pub const UNLISTED_SERVICE: &str = "org.openvirtualsoundcard.probe.unlisted";

    pub const UDP_PORT: u16 = 47800;
    pub const TCP_PORT: u16 = 47801;
    pub const UNIX_SOCKETS: [&str; 2] = ["/var/run/ovprobe.sock", "/tmp/ovprobe.sock"];

    /// Daemon-created POSIX shared memory: (name, mode, group is _coreaudiod).
    pub const PSHM: [(&str, u32, bool); 3] = [
        ("/ovprobe.666", 0o666, false),
        ("/ovprobe.660", 0o660, true),
        ("/ovprobe.600", 0o600, false),
    ];
    pub const PSHM_SIZE: usize = 64 * 1024;
    /// Created by the plug-in itself.
    pub const PSHM_PLUGIN: &str = "/ovprobe.plugin";

    /// Daemon-created files the plug-in tries to read.
    pub const DAEMON_FILES: [&str; 4] = [
        "/tmp/ovprobe-daemon.txt",
        "/private/var/tmp/ovprobe-daemon.txt",
        "/Users/Shared/ovprobe-daemon.txt",
        "/Library/Application Support/OpenVirtualSoundcard/ovprobe-daemon.txt",
    ];

    pub const REGION_SIZE: usize = 1 << 20;
    /// Offsets inside a shared region.
    pub const OFF_OWNER_MAGIC: usize = 0;
    pub const OFF_PEER_MAGIC: usize = 4096;
    pub const OFF_COUNTER: usize = 8192;
    pub const OFF_HOST_TICKS: usize = 8200;

    pub const PLUGIN_MAGIC: &[u8; 16] = b"ODPROBE-PLUGIN\0\0";
    pub const DAEMON_MAGIC: &[u8; 16] = b"ODPROBE-DAEMON\0\0";

    /// Large regions for sizing the OpenVirtualSoundcard layout: the v1 region and
    /// its fallback, plus a small one for the mapping-replacement test.
    pub const BIG_REGIONS: [(&str, usize); 3] =
        [("r64", 0x0401_0000), ("r32", 0x0201_0000), ("r1", 1 << 20)];
    /// Pattern stride in the big regions.
    pub const STRIDE: usize = 256 * 1024;
    pub const PAGE: usize = 16 * 1024;

    /// Value stored at the first and last u64 of stride `i` of a region of
    /// `size` bytes, by the daemon (lower half) or the plug-in (upper half).
    pub fn stride_pattern(daemon: bool, size: usize, i: usize) -> u64 {
        let who: u64 = if daemon { 0xD0 } else { 0xB0 };
        who << 56 | ((size >> 16) as u64) << 24 | i as u64
    }

    pub const COREAUDIOD_GID: u32 = 202;
    pub const REPORT_PATH: &str = "/var/log/ovprobe-report.txt";
}
