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

/// A transmit channel found on the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedChannel {
    pub device: String,
    pub channel: String,
    pub addr: Ipv4Addr,
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
            Directory::Mdns(m) => m.resolve_channel(channel, device, Duration::from_secs(2)).await,
            Directory::Static(s) => {
                s.get(channel, device).ok_or_else(|| Error::NotFound(format!("{channel}@{device}")))
            }
        }
    }
}

/// A hand-filled directory.
#[derive(Clone, Default)]
pub struct StaticDirectory {
    entries: Arc<RwLock<HashMap<(String, String), ResolvedChannel>>>,
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
}
