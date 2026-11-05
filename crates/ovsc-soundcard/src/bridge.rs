//! Opening device streams and wiring them to the network rings.

use std::sync::Arc;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use tracing::{info, warn};

use ovsc_clock::MediaClock;
use ovsc_core::AudioIo;

use crate::device::{
    Direction, StreamChoice, choose_config, device_id, find_device, open_host, resolve_map,
};
use crate::health::{Level, Watch};
use crate::kernel::HALF;
use crate::rt;
use crate::sample::DeviceSample;
use crate::stats::{DirectionStats, StatsCell};
use crate::stream::{Capture, Playout, transmit_lead};
use crate::{Error, Result};

/// Default extra safety margin, milliseconds.
pub const DEFAULT_MARGIN_MS: f64 = 1.0;
/// How long to wait for a backend to open a stream.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the health monitor looks at the streams.
const HEALTH_INTERVAL: Duration = Duration::from_millis(250);
/// A stream that has stalled for this long needs reopening.
const REOPEN_AFTER_MS: f64 = 1000.0;

/// What to bridge, and how.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BridgeConfig {
    /// Device that plays the network's receive channels (matched by exact
    /// name or identifier, else by unique substring, ignoring case;
    /// `"default"` for the system default). `None`: no playback.
    pub output_device: Option<String>,
    /// Device whose input feeds the network's transmit channels. `None`: no
    /// capture.
    pub input_device: Option<String>,
    /// Device buffer size to request, in frames (clamped to what the device
    /// supports). `None`: the device's default. Smaller buffers mean less
    /// latency and more CPU wake-ups.
    pub buffer_frames: Option<u32>,
    /// `output_map[i]` is the receive channel (0-based index into
    /// [`AudioIo::rx`]) played on device output channel `i`. `None` entries
    /// and device channels beyond the map stay silent. Without a map,
    /// receive channel `i` plays on device channel `i`, as far as both
    /// exist.
    pub output_map: Option<Vec<Option<usize>>>,
    /// `input_map[i]` is the transmit channel (0-based index into
    /// [`AudioIo::tx`]) fed by device input channel `i`, each at most once.
    /// `None` entries and device channels beyond the map are ignored.
    /// Without a map, device channel `i` feeds transmit channel `i`, as far
    /// as both exist.
    pub input_map: Option<Vec<Option<usize>>>,
    /// Extra safety margin between the device streams and the network's
    /// edge, in milliseconds (default 1 ms). It absorbs callback jitter;
    /// raise it if [`DirectionStats::underruns`] (or the transmitter's
    /// underruns in the daemon's status line) keep growing. Capture keeps
    /// the transmitter's packet lead on top of it, computed for the default
    /// `tx_guard_us` of 500 µs; a smaller guard needs a larger margin.
    pub margin_ms: Option<f64>,
}

/// A running bridge. Dropping it stops the streams. It is `Send`, so it
/// can live on whichever thread manages the device.
///
/// A small thread watches both streams and logs what an engineer should
/// know before it becomes audible: xruns, underruns and jumps (as
/// rate-limited warnings), a device that is lost, stalls or gets rerouted,
/// lost lock, refused real-time priority, and the latency each direction
/// adds.
pub struct Bridge {
    output: Option<Half>,
    input: Option<Half>,
    monitor: Option<Monitor>,
}

/// One open device stream.
struct Half {
    stream: cpal::Stream,
    probe: Probe,
}

impl Half {
    fn stats(&self) -> DirectionStats {
        self.probe.stats()
    }
}

/// What it takes to snapshot one direction's health.
#[derive(Clone)]
struct Probe {
    stats: Arc<StatsCell>,
    device: String,
    channels: u16,
    format: String,
}

impl Probe {
    fn stats(&self) -> DirectionStats {
        self.stats.snapshot(&self.device, self.channels, &self.format)
    }
}

/// The health-monitor thread; stops when dropped.
struct Monitor {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Monitor {
    fn spawn(probes: Vec<(&'static str, Probe)>) -> Option<Monitor> {
        let (stop, stopped) = mpsc::channel::<()>();
        let spawned =
            std::thread::Builder::new().name("ovsc-bridge-health".into()).spawn(move || {
                let start = Instant::now();
                let mut watches: Vec<_> =
                    probes.into_iter().map(|(what, probe)| (Watch::new(what), probe)).collect();
                // Ends when the bridge sends a stop or drops the sender.
                while let Err(mpsc::RecvTimeoutError::Timeout) =
                    stopped.recv_timeout(HEALTH_INTERVAL)
                {
                    let now = start.elapsed().as_secs_f64();
                    for (watch, probe) in &mut watches {
                        for (level, message) in watch.check(now, &probe.stats()) {
                            match level {
                                Level::Warn => warn!("{message}"),
                                Level::Info => info!("{message}"),
                            }
                        }
                    }
                }
            });
        match spawned {
            Ok(thread) => Some(Monitor { stop: Some(stop), thread: Some(thread) }),
            Err(e) => {
                warn!("cannot start the sound-card health monitor: {e}");
                None
            }
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Health of both directions of a [`Bridge`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BridgeStats {
    /// Playback (network receive channels → device), if open.
    pub output: Option<DirectionStats>,
    /// Capture (device → network transmit channels), if open.
    pub input: Option<DirectionStats>,
}

impl BridgeStats {
    /// Whether a direction has failed for good: its device is gone, or it
    /// hasn't called back for a second. Only a new [`Bridge::start`] gets
    /// audio flowing again then.
    pub fn needs_reopen(&self) -> bool {
        [&self.output, &self.input]
            .into_iter()
            .flatten()
            .any(|d| d.device_lost || (d.stalled && d.since_callback_ms >= REOPEN_AFTER_MS))
    }
}

impl Bridge {
    /// Opens the configured devices at the network's sample rate and starts
    /// streaming. Fails if a device can't be found or doesn't support the
    /// sample rate, or if a channel map doesn't fit.
    pub fn start(io: AudioIo, cfg: &BridgeConfig) -> Result<Bridge> {
        if cfg.output_device.is_none() && cfg.input_device.is_none() {
            return Err(Error::Config("no output_device or input_device to bridge".into()));
        }
        let margin_ms = cfg.margin_ms.unwrap_or(DEFAULT_MARGIN_MS);
        if !(0.1..=100.0).contains(&margin_ms) {
            return Err(Error::Config(format!(
                "margin_ms must be between 0.1 and 100, not {margin_ms}"
            )));
        }
        let margin = (margin_ms * io.sample_rate as f64 / 1000.0).round() as u64;
        let host = open_host()?;
        let output = match &cfg.output_device {
            Some(query) => Some(open_output(&host, &io, cfg, query, margin)?),
            None => None,
        };
        let input = match &cfg.input_device {
            Some(query) => Some(open_input(&host, &io, cfg, query, margin)?),
            None => None,
        };
        let probes = [("playback", &output), ("capture", &input)]
            .into_iter()
            .filter_map(|(what, half)| Some((what, half.as_ref()?.probe.clone())))
            .collect();
        let monitor = Monitor::spawn(probes);
        Ok(Bridge { output, input, monitor })
    }

    /// Current health of both directions.
    pub fn stats(&self) -> BridgeStats {
        BridgeStats {
            output: self.output.as_ref().map(Half::stats),
            input: self.input.as_ref().map(Half::stats),
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        for half in [&self.output, &self.input].into_iter().flatten() {
            // Dropping the stream stops it too; pausing first makes the
            // last callbacks end cleanly on backends that drain.
            let _ = half.stream.pause();
        }
        drop(self.monitor.take());
    }
}

fn open_output(
    host: &cpal::Host,
    io: &AudioIo,
    cfg: &BridgeConfig,
    query: &str,
    margin: u64,
) -> Result<Half> {
    let (device, name) = find_device(host, query, Direction::Output)?;
    let promote = promotion(&device, &name);
    let ranges: Vec<_> = device
        .supported_output_configs()
        .map_err(|e| Error::audio(format!("{name}: cannot query output formats"), e))?
        .collect();
    let map = cfg.output_map.as_deref();
    let choice = choose(&name, &ranges, io, map, io.rx.len(), cfg.buffer_frames)?;
    let map = resolve_map(map, choice.channels as usize, io.rx.len(), false, "output_map")
        .map_err(Error::Config)?;
    let sources = map.iter().map(|m| m.map(|i| io.rx[i].clone())).collect();
    let stats = Arc::new(StatsCell::new());
    let playout = Playout::new(io.sample_rate, io.latency.clone(), sources, margin, stats.clone());
    let config = stream_config(io, &choice);
    let clock = io.clock.clone();
    let s = stats.clone();
    let p = promote;
    let built = match choice.format {
        SampleFormat::F32 => build_output::<f32>(&device, config, playout, clock, s, p),
        SampleFormat::I32 => build_output::<i32>(&device, config, playout, clock, s, p),
        SampleFormat::I24 => build_output::<cpal::I24>(&device, config, playout, clock, s, p),
        SampleFormat::I16 => build_output::<i16>(&device, config, playout, clock, s, p),
        other => return Err(unsupported_format(&name, other)),
    };
    let stream =
        built.map_err(|e| Error::audio(format!("{name}: cannot open output stream"), e))?;
    stream.play().map_err(|e| Error::audio(format!("{name}: cannot start output stream"), e))?;
    info!(
        "playing {} on {name:?}: {} channels {} at {} Hz, buffer {}; latency: network + one \
         buffer + {:.2} ms + device output",
        describe_map(&map, &io.rx_names, true),
        choice.channels,
        choice.format,
        io.sample_rate,
        describe_buffer(&choice),
        (HALF as u64 + margin) as f64 * 1000.0 / io.sample_rate as f64,
    );
    let probe =
        Probe { stats, device: name, channels: choice.channels, format: choice.format.to_string() };
    Ok(Half { stream, probe })
}

fn open_input(
    host: &cpal::Host,
    io: &AudioIo,
    cfg: &BridgeConfig,
    query: &str,
    margin: u64,
) -> Result<Half> {
    let (device, name) = find_device(host, query, Direction::Input)?;
    let promote = promotion(&device, &name);
    let ranges: Vec<_> = device
        .supported_input_configs()
        .map_err(|e| Error::audio(format!("{name}: cannot query input formats"), e))?
        .collect();
    let map = cfg.input_map.as_deref();
    let choice = choose(&name, &ranges, io, map, io.tx.len(), cfg.buffer_frames)?;
    let map = resolve_map(map, choice.channels as usize, io.tx.len(), true, "input_map")
        .map_err(Error::Config)?;
    let sinks = map.iter().map(|m| m.map(|i| io.tx[i].clone())).collect();
    let stats = Arc::new(StatsCell::new());
    let bits = io.format.bits() as u32;
    // AudioIo doesn't carry the transmit guard; the default is what nearly
    // every configuration uses.
    let lead = transmit_lead(io.sample_rate, ovsc_core::DeviceConfig::default().tx_guard_us);
    let capture = Capture::new(io.sample_rate, bits, sinks, margin, lead, stats.clone());
    let config = stream_config(io, &choice);
    let clock = io.clock.clone();
    let s = stats.clone();
    let p = promote;
    let built = match choice.format {
        SampleFormat::F32 => build_input::<f32>(&device, config, capture, clock, s, p),
        SampleFormat::I32 => build_input::<i32>(&device, config, capture, clock, s, p),
        SampleFormat::I24 => build_input::<cpal::I24>(&device, config, capture, clock, s, p),
        SampleFormat::I16 => build_input::<i16>(&device, config, capture, clock, s, p),
        other => return Err(unsupported_format(&name, other)),
    };
    let stream = built.map_err(|e| Error::audio(format!("{name}: cannot open input stream"), e))?;
    stream.play().map_err(|e| Error::audio(format!("{name}: cannot start input stream"), e))?;
    info!(
        "capturing {} from {name:?}: {} channels {} at {} Hz, buffer {}; latency: one buffer + \
         {:.2} ms + device input",
        describe_map(&map, &io.tx_names, false),
        choice.channels,
        choice.format,
        io.sample_rate,
        describe_buffer(&choice),
        (HALF as u64 + margin + lead) as f64 * 1000.0 / io.sample_rate as f64,
    );
    let probe =
        Probe { stats, device: name, channels: choice.channels, format: choice.format.to_string() };
    Ok(Half { stream, probe })
}

/// Whether the bridge raises the priority of `device`'s callbacks itself
/// (see [`rt`]).
fn promotion(device: &cpal::Device, name: &str) -> bool {
    let id = device_id(device).unwrap_or_else(|| name.to_owned());
    let promote = rt::wanted(&id);
    if cfg!(all(unix, not(target_vendor = "apple"))) && !promote {
        info!("{name:?} ({id}) is served by a sound server: its callbacks keep their priority");
    }
    promote
}

fn unsupported_format(name: &str, format: SampleFormat) -> Error {
    Error::Unsupported { device: name.to_owned(), message: format!("uses unsupported {format}") }
}

/// Chooses the stream configuration for a device with `ranges`, bridging
/// `network_channels` channels (or the channels of `map`).
fn choose(
    name: &str,
    ranges: &[cpal::SupportedStreamConfigRange],
    io: &AudioIo,
    map: Option<&[Option<usize>]>,
    network_channels: usize,
    buffer_frames: Option<u32>,
) -> Result<StreamChoice> {
    let (wanted, required) = match map {
        Some(map) => (map.len(), map.len().max(1)),
        None => (network_channels, 1),
    };
    let choice = choose_config(ranges, io.sample_rate, wanted, required, buffer_frames)
        .map_err(|message| Error::Unsupported { device: name.to_owned(), message })?;
    if let (Some(asked), cpal::BufferSize::Fixed(got)) = (buffer_frames, choice.buffer)
        && asked != got
    {
        warn!("{name}: buffer of {asked} frames not supported, using {got}");
    }
    Ok(choice)
}

fn stream_config(io: &AudioIo, choice: &StreamChoice) -> StreamConfig {
    StreamConfig {
        channels: choice.channels,
        sample_rate: io.sample_rate,
        buffer_size: choice.buffer,
    }
}

/// Work for a stream's first callback, on the backend's audio thread:
/// raises its priority if `promote`, and records the outcome for the health
/// monitor to report (the callback itself can't log).
struct FirstCallback {
    pending: bool,
    promote: bool,
    stats: Arc<StatsCell>,
}

impl FirstCallback {
    fn new(promote: bool, stats: Arc<StatsCell>) -> Self {
        FirstCallback { pending: true, promote, stats }
    }

    #[inline]
    fn run(&mut self) {
        if self.pending {
            self.pending = false;
            if self.promote {
                self.stats.set_realtime(rt::promote_current_thread());
            }
        }
    }
}

fn build_output<S: DeviceSample>(
    device: &cpal::Device,
    config: StreamConfig,
    mut playout: Playout,
    clock: MediaClock,
    stats: Arc<StatsCell>,
    promote: bool,
) -> std::result::Result<cpal::Stream, cpal::Error> {
    let mut first = FirstCallback::new(promote, stats.clone());
    device.build_output_stream(
        config,
        move |data: &mut [S], _: &cpal::OutputCallbackInfo| {
            first.run();
            playout.render(clock.now_ns(), data)
        },
        move |e| stats.device_error(&e),
        Some(OPEN_TIMEOUT),
    )
}

fn build_input<S: DeviceSample>(
    device: &cpal::Device,
    config: StreamConfig,
    mut capture: Capture,
    clock: MediaClock,
    stats: Arc<StatsCell>,
    promote: bool,
) -> std::result::Result<cpal::Stream, cpal::Error> {
    let mut first = FirstCallback::new(promote, stats.clone());
    device.build_input_stream(
        config,
        move |data: &[S], _: &cpal::InputCallbackInfo| {
            first.run();
            capture.process(clock.now_ns(), data)
        },
        move |e| stats.device_error(&e),
        Some(OPEN_TIMEOUT),
    )
}

fn describe_buffer(choice: &StreamChoice) -> String {
    match choice.buffer {
        cpal::BufferSize::Fixed(n) => format!("{n} frames"),
        cpal::BufferSize::Default => "device default".into(),
    }
}

/// `[01→1 02→2]`-style summary of a map (network channel name, device
/// channel number), for the log.
fn describe_map(map: &[Option<usize>], names: &[String], output: bool) -> String {
    let pairs: Vec<String> = map
        .iter()
        .enumerate()
        .filter_map(|(dev, net)| {
            let net = net.map(|n| names.get(n).cloned().unwrap_or_else(|| format!("#{n}")))?;
            Some(if output {
                format!("{net}→{}", dev + 1)
            } else {
                format!("{}→{net}", dev + 1)
            })
        })
        .collect();
    format!("[{}]", pairs.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lost_or_long_stalled_streams_need_reopening() {
        let ok = DirectionStats { running: true, locked: true, ..Default::default() };
        let mut stats = BridgeStats { output: Some(ok.clone()), input: Some(ok.clone()) };
        assert!(!stats.needs_reopen());
        stats.input =
            Some(DirectionStats { stalled: true, since_callback_ms: 300.0, ..ok.clone() });
        assert!(!stats.needs_reopen(), "a short stall may recover by itself");
        stats.input =
            Some(DirectionStats { stalled: true, since_callback_ms: 1500.0, ..ok.clone() });
        assert!(stats.needs_reopen());
        stats.input = None;
        stats.output = Some(DirectionStats { device_lost: true, ..ok });
        assert!(stats.needs_reopen());
    }

    #[test]
    fn the_monitor_stops_with_its_owner() {
        let probe = Probe {
            stats: Arc::new(StatsCell::new()),
            device: "test".into(),
            channels: 2,
            format: "f32".into(),
        };
        let monitor = Monitor::spawn(vec![("capture", probe)]).expect("spawn");
        std::thread::sleep(HEALTH_INTERVAL * 2);
        let start = Instant::now();
        drop(monitor);
        assert!(start.elapsed() < HEALTH_INTERVAL * 2, "took {:?}", start.elapsed());
    }
}
