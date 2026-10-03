//! The `ovsc run` configuration file.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use ovsc_core::DeviceConfig;
use ovsc_hal_server::config::HELPER_UID;
use ovsc_hal_server::{EngineInfo, HalOptions, driver_config};
use ovsc_ipc::protocol::{
    CLOCK_ALGORITHM_RAW, CLOCK_ALGORITHM_SIMPLE_IIR, InputLatencyMode, SERVICE_NAME,
};
use ovsc_shm::layout::{MAX_CHANNELS, RING_FRAMES};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    pub device: DeviceConfig,
    pub clock: ClockConfig,
    pub audio: AudioConfig,
    /// Options of the `coreaudio` backend.
    pub coreaudio: CoreAudioConfig,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClockSource {
    /// Follow the PTPv1 master of the Dante network (normal operation).
    #[default]
    Ptp,
    /// Read the host's wall clock once, then run on the local monotonic
    /// clock at `free_rate_ppm`. For a network without any PTP master.
    Free,
    /// Use the host's wall clock. Only for tests and single-host setups.
    System,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClockConfig {
    pub source: ClockSource,
    pub ptp_event_port: u16,
    pub ptp_general_port: u16,
    /// `free`: how much faster than the local clock media time runs, in ppm.
    pub free_rate_ppm: f64,
}

impl Default for ClockConfig {
    fn default() -> Self {
        Self {
            source: ClockSource::Ptp,
            ptp_event_port: 319,
            ptp_general_port: 320,
            free_rate_ppm: 0.0,
        }
    }
}

/// The largest frequency offset of a free-running clock, in ppm: the range
/// PTP followers correct (real oscillators stay within ±100 ppm).
pub const MAX_RATE_PPM: f64 = 500.0;

/// The rate of a free-running clock `ppm` parts per million fast (or slow,
/// if negative): media nanoseconds per local nanosecond.
pub fn rate_from_ppm(ppm: f64) -> anyhow::Result<f64> {
    if !(ppm.is_finite() && (-MAX_RATE_PPM..=MAX_RATE_PPM).contains(&ppm)) {
        bail!("the rate offset must be between -{MAX_RATE_PPM} and {MAX_RATE_PPM} ppm, not {ppm}");
    }
    Ok(1.0 + ppm * 1e-6)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    /// No audio I/O: the device only routes.
    #[default]
    None,
    /// Test tones on every transmit channel.
    Tone,
    /// Record all receive channels to a WAV file.
    Record,
    /// Send every receive channel back out on the transmit channel with the
    /// same number.
    Loopback,
    /// Bridge to a sound card or virtual audio cable.
    Soundcard,
    /// Serve the OpenVirtualSoundcard Core Audio device (macOS): its driver reads the
    /// receive channels and writes the transmit channels.
    CoreAudio,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    pub backend: BackendKind,
    /// `tone`: frequency of transmit channel 1; channel n plays n times it.
    pub tone_hz: u32,
    /// `tone`: level in dBFS.
    pub tone_level_db: f64,
    /// `record`: output file.
    pub record_path: PathBuf,
    /// `soundcard`: device that plays the receive channels.
    pub output_device: Option<String>,
    /// `soundcard`: device whose input feeds the transmit channels.
    pub input_device: Option<String>,
    /// `soundcard`: requested buffer size in frames.
    pub buffer_frames: Option<u32>,
    /// `soundcard`: for each output channel of the device, the receive
    /// channel (1-based) it plays, or 0 for silence. Default: 1, 2, 3, …
    pub output_channels: Option<Vec<u16>>,
    /// `soundcard`: for each input channel of the device, the transmit
    /// channel (1-based) it feeds, or 0 to ignore it. Default: 1, 2, 3, …
    pub input_channels: Option<Vec<u16>>,
    /// `soundcard`: extra safety margin against timing jitter, in ms.
    pub margin_ms: Option<f64>,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            backend: BackendKind::None,
            tone_hz: 440,
            tone_level_db: -18.0,
            record_path: PathBuf::from("ovsc-recording.wav"),
            output_device: None,
            input_device: None,
            buffer_frames: None,
            output_channels: None,
            input_channels: None,
            margin_ms: None,
        }
    }
}

/// Where the Core Audio device reports the receive latency on its inputs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LatencyMode {
    /// In the input safety offset: input timestamps are the senders'
    /// capture times.
    #[default]
    Safety,
    /// As input latency, which applications add to their own.
    Latency,
}

/// How Core Audio smooths the device's timestamps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClockAlgorithm {
    /// Use them as they are (they are already smooth).
    #[default]
    Raw,
    /// Core Audio's simple IIR filter.
    Iir,
}

/// The `[coreaudio]` section: options of the `coreaudio` backend.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CoreAudioConfig {
    /// The Mach service the driver connects to. launchd must hold it for
    /// the daemon (the LaunchDaemon plist lists it under MachServices).
    pub service: String,
    /// Effective user IDs allowed to connect; 202 is Core Audio's driver
    /// helper.
    pub allowed_uids: Vec<u32>,
    /// Extra input safety offset against late packets, in µs.
    pub input_margin_us: u32,
    /// Extra output safety offset against a late transmit thread, in µs.
    pub output_margin_us: u32,
    /// Output latency reported to applications, in ms. Default: the
    /// receive latency.
    pub output_latency_ms: Option<f64>,
    pub input_latency_mode: LatencyMode,
    pub clock_algorithm: ClockAlgorithm,
    /// Keep the Mac from idle-sleeping while the engine runs.
    pub prevent_idle_sleep: bool,
    /// Seconds between status lines in the log; 0 disables them.
    pub status_log_interval_s: u64,
    /// Artificial jitter of the driver's timestamps, in ns. Only for
    /// measuring Core Audio's tolerance; keep it at 0.
    pub debug_zts_jitter_ns: u32,
    /// The control socket of the OpenVirtualSoundcard app; empty for none.
    pub control_socket: String,
}

impl Default for CoreAudioConfig {
    fn default() -> Self {
        Self {
            service: SERVICE_NAME.to_owned(),
            allowed_uids: vec![HELPER_UID],
            input_margin_us: 500,
            output_margin_us: 1000,
            output_latency_ms: None,
            input_latency_mode: LatencyMode::Safety,
            clock_algorithm: ClockAlgorithm::Raw,
            prevent_idle_sleep: true,
            status_log_interval_s: 30,
            debug_zts_jitter_ns: 0,
            control_socket: ovsc_control::SOCKET_PATH.to_owned(),
        }
    }
}

/// The longest `status_log_interval_s`: a day.
const MAX_STATUS_LOG_INTERVAL_S: u64 = 86_400;

impl CoreAudioConfig {
    /// The driver service's options.
    pub fn hal_options(&self) -> HalOptions {
        HalOptions {
            service_name: self.service.clone(),
            allowed_uids: self.allowed_uids.clone(),
            input_margin_us: self.input_margin_us,
            output_margin_us: self.output_margin_us,
            output_latency_ms: self.output_latency_ms,
            latency_mode: match self.input_latency_mode {
                LatencyMode::Safety => InputLatencyMode::Safety,
                LatencyMode::Latency => InputLatencyMode::Latency,
            },
            clock_algorithm: match self.clock_algorithm {
                ClockAlgorithm::Raw => CLOCK_ALGORITHM_RAW,
                ClockAlgorithm::Iir => CLOCK_ALGORITHM_SIMPLE_IIR,
            },
            prevent_idle_sleep: self.prevent_idle_sleep,
            status_log_interval: Duration::from_secs(self.status_log_interval_s),
            debug_zts_jitter_ns: self.debug_zts_jitter_ns,
        }
    }
}

/// Converts a 1-based channel list (0 = none) to the bridge's map.
pub fn channel_map(list: &Option<Vec<u16>>) -> Option<Vec<Option<usize>>> {
    list.as_ref().map(|l| l.iter().map(|&c| (c as usize).checked_sub(1)).collect())
}

impl AppConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Checks the configuration before `ovsc run` uses it on this host.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.validate_for(cfg!(target_os = "macos"))
    }

    /// Checks the configuration for a Mac (`macos`) or another host.
    pub fn validate_for(&self, macos: bool) -> anyhow::Result<()> {
        self.device.validate()?;
        rate_from_ppm(self.clock.free_rate_ppm).context("clock.free_rate_ppm")?;
        match self.audio.backend {
            BackendKind::Soundcard
                if self.audio.output_device.is_none() && self.audio.input_device.is_none() =>
            {
                bail!(
                    "the soundcard backend needs output_device and/or input_device (see `ovsc soundcards`)"
                )
            }
            BackendKind::CoreAudio => self.validate_coreaudio(macos),
            _ => Ok(()),
        }
    }

    /// The `coreaudio` backend's own rules: the driver's channel bounds and
    /// ring size, a clock it can mirror, and options it accepts.
    fn validate_coreaudio(&self, macos: bool) -> anyhow::Result<()> {
        if !macos {
            bail!("the coreaudio backend needs macOS");
        }
        let d = &self.device;
        for (what, list) in [("rx", &d.rx_channels), ("tx", &d.tx_channels)] {
            let n = list.names().len();
            if !(1..=MAX_CHANNELS).contains(&n) {
                bail!("the coreaudio backend needs 1 to {MAX_CHANNELS} {what} channels, not {n}");
            }
        }
        if d.ring_capacity != RING_FRAMES {
            bail!(
                "the coreaudio backend needs device.ring_capacity = {RING_FRAMES} (the driver's \
                 ring size), not {}",
                d.ring_capacity
            );
        }
        if self.clock.source == ClockSource::System {
            bail!(
                "the coreaudio backend needs clock source \"ptp\" or \"free\": the driver cannot \
                 follow the system clock"
            );
        }
        let c = &self.coreaudio;
        if c.service.is_empty() {
            bail!("coreaudio.service must not be empty");
        }
        if c.allowed_uids.is_empty() {
            bail!("coreaudio.allowed_uids must not be empty: the driver could never connect");
        }
        if c.status_log_interval_s > MAX_STATUS_LOG_INTERVAL_S {
            bail!("coreaudio.status_log_interval_s must be at most {MAX_STATUS_LOG_INTERVAL_S}");
        }
        // The configuration the driver will get at first, with the bounds
        // it applies (offsets, names, ring span).
        let engine = EngineInfo::from_config(d)?;
        driver_config(&engine, 1, &c.hal_options())
            .context("the Core Audio driver cannot take this configuration")?;
        Ok(())
    }
}

/// The macOS configuration installed with the driver, printed by
/// `ovsc example-config --macos`.
pub const EXAMPLE_MACOS: &str = include_str!("../../../packaging/macos/ovsc.toml.default");

/// Annotated example configuration, printed by `ovsc example-config`.
pub const EXAMPLE: &str = r#"# OpenVirtualSoundcard configuration.

[device]
# Name shown in Dante Controller: 1-31 of A-Z a-z 0-9 and inner hyphens.
name = "ovsc"
# Network interface name or IPv4 address. Empty: interface of the default route.
interface = ""
sample_rate = 48000          # 44100, 48000, 88200, 96000, 176400 or 192000
bits_per_sample = 24         # 16, 24 or 32
tx_channels = 8              # a count, or a list of names: ["Left", "Right"]
rx_channels = 8
latency_ms = 4.0             # receive latency
# Remember renames and subscriptions made from a controller.
state_file = "ovsc-state.toml"

# Subscriptions to set up at start (controllers can change them later).
# [[device.subscriptions]]
# rx_channel = 1
# tx_channel = "01"
# tx_device = "stagebox"

[clock]
# "ptp" (Dante network), "free" (this host's clock, for a network without a
# PTP master) or "system" (tests only).
source = "ptp"
# free_rate_ppm = 0.0        # "free": rate offset, -500 to 500 ppm

[audio]
# none, tone, record, loopback, soundcard, or coreaudio (macOS; see
# `ovsc example-config --macos`).
backend = "none"
# tone_hz = 440
# record_path = "recording.wav"
# output_device = "BlackHole 16ch"
# input_device = "BlackHole 16ch"
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use ovsc_core::Channels;

    #[test]
    fn example_parses() {
        let cfg: AppConfig = toml::from_str(EXAMPLE).unwrap();
        cfg.device.validate().unwrap();
        assert_eq!(cfg.device.name, "ovsc");
        assert_eq!(cfg.clock.source, ClockSource::Ptp);
        assert_eq!(cfg.audio.backend, BackendKind::None);
    }

    #[test]
    fn channel_maps_are_one_based() {
        assert_eq!(channel_map(&Some(vec![0, 1, 3])), Some(vec![None, Some(0), Some(2)]));
        assert_eq!(channel_map(&None), None);
    }

    /// The macOS end-to-end test's configurations (ci/macos) are valid on a
    /// Mac, and their device keeps off Dante's ports so that the test can run
    /// beside Dante software.
    #[test]
    fn e2e_configs_are_valid() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ci/macos");
        for name in ["e2e.toml", "e2e-4ch.toml", "e2e-96k.toml"] {
            let cfg = AppConfig::load(&dir.join(name)).unwrap();
            cfg.validate_for(true).unwrap_or_else(|e| panic!("{name}: {e:#}"));
            assert_eq!(cfg.audio.backend, BackendKind::CoreAudio, "{name}");
            assert_eq!(cfg.device.ports, ovsc_core::Ports::offset(20_000), "{name}");
        }
    }

    /// The installed default stays out of the way of Audinate's ConMon
    /// service, which holds 8800 and 8700 on every interface.
    #[test]
    fn macos_default_moves_cmc_and_settings() {
        let cfg: AppConfig = toml::from_str(EXAMPLE_MACOS).unwrap();
        cfg.validate_for(true).unwrap();
        let ports = cfg.device.ports;
        assert_eq!((ports.arc, ports.flow_control), (4440, 4455));
        assert_eq!((ports.cmc, ports.settings), (38800, 38700));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<AppConfig>("[audio]\nbackend = \"tone\"\ntypo = 1").is_err());
        assert!(toml::from_str::<AppConfig>("[coreaudio]\nstatus_log_interval = 5").is_err());
    }

    /// A configuration of the coreaudio backend that is valid on a Mac.
    fn coreaudio(toml: &str) -> AppConfig {
        let cfg: AppConfig =
            toml::from_str(&format!("[audio]\nbackend = \"coreaudio\"\n{toml}")).unwrap();
        assert_eq!(cfg.audio.backend, BackendKind::CoreAudio);
        cfg
    }

    fn error(cfg: &AppConfig) -> String {
        format!("{:#}", cfg.validate_for(true).unwrap_err())
    }

    #[test]
    fn the_coreaudio_section_parses() {
        let cfg = coreaudio(
            r#"
            [coreaudio]
            service = "org.example.audio"
            allowed_uids = [202, 0]
            input_margin_us = 700
            output_margin_us = 1500
            output_latency_ms = 2.0
            input_latency_mode = "latency"
            clock_algorithm = "iir"
            prevent_idle_sleep = false
            status_log_interval_s = 5
            debug_zts_jitter_ns = 30000
            "#,
        );
        cfg.validate_for(true).unwrap();
        let c = &cfg.coreaudio;
        assert_eq!(c.service, "org.example.audio");
        assert_eq!(c.input_latency_mode, LatencyMode::Latency);
        assert_eq!(c.clock_algorithm, ClockAlgorithm::Iir);
        assert_eq!(
            c.hal_options(),
            HalOptions {
                service_name: "org.example.audio".into(),
                allowed_uids: vec![202, 0],
                input_margin_us: 700,
                output_margin_us: 1500,
                output_latency_ms: Some(2.0),
                latency_mode: InputLatencyMode::Latency,
                clock_algorithm: CLOCK_ALGORITHM_SIMPLE_IIR,
                prevent_idle_sleep: false,
                status_log_interval: Duration::from_secs(5),
                debug_zts_jitter_ns: 30_000,
            }
        );
        for (mode, algorithm) in [("\"safety\"", "\"raw\""), ("\"latency\"", "\"iir\"")] {
            let text = format!("input_latency_mode = {mode}\nclock_algorithm = {algorithm}");
            assert!(toml::from_str::<CoreAudioConfig>(&text).is_ok());
        }
        assert!(toml::from_str::<CoreAudioConfig>("clock_algorithm = \"iirf\"").is_err());
    }

    #[test]
    fn coreaudio_defaults_are_the_services() {
        let cfg = coreaudio("");
        cfg.validate_for(true).unwrap();
        assert_eq!(cfg.coreaudio, CoreAudioConfig::default());
        assert_eq!(cfg.coreaudio.hal_options(), HalOptions::default());
        assert_eq!(cfg.coreaudio.allowed_uids, [202]);
        assert_eq!(cfg.coreaudio.service, "org.openvirtualsoundcard.audio");
    }

    #[test]
    fn the_coreaudio_backend_needs_macos() {
        let cfg = coreaudio("");
        assert_eq!(
            cfg.validate_for(false).unwrap_err().to_string(),
            "the coreaudio backend needs macOS"
        );
        // Other backends run anywhere.
        AppConfig::default().validate_for(false).unwrap();
    }

    #[test]
    fn coreaudio_channel_counts_are_bounded() {
        let mut cfg = coreaudio("");
        cfg.device.rx_channels = Channels::Count(128);
        cfg.device.tx_channels = Channels::Count(128);
        cfg.validate_for(true).unwrap();
        cfg.device.tx_channels = Channels::Count(129);
        assert_eq!(error(&cfg), "the coreaudio backend needs 1 to 128 tx channels, not 129");
        cfg.device.tx_channels = Channels::Count(2);
        cfg.device.rx_channels = Channels::Count(0);
        assert_eq!(error(&cfg), "the coreaudio backend needs 1 to 128 rx channels, not 0");
        // Other backends take up to 256.
        cfg.device.rx_channels = Channels::Count(129);
        cfg.audio.backend = BackendKind::Loopback;
        cfg.validate_for(true).unwrap();
    }

    #[test]
    fn coreaudio_rings_must_be_the_drivers() {
        let mut cfg = coreaudio("");
        cfg.device.ring_capacity = RING_FRAMES / 2;
        assert_eq!(
            error(&cfg),
            "the coreaudio backend needs device.ring_capacity = 32768 (the driver's ring size), \
             not 16384"
        );
    }

    #[test]
    fn coreaudio_needs_a_clock_it_can_mirror() {
        let mut cfg = coreaudio("[clock]\nsource = \"system\"");
        assert!(error(&cfg).contains("needs clock source \"ptp\" or \"free\""));
        cfg.clock.source = ClockSource::Free;
        cfg.validate_for(true).unwrap();
        cfg.clock.source = ClockSource::Ptp;
        cfg.validate_for(true).unwrap();
    }

    #[test]
    fn coreaudio_options_are_checked() {
        let bad = |toml: &str| error(&coreaudio(&format!("[coreaudio]\n{toml}")));
        assert!(bad("service = \"\"").contains("coreaudio.service"));
        assert!(bad("allowed_uids = []").contains("coreaudio.allowed_uids"));
        assert!(bad("status_log_interval_s = 86401").contains("status_log_interval_s"));
        assert!(bad("output_latency_ms = -1.0").contains("output_latency_ms"));
        // Offsets the driver cannot take: 44 ms of margin spans its rings.
        let wide = coreaudio(
            "[device]\nsample_rate = 192000\nlatency_ms = 40.0\n[coreaudio]\ninput_margin_us = 44000",
        );
        assert!(error(&wide).starts_with("the Core Audio driver cannot take this configuration"));
    }

    #[test]
    fn free_clocks_have_bounded_rates() {
        let cfg: AppConfig =
            toml::from_str("[clock]\nsource = \"free\"\nfree_rate_ppm = -50.0").unwrap();
        assert_eq!(cfg.clock.source, ClockSource::Free);
        cfg.validate_for(false).unwrap();
        assert_eq!(rate_from_ppm(-50.0).unwrap(), 1.0 - 50e-6);
        assert_eq!(rate_from_ppm(0.0).unwrap(), 1.0);
        for bad in [500.5, -501.0, f64::NAN, f64::INFINITY] {
            assert!(rate_from_ppm(bad).is_err(), "{bad}");
        }
        let cfg: AppConfig = toml::from_str("[clock]\nfree_rate_ppm = 1000.0").unwrap();
        assert!(format!("{:#}", cfg.validate_for(false).unwrap_err()).contains("free_rate_ppm"));
    }

    #[test]
    fn the_macos_example_parses_and_validates() {
        let cfg: AppConfig = toml::from_str(EXAMPLE_MACOS).unwrap();
        cfg.validate_for(true).unwrap();
        assert_eq!(cfg.audio.backend, BackendKind::CoreAudio);
        assert_eq!(cfg.clock.source, ClockSource::Ptp);
        assert_eq!(cfg.coreaudio, CoreAudioConfig::default());
        assert_eq!(
            (cfg.device.rx_channels.names().len(), cfg.device.tx_channels.names().len()),
            (8, 8)
        );
        // launchd starts the daemon in /, so the state file needs a full path.
        // (`has_root`, not `is_absolute`: on Windows, which runs this test
        // too, a full path also needs a drive.)
        assert!(cfg.device.state_file.unwrap().has_root());
        // Every option the example mentions in a comment exists.
        let section = EXAMPLE_MACOS.split("[coreaudio]").nth(1).unwrap();
        let options: String = section
            .lines()
            .filter_map(|l| l.strip_prefix("# "))
            .filter(|l| l.contains(" = "))
            .map(|l| format!("{l}\n"))
            .collect();
        let commented: CoreAudioConfig = toml::from_str(&options).unwrap();
        assert_eq!(commented.output_latency_ms, Some(4.0));
        assert_eq!(
            CoreAudioConfig { output_latency_ms: None, ..commented },
            CoreAudioConfig::default()
        );
    }
}
