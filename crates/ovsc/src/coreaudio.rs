//! The `coreaudio` backend (design section 14.4): the daemon behind the
//! OpenVirtualSoundcard Core Audio device on macOS.
//!
//! The device's driver runs inside Core Audio's driver helper and connects
//! to this process over the XPC Mach service `[coreaudio] service`. The two
//! share one memory region, which holds the device engine's channel rings
//! and a copy of its media clock (crate `ovsc-hal-server`).
//!
//! Startup order:
//!
//! 1. The region and the driver service come first, so the driver can
//!    attach and show the configured layout before the network is up.
//! 2. Then the engine: the network interface, the clock (mirrored into the
//!    region) and the device, on the region's rings. It is retried every
//!    few seconds until it runs. The daemon never exits for want of a
//!    network: launchd would restart it with a new region, which the driver
//!    would have to swap in every time.
//! 3. It runs until Ctrl-C or SIGTERM, then says goodbye to the driver
//!    before stopping the engine.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tokio::time::{Instant, Interval, MissedTickBehavior};
use tracing::{debug, error, info, warn};

use ovsc_clock::ptp::{PtpConfig, PtpFollower};
use ovsc_clock::{MediaClock, free_running_clock_with_rate};
use ovsc_control::{
    PlaybackStatus, PlaybackTarget, RecordingStatus, Request, Response, Restart, SettingsChange,
};
use ovsc_core::net::Interface;
use ovsc_core::{Device, FormatRequest, StartOptions};
use ovsc_hal_server::{EngineInfo, HalRegion, HalServer, ShmClockMirror};
use ovsc_ipc::transport::ServerTransport;

use crate::config::{self, AppConfig, ClockSource};
use crate::control::{self, Control};
use crate::{ClockStateLog, Stop, StopSignals, status_line};

/// The daemon version in the region header.
const VERSION: &str = concat!("ovsc ", env!("CARGO_PKG_VERSION"));

/// The wait after the first failed engine start; each further failure
/// waits a second longer, up to [`RETRY_MAX`].
const RETRY_FIRST: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(5);

/// How to start the daemon so that launchd gives it the driver's service.
const LAUNCHD_HINT: &str =
    "sudo launchctl bootstrap system /Library/LaunchDaemons/org.openvirtualsoundcard.daemon.plist";

/// Runs the daemon of the Core Audio device until Ctrl-C or SIGTERM, or
/// until the OpenVirtualSoundcard app changed settings that need a restart: launchd
/// starts it again then. `config_path` is the configuration file the app's
/// changes go to.
pub async fn run(cfg: AppConfig, config_path: Option<PathBuf>) -> anyhow::Result<()> {
    cfg.validate()?;
    let mut stop = StopSignals::new()?;
    let service = transport(&cfg.coreaudio.service)?;
    serve(cfg, config_path, service, &mut stop).await
}

/// Why [`run_engine`] or [`start_engine`] returned.
enum Next {
    /// A stop was requested, for this reason.
    Stop(&'static str),
    /// A controller or the app set a new latency: restart the device.
    RestartDevice,
    /// The app saved settings that take a new daemon.
    RestartDaemon,
}

/// The driver service's transport, and a check of whether its listener
/// turned out invalid.
struct Service {
    transport: Box<dyn ServerTransport>,
    listener_invalid: Box<dyn Fn() -> bool + Send>,
}

/// The XPC listener for the Mach service `service`.
#[cfg(target_os = "macos")]
fn transport(service: &str) -> anyhow::Result<Service> {
    use std::io;

    use ovsc_ipc::protocol::ToPlugin;
    use ovsc_ipc::transport::ServerHandler;
    use ovsc_ipc::xpc::XpcServer;

    /// The listener, shared with the check of its state.
    struct Shared(Arc<XpcServer>);

    impl ServerTransport for Shared {
        fn start(&self, h: Arc<dyn ServerHandler>) -> io::Result<()> {
            self.0.start(h)
        }

        fn send(&self, peer: u64, m: ToPlugin) {
            self.0.send(peer, m)
        }
    }

    let server = Arc::new(XpcServer::new(service));
    // Weak, so that the service still closes when the HalServer drops it.
    let weak = Arc::downgrade(&server);
    Ok(Service {
        transport: Box::new(Shared(server)),
        listener_invalid: Box::new(move || weak.upgrade().is_some_and(|s| s.listener_invalid())),
    })
}

#[cfg(not(target_os = "macos"))]
fn transport(_service: &str) -> anyhow::Result<Service> {
    anyhow::bail!("the coreaudio backend needs macOS")
}

/// Serves the driver on `service` and runs the engine until `stop`.
async fn serve(
    cfg: AppConfig,
    config_path: Option<PathBuf>,
    service: Service,
    stop: &mut impl Stop,
) -> anyhow::Result<()> {
    let mut opts = cfg.coreaudio.hal_options();
    // The daemon's status line carries the driver's status, both while it
    // waits for the engine and while the engine runs; the server's own line
    // would repeat it.
    let log_every = std::mem::take(&mut opts.status_log_interval);
    if opts.debug_zts_jitter_ns != 0 {
        warn!(
            "coreaudio.debug_zts_jitter_ns = {}: the driver jitters its timestamps on purpose \
             (tests only)",
            opts.debug_zts_jitter_ns
        );
    }
    let initial = EngineInfo::from_config(&cfg.device)?;
    let region =
        HalRegion::create(VERSION).context("creating the region shared with the driver")?;
    let server = HalServer::start(opts, region.clone(), service.transport, initial)
        .with_context(|| format!("starting the driver service {}", cfg.coreaudio.service))?;
    let mut listener = ListenerCheck {
        invalid: service.listener_invalid,
        service: cfg.coreaudio.service.clone(),
        reported: false,
    };
    let mut status = StatusTimer::new(log_every);
    let mut control = Control::listen(&cfg.coreaudio.control_socket);
    let mut ctx = Ctx {
        cfg: &cfg,
        config_path: config_path.as_deref(),
        region: &region,
        server: &server,
        listener: &mut listener,
        status: &mut status,
        control: &mut control,
        recorder: None,
        recording: RecordingStatus::default(),
        player: None,
    };

    let mut engine = match start_engine(&mut ctx, stop).await {
        Ok(Ok(engine)) => engine,
        Ok(Err(next)) => {
            log_next(&next);
            server.shutdown().await;
            return Ok(());
        }
        Err(e) => {
            server.shutdown().await;
            return Err(e);
        }
    };
    server.engine_started(&engine.device);
    let result = loop {
        match run_engine(&engine, &mut ctx, stop).await {
            Ok(Next::RestartDevice) => {
                info!(
                    "restarting the network engine for a receive latency of {} ms",
                    engine.device.configured_latency_ns() as f64 / 1e6
                );
                ctx.stop_recording();
                ctx.player = None;
                server.engine_stopped();
                engine = match engine.restart_device(&cfg, &region).await {
                    Ok(engine) => engine,
                    Err(e) => {
                        error!("cannot restart the network engine: {e:#}");
                        match start_engine(&mut ctx, stop).await {
                            Ok(Ok(engine)) => engine,
                            Ok(Err(next)) => {
                                log_next(&next);
                                server.shutdown().await;
                                return Ok(());
                            }
                            Err(e) => {
                                server.shutdown().await;
                                return Err(e);
                            }
                        }
                    }
                };
                server.engine_started(&engine.device);
            }
            Ok(next) => {
                log_next(&next);
                break Ok(());
            }
            Err(e) => {
                error!("shutting down: {e:#}");
                break Err(e);
            }
        }
    };
    ctx.stop_recording();
    ctx.player = None;
    server.shutdown().await;
    engine.shutdown().await;
    result
}

fn log_next(next: &Next) {
    match next {
        Next::Stop(why) => info!("{why}: shutting down"),
        Next::RestartDaemon => info!("restarting to apply the new settings"),
        Next::RestartDevice => {}
    }
}

/// What the engine loops share.
struct Ctx<'a> {
    cfg: &'a AppConfig,
    config_path: Option<&'a Path>,
    region: &'a Arc<HalRegion>,
    server: &'a HalServer,
    listener: &'a mut ListenerCheck,
    status: &'a mut StatusTimer,
    control: &'a mut Control,
    recorder: Option<crate::backend::Running>,
    recording: RecordingStatus,
    player: Option<crate::playback::Player>,
}

impl Ctx<'_> {
    /// Answers a control call; whether the daemon must restart.
    fn answer(&mut self, call: control::Call, engine: Option<&Engine>, error: &str) -> bool {
        let response = match &call.request {
            Request::AddMarker { label } => Some(
                match self
                    .recorder
                    .as_ref()
                    .context("recording is not running")
                    .and_then(|r| r.add_marker(label))
                {
                    Ok(()) => Response::Recording(self.recording_status()),
                    Err(e) => Response::Error { message: format!("{e:#}") },
                },
            ),
            Request::PlaybackStatus => Some(Response::Playback(
                self.player.as_ref().map_or_else(PlaybackStatus::default, |p| p.status()),
            )),
            Request::UnloadPlayback => {
                self.player = None;
                Some(Response::Playback(PlaybackStatus::default()))
            }
            Request::OpenPlayback { path, target } => {
                let result = (|| -> anyhow::Result<()> {
                    let device = &engine.context("the network engine is not running")?.device;
                    anyhow::ensure!(
                        !self.recording_status().recording
                            || self.recording_status().path.as_deref() != Some(path),
                        "stop this recording before opening it for playback"
                    );
                    let file = control::playback_file(Path::new(path), call.uid)?;
                    // Parse and validate before releasing an existing soundcheck.
                    let wave = crate::playback::Wave::open(file.try_clone()?)?;
                    anyhow::ensure!(
                        wave.rate == device.info().sample_rate,
                        "set the device to the WAV sample rate ({} Hz) before opening it",
                        wave.rate
                    );
                    use std::io::Seek;
                    let mut file = file;
                    file.rewind()?;
                    self.player = None;
                    self.player = Some(crate::playback::Player::open(device, file, path, *target)?);
                    Ok(())
                })();
                Some(match result {
                    Ok(()) => Response::Playback(self.player.as_ref().unwrap().status()),
                    Err(e) => Response::Error { message: format!("{e:#}") },
                })
            }
            Request::Play
            | Request::Pause
            | Request::StopPlayback
            | Request::SeekPlayback { .. }
            | Request::ConfigurePlayback { .. } => {
                let result = (|| -> anyhow::Result<()> {
                    let device = &engine.context("the network engine is not running")?.device;
                    let player = self.player.as_ref().context("open a recording first")?;
                    let outputs = match player.status().target {
                        PlaybackTarget::Receive => device.audio().rx.len(),
                        PlaybackTarget::Transmit => device.audio().tx.len(),
                    };
                    player.command(&call.request, outputs)
                })();
                Some(match result {
                    Ok(()) => Response::Playback(self.player.as_ref().unwrap().status()),
                    Err(e) => Response::Error { message: format!("{e:#}") },
                })
            }
            Request::RecordingStatus => Some(Response::Recording(self.recording_status())),
            Request::StopRecording => {
                self.stop_recording();
                Some(Response::Recording(self.recording_status()))
            }
            Request::StartRecording { path } => {
                let result = (|| -> anyhow::Result<()> {
                    anyhow::ensure!(
                        !self.recording_status().recording,
                        "a recording is already running"
                    );
                    self.stop_recording();
                    let engine = engine.context("the network engine is not running")?;
                    let path = Path::new(path);
                    let file = control::recording_file(path, call.uid)?;
                    self.recorder =
                        Some(crate::backend::record_file(engine.device.audio(), path, file)?);
                    Ok(())
                })();
                Some(match result {
                    Ok(()) => Response::Recording(self.recording_status()),
                    Err(e) => Response::Error { message: format!("{e:#}") },
                })
            }
            _ => None,
        };
        if let Some(response) = response {
            let _ = call.reply.send(response);
            return false;
        }
        let view = control::View {
            cfg: self.cfg,
            config_path: self.config_path,
            engine: engine.map(|e| (&e.device, &e.clock, e.started.elapsed())),
            hal: self.server.status(),
            engine_error: Some(error),
        };
        control::answer(call, &view) == control::After::RestartDaemon
    }

    fn recording_status(&self) -> RecordingStatus {
        self.recorder.as_ref().map_or_else(|| self.recording.clone(), |r| r.recording_status())
    }

    fn stop_recording(&mut self) {
        if let Some(recorder) = self.recorder.take() {
            self.recording = recorder.finish_recording();
        }
    }

    /// Saves a sample rate or bit depth that a controller asked for;
    /// whether the daemon must restart to apply it.
    fn apply_format(&self, engine: &Engine, request: FormatRequest) -> bool {
        let view = control::View {
            cfg: self.cfg,
            config_path: self.config_path,
            engine: Some((&engine.device, &engine.clock, engine.started.elapsed())),
            hal: self.server.status(),
            engine_error: None,
        };
        let change = SettingsChange {
            sample_rate: request.sample_rate.filter(|&r| r != self.cfg.device.sample_rate),
            bits_per_sample: request
                .bits_per_sample
                .filter(|&b| b != self.cfg.device.bits_per_sample),
            ..Default::default()
        };
        if change.is_empty() {
            return false;
        }
        match control::apply(&view, &change) {
            Ok(restart) => {
                info!("a controller changed the format: {change:?}");
                restart == Restart::Daemon
            }
            Err(e) => {
                error!("cannot apply {change:?} from a controller: {e:#}");
                false
            }
        }
    }
}

/// Starts the engine, retrying until it runs, or returns why it stopped
/// trying first. While it waits, the status line carries the driver's
/// status and the last failure, and the app's calls are answered.
async fn start_engine(
    ctx: &mut Ctx<'_>,
    stop: &mut impl Stop,
) -> anyhow::Result<Result<Engine, Next>> {
    let mut delay = RETRY_FIRST;
    let mut attempts = 0u32;
    let mut last_error = String::new();
    loop {
        attempts += 1;
        match Engine::start(ctx.cfg, ctx.region).await {
            Ok(engine) => {
                if attempts > 1 {
                    info!("engine started at attempt {attempts}");
                }
                return Ok(Ok(engine));
            }
            Err(e) => {
                let e = format!("{e:#}");
                // Once per distinct cause: a Mac can wait long for its network.
                if e != last_error {
                    warn!("cannot start the engine: {e}; retrying until it starts");
                } else {
                    debug!("cannot start the engine: {e}");
                }
                last_error = e;
            }
        }
        ctx.listener.poll();
        let mut retry = std::pin::pin!(tokio::time::sleep(delay));
        loop {
            tokio::select! {
                r = stop.requested() => return Ok(Err(Next::Stop(r?))),
                () = &mut retry => break,
                () = ctx.status.tick() => {
                    info!(
                        "engine not running: {last_error} (attempts: {attempts}); hal: {}",
                        ctx.server.status()
                    );
                }
                call = ctx.control.next() => {
                    if ctx.answer(call, None, &last_error) {
                        return Ok(Err(Next::RestartDaemon));
                    }
                }
            }
        }
        delay = next_retry_delay(delay);
    }
}

fn next_retry_delay(delay: Duration) -> Duration {
    (delay + Duration::from_secs(1)).min(RETRY_MAX)
}

/// Runs until a stop, a new latency or a restart is asked for, logging the
/// status line at its cadence and answering the app's calls.
async fn run_engine(
    engine: &Engine,
    ctx: &mut Ctx<'_>,
    stop: &mut impl Stop,
) -> anyhow::Result<Next> {
    let mut second = tokio::time::interval(Duration::from_secs(1));
    let mut clock_log = ClockStateLog::default();
    let mut latency = engine.device.latency_requests();
    latency.mark_unchanged();
    let mut format = engine.device.format_requests();
    format.mark_unchanged();
    loop {
        tokio::select! {
            r = stop.requested() => return Ok(Next::Stop(r?)),
            () = ctx.status.tick() => {
                info!("{}; hal: {}", status_line(&engine.device, &engine.clock), ctx.server.status());
            }
            _ = second.tick() => {
                clock_log.poll(&engine.clock);
                ctx.listener.poll();
            }
            Ok(()) = latency.changed() => {
                if engine.device.configured_latency_ns() != engine.device.info().latency_ns {
                    return Ok(Next::RestartDevice);
                }
            }
            Ok(()) = format.changed() => {
                let request = *format.borrow_and_update();
                if ctx.apply_format(engine, request) {
                    return Ok(Next::RestartDaemon);
                }
            }
            call = ctx.control.next() => {
                if ctx.answer(call, Some(engine), "") {
                    return Ok(Next::RestartDaemon);
                }
            }
        }
    }
}

/// The cadence of the status line: one every `[coreaudio]
/// status_log_interval_s`, none if that is zero. The first is due one
/// interval after the daemon starts, whether the engine runs by then or not.
struct StatusTimer(Option<Interval>);

impl StatusTimer {
    fn new(every: Duration) -> StatusTimer {
        StatusTimer((!every.is_zero()).then(|| {
            let mut i = tokio::time::interval_at(Instant::now() + every, every);
            i.set_missed_tick_behavior(MissedTickBehavior::Delay);
            i
        }))
    }

    /// Completes when the next line is due; never if there are none.
    async fn tick(&mut self) {
        match &mut self.0 {
            Some(i) => {
                i.tick().await;
            }
            None => std::future::pending().await,
        }
    }
}

/// The device engine and its clock.
struct Engine {
    device: Device,
    clock: MediaClock,
    /// The PTP follower feeding `clock`, when it follows the network.
    follower: Option<PtpFollower>,
    /// When the clock started.
    started: std::time::Instant,
}

impl Engine {
    /// Starts the device on `region`'s rings, with its clock mirrored into
    /// the region. Whatever started is stopped again on failure.
    async fn start(cfg: &AppConfig, region: &Arc<HalRegion>) -> anyhow::Result<Engine> {
        let d = &cfg.device;
        let iface = Interface::find(&d.interface)?;
        let (clock, follower) = match cfg.clock.source {
            ClockSource::Ptp => {
                let mut ptp = PtpConfig::new(iface.ip, iface.mac);
                ptp.event_port = cfg.clock.ptp_event_port;
                ptp.general_port = cfg.clock.ptp_general_port;
                let (follower, clock) =
                    PtpFollower::start(ptp).await.context("starting the PTP clock follower")?;
                (clock, Some(follower))
            }
            ClockSource::Free => {
                let rate = config::rate_from_ppm(cfg.clock.free_rate_ppm)?;
                (free_running_clock_with_rate(rate), None)
            }
            ClockSource::System => anyhow::bail!("the driver cannot follow the system clock"),
        };
        clock.set_mirror(Some(ShmClockMirror::new(region.clone())))?;
        let device = match Self::start_device(cfg, region, &clock).await {
            Ok(device) => device,
            Err(e) => {
                if let Some(f) = follower {
                    f.shutdown();
                }
                return Err(e);
            }
        };
        if follower.is_none() {
            warn!(
                "using a free-running clock at {:+} ppm: audio will not be in sync with a Dante \
                 network",
                cfg.clock.free_rate_ppm
            );
        }
        Ok(Engine { device, clock, follower, started: std::time::Instant::now() })
    }

    /// Starts the device on `region`'s rings, following `clock`.
    async fn start_device(
        cfg: &AppConfig,
        region: &Arc<HalRegion>,
        clock: &MediaClock,
    ) -> anyhow::Result<Device> {
        let d = &cfg.device;
        let rings = region
            .external_rings(d.rx_channels.names().len(), d.tx_channels.names().len())
            .context("placing the rings in the shared region")?;
        // Controllers may change the sample rate and the bit depth: the
        // daemon applies them (run_engine).
        let options =
            StartOptions { rings: Some(rings), format_configurable: true, ..Default::default() };
        Ok(Device::start_with_options(d.clone(), clock.clone(), options).await?)
    }

    /// Stops the device and starts it again with the latency a controller
    /// or the app set, on the same clock: the clock stays locked. On
    /// failure the clock is stopped too.
    async fn restart_device(
        self,
        cfg: &AppConfig,
        region: &Arc<HalRegion>,
    ) -> anyhow::Result<Engine> {
        let Engine { device, clock, follower, started } = self;
        let mut cfg = cfg.clone();
        cfg.device.latency_ms = device.configured_latency_ns() as f64 / 1e6;
        device.shutdown().await;
        match Self::start_device(&cfg, region, &clock).await {
            Ok(device) => Ok(Engine { device, clock, follower, started }),
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

/// Reports, once, that launchd did not give this process the driver's Mach
/// service: the driver can then never connect.
struct ListenerCheck {
    invalid: Box<dyn Fn() -> bool + Send>,
    service: String,
    reported: bool,
}

impl ListenerCheck {
    fn poll(&mut self) {
        if !self.reported && (self.invalid)() {
            self.reported = true;
            error!(
                "hal: the driver cannot connect: this process does not hold the Mach service \
                 {}; start through launchd: {LAUNCHD_HINT}",
                self.service
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    use tokio::sync::mpsc;

    use ovsc_control::{Request, Response, Restart, SettingsChange};
    use ovsc_core::{Channels, DeviceConfig, Ports};
    use ovsc_ipc::mem::{MemClient, MemRegistry};
    use ovsc_ipc::protocol::{Hello, PROTO_MAJOR, ToDaemon, ToPlugin, Welcome};
    use ovsc_ipc::region::MappedRegion;
    use ovsc_ipc::transport::{ClientHandler, ClientTransport};
    use ovsc_shm::clock::{ClockRead, READ_TRIES};
    use ovsc_shm::layout::{HOST_ARCH, LAYOUT_HASH, LAYOUT_VERSION, RegionRef};
    use ovsc_shm::status::{AudioWord, DAEMON_ENGINE_RUNNING, DAEMON_SHUTTING_DOWN};
    use ovsc_shm::time::ClockState;

    use super::*;
    use crate::config::BackendKind;

    const SERVICE: &str = "org.openvirtualsoundcard.audio.cli-test";
    const UID: u32 = 501;

    impl Stop for mpsc::UnboundedReceiver<&'static str> {
        async fn requested(&mut self) -> anyhow::Result<&'static str> {
            self.recv().await.context("no more stop requests")
        }
    }

    fn app_config(interface: &str, base: u16) -> AppConfig {
        let device = DeviceConfig {
            name: "cli-ca".into(),
            interface: interface.into(),
            tx_channels: Channels::Count(2),
            rx_channels: Channels::Count(3),
            ports: Ports { arc: base, cmc: base + 1, flow_control: base + 2, settings: base + 3 },
            discovery: false,
            process_id: 41,
            ..Default::default()
        };
        let mut cfg = AppConfig { device, ..Default::default() };
        cfg.clock.source = ClockSource::Free;
        cfg.clock.free_rate_ppm = 50.0;
        cfg.audio.backend = BackendKind::CoreAudio;
        cfg.coreaudio.service = SERVICE.into();
        cfg.coreaudio.allowed_uids = vec![UID];
        cfg.coreaudio.prevent_idle_sleep = false;
        cfg.coreaudio.status_log_interval_s = 1;
        cfg.coreaudio.control_socket = String::new();
        cfg.validate_for(true).unwrap();
        cfg
    }

    /// Records what the daemon sends a driver.
    #[derive(Default)]
    struct Received(Mutex<Vec<ToPlugin>>);

    impl ClientHandler for Received {
        fn on_message(&self, m: ToPlugin) {
            self.0.lock().unwrap().push(m);
        }
        fn on_interrupted(&self) {}
        fn on_invalid(&self) {}
    }

    impl Received {
        fn got_bye(&self) -> bool {
            self.0.lock().unwrap().iter().any(|m| matches!(m, ToPlugin::Bye { .. }))
        }
    }

    fn hello() -> Hello {
        Hello {
            proto_major: PROTO_MAJOR,
            proto_minor: 0,
            layout_version: LAYOUT_VERSION,
            layout_hash: LAYOUT_HASH,
            plugin_version: "cli test".into(),
            instance: 7,
            pid: 4242,
            applied_daemon_generation: 0,
            applied_config_gen: 0,
            sample_rate: 48_000,
            input_channels: 3,
            output_channels: 2,
            timebase_numer: 1,
            timebase_denom: 1,
            arch: HOST_ARCH,
        }
    }

    /// Connects a driver and gets its welcome.
    async fn attach(registry: &Arc<MemRegistry>) -> (MemClient, Arc<Received>, Welcome) {
        let client = registry.client(SERVICE, UID, 4242);
        let received = Arc::new(Received::default());
        client.connect(received.clone());
        let (tx, rx) = tokio::sync::oneshot::channel();
        client.request(
            ToDaemon::Hello(hello()),
            Duration::from_secs(2),
            Box::new(move |r| {
                let _ = tx.send(r);
            }),
        );
        match rx.await.unwrap() {
            Ok(ToPlugin::Welcome(w)) => (client, received, w),
            other => panic!("no welcome: {other:?}"),
        }
    }

    fn map(w: &Welcome) -> (MappedRegion, RegionRef<'static>) {
        let mapped = w.region.map().unwrap();
        // SAFETY: the mapping outlives every use of the view in these tests.
        let view = unsafe { RegionRef::from_raw(mapped.as_ptr(), mapped.len()) }.unwrap();
        (mapped, view)
    }

    async fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !check() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn service(registry: &Arc<MemRegistry>) -> Service {
        Service {
            transport: Box::new(registry.bind(SERVICE)),
            listener_invalid: Box::new(|| false),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_driver_attaches_and_the_engine_runs_on_the_region() {
        let registry = MemRegistry::new();
        let (stop_tx, mut stop) = mpsc::unbounded_channel();
        let daemon = serve(app_config("127.0.0.1", 24790), None, service(&registry), &mut stop);
        let driver = async {
            let (_client, received, w) = attach(&registry).await;
            assert_eq!((w.config.input_channels, w.config.output_channels), (3, 2));
            assert_eq!(w.config.device_name, "cli-ca");
            let (_mapped, view) = map(&w);
            let d = view.daemon();
            wait_for("the engine", || d.flags.load(Ordering::Acquire) & DAEMON_ENGINE_RUNNING != 0)
                .await;
            let audio = AudioWord::unpack(d.audio_word.load(Ordering::Acquire));
            assert_eq!(audio.sample_rate, 48_000);
            // The free-running clock is mirrored into the region.
            let ClockRead::Record(r) = view.clock().read_bounded(READ_TRIES) else {
                panic!("no clock record");
            };
            assert!(r.valid);
            assert_eq!(r.state, ClockState::FreeRunning);
            assert_eq!(r.snapshot.rate, 1.00005);
            stop_tx.send("test").unwrap();
            wait_for("the bye", || received.got_bye()).await;
            let flags = d.flags.load(Ordering::Acquire);
            assert_eq!(
                flags & (DAEMON_ENGINE_RUNNING | DAEMON_SHUTTING_DOWN),
                DAEMON_SHUTTING_DOWN
            );
        };
        let (result, ()) = tokio::join!(daemon, driver);
        result.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_service_runs_while_the_engine_cannot_start() {
        let registry = MemRegistry::new();
        let (stop_tx, mut stop) = mpsc::unbounded_channel();
        // No interface has this address (TEST-NET-1), so every start fails.
        let daemon = serve(app_config("192.0.2.1", 24800), None, service(&registry), &mut stop);
        let driver = async {
            let (_client, received, w) = attach(&registry).await;
            // The configured layout is on offer before the engine runs.
            assert_eq!((w.config.input_channels, w.config.output_channels), (3, 2));
            let (_mapped, view) = map(&w);
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(view.daemon().flags.load(Ordering::Acquire) & DAEMON_ENGINE_RUNNING, 0);
            let asked = Instant::now();
            stop_tx.send("test").unwrap();
            wait_for("the bye", || received.got_bye()).await;
            // The first attempt failed at once, about 200 ms ago, so the
            // next one is still some 1.8 s away: only a stop that cuts the
            // wait short says goodbye this soon.
            let waited = asked.elapsed();
            assert!(waited < Duration::from_millis(500), "the bye came after {waited:?}");
        };
        let (result, ()) = tokio::join!(daemon, driver);
        result.unwrap();
    }

    /// Asks the daemon's control socket at `path`, from a blocking thread
    /// as the app does.
    async fn ask(path: &Path, request: Request) -> Response {
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || {
            ovsc_control::Client::connect_to(&path).unwrap().request(&request).unwrap()
        })
        .await
        .unwrap()
    }

    async fn latency_ms(path: &Path) -> Option<f64> {
        match ask(path, Request::Status).await {
            Response::Status(s) => s.device.map(|d| d.latency_ms),
            other => panic!("not a status: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_app_sets_the_latency_and_the_device_restarts_on_the_same_region() {
        let dir = std::env::temp_dir().join(format!("ovsc-ca-control-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let socket = dir.join("control.sock");
        let mut cfg = app_config("127.0.0.1", 24820);
        cfg.coreaudio.control_socket = socket.to_str().unwrap().into();
        cfg.device.state_file = Some(dir.join("state.toml"));
        cfg.device.latency_ms = 4.0;
        let registry = MemRegistry::new();
        let (stop_tx, mut stop) = mpsc::unbounded_channel();
        let daemon = serve(cfg, None, service(&registry), &mut stop);
        let app = async {
            let (_client, received, w) = attach(&registry).await;
            let (_mapped, view) = map(&w);
            let d = view.daemon();
            let running = || d.flags.load(Ordering::Acquire) & DAEMON_ENGINE_RUNNING != 0;
            wait_for("the engine", running).await;
            assert_eq!(latency_ms(&socket).await, Some(4.0));
            let wav = dir.join("first.wav");
            let start = Request::StartRecording { path: wav.to_str().unwrap().into() };
            assert!(matches!(ask(&socket, start.clone()).await,
                Response::Recording(s) if s.recording && s.channels == 3));
            assert!(matches!(ask(&socket, start).await, Response::Error { .. }));
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(matches!(ask(&socket, Request::AddMarker { label: "Verse".into() }).await,
                Response::Recording(s) if s.markers.len() == 1 && s.markers[0].label == "Verse"));
            assert!(matches!(ask(&socket, Request::RecordingStatus).await,
                Response::Recording(s) if s.recording && s.bytes > 0));

            let change = SettingsChange { latency_ms: Some(2.0), ..Default::default() };
            match ask(&socket, Request::Apply { change }).await {
                Response::Applied { restart } => assert_eq!(restart, Restart::Device),
                other => panic!("not applied: {other:?}"),
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            while latency_ms(&socket).await != Some(2.0) {
                assert!(Instant::now() < deadline, "the device did not restart at 2 ms");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            wait_for("the engine again", running).await;
            let first = match ask(&socket, Request::RecordingStatus).await {
                Response::Recording(s) => s,
                other => panic!("no recording status: {other:?}"),
            };
            assert!(!first.recording);
            assert!(first.error.is_none());
            let bytes = std::fs::read(&wav).unwrap();
            assert!(bytes.len() as u64 > 44 + first.bytes);
            let wave = crate::playback::Wave::open(std::fs::File::open(&wav).unwrap()).unwrap();
            assert_eq!(wave.frames, first.frames);
            assert_eq!(wave.markers.len(), 1);
            assert_eq!(wave.markers[0].label, "Verse");
            assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as u64, first.bytes);
            assert!(matches!(
                ask(&socket, Request::StartRecording { path: wav.to_str().unwrap().into() }).await,
                Response::Error { .. }
            ));
            let second = dir.join("second.wav");
            assert!(
                matches!(ask(&socket, Request::StartRecording { path: second.to_str().unwrap().into() }).await,
                Response::Recording(s) if s.recording)
            );
            assert!(matches!(ask(&socket, Request::StopRecording).await,
                Response::Recording(s) if !s.recording && s.error.is_none()));
            assert!(matches!(ask(&socket, Request::StopRecording).await,
                Response::Recording(s) if !s.recording));
            // The driver stayed attached to the same region throughout.
            assert!(!received.got_bye());
            match ask(&socket, Request::Settings).await {
                Response::Settings(s) => assert_eq!(s.latency_ms, 2.0),
                other => panic!("no settings: {other:?}"),
            }
            let saved = std::fs::read_to_string(dir.join("state.toml")).unwrap();
            assert!(saved.contains("latency_ns = 2000000"), "{saved}");

            // Out of range: refused, and nothing restarts.
            let change = SettingsChange { latency_ms: Some(80.0), ..Default::default() };
            assert!(matches!(
                ask(&socket, Request::Apply { change }).await,
                Response::Error { .. }
            ));
            stop_tx.send("test").unwrap();
            wait_for("the bye", || received.got_bye()).await;
        };
        let (result, ()) = tokio::join!(daemon, app);
        result.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_engine_runs_on_the_regions_rings() {
        let cfg = app_config("127.0.0.1", 24810);
        let region = HalRegion::create(VERSION).unwrap();
        let engine = Engine::start(&cfg, &region).await.unwrap();
        let audio = engine.device.audio();
        assert_eq!((audio.rx.len(), audio.tx.len()), (3, 2));
        // Samples cross between the device's rings and the region's, both
        // ways and each on its own channel.
        let view = region.view();
        let ts = 0x1234_5678;
        for (ch, ring) in audio.rx.iter().enumerate() {
            let tag = 0x10_0000 + ch as i32;
            ring.write_one(ts, tag);
            assert_eq!(view.rx(ch).unwrap().read_one(ts), Some(tag), "rx channel {ch}");
        }
        for (ch, ring) in audio.tx.iter().enumerate() {
            let tag = 0x20_0000 + ch as i32;
            view.tx(ch).unwrap().write_one(ts, tag);
            assert_eq!(ring.read_one(ts), Some(tag), "tx channel {ch}");
        }
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn status_lines_come_at_their_interval() {
        let mut off = StatusTimer::new(Duration::ZERO);
        assert!(tokio::time::timeout(Duration::from_millis(50), off.tick()).await.is_err());
        // The first line is one interval away, not immediate.
        let mut on = StatusTimer::new(Duration::from_secs(1));
        assert!(tokio::time::timeout(Duration::from_millis(100), on.tick()).await.is_err());
        tokio::time::timeout(Duration::from_secs(5), on.tick()).await.unwrap();
    }

    #[test]
    fn retries_back_off_to_five_seconds() {
        let mut d = RETRY_FIRST;
        let mut seen = vec![d.as_secs()];
        for _ in 0..4 {
            d = next_retry_delay(d);
            seen.push(d.as_secs());
        }
        assert_eq!(seen, [2, 3, 4, 5, 5]);
    }

    #[test]
    fn an_invalid_listener_is_reported_once() {
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let c = calls.clone();
        let mut check = ListenerCheck {
            invalid: Box::new(move || {
                c.fetch_add(1, Ordering::Relaxed);
                true
            }),
            service: SERVICE.into(),
            reported: false,
        };
        check.poll();
        check.poll();
        assert!(check.reported);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}
