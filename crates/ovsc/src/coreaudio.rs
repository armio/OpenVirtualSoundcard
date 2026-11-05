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
//!
//! While it runs, the engine is restarted on the network interface when
//! that changes under it (a new address, an adapter that came back; see
//! [`crate::netwatch`]), and a failed part of the daemon (a panic, a device
//! task or the driver heartbeat that stopped; see [`crate::supervise`])
//! ends the daemon with an error after the driver's goodbye, for launchd to
//! start a fresh one. A new receive latency restarts nothing but the
//! device's receive side ([`Ctx::apply_latency`]).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use tokio::sync::oneshot;
use tokio::time::{Instant, Interval, MissedTickBehavior};
use tracing::{debug, error, info, warn};

use ovsc_clock::ptp::{PtpConfig, PtpFollower};
use ovsc_clock::{MediaClock, free_running_clock_with_rate};
use ovsc_control::{Request, Response, Restart, SettingsChange};
use ovsc_core::net::Interface;
use ovsc_core::{Device, FormatRequest, RemoteChanges, SavedState, StartOptions};
use ovsc_hal_server::{
    EngineInfo, HalOptions, HalRegion, HalServer, ShmClockMirror, driver_config,
};
use ovsc_ipc::transport::ServerTransport;

use crate::config::{self, AppConfig, ClockSource};
use crate::control::{self, Control, Health};
use crate::netwatch::{self, Verdict};
use crate::supervise::Supervision;
use crate::{ClockStateLog, Stop, StopSignals, status_line};

/// The daemon version in the region header.
const VERSION: &str = concat!("ovsc ", env!("CARGO_PKG_VERSION"));

/// The wait after the first failed engine start; each further failure
/// waits a second longer, up to [`RETRY_MAX`].
pub(crate) const RETRY_FIRST: Duration = Duration::from_secs(2);
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
    serve(cfg, config_path, service, Supervision::daemon(), &mut stop).await
}

/// Why [`run_engine`] or [`start_engine`] returned.
enum Next {
    /// A stop was requested, for this reason.
    Stop(&'static str),
    /// The network interface changed under the engine: restart all of it,
    /// the clock included, on the interface as it is now.
    RestartEngine,
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

/// Serves the driver on `service` and runs the engine until `stop`, or
/// until `sup` finds a fault.
async fn serve(
    cfg: AppConfig,
    config_path: Option<PathBuf>,
    service: Service,
    sup: Supervision,
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
    let initial = initial_engine(&cfg, &opts)?;
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
    let health = Mutex::new(Health::new());
    let mut ctx = Ctx {
        cfg: &cfg,
        config_path: config_path.as_deref(),
        region: &region,
        server: &server,
        listener: &mut listener,
        status: &mut status,
        control: &mut control,
        health: &health,
        sup,
    };

    let mut engine = match start_engine(&mut ctx, None, stop).await {
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
    control::restarted(&health);
    let result = loop {
        match run_engine(&engine, &mut ctx, stop).await {
            Ok(Next::RestartEngine) => {
                // The driver keeps the region and its configuration; it
                // reads silence until the engine runs again.
                server.engine_stopped();
                let carry = Carry::from(&engine.device);
                engine.shutdown().await;
                engine = match start_again(&mut ctx, carry, stop).await {
                    Ok(engine) => engine,
                    Err(end) => {
                        server.shutdown().await;
                        return end;
                    }
                };
                server.engine_started(&engine.device);
                control::restarted(&health);
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
    server.shutdown().await;
    engine.shutdown().await;
    result
}

/// The engine offered to the driver until one runs: as the device will
/// start, with the latency and names saved in the state file, so that its
/// start does not reconfigure Core Audio a second time. Without them if
/// the driver cannot take them (a long saved latency with a wide
/// `input_margin_us`, say): the configuration alone was checked at load,
/// and the daemon must not exit over a saved setting.
fn initial_engine(cfg: &AppConfig, opts: &HalOptions) -> anyhow::Result<EngineInfo> {
    let d = &cfg.device;
    let configured = EngineInfo::from_config(d)?;
    let saved = d.state_file.as_deref().and_then(SavedState::load).unwrap_or_default();
    Ok(match EngineInfo::from_saved(d, &saved) {
        Ok(e) if driver_config(&e, 1, opts).is_ok() => e,
        _ => configured,
    })
}

fn log_next(next: &Next) {
    match next {
        Next::Stop(why) => info!("{why}: shutting down"),
        Next::RestartDaemon => info!("restarting to apply the new settings"),
        Next::RestartEngine => {}
    }
}

/// Starts the engine again after it stopped, retrying until it runs; `Err`
/// holds what the daemon returns instead, when it stops first.
async fn start_again(
    ctx: &mut Ctx<'_>,
    carry: Carry,
    stop: &mut impl Stop,
) -> Result<Engine, anyhow::Result<()>> {
    match start_engine(ctx, Some(carry), stop).await {
        Ok(Ok(engine)) => Ok(engine),
        Ok(Err(next)) => {
            log_next(&next);
            Err(Ok(()))
        }
        Err(e) => {
            error!("shutting down: {e:#}");
            Err(Err(e))
        }
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
    /// Turns the counters into warnings for the app and the log.
    health: &'a Mutex<Health>,
    sup: Supervision,
}

impl Ctx<'_> {
    /// The engine as a control call or the health tracker sees it.
    fn view<'v>(&'v self, engine: Option<&'v Engine>, error: Option<&'v str>) -> control::View<'v> {
        control::View {
            cfg: self.cfg,
            config_path: self.config_path,
            engine: engine.map(|e| (&e.device, &e.clock, e.started.elapsed())),
            hal: self.server.status(),
            engine_error: error,
            health: Some(self.health),
        }
    }

    /// Feeds the health tracker, which logs warnings as they appear and
    /// clear; every second.
    fn watch(&self, engine: Option<&Engine>) {
        control::watch(&self.view(engine, None));
    }

    /// Why the daemon must restart, if it must: a panic, a part of the
    /// running `engine`'s device, or the driver heartbeat, that stopped.
    fn fault(&self, engine: Option<&Engine>) -> Option<String> {
        self.sup.fault(engine.map(|e| &e.device)).or_else(|| {
            self.server.status_task_ended().then(|| "the driver heartbeat task stopped".to_owned())
        })
    }

    /// Answers a control call; whether the daemon must restart.
    fn answer(&self, call: control::Call, engine: Option<&Engine>, error: &str) -> bool {
        control::answer(call, &self.view(engine, Some(error))) == control::After::RestartDaemon
    }

    /// Answers a control call while no engine runs, the last start having
    /// failed with `error`; whether the daemon must restart. A change the
    /// app applies meanwhile, saved in the state file, also goes into
    /// `carry`, the state the next device starts from (see
    /// [`Carry::take_app_change`]).
    fn answer_idle(&self, mut call: control::Call, error: &str, carry: Option<&mut Carry>) -> bool {
        let (Request::Apply { change }, Some(carry), Some(path)) =
            (&call.request, carry, self.cfg.device.state_file.as_deref())
        else {
            return self.answer(call, None, error);
        };
        let change = change.clone();
        // The answer passes through here, to see whether it was applied.
        let (reply, mut answer) = oneshot::channel();
        let app = std::mem::replace(&mut call.reply, reply);
        let restart = self.answer(call, None, error);
        if let Ok(response) = answer.try_recv() {
            if matches!(response, Response::Applied { .. }) {
                carry.take_app_change(&change, path);
            }
            let _ = app.send(response);
        }
        restart
    }

    /// Applies the receive latency a controller or the app set last, if the
    /// device does not run at it yet: the device restarts its receive side
    /// at it (its receive flows, `Device::restart_rx`), then the driver is
    /// offered the configuration with the new input offsets. The engine
    /// keeps running throughout, so the driver keeps its gate open and the
    /// transmit flows, the mDNS records and the clock go on; Core Audio
    /// sees one configuration change, for the new safety offset.
    fn apply_latency(&self, engine: &Engine) {
        if engine.device.restart_rx() {
            self.server.engine_changed(&engine.device);
        }
    }

    /// Saves a sample rate or bit depth that a controller asked for;
    /// whether the daemon must restart to apply it.
    fn apply_format(&self, engine: &Engine, request: FormatRequest) -> bool {
        let view = self.view(Some(engine), None);
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
///
/// The app's changes of the name, latency and show lock go to the state
/// file meanwhile, since no device runs to take them, and into `carry`
/// too (see [`Ctx::answer_idle`]): a device started from `carry` does not
/// read the state file, and would run without them and save the carried
/// state over them.
async fn start_engine(
    ctx: &mut Ctx<'_>,
    mut carry: Option<Carry>,
    stop: &mut impl Stop,
) -> anyhow::Result<Result<Engine, Next>> {
    let mut delay = RETRY_FIRST;
    let mut attempts = 0u32;
    let mut last_error = String::new();
    loop {
        attempts += 1;
        match Engine::start(ctx.cfg, ctx.region, carry.clone()).await {
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
        let mut second = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                r = stop.requested() => return Ok(Err(Next::Stop(r?))),
                () = &mut retry => break,
                _ = second.tick() => {
                    if let Some(fault) = ctx.fault(None) {
                        anyhow::bail!("{fault}: restarting the daemon");
                    }
                    ctx.watch(None);
                }
                () = ctx.status.tick() => {
                    info!(
                        "engine not running: {last_error} (attempts: {attempts}); {}; hal: {}",
                        control::health_summary(ctx.health),
                        ctx.server.status()
                    );
                }
                call = ctx.control.next() => {
                    if ctx.answer_idle(call, &last_error, carry.as_mut()) {
                        return Ok(Err(Next::RestartDaemon));
                    }
                }
            }
        }
        delay = next_retry_delay(delay);
    }
}

pub(crate) fn next_retry_delay(delay: Duration) -> Duration {
    (delay + Duration::from_secs(1)).min(RETRY_MAX)
}

/// Runs until a stop or a restart is asked for, or the network interface
/// changes, logging the status line at its cadence, answering the app's
/// calls and applying new receive latencies in place. A fault is an error.
async fn run_engine(
    engine: &Engine,
    ctx: &mut Ctx<'_>,
    stop: &mut impl Stop,
) -> anyhow::Result<Next> {
    let mut second = tokio::time::interval(Duration::from_secs(1));
    let mut clock_log = ClockStateLog::default();
    let iface = &engine.device.info().iface;
    let mut watch = ctx.sup.probe.watch(&iface.name, iface.ip);
    let mut latency = engine.device.latency_requests();
    latency.mark_unchanged();
    // A latency set between the device's start and this subscription
    // was not seen as a change: apply it now.
    ctx.apply_latency(engine);
    let mut format = engine.device.format_requests();
    format.mark_unchanged();
    loop {
        tokio::select! {
            r = stop.requested() => return Ok(Next::Stop(r?)),
            () = ctx.status.tick() => {
                let network = netwatch::status_note(watch.as_ref());
                info!(
                    "{}{network}; {}; hal: {}",
                    status_line(&engine.device, &engine.clock),
                    control::health_summary(ctx.health),
                    ctx.server.status()
                );
            }
            _ = second.tick() => {
                clock_log.poll(&engine.clock);
                ctx.listener.poll();
                if let Some(fault) = ctx.fault(Some(engine)) {
                    anyhow::bail!("{fault}: restarting the daemon");
                }
                ctx.watch(Some(engine));
                if let Some(w) = &mut watch {
                    if let Verdict::Restart(why) = w.poll(ctx.sup.probe, &engine.clock) {
                        warn!("{why}: restarting the network engine");
                        return Ok(Next::RestartEngine);
                    }
                }
            }
            Ok(()) = latency.changed() => ctx.apply_latency(engine),
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

/// What a device hands on to the one that replaces it, so that a restart
/// keeps the names, subscriptions, latency and show lock set since the
/// start even without a state file, still lists the controllers' changes
/// that explain the restart, and keeps the name it announced without
/// probing for it again (see [`StartOptions::announced`]).
#[derive(Clone)]
struct Carry {
    state: SavedState,
    remote_changes: RemoteChanges,
    announced: Option<String>,
}

impl From<&Device> for Carry {
    fn from(device: &Device) -> Self {
        Carry {
            state: device.saved_state(),
            remote_changes: device.remote_changes(),
            announced: device.announced_name(),
        }
    }
}

impl Carry {
    /// Takes in `change`, which the app applied while no engine ran:
    /// [`control::apply`] saved its name, latency and show lock in the
    /// state file at `path` then, and they are read back from there, as
    /// saved. Only what `change` set: the rest stays the carried state,
    /// which holds even if the last save of the device that stopped
    /// failed.
    fn take_app_change(&mut self, change: &SettingsChange, path: &Path) {
        let Some(saved) = SavedState::load(path) else { return };
        if change.name.is_some() {
            self.state.name = saved.name;
        }
        if change.latency_ms.is_some() {
            self.state.latency_ns = saved.latency_ns;
        }
        if change.lock_remote_settings.is_some() {
            self.state.lock_remote_settings = saved.lock_remote_settings;
        }
    }
}

impl Engine {
    /// Starts the device on `region`'s rings, with its clock mirrored into
    /// the region, from what a previous device left (`carry`), if any.
    /// Whatever started is stopped again on failure.
    async fn start(
        cfg: &AppConfig,
        region: &Arc<HalRegion>,
        carry: Option<Carry>,
    ) -> anyhow::Result<Engine> {
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
        let device = match Self::start_device(cfg, region, &clock, carry).await {
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

    /// Starts the device on `region`'s rings, following `clock`, from what
    /// a previous device left (`carry`), if any.
    async fn start_device(
        cfg: &AppConfig,
        region: &Arc<HalRegion>,
        clock: &MediaClock,
        carry: Option<Carry>,
    ) -> anyhow::Result<Device> {
        let d = &cfg.device;
        let rings = region
            .external_rings(d.rx_channels.names().len(), d.tx_channels.names().len())
            .context("placing the rings in the shared region")?;
        // Controllers may change the sample rate and the bit depth: the
        // daemon applies them (run_engine), unless the show lock
        // (`[device] lock_remote_settings`, or the app) refuses them.
        let (state, remote_changes, announced) = match carry {
            Some(c) => (Some(c.state), Some(c.remote_changes), c.announced),
            None => (None, None, None),
        };
        let options = StartOptions {
            rings: Some(rings),
            format_configurable: true,
            remote_changes,
            state,
            announced,
            ..Default::default()
        };
        Ok(Device::start_with_options(d.clone(), clock.clone(), options).await?)
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
    use std::sync::atomic::{AtomicBool, Ordering};
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
    use crate::netwatch::{Probe, Seen};

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

    /// Supervision without faults, on the system's interfaces (loopback
    /// ones are not watched).
    fn quiet() -> Supervision {
        static NEVER: AtomicBool = AtomicBool::new(false);
        Supervision { probe: Probe::System, panicked: &NEVER }
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
        let daemon =
            serve(app_config("127.0.0.1", 24790), None, service(&registry), quiet(), &mut stop);
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
        let daemon =
            serve(app_config("192.0.2.1", 24800), None, service(&registry), quiet(), &mut stop);
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

    /// Waits for the state file at `path` to hold all of `wanted`. The
    /// running device saves it by itself after each change, durably (with
    /// `F_FULLFSYNC` on macOS, which can take a while), so it may lag
    /// behind what the status already shows.
    async fn saved_state_holds(path: &Path, wanted: &[&str]) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let saved = std::fs::read_to_string(path).unwrap_or_default();
            if wanted.iter().all(|w| saved.contains(w)) {
                return;
            }
            assert!(Instant::now() < deadline, "the state file lacks {wanted:?}: {saved}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn latency_ms(path: &Path) -> Option<f64> {
        match ask(path, Request::Status).await {
            Response::Status(s) => s.device.map(|d| d.latency_ms),
            other => panic!("not a status: {other:?}"),
        }
    }

    /// Pushed configurations a driver received.
    fn configs(received: &Received) -> Vec<ovsc_ipc::protocol::DriverConfig> {
        let messages = received.0.lock().unwrap();
        messages
            .iter()
            .filter_map(|m| match m {
                ToPlugin::Config(c) => Some(c.clone()),
                _ => None,
            })
            .collect()
    }

    /// Counts, from a thread of its own, how often the engine is not
    /// running as a driver sees it in the region of `w`, until `watching`
    /// is cleared.
    fn count_engine_stops(w: &Welcome, watching: Arc<AtomicBool>) -> std::thread::JoinHandle<u64> {
        let handle = w.region.clone();
        std::thread::spawn(move || {
            let mapped = handle.map().unwrap();
            // SAFETY: the mapping lives until the end of the thread.
            let view = unsafe { RegionRef::from_raw(mapped.as_ptr(), mapped.len()) }.unwrap();
            let mut stops = 0;
            while watching.load(Ordering::Relaxed) {
                let flags = view.daemon().flags.load(Ordering::Acquire);
                if flags & DAEMON_ENGINE_RUNNING == 0 {
                    stops += 1;
                }
                std::thread::sleep(Duration::from_micros(100));
            }
            stops
        })
    }

    /// A latency from the app restarts only the device's receive side: the
    /// engine never stops as the driver sees it (its gate stays open, and
    /// the transmit flows go on), the clock is the same, and the driver is
    /// offered the new input safety offset, with the output side as it was.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_app_sets_the_latency_and_only_the_receive_side_restarts() {
        let dir = std::env::temp_dir().join(format!("ovsc-ca-control-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let socket = dir.join("control.sock");
        let mut cfg = app_config("127.0.0.1", 24820);
        cfg.coreaudio.control_socket = socket.to_str().unwrap().into();
        cfg.device.state_file = Some(dir.join("state.toml"));
        cfg.device.latency_ms = 4.0;
        let registry = MemRegistry::new();
        let (stop_tx, mut stop) = mpsc::unbounded_channel();
        let daemon = serve(cfg, None, service(&registry), quiet(), &mut stop);
        let app = async {
            let (_client, received, w) = attach(&registry).await;
            let (_mapped, view) = map(&w);
            let d = view.daemon();
            let running = || d.flags.load(Ordering::Acquire) & DAEMON_ENGINE_RUNNING != 0;
            wait_for("the engine", running).await;
            assert_eq!(latency_ms(&socket).await, Some(4.0));
            // 4 ms in and out: the transmit latency follows the configured
            // receive latency.
            assert_eq!((w.config.input_safety_offset, w.config.output_latency), (216, 192));
            let clock = clock_ref(&view);
            let watching = Arc::new(AtomicBool::new(true));
            let stops = count_engine_stops(&w, watching.clone());

            // The running device saves the name and latency itself: the
            // control socket does not write the state file behind its back.
            let change = SettingsChange {
                latency_ms: Some(2.0),
                name: Some("vocal-fx".into()),
                ..Default::default()
            };
            match ask(&socket, Request::Apply { change }).await {
                Response::Applied { restart } => assert_eq!(restart, Restart::Receive),
                other => panic!("not applied: {other:?}"),
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            while latency_ms(&socket).await != Some(2.0) {
                assert!(Instant::now() < deadline, "the device did not change to 2 ms");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let pushed = || configs(&received).into_iter().find(|c| c.input_safety_offset == 120);
            wait_for("the new input safety offset", || pushed().is_some()).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            watching.store(false, Ordering::Relaxed);
            assert_eq!(stops.join().unwrap(), 0, "the driver saw the engine stop");
            // 2 ms in; out, still the configured 4 ms.
            let pushed = pushed().unwrap();
            assert_eq!((pushed.input_safety_offset, pushed.output_latency), (120, 192));
            assert_eq!(pushed.output_safety_offset, w.config.output_safety_offset);
            // The same clock, and the driver stayed attached to the same
            // region throughout.
            assert_eq!(clock_ref(&view), clock);
            assert!(!received.got_bye());
            match ask(&socket, Request::Settings).await {
                Response::Settings(s) => assert_eq!(s.latency_ms, 2.0),
                other => panic!("no settings: {other:?}"),
            }
            let state = dir.join("state.toml");
            saved_state_holds(&state, &["latency_ns = 2000000", "name = \"vocal-fx\""]).await;

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

    /// The interface the fake probe shows: 127.0.0.1 until `MOVED`.
    static MOVED: AtomicBool = AtomicBool::new(false);

    fn moving_probe(_name: &str) -> std::io::Result<Option<Seen>> {
        let ip = if MOVED.load(Ordering::SeqCst) {
            std::net::Ipv4Addr::new(127, 0, 0, 9)
        } else {
            std::net::Ipv4Addr::LOCALHOST
        };
        Ok(Some(Seen { index: 1, ipv4: vec![ip] }))
    }

    /// The local time the region's clock mapping was made at: a new one
    /// means a new clock, so a restarted engine.
    fn clock_ref(view: &RegionRef<'_>) -> u64 {
        match view.clock().read_bounded(READ_TRIES) {
            ClockRead::Record(r) => r.snapshot.local_ref_ns,
            _ => 0,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_new_address_restarts_the_engine_on_the_same_region() {
        static NEVER: AtomicBool = AtomicBool::new(false);
        let sup = Supervision { probe: Probe::Fake(moving_probe), panicked: &NEVER };
        let registry = MemRegistry::new();
        let (stop_tx, mut stop) = mpsc::unbounded_channel();
        let daemon =
            serve(app_config("127.0.0.1", 24830), None, service(&registry), sup, &mut stop);
        let driver = async {
            let (_client, received, w) = attach(&registry).await;
            let (_mapped, view) = map(&w);
            let d = view.daemon();
            let running = || d.flags.load(Ordering::Acquire) & DAEMON_ENGINE_RUNNING != 0;
            wait_for("the engine", running).await;
            let first = clock_ref(&view);
            assert_ne!(first, 0);
            tokio::time::sleep(Duration::from_millis(1500)).await;
            assert_eq!(clock_ref(&view), first, "restarted without a reason");

            // The address moves: the engine restarts, clock and all, and
            // goes on restarting until the address is back.
            MOVED.store(true, Ordering::SeqCst);
            wait_for("a new clock", || clock_ref(&view) != first).await;
            MOVED.store(false, Ordering::SeqCst);
            wait_for("the engine again", running).await;
            let settled = clock_ref(&view);
            tokio::time::sleep(Duration::from_millis(2500)).await;
            if running() {
                assert_eq!(clock_ref(&view), settled, "still restarting");
            }
            // The driver stayed attached to the same region throughout.
            assert!(!received.got_bye());
            stop_tx.send("test").unwrap();
            wait_for("the bye", || received.got_bye()).await;
        };
        let (result, ()) = tokio::join!(daemon, driver);
        result.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_panic_ends_the_daemon_after_the_drivers_goodbye() {
        static PANICKED: AtomicBool = AtomicBool::new(false);
        let sup = Supervision { probe: Probe::System, panicked: &PANICKED };
        let registry = MemRegistry::new();
        let (_stop_tx, mut stop) = mpsc::unbounded_channel();
        let daemon =
            serve(app_config("127.0.0.1", 24840), None, service(&registry), sup, &mut stop);
        let driver = async {
            let (_client, received, w) = attach(&registry).await;
            let (_mapped, view) = map(&w);
            let d = view.daemon();
            wait_for("the engine", || d.flags.load(Ordering::Acquire) & DAEMON_ENGINE_RUNNING != 0)
                .await;
            // What the panic hook does when any thread panics.
            PANICKED.store(true, Ordering::SeqCst);
            wait_for("the bye", || received.got_bye()).await;
        };
        let (result, ()) = tokio::join!(daemon, driver);
        let e = result.unwrap_err();
        assert!(e.to_string().contains("panicked"), "{e:#}");
    }

    /// Sends Dante Controller's latency write for `ns` to ARC on `port` and
    /// returns the result code.
    async fn controller_sets_latency(port: u16, ns: u32) -> u16 {
        use ovsc_core::client;
        use ovsc_proto::{arc, frame::Frame};
        let to = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
        let packet = arc::encode_set_latency_request(client::next_seq(), ns, 16);
        let answer = client::transact(
            std::net::Ipv4Addr::LOCALHOST,
            to,
            &packet,
            Duration::from_millis(500),
            3,
        )
        .await
        .unwrap();
        Frame::parse(&answer).unwrap().header.result
    }

    async fn status(path: &Path) -> ovsc_control::Status {
        match ask(path, Request::Status).await {
            Response::Status(s) => *s,
            other => panic!("not a status: {other:?}"),
        }
    }

    async fn apply(path: &Path, change: SettingsChange) -> Restart {
        match ask(path, Request::Apply { change }).await {
            Response::Applied { restart } => restart,
            other => panic!("not applied: {other:?}"),
        }
    }

    /// The show lock, set from the app, refuses Dante Controller's latency
    /// write, so nothing changes; it holds across a new latency the app
    /// sets; and the app lists what controllers did, also after a change a
    /// controller made.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_show_lock_refuses_a_controller_and_survives_restarts() {
        use ovsc_proto::frame::result;

        const PORT: u16 = 24856;
        let dir = std::env::temp_dir().join(format!("ovsc-ca-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let socket = dir.join("control.sock");
        let mut cfg = app_config("127.0.0.1", PORT);
        cfg.coreaudio.control_socket = socket.to_str().unwrap().into();
        cfg.device.state_file = Some(dir.join("state.toml"));
        cfg.device.latency_ms = 4.0;
        let registry = MemRegistry::new();
        let (stop_tx, mut stop) = mpsc::unbounded_channel();
        let daemon = serve(cfg, None, service(&registry), quiet(), &mut stop);
        let app = async {
            let (_client, received, w) = attach(&registry).await;
            let (_mapped, view) = map(&w);
            let d = view.daemon();
            let running = || d.flags.load(Ordering::Acquire) & DAEMON_ENGINE_RUNNING != 0;
            wait_for("the engine", running).await;
            let locked =
                |s: &ovsc_control::Status| s.device.as_ref().unwrap().remote_settings_locked;
            assert!(!locked(&status(&socket).await));

            let lock = |on| SettingsChange { lock_remote_settings: Some(on), ..Default::default() };
            assert_eq!(apply(&socket, lock(true)).await, Restart::None);
            assert!(locked(&status(&socket).await));
            match ask(&socket, Request::Settings).await {
                Response::Settings(s) => assert!(s.lock_remote_settings),
                other => panic!("no settings: {other:?}"),
            }

            // Refused: an error result, and the device keeps running at 4 ms.
            assert_eq!(controller_sets_latency(PORT, 2_000_000).await, result::ERROR);
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(latency_ms(&socket).await, Some(4.0));
            let refused = "tried to change the receive latency to 2 ms";
            assert!(status(&socket).await.warnings.iter().any(|w| w.contains(refused)));

            // The app's own change applies, to the receive side; the device
            // stays locked and still lists the refusal.
            let change = SettingsChange { latency_ms: Some(2.0), ..Default::default() };
            assert_eq!(apply(&socket, change).await, Restart::Receive);
            let deadline = Instant::now() + Duration::from_secs(10);
            while latency_ms(&socket).await != Some(2.0) {
                assert!(Instant::now() < deadline, "the device did not change to 2 ms");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let s = status(&socket).await;
            assert!(locked(&s));
            assert!(s.warnings.iter().any(|w| w.contains(refused)), "{:?}", s.warnings);

            // Unlocked, the controller's change applies too, and is listed
            // afterwards.
            assert_eq!(apply(&socket, lock(false)).await, Restart::None);
            assert_eq!(controller_sets_latency(PORT, 3_000_000).await, result::SUCCESS);
            let deadline = Instant::now() + Duration::from_secs(10);
            while latency_ms(&socket).await != Some(3.0) {
                assert!(Instant::now() < deadline, "the device did not change to 3 ms");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let s = status(&socket).await;
            assert!(!locked(&s));
            // Newest first: the change that explains the dropout leads.
            let at = |text: &str| s.warnings.iter().position(|w| w.contains(text));
            let changed = "changed the receive latency to 3 ms";
            assert!(at(changed).unwrap() < at(refused).unwrap(), "{:?}", s.warnings);
            let state = dir.join("state.toml");
            saved_state_holds(&state, &["lock_remote_settings = false", "latency_ns = 3000000"])
                .await;
            assert!(!received.got_bye());
            stop_tx.send("test").unwrap();
            wait_for("the bye", || received.got_bye()).await;
        };
        let (result, ()) = tokio::join!(daemon, app);
        result.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Without a state file the lock, the name, the subscriptions and the
    /// latency live only in the running device: a new latency keeps the
    /// device, and so all of them, and a restart of the whole engine (a new
    /// address) hands them on, with the list of controllers' changes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_device_restart_keeps_the_lock_and_the_remote_changes() {
        let cfg = app_config("127.0.0.1", 24866);
        assert!(cfg.device.state_file.is_none() && !cfg.device.lock_remote_settings);
        let region = HalRegion::create(VERSION).unwrap();
        let engine = Engine::start(&cfg, &region, None).await.unwrap();
        engine.device.lock_remote_settings(true);
        let change = ovsc_core::RemoteChange {
            at: std::time::SystemTime::now(),
            from: std::net::SocketAddr::from(([192, 168, 1, 20], 4440)),
            what: "the receive latency to 2 ms".into(),
            refused: true,
        };
        engine.device.remote_changes().push(change.clone());
        engine.device.rename("stage-left").unwrap();
        engine.device.subscribe(1, "01", "console").unwrap();
        engine.device.request_latency(2_000_000).unwrap();
        assert!(engine.device.restart_rx());
        assert_eq!(engine.device.latency_ns(), 2_000_000);
        // The transmit latency stays the configured 4 ms.
        assert_eq!(engine.device.info().tx_latency_ns, 4_000_000);
        assert!(engine.device.remote_settings_locked());
        assert_eq!(engine.device.remote_changes().recent(), std::slice::from_ref(&change));
        assert_eq!(engine.device.name(), "stage-left");
        let saved = engine.device.saved_state();
        assert_eq!(saved.subscriptions.len(), 1);
        assert_eq!(
            (saved.subscriptions[0].tx_channel.as_str(), saved.subscriptions[0].tx_device.as_str()),
            ("01", "console")
        );

        // A restart of the whole engine (a new address) carries them all,
        // and the transmit latency stays as it was.
        let carry = Carry::from(&engine.device);
        engine.shutdown().await;
        let engine = Engine::start(&cfg, &region, Some(carry)).await.unwrap();
        assert_eq!(engine.device.name(), "stage-left");
        assert_eq!(engine.device.latency_ns(), 2_000_000);
        assert_eq!(engine.device.info().tx_latency_ns, 4_000_000);
        assert!(engine.device.remote_settings_locked());
        assert_eq!(engine.device.remote_changes().recent(), [change]);
        assert_eq!(engine.device.saved_state().subscriptions.len(), 1);
        engine.shutdown().await;
    }

    /// While the engine cannot start again after a restart, the app's
    /// changes go to the state file. The device that starts at last takes
    /// them in, rather than run on the carried state and save it over them,
    /// and still has what only the carry holds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_apps_changes_while_the_engine_restarts_are_kept() {
        const PORT: u16 = 24946;
        let dir = std::env::temp_dir().join(format!("ovsc-ca-carry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let socket = dir.join("control.sock");
        let state = dir.join("state.toml");
        let mut cfg = app_config("127.0.0.1", PORT);
        cfg.coreaudio.control_socket = socket.to_str().unwrap().into();
        cfg.device.state_file = Some(state.clone());
        cfg.device.latency_ms = 4.0;
        let region = HalRegion::create(VERSION).unwrap();
        let registry = MemRegistry::new();
        let initial = EngineInfo::from_config(&cfg.device).unwrap();
        let transport = Box::new(registry.bind(SERVICE));
        let server =
            HalServer::start(cfg.coreaudio.hal_options(), region.clone(), transport, initial)
                .unwrap();

        // The engine ran, with a subscription and a new name, and stopped:
        // the interface changed under it, say.
        let engine = Engine::start(&cfg, &region, None).await.unwrap();
        engine.device.subscribe(1, "01", "console").unwrap();
        engine.device.rename("stage-left").unwrap();
        let carry = Carry::from(&engine.device);
        engine.shutdown().await;
        // As if its last save had failed: the file holds an older name.
        SavedState::update(&state, |s| s.name = Some("older".into())).unwrap();
        // Something holds the device's ARC port: it cannot start yet.
        let blocker = std::net::UdpSocket::bind(("127.0.0.1", PORT)).unwrap();

        let mut listener =
            ListenerCheck { invalid: Box::new(|| false), service: SERVICE.into(), reported: false };
        let mut status = StatusTimer::new(Duration::ZERO);
        let mut control = Control::listen(&cfg.coreaudio.control_socket);
        let health = Mutex::new(Health::new());
        let mut ctx = Ctx {
            cfg: &cfg,
            config_path: None,
            region: &region,
            server: &server,
            listener: &mut listener,
            status: &mut status,
            control: &mut control,
            health: &health,
            sup: quiet(),
        };
        let (_stop_tx, mut stop) = mpsc::unbounded_channel();
        let restart = start_engine(&mut ctx, Some(carry), &mut stop);
        let app = async {
            // Refused: nothing changes.
            let change = SettingsChange {
                latency_ms: Some(80.0),
                name: Some("refused".into()),
                ..Default::default()
            };
            let refused = ask(&socket, Request::Apply { change }).await;
            assert!(matches!(refused, Response::Error { .. }), "{refused:?}");
            // Meanwhile the operator turns the show lock on and sets 2 ms:
            // saved in the state file, as the app is told.
            let change = SettingsChange {
                lock_remote_settings: Some(true),
                latency_ms: Some(2.0),
                ..Default::default()
            };
            assert_eq!(apply(&socket, change).await, Restart::None);
            let saved = SavedState::load(&state).unwrap();
            assert_eq!(
                (saved.lock_remote_settings, saved.latency_ns),
                (Some(true), Some(2_000_000))
            );
            // The network is back: the next attempt starts the engine.
            drop(blocker);
        };
        let (started, ()) = tokio::join!(restart, app);
        let Ok(Ok(engine)) = started else { panic!("the engine did not start") };
        assert!(engine.device.remote_settings_locked());
        assert_eq!(engine.device.latency_ns(), 2_000_000);
        // What the app did not change is the carried state's.
        assert_eq!(engine.device.name(), "stage-left");
        assert_eq!(engine.device.saved_state().subscriptions.len(), 1);
        // The state file keeps all of it.
        engine.shutdown().await;
        let saved = SavedState::load(&state).unwrap();
        assert_eq!((saved.lock_remote_settings, saved.latency_ns), (Some(true), Some(2_000_000)));
        assert_eq!(saved.name.as_deref(), Some("stage-left"));
        assert_eq!(saved.subscriptions.len(), 1);
        server.shutdown().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A daemon that starts again (launchd starts a new one after a panic)
    /// offers the driver the latency and names saved in the state file at
    /// once, as the device will run with them: the device's start then
    /// changes nothing, and Core Audio is not reconfigured a second time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_first_offer_has_the_saved_latency_and_names() {
        let dir = std::env::temp_dir().join(format!("ovsc-ca-offer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let state = dir.join("state.toml");
        // Set from the app: 2 ms, while the configuration says 4.
        SavedState::update(&state, |s| {
            s.latency_ns = Some(2_000_000);
            s.name = Some("vocal-fx".into());
            s.rx_names = vec!["Vox".into()];
        })
        .unwrap();
        let mut cfg = app_config("127.0.0.1", 24956);
        cfg.device.state_file = Some(state);
        cfg.device.latency_ms = 4.0;
        let registry = MemRegistry::new();
        let (stop_tx, mut stop) = mpsc::unbounded_channel();
        let daemon = serve(cfg, None, service(&registry), quiet(), &mut stop);
        let driver = async {
            let (_client, received, w) = attach(&registry).await;
            let (_mapped, view) = map(&w);
            let d = view.daemon();
            wait_for("the engine", || d.flags.load(Ordering::Acquire) & DAEMON_ENGINE_RUNNING != 0)
                .await;
            // Asked again with the engine running: the same configuration,
            // never replaced since the first.
            let (_again, _, now) = attach(&registry).await;
            assert_eq!(now.config, w.config);
            assert_eq!(now.config.config_gen, 1);
            assert!(!received.0.lock().unwrap().iter().any(|m| matches!(m, ToPlugin::Config(_))));
            // 2 ms at 48 kHz in, plus the 24-frame margin; out, the
            // configured 4 ms, which a saved receive latency leaves alone.
            assert_eq!((w.config.input_safety_offset, w.config.output_latency), (120, 192));
            assert_eq!(w.config.device_name, "vocal-fx");
            assert_eq!(w.config.input_names, ["Vox", "02", "03"]);
            stop_tx.send("test").unwrap();
            wait_for("the bye", || received.got_bye()).await;
        };
        let (result, ()) = tokio::join!(daemon, driver);
        result.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A saved latency the driver cannot take with these options is left
    /// out of the first offer, rather than stop the daemon: the device
    /// then reports it when it runs.
    #[test]
    fn the_first_offer_leaves_out_a_saved_latency_the_driver_cannot_take() {
        let dir = std::env::temp_dir().join(format!("ovsc-ca-wide-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let state = dir.join("state.toml");
        let mut cfg = app_config("127.0.0.1", 24966);
        cfg.device.sample_rate = 192_000;
        cfg.device.state_file = Some(state.clone());
        // Valid at the configured 4 ms; at 40 ms it spans the rings.
        cfg.coreaudio.input_margin_us = 44_000;
        cfg.validate_for(true).unwrap();
        let opts = cfg.coreaudio.hal_options();
        assert_eq!(initial_engine(&cfg, &opts).unwrap().latency_samples, 768);
        SavedState::update(&state, |s| s.latency_ns = Some(2_000_000)).unwrap();
        assert_eq!(initial_engine(&cfg, &opts).unwrap().latency_samples, 384);
        SavedState::update(&state, |s| s.latency_ns = Some(40_000_000)).unwrap();
        assert_eq!(initial_engine(&cfg, &opts).unwrap().latency_samples, 768);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Another device that announces `name` and answers for it, without
    /// having probed for it: as two Macs given one name that started
    /// together would.
    async fn name_holder(name: &str, base: u16) -> Device {
        let cfg = DeviceConfig {
            name: name.into(),
            interface: "127.0.0.1".into(),
            ports: Ports { arc: base, cmc: base + 1, flow_control: base + 2, settings: base + 3 },
            process_id: 99,
            ..Default::default()
        };
        let options = StartOptions { announced: Some(name.into()), ..Default::default() };
        Device::start_with_options(cfg, ovsc_clock::system_clock(), options).await.unwrap()
    }

    /// A new latency, and a restart of the whole engine, keep the name the
    /// device announced while another device announces it too: the first
    /// restarts nothing that announces, the second hands the name on
    /// rather than probe for it again, so the device never renames itself
    /// mid-show. The conflict is reported.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restarts_keep_the_announced_name_through_a_live_conflict() {
        let name = format!("ca-keep{}", std::process::id());
        let mut cfg = app_config("127.0.0.1", 24976);
        cfg.device.name = name.clone();
        cfg.device.discovery = true;
        let region = HalRegion::create(VERSION).unwrap();
        let engine = Engine::start(&cfg, &region, None).await.unwrap();
        wait_for("the name", || engine.device.announced_name().is_some()).await;
        let holder = name_holder(&name, 24986).await;
        wait_for("the conflict", || engine_conflict(&engine)).await;

        engine.device.request_latency(2_000_000).unwrap();
        assert!(engine.device.restart_rx());
        // Longer than a probe takes.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(engine.device.name(), name);
        assert!(engine_conflict(&engine));

        let carry = Carry::from(&engine.device);
        engine.shutdown().await;
        let engine = Engine::start(&cfg, &region, Some(carry)).await.unwrap();
        wait_for("the conflict after the engine restart", || engine_conflict(&engine)).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(engine.device.name(), name);
        assert_eq!(engine.device.saved_state().name.as_deref(), Some(name.as_str()));
        engine.shutdown().await;
        holder.shutdown().await;
    }

    /// Whether `engine`'s device reports another device announcing its
    /// name, without having renamed itself.
    fn engine_conflict(engine: &Engine) -> bool {
        engine.device.name_conflict().is_some_and(|c| c.renamed_to.is_none())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_driver_turns_the_apps_audio_red() {
        let dir = std::env::temp_dir().join(format!("ovsc-ca-health-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let socket = dir.join("control.sock");
        let mut cfg = app_config("127.0.0.1", 24876);
        cfg.coreaudio.control_socket = socket.to_str().unwrap().into();
        let registry = MemRegistry::new();
        let (stop_tx, mut stop) = mpsc::unbounded_channel();
        let daemon = serve(cfg, None, service(&registry), quiet(), &mut stop);
        let app = async {
            let (_client, received, w) = attach(&registry).await;
            let (_mapped, view) = map(&w);
            let running =
                || view.daemon().flags.load(Ordering::Acquire) & DAEMON_ENGINE_RUNNING != 0;
            wait_for("the engine", running).await;
            // The driver does IO through an open gate, as its threads would
            // write it into the region.
            let p = view.plugin();
            p.plugin_instance.store(hello().instance, Ordering::Relaxed);
            p.io_clients.store(1, Ordering::Relaxed);
            p.gate.store(1, Ordering::Relaxed);
            p.io_heartbeat_ns.store(ovsc_clock::local_now_ns(), Ordering::Relaxed);
            let s = status(&socket).await;
            assert!(s.driver.audio_flowing && !s.driver.faulted, "{:?}", s.driver);
            assert_eq!(s.urgent_warnings, 0, "{:?}", s.warnings);

            // It catches a panic: silent, whatever its gate says.
            p.faulted.store(1, Ordering::Relaxed);
            let s = status(&socket).await;
            assert!(!s.driver.audio_flowing && s.driver.faulted, "{:?}", s.driver);
            assert_eq!(s.urgent_warnings, 1, "{:?}", s.warnings);
            assert!(s.warnings[0].contains("sudo killall coreaudiod"), "{:?}", s.warnings);

            stop_tx.send("test").unwrap();
            wait_for("the bye", || received.got_bye()).await;
        };
        let (result, ()) = tokio::join!(daemon, app);
        result.unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_engine_runs_on_the_regions_rings() {
        let cfg = app_config("127.0.0.1", 24810);
        let region = HalRegion::create(VERSION).unwrap();
        let engine = Engine::start(&cfg, &region, None).await.unwrap();
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
