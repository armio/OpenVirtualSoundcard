//! Shared memory between the OpenVirtualSoundcard daemon and its macOS Core Audio
//! driver.
//!
//! The daemon creates one region per process and hands it to the driver
//! over XPC. Everything that crosses the process boundary lives here, so
//! both sides compile the same definitions:
//!
//! * [`layout`]: the region's constants, header and a validated view
//!   ([`layout::RegionRef`]);
//! * [`ring`]: the per-channel sample rings, slot-compatible with
//!   `ovsc-core`'s `TimedRing`;
//! * [`clock`]: the clock block, a seqlock that the driver reads with a
//!   bounded number of tries;
//! * [`status`]: the daemon's status, the driver's counters and the IO trace;
//! * [`time`]: host time and media time conversions, and the clock types
//!   shared with `ovsc-clock`;
//! * [`sample`]: conversion between ring samples and Core Audio's Float32;
//! * [`timeline`]: the device timeline the driver reports to Core Audio.
//!
//! The crate is `no_std` and has no dependencies, so the driver can use it
//! on its real-time threads. Only `layout` contains unsafe code.
//!
//! The shared blocks need lock-free 64-bit atomics, which every supported
//! host has. On targets without them (some microcontrollers, used to check
//! that the crate stays `no_std`) only [`time`], [`sample`], [`timeline`]
//! and the clock record types are available.

#![no_std]
#![deny(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod clock;
#[cfg(target_has_atomic = "64")]
pub mod layout;
#[cfg(target_has_atomic = "64")]
pub mod ring;
pub mod sample;
#[cfg(target_has_atomic = "64")]
pub mod status;
pub mod time;
pub mod timeline;
