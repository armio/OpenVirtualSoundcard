//! Turns a failed part of the daemon into a restart of the daemon.
//!
//! Tokio catches a panic in a task and a thread simply ends: either way the
//! rest of the daemon would run on without it. Without the driver heartbeat
//! the device goes silent, without the PTP follower the clock freezes while
//! still reporting Locked, without the transmit thread the flows stay listed
//! but silent, and launchd (or systemd) never restarts the process because
//! it never exits.
//!
//! So [`install_panic_hook`] logs every panic as an error and raises a
//! process-wide flag, and the engine loops check it, and the parts they can
//! see ([`fault`]), every second. On a fault they shut down in order (the
//! driver gets its goodbye first) and the daemon exits with an error, for
//! its supervisor to start a fresh one. `panic = "abort"` is not an option:
//! the driver's static library, built from the same workspace, relies on
//! catching panics.

use std::sync::atomic::{AtomicBool, Ordering};

use tracing::error;

use ovsc_core::Device;

use crate::netwatch::Probe;

/// Raised by the panic hook.
pub static PANICKED: AtomicBool = AtomicBool::new(false);

/// Logs every panic at error level and raises [`PANICKED`], then lets the
/// previous hook print it as usual.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        PANICKED.store(true, Ordering::SeqCst);
        let thread = std::thread::current();
        error!(
            "thread {} panicked: {info}; the daemon shuts down to be restarted",
            thread.name().unwrap_or("<unnamed>")
        );
        previous(info);
    }));
}

/// What the engine loops watch besides their engine.
#[derive(Clone, Copy, Debug)]
pub struct Supervision {
    /// How they look at the network interface.
    pub probe: Probe,
    /// Raised when a part of the daemon panicked: [`PANICKED`], except in
    /// tests.
    pub panicked: &'static AtomicBool,
}

impl Supervision {
    /// The daemon's: the system's interfaces and the panic hook's flag.
    pub fn daemon() -> Supervision {
        Supervision { probe: Probe::System, panicked: &PANICKED }
    }

    /// Why the daemon must restart, if it must: a panic anywhere, or a part
    /// of the running `device` that stopped on its own.
    pub fn fault(&self, device: Option<&Device>) -> Option<String> {
        fault(self.panicked.load(Ordering::SeqCst), device.and_then(Device::stopped_part))
    }
}

/// The message for a fault: a panic, or a part of the device that stopped.
pub fn fault(panicked: bool, stopped: Option<&str>) -> Option<String> {
    if panicked {
        return Some("a part of the daemon panicked (see the error above)".to_owned());
    }
    stopped.map(|part| format!("the device's {part} stopped"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faults_are_described() {
        assert_eq!(fault(false, None), None);
        assert!(fault(true, None).unwrap().contains("panicked"));
        assert_eq!(
            fault(false, Some("transmit thread")).unwrap(),
            "the device's transmit thread stopped"
        );
    }

    #[test]
    fn a_panicking_thread_raises_the_flag() {
        // The hook is process-wide, and test threads panic on failed
        // assertions: run it in a child process of this test binary.
        if std::env::var_os("OVSC_PANIC_HOOK_CHILD").is_some() {
            install_panic_hook();
            let t = std::thread::Builder::new()
                .name("doomed".into())
                .spawn(|| panic!("on purpose"))
                .unwrap();
            assert!(t.join().is_err());
            assert!(PANICKED.load(Ordering::SeqCst));
            println!("FLAG RAISED");
            return;
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "supervise::tests::a_panicking_thread_raises_the_flag",
                "--nocapture",
            ])
            .env("OVSC_PANIC_HOOK_CHILD", "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
        assert!(stdout.contains("FLAG RAISED"), "{stdout}");
    }
}
