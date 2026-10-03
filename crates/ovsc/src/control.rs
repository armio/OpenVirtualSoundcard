//! The daemon's control socket ([`ovsc_control`]): the status the
//! OpenVirtualSoundcard app shows and the settings it changes.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use ovsc_clock::{ClockState, MediaClock};
use ovsc_control::{
    self as ctl, ClockInfo, DeviceStatus, DriverInfo, InterfaceInfo, PacketCounts, Request,
    Response, Restart, RxChannelStatus, Settings, SettingsChange, Status, TxFlowStatus,
};
use ovsc_core::{Device, SavedState};
use ovsc_hal_server::HalStatus;
use ovsc_proto::arc::SubscriptionStatus;
use ovsc_proto::discovery::validate_device_name;

use crate::config::{AppConfig, ClockSource};

/// How long the clock may take to lock before the app is told why it may not.
const LOCK_WARNING_AFTER: Duration = Duration::from_secs(20);

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
    let hal = &view.hal;
    let driver = DriverInfo {
        connected: hal.peers > 0,
        io_running: hal.io_clients > 0,
        audio_flowing: hal.gate,
        detail: hal.to_string(),
    };
    let mut warnings = Vec::new();
    if !driver.connected {
        warnings.push(
            "Core Audio has not loaded the OpenVirtualSoundcard driver. Restart Core Audio with \
             `sudo killall coreaudiod`, or reinstall OpenVirtualSoundcard."
                .into(),
        );
    }
    if let Some((d, _, running)) = view.engine {
        if !clock.locked && cfg.clock.source == ClockSource::Ptp && running > LOCK_WARNING_AFTER {
            warnings.push(format!(
                "The clock has not locked: no Dante clock leader is reaching this Mac on {}. \
                 Check the cable and the interface, and see \"The clock does not lock\" in \
                 docs/MACOS.md.",
                d.info().iface.name
            ));
        }
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
    }
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
    ClockInfo {
        source: clock_source(cfg).into(),
        state: format!("{:?}", s.state),
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
        },
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
    let (name, latency_ns) = match view.engine {
        Some((dev, _, _)) => (dev.name(), dev.configured_latency_ns()),
        None => (
            saved.name.clone().unwrap_or_else(|| d.name.clone()),
            saved.latency_ns.unwrap_or_else(|| d.latency_ns()),
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
/// and channel counts in the configuration file, the name and latency with
/// the device's saved state, like a controller's changes.
pub fn apply(view: &View<'_>, change: &SettingsChange) -> anyhow::Result<Restart> {
    let cfg = view.cfg;
    let device = view.engine.map(|(d, _, _)| d);
    if let Some(name) = &change.name {
        validate_device_name(name)
            .map_err(|e| anyhow::anyhow!("invalid device name {name:?}: {e}"))?;
    }
    let latency_ns = match change.latency_ms {
        Some(ms) if !(ctl::LATENCY_MIN_MS..=ctl::LATENCY_MAX_MS).contains(&ms) => {
            bail!(
                "the latency must be between {} and {} ms",
                ctl::LATENCY_MIN_MS,
                ctl::LATENCY_MAX_MS
            )
        }
        Some(ms) => Some((ms * 1e6).round() as u32),
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
    if (change.name.is_some() || latency_ns.is_some()) && state_file.is_none() && device.is_none() {
        bail!("the configuration has no state_file to save the name and latency in");
    }

    // Everything checked: save.
    let mut restart = Restart::None;
    if let Some((path, text)) = new_config {
        write_config(path, &text)?;
        restart = Restart::Daemon;
    }
    if let Some(path) = &state_file {
        SavedState::update(path, |s| {
            if let Some(name) = &change.name {
                s.name = Some(name.clone());
            }
            if let Some(ns) = latency_ns {
                s.latency_ns = Some(ns);
            }
        })
        .with_context(|| format!("saving {}", path.display()))?;
    }
    if let Some(d) = device {
        if let Some(name) = &change.name {
            d.rename(name)?;
        }
        if let Some(ns) = latency_ns {
            if ns != d.info().latency_ns && restart != Restart::Daemon {
                d.request_latency(ns)?;
                restart = Restart::Device;
            } else if ns != d.configured_latency_ns() {
                d.request_latency(ns)?;
            }
        }
    }
    Ok(restart)
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
