//! Logging for code that has no `tracing` subscriber, the driver above all:
//! the unified log (os_log) on macOS, standard error elsewhere.
//!
//! Never called on real-time paths: formatting allocates and os_log may
//! block.

use std::fmt;

/// Message severity, from the least to the most severe. The values are
/// os_log's `os_log_type_t`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    Debug,
    Info,
    /// Notable events (os_log's default level, persisted).
    Default,
    Error,
    /// Faults in our own code.
    Fault,
}

impl Level {
    /// The matching `os_log_type_t`.
    #[cfg(target_os = "macos")]
    fn os_log_type(self) -> u8 {
        match self {
            Level::Default => 0x00,
            Level::Info => 0x01,
            Level::Debug => 0x02,
            Level::Error => 0x10,
            Level::Fault => 0x11,
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Default => "notice",
            Level::Error => "error",
            Level::Fault => "fault",
        })
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{CString, c_char, c_int};

    unsafe extern "C" {
        fn ovshim_log_init(subsystem: *const c_char, category: *const c_char);
        fn ovshim_log(level: c_int, msg: *const c_char);
    }

    /// `s` up to its first NUL.
    fn c_text(s: &str) -> CString {
        let end = s.find('\0').unwrap_or(s.len());
        CString::new(&s[..end]).unwrap_or_default()
    }

    pub fn init(subsystem: &str, category: &str) {
        let (s, c) = (c_text(subsystem), c_text(category));
        // SAFETY: both strings are NUL-terminated; os_log copies them.
        unsafe { ovshim_log_init(s.as_ptr(), c.as_ptr()) };
    }

    pub fn log(level: super::Level, msg: &str) {
        let m = c_text(msg);
        // SAFETY: `m` is NUL-terminated and outlives the call.
        unsafe { ovshim_log(level.os_log_type() as c_int, m.as_ptr()) };
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use std::io::Write;
    use std::sync::OnceLock;

    static PREFIX: OnceLock<String> = OnceLock::new();

    pub fn init(subsystem: &str, category: &str) {
        let _ = PREFIX.set(format!("[{subsystem}:{category}] "));
    }

    pub fn log(level: super::Level, msg: &str) {
        let prefix = PREFIX.get().map_or("", String::as_str);
        // A closed stderr is not worth a panic.
        let _ = writeln!(std::io::stderr().lock(), "{prefix}{level}: {msg}");
    }
}

/// Chooses the subsystem and category of every later message (os_log's
/// `os_log_create`). The first call wins; before it, messages go to the
/// default log.
pub fn log_init(subsystem: &str, category: &str) {
    imp::init(subsystem, category);
}

/// Logs one message. On macOS it is marked public, so `log show` prints it
/// in full.
pub fn log(level: Level, msg: &str) {
    imp::log(level, msg);
}
