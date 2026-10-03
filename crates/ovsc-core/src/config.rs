//! Device configuration.

use std::net::Ipv4Addr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use ovsc_proto::{arc, cmc, conmon, dbcp, discovery};

use crate::{Error, Result};

/// The receive latencies a device may run at, ms.
pub const MIN_LATENCY_MS: f64 = 0.25;
pub const MAX_LATENCY_MS: f64 = 40.0;

/// UDP ports of the device's services.
///
/// Dante peers expect the defaults; other values are only useful for running
/// several OpenVirtualSoundcard devices on one IP address (tests, development).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ports {
    pub arc: u16,
    pub cmc: u16,
    pub flow_control: u16,
    pub settings: u16,
}

impl Default for Ports {
    fn default() -> Self {
        Self {
            arc: arc::PORT,
            cmc: cmc::PORT,
            flow_control: dbcp::PORT,
            settings: conmon::SETTINGS_PORT,
        }
    }
}

impl Ports {
    /// Default ports shifted by `offset` (e.g. 1 → 4441, 8801, 4456, 8701).
    pub fn offset(offset: u16) -> Self {
        let d = Self::default();
        Self {
            arc: d.arc + offset,
            cmc: d.cmc + offset,
            flow_control: d.flow_control + offset,
            settings: d.settings + offset,
        }
    }
}

/// A channel list: either a count (channels get default names `01`, `02`, …)
/// or explicit names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Channels {
    Count(u16),
    Names(Vec<String>),
}

impl Channels {
    pub fn names(&self) -> Vec<String> {
        match self {
            Channels::Count(n) => (1..=*n).map(|i| format!("{i:02}")).collect(),
            Channels::Names(names) => names.clone(),
        }
    }
}

/// A subscription set up at start (in addition to restored state).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitialSubscription {
    /// 1-based receive channel.
    pub rx_channel: u16,
    pub tx_channel: String,
    pub tx_device: String,
}

/// Everything needed to start a device.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceConfig {
    /// Device name shown in controllers (1–31 of `A-Z a-z 0-9 -`).
    pub name: String,
    /// Network interface name (e.g. `en0`, `eth0`) or IPv4 address. Empty
    /// means the interface of the default route.
    pub interface: String,
    pub sample_rate: u32,
    /// Bit depth of transmitted audio: 16, 24 or 32.
    pub bits_per_sample: u16,
    pub tx_channels: Channels,
    pub rx_channels: Channels,
    /// Receive latency: audio is played this long after its timestamp.
    pub latency_ms: f64,
    /// How late (after its timestamp) a transmit packet is sent, in µs.
    /// Keeps receivers from seeing timestamps from "the future" when clocks
    /// differ slightly.
    pub tx_guard_us: u32,
    /// Maximum channels per receive flow we request.
    pub max_channels_per_rx_flow: u16,
    pub ports: Ports,
    /// Advertise over mDNS and send conmon multicasts. Disable for tests on
    /// hosts without multicast.
    pub discovery: bool,
    /// Where names and subscriptions are persisted. `None` disables it.
    pub state_file: Option<PathBuf>,
    /// Device id (16 hex digits). Default: derived from IP and process id.
    pub device_id: Option<String>,
    /// Distinguishes several devices on one host.
    pub process_id: u16,
    /// Samples per channel kept in each ring buffer.
    pub ring_capacity: usize,
    pub subscriptions: Vec<InitialSubscription>,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            interface: String::new(),
            sample_rate: 48_000,
            bits_per_sample: 24,
            tx_channels: Channels::Count(2),
            rx_channels: Channels::Count(2),
            latency_ms: 4.0,
            tx_guard_us: 500,
            max_channels_per_rx_flow: 8,
            ports: Ports::default(),
            discovery: true,
            state_file: None,
            device_id: None,
            process_id: 0,
            ring_capacity: crate::buffer::DEFAULT_CAPACITY,
            subscriptions: Vec::new(),
        }
    }
}

/// Upper bounds that keep packets and tables within protocol limits.
pub const MAX_CHANNELS: usize = 256;

/// The sample rates a device can run at.
pub const SAMPLE_RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

/// The bit depths a device can transmit.
pub const BITS_PER_SAMPLE: [u16; 3] = [16, 24, 32];

impl DeviceConfig {
    pub fn validate(&self) -> Result<()> {
        if !self.name.is_empty() {
            discovery::validate_device_name(&self.name)
                .map_err(|_| Error::Config(format!("invalid device name {:?}", self.name)))?;
        }
        if !SAMPLE_RATES.contains(&self.sample_rate) {
            return Err(Error::Config(format!("unsupported sample rate {}", self.sample_rate)));
        }
        if !BITS_PER_SAMPLE.contains(&self.bits_per_sample) {
            return Err(Error::Config(format!("unsupported bit depth {}", self.bits_per_sample)));
        }
        for (what, list) in [("tx", &self.tx_channels), ("rx", &self.rx_channels)] {
            let names = list.names();
            if names.len() > MAX_CHANNELS {
                return Err(Error::Config(format!("too many {what} channels")));
            }
            for n in &names {
                discovery::validate_channel_name(n)
                    .map_err(|_| Error::Config(format!("invalid {what} channel name {n:?}")))?;
            }
            let mut sorted = names.clone();
            sorted.sort();
            sorted.dedup();
            if sorted.len() != names.len() {
                return Err(Error::Config(format!("duplicate {what} channel names")));
            }
        }
        if !(MIN_LATENCY_MS..=MAX_LATENCY_MS).contains(&self.latency_ms) {
            return Err(Error::Config(format!(
                "latency_ms must be between {MIN_LATENCY_MS} and {MAX_LATENCY_MS}"
            )));
        }
        if self.max_channels_per_rx_flow == 0 {
            return Err(Error::Config("max_channels_per_rx_flow must be at least 1".into()));
        }
        if let Some(id) = &self.device_id {
            parse_device_id(id)?;
        }
        Ok(())
    }

    pub fn latency_samples(&self) -> u64 {
        (self.latency_ms * self.sample_rate as f64 / 1000.0).round() as u64
    }

    pub fn latency_ns(&self) -> u32 {
        (self.latency_ms * 1e6).round() as u32
    }
}

pub(crate) fn parse_device_id(s: &str) -> Result<[u8; 8]> {
    let bytes = hex::decode(s).map_err(|_| Error::Config("device_id must be hex".into()))?;
    bytes.try_into().map_err(|_| Error::Config("device_id must be 8 bytes (16 hex digits)".into()))
}

/// Resolves the configured interface to an IPv4 address.
pub(crate) fn parse_interface_ip(s: &str) -> Option<Ipv4Addr> {
    s.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_toml() {
        let cfg: DeviceConfig = toml::from_str(
            r#"
            name = "studio-mac"
            sample_rate = 96000
            tx_channels = 4
            rx_channels = ["Left", "Right"]
            latency_ms = 2.0
            [ports]
            arc = 5440
            [[subscriptions]]
            rx_channel = 1
            tx_channel = "01"
            tx_device = "stagebox"
            "#,
        )
        .unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.tx_channels.names(), ["01", "02", "03", "04"]);
        assert_eq!(cfg.rx_channels.names(), ["Left", "Right"]);
        assert_eq!(cfg.ports.arc, 5440);
        assert_eq!(cfg.ports.cmc, 8800);
        assert_eq!(cfg.latency_samples(), 192);
        assert_eq!(cfg.subscriptions[0].tx_device, "stagebox");
    }

    #[test]
    fn rejects_bad_values() {
        let bad = |f: fn(&mut DeviceConfig)| {
            let mut c = DeviceConfig::default();
            f(&mut c);
            c.validate().is_err()
        };
        assert!(bad(|c| c.sample_rate = 22_050));
        assert!(bad(|c| c.bits_per_sample = 20));
        assert!(bad(|c| c.name = "bad name".into()));
        assert!(bad(|c| c.rx_channels = Channels::Names(vec!["a".into(), "a".into()])));
        assert!(bad(|c| c.device_id = Some("xyz".into())));
        assert!(!bad(|_| {}));
    }

    #[test]
    fn port_offsets() {
        assert_eq!(Ports::offset(2).flow_control, 4457);
    }
}
