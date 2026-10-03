//! What the daemon offers the driver: the engine's facts, the options of the
//! `[coreaudio]` section, and the driver configuration built from both
//! (design sections 5.4, 8.4 and 10.4).

use std::time::Duration;

use ovsc_core::{Device, DeviceConfig};
use ovsc_ipc::protocol::{
    self, CLOCK_ALGORITHM_RAW, DriverConfig, InputLatencyMode, MAX_NAME_BYTES, OffsetInputs,
    SERVICE_NAME, compute_offsets,
};
use ovsc_shm::layout::{IO_FRAMES_CAP, RING_FRAMES};
use ovsc_shm::time::ns_to_samples;

/// ovsc-core's `FPP_MAX`: the most frames per packet the device sends.
pub const FPP_MAX: u32 = 32;
/// ovsc-core's `FPP_LIMIT`: the most frames per packet of any flow the
/// device receives.
pub const RX_FPP_LIMIT: u32 = 256;
/// The user ID of Core Audio's driver helper (`_coreaudiod`).
pub const HELPER_UID: u32 = 202;
/// The device name offered while the configured one is empty.
const DEFAULT_DEVICE_NAME: &str = "OpenVirtualSoundcard";

/// The daemon's options for the driver, from the `[coreaudio]` section.
#[derive(Clone, Debug, PartialEq)]
pub struct HalOptions {
    /// The Mach service the driver connects to (for log messages; the
    /// transport is created by the caller).
    pub service_name: String,
    /// Effective user IDs allowed to connect; others are cancelled.
    pub allowed_uids: Vec<u32>,
    /// Added to the input safety offset (design section 8.4).
    pub input_margin_us: u32,
    /// Added to the output safety offset.
    pub output_margin_us: u32,
    /// Reported as the output latency instead of the receive latency.
    pub output_latency_ms: Option<f64>,
    pub latency_mode: InputLatencyMode,
    /// `CLOCK_ALGORITHM_RAW` or `CLOCK_ALGORITHM_SIMPLE_IIR`.
    pub clock_algorithm: u32,
    /// Keep the Mac from idle-sleeping while the engine runs.
    pub prevent_idle_sleep: bool,
    /// How often the status line is logged; zero disables it.
    pub status_log_interval: Duration,
    /// Artificial zero-timestamp jitter, for the tolerance sweep only.
    pub debug_zts_jitter_ns: u32,
}

impl Default for HalOptions {
    fn default() -> Self {
        Self {
            service_name: SERVICE_NAME.to_owned(),
            allowed_uids: vec![HELPER_UID],
            input_margin_us: 500,
            output_margin_us: 1000,
            output_latency_ms: None,
            latency_mode: InputLatencyMode::Safety,
            clock_algorithm: CLOCK_ALGORITHM_RAW,
            prevent_idle_sleep: true,
            status_log_interval: Duration::from_secs(30),
            debug_zts_jitter_ns: 0,
        }
    }
}

/// What the driver configuration depends on in the device engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineInfo {
    pub sample_rate: u32,
    /// The receive latency L, in samples.
    pub latency_samples: u32,
    /// How long after its timestamp a packet is sent, in samples.
    pub tx_guard_samples: u32,
    /// The most frames per packet the device sends.
    pub fpp_max: u32,
    /// Dante receive channel names: the driver's inputs.
    pub rx_names: Vec<String>,
    /// Dante transmit channel names: the driver's outputs.
    pub tx_names: Vec<String>,
    pub device_name: String,
}

impl EngineInfo {
    /// The facts of a running device, with its current names.
    pub fn from_device(d: &Device) -> Self {
        let info = d.info();
        let (rx_names, tx_names) = d.channel_names();
        Self {
            sample_rate: info.sample_rate,
            latency_samples: saturate(info.latency_samples),
            tx_guard_samples: saturate(ns_to_samples(info.tx_guard_ns, info.sample_rate)),
            fpp_max: FPP_MAX,
            rx_names,
            tx_names,
            device_name: d.name(),
        }
    }

    /// The facts a device started with `c` will have, before it runs. The
    /// names are the configured ones; a device restores renamed channels
    /// from its state file only when it starts.
    pub fn from_config(c: &DeviceConfig) -> Result<Self, ConfigError> {
        c.validate().map_err(|e| ConfigError::Device(e.to_string()))?;
        let device_name =
            if c.name.is_empty() { DEFAULT_DEVICE_NAME.to_owned() } else { c.name.clone() };
        Ok(Self {
            sample_rate: c.sample_rate,
            latency_samples: saturate(c.latency_samples()),
            tx_guard_samples: saturate(ns_to_samples(c.tx_guard_us as u64 * 1000, c.sample_rate)),
            fpp_max: FPP_MAX,
            rx_names: c.rx_channels.names(),
            tx_names: c.tx_channels.names(),
            device_name,
        })
    }
}

/// Why the engine cannot be offered to the driver.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The driver configuration breaks a bound of design section 10.4.
    #[error("driver configuration: {0}")]
    Driver(#[from] protocol::ConfigError),
    /// The device configuration is invalid.
    #[error("device configuration: {0}")]
    Device(String),
    /// `output_latency_ms` is negative or not a number.
    #[error("output_latency_ms must be a number of at least 0")]
    OutputLatency,
    /// A received sample could be overwritten before the driver reads it:
    /// the input safety offset, read delay, largest IO buffer and largest
    /// packet span `span` frames, which the rings cannot hold.
    #[error(
        "input safety offset and read delay too large: they span {span} frames with the largest \
         IO buffer and packet, rings hold {RING_FRAMES}"
    )]
    RingSpan { span: u64 },
}

/// The configuration the driver publishes for engine `e`, numbered
/// `config_gen`.
///
/// The offsets come from [`compute_offsets`]. Names are cut to the 31 bytes
/// the driver accepts, at a character boundary (Dante allows 31
/// characters). The result passes [`DriverConfig::validate`] and fits the
/// rings: see [`input_ring_span`].
pub fn driver_config(
    e: &EngineInfo,
    config_gen: u64,
    o: &HalOptions,
) -> Result<DriverConfig, ConfigError> {
    let fs = e.sample_rate;
    let output_latency_override = match o.output_latency_ms {
        None => None,
        Some(ms) if ms.is_finite() && ms >= 0.0 => {
            // Saturates; validate() rejects anything that large.
            Some((ms * fs as f64 / 1000.0).round() as u32)
        }
        Some(_) => return Err(ConfigError::OutputLatency),
    };
    let offsets = compute_offsets(&OffsetInputs {
        sample_rate: fs,
        latency_samples: e.latency_samples,
        tx_guard_samples: e.tx_guard_samples,
        fpp_max: e.fpp_max,
        input_margin_us: o.input_margin_us,
        output_margin_us: o.output_margin_us,
        output_latency_override,
        latency_mode: o.latency_mode,
    });
    let names = |names: &[String]| -> Vec<String> {
        names.iter().enumerate().map(|(i, n)| driver_name(n, || format!("{:02}", i + 1))).collect()
    };
    let cfg = DriverConfig {
        config_gen,
        sample_rate: fs,
        input_channels: u32::try_from(e.rx_names.len()).unwrap_or(u32::MAX),
        output_channels: u32::try_from(e.tx_names.len()).unwrap_or(u32::MAX),
        input_safety_offset: offsets.input_safety,
        output_safety_offset: offsets.output_safety,
        input_latency: offsets.input_latency,
        output_latency: offsets.output_latency,
        input_read_delay: offsets.input_read_delay,
        clock_algorithm: o.clock_algorithm,
        clock_domain: 0,
        input_names: names(&e.rx_names),
        output_names: names(&e.tx_names),
        device_name: driver_name(&e.device_name, || DEFAULT_DEVICE_NAME.to_owned()),
        debug_zts_jitter_ns: o.debug_zts_jitter_ns,
    };
    cfg.validate()?;
    let span = input_ring_span(&cfg);
    if span >= RING_FRAMES as u64 {
        return Err(ConfigError::RingSpan { span });
    }
    Ok(cfg)
}

/// How far apart, in frames, the newest sample the network may have
/// written and the oldest one ReadInput may still read can be: input
/// safety offset + largest IO buffer (`IO_FRAMES_CAP`) + read delay +
/// largest packet (design section 8.4). It must stay below `RING_FRAMES`,
/// or a ring slot is overwritten before it is read.
///
/// The transmit side has no such bound: near the largest IO buffers, stale
/// tags make the transmit thread send silence, never wrong audio.
pub fn input_ring_span(cfg: &DriverConfig) -> u64 {
    cfg.input_safety_offset as u64
        + IO_FRAMES_CAP as u64
        + cfg.input_read_delay as u64
        + RX_FPP_LIMIT as u64
}

/// `name` as the driver accepts it: NULs and line breaks become spaces, and
/// it is cut to `MAX_NAME_BYTES` at a character boundary. An empty name
/// becomes `fallback()`.
fn driver_name(name: &str, fallback: impl FnOnce() -> String) -> String {
    let mut s: String =
        name.chars().map(|c| if matches!(c, '\0' | '\n' | '\r') { ' ' } else { c }).collect();
    let mut end = s.len().min(MAX_NAME_BYTES);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    if s.is_empty() { fallback() } else { s }
}

fn saturate(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ovsc_core::Channels;
    use ovsc_ipc::protocol::{CLOCK_ALGORITHM_SIMPLE_IIR, SAMPLE_RATES};

    fn engine(rate: u32, latency_ms: f64) -> EngineInfo {
        let c = DeviceConfig {
            name: "studio".into(),
            sample_rate: rate,
            latency_ms,
            tx_channels: Channels::Count(8),
            rx_channels: Channels::Count(8),
            ..Default::default()
        };
        EngineInfo::from_config(&c).unwrap()
    }

    #[test]
    fn offsets_match_the_design_table() {
        // Design section 8.4, at 4 ms with the default margins:
        // (fs, L, input safety, output safety).
        let table = [
            (44_100, 176, 199, 54),
            (48_000, 192, 216, 55),
            (88_200, 353, 398, 89),
            (96_000, 384, 432, 96),
            (176_400, 706, 795, 177),
            (192_000, 768, 864, 192),
        ];
        for (fs, l, input_safety, output_safety) in table {
            let c = driver_config(&engine(fs, 4.0), 7, &HalOptions::default()).unwrap();
            assert_eq!(c.config_gen, 7);
            assert_eq!(c.sample_rate, fs);
            assert_eq!((c.input_channels, c.output_channels), (8, 8));
            assert_eq!(c.input_safety_offset, input_safety, "{fs}");
            assert_eq!(c.output_safety_offset, output_safety, "{fs}");
            assert_eq!((c.input_latency, c.input_read_delay), (0, 0));
            assert_eq!(c.output_latency, l, "{fs}");
            assert_eq!(c.clock_algorithm, CLOCK_ALGORITHM_RAW);
            assert_eq!(c.device_name, "studio");
            assert_eq!(c.input_names[7], "08");
        }
        assert_eq!(table.map(|t| t.0), SAMPLE_RATES);
    }

    #[test]
    fn options_shape_the_configuration() {
        let o = HalOptions {
            latency_mode: InputLatencyMode::Latency,
            output_latency_ms: Some(2.0),
            clock_algorithm: CLOCK_ALGORITHM_SIMPLE_IIR,
            debug_zts_jitter_ns: 30_000,
            ..Default::default()
        };
        let c = driver_config(&engine(48_000, 4.0), 1, &o).unwrap();
        assert_eq!((c.input_safety_offset, c.input_latency, c.input_read_delay), (24, 192, 192));
        assert_eq!(c.output_latency, 96);
        assert_eq!(c.clock_algorithm, CLOCK_ALGORITHM_SIMPLE_IIR);
        assert_eq!(c.debug_zts_jitter_ns, 30_000);
        for bad in [-1.0, f64::NAN, f64::INFINITY] {
            let o = HalOptions { output_latency_ms: Some(bad), ..Default::default() };
            assert_eq!(driver_config(&engine(48_000, 4.0), 1, &o), Err(ConfigError::OutputLatency));
        }
        // Too large for the driver: refused by validate().
        let o = HalOptions { output_latency_ms: Some(1000.0), ..Default::default() };
        assert!(matches!(driver_config(&engine(48_000, 4.0), 1, &o), Err(ConfigError::Driver(_))));
    }

    #[test]
    fn names_are_cut_to_what_the_driver_accepts() {
        let mut e = engine(48_000, 4.0);
        // 31 characters, 62 bytes.
        e.rx_names[0] = "ü".repeat(31);
        e.rx_names[1] = "a\nb".into();
        e.tx_names[2] = String::new();
        e.device_name = String::new();
        let c = driver_config(&e, 1, &HalOptions::default()).unwrap();
        assert_eq!(c.input_names[0], "ü".repeat(15));
        assert_eq!(c.input_names[1], "a b");
        assert_eq!(c.output_names[2], "03");
        assert_eq!(c.device_name, "OpenVirtualSoundcard");
    }

    #[test]
    fn engines_outside_the_driver_bounds_are_refused() {
        let mut e = engine(48_000, 4.0);
        e.rx_names = (0..129).map(|i| format!("{i}")).collect();
        assert!(matches!(
            driver_config(&e, 1, &HalOptions::default()),
            Err(ConfigError::Driver(protocol::ConfigError::Channels { .. }))
        ));
        e.rx_names.clear();
        assert!(driver_config(&e, 1, &HalOptions::default()).is_err());
        let bad = DeviceConfig { sample_rate: 22_050, ..Default::default() };
        assert!(matches!(EngineInfo::from_config(&bad), Err(ConfigError::Device(_))));
    }

    #[test]
    fn the_input_span_must_fit_the_rings() {
        // 40 ms at 192 kHz: L = 7680 frames, input safety 7776.
        let e = engine(192_000, 40.0);
        let c = driver_config(&e, 1, &HalOptions::default()).unwrap();
        assert_eq!(c.input_safety_offset, 7776);
        assert_eq!(input_ring_span(&c), 7776 + 16384 + 256);
        // A 44 ms input margin keeps the offset within the driver's bounds
        // (16128 <= 16384) but spans the whole ring.
        let wide = HalOptions { input_margin_us: 44_000, ..Default::default() };
        assert_eq!(driver_config(&e, 1, &wide), Err(ConfigError::RingSpan { span: 32768 }));
        assert!(driver_config(&engine(192_000, 4.0), 1, &wide).is_ok());
        // Latency mode moves L into the read delay: same span.
        let latency = HalOptions { latency_mode: InputLatencyMode::Latency, ..wide };
        assert_eq!(driver_config(&e, 1, &latency), Err(ConfigError::RingSpan { span: 32768 }));
    }
}
