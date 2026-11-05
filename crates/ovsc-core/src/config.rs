//! Device configuration.

use std::net::Ipv4Addr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use ovsc_proto::{arc, cmc, conmon, dbcp, discovery};

use crate::{Error, Result};

/// The receive and transmit latencies a device may run at, ms. 1 ms is the
/// least Dante Controller offers (ARC `MIN_LATENCY`, `control.rs`) and the
/// least the default transmit guard allows ([`min_latency_ms`]).
pub const MIN_LATENCY_MS: f64 = 1.0;
pub const MAX_LATENCY_MS: f64 = 40.0;

/// How far a latency must reach beyond the transmit guard, in µs.
///
/// The transmit thread sends the packet stamped `t` once media time reaches
/// `t + tx_guard` (`tx.rs`), so it arrives that late plus the thread's
/// wake-up delay plus the network's transit. A receiver plays frame `t` at
/// `t + latency` (docs/PROTOCOL.md 8.4), so a latency of `tx_guard` or less
/// makes every packet late. This is the room left for the wake-up and the
/// network.
///
/// It bounds the receive latency too, though not for OVSC peers: they
/// advertise a transmit latency (TXT `latency_ns`) of at least their guard
/// plus this headroom, and our receiver plays each flow at the larger of
/// its own latency and the one its transmitter advertises (docs/PROTOCOL.md
/// 2.2 and 8.4; `flow_latency` in `rx.rs`). The floor on our own receive
/// latency guards our transmit latency while `tx_latency_ms` is unset (it
/// is the receive latency then), and flows from transmitters that send
/// later than they advertise.
pub const LATENCY_HEADROOM_US: u32 = 500;

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
    /// Transmit latency: the least receive latency the device asks of the
    /// receivers of its transmit channels (TXT `latency_ns`). They play its
    /// audio at the larger of this and their own latency (docs/PROTOCOL.md
    /// 2.2 and 8.4). `None`: `latency_ms`.
    pub tx_latency_ms: Option<f64>,
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
    /// Show lock: refuse controllers' changes of the latency, sample rate
    /// and encoding (they interrupt audio both ways) and of the device and
    /// channel names (other devices subscribe by them). Subscriptions stay
    /// open. The owner's app can turn it on and off; its choice is saved in
    /// `state_file` and then replaces this.
    pub lock_remote_settings: bool,
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
            tx_latency_ms: None,
            tx_guard_us: 500,
            max_channels_per_rx_flow: 8,
            ports: Ports::default(),
            discovery: true,
            lock_remote_settings: false,
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
        if min_latency_ms(self.tx_guard_us) > MAX_LATENCY_MS {
            return Err(Error::Config(format!(
                "tx_guard_us must be at most {}",
                MAX_LATENCY_MS as u32 * 1000 - LATENCY_HEADROOM_US
            )));
        }
        check_latency_ms("latency_ms", self.latency_ms, self.tx_guard_us)?;
        if let Some(ms) = self.tx_latency_ms {
            check_latency_ms("tx_latency_ms", ms, self.tx_guard_us)?;
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
        ms_to_samples(self.latency_ms, self.sample_rate)
    }

    pub fn latency_ns(&self) -> u32 {
        ms_to_ns(self.latency_ms)
    }

    /// The transmit latency in samples: `tx_latency_ms`, or `latency_ms`.
    pub fn tx_latency_samples(&self) -> u64 {
        ms_to_samples(self.tx_latency_ms.unwrap_or(self.latency_ms), self.sample_rate)
    }

    /// The transmit latency in ns: `tx_latency_ms`, or `latency_ms`.
    pub fn tx_latency_ns(&self) -> u32 {
        ms_to_ns(self.tx_latency_ms.unwrap_or(self.latency_ms))
    }
}

fn ms_to_samples(ms: f64, rate: u32) -> u64 {
    (ms * rate as f64 / 1000.0).round() as u64
}

fn ms_to_ns(ms: f64) -> u32 {
    (ms * 1e6).round() as u32
}

/// The least receive or transmit latency a device whose transmit guard is
/// `tx_guard_us` accepts, in ms: [`MIN_LATENCY_MS`], or the guard plus
/// [`LATENCY_HEADROOM_US`] if that is more.
pub fn min_latency_ms(tx_guard_us: u32) -> f64 {
    MIN_LATENCY_MS.max((tx_guard_us as f64 + LATENCY_HEADROOM_US as f64) / 1000.0)
}

/// Checks a receive or transmit latency (`what`) of `ms` against
/// [`min_latency_ms`] and [`MAX_LATENCY_MS`]. Configuration files,
/// controllers and the app all go through it, so that none of them can set
/// a latency the device would then refuse to start with.
pub fn check_latency_ms(what: &str, ms: f64, tx_guard_us: u32) -> Result<()> {
    let min = min_latency_ms(tx_guard_us);
    if (min..=MAX_LATENCY_MS).contains(&ms) {
        return Ok(());
    }
    Err(Error::Config(format!(
        "{what} must be between {min} and {MAX_LATENCY_MS} ms (at least tx_guard_us + \
         {LATENCY_HEADROOM_US} µs), not {ms}"
    )))
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
    fn latencies_must_clear_the_transmit_guard() {
        let check = |latency_ms: f64, tx_latency_ms: Option<f64>, tx_guard_us: u32| {
            DeviceConfig { latency_ms, tx_latency_ms, tx_guard_us, ..Default::default() }
                .validate()
                .map_err(|e| e.to_string())
        };
        // With the default 500 µs guard, 1 ms is the least either may be: a
        // packet leaves 0.5 ms after its timestamp, so 0.5 ms would be late
        // at every receiver.
        assert_eq!(check(1.0, Some(1.0), 500), Ok(()));
        for bad in [0.25, 0.5, 0.75, 0.99] {
            let e = check(bad, None, 500).unwrap_err();
            assert!(e.contains("latency_ms must be between 1 and 40 ms"), "{e}");
            let e = check(4.0, Some(bad), 500).unwrap_err();
            assert!(e.contains("tx_latency_ms must be between 1 and 40 ms"), "{e}");
        }
        for bad in [40.5, f64::NAN, f64::INFINITY] {
            assert!(check(bad, None, 500).is_err(), "{bad}");
            assert!(check(4.0, Some(bad), 500).is_err(), "{bad}");
        }
        // A longer guard raises the floor; a shorter one keeps 1 ms.
        assert!(check(1.0, None, 800).is_err());
        assert!(check(4.0, Some(1.2), 800).is_err());
        assert_eq!(check(1.3, Some(1.3), 800), Ok(()));
        assert!(check(0.75, None, 0).is_err());
        assert_eq!(min_latency_ms(0), 1.0);
        assert_eq!(min_latency_ms(500), 1.0);
        assert_eq!(min_latency_ms(2000), 2.5);
        // A guard that leaves no latency at all.
        assert_eq!(check(40.0, None, 39_500), Ok(()));
        let e = check(40.0, None, 39_501).unwrap_err();
        assert_eq!(e, "configuration: tx_guard_us must be at most 39500");
    }

    #[test]
    fn the_transmit_latency_defaults_to_the_receive_latency() {
        let c = DeviceConfig { latency_ms: 4.0, ..Default::default() };
        assert_eq!((c.tx_latency_ns(), c.tx_latency_samples()), (4_000_000, 192));
        let c = DeviceConfig { tx_latency_ms: Some(1.0), ..c };
        assert_eq!((c.tx_latency_ns(), c.tx_latency_samples()), (1_000_000, 48));
        assert_eq!((c.latency_ns(), c.latency_samples()), (4_000_000, 192));
        let c: DeviceConfig = toml::from_str("latency_ms = 2.0\ntx_latency_ms = 1.5").unwrap();
        c.validate().unwrap();
        assert_eq!((c.latency_ns(), c.tx_latency_ns()), (2_000_000, 1_500_000));
        let c: DeviceConfig = toml::from_str("latency_ms = 2.0").unwrap();
        assert_eq!(c.tx_latency_ms, None);
        assert_eq!(c.tx_latency_ns(), 2_000_000);
    }

    #[test]
    fn port_offsets() {
        assert_eq!(Ports::offset(2).flow_control, 4457);
    }
}
