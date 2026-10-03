//! Wire formats used by Dante-compatible devices.
//!
//! Everything here is pure data manipulation: no sockets, no clocks, no
//! threads. The formats were reverse-engineered by the community (see
//! `docs/PROTOCOL.md`); names starting with `unknown` or comments saying
//! "observed" mark values whose meaning is not understood but which real
//! devices expect.
//!
//! | Module | Port | Purpose |
//! |---|---|---|
//! | [`dns`], [`discovery`] | 5353 | mDNS / DNS-SD advertisement and lookup |
//! | [`arc`] | 4440 | routing control: channels, flows, subscriptions, names |
//! | [`cmc`] | 8800 | device advertisement |
//! | [`dbcp`] | 4455 | flow setup between receiver and transmitter |
//! | [`conmon`] | 8700, 8702, 8708 | device info, notifications, heartbeat |
//! | [`audio`] | negotiated | media packets |
//!
//! Clock synchronisation (PTPv1) lives in the `ovsc-clock` crate.

pub mod arc;
pub mod audio;
pub mod cmc;
pub mod conmon;
pub mod dbcp;
pub mod discovery;
pub mod dns;
pub mod frame;
pub mod wire;

/// 8-byte device identifier carried in CMC and conmon messages.
pub type DeviceId = [u8; 8];

/// Errors returned by decoders.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("packet truncated: needed {needed} bytes at offset {offset}, packet has {len}")]
    Truncated { offset: usize, needed: usize, len: usize },
    #[error("invalid {0}")]
    Invalid(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;
