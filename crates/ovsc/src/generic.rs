//! The daemon of every backend but `coreaudio`: none, tone, record,
//! loopback and soundcard.
//!
//! Like the `coreaudio` daemon it keeps the device on the network whatever
//! happens around it, and it never runs on silently broken:
//!
//! * the engine (interface, clock, device) is retried until it starts, so
//!   a machine that boots before its network switch waits for it;
//! * the interface is watched ([`crate::netwatch`]): a new address or an
//!   adapter that came back restarts the engine on it;
//! * a receive latency set by a controller or the app restarts the device's
//!   receive side with it at once, as on macOS (`Device::restart_rx`): the
//!   receive flows are requested again and the audio backend reads the
//!   receive rings at the new latency, while the transmit flows, the
//!   backend and the device itself run on;
//! * a sound card whose stream died (unplugged, invalidated by a format
//!   change, a restart of the system's audio service) is reopened, with
//!   back-off, while the device stays on the network ([`BridgeKeeper`]);
//! * a panic anywhere, or a part of the device that stopped, shuts the
//!   daemon down with an error, for its supervisor to restart it
//!   ([`crate::supervise`]).

use std::time::{Duration, Instant};

use anyhow::Context;
use tracing::{debug, error, info, warn};

use ovsc_clock::ptp::{PtpConfig, PtpFollower};
use ovsc_clock::{MediaClock, free_running_clock_with_rate, system_clock};
use ovsc_core::net::Interface;
use ovsc_core::{Device, SavedState, StartOptions};
use ovsc_soundcard::{Bridge, BridgeConfig, BridgeStats};

use crate::config::{self, AppConfig, BackendKind, ClockSource};
use crate::control::{self, Health};
use crate::coreaudio::{RETRY_FIRST, next_retry_delay};
use crate::netwatch::Verdict;
use crate::supervise::Supervision;
use crate::{ClockStateLog, Stop, backend, status_line};

/// Runs the device with `cfg`'s backend until `stop`.
pub async fn run(cfg: AppConfig, sup: Supervision, stop: &mut impl Stop) -> anyhow::Result<()> {
    let Some(mut engine) = start_engine(&cfg, None, sup, stop).await? else { return Ok(()) };
    let mut generation = 0;
    let mut audio = match Audio::start(&cfg, &engine.device, generation).await {
        Ok(audio) => audio,
        Err(e) => {
            engine.shutdown().await;
            return Err(e);
        }
    };
    loop {
        let next = run_engine(&cfg, &engine, &mut audio, sup, stop).await;
        audio.stop();
        let next = match next {
            Ok(next) => next,
            Err(e) => {
                error!("{e:#}");
                engine.shutdown().await;
                return Err(e);
            }
        };
        engine = match next {
            Next::Stop(why) => {
                info!("{why}: shutting down");
                engine.shutdown().await;
                return Ok(());
            }
            Next::RestartEngine => {
                let carry = Carry::from(&engine.device);
                engine.shutdown().await;
                match start_engine(&cfg, Some(carry), sup, stop).await? {
                    Some(engine) => engine,
                    None => return Ok(()),
                }
            }
        };
        generation += 1;
        audio = match Audio::start(&cfg, &engine.device, generation).await {
            Ok(audio) => audio,
            Err(e) => {
                engine.shutdown().await;
                return Err(e);
            }
        };
    }
}

/// Why [`run_engine`] returned.
#[derive(Debug, PartialEq, Eq)]
enum Next {
    Stop(&'static str),
    /// The network interface changed under the engine.
    RestartEngine,
}

/// Runs until a stop or an interface change, supervising the audio
/// backend, following the device's health, applying new receive latencies
/// in place and logging the status every 30 s. A fault is an error.
async fn run_engine(
    cfg: &AppConfig,
    engine: &Engine,
    audio: &mut Audio,
    sup: Supervision,
    stop: &mut impl Stop,
) -> anyhow::Result<Next> {
    // Each run follows a start or a restart: the device's counters start
    // over, and so does the clock's age, which only tells the tracker that
    // the clock has to be seen locked again (it is at once, if it is).
    let started = Instant::now();
    let mut health = Health::new();
    health.restarted(started);
    let mut status = tokio::time::interval(Duration::from_secs(30));
    status.tick().await;
    let mut second = tokio::time::interval(Duration::from_secs(1));
    let mut clock_log = ClockStateLog::default();
    let mut latency = engine.device.latency_requests();
    latency.mark_unchanged();
    // A latency set between the device's start and this subscription
    // was not seen as a change: apply it now. The backend follows it by
    // itself (`AudioIo::latency_samples`).
    engine.device.restart_rx();
    let mut watch = sup.probe.watch_device(engine.device.info());
    loop {
        tokio::select! {
            r = stop.requested() => return Ok(Next::Stop(r?)),
            _ = status.tick() => {
                let network = watch.note();
                info!(
                    "{}{network}; {}",
                    status_line(&engine.device, &engine.clock),
                    health.summary()
                );
                audio.log_status();
            }
            _ = second.tick() => {
                clock_log.poll(&engine.clock);
                if let Some(fault) = sup.fault(Some(&engine.device)) {
                    anyhow::bail!("{fault}: shutting down");
                }
                control::watch_device(
                    &mut health,
                    cfg,
                    &engine.device,
                    &engine.clock,
                    started.elapsed(),
                );
                if let Verdict::Restart(why) = watch.poll(sup.probe, &engine.clock) {
                    warn!("{why}: restarting the network engine");
                    return Ok(Next::RestartEngine);
                }
                audio.supervise(&engine.device).await;
            }
            Ok(()) = latency.changed() => {
                engine.device.restart_rx();
            }
        }
    }
}

/// Starts the engine, retrying until it runs (the network may come up
/// late), from what the engine it replaces `carried` over. `None` if a stop
/// came first.
async fn start_engine(
    cfg: &AppConfig,
    carried: Option<Carry>,
    sup: Supervision,
    stop: &mut impl Stop,
) -> anyhow::Result<Option<Engine>> {
    let mut delay = RETRY_FIRST;
    let mut attempts = 0u32;
    let mut last_error = String::new();
    loop {
        attempts += 1;
        match Engine::start(cfg, carried.clone()).await {
            Ok(engine) => {
                if attempts > 1 {
                    info!("engine started at attempt {attempts}");
                }
                return Ok(Some(engine));
            }
            Err(e) => {
                let e = format!("{e:#}");
                // Once per distinct cause: a machine can wait long for its
                // network.
                if e != last_error {
                    warn!("cannot start the engine: {e}; retrying until it starts");
                } else {
                    debug!("cannot start the engine: {e}");
                }
                last_error = e;
            }
        }
        if let Some(fault) = sup.fault(None) {
            anyhow::bail!("{fault}: shutting down");
        }
        tokio::select! {
            r = stop.requested() => {
                info!("{}: shutting down", r?);
                return Ok(None);
            }
            () = tokio::time::sleep(delay) => {}
        }
        delay = next_retry_delay(delay);
    }
}

/// What a device hands on to the one that replaces it: the names and
/// subscriptions set since the start, kept even without a state file, and
/// the name it announced, kept without probing for it again (see
/// [`StartOptions::announced`]).
#[derive(Clone)]
struct Carry {
    state: SavedState,
    announced: Option<String>,
}

impl From<&Device> for Carry {
    fn from(device: &Device) -> Self {
        Carry { state: device.saved_state(), announced: device.announced_name() }
    }
}

/// The device and its clock.
struct Engine {
    device: Device,
    clock: MediaClock,
    /// The PTP follower feeding `clock`, when it follows the network.
    follower: Option<PtpFollower>,
}

impl Engine {
    async fn start(cfg: &AppConfig, carried: Option<Carry>) -> anyhow::Result<Engine> {
        let iface = Interface::find(&cfg.device.interface)?;
        let (clock, follower) = match cfg.clock.source {
            ClockSource::System => {
                warn!("using the system clock: audio will not be in sync with a Dante network");
                (system_clock(), None)
            }
            ClockSource::Free => {
                warn!("using a free-running clock: audio will not be in sync with a Dante network");
                let rate = config::rate_from_ppm(cfg.clock.free_rate_ppm)?;
                (free_running_clock_with_rate(rate), None)
            }
            ClockSource::Ptp => {
                let mut ptp = PtpConfig::new(iface.ip, iface.mac);
                ptp.event_port = cfg.clock.ptp_event_port;
                ptp.general_port = cfg.clock.ptp_general_port;
                ptp.secondary = config::ptp_secondary(&cfg.device);
                let (follower, clock) =
                    PtpFollower::start(ptp).await.context("starting the PTP clock follower")?;
                (clock, Some(follower))
            }
        };
        match start_device(cfg, &clock, carried).await {
            Ok(device) => Ok(Engine { device, clock, follower }),
            Err(e) => {
                if let Some(f) = follower {
                    f.shutdown();
                }
                Err(e)
            }
        }
    }

    async fn shutdown(self) {
        self.device.shutdown().await;
        if let Some(f) = self.follower {
            f.shutdown();
        }
    }
}

async fn start_device(
    cfg: &AppConfig,
    clock: &MediaClock,
    carry: Option<Carry>,
) -> anyhow::Result<Device> {
    let (state, announced) = match carry {
        Some(c) => (Some(c.state), c.announced),
        None => (None, None),
    };
    let options = StartOptions { state, announced, ..Default::default() };
    Ok(Device::start_with_options(cfg.device.clone(), clock.clone(), options).await?)
}

/// The audio backend of the running device.
enum Audio {
    None,
    Thread(backend::Running),
    Bridge(Box<BridgeKeeper>),
}

impl Audio {
    /// Starts `cfg`'s backend on `device`. `generation` counts the engine
    /// restarts: each recording goes to a file of its own.
    async fn start(cfg: &AppConfig, device: &Device, generation: u32) -> anyhow::Result<Audio> {
        let a = &cfg.audio;
        let io = device.audio();
        Ok(match a.backend {
            BackendKind::None => Audio::None,
            BackendKind::Tone => Audio::Thread(backend::tone(io, a.tone_hz, a.tone_level_db)),
            BackendKind::Loopback => Audio::Thread(backend::loopback(io)),
            BackendKind::Record => {
                let path = backend::numbered(&a.record_path, generation);
                Audio::Thread(backend::record(io, &path)?)
            }
            BackendKind::Soundcard => {
                let mut keeper = BridgeKeeper::new(BridgeConfig {
                    output_device: a.output_device.clone(),
                    input_device: a.input_device.clone(),
                    buffer_frames: a.buffer_frames,
                    output_map: config::channel_map(&a.output_channels),
                    input_map: config::channel_map(&a.input_channels),
                    margin_ms: a.margin_ms,
                });
                // Opened at once; if it cannot be, the device stays on the
                // network and the bridge is retried.
                keeper.supervise(device, Instant::now()).await;
                Audio::Bridge(Box::new(keeper))
            }
            BackendKind::CoreAudio => unreachable!("served by coreaudio::run"),
        })
    }

    /// Checks the sound card stream, reopening it if it died.
    async fn supervise(&mut self, device: &Device) {
        if let Audio::Bridge(keeper) = self {
            keeper.supervise(device, Instant::now()).await;
        }
    }

    fn log_status(&self) {
        let Audio::Bridge(keeper) = self else { return };
        let Some(bridge) = &keeper.bridge else {
            warn!("soundcard: closed; reopening: {}", keeper.reopen.last_error);
            return;
        };
        let stats = bridge.stats();
        for (what, d) in [("playback", stats.output), ("capture", stats.input)] {
            if let Some(d) = d {
                info!(
                    "{what} {}: {}, {:+.1} ppm, error {:.1} samples, {} underruns, {} realigns{}",
                    d.device,
                    if d.locked {
                        "locked"
                    } else if d.running {
                        "locking"
                    } else {
                        "starting"
                    },
                    d.ratio_ppm,
                    d.error_samples,
                    d.underruns,
                    d.realigns,
                    if d.device_lost { ", DEVICE LOST" } else { "" }
                );
            }
        }
    }

    fn stop(&mut self) {
        match std::mem::replace(self, Audio::None) {
            Audio::None => {}
            Audio::Thread(t) => t.stop(),
            Audio::Bridge(keeper) => drop(keeper),
        }
    }
}

/// How many 1 s checks in a row a stream may go without a callback before
/// it counts as dead. Device callbacks come every few milliseconds; a
/// little slack covers a stream that is slow to start.
const STALL_CHECKS: u32 = 3;

/// The wait before reopening a lost sound card; each failure waits a second
/// longer, up to [`REOPEN_MAX`].
const REOPEN_FIRST: Duration = Duration::from_secs(1);
const REOPEN_MAX: Duration = Duration::from_secs(5);

/// Judges a sound card bridge's health from its statistics, one check a
/// second. The audio backend can end a stream for good without the device
/// disappearing (cpal pauses a Core Audio stream whose device changed its
/// sample rate; WASAPI ends its loop on any error), so a stream whose
/// callbacks stopped counts as dead too.
#[derive(Debug, Default)]
struct BridgeHealth {
    /// Per direction (playback, capture): the callback count at the last
    /// check, and the checks since it last moved.
    last: [Option<(u64, u32)>; 2],
}

impl BridgeHealth {
    /// Why the bridge must be reopened, if it must.
    fn check(&mut self, stats: &BridgeStats) -> Option<String> {
        let halves = [("playback", stats.output.as_ref()), ("capture", stats.input.as_ref())];
        let mut dead = None;
        for ((what, d), last) in halves.into_iter().zip(&mut self.last) {
            let Some(d) = d else { continue };
            if d.device_lost {
                dead.get_or_insert_with(|| format!("the {what} device {} is gone", d.device));
            }
            let stalled = match *last {
                Some((callbacks, n)) if callbacks == d.callbacks => n + 1,
                _ => 0,
            };
            *last = Some((d.callbacks, stalled));
            if stalled >= STALL_CHECKS {
                dead.get_or_insert_with(|| {
                    format!(
                        "the {what} stream on {} stopped (no callbacks for {stalled} s)",
                        d.device
                    )
                });
            }
        }
        dead
    }
}

/// When to try opening a closed bridge again.
#[derive(Debug)]
struct Reopen {
    /// The next attempt is due then.
    at: Instant,
    delay: Duration,
    attempts: u32,
    /// Why the last attempt failed (or the bridge closed).
    last_error: String,
}

impl Reopen {
    /// Opening is due at `at`, with no failure yet.
    fn at(at: Instant, why: String) -> Reopen {
        Reopen { at, delay: REOPEN_FIRST, attempts: 0, last_error: why }
    }

    fn due(&self, now: Instant) -> bool {
        now >= self.at
    }

    /// An attempt at `now` failed with `error`: the next is due after a
    /// longer wait. Whether the error is a new one, worth a warning.
    fn failed(&mut self, now: Instant, error: String) -> bool {
        self.attempts += 1;
        self.at = now + self.delay;
        self.delay = (self.delay + Duration::from_secs(1)).min(REOPEN_MAX);
        let new = error != self.last_error;
        self.last_error = error;
        new
    }
}

/// Keeps a sound card bridge open: reopens it, with back-off, when it dies
/// or could not be opened, while the device stays on the network.
struct BridgeKeeper {
    cfg: BridgeConfig,
    bridge: Option<Bridge>,
    health: BridgeHealth,
    reopen: Reopen,
}

impl BridgeKeeper {
    fn new(cfg: BridgeConfig) -> BridgeKeeper {
        BridgeKeeper {
            cfg,
            bridge: None,
            health: BridgeHealth::default(),
            reopen: Reopen::at(Instant::now(), String::new()),
        }
    }

    /// One check at `now`: closes a dead bridge, and opens a closed one
    /// when its next attempt is due.
    async fn supervise(&mut self, device: &Device, now: Instant) {
        if let Some(bridge) = &self.bridge {
            let Some(why) = self.health.check(&bridge.stats()) else { return };
            warn!("soundcard: {why}; reopening it");
            self.bridge = None;
            self.health = BridgeHealth::default();
            self.reopen = Reopen::at(now + REOPEN_FIRST, why);
            return;
        }
        if !self.reopen.due(now) {
            return;
        }
        let (io, cfg) = (device.audio(), self.cfg.clone());
        // Opening a device can take seconds (OPEN_TIMEOUT in the bridge):
        // not on the runtime's threads.
        let opened = tokio::task::spawn_blocking(move || Bridge::start(io, &cfg))
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r.map_err(|e| e.to_string()));
        match opened {
            Ok(bridge) => {
                if self.reopen.attempts > 0 || !self.reopen.last_error.is_empty() {
                    info!("soundcard: bridge open again (attempt {})", self.reopen.attempts + 1);
                }
                self.bridge = Some(bridge);
                self.reopen = Reopen::at(now, String::new());
            }
            Err(e) => {
                let delay = self.reopen.delay;
                if self.reopen.failed(now, e.clone()) {
                    warn!(
                        "soundcard: cannot open the bridge: {e}; retrying every {} s while the \
                         device stays on the network",
                        delay.as_secs().max(1)
                    );
                } else {
                    debug!("soundcard: cannot open the bridge: {e}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use ovsc_core::{Channels, DeviceConfig, Ports};
    use ovsc_proto::arc;
    use ovsc_proto::frame::Frame;
    use ovsc_soundcard::DirectionStats;
    use tokio::sync::mpsc;

    use super::*;
    use crate::netwatch::{Probe, Seen};

    fn half(callbacks: u64, device_lost: bool) -> Option<DirectionStats> {
        Some(DirectionStats {
            device: "USB Audio".into(),
            callbacks,
            device_lost,
            ..Default::default()
        })
    }

    #[test]
    fn a_lost_device_needs_a_reopen() {
        let mut h = BridgeHealth::default();
        let ok = BridgeStats { output: half(10, false), input: half(10, false) };
        assert_eq!(h.check(&ok), None);
        let lost = BridgeStats { output: half(20, false), input: half(20, true) };
        let why = h.check(&lost).unwrap();
        assert_eq!(why, "the capture device USB Audio is gone");
    }

    #[test]
    fn a_stream_without_callbacks_needs_a_reopen() {
        let mut h = BridgeHealth::default();
        let at = |playback, capture| BridgeStats { output: half(playback, false), input: capture };
        // Callbacks flow, then playback stops while capture goes on.
        for n in 1..=5 {
            assert_eq!(h.check(&at(n * 100, half(n * 100, false))), None);
        }
        for n in 0..STALL_CHECKS - 1 {
            assert_eq!(h.check(&at(500, half(600 + u64::from(n), false))), None);
        }
        let why = h.check(&at(500, half(700, false))).unwrap();
        assert!(why.starts_with("the playback stream on USB Audio stopped"), "{why}");
        // A direction that is not open is not judged.
        let mut h = BridgeHealth::default();
        for n in 1..10 {
            assert_eq!(h.check(&at(n * 100, None)), None);
        }
    }

    #[test]
    fn reopening_backs_off_to_five_seconds() {
        let start = Instant::now();
        let mut r = Reopen::at(start, "gone".into());
        assert!(r.due(start));
        let mut now = start;
        let mut waits = Vec::new();
        for i in 0..6 {
            assert!(r.due(now));
            let new = r.failed(now, "busy".into());
            assert_eq!(new, i == 0, "only a new error is worth a warning");
            waits.push((r.at - now).as_secs());
            assert!(!r.due(now));
            now = r.at;
        }
        assert_eq!(waits, [1, 2, 3, 4, 5, 5]);
        assert_eq!(r.attempts, 6);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_missing_sound_card_leaves_the_device_on_the_network() {
        let cfg = DeviceConfig {
            name: "gen-bridge".into(),
            interface: "127.0.0.1".into(),
            ports: Ports { arc: 26130, cmc: 26131, flow_control: 26132, settings: 26133 },
            discovery: false,
            process_id: 62,
            ..Default::default()
        };
        let device = Device::start(cfg, system_clock()).await.unwrap();
        let mut keeper = BridgeKeeper::new(BridgeConfig {
            output_device: Some("no such sound card, anywhere".into()),
            ..Default::default()
        });
        let start = Instant::now();
        keeper.supervise(&device, start).await;
        assert!(keeper.bridge.is_none());
        assert_eq!(keeper.reopen.attempts, 1);
        // Not retried before its time, then retried.
        keeper.supervise(&device, start).await;
        assert_eq!(keeper.reopen.attempts, 1);
        keeper.supervise(&device, start + REOPEN_FIRST).await;
        assert_eq!(keeper.reopen.attempts, 2);
        device.shutdown().await;
    }

    fn app_config(base: u16) -> AppConfig {
        let device = DeviceConfig {
            name: "gen-dev".into(),
            interface: "127.0.0.1".into(),
            tx_channels: Channels::Count(2),
            rx_channels: Channels::Count(2),
            latency_ms: 4.0,
            ports: Ports { arc: base, cmc: base + 1, flow_control: base + 2, settings: base + 3 },
            discovery: false,
            process_id: 63,
            ..Default::default()
        };
        let mut cfg = AppConfig { device, ..Default::default() };
        cfg.clock.source = ClockSource::System;
        cfg.audio.backend = BackendKind::Loopback;
        cfg
    }

    fn arc_addr(base: u16) -> std::net::SocketAddr {
        std::net::SocketAddr::from(([127, 0, 0, 1], base))
    }

    /// Asks the device's ARC server, as Dante Controller does.
    async fn ask(base: u16, packet: Vec<u8>) -> Option<Vec<u8>> {
        let local = std::net::Ipv4Addr::LOCALHOST;
        let t = Duration::from_millis(300);
        ovsc_core::client::transact(local, arc_addr(base), &packet, t, 3).await.ok()
    }

    /// Whether the running device receives at `ms` (its RX_LATENCY).
    async fn runs_at(base: u16, ms: u32) -> bool {
        let seq = ovsc_core::client::next_seq();
        let request = arc::encode_simple_request(seq, arc::opcode::PROPERTIES_1100);
        let Some(r) = ask(base, request).await else { return false };
        let Ok(frame) = Frame::parse(&r) else { return false };
        arc::decode_properties_response(&frame)
            .is_ok_and(|p| p.contains(&(arc::property::RX_LATENCY, Some(ms * 1_000_000))))
    }

    async fn eventually_runs_at(base: u16, ms: u32, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !runs_at(base, ms).await {
            assert!(Instant::now() < deadline, "{what}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    static NO_PANIC: AtomicBool = AtomicBool::new(false);

    /// The interface the fake probe shows: 127.0.0.1 until `MOVED`.
    static MOVED: AtomicBool = AtomicBool::new(false);

    fn fake_probe(_name: &str) -> std::io::Result<Option<Seen>> {
        let ip = if MOVED.load(std::sync::atomic::Ordering::SeqCst) {
            std::net::Ipv4Addr::new(127, 0, 0, 9)
        } else {
            std::net::Ipv4Addr::LOCALHOST
        };
        Ok(Some(Seen { index: 1, ipv4: vec![ip] }))
    }

    async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !check() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Whether `engine`'s device reports another device announcing its
    /// name, without having renamed itself.
    fn live_conflict(engine: &Engine) -> bool {
        engine.device.name_conflict().is_some_and(|c| c.renamed_to.is_none())
    }

    /// A new latency, and a restart of the whole engine, keep the name the
    /// device announced while another device announces it too: the first
    /// restarts nothing that announces, the second hands the name on
    /// rather than probe for it again, so the device never renames itself
    /// mid-show. The conflict is reported.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restarts_keep_the_announced_name_through_a_live_conflict() {
        let name = format!("gen-keep{}", std::process::id());
        let mut cfg = app_config(26150);
        cfg.device.name = name.clone();
        cfg.device.discovery = true;
        let engine = Engine::start(&cfg, None).await.unwrap();
        eventually("the name", || engine.device.announced_name().is_some()).await;
        // Another device that announces the name and answers for it,
        // without having probed for it: as two machines given one name
        // that started together would.
        let holder_cfg = DeviceConfig {
            name: name.clone(),
            interface: "127.0.0.1".into(),
            ports: Ports { arc: 26160, cmc: 26161, flow_control: 26162, settings: 26163 },
            process_id: 99,
            ..Default::default()
        };
        let options = StartOptions { announced: Some(name.clone()), ..Default::default() };
        let holder = Device::start_with_options(holder_cfg, system_clock(), options).await.unwrap();
        eventually("the conflict", || live_conflict(&engine)).await;

        engine.device.request_latency(2_000_000).unwrap();
        assert!(engine.device.restart_rx());
        // Longer than a probe takes.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(engine.device.name(), name);
        assert!(live_conflict(&engine));

        let carry = Carry::from(&engine.device);
        engine.shutdown().await;
        let engine = Engine::start(&cfg, Some(carry)).await.unwrap();
        eventually("the conflict after the engine restart", || live_conflict(&engine)).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(engine.device.name(), name);
        assert_eq!(engine.device.saved_state().name.as_deref(), Some(name.as_str()));
        engine.shutdown().await;
        holder.shutdown().await;
    }

    /// A controller's latency restarts only the device's receive side: the
    /// device, and with it the audio backend, runs on at the new latency.
    /// A new address restarts the whole engine.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_controllers_latency_restarts_the_receive_side_and_a_new_address_the_engine() {
        let base = 26140;
        let dir = std::env::temp_dir().join(format!("ovsc-generic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = app_config(base);
        // Each start of the device opens a new recording: they count the
        // restarts.
        cfg.audio.backend = BackendKind::Record;
        cfg.audio.record_path = dir.join("take.wav");
        let takes =
            |n: u32| dir.join(if n == 1 { "take.wav".into() } else { format!("take-{n}.wav") });
        let count = || std::fs::read_dir(&dir).unwrap().count();
        let sup = Supervision { probe: Probe::Fake(fake_probe), panicked: &NO_PANIC };
        let (stop_tx, mut stop) = mpsc::unbounded_channel();
        let daemon = run(cfg, sup, &mut stop);
        let test = async {
            eventually("the first recording", || takes(1).exists()).await;
            eventually_runs_at(base, 4, "the device did not start at 4 ms").await;

            // A controller sets 2 ms: the device runs at 2 ms, and the
            // recording goes on.
            let seq = ovsc_core::client::next_seq();
            let r = ask(base, arc::encode_set_latency_request(seq, 2_000_000, 16)).await.unwrap();
            let result = Frame::parse(&r).unwrap().header.result;
            assert_eq!(result, ovsc_proto::frame::result::SUCCESS);
            eventually_runs_at(base, 2, "the device did not change to 2 ms").await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            assert_eq!(count(), 1, "the device restarted");

            // The interface's address moves: the engine restarts (and goes
            // on restarting while the address is elsewhere). Back on it,
            // the engine runs on at 2 ms and stays.
            MOVED.store(true, std::sync::atomic::Ordering::SeqCst);
            eventually("a restart on the new address", || takes(2).exists()).await;
            MOVED.store(false, std::sync::atomic::Ordering::SeqCst);
            eventually_runs_at(base, 2, "the engine did not come back at 2 ms").await;
            let settled = count();
            tokio::time::sleep(Duration::from_millis(3500)).await;
            assert_eq!(count(), settled, "still restarting");
            eventually_runs_at(base, 2, "the engine stopped").await;
            stop_tx.send("test").unwrap();
        };
        let (result, ()) = tokio::join!(daemon, test);
        result.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
