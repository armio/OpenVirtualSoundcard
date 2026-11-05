//! Finding transmit channels on the network.
//!
//! In production the [`Directory`] asks mDNS; tests and setups without
//! multicast can use a [`StaticDirectory`] filled by hand.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use ovsc_proto::discovery::ChannelTxt;

use crate::mdns::Mdns;
use crate::{Error, Result};

/// How long an mDNS lookup waits for answers.
const MDNS_TIMEOUT: Duration = Duration::from_secs(2);

/// A transmit channel found on the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedChannel {
    pub device: String,
    pub channel: String,
    /// The transmitter's address that flows are requested from.
    pub addr: Ipv4Addr,
    /// Every address the transmitter has, `addr` first. A computer with
    /// two interfaces on the network, Wi-Fi and Ethernet say, has one per
    /// interface (mDNS: the A records of its SRV target), and may send a
    /// flow's audio from either, whichever address the flow was requested
    /// from.
    pub addrs: Vec<Ipv4Addr>,
    /// The transmitter's flow-control port.
    pub flow_control_port: u16,
    pub txt: ChannelTxt,
}

/// Where subscriptions look up transmit channels.
#[derive(Clone)]
pub enum Directory {
    Mdns(Mdns),
    Static(StaticDirectory),
}

impl Directory {
    pub async fn resolve(&self, channel: &str, device: &str) -> Result<ResolvedChannel> {
        match self {
            Directory::Mdns(m) => m.resolve_channel(channel, device, MDNS_TIMEOUT).await,
            Directory::Static(s) => s.resolve(channel, device).await,
        }
    }

    /// What the directory already knows about `channel@device`, without
    /// asking the network or waiting: for mDNS, what its cache holds. The
    /// subscription manager uses it to tell, without blocking, whether a
    /// subscription that changed its spelling still names the same channel.
    pub async fn cached(&self, channel: &str, device: &str) -> Option<ResolvedChannel> {
        match self {
            // A lookup with no time to wait answers from the cache and
            // gives up before it would send a query.
            Directory::Mdns(m) => m.resolve_channel(channel, device, Duration::ZERO).await.ok(),
            Directory::Static(s) => s.get(channel, device),
        }
    }
}

/// A hand-filled directory.
#[derive(Clone, Default)]
pub struct StaticDirectory {
    entries: Arc<RwLock<HashMap<(String, String), ResolvedChannel>>>,
    /// How long a lookup of an unknown channel takes to fail.
    miss_delay: Arc<RwLock<Duration>>,
}

impl StaticDirectory {
    pub fn new() -> Self {
        Self::default()
    }

    fn key(channel: &str, device: &str) -> (String, String) {
        (channel.to_ascii_lowercase(), device.to_ascii_lowercase())
    }

    pub fn insert(&self, entry: ResolvedChannel) {
        let key = Self::key(&entry.channel, &entry.device);
        self.entries.write().unwrap_or_else(|e| e.into_inner()).insert(key, entry);
    }

    pub fn get(&self, channel: &str, device: &str) -> Option<ResolvedChannel> {
        self.entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&Self::key(channel, device))
            .cloned()
    }

    /// Makes lookups of unknown channels fail only after `delay`, as an
    /// mDNS lookup does once it times out; by default they fail at once.
    pub fn set_miss_delay(&self, delay: Duration) {
        *self.miss_delay.write().unwrap_or_else(|e| e.into_inner()) = delay;
    }

    async fn resolve(&self, channel: &str, device: &str) -> Result<ResolvedChannel> {
        if let Some(entry) = self.get(channel, device) {
            return Ok(entry);
        }
        let delay = *self.miss_delay.read().unwrap_or_else(|e| e.into_inner());
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        self.get(channel, device).ok_or_else(|| Error::NotFound(format!("{channel}@{device}")))
    }
}
