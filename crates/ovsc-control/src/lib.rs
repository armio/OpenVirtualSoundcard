//! The OpenVirtualSoundcard daemon's local control interface.
//!
//! The OpenVirtualSoundcard app, and scripts, use it to read the device's status and to
//! change its settings. The daemon listens on a Unix stream socket at
//! [`SOCKET_PATH`], owned by root and the `admin` group with mode 0660, so
//! an administrator's processes may connect and nobody else's. Each request
//! is one line of JSON ([`Request`]), answered by one line of JSON
//! ([`Response`]); a connection may carry any number of them.
//!
//! ```text
//! → {"op":"status"}
//! ← {"result":"status","protocol":1,"version":"0.1.0",...}
//! → {"op":"apply","change":{"latency_ms":2.0}}
//! ← {"result":"applied","restart":"device"}
//! ```

use serde::{Deserialize, Serialize};

/// Where the daemon listens.
pub const SOCKET_PATH: &str = "/var/run/ovsc/control.sock";

/// Version of these messages. The daemon reports it in
/// [`Status::protocol`]; it changes only with incompatible changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// Receive latencies offered for selection, in milliseconds. Any value in
/// [`LATENCY_MIN_MS`]..=[`LATENCY_MAX_MS`] is accepted.
pub const LATENCY_CHOICES_MS: &[f64] = &[1.0, 2.0, 4.0, 5.0, 6.0, 10.0, 20.0, 40.0];
pub const LATENCY_MIN_MS: f64 = 0.25;
pub const LATENCY_MAX_MS: f64 = 40.0;

/// Sample rates the device can run at.
pub const SAMPLE_RATES: &[u32] = &[44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

/// Most receive or transmit channels.
pub const MAX_CHANNELS: u16 = 128;

/// Bit depths the device can transmit (Dante Controller's "encoding").
pub const BITS_PER_SAMPLE: &[u16] = &[16, 24, 32];

/// A request to the daemon.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// What the device is doing: answered by [`Response::Status`].
    Status,
    /// The current settings: answered by [`Response::Settings`].
    Settings,
    /// The network interfaces the device could use: answered by
    /// [`Response::Interfaces`].
    Interfaces,
    /// Changes settings: answered by [`Response::Applied`] or
    /// [`Response::Error`]. Fields left out keep their value.
    Apply { change: SettingsChange },
}

/// The daemon's answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Status(Box<Status>),
    Settings(Settings),
    Interfaces {
        interfaces: Vec<InterfaceInfo>,
    },
    /// The change was accepted and saved; `restart` says what restarts to
    /// apply it.
    Applied {
        restart: Restart,
    },
    Error {
        message: String,
    },
}

/// What restarts to apply a change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Restart {
    /// Nothing: applied at once (a new name).
    None,
    /// The network engine restarts, the clock keeps running: audio stops
    /// for a second or two (a new latency).
    Device,
    /// The whole daemon restarts and the clock locks again: audio stops for
    /// ten to twenty seconds (a new interface, sample rate or channel
    /// count).
    Daemon,
}

/// What the device is doing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// [`PROTOCOL_VERSION`] of the daemon.
    pub protocol: u32,
    /// The daemon's version.
    pub version: String,
    /// Whether the network engine runs.
    pub engine_running: bool,
    /// Why the engine does not run, if it does not.
    pub engine_error: Option<String>,
    /// The device, while the engine runs.
    pub device: Option<DeviceStatus>,
    pub clock: ClockInfo,
    pub driver: DriverInfo,
    /// Problems worth showing, in plain words.
    pub warnings: Vec<String>,
}

/// The running Dante device.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceStatus {
    pub name: String,
    /// The network interface, as configured (`en7`) or chosen.
    pub interface: String,
    pub ip: String,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
    pub latency_ms: f64,
    /// The receive channels: the Core Audio device's inputs.
    pub rx_channels: Vec<RxChannelStatus>,
    /// The transmit channel names: the Core Audio device's outputs.
    pub tx_channels: Vec<String>,
    /// Flows other devices receive from this one.
    pub tx_flows: Vec<TxFlowStatus>,
    pub packets: PacketCounts,
}

/// One receive channel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RxChannelStatus {
    pub name: String,
    /// The transmit channel it is subscribed to, as `channel@device`.
    pub source: Option<String>,
    /// How the subscription is doing, in plain words ("receiving",
    /// "connecting", "transmitter not found", …); empty without one.
    pub state: String,
    /// Whether audio arrives.
    pub receiving: bool,
}

/// A flow another device receives from this one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TxFlowStatus {
    /// The receiving device's name, if it gave one.
    pub receiver: Option<String>,
    /// Where the audio goes, `address:port`.
    pub destination: String,
    /// The 1-based transmit channels it carries, 0 for an empty slot.
    pub channels: Vec<u16>,
}

/// Audio packet counters since the engine started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PacketCounts {
    pub tx: u64,
    /// Packets sent while an application gave no audio for them.
    pub tx_underruns: u64,
    pub rx: u64,
    /// Packets that arrived after their play-out time.
    pub rx_late: u64,
}

/// The media clock.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClockInfo {
    /// `"ptp"` (following the network) or `"free"` (this Mac's own clock).
    pub source: String,
    /// `"Unlocked"`, `"Locking"`, `"Locked"`, `"Holdover"` or `"FreeRunning"`.
    pub state: String,
    /// Whether the clock is good to play audio on.
    pub locked: bool,
    /// The clock leader followed, as its address, if any.
    pub leader: Option<String>,
    /// Last measured offset from the leader, ns.
    pub offset_ns: i64,
    /// Last measured network delay to the leader, ns.
    pub path_delay_ns: i64,
    /// This Mac's clock rate against the leader's, ppm.
    pub freq_offset_ppm: f64,
}

/// The Core Audio driver, as the daemon sees it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DriverInfo {
    /// Whether a driver instance is connected to the daemon.
    pub connected: bool,
    /// Whether an application is playing or recording through the device.
    pub io_running: bool,
    /// Whether audio may flow: the driver's gate is open.
    pub audio_flowing: bool,
    /// The driver's status line, for diagnostics.
    pub detail: String,
}

/// The settings the app can change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// The device name shown in Dante Controller.
    pub name: String,
    /// The network interface: a name (`en7`), an IPv4 address, or empty for
    /// the interface of the default route.
    pub interface: String,
    pub sample_rate: u32,
    /// Bit depth of the transmitted audio.
    pub bits_per_sample: u16,
    pub rx_channels: u16,
    pub tx_channels: u16,
    /// Receive latency, ms.
    pub latency_ms: f64,
}

/// A change of [`Settings`]: fields left out keep their value.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SettingsChange {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bits_per_sample: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_channels: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_channels: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<f64>,
}

impl SettingsChange {
    /// Whether it changes nothing.
    pub fn is_empty(&self) -> bool {
        *self == SettingsChange::default()
    }
}

/// A network interface with an IPv4 address.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InterfaceInfo {
    /// BSD name, `en7`.
    pub name: String,
    /// What macOS calls it, `USB 10/100/1000 LAN`, if known.
    pub description: String,
    pub ipv4: Vec<String>,
    /// Whether it carries the default route.
    pub default_route: bool,
}

/// One line of the protocol: `value` as JSON and a newline.
pub fn encode_line<T: Serialize>(value: &T) -> String {
    let mut s = serde_json::to_string(value).expect("control messages always serialize");
    s.push('\n');
    s
}

/// Parses one line of the protocol.
pub fn decode_line<'a, T: Deserialize<'a>>(line: &'a str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim_end())
}

#[cfg(unix)]
pub use client::Client;

#[cfg(unix)]
mod client {
    use std::io::{self, BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::time::Duration;

    use super::{Request, Response, SOCKET_PATH, decode_line, encode_line};

    /// A connection to the daemon. Blocking, with a timeout on each request.
    pub struct Client {
        reader: BufReader<UnixStream>,
        writer: UnixStream,
    }

    impl Client {
        /// Connects to the daemon at [`SOCKET_PATH`].
        pub fn connect() -> io::Result<Client> {
            Self::connect_to(Path::new(SOCKET_PATH))
        }

        pub fn connect_to(path: &Path) -> io::Result<Client> {
            let stream = UnixStream::connect(path)?;
            stream.set_read_timeout(Some(Duration::from_secs(10)))?;
            stream.set_write_timeout(Some(Duration::from_secs(5)))?;
            Ok(Client { reader: BufReader::new(stream.try_clone()?), writer: stream })
        }

        /// Sends `request` and waits for the answer.
        pub fn request(&mut self, request: &Request) -> io::Result<Response> {
            self.writer.write_all(encode_line(request).as_bytes())?;
            let mut line = String::new();
            if self.reader.read_line(&mut line)? == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the daemon hung up"));
            }
            decode_line(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_one_json_line() {
        assert_eq!(encode_line(&Request::Status), "{\"op\":\"status\"}\n");
        let apply = Request::Apply {
            change: SettingsChange { latency_ms: Some(2.0), ..Default::default() },
        };
        assert_eq!(encode_line(&apply), "{\"op\":\"apply\",\"change\":{\"latency_ms\":2.0}}\n");
        assert_eq!(decode_line::<Request>(&encode_line(&apply)).unwrap(), apply);
        assert_eq!(
            decode_line::<Request>("{\"op\":\"apply\",\"change\":{}}").unwrap(),
            Request::Apply { change: SettingsChange::default() }
        );
    }

    #[test]
    fn responses_round_trip() {
        let responses = [
            Response::Applied { restart: Restart::Device },
            Response::Error { message: "no".into() },
            Response::Interfaces {
                interfaces: vec![InterfaceInfo {
                    name: "en7".into(),
                    description: "USB 10/100/1000 LAN".into(),
                    ipv4: vec!["192.168.0.8".into()],
                    default_route: false,
                }],
            },
            Response::Settings(Settings {
                name: "m1".into(),
                interface: "en7".into(),
                sample_rate: 48_000,
                bits_per_sample: 24,
                rx_channels: 8,
                tx_channels: 8,
                latency_ms: 4.0,
            }),
        ];
        for r in responses {
            assert_eq!(decode_line::<Response>(&encode_line(&r)).unwrap(), r);
        }
        assert_eq!(
            encode_line(&Response::Applied { restart: Restart::None }),
            "{\"result\":\"applied\",\"restart\":\"none\"}\n"
        );
    }

    #[test]
    fn the_latency_choices_are_in_range() {
        assert!(LATENCY_CHOICES_MS.iter().all(|&l| (LATENCY_MIN_MS..=LATENCY_MAX_MS).contains(&l)));
    }
}
