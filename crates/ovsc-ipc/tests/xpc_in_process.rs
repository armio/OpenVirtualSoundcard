//! The XPC transports in one process, over an anonymous listener: the
//! handshake, a full-size xpc_shmem region shared both ways, retiring a
//! mapping, the shim queue, and what clients see when the server goes away.

#![cfg(target_os = "macos")]

use std::ffi::{CStr, c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ovsc_ipc::protocol::*;
use ovsc_ipc::region::SharedRegion;
use ovsc_ipc::transport::*;
use ovsc_ipc::xpc::{XpcClient, anonymous_pair, queue_run};
use ovsc_shm::layout::{HOST_ARCH, HeaderInit, REGION_SIZE, RING_FRAMES, RegionRef};
use ovsc_shm::time::Timebase;

const WAIT: Duration = Duration::from_secs(10);

unsafe extern "C" {
    fn dispatch_queue_get_label(queue: *mut c_void) -> *const c_char;
}

/// Whether the caller runs on the shim's queue.
fn on_ipc_queue() -> bool {
    // SAFETY: a null queue asks for the current queue's label, which lives
    // as long as the queue.
    let label = unsafe { dispatch_queue_get_label(std::ptr::null_mut()) };
    !label.is_null()
        && unsafe { CStr::from_ptr(label) }.to_bytes() == b"org.openvirtualsoundcard.ipc"
}

#[derive(Debug)]
enum Event {
    Peer(PeerInfo, bool),
    Request(u64, ToDaemon, bool),
    Message(u64, ToDaemon),
    Gone(u64),
    ClientMessage(ToPlugin),
    Interrupted,
    Invalid,
}

struct Daemon {
    region: Arc<SharedRegion>,
    generation: u64,
    accept: AtomicBool,
    answer: AtomicBool,
    events: Mutex<Sender<Event>>,
}

impl Daemon {
    fn new(region: Arc<SharedRegion>, generation: u64) -> (Arc<Self>, Receiver<Event>) {
        let (tx, rx) = mpsc::channel();
        let d = Daemon {
            region,
            generation,
            accept: AtomicBool::new(true),
            answer: AtomicBool::new(true),
            events: Mutex::new(tx),
        };
        (Arc::new(d), rx)
    }

    fn emit(&self, e: Event) {
        let _ = self.events.lock().unwrap().send(e);
    }
}

impl ServerHandler for Daemon {
    fn on_peer(&self, p: PeerInfo) -> bool {
        self.emit(Event::Peer(p, on_ipc_queue()));
        self.accept.load(Ordering::SeqCst)
    }

    fn on_request(&self, peer: u64, m: ToDaemon) -> Option<ToPlugin> {
        self.emit(Event::Request(peer, m, on_ipc_queue()));
        self.answer.load(Ordering::SeqCst).then(|| {
            ToPlugin::Welcome(Welcome {
                proto_major: PROTO_MAJOR,
                proto_minor: PROTO_MINOR,
                daemon_version: "xpc test".to_owned(),
                daemon_generation: self.generation,
                region: self.region.handle(),
                region_size: self.region.len() as u64,
                config: DriverConfig::fallback(),
            })
        })
    }

    fn on_message(&self, peer: u64, m: ToDaemon) {
        self.emit(Event::Message(peer, m));
    }

    fn on_peer_gone(&self, peer: u64) {
        self.emit(Event::Gone(peer));
    }
}

struct Plugin(Mutex<Sender<Event>>);

impl Plugin {
    fn new() -> (Arc<Self>, Receiver<Event>) {
        let (tx, rx) = mpsc::channel();
        (Arc::new(Plugin(Mutex::new(tx))), rx)
    }

    fn emit(&self, e: Event) {
        let _ = self.0.lock().unwrap().send(e);
    }
}

impl ClientHandler for Plugin {
    fn on_message(&self, m: ToPlugin) {
        self.emit(Event::ClientMessage(m));
    }
    fn on_interrupted(&self) {
        self.emit(Event::Interrupted);
    }
    fn on_invalid(&self) {
        self.emit(Event::Invalid);
    }
}

fn hello() -> ToDaemon {
    ToDaemon::Hello(Hello {
        proto_major: PROTO_MAJOR,
        proto_minor: PROTO_MINOR,
        layout_version: 1,
        layout_hash: 2,
        plugin_version: "xpc test".to_owned(),
        instance: 3,
        pid: std::process::id() as i32,
        applied_daemon_generation: 0,
        applied_config_gen: 0,
        sample_rate: 48_000,
        input_channels: 8,
        output_channels: 8,
        timebase_numer: 1,
        timebase_denom: 1,
        arch: HOST_ARCH,
    })
}

/// Sends a hello and waits for the outcome; also reports whether the reply
/// ran on the shim queue.
fn request_hello(
    client: &XpcClient,
    timeout: Duration,
) -> (Result<ToPlugin, TransportError>, bool) {
    let (tx, rx) = mpsc::channel();
    client.request(hello(), timeout, Box::new(move |r| tx.send((r, on_ipc_queue())).unwrap()));
    rx.recv_timeout(WAIT + timeout).expect("no reply at all")
}

/// A daemon region, laid out.
fn daemon_region(generation: u64) -> Arc<SharedRegion> {
    let region = SharedRegion::create(REGION_SIZE).unwrap();
    let init = HeaderInit {
        daemon_generation: generation,
        daemon_pid: std::process::id(),
        arch: HOST_ARCH,
        timebase: Timebase::NANOS,
        created_host_ns: 1,
        daemon_version: HeaderInit::version_bytes("xpc test"),
    };
    // SAFETY: fresh and not yet shared; the Arc keeps it alive while the
    // test uses views of it.
    unsafe { RegionRef::init(region.as_ptr(), region.len(), &init) }.unwrap();
    region
}

fn view(ptr: *mut u8, len: usize) -> RegionRef<'static> {
    // SAFETY: each test keeps the mapping alive while it uses the view.
    unsafe { RegionRef::from_raw(ptr, len) }.unwrap()
}

fn word(base: *mut u8, off: usize) -> &'static AtomicU64 {
    // SAFETY: the offsets used are 8-aligned and inside a live mapping.
    unsafe { &*(base.add(off) as *const AtomicU64) }
}

#[test]
fn handshake_shares_a_full_region_both_ways() {
    let region = daemon_region(0x5EED);
    let (server, client) = anonymous_pair();
    let (daemon, server_events) = Daemon::new(region.clone(), 0x5EED);
    server.start(daemon).unwrap();
    let (plugin, _client_events) = Plugin::new();
    client.connect(plugin);

    let (reply, reply_on_queue) = request_hello(&client, Duration::from_secs(5));
    assert!(reply_on_queue, "the reply ran off the shim queue");
    let Ok(ToPlugin::Welcome(w)) = reply else { panic!("{reply:?}") };
    assert_eq!((w.daemon_generation, w.region_size), (0x5EED, REGION_SIZE as u64));
    assert_eq!(w.config, DriverConfig::fallback());

    let peer = match server_events.recv_timeout(WAIT).unwrap() {
        Event::Peer(p, on_queue) => {
            assert!(on_queue);
            // SAFETY: no preconditions.
            assert_eq!(p.euid, unsafe { libc::geteuid() });
            assert_eq!(p.pid, std::process::id() as i32);
            p
        }
        e => panic!("{e:?}"),
    };
    match server_events.recv_timeout(WAIT).unwrap() {
        Event::Request(id, m, on_queue) => {
            assert!(on_queue);
            assert_eq!((id, m), (peer.id, hello()));
        }
        e => panic!("{e:?}"),
    }

    // The client maps the xpc_shmem: the same memory, validated.
    let mapped = w.region.map().unwrap();
    assert!(mapped.len() >= REGION_SIZE);
    assert_ne!(mapped.as_ptr(), region.as_ptr(), "a mapping of its own");
    let daemon_view = view(region.as_ptr(), region.len());
    let plugin_view = view(mapped.as_ptr(), mapped.len());
    assert_eq!(plugin_view.header().daemon_generation, 0x5EED);
    mapped.touch(0, mapped.len());

    // Daemon to plug-in, first and last RX rings.
    daemon_view.rx(0).unwrap().write_one(1000, 11);
    daemon_view.rx(127).unwrap().write_one(RING_FRAMES as u64 - 1, -22);
    assert_eq!(plugin_view.rx(0).unwrap().read_one(1000), Some(11));
    assert_eq!(plugin_view.rx(127).unwrap().read_one(RING_FRAMES as u64 - 1), Some(-22));
    // Plug-in to daemon, last TX ring (the end of the region).
    plugin_view.tx(127).unwrap().write_one(RING_FRAMES as u64 - 1, 33);
    plugin_view.tx(64).unwrap().write_one(5, 44);
    assert_eq!(daemon_view.tx(127).unwrap().read_one(RING_FRAMES as u64 - 1), Some(33));
    assert_eq!(daemon_view.tx(64).unwrap().read_one(5), Some(44));
    // The region's last word is that ring's last slot; overwrite it raw.
    word(region.as_ptr(), REGION_SIZE - 8).store(0xD0, Ordering::SeqCst);
    assert_eq!(word(mapped.as_ptr(), REGION_SIZE - 8).load(Ordering::SeqCst), 0xD0);

    // Retiring the mapping: the plug-in reads zeros, the daemon keeps its
    // data, and the plug-in's later writes stay private.
    mapped.neutralize().unwrap();
    assert_eq!(plugin_view.rx(0).unwrap().read_one(1000), None);
    assert_eq!(word(mapped.as_ptr(), REGION_SIZE - 8).load(Ordering::SeqCst), 0);
    assert_eq!(word(mapped.as_ptr(), 0).load(Ordering::SeqCst), 0, "header gone");
    plugin_view.rx(0).unwrap().write_one(1000, 99);
    assert_eq!(daemon_view.rx(0).unwrap().read_one(1000), Some(11));
    assert_eq!(daemon_view.tx(64).unwrap().read_one(5), Some(44));
    assert_eq!(word(region.as_ptr(), REGION_SIZE - 8).load(Ordering::SeqCst), 0xD0);
    assert_eq!(daemon_view.header().daemon_generation, 0x5EED);
    mapped.neutralize().unwrap();
    drop(mapped);
    assert_eq!(daemon_view.rx(127).unwrap().read_one(RING_FRAMES as u64 - 1), Some(-22));

    // A second mapping of the same handle sees the daemon's data again.
    let again = w.region.map().unwrap();
    assert_eq!(view(again.as_ptr(), again.len()).rx(0).unwrap().read_one(1000), Some(11));
    drop(again);
    drop(client);
    drop(server);
}

#[test]
fn config_push_and_config_applied() {
    let (server, client) = anonymous_pair();
    let (daemon, server_events) = Daemon::new(SharedRegion::create(1 << 20).unwrap(), 1);
    server.start(daemon).unwrap();
    let (plugin, client_events) = Plugin::new();
    client.connect(plugin);
    assert!(matches!(request_hello(&client, Duration::from_secs(5)).0, Ok(ToPlugin::Welcome(_))));
    let Event::Peer(peer, _) = server_events.recv_timeout(WAIT).unwrap() else { panic!() };
    let _request = server_events.recv_timeout(WAIT).unwrap();

    let mut cfg = DriverConfig::fallback();
    cfg.config_gen = 4;
    cfg.output_names[7] = "Grüße, 100%".to_owned();
    server.send(peer.id, ToPlugin::Config(cfg.clone()));
    match client_events.recv_timeout(WAIT).unwrap() {
        Event::ClientMessage(ToPlugin::Config(got)) => assert_eq!(got, cfg),
        e => panic!("{e:?}"),
    }

    let applied = ToDaemon::ConfigApplied(ConfigApplied {
        daemon_generation: 1,
        config_gen: 4,
        sample_rate: 48_000,
        input_channels: 8,
        output_channels: 8,
    });
    client.send(applied.clone());
    match server_events.recv_timeout(WAIT).unwrap() {
        Event::Message(p, m) => assert_eq!((p, m), (peer.id, applied)),
        e => panic!("{e:?}"),
    }

    // The client leaving is a gone peer.
    client.cancel();
    match server_events.recv_timeout(WAIT).unwrap() {
        Event::Gone(p) => assert_eq!(p, peer.id),
        e => panic!("{e:?}"),
    }
}

#[test]
fn after_and_run_fire_on_the_shim_queue() {
    let client = XpcClient::new("org.openvirtualsoundcard.test.unused");
    let (tx, rx) = mpsc::channel();
    let start = Instant::now();
    let t = tx.clone();
    client.after(
        Duration::from_millis(50),
        Box::new(move || t.send(("after", on_ipc_queue())).unwrap()),
    );
    let t = tx.clone();
    client.run(Box::new(move || t.send(("run", on_ipc_queue())).unwrap()));
    queue_run(Box::new(move || tx.send(("queue_run", on_ipc_queue())).unwrap()));
    let mut got: Vec<_> = (0..3).map(|_| rx.recv_timeout(WAIT).unwrap()).collect();
    assert!(start.elapsed() >= Duration::from_millis(50));
    assert_eq!(got.pop(), Some(("after", true)), "the delayed one comes last");
    got.sort_unstable();
    assert_eq!(got, [("queue_run", true), ("run", true)]);
}

#[test]
fn request_timeout() {
    let (server, client) = anonymous_pair();
    let (daemon, _events) = Daemon::new(SharedRegion::create(1 << 20).unwrap(), 1);
    daemon.answer.store(false, Ordering::SeqCst);
    server.start(daemon).unwrap();
    let (plugin, _client_events) = Plugin::new();
    client.connect(plugin);
    let start = Instant::now();
    let (r, on_queue) = request_hello(&client, Duration::from_millis(200));
    assert_eq!(r, Err(TransportError::Timeout));
    assert!(on_queue);
    assert!(start.elapsed() >= Duration::from_millis(200));
    // Without a connection, a request fails at once.
    client.cancel();
    assert_eq!(request_hello(&client, Duration::from_secs(5)).0, Err(TransportError::Invalid));
}

#[test]
fn cancelling_the_server_interrupts_or_invalidates_the_client() {
    let (server, client) = anonymous_pair();
    let (daemon, _events) = Daemon::new(SharedRegion::create(1 << 20).unwrap(), 1);
    server.start(daemon.clone()).unwrap();
    let (plugin, client_events) = Plugin::new();
    client.connect(plugin);
    assert!(matches!(request_hello(&client, Duration::from_secs(5)).0, Ok(ToPlugin::Welcome(_))));

    // A request in flight when the server goes away ends early.
    daemon.answer.store(false, Ordering::SeqCst);
    let (tx, rx) = mpsc::channel();
    client.request(hello(), Duration::from_secs(30), Box::new(move |r| tx.send(r).unwrap()));
    std::thread::sleep(Duration::from_millis(100));
    drop(server);
    let r = rx.recv_timeout(WAIT).expect("the pending request did not end");
    assert!(matches!(r, Err(TransportError::Interrupted | TransportError::Invalid)), "{r:?}");
    let e = client_events.recv_timeout(WAIT).unwrap();
    assert!(matches!(e, Event::Interrupted | Event::Invalid), "{e:?}");
}

#[test]
fn refused_peer_gets_no_reply() {
    let (server, client) = anonymous_pair();
    let (daemon, server_events) = Daemon::new(SharedRegion::create(1 << 20).unwrap(), 1);
    daemon.accept.store(false, Ordering::SeqCst);
    server.start(daemon).unwrap();
    let (plugin, _client_events) = Plugin::new();
    client.connect(plugin);
    let (r, _) = request_hello(&client, Duration::from_secs(5));
    assert!(matches!(r, Err(TransportError::Interrupted | TransportError::Invalid)), "{r:?}");
    assert!(matches!(server_events.recv_timeout(WAIT).unwrap(), Event::Peer(_, true)));
    assert!(server_events.recv_timeout(Duration::from_millis(200)).is_err(), "no request");
}

#[test]
fn unknown_service_is_invalid() {
    let client =
        XpcClient::new(&format!("org.openvirtualsoundcard.test.missing.{}", std::process::id()));
    let (plugin, client_events) = Plugin::new();
    client.connect(plugin);
    assert_eq!(request_hello(&client, Duration::from_secs(5)).0, Err(TransportError::Invalid));
    assert!(matches!(client_events.recv_timeout(WAIT).unwrap(), Event::Invalid));
}
