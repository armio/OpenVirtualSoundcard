//! The driver service over the in-memory transport, with a real device on
//! 127.0.0.1 whose rings live in the shared region.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ovsc_clock::{
    ClockSnapshot, ClockState, ClockStatus, MediaClock, free_running_clock_with_rate, local_now_ns,
};
use ovsc_core::client;
use ovsc_core::directory::StaticDirectory;
use ovsc_core::{AudioIo, Channels, Device, DeviceConfig, Ports, StartOptions};
use ovsc_hal_server::{
    ConfigError, EngineInfo, HalOptions, HalRegion, HalServer, ShmClockMirror, driver_config,
};
use ovsc_ipc::mem::{MemClient, MemRegistry};
use ovsc_ipc::protocol::{
    ConfigApplied, DriverConfig, Hello, PROTO_MAJOR, Reject, RejectReason, ToDaemon, ToPlugin,
    Welcome,
};
use ovsc_ipc::transport::{ClientHandler, ClientTransport, TransportError};
use ovsc_proto::arc;
use ovsc_shm::clock::{ClockRead, ClockRecord, READ_TRIES};
use ovsc_shm::layout::{HOST_ARCH, LAYOUT_HASH, LAYOUT_VERSION, REGION_SIZE, RegionRef};
use ovsc_shm::status::{
    AudioWord, ChannelsWord, DAEMON_ENGINE_RUNNING, DAEMON_SHUTTING_DOWN, DaemonStatus,
};

const SERVICE: &str = "org.openvirtualsoundcard.audio.test";

fn options(allowed_uids: &[u32]) -> HalOptions {
    HalOptions {
        allowed_uids: allowed_uids.to_vec(),
        prevent_idle_sleep: false,
        status_log_interval: Duration::from_secs(1),
        ..Default::default()
    }
}

fn device_config(name: &str, base: u16, process_id: u16) -> DeviceConfig {
    DeviceConfig {
        name: name.into(),
        interface: "127.0.0.1".into(),
        sample_rate: 48_000,
        tx_channels: Channels::Count(8),
        rx_channels: Channels::Count(8),
        latency_ms: 4.0,
        ports: Ports { arc: base, cmc: base + 1, flow_control: base + 2, settings: base + 3 },
        discovery: false,
        process_id,
        ..Default::default()
    }
}

fn hello() -> Hello {
    Hello {
        proto_major: PROTO_MAJOR,
        proto_minor: 0,
        layout_version: LAYOUT_VERSION,
        layout_hash: LAYOUT_HASH,
        plugin_version: "service test".into(),
        instance: 0x1234,
        pid: 4242,
        applied_daemon_generation: 0,
        applied_config_gen: 0,
        sample_rate: 48_000,
        input_channels: 8,
        output_channels: 8,
        timebase_numer: 1,
        timebase_denom: 1,
        arch: HOST_ARCH,
    }
}

#[derive(Clone, Debug)]
enum Event {
    Message(ToPlugin),
    Interrupted,
    Invalid,
}

/// A driver's connection events.
#[derive(Default)]
struct ClientLog(Mutex<Vec<Event>>);

impl ClientLog {
    fn events(&self) -> Vec<Event> {
        self.0.lock().unwrap().clone()
    }

    fn configs(&self) -> Vec<DriverConfig> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                Event::Message(ToPlugin::Config(c)) => Some(c),
                _ => None,
            })
            .collect()
    }
}

impl ClientHandler for ClientLog {
    fn on_message(&self, m: ToPlugin) {
        self.0.lock().unwrap().push(Event::Message(m));
    }
    fn on_interrupted(&self) {
        self.0.lock().unwrap().push(Event::Interrupted);
    }
    fn on_invalid(&self) {
        self.0.lock().unwrap().push(Event::Invalid);
    }
}

/// A connected driver.
fn driver(registry: &Arc<MemRegistry>, euid: u32) -> (MemClient, Arc<ClientLog>) {
    let client = registry.client(SERVICE, euid, 4242);
    let log = Arc::new(ClientLog::default());
    client.connect(log.clone());
    (client, log)
}

async fn request(client: &MemClient, m: ToDaemon) -> Result<ToPlugin, TransportError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    client.request(
        m,
        Duration::from_secs(2),
        Box::new(move |r| {
            let _ = tx.send(r);
        }),
    );
    rx.await.expect("every request gets a reply")
}

async fn welcome(client: &MemClient) -> Welcome {
    match request(client, ToDaemon::Hello(hello())).await {
        Ok(ToPlugin::Welcome(w)) => w,
        other => panic!("no welcome: {other:?}"),
    }
}

/// Polls `check` every 10 ms for up to 5 s.
async fn wait_for<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(v) = check() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn daemon(region: &HalRegion) -> &DaemonStatus {
    region.view().daemon()
}

fn flags(region: &HalRegion) -> u64 {
    daemon(region).flags.load(Ordering::Acquire)
}

fn clock_record(view: RegionRef<'_>) -> ClockRecord {
    match view.clock().read_bounded(READ_TRIES) {
        ClockRead::Record(r) => r,
        other => panic!("no clock record: {other:?}"),
    }
}

/// Deterministic 24-bit test signal, distinct per channel.
fn signal(ts: u64, ch: usize) -> i32 {
    let v = (ts.wrapping_mul(2654435761).wrapping_add(ch as u64 * 7919) & 0xff_ffff) as i32;
    (v - 0x80_0000) << 8
}

/// Keeps the transmit rings filled ahead of time, like an audio backend.
fn start_feeder(io: AudioIo, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let lead = io.sample_rate as u64 / 10;
        let mut written = io.now().unwrap();
        let mut buf = vec![0i32; 8192];
        while !stop.load(Ordering::Relaxed) {
            let until = io.now().unwrap() + lead;
            if until > written {
                let n = ((until - written) as usize).min(buf.len());
                for ch in 0..io.tx.len() {
                    for (i, s) in buf[..n].iter_mut().enumerate() {
                        *s = signal(written + i as u64, ch);
                    }
                    io.write_tx(ch, written, &buf[..n]);
                }
                written += n as u64;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drivers_attach_and_follow_the_device() {
    let region = HalRegion::create("ovsc service test").unwrap();
    let clock = free_running_clock_with_rate(1.00005);
    clock.set_mirror(Some(ShmClockMirror::new(region.clone()))).unwrap();
    let registry = MemRegistry::new();
    let cfg = device_config("hal-dev", 24610, 31);
    let server = HalServer::start(
        options(&[202]),
        region.clone(),
        Box::new(registry.bind(SERVICE)),
        EngineInfo::from_config(&cfg).unwrap(),
    )
    .unwrap();

    // Before the engine runs, a driver gets the configured layout.
    let (client, log) = driver(&registry, 202);
    let w = welcome(&client).await;
    assert_eq!(w.region_size, 67_174_400);
    assert_eq!(w.region_size, REGION_SIZE as u64);
    assert_eq!(w.daemon_generation, region.generation());
    assert_eq!(w.daemon_version, "ovsc service test");
    let c = &w.config;
    assert_eq!(
        (c.config_gen, c.sample_rate, c.input_channels, c.output_channels),
        (1, 48_000, 8, 8)
    );
    assert_eq!((c.input_safety_offset, c.output_safety_offset, c.output_latency), (216, 55, 192));
    assert_eq!((c.input_latency, c.input_read_delay), (0, 0));
    assert_eq!(c.input_names, (1..=8).map(|i| format!("{i:02}")).collect::<Vec<_>>());
    assert_eq!(c.device_name, "hal-dev");
    assert_eq!(flags(&region), 0);
    assert_eq!(daemon(&region).audio_word.load(Ordering::Acquire), 0);
    // The handle maps the same region.
    let mapped = w.region.map().unwrap();
    // SAFETY: the mapping lives until the end of the test.
    let view = unsafe { RegionRef::from_raw(mapped.as_ptr(), mapped.len()) }.unwrap();
    assert_eq!(view.header().daemon_generation, region.generation());

    // The engine starts on the region's rings, next to a peer device.
    let hal_dir = StaticDirectory::new();
    let peer_dir = StaticDirectory::new();
    let start = StartOptions {
        directory: Some(hal_dir.clone()),
        rings: Some(region.external_rings(8, 8).unwrap()),
        ..Default::default()
    };
    let device = Device::start_with_options(cfg, clock.clone(), start).await.unwrap();
    let peer = Device::start_with_directory(
        device_config("hal-peer", 24620, 32),
        clock.clone(),
        peer_dir.clone(),
    )
    .await
    .unwrap();
    for e in peer.directory_entries() {
        hal_dir.insert(e);
    }
    for e in device.directory_entries() {
        peer_dir.insert(e);
    }
    server.engine_started(&device);
    let d = daemon(&region);
    assert_eq!(flags(&region), DAEMON_ENGINE_RUNNING);
    let audio = AudioWord::unpack(d.audio_word.load(Ordering::Acquire));
    assert_eq!(audio, AudioWord { sample_rate: 48_000, config_gen: 1 });
    let channels = ChannelsWord::unpack(d.channels_word.load(Ordering::Acquire));
    assert_eq!(channels, ChannelsWord { rx: 8, tx: 8, latency_samples: 192 });
    assert_eq!(d.tx_guard_samples.load(Ordering::Acquire), 24);
    let status = server.status();
    assert_eq!((status.peers, status.engine_running, status.config_gen), (1, true, 1));
    // The view the driver maps sees the same words.
    assert_eq!(view.daemon().flags.load(Ordering::Acquire), DAEMON_ENGINE_RUNNING);

    // External rings: a peer's transmit channel 2 arrives in the region's
    // receive ring 0. The subscription is a device change, but not a
    // configuration change.
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = start_feeder(peer.audio(), stop.clone());
    device.subscribe(1, "02", "hal-peer").unwrap();
    let io = device.audio();
    let rx0 = region.view().rx(0).unwrap();
    wait_for("peer audio in the region's rx ring 0", || {
        let end = io.now().unwrap() - io.latency_samples;
        let start = end - 4800;
        let mut present = 0;
        for ts in start..end {
            if let Some(s) = rx0.read_one(ts) {
                assert_eq!(s, signal(ts, 1), "region rx 0 at {ts}");
                present += 1;
            }
        }
        (present > 4700).then_some(())
    })
    .await;
    // The device's counters reach the daemon status through the heartbeat.
    wait_for("rx packet counts", || (d.rx_packets.load(Ordering::Relaxed) > 0).then_some(())).await;
    assert!(log.configs().is_empty(), "{:?}", log.configs());

    // A rename over ARC pushes the names with the next generation.
    let req = arc::encode_rename_rx_channels_request(client::next_seq(), &[(4, "Return")]);
    let arc_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 24610));
    client::transact_ok(Ipv4Addr::LOCALHOST, arc_addr, &req, Duration::from_millis(500), 3)
        .await
        .unwrap();
    let pushed = wait_for("a pushed configuration", || log.configs().pop()).await;
    assert_eq!(pushed.config_gen, 2);
    assert_eq!(pushed.input_names[3], "Return");
    assert_eq!(pushed.output_names[3], "04");
    assert!(pushed.structural_eq(&w.config));
    assert_eq!(log.configs().len(), 1);
    assert_eq!(AudioWord::unpack(d.audio_word.load(Ordering::Acquire)).config_gen, 2);

    // A driver reporting an older configuration applied gets the current
    // one again, once.
    let applied = |config_gen| {
        ToDaemon::ConfigApplied(ConfigApplied {
            daemon_generation: region.generation(),
            config_gen,
            sample_rate: 48_000,
            input_channels: 8,
            output_channels: 8,
        })
    };
    client.send(applied(1));
    wait_for("the configuration sent again", || (log.configs().len() == 2).then_some(())).await;
    assert_eq!(log.configs()[1], pushed);
    client.send(applied(1));
    client.send(applied(2));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(log.configs().len(), 2);

    // Stopping the engine clears its words; starting it again restores them
    // without a new configuration.
    server.engine_stopped();
    assert_eq!(flags(&region), 0);
    assert_eq!(d.audio_word.load(Ordering::Acquire), 0);
    assert!(!server.status().engine_running);
    server.engine_started(&device);
    assert_eq!(flags(&region), DAEMON_ENGINE_RUNNING);
    assert_eq!(AudioWord::unpack(d.audio_word.load(Ordering::Acquire)).config_gen, 2);

    // Shutdown: SHUTTING_DOWN, ENGINE_RUNNING cleared, a bye, then the
    // connection closes.
    stop.store(true, Ordering::Relaxed);
    feeder.join().unwrap();
    server.shutdown().await;
    assert_eq!(flags(&region), DAEMON_SHUTTING_DOWN);
    assert_eq!(d.audio_word.load(Ordering::Acquire), 0);
    wait_for("the interruption", || {
        log.events().iter().any(|e| matches!(e, Event::Interrupted)).then_some(())
    })
    .await;
    let events = log.events();
    let bye = events.iter().position(|e| matches!(e, Event::Message(ToPlugin::Bye { .. })));
    let gone = events.iter().position(|e| matches!(e, Event::Interrupted));
    assert!(bye.unwrap() < gone.unwrap(), "{events:?}");
    peer.shutdown().await;
    device.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drivers_are_admitted_by_effective_uid() {
    let region = HalRegion::create("test").unwrap();
    let registry = MemRegistry::new();
    let initial = EngineInfo::from_config(&device_config("uids", 1, 1)).unwrap();
    let server = HalServer::start(
        options(&[202, 0]),
        region.clone(),
        Box::new(registry.bind(SERVICE)),
        initial.clone(),
    )
    .unwrap();

    // The helper's uid and root (allowed here) attach, side by side.
    let (helper, _) = driver(&registry, 202);
    let (root, _) = driver(&registry, 0);
    welcome(&helper).await;
    welcome(&root).await;
    assert_eq!(server.status().peers, 2);
    assert_eq!(daemon(&region).peers.load(Ordering::Relaxed), 2);

    // Anyone else is cancelled without a reply.
    let (user, user_log) = driver(&registry, 501);
    assert_eq!(request(&user, ToDaemon::Hello(hello())).await, Err(TransportError::Interrupted));
    wait_for("the refusal", || {
        user_log.events().iter().any(|e| matches!(e, Event::Interrupted)).then_some(())
    })
    .await;
    assert_eq!(server.status().peers, 2);

    // A driver that goes away leaves the table.
    drop(root);
    wait_for("the peer to leave", || (server.status().peers == 1).then_some(())).await;
    assert_eq!(daemon(&region).peers.load(Ordering::Relaxed), 1);
    drop(server);

    // Root is refused when only the helper is allowed (the default).
    let registry = MemRegistry::new();
    let _server = HalServer::start(
        HalOptions { prevent_idle_sleep: false, ..Default::default() },
        region,
        Box::new(registry.bind(SERVICE)),
        initial,
    )
    .unwrap();
    let (root, _) = driver(&registry, 0);
    assert_eq!(request(&root, ToDaemon::Hello(hello())).await, Err(TransportError::Interrupted));
    let (helper, _) = driver(&registry, 202);
    welcome(&helper).await;
}

#[tokio::test]
async fn incompatible_drivers_are_rejected() {
    let region = HalRegion::create("test").unwrap();
    let registry = MemRegistry::new();
    let server = HalServer::start(
        options(&[202]),
        region,
        Box::new(registry.bind(SERVICE)),
        EngineInfo::from_config(&device_config("rejects", 1, 1)).unwrap(),
    )
    .unwrap();
    let (client, _) = driver(&registry, 202);
    let cases = [
        (Hello { proto_major: PROTO_MAJOR + 1, ..hello() }, RejectReason::Proto),
        (Hello { layout_hash: LAYOUT_HASH ^ 1, ..hello() }, RejectReason::Layout),
        (Hello { layout_version: LAYOUT_VERSION + 1, ..hello() }, RejectReason::Layout),
    ];
    for (h, reason) in cases {
        match request(&client, ToDaemon::Hello(h)).await {
            Ok(ToPlugin::Reject(Reject {
                reason: r,
                proto_major,
                layout_version,
                layout_hash,
                message,
            })) => {
                assert_eq!(r, reason);
                // The daemon's own numbers.
                assert_eq!(
                    (proto_major, layout_version, layout_hash),
                    (PROTO_MAJOR, LAYOUT_VERSION, LAYOUT_HASH)
                );
                assert!(!message.is_empty());
            }
            other => panic!("no reject for {reason:?}: {other:?}"),
        }
        assert_eq!(server.status().peers, 0);
    }
    // The same connection attaches once compatible.
    welcome(&client).await;
    assert_eq!(server.status().peers, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_clock_and_the_heartbeat_reach_the_region() {
    let region = HalRegion::create("test").unwrap();

    // A free-running clock is replayed into the block by set_mirror.
    let clock = free_running_clock_with_rate(1.00005);
    clock.set_mirror(Some(ShmClockMirror::new(region.clone()))).unwrap();
    let r = clock_record(region.view());
    assert!(r.valid);
    assert_eq!(r.state, ClockState::FreeRunning);
    assert_eq!(r.snapshot.rate, 1.00005);
    assert_eq!(r.step_gen, 1);
    let now = local_now_ns();
    let theirs = clock.snapshot().unwrap().media_ns_at(now);
    assert_eq!(r.snapshot.media_ns_at(now), theirs);

    // The heartbeat runs at about 10 Hz, on the host's clock.
    let registry = MemRegistry::new();
    let _server = HalServer::start(
        options(&[202]),
        region.clone(),
        Box::new(registry.bind(SERVICE)),
        EngineInfo::from_config(&device_config("beat", 1, 1)).unwrap(),
    )
    .unwrap();
    let beat = &daemon(&region).heartbeat_ns;
    let start = Instant::now();
    let mut last = beat.load(Ordering::Acquire);
    let mut changes = 0;
    while start.elapsed() < Duration::from_secs(2) {
        tokio::time::sleep(Duration::from_millis(5)).await;
        let b = beat.load(Ordering::Acquire);
        let age = local_now_ns().saturating_sub(b);
        assert!(age < 300_000_000, "heartbeat {age} ns old");
        if b != last {
            assert!(b > last);
            changes += 1;
            last = b;
        }
    }
    assert!((15..=25).contains(&changes), "{changes} heartbeats in 2 s");

    // A clock with a writer: continuous publishes keep step_gen, a step
    // bumps it. Mirrors of one region continue one count.
    let (clock, writer) = MediaClock::new();
    clock.set_mirror(Some(ShmClockMirror::new(region.clone()))).unwrap();
    writer.set_status(ClockStatus { state: ClockState::Locked, ..Default::default() });
    let t0 = local_now_ns();
    let snap = ClockSnapshot { local_ref_ns: t0, media_ref_ns: 1_000_000_000_000, rate: 1.0 };
    writer.publish(snap);
    let base = clock_record(region.view()).step_gen;
    assert!(base > 1);
    let later = |dt: u64, extra: u64| ClockSnapshot {
        local_ref_ns: t0 + dt,
        media_ref_ns: snap.media_ref_ns + dt + extra,
        rate: 1.0,
    };
    writer.publish(later(1_000_000, 0));
    writer.publish(later(2_000_000, 500));
    assert_eq!(clock_record(region.view()).step_gen, base);
    writer.publish(later(3_000_000, 10_000));
    let r = clock_record(region.view());
    assert_eq!(r.step_gen, base + 1);
    assert_eq!((r.valid, r.state), (true, ClockState::Locked));
    assert_eq!(r.snapshot, later(3_000_000, 10_000));
    writer.invalidate();
    assert!(!clock_record(region.view()).valid);
}

#[test]
fn driver_config_bounds_the_input_span() {
    let engine = |latency_ms| {
        let c = DeviceConfig { sample_rate: 192_000, latency_ms, ..device_config("span", 1, 1) };
        EngineInfo::from_config(&c).unwrap()
    };
    // 40 ms at 192 kHz fits with the default margins...
    let c = driver_config(&engine(40.0), 1, &HalOptions::default()).unwrap();
    assert_eq!((c.input_safety_offset, c.output_latency), (7776, 7680));
    // ...but not with a 44 ms input margin, which only 4 ms of latency
    // leaves room for.
    let wide = HalOptions { input_margin_us: 44_000, ..Default::default() };
    assert!(matches!(
        driver_config(&engine(40.0), 1, &wide),
        Err(ConfigError::RingSpan { span: 32768 })
    ));
    assert!(driver_config(&engine(4.0), 1, &wide).is_ok());
}

#[test]
fn start_needs_a_runtime_and_a_valid_engine() {
    let region = HalRegion::create("test").unwrap();
    let registry = MemRegistry::new();
    let initial = EngineInfo::from_config(&device_config("start", 1, 1)).unwrap();
    let err = HalServer::start(
        options(&[202]),
        region.clone(),
        Box::new(registry.bind(SERVICE)),
        initial.clone(),
    )
    .err()
    .unwrap();
    assert!(err.to_string().contains("tokio runtime"), "{err}");

    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let _guard = runtime.enter();
    let mut too_many = initial;
    too_many.rx_names = (0..129).map(|i| format!("{i}")).collect();
    let err = HalServer::start(options(&[202]), region, Box::new(registry.bind(SERVICE)), too_many)
        .err()
        .unwrap();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}
