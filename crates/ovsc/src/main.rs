//! `ovsc` – the OpenVirtualSoundcard virtual soundcard daemon and network tools.

mod backend;
mod config;
mod control;
mod coreaudio;
mod playback;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use tracing::{info, warn};

use ovsc_clock::ptp::master::{TestMaster, TestMasterConfig};
use ovsc_clock::ptp::{PtpConfig, PtpFollower};
use ovsc_clock::{ClockState, MediaClock, free_running_clock_with_rate, system_clock};
use ovsc_core::client::{self, next_seq};
use ovsc_core::mdns::Mdns;
use ovsc_core::net::Interface;
use ovsc_core::{Channels, Device};
use ovsc_proto::arc::{self, SubscriptionRequest};
use ovsc_proto::discovery::{self, ChannelTxt, parse_txt};
use ovsc_proto::frame::Frame;
use ovsc_soundcard::{Bridge, BridgeConfig};

use crate::config::{AppConfig, BackendKind, ClockSource};

#[derive(Parser)]
#[command(
    version,
    about = "OpenVirtualSoundcard: an open-source Dante-compatible virtual soundcard"
)]
struct Cli {
    /// Log filter, e.g. "info", "debug", "ovsc_core=trace".
    #[arg(long, global = true, default_value = "info", env = "OVSC_LOG")]
    log: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a virtual device.
    Run(RunArgs),
    /// Print an annotated example configuration file.
    ExampleConfig {
        /// Print the macOS configuration installed with the Core Audio
        /// driver instead.
        #[arg(long)]
        macos: bool,
    },
    /// List Dante devices and channels on the network.
    Discover {
        /// Network interface name or IPv4 address.
        #[arg(short, long, default_value = "")]
        interface: String,
        /// Seconds to listen for answers.
        #[arg(long, default_value_t = 3)]
        wait: u64,
        /// Also list every transmit channel.
        #[arg(long)]
        channels: bool,
    },
    /// Show a device's channels and subscriptions.
    Info {
        /// Device name, or IP[:port] of its ARC service.
        device: String,
        #[arg(short, long, default_value = "")]
        interface: String,
    },
    /// Subscribe a receive channel to a transmit channel.
    Route {
        /// Receiving device (name or IP[:port]).
        rx_device: String,
        /// Receive channel number or name.
        rx_channel: String,
        /// Source as CHANNEL@DEVICE.
        source: String,
        #[arg(short, long, default_value = "")]
        interface: String,
    },
    /// Remove a receive channel's subscription.
    Unroute {
        rx_device: String,
        rx_channel: String,
        #[arg(short, long, default_value = "")]
        interface: String,
    },
    /// List audio devices usable by the soundcard backend.
    Soundcards,
    /// Run a development PTPv1 master driven by the system clock, so that
    /// OpenVirtualSoundcard devices can sync without Dante hardware. It goes silent as
    /// soon as it hears another master, but still: never run it on a
    /// production Dante network.
    PtpMaster {
        #[arg(short, long, default_value = "")]
        interface: String,
        #[arg(long, default_value_t = 319)]
        event_port: u16,
        #[arg(long, default_value_t = 320)]
        general_port: u16,
        /// How much faster than this host's clock the master runs, in ppm
        /// (negative: slower), to test how followers cope.
        #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
        rate_ppm: f64,
    },
}

#[derive(clap::Args)]
struct RunArgs {
    /// Configuration file (see `ovsc example-config`).
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// Device name (overrides the configuration file).
    #[arg(short, long)]
    name: Option<String>,
    /// Network interface name or IPv4 address.
    #[arg(short, long)]
    interface: Option<String>,
    /// Number of transmit channels.
    #[arg(long)]
    tx: Option<u16>,
    /// Number of receive channels.
    #[arg(long)]
    rx: Option<u16>,
    /// Receive latency in milliseconds.
    #[arg(long)]
    latency_ms: Option<f64>,
    /// Clock source: ptp (Dante network), free (this host's clock, for a
    /// network without a PTP master) or system (tests only).
    #[arg(long, value_parser = parse_clock)]
    clock: Option<ClockSource>,
    /// Audio backend: none, tone, record, loopback, soundcard, coreaudio.
    #[arg(short, long, value_parser = parse_backend)]
    backend: Option<BackendKind>,
    /// Output file of the `record` backend.
    #[arg(long)]
    record_path: Option<PathBuf>,
}

fn parse_clock(s: &str) -> Result<ClockSource, String> {
    match s {
        "ptp" => Ok(ClockSource::Ptp),
        "free" => Ok(ClockSource::Free),
        "system" => Ok(ClockSource::System),
        _ => Err("expected ptp, free or system".into()),
    }
}

fn parse_backend(s: &str) -> Result<BackendKind, String> {
    toml::Value::String(s.into()).try_into().map_err(|_| {
        "expected one of none, tone, record, loopback, soundcard, coreaudio".to_owned()
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_new(&cli.log)?)
        .init();
    match cli.command {
        Command::Run(args) => run(args).await,
        Command::ExampleConfig { macos } => {
            print!("{}", if macos { config::EXAMPLE_MACOS } else { config::EXAMPLE });
            Ok(())
        }
        Command::Discover { interface, wait, channels } => {
            discover(&interface, Duration::from_secs(wait), channels).await
        }
        Command::Info { device, interface } => show_info(&device, &interface).await,
        Command::Route { rx_device, rx_channel, source, interface } => {
            let (tx_channel, tx_device) = discovery::split_channel_instance(&source)
                .context("source must look like CHANNEL@DEVICE")?;
            set_route(&rx_device, &rx_channel, Some((tx_channel, tx_device)), &interface).await
        }
        Command::Unroute { rx_device, rx_channel, interface } => {
            set_route(&rx_device, &rx_channel, None, &interface).await
        }
        Command::Soundcards => list_soundcards(),
        Command::PtpMaster { interface, event_port, general_port, rate_ppm } => {
            ptp_master(&interface, event_port, general_port, rate_ppm).await
        }
    }
}

/// What ends a long-running command: Ctrl-C, or SIGTERM (which launchd and
/// systemd send to stop a service). The listeners live as long as this, so
/// a signal that arrives while the command is busy elsewhere is not lost.
pub(crate) struct StopSignals {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(windows)]
    ctrl_c: tokio::signal::windows::CtrlC,
}

impl StopSignals {
    /// Starts listening. Must be called within the tokio runtime.
    pub(crate) fn new() -> anyhow::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                interrupt: signal(SignalKind::interrupt()).context("listening for SIGINT")?,
                terminate: signal(SignalKind::terminate()).context("listening for SIGTERM")?,
            })
        }
        #[cfg(windows)]
        {
            Ok(Self { ctrl_c: tokio::signal::windows::ctrl_c().context("listening for Ctrl-C")? })
        }
    }
}

/// A source of requests to stop.
pub(crate) trait Stop {
    /// Waits for the next request and says what it was.
    async fn requested(&mut self) -> anyhow::Result<&'static str>;
}

impl Stop for StopSignals {
    async fn requested(&mut self) -> anyhow::Result<&'static str> {
        #[cfg(unix)]
        {
            let (got, what) = tokio::select! {
                r = self.interrupt.recv() => (r, "SIGINT"),
                r = self.terminate.recv() => (r, "SIGTERM"),
            };
            got.context("signal listener closed")?;
            Ok(what)
        }
        #[cfg(windows)]
        {
            self.ctrl_c.recv().await.context("signal listener closed")?;
            Ok("Ctrl-C")
        }
    }
}

/// The device's status line: clock state, receive channels, transmit flows
/// and packet counters.
pub(crate) fn status_line(device: &Device, clock: &MediaClock) -> String {
    let rx = device.rx_channels();
    let receiving = rx.iter().filter(|c| c.status.is_receiving()).count();
    let subscribed = rx.iter().filter(|c| c.subscription.is_some()).count();
    let stats = device.stats();
    format!(
        "clock {:?}; receiving {receiving}/{subscribed} subscribed channels; {} transmit flows; \
         packets: tx {} ({} underruns), rx {} ({} late)",
        clock.status().state,
        device.tx_flows().len(),
        stats.tx_packets,
        stats.tx_underruns,
        stats.rx_packets,
        stats.rx_late_packets
    )
}

/// Logs when the clock locks or loses its master.
#[derive(Default)]
pub(crate) struct ClockStateLog {
    last: Option<ClockState>,
}

impl ClockStateLog {
    /// Checks `clock` for a change of state since the last call.
    pub(crate) fn poll(&mut self, clock: &MediaClock) {
        let status = clock.status();
        if self.last == Some(status.state) {
            return;
        }
        match status.state {
            ClockState::Locked => info!("clock locked to {:?}", status.master),
            ClockState::Unlocked if self.last.is_some() => warn!("clock lost"),
            _ => {}
        }
        self.last = Some(status.state);
    }
}

async fn ptp_master(
    interface: &str,
    event_port: u16,
    general_port: u16,
    rate_ppm: f64,
) -> anyhow::Result<()> {
    let rate = config::rate_from_ppm(rate_ppm).context("--rate-ppm")?;
    let mut stop = StopSignals::new()?;
    let iface = Interface::find(interface)?;
    // A locally administered variant of the MAC, so that a follower on the
    // same host (which uses the MAC itself) has a different identity.
    let mut uuid = iface.mac;
    uuid[0] |= 0x02;
    uuid[5] ^= 0x5a;
    let mut cfg = TestMasterConfig::new(iface.ip, uuid);
    cfg.event_port = event_port;
    cfg.general_port = general_port;
    // Free-running: NTP adjustments of the wall clock must not reach followers.
    let master = TestMaster::start(cfg, free_running_clock_with_rate(rate)).await?;
    warn!(
        "development PTP master running on {} ({}) at {rate_ppm:+} ppm; Ctrl-C to stop",
        iface.name, iface.ip
    );
    let mut check = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            r = stop.requested() => {
                info!("{}: stopping", r?);
                break;
            }
            _ = check.tick() => {
                if master.is_silenced() {
                    bail!("another PTP master is active on this network; stopped");
                }
            }
        }
    }
    master.shutdown();
    Ok(())
}

async fn run(args: RunArgs) -> anyhow::Result<()> {
    let mut cfg = match &args.config {
        Some(path) => AppConfig::load(path)?,
        None => AppConfig::default(),
    };
    if let Some(v) = args.name {
        cfg.device.name = v;
    }
    if let Some(v) = args.interface {
        cfg.device.interface = v;
    }
    if let Some(v) = args.tx {
        cfg.device.tx_channels = Channels::Count(v);
    }
    if let Some(v) = args.rx {
        cfg.device.rx_channels = Channels::Count(v);
    }
    if let Some(v) = args.latency_ms {
        cfg.device.latency_ms = v;
    }
    if let Some(v) = args.clock {
        cfg.clock.source = v;
    }
    if let Some(v) = args.backend {
        cfg.audio.backend = v;
    }
    if let Some(v) = args.record_path {
        cfg.audio.record_path = v;
    }
    cfg.validate()?;
    if cfg.audio.backend == BackendKind::CoreAudio {
        return coreaudio::run(cfg, args.config.clone()).await;
    }

    let mut stop = StopSignals::new()?;
    let iface = Interface::find(&cfg.device.interface)?;
    let (clock, follower) = match cfg.clock.source {
        ClockSource::System => {
            warn!("using the system clock: audio will not be in sync with a Dante network");
            (system_clock(), None)
        }
        ClockSource::Free => {
            warn!("using a free-running clock: audio will not be in sync with a Dante network");
            (free_running_clock_with_rate(config::rate_from_ppm(cfg.clock.free_rate_ppm)?), None)
        }
        ClockSource::Ptp => {
            let mut ptp = PtpConfig::new(iface.ip, iface.mac);
            ptp.event_port = cfg.clock.ptp_event_port;
            ptp.general_port = cfg.clock.ptp_general_port;
            let (follower, clock) =
                PtpFollower::start(ptp).await.context("starting the PTP clock follower")?;
            (clock, Some(follower))
        }
    };

    let device = Device::start(cfg.device.clone(), clock.clone()).await?;
    let audio = device.audio();
    let a = &cfg.audio;
    let mut bridge = None;
    let backend = match a.backend {
        BackendKind::None => None,
        BackendKind::Tone => Some(backend::tone(audio, a.tone_hz, a.tone_level_db)),
        BackendKind::Loopback => Some(backend::loopback(audio)),
        BackendKind::Record => Some(backend::record(audio, &a.record_path)?),
        BackendKind::Soundcard => {
            let bridge_cfg = BridgeConfig {
                output_device: a.output_device.clone(),
                input_device: a.input_device.clone(),
                buffer_frames: a.buffer_frames,
                output_map: config::channel_map(&a.output_channels),
                input_map: config::channel_map(&a.input_channels),
                margin_ms: a.margin_ms,
            };
            bridge =
                Some(Bridge::start(audio, &bridge_cfg).context("starting the soundcard bridge")?);
            None
        }
        BackendKind::CoreAudio => unreachable!("served by coreaudio::run"),
    };

    let mut status = tokio::time::interval(Duration::from_secs(30));
    status.tick().await;
    let mut clock_log = ClockStateLog::default();
    let why = loop {
        tokio::select! {
            r = stop.requested() => break r?,
            _ = status.tick() => {
                info!("{}", status_line(&device, &clock));
                if let Some(b) = &bridge {
                    let stats = b.stats();
                    for (what, d) in [("playback", stats.output), ("capture", stats.input)] {
                        if let Some(d) = d {
                            info!(
                                "{what} {}: {}, {:+.1} ppm, error {:.1} samples, {} underruns, {} realigns{}",
                                d.device,
                                if d.locked { "locked" } else if d.running { "locking" } else { "starting" },
                                d.ratio_ppm,
                                d.error_samples,
                                d.underruns,
                                d.realigns,
                                if d.device_lost { ", DEVICE LOST" } else { "" }
                            );
                        }
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(1)) => clock_log.poll(&clock),
        }
    };
    info!("{why}: shutting down");
    if let Some(b) = backend {
        b.stop();
    }
    drop(bridge);
    device.shutdown().await;
    if let Some(f) = follower {
        f.shutdown();
    }
    Ok(())
}

fn list_soundcards() -> anyhow::Result<()> {
    let devices = ovsc_soundcard::list_devices()?;
    if devices.is_empty() {
        println!("No audio devices found.");
        return Ok(());
    }
    println!("{:<36} {:>4} {:>4}  {:<28} ID", "NAME", "IN", "OUT", "RATES");
    for d in devices {
        let rates: Vec<String> = d.sample_rates.iter().map(|r| r.to_string()).collect();
        let mut name = d.name.clone();
        if d.is_default_output || d.is_default_input {
            name.push_str(" *");
        }
        println!(
            "{:<36} {:>4} {:>4}  {:<28} {}",
            name,
            d.max_input_channels,
            d.max_output_channels,
            rates.join(","),
            d.id
        );
    }
    println!("\n* default device");
    Ok(())
}

fn bind_ip(interface: &str) -> anyhow::Result<Ipv4Addr> {
    Ok(if interface.is_empty() { Ipv4Addr::UNSPECIFIED } else { Interface::find(interface)?.ip })
}

/// Finds a device's ARC address from its name (via mDNS) or IP[:port].
async fn locate(device: &str, interface: &str) -> anyhow::Result<(String, SocketAddr)> {
    if let Ok(addr) = device.parse::<SocketAddr>() {
        return Ok((device.to_owned(), addr));
    }
    if let Ok(ip) = device.parse::<IpAddr>() {
        return Ok((device.to_owned(), SocketAddr::new(ip, arc::PORT)));
    }
    let iface = Interface::find(interface)?;
    let mdns = Mdns::start(iface.ip)?;
    for wait in [1, 3] {
        let found = mdns.browse(discovery::ARC_SERVICE, Duration::from_secs(wait)).await;
        if let Some(s) = found.iter().find(|s| s.instance.eq_ignore_ascii_case(device)) {
            let ip = s.addr.with_context(|| format!("no address for {device}"))?;
            mdns.stop();
            return Ok((s.instance.clone(), SocketAddr::from((ip, s.port))));
        }
    }
    mdns.stop();
    bail!("device {device:?} not found (try `ovsc discover`)")
}

async fn discover(interface: &str, wait: Duration, channels: bool) -> anyhow::Result<()> {
    let iface = Interface::find(interface)?;
    println!("Listening on {} ({}) for {:?}…", iface.name, iface.ip, wait);
    let mdns = Mdns::start(iface.ip)?;
    let (devices, chans) = tokio::join!(
        mdns.browse(discovery::ARC_SERVICE, wait),
        mdns.browse(discovery::CHAN_SERVICE, wait)
    );
    mdns.stop();
    if devices.is_empty() {
        println!("No Dante devices found.");
        return Ok(());
    }
    println!("{:<32} {:<16} {:<20} TX CHANNELS", "DEVICE", "ADDRESS", "MANUFACTURER");
    for d in &devices {
        let txt = parse_txt(&d.txt);
        let mf = txt.get("mf").cloned().flatten().unwrap_or_default();
        let count = chans
            .iter()
            .filter(|c| {
                discovery::split_channel_instance(&c.instance)
                    .is_some_and(|(_, dev)| dev.eq_ignore_ascii_case(&d.instance))
                    && ChannelTxt::from_entries(&c.txt).is_ok_and(|t| t.is_default_name)
            })
            .count();
        let addr = d.addr.map(|a| a.to_string()).unwrap_or_else(|| "?".into());
        println!("{:<32} {:<16} {:<20} {count}", d.instance, addr, mf);
    }
    if channels {
        println!();
        println!("{:<40} {:>4} {:>7} {:>4} {:>11}", "CHANNEL", "ID", "RATE", "BITS", "LATENCY");
        for c in &chans {
            if let Ok(t) = ChannelTxt::from_entries(&c.txt) {
                println!(
                    "{:<40} {:>4} {:>7} {:>4} {:>8.2} ms",
                    c.instance,
                    t.id,
                    t.sample_rate,
                    t.bits_per_sample,
                    t.latency_ns as f64 / 1e6
                );
            }
        }
    }
    Ok(())
}

async fn show_info(device: &str, interface: &str) -> anyhow::Result<()> {
    let (label, addr) = locate(device, interface).await?;
    let ip = bind_ip(interface)?;
    let t = Duration::from_millis(700);
    let name_resp = client::transact_ok(
        ip,
        addr,
        &arc::encode_simple_request(next_seq(), arc::opcode::DEVICE_NAME),
        t,
        3,
    )
    .await
    .with_context(|| format!("no answer from {label} at {addr}"))?;
    let name = arc::decode_device_name_response(&Frame::parse(&name_resp)?)?;
    let counts_resp = client::transact_ok(
        ip,
        addr,
        &arc::encode_simple_request(next_seq(), arc::opcode::CHANNEL_COUNTS),
        t,
        3,
    )
    .await?;
    let counts = arc::ChannelCounts::decode_response(&Frame::parse(&counts_resp)?)?;
    println!("{name} at {addr}: {} tx / {} rx channels", counts.tx_channels, counts.rx_channels);

    if counts.tx_channels > 0 {
        let tx =
            client::arc_paged(ip, addr, arc::opcode::TX_CHANNELS, arc::decode_tx_channels_response)
                .await?;
        let names = client::arc_paged(
            ip,
            addr,
            arc::opcode::TX_CHANNEL_NAMES,
            arc::decode_tx_channel_names_response,
        )
        .await
        .unwrap_or_default();
        println!("\nTransmit channels:");
        for ch in tx {
            let label = names.iter().find(|(id, _)| *id == ch.id).map(|(_, n)| n.as_str());
            let format = ch
                .format
                .map(|f| format!("{} Hz {}-bit", f.sample_rate, f.bits_per_sample))
                .unwrap_or_default();
            match label {
                Some(l) if l != ch.factory_name => {
                    println!("  {:>3}  {} ({})  {format}", ch.id, l, ch.factory_name)
                }
                _ => println!("  {:>3}  {}  {format}", ch.id, ch.factory_name),
            }
        }
    }
    if counts.rx_channels > 0 {
        let rx =
            client::arc_paged(ip, addr, arc::opcode::RX_CHANNELS, arc::decode_rx_channels_response)
                .await?;
        println!("\nReceive channels:");
        for ch in rx {
            let source = ch
                .subscription
                .map(|(c, d)| format!("<- {c}@{d}  [{:?}]", ch.status))
                .unwrap_or_default();
            println!("  {:>3}  {:<24} {source}", ch.id, ch.name);
        }
    }
    Ok(())
}

async fn set_route(
    rx_device: &str,
    rx_channel: &str,
    source: Option<(&str, &str)>,
    interface: &str,
) -> anyhow::Result<()> {
    let (label, addr) = locate(rx_device, interface).await?;
    let ip = bind_ip(interface)?;
    let id = match rx_channel.parse::<u16>() {
        Ok(id) => id,
        Err(_) => {
            let rx = client::arc_paged(
                ip,
                addr,
                arc::opcode::RX_CHANNELS,
                arc::decode_rx_channels_response,
            )
            .await?;
            rx.iter()
                .find(|c| c.name.eq_ignore_ascii_case(rx_channel))
                .map(|c| c.id)
                .with_context(|| format!("{label} has no receive channel {rx_channel:?}"))?
        }
    };
    let request = match source {
        Some((c, d)) => arc::encode_set_subscriptions_request(
            next_seq(),
            &[SubscriptionRequest { rx_channel: id, source: Some((c.into(), d.into())) }],
        ),
        None => arc::encode_remove_subscriptions_request(next_seq(), &[id]),
    };
    client::transact_ok(ip, addr, &request, Duration::from_millis(700), 3)
        .await
        .with_context(|| format!("{label} did not accept the change"))?;
    match source {
        Some((c, d)) => println!("{label} rx {id} <- {c}@{d}"),
        None => println!("{label} rx {id} unsubscribed"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_takes_macos() {
        let cli = Cli::try_parse_from(["ovsc", "example-config", "--macos"]).unwrap();
        assert!(matches!(cli.command, Command::ExampleConfig { macos: true }));
        let cli = Cli::try_parse_from(["ovsc", "example-config"]).unwrap();
        assert!(matches!(cli.command, Command::ExampleConfig { macos: false }));
    }

    #[test]
    fn run_takes_the_new_backend_and_clock() {
        let cli =
            Cli::try_parse_from(["ovsc", "run", "-b", "coreaudio", "--clock", "free"]).unwrap();
        let Command::Run(args) = cli.command else { panic!("not run") };
        assert_eq!(args.backend, Some(BackendKind::CoreAudio));
        assert_eq!(args.clock, Some(ClockSource::Free));
        assert!(Cli::try_parse_from(["ovsc", "run", "--clock", "wall"]).is_err());
    }

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn the_coreaudio_backend_exits_elsewhere() {
        let cli = Cli::try_parse_from(["ovsc", "run", "--backend", "coreaudio"]).unwrap();
        let Command::Run(args) = cli.command else { panic!("not run") };
        let err = run(args).await.unwrap_err();
        assert_eq!(err.to_string(), "the coreaudio backend needs macOS");
    }

    #[test]
    fn the_ptp_master_runs_at_its_rate() {
        let parse = |args: &[&str]| -> f64 {
            let cli = Cli::try_parse_from([&["ovsc", "ptp-master"], args].concat()).unwrap();
            let Command::PtpMaster { rate_ppm, .. } = cli.command else { panic!("not ptp-master") };
            rate_ppm
        };
        assert_eq!(parse(&[]), 0.0);
        assert_eq!(parse(&["--rate-ppm", "-50"]), -50.0);
        assert_eq!(parse(&["--rate-ppm", "50"]), 50.0);

        let rate = config::rate_from_ppm(50.0).unwrap();
        assert_eq!(rate, 1.00005);
        let clock = free_running_clock_with_rate(rate);
        let snap = clock.snapshot().unwrap();
        // 1 s of local time is 1 s + 50 us of media time, at any point.
        for local in [snap.local_ref_ns, snap.local_ref_ns + 3_600_000_000_000] {
            let advance = snap.media_ns_at(local + 1_000_000_000) - snap.media_ns_at(local);
            assert!(advance.abs_diff(1_000_050_000) <= 1, "{advance}");
        }
        // The clock reads that mapping.
        let before = ovsc_clock::local_now_ns();
        let now = clock.now_ns().unwrap();
        let after = ovsc_clock::local_now_ns();
        assert!(now >= snap.media_ns_at(before) && now <= snap.media_ns_at(after));
        assert!((clock.status().freq_offset_ppb - 50_000.0).abs() < 1e-3);
        assert!(config::rate_from_ppm(600.0).is_err());
    }
}
