//! Immutable facts about a running device.

use ovsc_proto::DeviceId;
use ovsc_proto::arc::ChannelFormat;
use ovsc_proto::audio::SampleFormat;

use crate::config::{DeviceConfig, Ports, parse_device_id};
use crate::net::Interface;
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
/// Frames-per-packet range we advertise for transmit flows.
pub const FPP_MAX: u16 = 32;
pub const FPP_MIN: u16 = 2;
/// Largest frames-per-packet value we accept in flow requests.
pub const FPP_LIMIT: u16 = 256;
/// PCM type of current Dante devices.
pub const PCM_TYPE: u8 = 0x0e;

/// Everything about the device that doesn't change while it runs.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub iface: Interface,
    pub device_id: DeviceId,
    pub process_id: u16,
    /// Name used when the user hasn't chosen one.
    pub factory_name: String,
    pub sample_rate: u32,
    pub format: SampleFormat,
    pub latency_ns: u32,
    pub latency_samples: u64,
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
        let factory_name =
            format!("ovsc-{}", hex::encode(&device_id[2..])).chars().take(31).collect();
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
            latency_ns: config.latency_ns(),
            latency_samples: config.latency_samples(),
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
