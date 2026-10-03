//! OpenVirtualSoundcard device engine.
//!
//! [`Device`] runs everything a Dante-compatible network audio device needs:
//!
//! * mDNS advertisement and lookup ([`mdns`], [`directory`]);
//! * the control servers controllers talk to (ARC, CMC, conmon);
//! * the flow-control server that lets receivers request audio from us, and
//!   the real-time transmit thread that sends it;
//! * the subscription manager that requests audio from other devices and
//!   receives it.
//!
//! Audio enters and leaves through per-channel [`buffer::TimedRing`]s exposed
//! by [`AudioIo`]; an audio backend (a sound-card bridge, a file recorder, a
//! virtual-device driver) only ever touches those rings and the media clock.

pub mod buffer;
pub mod client;
pub mod config;
mod control;
mod device;
pub mod directory;
mod info;
pub mod mdns;
pub mod net;
mod persist;
pub mod rt;
mod rx;
mod state;
mod tx;

pub use config::{Channels, DeviceConfig, InitialSubscription, Ports};
pub use device::{
    AudioIo, Device, DeviceObserver, DeviceStats, ExternalRings, RxChannelStatus, StartOptions,
    TxFlowStatus,
};
pub use info::DeviceInfo;
pub use persist::SavedState;
pub use state::FormatRequest;

/// Errors of the device engine.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("configuration: {0}")]
    Config(String),
    #[error("protocol: {0}")]
    Protocol(#[from] ovsc_proto::Error),
    #[error("timed out waiting for {0}")]
    Timeout(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("request refused by peer (result code {0:#06x})")]
    Refused(u16),
}

pub type Result<T> = std::result::Result<T, Error>;
