//! The link between the OpenVirtualSoundcard daemon and its macOS Core Audio driver.
//!
//! The driver (a HAL plug-in in coreaudiod's helper process) and the daemon
//! talk over one XPC Mach service and share one memory region (design
//! sections 5.2 and 10):
//!
//! * [`protocol`]: the messages, the driver's configuration with its
//!   validation and storage text, and the latency and safety offsets;
//! * [`transport`]: the client and server interfaces, with XPC's semantics;
//! * `xpc` (macOS): the XPC transports, over a small C shim;
//! * [`mem`]: an in-process transport for tests on every OS;
//! * [`region`]: the shared region, its handle and the driver's mapping;
//! * [`log`]: os_log on macOS, standard error elsewhere.
//!
//! The crate builds on every OS; only `xpc` and the C shim are macOS-only.

pub mod log;
pub mod mem;
pub mod protocol;
pub mod region;
pub mod transport;
#[cfg(target_os = "macos")]
pub mod xpc;
