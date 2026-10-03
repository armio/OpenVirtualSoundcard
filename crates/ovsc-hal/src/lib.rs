//! OpenVirtualSoundcard.driver: the Core Audio server plug-in (AudioServerPlugIn) that
//! gives macOS one audio device backed by the OpenVirtualSoundcard daemon.
//!
//! The crate builds as a static library that `packaging/macos/build-driver.sh`
//! links into the bundle's MH_BUNDLE, exporting only [`OpenVirtualSoundcard_Factory`],
//! and as an rlib so the whole driver runs under `cargo test` on any OS:
//!
//! * [`abi`] mirrors the C ABI, checked against the SDK by `abi_check.c`;
//! * [`entry`] holds the factory, the driver object and its 23-entry vtable,
//!   every entry guarded against panics;
//! * [`platform`] abstracts host time, CoreFoundation and logging, with a
//!   stub for tests;
//! * [`host`] wraps the host interface the HAL passes to Initialize;
//! * [`model`] publishes the HAL objects and answers their properties;
//! * [`io`] runs the zero time stamps and the IO on the daemon's clock and
//!   shared region;
//! * [`link`] connects to the daemon: the handshake, the region the IO
//!   engine runs on, and the daemon's configurations;
//! * [`testing`] has a fake host and hooks for tests.
//!
//! The architecture is described in the macOS design document (sections 5.3
//! and 13): the plug-in creates no threads, real-time paths never allocate,
//! lock or log, and all daemon traffic runs on one serial queue.

// The SDK's constant names (kAudio...) are matched on throughout.
#![allow(non_upper_case_globals)]

pub mod abi;
mod atomic;
mod driver;
pub mod entry;
mod ffi;
pub mod host;
pub mod io;
pub mod link;
pub mod model;
pub mod platform;
pub mod testing;

pub use entry::{DriverObject, LinkFactory, OpenVirtualSoundcard_Factory, new_driver_object};
pub use link::ClientTransport;
