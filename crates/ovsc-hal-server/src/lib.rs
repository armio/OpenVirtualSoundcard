//! The daemon's side of the OpenVirtualSoundcard macOS driver (design sections 5.4, 7.2,
//! 10 and 14).
//!
//! The driver is a Core Audio HAL plug-in in coreaudiod's helper process. It
//! talks to the daemon over one XPC Mach service and shares one memory
//! region with it, which this crate provides:
//!
//! * [`HalRegion`]: the region, created once per daemon process. The device
//!   engine's rings live in it ([`HalRegion::external_rings`]).
//! * [`ShmClockMirror`]: copies the media clock into the region's clock
//!   block on every change, counting discontinuities.
//! * [`HalServer`]: the service. It welcomes driver instances with the region
//!   and the configuration of the engine ([`driver_config`]), pushes a new
//!   configuration when the device's names change, writes the region's
//!   daemon status (a 10 Hz heartbeat, the engine words, packet counters),
//!   logs the driver's counters and IO trace, and keeps the Mac awake while
//!   the engine runs.
//!
//! The transports come from `ovsc-ipc`: `XpcServer` on macOS, the
//! in-memory `MemServer` in tests. Everything builds and runs on every OS;
//! only the power assertion is macOS-specific.
//!
//! Startup order in the daemon: create the region, start the server (so the
//! driver can attach and show the configured layout before the network is
//! up), then, once the engine runs on the region's rings with the mirror on
//! its clock, call [`HalServer::engine_started`].

pub mod config;
pub mod mirror;
mod power;
pub mod region;
pub mod server;
pub mod status;

pub use config::{ConfigError, EngineInfo, HalOptions, driver_config};
pub use mirror::ShmClockMirror;
pub use region::HalRegion;
pub use server::HalServer;
pub use status::HalStatus;
