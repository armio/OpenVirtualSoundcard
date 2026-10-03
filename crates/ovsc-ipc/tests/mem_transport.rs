//! The in-process transport: XPC's semantics, on manual and real time.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ovsc_ipc::mem::MemRegistry;
use ovsc_ipc::protocol::*;
use ovsc_ipc::region::SharedRegion;
use ovsc_ipc::transport::*;

const SERVICE: &str = "org.openvirtualsoundcard.test";
const SECOND: Duration = Duration::from_secs(1);

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

#[derive(Clone, Debug, PartialEq)]
enum ClientEvent {
    Message(ToPlugin),
    Interrupted,
    Invalid,
}

#[derive(Default)]
struct ClientLog(Mutex<Vec<ClientEvent>>);

impl ClientLog {
    fn take(&self) -> Vec<ClientEvent> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

impl ClientHandler for ClientLog {
    fn on_message(&self, m: ToPlugin) {
        self.0.lock().unwrap().push(ClientEvent::Message(m));
    }
    fn on_interrupted(&self) {
        self.0.lock().unwrap().push(ClientEvent::Interrupted);
    }
    fn on_invalid(&self) {
        self.0.lock().unwrap().push(ClientEvent::Invalid);
    }
}

#[derive(Clone, Debug, PartialEq)]
enum ServerEvent {
    Peer(PeerInfo),
    Request(u64, ToDaemon),
    Message(u64, ToDaemon),
    Gone(u64),
}

/// A daemon: welcomes compatible hellos from allowed users.
struct Daemon {
    allowed: Vec<u32>,
    region: Arc<SharedRegion>,
    generation: u64,
    answer: AtomicBool,
    events: Mutex<Vec<ServerEvent>>,
}

impl Daemon {
    fn new(generation: u64) -> Arc<Self> {
        let region = SharedRegion::create(64 * 1024).unwrap();
        Arc::new(Daemon {
            allowed: vec![202, 0],
            region,
            generation,
            answer: AtomicBool::new(true),
            events: Mutex::new(Vec::new()),
        })
    }

    fn take(&self) -> Vec<ServerEvent> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }
}

impl ServerHandler for Daemon {
    fn on_peer(&self, p: PeerInfo) -> bool {
        self.events.lock().unwrap().push(ServerEvent::Peer(p));
        self.allowed.contains(&p.euid)
    }

    fn on_request(&self, peer: u64, m: ToDaemon) -> Option<ToPlugin> {
        self.events.lock().unwrap().push(ServerEvent::Request(peer, m.clone()));
        if !self.answer.load(Ordering::SeqCst) {
            return None;
        }
        let ToDaemon::Hello(h) = m else { return None };
        if h.proto_major != PROTO_MAJOR {
            return Some(ToPlugin::Reject(Reject {
                reason: RejectReason::Proto,
                proto_major: PROTO_MAJOR,
                layout_version: 1,
                layout_hash: 0,
                message: format!("protocol {} unsupported", h.proto_major),
            }));
        }
        Some(ToPlugin::Welcome(Welcome {
            proto_major: PROTO_MAJOR,
            proto_minor: PROTO_MINOR,
            daemon_version: "test".to_owned(),
            daemon_generation: self.generation,
            region: self.region.handle(),
            region_size: self.region.len() as u64,
            config: DriverConfig::fallback(),
        }))
    }

    fn on_message(&self, peer: u64, m: ToDaemon) {
        self.events.lock().unwrap().push(ServerEvent::Message(peer, m));
    }

    fn on_peer_gone(&self, peer: u64) {
        self.events.lock().unwrap().push(ServerEvent::Gone(peer));
    }
}

fn hello(major: u32) -> ToDaemon {
    ToDaemon::Hello(Hello {
        proto_major: major,
        proto_minor: 0,
        layout_version: 1,
        layout_hash: 2,
        plugin_version: "test".to_owned(),
        instance: 3,
        pid: 77,
        applied_daemon_generation: 0,
        applied_config_gen: 0,
        sample_rate: 48_000,
        input_channels: 8,
        output_channels: 8,
        timebase_numer: 1,
        timebase_denom: 1,
        arch: 1,
    })
}

type Outcome = Arc<Mutex<Vec<Result<ToPlugin, TransportError>>>>;

/// A reply callback that records its outcome.
fn recorder() -> (Outcome, ReplyFn) {
    let out: Outcome = Arc::default();
    let o = out.clone();
    (out, Box::new(move |r| o.lock().unwrap().push(r)))
}

fn taken(o: &Outcome) -> Vec<Result<ToPlugin, TransportError>> {
    std::mem::take(&mut *o.lock().unwrap())
}

fn welcome_generation(r: &Result<ToPlugin, TransportError>) -> u64 {
    match r {
        Ok(ToPlugin::Welcome(w)) => w.daemon_generation,
        other => panic!("expected a welcome, got {other:?}"),
    }
}

#[test]
fn handshake_shares_the_region() {
    let reg = MemRegistry::with_manual_time();
    let server = reg.bind(SERVICE);
    let daemon = Daemon::new(11);
    server.start(daemon.clone()).unwrap();
    let client = reg.client(SERVICE, 202, 77);
    let log = Arc::new(ClientLog::default());
    client.connect(log.clone());

    let (out, reply) = recorder();
    client.request(hello(PROTO_MAJOR), 2 * SECOND, reply);
    reg.settle();

    let events = daemon.take();
    let ServerEvent::Peer(peer) = events[0] else { panic!("{events:?}") };
    assert_eq!((peer.euid, peer.pid), (202, 77));
    assert_eq!(events[1], ServerEvent::Request(peer.id, hello(PROTO_MAJOR)));
    let outcome = taken(&out);
    assert_eq!(outcome.len(), 1);
    assert_eq!(welcome_generation(&outcome[0]), 11);
    let Ok(ToPlugin::Welcome(w)) = &outcome[0] else { unreachable!() };
    assert_eq!(w.config, DriverConfig::fallback());

    // The mapping and the daemon's region are the same memory.
    let mapped = w.region.map().unwrap();
    assert_eq!(mapped.len(), daemon.region.len());
    let word =
        |p: *mut u8, off: usize| unsafe { &*(p.add(off) as *const std::sync::atomic::AtomicU64) };
    word(daemon.region.as_ptr(), 4096).store(0xD0, Ordering::SeqCst);
    assert_eq!(word(mapped.as_ptr(), 4096).load(Ordering::SeqCst), 0xD0);
    word(mapped.as_ptr(), 8192).store(0xB0, Ordering::SeqCst);
    assert_eq!(word(daemon.region.as_ptr(), 8192).load(Ordering::SeqCst), 0xB0);
    mapped.touch(0, mapped.len());
    mapped.neutralize().unwrap();
    assert!(log.take().is_empty());
}

#[test]
fn incompatible_hello_is_rejected() {
    let reg = MemRegistry::with_manual_time();
    let server = reg.bind(SERVICE);
    server.start(Daemon::new(1)).unwrap();
    let client = reg.client(SERVICE, 202, 1);
    client.connect(Arc::new(ClientLog::default()));
    let (out, reply) = recorder();
    client.request(hello(PROTO_MAJOR + 1), SECOND, reply);
    reg.settle();
    match &taken(&out)[..] {
        [Ok(ToPlugin::Reject(r))] => {
            assert_eq!(r.reason, RejectReason::Proto);
            assert_eq!(r.proto_major, PROTO_MAJOR);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn config_push_and_config_applied() {
    let reg = MemRegistry::with_manual_time();
    let server = reg.bind(SERVICE);
    let daemon = Daemon::new(5);
    server.start(daemon.clone()).unwrap();
    let client = reg.client(SERVICE, 0, 2);
    let log = Arc::new(ClientLog::default());
    client.connect(log.clone());
    let (out, reply) = recorder();
    client.request(hello(PROTO_MAJOR), SECOND, reply);
    reg.settle();
    assert_eq!(welcome_generation(&taken(&out)[0]), 5);
    let ServerEvent::Peer(peer) = daemon.take()[0] else { panic!() };

    let mut cfg = DriverConfig::fallback();
    cfg.config_gen = 2;
    cfg.input_names[0] = "renamed".to_owned();
    server.send(peer.id, ToPlugin::Config(cfg.clone()));
    server.send(peer.id + 100, ToPlugin::Config(cfg.clone()));
    reg.settle();
    assert_eq!(log.take(), vec![ClientEvent::Message(ToPlugin::Config(cfg))]);

    let applied = ToDaemon::ConfigApplied(ConfigApplied {
        daemon_generation: 5,
        config_gen: 2,
        sample_rate: 48_000,
        input_channels: 8,
        output_channels: 8,
    });
    client.send(applied.clone());
    reg.settle();
    assert_eq!(daemon.take(), vec![ServerEvent::Message(peer.id, applied)]);

    server.send(peer.id, ToPlugin::Bye { reason: "stopping".to_owned() });
    reg.settle();
    assert_eq!(
        log.take(),
        vec![ClientEvent::Message(ToPlugin::Bye { reason: "stopping".to_owned() })]
    );
}

#[test]
fn interrupted_then_reconnected() {
    let reg = MemRegistry::with_manual_time();
    let server = reg.bind(SERVICE);
    let daemon = Daemon::new(1);
    server.start(daemon.clone()).unwrap();
    let client = reg.client(SERVICE, 202, 9);
    let log = Arc::new(ClientLog::default());
    client.connect(log.clone());
    let (out, reply) = recorder();
    client.request(hello(PROTO_MAJOR), 2 * SECOND, reply);
    reg.settle();
    assert_eq!(welcome_generation(&taken(&out)[0]), 1);
    let ServerEvent::Peer(first) = daemon.take()[0] else { panic!() };

    // A request the daemon never answers, then the daemon dies.
    daemon.answer.store(false, Ordering::SeqCst);
    let (pending, reply) = recorder();
    client.request(hello(PROTO_MAJOR), 2 * SECOND, reply);
    reg.settle();
    drop(server);
    reg.settle();
    assert_eq!(taken(&pending), vec![Err(TransportError::Interrupted)]);
    assert_eq!(log.take(), vec![ClientEvent::Interrupted]);

    // No daemon running: the hello is lost and times out.
    let (lost, reply) = recorder();
    client.request(hello(PROTO_MAJOR), 2 * SECOND, reply);
    reg.advance(2 * SECOND - ms(1));
    assert!(taken(&lost).is_empty());
    reg.advance(ms(1));
    assert_eq!(taken(&lost), vec![Err(TransportError::Timeout)]);

    // The next daemon instance answers on the same connection.
    let server = reg.bind(SERVICE);
    let daemon2 = Daemon::new(2);
    server.start(daemon2.clone()).unwrap();
    let (out, reply) = recorder();
    client.request(hello(PROTO_MAJOR), 2 * SECOND, reply);
    reg.settle();
    assert_eq!(welcome_generation(&taken(&out)[0]), 2);
    let ServerEvent::Peer(second) = daemon2.take()[0] else { panic!() };
    assert_ne!(second.id, first.id);
    assert!(log.take().is_empty());
    // The old daemon's requests had ended; nothing more arrives.
    reg.advance(10 * SECOND);
    assert!(taken(&pending).is_empty());
}

#[test]
fn unknown_service_is_invalid() {
    let reg = MemRegistry::with_manual_time();
    let client = reg.client("org.openvirtualsoundcard.missing", 202, 1);
    let log = Arc::new(ClientLog::default());

    // Without a connection, a request fails at once.
    let (out, reply) = recorder();
    client.request(hello(PROTO_MAJOR), SECOND, reply);
    reg.settle();
    assert_eq!(taken(&out), vec![Err(TransportError::Invalid)]);

    client.connect(log.clone());
    let (out, reply) = recorder();
    client.request(hello(PROTO_MAJOR), SECOND, reply);
    reg.settle();
    assert_eq!(taken(&out), vec![Err(TransportError::Invalid)]);
    assert_eq!(log.take(), vec![ClientEvent::Invalid]);

    // The connection stays dead: no second event, requests fail at once.
    let (out, reply) = recorder();
    client.request(hello(PROTO_MAJOR), SECOND, reply);
    client.send(hello(PROTO_MAJOR));
    reg.settle();
    assert_eq!(taken(&out), vec![Err(TransportError::Invalid)]);
    assert!(log.take().is_empty());
    reg.advance(5 * SECOND);
    assert!(taken(&out).is_empty());

    // Installing the service and reconnecting works.
    let server = reg.bind("org.openvirtualsoundcard.missing");
    server.start(Daemon::new(3)).unwrap();
    client.connect(log.clone());
    let (out, reply) = recorder();
    client.request(hello(PROTO_MAJOR), SECOND, reply);
    reg.settle();
    assert_eq!(welcome_generation(&taken(&out)[0]), 3);
    assert!(log.take().is_empty());
}

#[test]
fn refused_peer_is_interrupted() {
    let reg = MemRegistry::with_manual_time();
    let server = reg.bind(SERVICE);
    let daemon = Daemon::new(1);
    server.start(daemon.clone()).unwrap();
    let client = reg.client(SERVICE, 501, 4);
    let log = Arc::new(ClientLog::default());
    client.connect(log.clone());
    let (out, reply) = recorder();
    client.request(hello(PROTO_MAJOR), SECOND, reply);
    reg.settle();
    assert_eq!(taken(&out), vec![Err(TransportError::Interrupted)]);
    assert_eq!(log.take(), vec![ClientEvent::Interrupted]);
    // The daemon saw the peer and nothing else; a refused peer is never
    // "gone".
    let events = daemon.take();
    let [ServerEvent::Peer(first)] = events[..] else { panic!("{events:?}") };
    assert_eq!((first.euid, first.pid), (501, 4));

    // As with XPC, the next message comes back as a new peer.
    client.send(hello(PROTO_MAJOR));
    reg.settle();
    assert_eq!(log.take(), vec![ClientEvent::Interrupted]);
    let events = daemon.take();
    let [ServerEvent::Peer(second)] = events[..] else { panic!("{events:?}") };
    assert_ne!(second.id, first.id);
    client.cancel();
    reg.settle();
    assert!(daemon.take().is_empty());
}

#[test]
fn cancel_ends_the_peer_and_silences_the_handler() {
    let reg = MemRegistry::with_manual_time();
    let server = reg.bind(SERVICE);
    let daemon = Daemon::new(1);
    server.start(daemon.clone()).unwrap();
    let client = reg.client(SERVICE, 202, 4);
    let log = Arc::new(ClientLog::default());
    client.connect(log.clone());
    client.send(hello(PROTO_MAJOR));
    reg.settle();
    let ServerEvent::Peer(peer) = daemon.take()[0] else { panic!() };

    // A pending request fails with Invalid on cancel.
    daemon.answer.store(false, Ordering::SeqCst);
    let (out, reply) = recorder();
    client.request(hello(PROTO_MAJOR), SECOND, reply);
    reg.settle();
    client.cancel();
    reg.settle();
    assert_eq!(taken(&out), vec![Err(TransportError::Invalid)]);
    let events = daemon.take();
    assert_eq!(events.last(), Some(&ServerEvent::Gone(peer.id)));

    server.send(peer.id, ToPlugin::Bye { reason: String::new() });
    drop(server);
    reg.advance(5 * SECOND);
    assert!(log.take().is_empty());

    // A new connect replaces the old one silently and gets a new peer.
    let server = reg.bind(SERVICE);
    let daemon = Daemon::new(2);
    server.start(daemon.clone()).unwrap();
    let old_log = Arc::new(ClientLog::default());
    client.connect(old_log.clone());
    client.send(hello(PROTO_MAJOR));
    reg.settle();
    client.connect(log.clone());
    drop(server);
    reg.settle();
    assert!(old_log.take().is_empty());
    assert!(log.take().is_empty(), "the new connection never reached a server");
}

#[test]
fn manual_time_orders_timers() {
    let reg = MemRegistry::with_manual_time();
    let a = Arc::new(reg.client(SERVICE, 0, 1));
    let b = reg.client(SERVICE, 0, 2);
    let order: Arc<Mutex<Vec<String>>> = Arc::default();
    let push = |name: &str| {
        let (order, name) = (order.clone(), name.to_owned());
        Box::new(move || order.lock().unwrap().push(name)) as Box<dyn FnOnce() + Send>
    };

    a.after(ms(30), push("a30"));
    {
        // A timer that schedules another one on the way.
        let (a2, d) = (a.clone(), push("d15"));
        let order = order.clone();
        a.after(
            ms(10),
            Box::new(move || {
                order.lock().unwrap().push("b10".to_owned());
                a2.after(ms(5), d);
            }),
        );
    }
    a.after(ms(10), push("c10"));
    b.after(ms(12), push("e12"));
    b.after(Duration::ZERO, push("z0"));
    a.run(push("run"));

    reg.settle();
    assert_eq!(*order.lock().unwrap(), ["run"]);
    reg.advance(Duration::ZERO);
    assert_eq!(*order.lock().unwrap(), ["run", "z0"]);
    reg.advance(ms(5));
    assert_eq!(reg.now(), ms(5));
    assert_eq!(order.lock().unwrap().len(), 2);
    reg.advance(ms(15));
    assert_eq!(reg.now(), ms(20));
    assert_eq!(*order.lock().unwrap(), ["run", "z0", "b10", "c10", "e12", "d15"]);
    reg.advance(ms(9));
    assert_eq!(order.lock().unwrap().len(), 6);
    reg.advance(ms(1));
    assert_eq!(order.lock().unwrap().last().map(String::as_str), Some("a30"));

    // Dropping a client drops its timers.
    b.after(ms(1), push("never"));
    drop(b);
    reg.advance(SECOND);
    assert_eq!(order.lock().unwrap().len(), 7);
}

#[test]
fn request_timeout_runs_on_manual_time() {
    let reg = MemRegistry::with_manual_time();
    let server = reg.bind(SERVICE);
    let daemon = Daemon::new(1);
    daemon.answer.store(false, Ordering::SeqCst);
    server.start(daemon).unwrap();
    let client = reg.client(SERVICE, 0, 1);
    client.connect(Arc::new(ClientLog::default()));
    let (out, reply) = recorder();
    client.request(hello(PROTO_MAJOR), 2 * SECOND, reply);
    reg.advance(SECOND);
    assert!(taken(&out).is_empty());
    reg.advance(SECOND);
    assert_eq!(taken(&out), vec![Err(TransportError::Timeout)]);
    // A bound but unstarted server loses messages too.
    let _idle = reg.bind("org.openvirtualsoundcard.idle");
    let client = reg.client("org.openvirtualsoundcard.idle", 0, 1);
    client.connect(Arc::new(ClientLog::default()));
    let (out, reply) = recorder();
    client.request(hello(PROTO_MAJOR), SECOND, reply);
    reg.advance(SECOND);
    assert_eq!(taken(&out), vec![Err(TransportError::Timeout)]);
}

#[test]
fn real_time_registry_works_too() {
    let reg = MemRegistry::new();
    let server = reg.bind(SERVICE);
    server.start(Daemon::new(8)).unwrap();
    assert!(server.start(Daemon::new(9)).is_err(), "started twice");
    let client = reg.client(SERVICE, 202, 1);
    client.connect(Arc::new(ClientLog::default()));
    let (tx, rx) = mpsc::channel();
    let tx2 = tx.clone();
    client.request(
        hello(PROTO_MAJOR),
        2 * SECOND,
        Box::new(move |r| tx.send(welcome_generation(&r)).unwrap()),
    );
    assert_eq!(rx.recv_timeout(5 * SECOND).unwrap(), 8);
    client.after(ms(20), Box::new(move || tx2.send(0).unwrap()));
    assert_eq!(rx.recv_timeout(5 * SECOND).unwrap(), 0);
    reg.settle();
}

#[test]
fn callback_panics_surface_in_settle() {
    let reg = MemRegistry::with_manual_time();
    let client = reg.client(SERVICE, 0, 1);
    client.run(Box::new(|| panic!("boom")));
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reg.settle()));
    let msg = r.unwrap_err().downcast::<String>().unwrap();
    assert!(msg.contains("boom"), "{msg}");
    // The executor survives.
    let (tx, rx) = mpsc::channel();
    client.run(Box::new(move || tx.send(1).unwrap()));
    reg.settle();
    assert_eq!(rx.try_recv().unwrap(), 1);
}
