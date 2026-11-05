//! Immutable facts about a running device.

use ovsc_proto::DeviceId;
use ovsc_proto::arc::ChannelFormat;
use ovsc_proto::audio::SampleFormat;

use crate::config::{DeviceConfig, Ports, parse_device_id};
use crate::net::Interface;
use crate::state::RxLatency;
use crate::{Error, Result};

/// Manufacturer string shown by controllers.
pub const MANUFACTURER: &str = "OpenVirtualSoundcard";
/// Board name shown by controllers.
pub const BOARD_NAME: &str = "OpenVirtualSoundcard";
/// Model name shown by controllers.
pub const MODEL_NAME: &str = "Open Virtual Soundcard";
/// Model id advertised in mDNS (`model=`). Value known to be accepted by
/// Dante Controller.
pub const MODEL_ID: &str = "_000000000000000b";
/// Firmware revision string returned by ARC device-info queries.
pub const REVISION: &str = ":705";
/// Largest number of transmit flows we accept.
pub const MAX_TX_FLOWS: usize = 32;
/// Largest number of receive flows we create.
pub const MAX_RX_FLOWS: usize = 32;
/// Channels per transmit flow we advertise.
pub const MAX_CHANNELS_PER_TX_FLOW: u16 = 8;
/// Frames-per-packet range we advertise for transmit flows, and accept in
/// flow requests.
pub const FPP_MAX: u16 = 32;
pub const FPP_MIN: u16 = 2;
/// The most frames a received packet may carry: the frame count is read
/// from a packet's length (PROTOCOL.md §8.2), and the macOS driver's ring
/// span assumes no more (`ovsc_hal_server::config::RX_FPP_LIMIT`).
pub const RX_FPP_LIMIT: usize = 256;
/// PCM type of current Dante devices.
pub const PCM_TYPE: u8 = 0x0e;

/// Everything about the device that doesn't change while it runs, and the
/// receive latency, which does.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub iface: Interface,
    pub device_id: DeviceId,
    pub process_id: u16,
    /// Name used when the user hasn't chosen one.
    pub factory_name: String,
    pub sample_rate: u32,
    pub format: SampleFormat,
    /// Receive latency: received audio is played this long after its
    /// timestamp. The configuration's (with a saved one applied, see
    /// [`DeviceConfig::with_saved`]) at start; while the device runs,
    /// [`crate::Device::restart_rx`] changes it, for the receive side only.
    /// Clones of the info share it.
    pub latency: RxLatency,
    /// Transmit latency: the least receive latency asked of the receivers
    /// of our transmit channels (TXT `latency_ns`). Fixed while the device
    /// runs: a new receive latency leaves the transmit side alone.
    pub tx_latency_ns: u32,
    pub tx_latency_samples: u64,
    pub tx_guard_ns: u64,
    pub ports: Ports,
    /// Factory (default) channel names.
    pub tx_channels: Vec<String>,
    pub rx_channels: Vec<String>,
    pub max_channels_per_rx_flow: u16,
    pub discovery: bool,
    pub version: [u8; 4],
}

impl DeviceInfo {
    pub fn new(config: &DeviceConfig, iface: Interface) -> Result<Self> {
        config.validate()?;
        let device_id = match &config.device_id {
            Some(id) => parse_device_id(id)?,
            None => {
                // Derived from the IP address rather than the MAC so that it
                // can't collide with Dante Virtual Soundcard on the same host.
                let mut id = [0u8; 8];
                id[2..6].copy_from_slice(&iface.ip.octets());
                id[6..8].copy_from_slice(&config.process_id.to_be_bytes());
                id
            }
        };
        // The name the device has until someone names it, and its mDNS host
        // name. It must not change with the IP address, as the device id
        // does: controllers' subscriptions to the device go by name. So, as
        // Dante hardware does, it ends in the last three bytes of the
        // interface's MAC address, which tell computers apart, and the
        // process id when there are several devices on one computer. Without
        // a MAC address (loopback), the device id stands in.
        let factory_name = if iface.mac != [0; 6] {
            match config.process_id {
                0 => format!("ovsc-{}", hex::encode(&iface.mac[3..])),
                p => format!("ovsc-{}-{p}", hex::encode(&iface.mac[3..])),
            }
        } else {
            format!("ovsc-{}", hex::encode(&device_id[2..]))
        };
        let format = SampleFormat::from_bits(config.bits_per_sample as u32)
            .ok_or_else(|| Error::Config("bits_per_sample".into()))?;
        let version = [
            env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap_or(0),
            env!("CARGO_PKG_VERSION_MINOR").parse().unwrap_or(0),
            0,
            env!("CARGO_PKG_VERSION_PATCH").parse().unwrap_or(0),
        ];
        Ok(Self {
            iface,
            device_id,
            process_id: config.process_id,
            factory_name,
            sample_rate: config.sample_rate,
            format,
            latency: RxLatency::new(config.latency_ns(), config.latency_samples()),
            tx_latency_ns: config.tx_latency_ns(),
            tx_latency_samples: config.tx_latency_samples(),
            tx_guard_ns: config.tx_guard_us as u64 * 1000,
            ports: config.ports,
            tx_channels: config.tx_channels.names(),
            rx_channels: config.rx_channels.names(),
            max_channels_per_rx_flow: config.max_channels_per_rx_flow,
            discovery: config.discovery,
            version,
        })
    }

    pub fn channel_format(&self) -> ChannelFormat {
        ChannelFormat {
            sample_rate: self.sample_rate,
            bits_per_sample: self.format.bits(),
            pcm_type: PCM_TYPE as u16,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;
    use crate::config::LATENCY_HEADROOM_US;
    use crate::rx::own_channel_txt;

    fn info(config: DeviceConfig) -> Result<DeviceInfo> {
        let iface = Interface {
            name: "lo".into(),
            ip: Ipv4Addr::LOCALHOST,
            netmask: Ipv4Addr::new(255, 0, 0, 0),
            gateway: Ipv4Addr::UNSPECIFIED,
            mac: [0; 6],
            link_speed_mbps: 1000,
        };
        DeviceInfo::new(&config, iface)
    }

    #[test]
    fn transmit_channels_ask_for_the_transmit_latency() {
        // Receive at 4 ms, ask receivers of our channels for only 1 ms.
        let i = info(DeviceConfig { tx_latency_ms: Some(1.0), ..Default::default() }).unwrap();
        assert_eq!(i.latency.get(), (4_000_000, 192));
        assert_eq!((i.tx_latency_ns, i.tx_latency_samples), (1_000_000, 48));
        let txt = own_channel_txt(&i, 1);
        assert_eq!(txt.latency_ns, 1_000_000);
        // Enough for packets that leave the guard after their timestamp.
        assert!(u64::from(txt.latency_ns) >= i.tx_guard_ns + u64::from(LATENCY_HEADROOM_US) * 1000);
        // Unset, the transmit latency is the receive latency, as before.
        let i = info(DeviceConfig { latency_ms: 2.0, ..Default::default() }).unwrap();
        assert_eq!(own_channel_txt(&i, 2).latency_ns, 2_000_000);
        // A device does not start with a latency its transmitter cannot meet.
        assert!(info(DeviceConfig { tx_latency_ms: Some(0.5), ..Default::default() }).is_err());
        assert!(info(DeviceConfig { latency_ms: 0.5, ..Default::default() }).is_err());
    }
}
