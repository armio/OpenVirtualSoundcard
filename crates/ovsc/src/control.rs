//! The daemon's control socket ([`ovsc_control`]): the status the
//! OpenVirtualSoundcard app shows and the settings it changes, and the
//! health tracker behind the status's warnings ([`health`]).

mod health;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use ovsc_clock::{ClockState, MediaClock, local_now_ns};
use ovsc_control::{
    self as ctl, ClockInfo, DeviceStatus, DriverInfo, InterfaceInfo, PacketCounts, Request,
    Response, Restart, RxChannelStatus, Settings, SettingsChange, Status, TxFlowStatus,
};
use ovsc_core::config::min_latency_ms;
use ovsc_core::{Device, RemoteChange, SavedState};
use ovsc_hal_server::HalStatus;
use ovsc_proto::arc::SubscriptionStatus;
use ovsc_proto::discovery::validate_device_name;

pub use self::health::Health;
use self::health::{Assessment, EngineReading, Reading};
use crate::config::{AppConfig, ClockSource};

/// How long a controller's change stays a warning. After that it is listed
/// in [`Status::earlier`]: a change made at sound check is no news at the
/// end of the show.
const RECENT_CHANGE: Duration = Duration::from_secs(600);

/// A request from a control connection, and where its answer goes.
pub struct Call {
    pub request: Request,
    pub reply: oneshot::Sender<Response>,
}

/// The control socket's requests, for the daemon to answer.
pub struct Control {
    calls: Option<mpsc::Receiver<Call>>,
}

impl Control {
    /// Listens at `path`, or not at all if it is empty or cannot be bound:
    /// the device runs without the app's control then.
    pub fn listen(path: &str) -> Control {
        if path.is_empty() {
            return Control { calls: None };
        }
        match bind(Path::new(path)) {
            Ok(listener) => {
                info!("control socket for the OpenVirtualSoundcard app at {path}");
                let (tx, rx) = mpsc::channel(16);
                tokio::spawn(accept(listener, tx));
                Control { calls: Some(rx) }
            }
            Err(e) => {
                warn!("no control socket for the OpenVirtualSoundcard app at {path}: {e:#}");
                Control { calls: None }
            }
        }
    }

    /// The next request; never completes without a socket.
    pub async fn next(&mut self) -> Call {
        match &mut self.calls {
            Some(rx) => match rx.recv().await {
                Some(call) => call,
                None => std::future::pending().await,
            },
            None => std::future::pending().await,
        }
    }
}

/// Binds `path`, replacing a stale socket, for root and the admin group.
fn bind(path: &Path) -> anyhow::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("removing the old {}", path.display()));
        }
        _ => {}
    }
    let listener = UnixListener::bind(path)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    }
    if let Some(gid) = group_id("admin") {
        if let Err(e) = std::os::unix::fs::chown(path, None, Some(gid)) {
            warn!("cannot give the admin group the control socket: {e}");
        }
    }
    Ok(listener)
}

/// The id of group `name`, if there is one.
fn group_id(name: &str) -> Option<u32> {
    let name = std::ffi::CString::new(name).ok()?;
    // SAFETY: a NUL-terminated name; the entry is read at once.
    let entry = unsafe { libc::getgrnam(name.as_ptr()) };
    (!entry.is_null()).then(|| unsafe { (*entry).gr_gid })
}

async fn accept(listener: UnixListener, calls: mpsc::Sender<Call>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(connection(stream, calls.clone()));
            }
            Err(e) => {
                warn!("control socket: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

/// Answers one connection's requests, one line each, until it closes.
async fn connection(stream: UnixStream, calls: mpsc::Sender<Call>) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let response = match ctl::decode_line::<Request>(&line) {
            Ok(request) => {
                let (reply, answer) = oneshot::channel();
                if calls.send(Call { request, reply }).await.is_err() {
                    break;
                }
                answer.await.unwrap_or_else(|_| Response::Error {
                    message: "the daemon is shutting down".into(),
                })
            }
            Err(e) => Response::Error { message: format!("unreadable request: {e}") },
        };
        if write.write_all(ctl::encode_line(&response).as_bytes()).await.is_err() {
            break;
        }
    }
    debug!("control connection closed");
}

/// What the daemon must do after a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum After {
    Nothing,
    /// Exit, for launchd to start the daemon again with the new
    /// configuration.
    RestartDaemon,
}

/// The engine as a call sees it.
pub struct View<'a> {
    pub cfg: &'a AppConfig,
    /// The configuration file, if the daemon has one.
    pub config_path: Option<&'a Path>,
    /// The running engine: its device, clock, and how long it has run.
    pub engine: Option<(&'a Device, &'a MediaClock, Duration)>,
    pub hal: HalStatus,
    /// Why the engine does not run.
    pub engine_error: Option<&'a str>,
    /// The daemon's health tracker, which keeps the recent readings. `None`
    /// judges the current reading alone: states, but no counter rises.
    pub health: Option<&'a Mutex<Health>>,
}

/// The health tracker, also after a panic while it was held: it only
/// keeps readings.
fn lock(health: &Mutex<Health>) -> MutexGuard<'_, Health> {
    health.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Takes a reading for the health tracker and logs the warnings that
/// appeared or cleared. The daemon calls it every second, so that the
/// health is followed and logged whether or not the app asks.
pub fn watch(view: &View<'_>) {
    let Some(health) = view.health else { return };
    let mut h = lock(health);
    let now = Instant::now();
    let a = h.observe(now, &reading(view));
    h.log(now, &a.warnings);
}

/// The health in a few words, for the daemon's status line: `healthy`, or
/// the warnings of the last reading.
pub fn health_summary(health: &Mutex<Health>) -> String {
    lock(health).summary()
}

/// The engine (re)started: its counters start over, and its first seconds
/// do not count.
pub fn restarted(health: &Mutex<Health>) {
    lock(health).restarted(Instant::now());
}

/// [`watch`] for the daemons of the other backends, which have no Core
/// Audio driver: the device's and clock's health only. `clock_age` is how
/// long `clock` has run.
pub fn watch_device(
    health: &mut Health,
    cfg: &AppConfig,
    device: &Device,
    clock: &MediaClock,
    clock_age: Duration,
) {
    let now = Instant::now();
    let reading =
        Reading { hal: None, engine: Some(engine_reading(cfg, device, clock, clock_age)) };
    let a = health.observe(now, &reading);
    health.log(now, &a.warnings);
}

/// What the health tracker reads from `view`.
fn reading(view: &View<'_>) -> Reading {
    Reading {
        hal: Some(view.hal),
        engine: view.engine.map(|(d, clock, age)| engine_reading(view.cfg, d, clock, age)),
    }
}

fn engine_reading(
    cfg: &AppConfig,
    d: &Device,
    clock: &MediaClock,
    clock_age: Duration,
) -> EngineReading {
    EngineReading {
        stats: d.stats(),
        clock: clock.status(),
        ptp: cfg.clock.source == ClockSource::Ptp,
        clock_age,
        local_ns: local_now_ns(),
        latency_ms: d.info().latency_ns as f64 / 1e6,
        interface: d.info().iface.name.clone(),
        rx: d.rx_channels(),
        stopped_part: d.stopped_part(),
        name_conflict: d.name_conflict(),
    }
}

/// Answers `call`.
pub fn answer(call: Call, view: &View<'_>) -> After {
    let (response, after) = match &call.request {
        Request::Status => (Response::Status(Box::new(status(view))), After::Nothing),
        Request::Settings => (Response::Settings(settings(view)), After::Nothing),
        Request::Interfaces => (Response::Interfaces { interfaces: interfaces() }, After::Nothing),
        Request::Apply { change } => match apply(view, change) {
            Ok(restart) => {
                info!("settings changed by the OpenVirtualSoundcard app: {change:?}");
                let after =
                    if restart == Restart::Daemon { After::RestartDaemon } else { After::Nothing };
                (Response::Applied { restart }, after)
            }
            Err(e) => (Response::Error { message: format!("{e:#}") }, After::Nothing),
        },
    };
    let _ = call.reply.send(response);
    after
}

fn status(view: &View<'_>) -> Status {
    let cfg = view.cfg;
    let device = view.engine.map(|(d, _, _)| device_status(d));
    let clock = match view.engine {
        Some((_, clock, _)) => clock_info(cfg, clock),
        None => ClockInfo {
            source: clock_source(cfg).into(),
            state: "Unlocked".into(),
            locked: false,
            leader: None,
            offset_ns: 0,
            path_delay_ns: 0,
            freq_offset_ppm: 0.0,
        },
    };
    let reading = reading(view);
    let now = Instant::now();
    let health = match view.health {
        Some(h) => lock(h).observe(now, &reading),
        None => Health::new().observe(now, &reading),
    };
    let hal = &view.hal;
    let connected = hal.peers > 0;
    let driver = driver_info(hal, &health);

    // The urgent first: audio is down now in at least one direction.
    let mut warnings = Vec::new();
    if !connected {
        warnings.push(
            "Core Audio has not loaded the OpenVirtualSoundcard driver, so the device is \
             missing. Restart Core Audio with `sudo killall coreaudiod`, or reinstall \
             OpenVirtualSoundcard."
                .into(),
        );
    }
    let (urgent, other): (Vec<_>, Vec<_>) = health.warnings.into_iter().partition(|w| w.urgent);
    warnings.extend(urgent.into_iter().map(|w| w.text));
    let urgent_warnings = warnings.len();
    if let Some(problem) = view.engine.and_then(|(d, _, _)| d.saved_state_problem()) {
        warnings.push(format!(
            "The saved names and subscriptions could not be read at start: {problem}. Check \
             the subscriptions in Dante Controller."
        ));
    }
    warnings.extend(other.into_iter().map(|w| w.text));
    let mut earlier = Vec::new();
    if let Some((d, _, _)) = view.engine {
        let (recent, older) = remote_changes_words(
            &d.remote_changes().recent(),
            SystemTime::now(),
            d.remote_settings_locked(),
        );
        warnings.extend(recent);
        earlier = older;
    }
    Status {
        protocol: ctl::PROTOCOL_VERSION,
        version: env!("CARGO_PKG_VERSION").into(),
        engine_running: view.engine.is_some(),
        engine_error: view.engine_error.filter(|e| !e.is_empty()).map(str::to_owned),
        device,
        clock,
        driver,
        warnings,
        urgent_warnings,
        earlier,
    }
}

/// The driver as the app shows it, judged by the health tracker: audio
/// does not flow through a failed driver, nor while Core Audio has stopped
/// calling it, whatever the gate says.
fn driver_info(hal: &HalStatus, health: &Assessment) -> DriverInfo {
    let flowing = hal.gate && !health.driver_failed && !health.io_stalled;
    DriverInfo {
        connected: hal.peers > 0,
        io_running: hal.io_clients > 0,
        audio_flowing: flowing,
        faulted: health.driver_failed,
        audio_trouble: flowing && health.audible(),
        detail: hal.to_string(),
    }
}

/// Controllers' `changes` (oldest first, as kept) in words, newest first:
/// those of the last [`RECENT_CHANGE`] as warnings, the older ones apart.
/// The newest come first because the change that explains a dropout is
/// usually the last one. A change from the future (the clock set back)
/// counts as recent.
fn remote_changes_words(
    changes: &[RemoteChange],
    now: SystemTime,
    locked: bool,
) -> (Vec<String>, Vec<String>) {
    let (mut recent, mut older) = (Vec::new(), Vec::new());
    for c in changes.iter().rev() {
        let words = remote_change_words(c, now, locked);
        match now.duration_since(c.at) {
            Ok(ago) if ago >= RECENT_CHANGE => older.push(words),
            _ => recent.push(words),
        }
    }
    (recent, older)
}

/// A controller's change, or refused attempt, as a warning for the app:
/// who, what and when, so that a dropout mid-show has an explanation.
/// `locked` is whether the show lock is on now.
fn remote_change_words(c: &RemoteChange, now: SystemTime, locked: bool) -> String {
    let ago = ago(now.duration_since(c.at).unwrap_or_default());
    // Dante Controller on this Mac reaches ARC over 127.0.0.1.
    let who = if c.from.ip().is_loopback() {
        "A controller on this Mac".to_owned()
    } else {
        format!("A controller at {}", c.from.ip())
    };
    if c.refused {
        format!(
            "{who} tried to change {} {ago}. It was refused: settings from the network are \
             locked.",
            c.what
        )
    } else if locked {
        format!("{who} changed {} {ago}, before the settings were locked.", c.what)
    } else {
        format!(
            "{who} changed {} {ago}. To refuse such changes during a show, lock the settings \
             from the network (Settings).",
            c.what
        )
    }
}

/// How long ago, in words: `12 s ago`, `5 min ago`, `2 h 5 min ago`.
fn ago(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s} s ago"),
        60..3600 => format!("{} min ago", s / 60),
        _ => format!("{} h {} min ago", s / 3600, s % 3600 / 60),
    }
}

/// The devices receiving flows from `d`, for a log line: their names, or
/// their addresses when they gave none.
fn receivers(d: &Device) -> String {
    let flows = d.tx_flows();
    let mut names: Vec<String> = flows
        .iter()
        .map(|f| f.receiver.clone().unwrap_or_else(|| f.destination.ip().to_string()))
        .collect();
    names.sort();
    names.dedup();
    names.join(", ")
}

fn clock_source(cfg: &AppConfig) -> &'static str {
    match cfg.clock.source {
        ClockSource::Ptp => "ptp",
        ClockSource::Free => "free",
        ClockSource::System => "system",
    }
}

fn clock_info(cfg: &AppConfig, clock: &MediaClock) -> ClockInfo {
    let s = clock.status();
    // Without a leader, a clock that runs on until a deadline holds over:
    // audio still flows, for now (MACOS.md, "Clock states and silence").
    let holdover = s.master.is_none() && s.holdover_until_ns.is_some();
    ClockInfo {
        source: clock_source(cfg).into(),
        state: if holdover { "Holdover".to_owned() } else { format!("{:?}", s.state) },
        locked: matches!(s.state, ClockState::Locked | ClockState::FreeRunning),
        leader: s.master.map(|m| m.addr.to_string()),
        offset_ns: s.offset_ns,
        path_delay_ns: s.mean_path_delay_ns,
        freq_offset_ppm: s.freq_offset_ppb / 1000.0,
    }
}

fn device_status(d: &Device) -> DeviceStatus {
    let info = d.info();
    let (_, tx_names) = d.channel_names();
    let stats = d.stats();
    DeviceStatus {
        name: d.name(),
        interface: info.iface.name.clone(),
        ip: info.iface.ip.to_string(),
        sample_rate: info.sample_rate,
        bits_per_sample: info.format.bits(),
        latency_ms: info.latency_ns as f64 / 1e6,
        rx_channels: d
            .rx_channels()
            .into_iter()
            .map(|c| RxChannelStatus {
                receiving: matches!(
                    c.status,
                    SubscriptionStatus::ReceivingUnicast | SubscriptionStatus::ReceivingMulticast
                ),
                state: subscription_words(c.status).into(),
                source: c.subscription.map(|(ch, dev)| format!("{ch}@{dev}")),
                name: c.name,
            })
            .collect(),
        tx_channels: tx_names,
        tx_flows: d
            .tx_flows()
            .into_iter()
            .map(|f| TxFlowStatus {
                receiver: f.receiver,
                destination: f.destination.to_string(),
                channels: f.channels,
            })
            .collect(),
        packets: PacketCounts {
            tx: stats.tx_packets,
            tx_underruns: stats.tx_underruns,
            rx: stats.rx_packets,
            rx_late: stats.rx_late_packets,
            rx_lost: stats.rx_lost_packets,
        },
        remote_settings_locked: d.remote_settings_locked(),
    }
}

/// A subscription's state in plain words.
fn subscription_words(s: SubscriptionStatus) -> &'static str {
    match s {
        SubscriptionStatus::None => "",
        SubscriptionStatus::Unresolved => "transmitter not found",
        SubscriptionStatus::InProgress => "connecting",
        SubscriptionStatus::TxNoFlows => "transmitter has no free flows",
        SubscriptionStatus::TxFail => "transmitter refused",
        SubscriptionStatus::ReceivingUnicast | SubscriptionStatus::ReceivingMulticast => {
            "receiving"
        }
        SubscriptionStatus::Other(_) => "unknown state",
    }
}

fn settings(view: &View<'_>) -> Settings {
    let d = &view.cfg.device;
    let saved = d.state_file.as_deref().and_then(SavedState::load).unwrap_or_default();
    let (name, latency_ns, locked) = match view.engine {
        Some((dev, _, _)) => {
            (dev.name(), dev.configured_latency_ns(), dev.remote_settings_locked())
        }
        None => (
            saved.name.clone().unwrap_or_else(|| d.name.clone()),
            saved.latency_ns.unwrap_or_else(|| d.latency_ns()),
            saved.lock_remote_settings.unwrap_or(d.lock_remote_settings),
        ),
    };
    Settings {
        name,
        interface: d.interface.clone(),
        sample_rate: d.sample_rate,
        bits_per_sample: d.bits_per_sample,
        rx_channels: d.rx_channels.names().len() as u16,
        tx_channels: d.tx_channels.names().len() as u16,
        latency_ms: latency_ns as f64 / 1e6,
        lock_remote_settings: locked,
    }
}

fn interfaces() -> Vec<InterfaceInfo> {
    ovsc_core::net::list_interfaces()
        .into_iter()
        .map(|i| InterfaceInfo {
            description: i.description.unwrap_or_default(),
            ipv4: i.ipv4.iter().map(|a| a.to_string()).collect(),
            default_route: i.default_route,
            name: i.name,
        })
        .collect()
}

/// Checks `change`, then saves it: the interface, sample rate, bit depth
/// and channel counts in the configuration file, the name, latency and
/// show lock with the device's saved state, like a controller's changes.
/// The show lock only stops controllers: the app's own changes apply.
pub fn apply(view: &View<'_>, change: &SettingsChange) -> anyhow::Result<Restart> {
    let cfg = view.cfg;
    let device = view.engine.map(|(d, _, _)| d);
    if let Some(name) = &change.name {
        validate_device_name(name)
            .map_err(|e| anyhow::anyhow!("invalid device name {name:?}: {e}"))?;
    }
    let latency_ns = match change.latency_ms {
        Some(ms) => Some(check_latency(ms, cfg.device.tx_guard_us)?),
        None => None,
    };
    let host = change.interface.is_some()
        || change.sample_rate.is_some()
        || change.bits_per_sample.is_some()
        || change.rx_channels.is_some()
        || change.tx_channels.is_some();
    let new_config = if host {
        let Some(path) = view.config_path else {
            bail!("the daemon runs without a configuration file to save these settings in")
        };
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        edit_config(&text, change)?.map(|new| (path, new))
    } else {
        None
    };
    let state_file: Option<PathBuf> = cfg.device.state_file.clone();
    let lock = change.lock_remote_settings;
    if (change.name.is_some() || latency_ns.is_some() || lock.is_some())
        && state_file.is_none()
        && device.is_none()
    {
        bail!("the configuration has no state_file to save the name, latency and lock in");
    }

    // Everything checked: save.
    let mut restart = Restart::None;
    if let Some((path, text)) = new_config {
        write_config(path, &text)?;
        restart = Restart::Daemon;
    }
    match device {
        // A running device saves its own state file: changed through it,
        // the change is saved with everything else, by the one writer.
        Some(d) => {
            if let Some(locked) = lock {
                if locked != d.remote_settings_locked() {
                    info!(
                        "settings from the network {} by the app",
                        if locked { "locked" } else { "unlocked" }
                    );
                }
                d.lock_remote_settings(locked);
            }
            if let Some(name) = &change.name {
                let old = d.name();
                if *name != old && !d.tx_flows().is_empty() {
                    // Receivers subscribe by `channel@device`; the old names'
                    // records are withdrawn (mDNS goodbyes).
                    warn!(
                        "renaming the device from {old} to {name} while {} receive from it: \
                         their subscriptions to {old}'s channels stop resolving until they are \
                         patched again",
                        receivers(d)
                    );
                }
                d.rename(name)?;
            }
            if let Some(ns) = latency_ns {
                if ns != d.info().latency_ns && restart != Restart::Daemon {
                    if !d.tx_flows().is_empty() {
                        warn!(
                            "the new latency restarts the network engine: the flows {} receive \
                             from this device stop until each of them requests its flow again",
                            receivers(d)
                        );
                    }
                    d.request_latency(ns)?;
                    restart = Restart::Device;
                } else if ns != d.configured_latency_ns() {
                    d.request_latency(ns)?;
                }
            }
        }
        None => {
            if let Some(path) = &state_file {
                SavedState::update(path, |s| {
                    if let Some(name) = &change.name {
                        s.name = Some(name.clone());
                    }
                    if let Some(ns) = latency_ns {
                        s.latency_ns = Some(ns);
                    }
                    if lock.is_some() {
                        s.lock_remote_settings = lock;
                    }
                })
                .with_context(|| format!("saving {}", path.display()))?;
            }
        }
    }
    Ok(restart)
}

/// A receive latency of `ms` from the app, in ns, if the device allows it
/// with a transmit guard of `tx_guard_us`: the rule of
/// `ovsc_core::config::check_latency_ms`, which also holds controllers'
/// writes and the configuration, so that the device never refuses a
/// latency it restarts to apply.
fn check_latency(ms: f64, tx_guard_us: u32) -> anyhow::Result<u32> {
    let min = ctl::LATENCY_MIN_MS.max(min_latency_ms(tx_guard_us));
    if !(min..=ctl::LATENCY_MAX_MS).contains(&ms) {
        bail!(
            "the latency must be between {min} and {} ms (at least the transmit guard of {} ms \
             plus 0.5 ms)",
            ctl::LATENCY_MAX_MS,
            tx_guard_us as f64 / 1000.0
        )
    }
    Ok((ms * 1e6).round() as u32)
}

/// `text` with `change`'s host settings in its `[device]` table, keeping
/// its comments and layout; `None` if nothing changes. The result must be a
/// valid configuration for a Mac.
fn edit_config(text: &str, change: &SettingsChange) -> anyhow::Result<Option<String>> {
    use toml_edit::{DocumentMut, Item, Table, Value};

    let mut doc: DocumentMut = text.parse().context("the configuration file is not valid TOML")?;
    let device = doc
        .entry("device")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .context("[device] is not a table")?;
    let mut set = |key: &str, v: Value| match device.get_mut(key).and_then(Item::as_value_mut) {
        Some(old) => {
            let decor = old.decor().clone();
            *old = v;
            *old.decor_mut() = decor;
        }
        None => {
            device.insert(key, Item::Value(v));
        }
    };
    if let Some(i) = &change.interface {
        set("interface", i.as_str().into());
    }
    if let Some(r) = change.sample_rate {
        set("sample_rate", i64::from(r).into());
    }
    if let Some(b) = change.bits_per_sample {
        set("bits_per_sample", i64::from(b).into());
    }
    if let Some(n) = change.rx_channels {
        set("rx_channels", i64::from(n).into());
    }
    if let Some(n) = change.tx_channels {
        set("tx_channels", i64::from(n).into());
    }
    let new = doc.to_string();
    if new == text {
        return Ok(None);
    }
    let cfg: AppConfig = toml::from_str(&new).context("the changed configuration")?;
    cfg.validate_for(true)?;
    Ok(Some(new))
}

/// Replaces the configuration file with `text`, atomically, keeping its
/// permissions.
fn write_config(path: &Path, text: &str) -> anyhow::Result<()> {
    let tmp = path.with_extension("toml.new");
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"# OpenVirtualSoundcard on macOS.
[device]
# Name shown in Dante Controller.
name = "ovsc"
interface = ""               # empty: the default route
sample_rate = 48000          # 44100, 48000, ...
rx_channels = 8
tx_channels = 8
latency_ms = 4.0

[audio]
backend = "coreaudio"
"#;

    #[test]
    fn config_edits_keep_comments_and_layout() {
        let change = SettingsChange {
            interface: Some("en7".into()),
            sample_rate: Some(96_000),
            rx_channels: Some(16),
            ..Default::default()
        };
        let new = edit_config(CONFIG, &change).unwrap().unwrap();
        assert!(
            new.contains("interface = \"en7\"               # empty: the default route"),
            "{new}"
        );
        assert!(new.contains("sample_rate = 96000          # 44100, 48000, ..."), "{new}");
        assert!(new.contains("rx_channels = 16\n"), "{new}");
        assert!(new.contains("# Name shown in Dante Controller.\nname = \"ovsc\""), "{new}");
        let cfg: AppConfig = toml::from_str(&new).unwrap();
        assert_eq!((cfg.device.sample_rate, cfg.device.interface.as_str()), (96_000, "en7"));
    }

    #[test]
    fn config_edits_that_change_nothing_or_break_it() {
        let same = SettingsChange { sample_rate: Some(48_000), ..Default::default() };
        assert_eq!(edit_config(CONFIG, &same).unwrap(), None);
        let bad = SettingsChange { sample_rate: Some(12_345), ..Default::default() };
        assert!(edit_config(CONFIG, &bad).is_err());
        let none = SettingsChange { rx_channels: Some(0), ..Default::default() };
        assert!(edit_config(CONFIG, &none).is_err());
    }

    #[test]
    fn latencies_from_the_app_follow_the_devices_rule() {
        use ovsc_core::config::{MAX_LATENCY_MS, MIN_LATENCY_MS, check_latency_ms};

        // The app's limits are the device's, so nothing passes here that the
        // device would refuse when it restarts with it.
        assert_eq!((ctl::LATENCY_MIN_MS, ctl::LATENCY_MAX_MS), (MIN_LATENCY_MS, MAX_LATENCY_MS));
        assert_eq!(check_latency(1.0, 500).unwrap(), 1_000_000);
        assert_eq!(check_latency(2.5, 500).unwrap(), 2_500_000);
        for bad in [0.25, 0.5, 0.99, 40.5, f64::NAN] {
            let e = check_latency(bad, 500).unwrap_err().to_string();
            assert!(e.starts_with("the latency must be between 1 and 40 ms"), "{e}");
        }
        // A longer transmit guard raises the least latency.
        assert!(check_latency(1.0, 800).is_err());
        assert_eq!(check_latency(1.3, 800).unwrap(), 1_300_000);
        for guard in [0, 500, 800, 2000] {
            for ms in [0.5, 1.0, 1.2, 1.3, 2.5, 3.0, 40.0] {
                assert_eq!(
                    check_latency(ms, guard).is_ok(),
                    check_latency_ms("latency", ms, guard).is_ok(),
                    "{ms} ms, guard {guard} us"
                );
            }
        }
    }

    #[test]
    fn controllers_changes_read_as_warnings() {
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let change = |refused| RemoteChange {
            at,
            from: "192.168.1.20:51000".parse().unwrap(),
            what: "the receive latency to 2 ms".into(),
            refused,
        };
        let later = |s| at + Duration::from_secs(s);
        assert_eq!(
            remote_change_words(&change(false), later(300), false),
            "A controller at 192.168.1.20 changed the receive latency to 2 ms 5 min ago. To \
             refuse such changes during a show, lock the settings from the network (Settings)."
        );
        assert_eq!(
            remote_change_words(&change(false), later(5), true),
            "A controller at 192.168.1.20 changed the receive latency to 2 ms 5 s ago, before \
             the settings were locked."
        );
        assert_eq!(
            remote_change_words(&change(true), later(7_500), true),
            "A controller at 192.168.1.20 tried to change the receive latency to 2 ms 2 h 5 min \
             ago. It was refused: settings from the network are locked."
        );
        // A clock set back reads as just now.
        assert!(
            remote_change_words(&change(true), later(0) - Duration::from_secs(9), true)
                .contains(" 0 s ago")
        );
        // Dante Controller on this Mac talks to the device over 127.0.0.1.
        let local = RemoteChange { from: "127.0.0.1:51000".parse().unwrap(), ..change(true) };
        assert!(
            remote_change_words(&local, later(60), true)
                .starts_with("A controller on this Mac tried to change")
        );
    }

    #[test]
    fn controllers_changes_age_out_of_the_warnings() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let change = |ago: u64, what: &str| RemoteChange {
            at: now - Duration::from_secs(ago),
            from: "192.168.1.20:51000".parse().unwrap(),
            what: what.into(),
            refused: false,
        };
        // Oldest first, as the device keeps them; one from the future.
        let changes = [
            change(3_600, "the name to a"),
            change(601, "the name to b"),
            change(599, "the receive latency to 2 ms"),
            RemoteChange { at: now + Duration::from_secs(5), ..change(0, "the name to c") },
        ];
        let (recent, older) = remote_changes_words(&changes, now, false);
        let has = |list: &[String], what: &str| list.iter().any(|w| w.contains(what));
        assert_eq!((recent.len(), older.len()), (2, 2));
        // Newest first in both.
        assert!(recent[0].contains("the name to c") && recent[1].contains("latency to 2 ms"));
        assert!(older[0].contains("the name to b 10 min ago"), "{older:?}");
        assert!(has(&older, "the name to a 1 h 0 min ago"));
    }

    fn view_with<'a>(cfg: &'a AppConfig, hal: HalStatus, health: &'a Mutex<Health>) -> View<'a> {
        View { cfg, config_path: None, engine: None, hal, engine_error: None, health: Some(health) }
    }

    /// A driver connected, with an application doing IO through an open gate.
    fn playing() -> HalStatus {
        HalStatus {
            peers: 1,
            engine_running: true,
            io_clients: 1,
            gate: true,
            min_output_margin: 40,
            min_input_margin: 40,
            io_heartbeat_age: Some(Duration::from_millis(1)),
            plugin_instance: 3,
            ..Default::default()
        }
    }

    #[test]
    fn a_failed_driver_is_not_flowing_and_says_how_to_recover() {
        let cfg: AppConfig = toml::from_str(CONFIG).unwrap();
        let failed = HalStatus { faulted: true, ..playing() };
        // Without a tracker's history: the state alone says it.
        let unused = Mutex::default();
        let view = View { health: None, ..view_with(&cfg, failed, &unused) };
        let s = status(&view);
        // The gate is open, but nothing flows through a failed driver.
        assert!(view.hal.gate);
        assert!(s.driver.faulted && !s.driver.audio_flowing && !s.driver.audio_trouble);
        assert_eq!(s.urgent_warnings, 1);
        assert!(s.warnings[0].contains("`sudo killall coreaudiod`"), "{:?}", s.warnings);
        // The detail still carries the raw status line.
        assert!(s.driver.detail.ends_with("faulted=1"), "{}", s.driver.detail);
    }

    #[test]
    fn a_closed_gate_while_an_application_plays_is_urgent() {
        let cfg: AppConfig = toml::from_str(CONFIG).unwrap();
        let health = Mutex::new(Health::new());
        let closed = HalStatus { gate: false, ..playing() };
        // The tracker saw the gate close five seconds ago.
        let before = Instant::now().checked_sub(Duration::from_secs(5)).unwrap();
        let view = view_with(&cfg, closed, &health);
        lock(&health).observe(before, &reading(&view));
        let s = status(&view);
        assert!(!s.driver.audio_flowing && !s.driver.faulted && s.driver.io_running);
        assert_eq!(s.urgent_warnings, 1);
        assert!(s.warnings[0].contains("holds the audio back"), "{:?}", s.warnings);
        assert!(s.warnings[0].contains("network engine is not running"), "{:?}", s.warnings);
    }

    #[test]
    fn rising_driver_counters_make_flowing_audio_troubled() {
        let cfg: AppConfig = toml::from_str(CONFIG).unwrap();
        let health = Mutex::new(Health::new());
        let now = Instant::now();
        let ago = |s| now.checked_sub(Duration::from_secs(s)).unwrap();
        // IO started 25 s ago and ran clean until 20 s ago.
        lock(&health).observe(ago(25), &reading(&view_with(&cfg, playing(), &health)));
        lock(&health).observe(ago(20), &reading(&view_with(&cfg, playing(), &health)));
        let late = HalStatus { late_output_cycles: 3, min_output_margin: -12, ..playing() };
        let s = status(&view_with(&cfg, late, &health));
        assert!(s.driver.audio_flowing && s.driver.audio_trouble);
        assert_eq!(s.urgent_warnings, 0);
        assert!(
            s.warnings[0].starts_with("3 output cycles from the application came too late"),
            "{:?}",
            s.warnings
        );
        assert!(s.warnings[0].contains("-12 frames"), "{:?}", s.warnings);
        // The daemon's second tick logs it and the status line lists it.
        watch(&view_with(&cfg, late, &health));
        assert_eq!(health_summary(&health), "warnings: 3 late output cycles");
        // Thirty seconds after the rise, it has cleared.
        let mut h = lock(&health);
        let a = h.observe(now + Duration::from_secs(31), &reading(&view_with(&cfg, late, &health)));
        assert!(a.warnings.is_empty(), "{a:?}");
    }

    #[tokio::test]
    async fn a_running_device_is_read_for_the_tracker() {
        use ovsc_core::{Channels, DeviceConfig, Ports};

        let base = 24886;
        let config = DeviceConfig {
            name: "health-dev".into(),
            interface: "127.0.0.1".into(),
            tx_channels: Channels::Count(2),
            rx_channels: Channels::Count(2),
            latency_ms: 2.0,
            ports: Ports { arc: base, cmc: base + 1, flow_control: base + 2, settings: base + 3 },
            discovery: false,
            process_id: 64,
            ..Default::default()
        };
        let clock = ovsc_clock::free_running_clock();
        let device = Device::start(config, clock.clone()).await.unwrap();
        device.subscribe(2, "lead", "stagebox").unwrap();
        let mut cfg: AppConfig = toml::from_str(CONFIG).unwrap();
        cfg.clock.source = ClockSource::Free;
        let r = engine_reading(&cfg, &device, &clock, Duration::from_secs(3));
        assert!(!r.ptp && r.stopped_part.is_none() && r.name_conflict.is_none());
        assert_eq!((r.interface.as_str(), r.latency_ms, r.clock_age.as_secs()), ("lo", 2.0, 3));
        assert_eq!(r.rx.len(), 2);
        assert_eq!(r.rx[1].subscription, Some(("lead".into(), "stagebox".into())));
        assert_eq!(r.clock.state, ClockState::FreeRunning);
        // The other daemons' tracker: nothing wrong with a fresh device.
        let mut health = Health::new();
        watch_device(&mut health, &cfg, &device, &clock, Duration::from_secs(3));
        assert_eq!(health.summary(), "healthy");
        device.shutdown().await;
    }

    #[test]
    fn a_missing_driver_is_urgent() {
        let cfg: AppConfig = toml::from_str(CONFIG).unwrap();
        let s = status(&view_with(&cfg, HalStatus::default(), &Mutex::default()));
        assert!(!s.driver.connected && !s.driver.faulted);
        assert_eq!(s.urgent_warnings, 1);
        assert!(s.warnings[0].starts_with("Core Audio has not loaded"), "{:?}", s.warnings);
        assert!(s.earlier.is_empty());
    }

    #[test]
    fn subscription_states_read_as_words() {
        assert_eq!(subscription_words(SubscriptionStatus::ReceivingUnicast), "receiving");
        assert_eq!(subscription_words(SubscriptionStatus::Unresolved), "transmitter not found");
        assert_eq!(subscription_words(SubscriptionStatus::None), "");
    }

    #[tokio::test]
    async fn the_socket_answers_one_line_per_request() {
        let dir = std::env::temp_dir().join(format!("ovsc-ctl-{}", std::process::id()));
        let path = dir.join("control.sock");
        let mut control = Control::listen(path.to_str().unwrap());
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let call = control.next().await;
                let r = match call.request {
                    Request::Settings => Response::Error { message: "settings".into() },
                    _ => Response::Applied { restart: Restart::None },
                };
                let _ = call.reply.send(r);
            }
        });
        let path2 = path.clone();
        let answers = tokio::task::spawn_blocking(move || {
            let mut c = ctl::Client::connect_to(&path2).unwrap();
            let a = c.request(&Request::Settings).unwrap();
            let b = c.request(&Request::Interfaces).unwrap();
            (a, b)
        })
        .await
        .unwrap();
        assert_eq!(answers.0, Response::Error { message: "settings".into() });
        assert_eq!(answers.1, Response::Applied { restart: Restart::None });
        server.await.unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o660);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
