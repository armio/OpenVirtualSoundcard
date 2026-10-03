//! What the daemon and the driver say to each other (design section 10),
//! the driver's configuration with its validation and its text form for
//! host storage, and the latency and safety offsets (section 8.4).
//!
//! Messages travel as [`Kv`] maps: string keys, typed values. Each transport
//! converts a map to its own format (an XPC dictionary, or nothing at all in
//! the in-memory transport), so the codec here is the only place that knows
//! the keys. Unknown keys and ops are ignored, so a minor version can add
//! keys; a missing or mistyped required key is a [`ProtoError`].

use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write as _;

use ovsc_shm::layout::{IO_FRAMES_CAP, MAX_CHANNELS};

use crate::region::RegionHandle;

/// The daemon's Mach service, listed in the driver's
/// `AudioServerPlugIn_MachServices`.
pub const SERVICE_NAME: &str = "org.openvirtualsoundcard.audio";
pub const PROTO_MAJOR: u32 = 1;
pub const PROTO_MINOR: u32 = 0;
/// The host-storage key of the driver's last configuration.
pub const STORAGE_KEY: &str = "org.openvirtualsoundcard.config.v1";
pub const DEVICE_UID: &str = "org.openvirtualsoundcard.vsc";
pub const MODEL_UID: &str = "org.openvirtualsoundcard.vsc.model";

/// `kAudioDeviceClockAlgorithmRaw` (`'raww'`).
pub const CLOCK_ALGORITHM_RAW: u32 = u32::from_be_bytes(*b"raww");
/// `kAudioDeviceClockAlgorithmSimpleIIR` (`'iirf'`).
pub const CLOCK_ALGORITHM_SIMPLE_IIR: u32 = u32::from_be_bytes(*b"iirf");

/// The sample rates a configuration may use.
pub const SAMPLE_RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];
/// The largest safety offset, latency or read delay, in frames.
pub const MAX_OFFSET_FRAMES: u32 = IO_FRAMES_CAP as u32;
/// The longest channel or device name, in bytes.
pub const MAX_NAME_BYTES: usize = 31;

// --- Configuration -------------------------------------------------------

/// What the daemon tells the driver to publish.
///
/// Every frame count is in device frames at `sample_rate`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriverConfig {
    /// Incremented by the daemon on every change.
    pub config_gen: u64,
    pub sample_rate: u32,
    pub input_channels: u32,
    pub output_channels: u32,
    pub input_safety_offset: u32,
    pub output_safety_offset: u32,
    pub input_latency: u32,
    pub output_latency: u32,
    /// Frames ReadInput reads behind the input time ("latency" mode).
    pub input_read_delay: u32,
    /// [`CLOCK_ALGORITHM_RAW`] or [`CLOCK_ALGORITHM_SIMPLE_IIR`].
    pub clock_algorithm: u32,
    pub clock_domain: u32,
    /// Dante RX channel names, one per input channel.
    pub input_names: Vec<String>,
    /// Dante TX channel names, one per output channel.
    pub output_names: Vec<String>,
    /// The Dante device name.
    pub device_name: String,
    /// Artificial jitter on zero timestamps, for the tolerance sweep only.
    pub debug_zts_jitter_ns: u32,
}

/// Why a configuration is unusable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// Not one of [`SAMPLE_RATES`].
    SampleRate(u32),
    /// A channel count outside 1..=128.
    Channels { field: &'static str, count: u32 },
    /// A safety offset, latency or read delay above [`MAX_OFFSET_FRAMES`].
    Offset { field: &'static str, value: u32 },
    /// A name list whose length differs from its channel count.
    NameCount { field: &'static str, expected: u32, got: usize },
    /// A name that is empty, longer than [`MAX_NAME_BYTES`], or holds a NUL
    /// or a line break. `index` counts from 0 (always 0 for `device_name`).
    Name { field: &'static str, index: usize, reason: &'static str },
    /// Neither [`CLOCK_ALGORITHM_RAW`] nor [`CLOCK_ALGORITHM_SIMPLE_IIR`].
    ClockAlgorithm(u32),
    /// Stored text that does not parse.
    Storage(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::SampleRate(r) => write!(f, "unsupported sample rate {r}"),
            ConfigError::Channels { field, count } => {
                write!(f, "{field} is {count}, must be 1 to {MAX_CHANNELS}")
            }
            ConfigError::Offset { field, value } => {
                write!(f, "{field} is {value} frames, at most {MAX_OFFSET_FRAMES} allowed")
            }
            ConfigError::NameCount { field, expected, got } => {
                write!(f, "{field} has {got} names for {expected} channels")
            }
            ConfigError::Name { field, index, reason } => write!(f, "{field}[{index}] {reason}"),
            ConfigError::ClockAlgorithm(a) => write!(f, "unknown clock algorithm {a:#010x}"),
            ConfigError::Storage(m) => write!(f, "stored configuration: {m}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl DriverConfig {
    /// Checks every bound of design section 10.4. Both sides call it: the
    /// daemon before sending, the driver before publishing.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !SAMPLE_RATES.contains(&self.sample_rate) {
            return Err(ConfigError::SampleRate(self.sample_rate));
        }
        for (field, count) in
            [("input_channels", self.input_channels), ("output_channels", self.output_channels)]
        {
            if count == 0 || count as usize > MAX_CHANNELS {
                return Err(ConfigError::Channels { field, count });
            }
        }
        for (field, value) in [
            ("input_safety_offset", self.input_safety_offset),
            ("output_safety_offset", self.output_safety_offset),
            ("input_latency", self.input_latency),
            ("output_latency", self.output_latency),
            ("input_read_delay", self.input_read_delay),
        ] {
            if value > MAX_OFFSET_FRAMES {
                return Err(ConfigError::Offset { field, value });
            }
        }
        check_names("input_names", &self.input_names, self.input_channels)?;
        check_names("output_names", &self.output_names, self.output_channels)?;
        check_name("device_name", 0, &self.device_name)?;
        if self.clock_algorithm != CLOCK_ALGORITHM_RAW
            && self.clock_algorithm != CLOCK_ALGORITHM_SIMPLE_IIR
        {
            return Err(ConfigError::ClockAlgorithm(self.clock_algorithm));
        }
        Ok(())
    }

    /// Whether `o` publishes the same device structure: every field except
    /// the names, the device name and `config_gen`. A difference here needs
    /// a HAL configuration change; names alone do not.
    pub fn structural_eq(&self, o: &Self) -> bool {
        self.sample_rate == o.sample_rate
            && self.input_channels == o.input_channels
            && self.output_channels == o.output_channels
            && self.input_safety_offset == o.input_safety_offset
            && self.output_safety_offset == o.output_safety_offset
            && self.input_latency == o.input_latency
            && self.output_latency == o.output_latency
            && self.input_read_delay == o.input_read_delay
            && self.clock_algorithm == o.clock_algorithm
            && self.clock_domain == o.clock_domain
            && self.debug_zts_jitter_ns == o.debug_zts_jitter_ns
    }

    /// The text kept in host storage under [`STORAGE_KEY`]: one `key=value`
    /// line per field. Names are percent-escaped (everything outside
    /// printable ASCII, and `%`, `,` and `=`), and name lists are
    /// comma-separated, so the text is plain ASCII.
    pub fn to_storage_string(&self) -> String {
        let mut s = String::new();
        // Writing to a String cannot fail.
        let _ = write!(
            s,
            "config_gen={}\nsample_rate={}\ninput_channels={}\noutput_channels={}\n\
             input_safety_offset={}\noutput_safety_offset={}\ninput_latency={}\n\
             output_latency={}\ninput_read_delay={}\nclock_algorithm={}\nclock_domain={}\n\
             debug_zts_jitter_ns={}\n",
            self.config_gen,
            self.sample_rate,
            self.input_channels,
            self.output_channels,
            self.input_safety_offset,
            self.output_safety_offset,
            self.input_latency,
            self.output_latency,
            self.input_read_delay,
            self.clock_algorithm,
            self.clock_domain,
            self.debug_zts_jitter_ns,
        );
        s.push_str("device_name=");
        escape_into(&mut s, &self.device_name);
        for (key, names) in
            [("input_names", &self.input_names), ("output_names", &self.output_names)]
        {
            s.push('\n');
            s.push_str(key);
            s.push('=');
            for (i, name) in names.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                escape_into(&mut s, name);
            }
        }
        s.push('\n');
        s
    }

    /// Parses [`DriverConfig::to_storage_string`] output and validates the
    /// result. Unknown keys and empty lines are ignored; a missing or
    /// repeated key, a bad number or a bad escape is an error.
    pub fn from_storage_string(s: &str) -> Result<Self, ConfigError> {
        let mut fields: BTreeMap<&str, &str> = BTreeMap::new();
        for (n, line) in s.lines().enumerate() {
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(ConfigError::Storage(format!("line {} has no '='", n + 1)));
            };
            if fields.insert(key, value).is_some() {
                return Err(ConfigError::Storage(format!("{key} appears twice")));
            }
        }
        let text = |key: &'static str| {
            fields.get(key).copied().ok_or_else(|| ConfigError::Storage(format!("{key} missing")))
        };
        let number = |key: &'static str| {
            text(key)?
                .parse::<u64>()
                .map_err(|_| ConfigError::Storage(format!("{key} is not a number")))
        };
        let small = |key: &'static str| {
            u32::try_from(number(key)?)
                .map_err(|_| ConfigError::Storage(format!("{key} is out of range")))
        };
        let names = |key: &'static str| -> Result<Vec<String>, ConfigError> {
            let t = text(key)?;
            if t.is_empty() {
                return Ok(Vec::new());
            }
            t.split(',').map(|n| unescape(key, n)).collect()
        };
        let cfg = DriverConfig {
            config_gen: number("config_gen")?,
            sample_rate: small("sample_rate")?,
            input_channels: small("input_channels")?,
            output_channels: small("output_channels")?,
            input_safety_offset: small("input_safety_offset")?,
            output_safety_offset: small("output_safety_offset")?,
            input_latency: small("input_latency")?,
            output_latency: small("output_latency")?,
            input_read_delay: small("input_read_delay")?,
            clock_algorithm: small("clock_algorithm")?,
            clock_domain: small("clock_domain")?,
            input_names: names("input_names")?,
            output_names: names("output_names")?,
            device_name: unescape("device_name", text("device_name")?)?,
            // Optional, like the protocol key.
            debug_zts_jitter_ns: match fields.get("debug_zts_jitter_ns") {
                Some(_) => small("debug_zts_jitter_ns")?,
                None => 0,
            },
        };
        cfg.validate()?;
        Ok(cfg)
    }

    /// The configuration used until host storage or the daemon provides
    /// one: 48 kHz, 8 x 8 channels named 01 to 08, 4 ms latency, the
    /// default margins (500 us in, 1000 us out) and TX guard (500 us), in
    /// safety mode. Its offsets come from [`compute_offsets`], so they match
    /// what a default daemon sends.
    pub fn fallback() -> Self {
        const RATE: u32 = 48_000;
        const LATENCY_US: u64 = 4_000;
        const TX_GUARD_US: u64 = 500;
        // ovsc-core's FPP_MAX.
        const FPP_MAX: u32 = 32;
        let o = compute_offsets(&OffsetInputs {
            sample_rate: RATE,
            latency_samples: ((LATENCY_US * RATE as u64 + 500_000) / 1_000_000) as u32,
            tx_guard_samples: (TX_GUARD_US * RATE as u64 / 1_000_000) as u32,
            fpp_max: FPP_MAX,
            input_margin_us: 500,
            output_margin_us: 1000,
            output_latency_override: None,
            latency_mode: InputLatencyMode::Safety,
        });
        let names: Vec<String> = (1..=8).map(|i| format!("{i:02}")).collect();
        DriverConfig {
            config_gen: 0,
            sample_rate: RATE,
            input_channels: 8,
            output_channels: 8,
            input_safety_offset: o.input_safety,
            output_safety_offset: o.output_safety,
            input_latency: o.input_latency,
            output_latency: o.output_latency,
            input_read_delay: o.input_read_delay,
            clock_algorithm: CLOCK_ALGORITHM_RAW,
            clock_domain: 0,
            input_names: names.clone(),
            output_names: names,
            device_name: "OpenVirtualSoundcard".to_owned(),
            debug_zts_jitter_ns: 0,
        }
    }
}

fn check_names(field: &'static str, names: &[String], channels: u32) -> Result<(), ConfigError> {
    if names.len() != channels as usize {
        return Err(ConfigError::NameCount { field, expected: channels, got: names.len() });
    }
    names.iter().enumerate().try_for_each(|(i, n)| check_name(field, i, n))
}

fn check_name(field: &'static str, index: usize, name: &str) -> Result<(), ConfigError> {
    let reason = if name.is_empty() {
        "is empty"
    } else if name.len() > MAX_NAME_BYTES {
        "is longer than 31 bytes"
    } else if name.contains('\0') {
        "contains a NUL"
    } else if name.contains(['\n', '\r']) {
        "contains a line break"
    } else {
        return Ok(());
    };
    Err(ConfigError::Name { field, index, reason })
}

fn escape_into(out: &mut String, s: &str) {
    for b in s.bytes() {
        if (0x20..0x7f).contains(&b) && !matches!(b, b'%' | b',' | b'=') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
}

fn unescape(key: &str, s: &str) -> Result<String, ConfigError> {
    let bad = || ConfigError::Storage(format!("{key} has a bad escape"));
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b != b'%' {
            out.push(b);
            continue;
        }
        let mut hex = || bytes.next().and_then(|h| (h as char).to_digit(16)).ok_or_else(bad);
        let (hi, lo) = (hex()?, hex()?);
        out.push((hi * 16 + lo) as u8);
    }
    String::from_utf8(out).map_err(|_| ConfigError::Storage(format!("{key} is not UTF-8")))
}

// --- Latency and safety offsets ------------------------------------------

/// Where the driver reports the network latency on the input side.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InputLatencyMode {
    /// In the input safety offset; input latency 0, no read delay. Input
    /// times are then the senders' capture times.
    #[default]
    Safety,
    /// As input latency, with a matching read delay; the safety offset
    /// holds only the margin. Hosts that add both see the same total.
    Latency,
}

/// What [`compute_offsets`] needs, all at one sample rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OffsetInputs {
    pub sample_rate: u32,
    /// L: the receive latency, `round(latency_ms * fs / 1000)`.
    pub latency_samples: u32,
    /// The TX guard, `floor(tx_guard_us * fs / 1e6)`.
    pub tx_guard_samples: u32,
    /// The largest number of frames per packet the device sends.
    pub fpp_max: u32,
    pub input_margin_us: u32,
    pub output_margin_us: u32,
    /// Replaces L as the output latency when set.
    pub output_latency_override: Option<u32>,
    pub latency_mode: InputLatencyMode,
}

/// The driver's latency properties, in frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Offsets {
    pub input_safety: u32,
    pub output_safety: u32,
    pub input_latency: u32,
    pub output_latency: u32,
    pub input_read_delay: u32,
}

/// The offsets of design section 8.4, where `margin(us) = ceil(us * fs / 1e6)`:
///
/// * safety mode: input safety `L + margin(input)`, input latency 0, read
///   delay 0;
/// * latency mode: input safety `margin(input)`, input latency L, read
///   delay L;
/// * both modes: output safety `max(fpp_max - 1 - guard, 0) + margin(output)`,
///   output latency L or the override.
///
/// Results saturate at `u32::MAX`; [`DriverConfig::validate`] bounds them.
pub fn compute_offsets(i: &OffsetInputs) -> Offsets {
    let fs = i.sample_rate as u64;
    let margin = |us: u32| (us as u64 * fs).div_ceil(1_000_000);
    let sat = |v: u64| u32::try_from(v).unwrap_or(u32::MAX);
    let l = i.latency_samples as u64;
    let (input_safety, input_latency, input_read_delay) = match i.latency_mode {
        InputLatencyMode::Safety => (l + margin(i.input_margin_us), 0, 0),
        InputLatencyMode::Latency => (margin(i.input_margin_us), l, l),
    };
    let tx_lead = (i.fpp_max as u64).saturating_sub(1).saturating_sub(i.tx_guard_samples as u64);
    Offsets {
        input_safety: sat(input_safety),
        output_safety: sat(tx_lead + margin(i.output_margin_us)),
        input_latency: sat(input_latency),
        output_latency: i.output_latency_override.unwrap_or(i.latency_samples),
        input_read_delay: sat(input_read_delay),
    }
}

// --- Messages ------------------------------------------------------------

/// The driver's first message on every connection (sent with a reply).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub proto_major: u32,
    pub proto_minor: u32,
    pub layout_version: u32,
    pub layout_hash: u64,
    pub plugin_version: String,
    /// Random per driver instance.
    pub instance: u64,
    pub pid: i32,
    /// The generation of the region the driver has mapped (0 if none).
    pub applied_daemon_generation: u64,
    pub applied_config_gen: u64,
    pub sample_rate: u32,
    pub input_channels: u32,
    pub output_channels: u32,
    pub timebase_numer: u32,
    pub timebase_denom: u32,
    pub arch: u32,
}

/// The daemon's reply to a compatible [`Hello`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Welcome {
    pub proto_major: u32,
    pub proto_minor: u32,
    pub daemon_version: String,
    pub daemon_generation: u64,
    /// The daemon's shared region.
    pub region: RegionHandle,
    pub region_size: u64,
    pub config: DriverConfig,
}

/// Why the daemon refused a [`Hello`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    /// A different protocol major version.
    Proto,
    /// A different shared-memory layout.
    Layout,
}

/// The daemon's reply to an incompatible [`Hello`]. The numbers are the
/// daemon's own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reject {
    pub reason: RejectReason,
    pub proto_major: u32,
    pub layout_version: u32,
    pub layout_hash: u64,
    pub message: String,
}

/// Sent by the driver once it publishes a configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfigApplied {
    pub daemon_generation: u64,
    pub config_gen: u64,
    pub sample_rate: u32,
    pub input_channels: u32,
    pub output_channels: u32,
}

/// Driver to daemon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToDaemon {
    Hello(Hello),
    ConfigApplied(ConfigApplied),
}

/// Daemon to driver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToPlugin {
    Welcome(Welcome),
    Reject(Reject),
    /// A new configuration, unsolicited.
    Config(DriverConfig),
    /// The daemon is shutting down.
    Bye {
        reason: String,
    },
}

/// A message on the wire: keys to typed values.
pub type Kv = BTreeMap<String, Value>;

/// A typed value. Each transport maps these to its own types (XPC: uint64,
/// int64, bool, string, array of strings, dictionary, shmem).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    U64(u64),
    I64(i64),
    Bool(bool),
    Str(String),
    StrList(Vec<String>),
    Dict(Kv),
    Region(RegionHandle),
}

/// Why a [`Kv`] is not a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtoError {
    /// No `op` string.
    MissingOp,
    /// An `op` this version does not know (ignored by the transports).
    UnknownOp(String),
    /// A required key is absent.
    Missing(&'static str),
    /// A key holds the wrong type.
    WrongType(&'static str),
    /// A number does not fit its field.
    OutOfRange(&'static str),
    /// A string field holds a value this version does not know.
    BadValue(&'static str),
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::MissingOp => write!(f, "message without op"),
            ProtoError::UnknownOp(op) => write!(f, "unknown op {op:?}"),
            ProtoError::Missing(k) => write!(f, "missing key {k}"),
            ProtoError::WrongType(k) => write!(f, "key {k} has the wrong type"),
            ProtoError::OutOfRange(k) => write!(f, "key {k} is out of range"),
            ProtoError::BadValue(k) => write!(f, "key {k} has an unknown value"),
        }
    }
}

impl std::error::Error for ProtoError {}

const OP: &str = "op";
const OP_HELLO: &str = "hello";
const OP_CONFIG_APPLIED: &str = "config_applied";
const OP_WELCOME: &str = "welcome";
const OP_REJECT: &str = "reject";
const OP_CONFIG: &str = "config";
const OP_BYE: &str = "bye";
const REASON_PROTO: &str = "proto";
const REASON_LAYOUT: &str = "layout";

/// Builds a [`Kv`].
struct Enc(Kv);

impl Enc {
    fn op(op: &str) -> Self {
        Enc(Kv::new()).str(OP, op)
    }

    fn new() -> Self {
        Enc(Kv::new())
    }

    fn u64(mut self, k: &str, v: u64) -> Self {
        self.0.insert(k.to_owned(), Value::U64(v));
        self
    }

    fn u32(self, k: &str, v: u32) -> Self {
        self.u64(k, v as u64)
    }

    fn i64(mut self, k: &str, v: i64) -> Self {
        self.0.insert(k.to_owned(), Value::I64(v));
        self
    }

    fn str(mut self, k: &str, v: &str) -> Self {
        self.0.insert(k.to_owned(), Value::Str(v.to_owned()));
        self
    }

    fn list(mut self, k: &str, v: &[String]) -> Self {
        self.0.insert(k.to_owned(), Value::StrList(v.to_vec()));
        self
    }

    fn value(mut self, k: &str, v: Value) -> Self {
        self.0.insert(k.to_owned(), v);
        self
    }
}

/// Reads typed fields from a [`Kv`].
struct Dec<'a>(&'a Kv);

impl Dec<'_> {
    fn get(&self, k: &'static str) -> Result<&Value, ProtoError> {
        self.0.get(k).ok_or(ProtoError::Missing(k))
    }

    fn u64(&self, k: &'static str) -> Result<u64, ProtoError> {
        match self.get(k)? {
            Value::U64(v) => Ok(*v),
            _ => Err(ProtoError::WrongType(k)),
        }
    }

    fn u32(&self, k: &'static str) -> Result<u32, ProtoError> {
        u32::try_from(self.u64(k)?).map_err(|_| ProtoError::OutOfRange(k))
    }

    fn i32(&self, k: &'static str) -> Result<i32, ProtoError> {
        match self.get(k)? {
            Value::I64(v) => i32::try_from(*v).map_err(|_| ProtoError::OutOfRange(k)),
            _ => Err(ProtoError::WrongType(k)),
        }
    }

    fn str(&self, k: &'static str) -> Result<String, ProtoError> {
        match self.get(k)? {
            Value::Str(s) => Ok(s.clone()),
            _ => Err(ProtoError::WrongType(k)),
        }
    }

    fn list(&self, k: &'static str) -> Result<Vec<String>, ProtoError> {
        match self.get(k)? {
            Value::StrList(l) => Ok(l.clone()),
            _ => Err(ProtoError::WrongType(k)),
        }
    }

    fn dict(&self, k: &'static str) -> Result<&Kv, ProtoError> {
        match self.get(k)? {
            Value::Dict(d) => Ok(d),
            _ => Err(ProtoError::WrongType(k)),
        }
    }

    fn op(&self) -> Result<&str, ProtoError> {
        match self.0.get(OP) {
            Some(Value::Str(s)) => Ok(s),
            _ => Err(ProtoError::MissingOp),
        }
    }
}

fn encode_config(c: &DriverConfig) -> Kv {
    Enc::new()
        .u64("config_gen", c.config_gen)
        .u32("sample_rate", c.sample_rate)
        .u32("input_channels", c.input_channels)
        .u32("output_channels", c.output_channels)
        .u32("input_safety_offset", c.input_safety_offset)
        .u32("output_safety_offset", c.output_safety_offset)
        .u32("input_latency", c.input_latency)
        .u32("output_latency", c.output_latency)
        .u32("input_read_delay", c.input_read_delay)
        .u32("clock_algorithm", c.clock_algorithm)
        .u32("clock_domain", c.clock_domain)
        .list("input_names", &c.input_names)
        .list("output_names", &c.output_names)
        .str("device_name", &c.device_name)
        .u32("debug_zts_jitter_ns", c.debug_zts_jitter_ns)
        .0
}

/// Decodes a config dict. Checks only presence and types; the receiver
/// validates the result with [`DriverConfig::validate`].
fn decode_config(kv: &Kv) -> Result<DriverConfig, ProtoError> {
    let d = Dec(kv);
    Ok(DriverConfig {
        config_gen: d.u64("config_gen")?,
        sample_rate: d.u32("sample_rate")?,
        input_channels: d.u32("input_channels")?,
        output_channels: d.u32("output_channels")?,
        input_safety_offset: d.u32("input_safety_offset")?,
        output_safety_offset: d.u32("output_safety_offset")?,
        input_latency: d.u32("input_latency")?,
        output_latency: d.u32("output_latency")?,
        input_read_delay: d.u32("input_read_delay")?,
        clock_algorithm: d.u32("clock_algorithm")?,
        clock_domain: d.u32("clock_domain")?,
        input_names: d.list("input_names")?,
        output_names: d.list("output_names")?,
        device_name: d.str("device_name")?,
        // A minor-version key: absent from older daemons.
        debug_zts_jitter_ns: match kv.get("debug_zts_jitter_ns") {
            None => 0,
            Some(_) => d.u32("debug_zts_jitter_ns")?,
        },
    })
}

/// Encodes a driver-to-daemon message.
pub fn encode_to_daemon(m: &ToDaemon) -> Kv {
    match m {
        ToDaemon::Hello(h) => {
            Enc::op(OP_HELLO)
                .u32("proto_major", h.proto_major)
                .u32("proto_minor", h.proto_minor)
                .u32("layout_version", h.layout_version)
                .u64("layout_hash", h.layout_hash)
                .str("plugin_version", &h.plugin_version)
                .u64("instance", h.instance)
                .i64("pid", h.pid as i64)
                .u64("applied_daemon_generation", h.applied_daemon_generation)
                .u64("applied_config_gen", h.applied_config_gen)
                .u32("sample_rate", h.sample_rate)
                .u32("input_channels", h.input_channels)
                .u32("output_channels", h.output_channels)
                .u32("timebase_numer", h.timebase_numer)
                .u32("timebase_denom", h.timebase_denom)
                .u32("arch", h.arch)
                .0
        }
        ToDaemon::ConfigApplied(a) => {
            Enc::op(OP_CONFIG_APPLIED)
                .u64("daemon_generation", a.daemon_generation)
                .u64("config_gen", a.config_gen)
                .u32("sample_rate", a.sample_rate)
                .u32("input_channels", a.input_channels)
                .u32("output_channels", a.output_channels)
                .0
        }
    }
}

/// Decodes a driver-to-daemon message.
pub fn decode_to_daemon(kv: &Kv) -> Result<ToDaemon, ProtoError> {
    let d = Dec(kv);
    match d.op()? {
        OP_HELLO => Ok(ToDaemon::Hello(Hello {
            proto_major: d.u32("proto_major")?,
            proto_minor: d.u32("proto_minor")?,
            layout_version: d.u32("layout_version")?,
            layout_hash: d.u64("layout_hash")?,
            plugin_version: d.str("plugin_version")?,
            instance: d.u64("instance")?,
            pid: d.i32("pid")?,
            applied_daemon_generation: d.u64("applied_daemon_generation")?,
            applied_config_gen: d.u64("applied_config_gen")?,
            sample_rate: d.u32("sample_rate")?,
            input_channels: d.u32("input_channels")?,
            output_channels: d.u32("output_channels")?,
            timebase_numer: d.u32("timebase_numer")?,
            timebase_denom: d.u32("timebase_denom")?,
            arch: d.u32("arch")?,
        })),
        OP_CONFIG_APPLIED => Ok(ToDaemon::ConfigApplied(ConfigApplied {
            daemon_generation: d.u64("daemon_generation")?,
            config_gen: d.u64("config_gen")?,
            sample_rate: d.u32("sample_rate")?,
            input_channels: d.u32("input_channels")?,
            output_channels: d.u32("output_channels")?,
        })),
        op => Err(ProtoError::UnknownOp(op.to_owned())),
    }
}

/// Encodes a daemon-to-driver message. The region of a welcome travels
/// under the key `region`; a configuration, in a welcome or on its own,
/// as a dict under the key `config`.
pub fn encode_to_plugin(m: &ToPlugin) -> Kv {
    match m {
        ToPlugin::Welcome(w) => {
            Enc::op(OP_WELCOME)
                .u32("proto_major", w.proto_major)
                .u32("proto_minor", w.proto_minor)
                .str("daemon_version", &w.daemon_version)
                .u64("daemon_generation", w.daemon_generation)
                .value("region", Value::Region(w.region.clone()))
                .u64("region_size", w.region_size)
                .value("config", Value::Dict(encode_config(&w.config)))
                .0
        }
        ToPlugin::Reject(r) => {
            let reason = match r.reason {
                RejectReason::Proto => REASON_PROTO,
                RejectReason::Layout => REASON_LAYOUT,
            };
            Enc::op(OP_REJECT)
                .str("reason", reason)
                .u32("proto_major", r.proto_major)
                .u32("layout_version", r.layout_version)
                .u64("layout_hash", r.layout_hash)
                .str("message", &r.message)
                .0
        }
        ToPlugin::Config(c) => Enc::op(OP_CONFIG).value("config", Value::Dict(encode_config(c))).0,
        ToPlugin::Bye { reason } => Enc::op(OP_BYE).str("reason", reason).0,
    }
}

/// Decodes a daemon-to-driver message. Takes the map by value so that the
/// region handle moves out of it.
pub fn decode_to_plugin(mut kv: Kv) -> Result<ToPlugin, ProtoError> {
    let op = Dec(&kv).op()?.to_owned();
    match op.as_str() {
        OP_WELCOME => {
            let region = match kv.remove("region") {
                Some(Value::Region(r)) => r,
                Some(_) => return Err(ProtoError::WrongType("region")),
                None => return Err(ProtoError::Missing("region")),
            };
            let d = Dec(&kv);
            Ok(ToPlugin::Welcome(Welcome {
                proto_major: d.u32("proto_major")?,
                proto_minor: d.u32("proto_minor")?,
                daemon_version: d.str("daemon_version")?,
                daemon_generation: d.u64("daemon_generation")?,
                region,
                region_size: d.u64("region_size")?,
                config: decode_config(d.dict("config")?)?,
            }))
        }
        OP_REJECT => {
            let d = Dec(&kv);
            let reason = match d.str("reason")?.as_str() {
                REASON_PROTO => RejectReason::Proto,
                REASON_LAYOUT => RejectReason::Layout,
                _ => return Err(ProtoError::BadValue("reason")),
            };
            Ok(ToPlugin::Reject(Reject {
                reason,
                proto_major: d.u32("proto_major")?,
                layout_version: d.u32("layout_version")?,
                layout_hash: d.u64("layout_hash")?,
                message: d.str("message")?,
            }))
        }
        OP_CONFIG => Ok(ToPlugin::Config(decode_config(Dec(&kv).dict("config")?)?)),
        OP_BYE => Ok(ToPlugin::Bye { reason: Dec(&kv).str("reason")? }),
        _ => Err(ProtoError::UnknownOp(op)),
    }
}
