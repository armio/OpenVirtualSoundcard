//! The daemon link (design sections 10.6, 11 and 12), end to end on any OS:
//! driver objects driven through their vtable with a FakeHost, on a manual
//! clock, connected over the in-process transport with manual time to a
//! fake daemon (a `ServerHandler`) that hands out regions and
//! configurations. A spy around each driver's transport records when the
//! link connects and what it sends, also while no daemon listens.
//!
//! The driver's clock and the transport's timers move together, in 10 ms
//! steps, so the link's own time checks (the request delay, the retire
//! grace) see the time its timers fire at, to within a step.

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use ovsc_hal::abi::*;
use ovsc_hal::io::Attachment;
use ovsc_hal::link::{ClientTransport, ConfigPlan, Link, LinkSink, LinkStatus, RETIRE_GRACE_NS};
use ovsc_hal::model::DriverConfig;
use ovsc_hal::new_driver_object;
use ovsc_hal::platform::Platform;
use ovsc_hal::platform::stub::{self, StubPlatform};
use ovsc_hal::testing::{self, FakeHost, HostCall};
use ovsc_ipc::mem::{MemClient, MemRegistry, MemServer};
use ovsc_ipc::protocol::{
    ConfigApplied, Hello, PROTO_MAJOR, Reject, RejectReason, SERVICE_NAME, STORAGE_KEY, ToDaemon,
    ToPlugin, Welcome,
};
use ovsc_ipc::region::SharedRegion;
use ovsc_ipc::transport::{
    ClientHandler, PeerInfo, ServerHandler, ServerTransport, TransportError,
};
use ovsc_shm::layout::{
    HOST_ARCH, HeaderInit, LAYOUT_HASH, LAYOUT_VERSION, REGION_SIZE, RegionRef,
};
use ovsc_shm::status::AppliedWord;
use ovsc_shm::time::Timebase;

const DEVICE: AudioObjectID = 2;
const INPUT_STREAM: AudioObjectID = 3;
const OUTPUT_STREAM: AudioObjectID = 4;
const GEN_A: u64 = 0x1f3a_0000_0000_000a;
const GEN_B: u64 = 0x1f3a_0000_0000_000b;
const STEP: Duration = Duration::from_millis(10);
/// The layout hash of a daemon built with another layout.
const OTHER_LAYOUT_HASH: u64 = LAYOUT_HASH ^ 1;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn secs(s: f64) -> Duration {
    Duration::from_secs_f64(s)
}

/// The manual clocks: the driver's platform and the transport's timers.
struct World {
    reg: Arc<MemRegistry>,
    platform: &'static StubPlatform,
}

impl World {
    fn new() -> World {
        World { reg: MemRegistry::with_manual_time(), platform: StubPlatform::new().leak() }
    }

    /// Moves both clocks forward by `d`, in steps of [`STEP`], the driver's
    /// first, so that a timer firing in a step sees the step's end.
    fn advance(&self, d: Duration) {
        let mut left = d;
        while !left.is_zero() {
            let step = left.min(STEP);
            self.platform.advance_ns(step.as_nanos() as u64);
            self.reg.advance(step);
            left -= step;
        }
    }

    /// Advances to `t` on the transport's clock.
    fn advance_to(&self, t: Duration) {
        self.advance(t.saturating_sub(self.reg.now()));
    }

    fn now(&self) -> Duration {
        self.reg.now()
    }
}

// --- The fake daemon -----------------------------------------------------

/// A daemon's region, laid out as the daemon would.
fn region(generation: u64) -> Arc<SharedRegion> {
    let r = SharedRegion::create(REGION_SIZE).unwrap();
    let h = HeaderInit {
        daemon_generation: generation,
        daemon_pid: 77,
        arch: HOST_ARCH,
        timebase: Timebase::NANOS,
        created_host_ns: 0,
        daemon_version: HeaderInit::version_bytes("test"),
    };
    // SAFETY: a fresh region, laid out before anyone else sees it.
    unsafe { RegionRef::init(r.as_ptr(), r.len(), &h) }.unwrap();
    r
}

/// The plug-in status of a region, as the daemon reads it.
fn plugin_status(r: &SharedRegion) -> (u64, u64, u64, u64, u64, u64) {
    // SAFETY: the region was laid out by `region` and outlives the view.
    let view = unsafe { RegionRef::from_raw(r.as_ptr(), r.len()) }.unwrap();
    let p = view.plugin();
    let l = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Acquire);
    (
        l(&p.plugin_instance),
        l(&p.plugin_pid),
        l(&p.applied_word),
        l(&p.io_clients),
        l(&p.attach_count),
        l(&p.attached_generation),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    Welcome,
    /// No reply: the peer times out.
    Silent,
    /// The daemon's layout differs from the plug-in's.
    RejectLayout,
}

struct DaemonState {
    reg: Arc<MemRegistry>,
    region: Arc<SharedRegion>,
    generation: u64,
    config: Mutex<DriverConfig>,
    answer: Mutex<Answer>,
    peers: Mutex<Vec<u64>>,
    hellos: Mutex<Vec<(Duration, u64, Hello)>>,
    applied: Mutex<Vec<(u64, ConfigApplied)>>,
}

impl ServerHandler for DaemonState {
    fn on_peer(&self, p: PeerInfo) -> bool {
        assert_eq!(p.euid, 202);
        lock(&self.peers).push(p.id);
        true
    }

    fn on_request(&self, peer: u64, m: ToDaemon) -> Option<ToPlugin> {
        let ToDaemon::Hello(h) = m else { panic!("a request that is no hello") };
        let answer = *lock(&self.answer);
        let layout_hash =
            if answer == Answer::RejectLayout { OTHER_LAYOUT_HASH } else { LAYOUT_HASH };
        let ok = h.proto_major == PROTO_MAJOR && h.layout_hash == layout_hash;
        lock(&self.hellos).push((self.reg.now(), peer, h));
        match answer {
            Answer::Silent => None,
            _ if !ok => Some(ToPlugin::Reject(Reject {
                reason: RejectReason::Layout,
                proto_major: PROTO_MAJOR,
                layout_version: LAYOUT_VERSION,
                layout_hash,
                message: "the daemon's layout differs".into(),
            })),
            _ => Some(ToPlugin::Welcome(Welcome {
                proto_major: PROTO_MAJOR,
                proto_minor: 0,
                daemon_version: "test".into(),
                daemon_generation: self.generation,
                // Shared in-process on every OS, so the tests can count
                // the driver's references to it.
                region: self.region.local_handle(),
                region_size: REGION_SIZE as u64,
                config: lock(&self.config).clone(),
            })),
        }
    }

    fn on_message(&self, peer: u64, m: ToDaemon) {
        if let ToDaemon::ConfigApplied(a) = m {
            lock(&self.applied).push((peer, a));
        }
    }

    fn on_peer_gone(&self, peer: u64) {
        lock(&self.peers).retain(|p| *p != peer);
    }
}

/// A daemon serving `service`, alive until dropped.
struct Daemon {
    server: MemServer,
    state: Arc<DaemonState>,
}

impl Daemon {
    fn start(world: &World, service: &str, generation: u64, config: DriverConfig) -> Daemon {
        let state = Arc::new(DaemonState {
            reg: world.reg.clone(),
            region: region(generation),
            generation,
            config: Mutex::new(config),
            answer: Mutex::new(Answer::Welcome),
            peers: Mutex::new(Vec::new()),
            hellos: Mutex::new(Vec::new()),
            applied: Mutex::new(Vec::new()),
        });
        let server = world.reg.bind(service);
        server.start(state.clone()).unwrap();
        Daemon { server, state }
    }

    /// Pushes `cfg` to every peer, as after a change of the engine.
    fn push(&self, world: &World, cfg: DriverConfig) {
        *lock(&self.state.config) = cfg.clone();
        for &p in lock(&self.state.peers).iter() {
            self.server.send(p, ToPlugin::Config(cfg.clone()));
        }
        world.reg.settle();
    }

    fn send(&self, world: &World, m: ToPlugin) {
        for &p in lock(&self.state.peers).iter() {
            self.server.send(p, m.clone());
        }
        world.reg.settle();
    }

    fn hellos(&self) -> Vec<(Duration, u64, Hello)> {
        lock(&self.state.hellos).clone()
    }

    fn applied(&self) -> Vec<ConfigApplied> {
        lock(&self.state.applied).iter().map(|(_, a)| *a).collect()
    }
}

// --- The spy transport ------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Sent {
    Connect,
    Cancel,
    Hello,
    Applied(ConfigApplied),
}

/// A transport that records what the link does with it, on the
/// transport's clock, then passes it on.
struct Spy {
    inner: MemClient,
    reg: Arc<MemRegistry>,
    log: Mutex<Vec<(Duration, Sent)>>,
}

impl Spy {
    fn new(world: &World, service: &str) -> Arc<Spy> {
        Arc::new(Spy {
            inner: world.reg.client(service, 202, 4242),
            reg: world.reg.clone(),
            log: Mutex::new(Vec::new()),
        })
    }

    fn record(&self, s: Sent) {
        lock(&self.log).push((self.reg.now(), s));
    }

    /// When the link did `what`, at or after `since`.
    fn times(&self, what: &Sent, since: Duration) -> Vec<Duration> {
        lock(&self.log).iter().filter(|(t, s)| s == what && *t >= since).map(|(t, _)| *t).collect()
    }
}

impl ClientTransport for Spy {
    fn connect(&self, h: Arc<dyn ClientHandler>) {
        self.record(Sent::Connect);
        self.inner.connect(h);
    }

    fn cancel(&self) {
        self.record(Sent::Cancel);
        self.inner.cancel();
    }

    fn send(&self, m: ToDaemon) {
        if let ToDaemon::ConfigApplied(a) = &m {
            self.record(Sent::Applied(*a));
        }
        self.inner.send(m);
    }

    fn request(
        &self,
        m: ToDaemon,
        timeout: Duration,
        reply: Box<dyn FnOnce(Result<ToPlugin, TransportError>) + Send>,
    ) {
        if matches!(m, ToDaemon::Hello(_)) {
            self.record(Sent::Hello);
        }
        self.inner.request(m, timeout, reply);
    }

    fn after(&self, delay: Duration, f: Box<dyn FnOnce() + Send>) {
        self.inner.after(delay, f);
    }

    fn run(&self, f: Box<dyn FnOnce() + Send>) {
        self.inner.run(f);
    }
}

// --- The driver -------------------------------------------------------------

/// A driver object, initialized with `stored` in its host's storage, whose
/// link reaches `service` through a spy.
struct Hal {
    driver: *mut c_void,
    vt: &'static DriverInterface,
    host: &'static FakeHost,
    spy: Arc<Spy>,
}

impl Hal {
    fn new(world: &World, service: &str, stored: Option<&DriverConfig>) -> Hal {
        let host = FakeHost::new();
        if let Some(cfg) = stored {
            host.set_storage(STORAGE_KEY, &cfg.to_storage_string());
        }
        let spy = Spy::new(world, service);
        let t: Arc<dyn ClientTransport> = spy.clone();
        let driver = new_driver_object(world.platform, Box::new(move || t.clone()));
        let vt = unsafe { testing::interface(driver) };
        assert_eq!(unsafe { (vt.Initialize)(driver, host.host_ref()) }, 0);
        world.reg.settle();
        Hal { driver, vt, host, spy }
    }

    fn get(&self, obj: AudioObjectID, a: AudioObjectPropertyAddress, cap: usize) -> Vec<u8> {
        let mut buf = vec![0u8; cap];
        let mut used = 0;
        let status = unsafe {
            (self.vt.GetPropertyData)(
                self.driver,
                obj,
                1,
                &a,
                0,
                ptr::null(),
                cap as u32,
                &mut used,
                buf.as_mut_ptr().cast(),
            )
        };
        assert_eq!(status, 0, "{a:?}");
        buf.truncate(used as usize);
        buf
    }

    fn get_string(&self, obj: AudioObjectID, a: AudioObjectPropertyAddress) -> String {
        let b = self.get(obj, a, size_of::<CFStringRef>());
        let s = unsafe { b.as_ptr().cast::<CFStringRef>().read_unaligned() };
        let text = unsafe { stub::read_string(s) }.expect("a stub CFString");
        unsafe { stub::cf_free(s) };
        text
    }

    /// The status property.
    fn ovst(&self) -> String {
        self.get_string(DEVICE, address(fourcc(b"ovst"), kAudioObjectPropertyScopeGlobal, 0))
    }

    /// The channel count of a stream's format.
    fn stream_channels(&self, stream: AudioObjectID) -> u32 {
        let a = address(kAudioStreamPropertyVirtualFormat, kAudioObjectPropertyScopeGlobal, 0);
        let b = self.get(stream, a, size_of::<AudioStreamBasicDescription>());
        let asbd = unsafe { b.as_ptr().cast::<AudioStreamBasicDescription>().read_unaligned() };
        asbd.mChannelsPerFrame
    }

    fn element_name(&self, scope: u32, element: u32) -> String {
        self.get_string(DEVICE, address(kAudioObjectPropertyElementName, scope, element))
    }

    fn perform(&self) {
        let r = unsafe {
            (self.vt.PerformDeviceConfigurationChange)(self.driver, DEVICE, 1, ptr::null_mut())
        };
        assert_eq!(r, 0);
    }

    fn abort(&self) {
        let r = unsafe {
            (self.vt.AbortDeviceConfigurationChange)(self.driver, DEVICE, 1, ptr::null_mut())
        };
        assert_eq!(r, 0);
    }

    fn requests(&self) -> usize {
        let request = HostCall::RequestDeviceConfigurationChange { device: DEVICE, action: 1 };
        self.host.calls().iter().filter(|c| **c == request).count()
    }

    fn hellos_since(&self, since: Duration) -> Vec<Duration> {
        self.spy.times(&Sent::Hello, since)
    }
}

fn address(selector: u32, scope: u32, element: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress { mSelector: selector, mScope: scope, mElement: element }
}

/// The daemon's first configuration: the fallback's, one generation on.
fn config_1() -> DriverConfig {
    DriverConfig { config_gen: 1, ..DriverConfig::fallback() }
}

/// A 4 x 4 configuration of generation `config_gen`.
fn config_4x4(config_gen: u64) -> DriverConfig {
    let names: Vec<String> = (1..=4).map(|i| format!("{i:02}")).collect();
    DriverConfig {
        config_gen,
        input_channels: 4,
        output_channels: 4,
        input_names: names.clone(),
        output_names: names,
        ..DriverConfig::fallback()
    }
}

fn attached(hal: &Hal, generation: u64) -> bool {
    hal.ovst().starts_with(&format!("daemon=attached gen={generation:x} "))
}

// --- Scenarios ----------------------------------------------------------------

#[test]
fn a_first_connect_with_an_equal_configuration_changes_nothing() {
    let world = World::new();
    let daemon = Daemon::start(&world, SERVICE_NAME, GEN_A, config_1());
    let hal = Hal::new(&world, SERVICE_NAME, None);

    assert!(attached(&hal, GEN_A), "{}", hal.ovst());
    assert!(hal.ovst().contains(" rate=48000 in=8 out=8 "), "{}", hal.ovst());
    assert!(hal.ovst().contains(" attach=1 "), "{}", hal.ovst());
    let hellos = daemon.hellos();
    assert_eq!(hellos.len(), 1);
    let h = &hellos[0].2;
    assert_eq!((h.proto_major, h.layout_version, h.layout_hash), (1, LAYOUT_VERSION, LAYOUT_HASH));
    assert_eq!((h.pid, h.arch, h.sample_rate, h.input_channels), (4242, HOST_ARCH, 48_000, 8));
    assert_eq!((h.applied_daemon_generation, h.applied_config_gen), (0, 0));
    assert_eq!((h.timebase_numer, h.timebase_denom), (1, 1));
    // Only the generation differs: no device change, just the report.
    let applied = ConfigApplied {
        daemon_generation: GEN_A,
        config_gen: 1,
        sample_rate: 48_000,
        input_channels: 8,
        output_channels: 8,
    };
    assert_eq!(daemon.applied(), vec![applied]);
    assert_eq!(hal.host.calls(), vec![HostCall::CopyFromStorage { key: STORAGE_KEY.into() }]);

    // Plug-in status line 2 of the daemon's region.
    let word = AppliedWord { sample_rate: 48_000, config_gen: 1 }.pack();
    assert_eq!(
        plugin_status(&daemon.state.region),
        (h.instance, 4242, word, 0, 1, GEN_A),
        "instance, pid, applied word, io clients, attach count, generation"
    );

    // StartIO reports the running device from the queue.
    assert_eq!(unsafe { (hal.vt.StartIO)(hal.driver, DEVICE, 0) }, 0);
    world.reg.settle();
    let goin = address(kAudioDevicePropertyDeviceIsRunning, kAudioObjectPropertyScopeGlobal, 0);
    assert_eq!(
        hal.host.calls().last(),
        Some(&HostCall::PropertiesChanged { object: DEVICE, addresses: vec![goin] })
    );
    assert_eq!(plugin_status(&daemon.state.region).3, 1);
    assert_eq!(unsafe { (hal.vt.StopIO)(hal.driver, DEVICE, 0) }, 0);
    world.reg.settle();
    assert_eq!(plugin_status(&daemon.state.region).3, 0);

    // Nothing more happens while the daemon stays.
    world.advance(secs(10.0));
    assert_eq!(daemon.hellos().len(), 1);
    assert_eq!(hal.requests(), 0);
    assert_eq!(hal.spy.times(&Sent::Connect, Duration::ZERO), vec![Duration::ZERO]);
}

#[test]
fn a_structural_change_is_requested_once_after_2_s_then_performed() {
    let world = World::new();
    let daemon = Daemon::start(&world, SERVICE_NAME, GEN_A, config_1());
    let hal = Hal::new(&world, SERVICE_NAME, None);
    assert!(attached(&hal, GEN_A), "{}", hal.ovst());

    daemon.push(&world, config_4x4(2));
    world.advance(secs(1.95));
    assert_eq!(hal.requests(), 0, "no request before Initialize + 2 s");
    // A newer structural configuration replaces the waiting one.
    daemon.push(&world, DriverConfig { output_latency: 200, ..config_4x4(3) });
    world.advance(secs(0.1));
    assert_eq!(hal.requests(), 1);
    // At most one request is outstanding.
    let last = DriverConfig { input_latency: 5, ..config_4x4(4) };
    daemon.push(&world, last.clone());
    world.advance(secs(3.0));
    assert_eq!(hal.requests(), 1);
    // Nothing changed before Perform.
    assert_eq!(hal.stream_channels(INPUT_STREAM), 8);
    assert!(daemon.applied().iter().all(|a| a.config_gen == 1));

    hal.perform();
    world.reg.settle();
    assert_eq!(hal.stream_channels(INPUT_STREAM), 4);
    assert_eq!(hal.stream_channels(OUTPUT_STREAM), 4);
    assert!(hal.ovst().contains(" in=4 out=4 "), "{}", hal.ovst());
    assert_eq!(hal.host.storage(STORAGE_KEY), Some(last.to_storage_string()));
    assert_eq!(
        daemon.applied().last(),
        Some(&ConfigApplied {
            daemon_generation: GEN_A,
            config_gen: 4,
            sample_rate: 48_000,
            input_channels: 4,
            output_channels: 4,
        })
    );
    let word = AppliedWord { sample_rate: 48_000, config_gen: 4 }.pack();
    assert_eq!(plugin_status(&daemon.state.region).2, word);

    // Nothing is wanted any more.
    world.advance(secs(3.0));
    assert_eq!(hal.requests(), 1);
}

#[test]
fn a_change_of_names_is_published_at_once() {
    let world = World::new();
    let daemon = Daemon::start(&world, SERVICE_NAME, GEN_A, config_1());
    let hal = Hal::new(&world, SERVICE_NAME, None);
    hal.host.take_calls();

    let mut renamed = DriverConfig { config_gen: 2, ..config_1() };
    renamed.input_names[0] = "Vox".into();
    renamed.output_names[7] = "Click".into();
    daemon.push(&world, renamed.clone());

    let lchn = |scope| {
        address(kAudioObjectPropertyElementName, scope, kAudioObjectPropertyElementWildcard)
    };
    assert_eq!(
        hal.host.take_calls(),
        vec![
            HostCall::PropertiesChanged {
                object: DEVICE,
                addresses: vec![
                    lchn(kAudioObjectPropertyScopeInput),
                    lchn(kAudioObjectPropertyScopeOutput)
                ],
            },
            HostCall::WriteToStorage {
                key: STORAGE_KEY.into(),
                value: Some(renamed.to_storage_string())
            },
        ]
    );
    assert_eq!(hal.element_name(kAudioObjectPropertyScopeInput, 1), "Vox");
    assert_eq!(hal.element_name(kAudioObjectPropertyScopeOutput, 8), "Click");
    assert_eq!(daemon.applied().last().map(|a| a.config_gen), Some(2));

    // An invalid configuration is ignored, and reported once.
    let invalid = DriverConfig { input_channels: 0, input_names: vec![], ..config_4x4(3) };
    daemon.push(&world, invalid.clone());
    daemon.push(&world, invalid);
    world.advance(secs(5.0));
    assert!(hal.host.take_calls().is_empty());
    assert_eq!(hal.element_name(kAudioObjectPropertyScopeInput, 1), "Vox");
    let reports = world
        .platform
        .logs()
        .iter()
        .filter(|(_, l)| l.contains("configuration 3 from the daemon ignored"))
        .count();
    assert_eq!(reports, 1);
    assert_eq!(hal.requests(), 0);
}

#[test]
fn an_aborted_change_is_requested_again_after_1_s() {
    let world = World::new();
    let daemon = Daemon::start(&world, SERVICE_NAME, GEN_A, config_1());
    let hal = Hal::new(&world, SERVICE_NAME, None);
    world.advance(secs(2.5));

    // Past the start delay, the request goes out at once.
    daemon.push(&world, config_4x4(2));
    assert_eq!(hal.requests(), 1);
    hal.abort();
    world.reg.settle();
    world.advance(secs(0.95));
    assert_eq!(hal.requests(), 1);
    world.advance(secs(0.1));
    assert_eq!(hal.requests(), 2);

    hal.perform();
    world.reg.settle();
    assert_eq!(hal.stream_channels(INPUT_STREAM), 4);
    assert_eq!(daemon.applied().last().map(|a| a.config_gen), Some(2));

    // A Perform with nothing pending ends a request too: here a change of
    // names superseded the structural one while it was outstanding.
    daemon.push(&world, DriverConfig { config_gen: 3, ..config_1() });
    world.advance(secs(1.1));
    assert_eq!(hal.requests(), 3);
    let mut renamed = config_4x4(4);
    renamed.input_names[0] = "Vox".into();
    daemon.push(&world, renamed);
    hal.perform();
    world.reg.settle();
    assert_eq!(hal.stream_channels(INPUT_STREAM), 4);
    assert_eq!(hal.element_name(kAudioObjectPropertyScopeInput, 1), "Vox");
    // The next structural change is requested again.
    daemon.push(&world, DriverConfig { config_gen: 5, ..config_1() });
    world.advance(secs(1.1));
    assert_eq!(hal.requests(), 4);
}

#[test]
fn a_new_daemon_is_found_on_the_fast_schedule_and_its_region_swapped_in() {
    let world = World::new();
    let a = Daemon::start(&world, SERVICE_NAME, GEN_A, config_1());
    let hal = Hal::new(&world, SERVICE_NAME, None);
    assert!(attached(&hal, GEN_A));
    let region_a = a.state.region.clone();
    world.advance(secs(1.0));

    // The daemon dies; nobody serves the name for a while.
    let held = Arc::strong_count(&region_a);
    let t0 = world.now();
    drop(a.server);
    world.reg.settle();
    assert!(hal.ovst().starts_with("daemon=connecting "), "{}", hal.ovst());
    world.advance(secs(12.0));
    let hellos: Vec<Duration> = hal.hellos_since(t0).iter().map(|t| *t - t0).collect();
    assert_eq!(hellos, [0.0, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0].map(secs));
    // The region stays attached meanwhile: the gate closes on the stale
    // heartbeat instead.
    assert_eq!(Arc::strong_count(&region_a), held);

    // A new daemon, with a new region, answers the next hello.
    let b = Daemon::start(&world, SERVICE_NAME, GEN_B, config_1());
    world.advance_to(t0 + secs(15.0));
    let swap = world.now();
    assert!(attached(&hal, GEN_B), "{}", hal.ovst());
    assert!(hal.ovst().contains(" attach=2 "), "{}", hal.ovst());
    let hellos = b.hellos();
    assert_eq!(hellos.len(), 1);
    assert_eq!(hellos[0].0, t0 + secs(15.0));
    assert_eq!((hellos[0].2.applied_daemon_generation, hellos[0].2.applied_config_gen), (GEN_A, 1));
    assert_eq!(b.applied().last().map(|a| a.daemon_generation), Some(GEN_B));
    assert_eq!(plugin_status(&b.state.region).4, 2);
    // No device change: the HAL sees nothing of the restart.
    assert_eq!(hal.requests(), 0);

    // The old region is freed only after the grace.
    world.advance(secs(0.5));
    assert_eq!(Arc::strong_count(&region_a), held);
    world.advance_to(swap + Duration::from_nanos(RETIRE_GRACE_NS) + secs(0.05));
    assert_eq!(Arc::strong_count(&region_a), held - 1);

    // The countdown to releasing the region stopped with the welcome.
    world.advance_to(t0 + secs(40.0));
    assert!(attached(&hal, GEN_B), "{}", hal.ovst());
    assert_eq!(b.hellos().len(), 1);
}

#[test]
fn a_lost_daemon_is_released_after_30_s() {
    let world = World::new();
    let a = Daemon::start(&world, SERVICE_NAME, GEN_A, config_1());
    let hal = Hal::new(&world, SERVICE_NAME, None);
    let region_a = a.state.region.clone();
    let held = Arc::strong_count(&region_a);
    drop(a.server);
    world.reg.settle();
    world.advance(secs(29.9));
    assert_eq!(Arc::strong_count(&region_a), held);
    world.advance(secs(1.2));
    assert!(hal.ovst().starts_with("daemon=connecting "), "{}", hal.ovst());
    assert_eq!(Arc::strong_count(&region_a), held - 1);
}

#[test]
fn bye_reconnects_without_remapping_the_same_region() {
    let world = World::new();
    let a = Daemon::start(&world, SERVICE_NAME, GEN_A, config_1());
    let hal = Hal::new(&world, SERVICE_NAME, None);
    let held = Arc::strong_count(&a.state.region);
    world.advance(secs(1.0));
    *lock(&a.state.answer) = Answer::Silent;
    let t0 = world.now();
    a.send(&world, ToPlugin::Bye { reason: "test".into() });
    assert!(hal.ovst().starts_with("daemon=connecting "), "{}", hal.ovst());
    world.advance(secs(0.3));
    assert_eq!(hal.hellos_since(t0), vec![t0, t0 + secs(0.25)]);
    // The daemon stayed after all and answers: the same generation keeps
    // its mapping.
    *lock(&a.state.answer) = Answer::Welcome;
    world.advance(secs(0.25));
    assert!(attached(&hal, GEN_A), "{}", hal.ovst());
    assert!(hal.ovst().contains(" attach=1 "), "{}", hal.ovst());
    assert_eq!(Arc::strong_count(&a.state.region), held);
}

#[test]
fn a_daemon_with_another_layout_is_asked_again_every_30_s() {
    let world = World::new();
    let daemon = Daemon::start(&world, SERVICE_NAME, GEN_A, config_1());
    *lock(&daemon.state.answer) = Answer::RejectLayout;
    let hal = Hal::new(&world, SERVICE_NAME, None);
    assert!(hal.ovst().starts_with("daemon=incompatible(layout) "), "{}", hal.ovst());
    assert!(world.platform.logged("the daemon refused this plug-in (layout)"));
    // The device stays, inert.
    assert_eq!(hal.stream_channels(INPUT_STREAM), 8);

    world.advance(secs(95.0));
    let at: Vec<Duration> = daemon.hellos().iter().map(|(t, _, _)| *t).collect();
    assert_eq!(at, [0.0, 30.0, 60.0, 90.0].map(secs));
    assert!(hal.ovst().starts_with("daemon=incompatible(layout) "), "{}", hal.ovst());

    // An upgraded daemon is found at the next try.
    *lock(&daemon.state.answer) = Answer::Welcome;
    world.advance(secs(30.0));
    assert!(attached(&hal, GEN_A), "{}", hal.ovst());
}

#[test]
fn a_missing_service_is_tried_every_5_s() {
    let world = World::new();
    let service = "org.openvirtualsoundcard.test.absent";
    let hal = Hal::new(&world, service, None);
    assert!(hal.ovst().starts_with("daemon=absent "), "{}", hal.ovst());
    world.advance(secs(21.0));
    let every_5 = [0.0, 5.0, 10.0, 15.0, 20.0].map(secs);
    assert_eq!(hal.spy.times(&Sent::Connect, Duration::ZERO), every_5);
    assert_eq!(hal.spy.times(&Sent::Cancel, Duration::ZERO), every_5);
    assert_eq!(hal.hellos_since(Duration::ZERO), every_5);

    // Installed later, the daemon is found on the 5 s beat.
    let daemon = Daemon::start(&world, service, GEN_A, config_1());
    world.advance(secs(3.0));
    assert!(daemon.hellos().is_empty());
    world.advance(secs(2.0));
    assert_eq!(daemon.hellos().len(), 1);
    assert_eq!(daemon.hellos()[0].0, secs(25.0));
    assert!(attached(&hal, GEN_A), "{}", hal.ovst());
}

#[test]
fn a_second_driver_object_attaches_to_the_same_region() {
    let world = World::new();
    let daemon = Daemon::start(&world, SERVICE_NAME, GEN_A, config_1());
    let region = daemon.state.region.clone();
    let before = Arc::strong_count(&region);
    let first = Hal::new(&world, SERVICE_NAME, None);
    assert!(attached(&first, GEN_A));
    assert_eq!(Arc::strong_count(&region), before + 1);

    // coreaudiod restarted: a new helper loads a new driver object, with
    // the configuration the first one stored.
    let second = Hal::new(&world, SERVICE_NAME, Some(&config_1()));
    assert!(attached(&second, GEN_A), "{}", second.ovst());
    assert!(second.ovst().contains(" attach=1 "), "{}", second.ovst());
    assert_eq!(Arc::strong_count(&region), before + 2);
    let hellos = daemon.hellos();
    assert_eq!(hellos.len(), 2);
    assert_ne!(hellos[0].1, hellos[1].1, "two peers");
    assert_ne!(hellos[0].2.instance, hellos[1].2.instance);
    assert_eq!(hellos[1].2.applied_config_gen, 1);
    assert_eq!(lock(&daemon.state.peers).len(), 2);
    // The newest driver object wrote the plug-in status last.
    assert_eq!(plugin_status(&region).0, hellos[1].2.instance);
    assert_eq!(daemon.applied().len(), 2);
}

// --- The retire protocol, with a sink whose quiescence the test controls -----

/// A sink with an attachment slot of its own, whose IO engine is busy until
/// the test says otherwise.
struct Sink {
    platform: &'static StubPlatform,
    quiet: AtomicBool,
    slot: Mutex<Option<Box<Attachment>>>,
    statuses: Mutex<Vec<LinkStatus>>,
}

impl LinkSink for Sink {
    fn attach(&self, a: Option<Box<Attachment>>) -> Option<Box<Attachment>> {
        std::mem::replace(&mut *lock(&self.slot), a)
    }

    fn quiescent(&self) -> bool {
        self.quiet.load(Ordering::SeqCst)
    }

    fn current_generation(&self) -> Option<u64> {
        lock(&self.slot).as_ref().map(|a| a.generation)
    }

    fn published_config(&self) -> DriverConfig {
        config_1()
    }

    fn stage(&self, cfg: DriverConfig) -> ConfigPlan {
        ConfigPlan::of(&config_1(), &cfg)
    }

    fn request_config_change(&self) -> OSStatus {
        0
    }

    fn names_changed(&self) {}

    fn store(&self, _cfg: &DriverConfig) {}

    fn set_status(&self, s: LinkStatus) {
        lock(&self.statuses).push(s);
    }

    fn now_ns(&self) -> u64 {
        self.platform.timebase().ticks_to_ns(self.platform.now_ticks())
    }
}

#[test]
fn an_old_region_waits_for_a_quiescent_io_engine() {
    let world = World::new();
    let a = Daemon::start(&world, SERVICE_NAME, GEN_A, config_1());
    let sink = Arc::new(Sink {
        platform: world.platform,
        quiet: AtomicBool::new(false),
        slot: Mutex::new(None),
        statuses: Mutex::new(Vec::new()),
    });
    let spy = Spy::new(&world, SERVICE_NAME);
    let link = Link::new(spy, sink.clone(), 7, 4242);
    link.start();
    world.reg.settle();
    assert_eq!(link.status(), LinkStatus::Attached { generation: GEN_A });
    let region_a = a.state.region.clone();
    let held = Arc::strong_count(&region_a);

    // The daemon restarts with a new region while IO runs. The new one is
    // serving the name when the old one goes.
    let b = Daemon::start(&world, SERVICE_NAME, GEN_B, config_1());
    drop(a.server);
    world.reg.settle();
    assert_eq!(link.status(), LinkStatus::Attached { generation: GEN_B });
    assert_eq!(link.attach_count(), 2);
    assert_eq!(sink.current_generation(), Some(GEN_B));
    assert_eq!(
        *lock(&sink.statuses),
        vec![
            LinkStatus::Connecting,
            LinkStatus::Attached { generation: GEN_A },
            LinkStatus::Connecting,
            LinkStatus::Attached { generation: GEN_B },
        ]
    );

    // Long past the grace, a busy engine keeps the old region.
    world.advance(secs(3.0));
    assert_eq!(Arc::strong_count(&region_a), held);
    sink.quiet.store(true, Ordering::SeqCst);
    world.advance(secs(0.02));
    assert_eq!(Arc::strong_count(&region_a), held - 1);
    assert_eq!(b.hellos().len(), 1);
}
