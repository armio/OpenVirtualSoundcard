//! In-process PTPv1 (IEEE 1588-2002) follower.
//!
//! Dante devices elect a PTPv1 master among themselves and synchronise to it
//! over UDP multicast (`224.0.1.129`, ports 319 and 320). OpenVirtualSoundcard only ever
//! follows: [`PtpFollower`] listens for Sync/Follow_Up messages, measures the
//! path delay with Delay_Req/Delay_Resp, picks the best master and steers a
//! [`MediaClock`] to the master's time.
//!
//! Timestamps are taken in software ([`crate::local_now_ns`] right after a
//! packet is received, right before one is sent), which is portable and,
//! with the filtering done by the [`servo`], good to tens of microseconds on
//! a quiet LAN, plenty for audio with latencies of a millisecond or more.
//!
//! # Lifecycle
//!
//! [`MediaClock::status`] reports [`ClockState::Unlocked`](crate::ClockState)
//! until a master is heard, then `Locking`: after about two seconds of Sync
//! messages the clock gets its first time (a step), and once the error has
//! stayed below 50 µs for a few Syncs the state becomes `Locked`. From then
//! on the clock is only ever slewed, never stepped, unless the error exceeds
//! 2 ms (the master's clock jumped) or the master changes. If the master
//! goes silent for [`PtpConfig::master_timeout`] the state drops back to
//! `Unlocked` but the clock keeps running on its frequency estimate for
//! [`HOLDOVER`] before it is invalidated.
//!
//! ```no_run
//! # async fn example() -> std::io::Result<()> {
//! use std::net::Ipv4Addr;
//! use ovsc_clock::ptp::{PtpConfig, PtpFollower};
//!
//! let config = PtpConfig::new(Ipv4Addr::new(192, 168, 1, 20), [0x02, 0, 0, 0, 0, 0x01]);
//! let (follower, clock) = PtpFollower::start(config).await?;
//! // Any thread, including real-time audio threads, can now read the time:
//! if let Some(now) = clock.now_samples(48_000) {
//!     println!("network time: sample {now}");
//! }
//! follower.shutdown();
//! # Ok(())
//! # }
//! ```
//!
//! # Modules
//!
//! * [`wire`]: message encoding and decoding.
//! * [`bmc`]: master selection.
//! * [`servo`]: the clock servo (pure, deterministic).
//! * [`master`]: a minimal master for development without Dante hardware.

use std::io;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use tracing::info;

use crate::{ClockStatus, MediaClock, local_now_ns};

pub mod bmc;
mod follower;
pub mod master;
mod net;
pub mod servo;
pub mod wire;

use follower::{ClockOutput, Core, CoreConfig};
use net::Endpoints;
use servo::ServoConfig;
use wire::Subdomain;

/// How long the clock keeps running on its last estimate after the master
/// disappears, before it is invalidated.
pub const HOLDOVER: Duration = Duration::from_secs(30);

/// Configuration of the PTPv1 follower.
#[derive(Clone, Debug)]
pub struct PtpConfig {
    /// IPv4 address of the interface connected to the Dante network.
    pub interface: Ipv4Addr,
    /// Our PTPv1 source UUID, normally the interface's MAC address.
    pub uuid: [u8; 6],
    /// PTPv1 subdomain name; Dante uses the default `_DFLT`.
    pub subdomain: String,
    /// UDP port of event messages (Sync, Delay_Req). Standard: 319.
    pub event_port: u16,
    /// UDP port of general messages (Follow_Up, Delay_Resp). Standard: 320.
    pub general_port: u16,
    /// Multicast group of the default PTP domain. Standard: 224.0.1.129.
    pub multicast_group: Ipv4Addr,
    /// Interval between our Delay_Req messages.
    pub delay_req_interval: Duration,
    /// How long without Sync messages before the master is considered gone.
    pub master_timeout: Duration,
}

impl PtpConfig {
    /// Standard PTPv1 settings for the given interface.
    pub fn new(interface: Ipv4Addr, uuid: [u8; 6]) -> Self {
        Self {
            interface,
            uuid,
            subdomain: "_DFLT".to_owned(),
            event_port: 319,
            general_port: 320,
            multicast_group: Ipv4Addr::new(224, 0, 1, 129),
            delay_req_interval: Duration::from_secs(1),
            master_timeout: Duration::from_secs(5),
        }
    }

    fn endpoints(&self) -> Endpoints {
        Endpoints {
            interface: self.interface,
            group: self.multicast_group,
            event_port: self.event_port,
            general_port: self.general_port,
        }
    }
}

/// Parses a subdomain name, or explains why it is unusable.
pub(crate) fn parse_subdomain(name: &str) -> io::Result<Subdomain> {
    Subdomain::new(name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("PTP subdomain {name:?} is longer than 16 bytes"),
        )
    })
}

/// A running PTPv1 follower.
///
/// Dropping it (or calling [`shutdown`](Self::shutdown)) stops the
/// background task and invalidates the clock.
#[derive(Debug)]
pub struct PtpFollower {
    task: tokio::task::JoinHandle<()>,
    output: Arc<ClockOutput>,
    clock: MediaClock,
}

impl PtpFollower {
    /// Starts following the best PTPv1 master on the network.
    ///
    /// Binds the PTP ports, joins the multicast group on
    /// `config.interface` and spawns the follower on the current tokio
    /// runtime (so this must run inside one), then returns immediately. The
    /// returned clock has no time until a master has been heard for a couple
    /// of seconds, and reports [`ClockState::Locked`](crate::ClockState) a
    /// second or two after that; see [`MediaClock::status`].
    ///
    /// Binding the standard ports (319/320) needs privileges on Linux; the
    /// error says how to grant them.
    pub async fn start(config: PtpConfig) -> io::Result<(PtpFollower, MediaClock)> {
        let subdomain = parse_subdomain(&config.subdomain)?;
        let endpoints = config.endpoints();
        let (event, general) = endpoints.bind()?;

        let (clock, writer) = MediaClock::new();
        let output = Arc::new(ClockOutput::new(writer));
        // Different followers should pick different Delay_Req schedules.
        let seed = config.uuid.iter().fold(local_now_ns(), |acc, &b| acc.rotate_left(8) ^ b as u64);
        let core = Core::new(CoreConfig {
            uuid: config.uuid,
            subdomain,
            delay_req_interval: config.delay_req_interval,
            master_timeout: config.master_timeout,
            holdover: HOLDOVER,
            servo: ServoConfig::default(),
            seed,
        });
        let task = tokio::spawn(follower::run(core, event, general, endpoints, output.clone()));
        info!(
            interface = %config.interface,
            uuid = %follower::fmt_uuid(&config.uuid),
            group = %config.multicast_group,
            event_port = config.event_port,
            general_port = config.general_port,
            "PTPv1 follower started"
        );
        Ok((PtpFollower { task, output, clock: clock.clone() }, clock))
    }

    /// Current synchronisation status (same as [`MediaClock::status`]).
    pub fn status(&self) -> ClockStatus {
        self.clock.status()
    }

    /// Another handle to the clock this follower drives.
    pub fn clock(&self) -> MediaClock {
        self.clock.clone()
    }

    /// Stops the follower. The clock is invalidated.
    pub fn shutdown(self) {
        drop(self);
    }
}

impl Drop for PtpFollower {
    fn drop(&mut self) {
        self.task.abort();
        self.output.stop();
    }
}
