//! An in-process transport with XPC's semantics, for tests on every OS.
//!
//! A [`MemRegistry`] plays launchd: it knows service names, the
//! [`MemServer`] currently serving each, and every [`MemClient`]. Each server
//! and client has its own serial executor thread, which runs all of its
//! callbacks, like the XPC shim's dispatch queue. Every message goes through
//! the codec ([`crate::protocol`]) on its way, so the tests exercise it too.
//!
//! What clients see, as with XPC and a launchd daemon:
//!
//! * Sending to a name that was never bound: `on_invalid`, and requests fail
//!   with `Invalid` (the service is not installed).
//! * Sending to a bound name whose server is not running (not started, or
//!   dropped): the message is lost and a request times out (the daemon is
//!   between instances).
//! * Dropping a [`MemServer`] (the daemon died): every connected client gets
//!   `on_interrupted`, and its pending requests fail with `Interrupted`. The
//!   next message reaches the next server bound to the name.
//! * A server refusing a peer in `on_peer` interrupts that peer.
//!
//! With [`MemRegistry::with_manual_time`] time stands still until
//! [`MemRegistry::advance`] moves it, firing due timers (`after`, request
//! timeouts) one at a time in deadline order and letting every executor go
//! idle in between, so timer-driven code runs deterministically.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::thread;
use std::time::{Duration, Instant};

use crate::protocol::{
    Kv, ToDaemon, ToPlugin, decode_to_daemon, decode_to_plugin, encode_to_daemon, encode_to_plugin,
};
use crate::transport::{
    ClientHandler, ClientTransport, PeerInfo, ReplyFn, ServerHandler, ServerTransport,
    TransportError,
};

type Task = Box<dyn FnOnce() + Send>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

thread_local! {
    /// The registry (its `Shared` address) whose executor runs on this
    /// thread, 0 if none.
    static EXECUTOR_OF: Cell<usize> = const { Cell::new(0) };
}

// --- Time, activity and executors ----------------------------------------

enum Clock {
    Real(Instant),
    Manual(Mutex<Duration>),
}

impl Clock {
    fn now(&self) -> Duration {
        match self {
            Clock::Real(start) => start.elapsed(),
            Clock::Manual(now) => *lock(now),
        }
    }
}

/// Counts queued and running tasks across a registry's executors, so that
/// tests can wait for every executor to be idle.
struct Activity {
    state: Mutex<ActivityState>,
    idle: Condvar,
}

struct ActivityState {
    busy: usize,
    /// The first callback panic not yet reported.
    panic: Option<String>,
}

impl Activity {
    fn begin(&self) {
        lock(&self.state).busy += 1;
    }

    fn end(&self, panic: Option<String>) {
        let mut s = lock(&self.state);
        s.busy -= 1;
        if s.panic.is_none() {
            s.panic = panic;
        }
        if s.busy == 0 {
            self.idle.notify_all();
        }
    }

    /// Waits until nothing is queued or running; returns a callback panic
    /// that happened meanwhile.
    fn wait_idle(&self) -> Option<String> {
        let mut s = lock(&self.state);
        while s.busy > 0 {
            s = self.idle.wait(s).unwrap_or_else(PoisonError::into_inner);
        }
        s.panic.take()
    }
}

/// What a registry's executors share.
struct Shared {
    clock: Clock,
    activity: Activity,
    /// Orders timers with equal deadlines by creation.
    timer_seq: AtomicU64,
}

/// A serial executor: one thread running tasks in order, plus timers.
struct Executor {
    shared: Arc<Shared>,
    state: Mutex<ExecState>,
    wake: Condvar,
}

/// A timer's place in line: (deadline, creation order).
type TimerKey = (Duration, u64);

struct ExecState {
    queue: VecDeque<Task>,
    timers: BTreeMap<TimerKey, Task>,
    stopped: bool,
}

impl Executor {
    fn spawn(name: &str, shared: &Arc<Shared>) -> Arc<Executor> {
        let ex = Arc::new(Executor {
            shared: shared.clone(),
            state: Mutex::new(ExecState {
                queue: VecDeque::new(),
                timers: BTreeMap::new(),
                stopped: false,
            }),
            wake: Condvar::new(),
        });
        let worker = ex.clone();
        thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || worker.work())
            .expect("cannot start a mem transport thread");
        ex
    }

    fn work(self: Arc<Self>) {
        EXECUTOR_OF.with(|e| e.set(Arc::as_ptr(&self.shared) as usize));
        loop {
            let task = {
                let mut st = lock(&self.state);
                loop {
                    if st.stopped {
                        return;
                    }
                    if let Some(t) = st.queue.pop_front() {
                        break t;
                    }
                    st = match self.shared.clock {
                        Clock::Real(_) => {
                            let now = self.shared.clock.now();
                            if self.move_due(&mut st, now) {
                                continue;
                            }
                            match st.timers.keys().next() {
                                Some(&(deadline, _)) => {
                                    self.wake
                                        .wait_timeout(st, deadline.saturating_sub(now))
                                        .unwrap_or_else(PoisonError::into_inner)
                                        .0
                                }
                                None => self.wake.wait(st).unwrap_or_else(PoisonError::into_inner),
                            }
                        }
                        // Timers fire only through MemRegistry::advance.
                        Clock::Manual(_) => {
                            self.wake.wait(st).unwrap_or_else(PoisonError::into_inner)
                        }
                    };
                }
            };
            let panic = catch_unwind(AssertUnwindSafe(task)).err().map(|p| {
                p.downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| p.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "a callback panicked".to_owned())
            });
            self.shared.activity.end(panic);
        }
    }

    /// Queues every timer due at `now`, in order. Returns whether any was.
    fn move_due(&self, st: &mut ExecState, now: Duration) -> bool {
        let mut moved = false;
        while let Some(entry) = st.timers.first_entry() {
            if entry.key().0 > now {
                break;
            }
            st.queue.push_back(entry.remove());
            self.shared.activity.begin();
            moved = true;
        }
        moved
    }

    fn post(&self, task: Task) {
        let mut st = lock(&self.state);
        if st.stopped {
            drop(st);
            drop(task);
            return;
        }
        st.queue.push_back(task);
        self.shared.activity.begin();
        self.wake.notify_one();
    }

    fn post_after(&self, delay: Duration, task: Task) {
        let deadline = self.shared.clock.now().saturating_add(delay);
        let seq = self.shared.timer_seq.fetch_add(1, Ordering::Relaxed);
        let mut st = lock(&self.state);
        if st.stopped {
            drop(st);
            drop(task);
            return;
        }
        st.timers.insert((deadline, seq), task);
        self.wake.notify_one();
    }

    /// The earliest pending timer.
    fn earliest(&self) -> Option<TimerKey> {
        lock(&self.state).timers.keys().next().copied()
    }

    /// Queues one timer now (manual time).
    fn fire(&self, key: TimerKey) {
        let mut st = lock(&self.state);
        if let Some(t) = st.timers.remove(&key) {
            st.queue.push_back(t);
            self.shared.activity.begin();
            self.wake.notify_one();
        }
    }

    /// Ends the thread after the running task; queued tasks and timers are
    /// dropped without running.
    fn stop(&self) {
        let (queue, timers) = {
            let mut st = lock(&self.state);
            st.stopped = true;
            (std::mem::take(&mut st.queue), std::mem::take(&mut st.timers))
        };
        self.wake.notify_all();
        for _ in 0..queue.len() {
            self.shared.activity.end(None);
        }
        drop(queue);
        drop(timers);
    }
}

// --- Registry --------------------------------------------------------------

/// The in-process service namespace and the clock of its transports.
pub struct MemRegistry {
    shared: Arc<Shared>,
    state: Mutex<RegistryState>,
}

struct RegistryState {
    /// Every bound name and the server serving it (dangling when none).
    services: HashMap<String, Weak<ServerCore>>,
    executors: Vec<Weak<Executor>>,
    next_peer: u64,
}

enum Lookup {
    Unknown,
    NoServer,
    Server(Arc<ServerCore>),
}

impl MemRegistry {
    /// A registry on real (monotonic) time.
    pub fn new() -> Arc<Self> {
        Self::with_clock(Clock::Real(Instant::now()))
    }

    /// A registry whose time starts at 0 and moves only through
    /// [`MemRegistry::advance`].
    pub fn with_manual_time() -> Arc<Self> {
        Self::with_clock(Clock::Manual(Mutex::new(Duration::ZERO)))
    }

    fn with_clock(clock: Clock) -> Arc<Self> {
        Arc::new(MemRegistry {
            shared: Arc::new(Shared {
                clock,
                activity: Activity {
                    state: Mutex::new(ActivityState { busy: 0, panic: None }),
                    idle: Condvar::new(),
                },
                timer_seq: AtomicU64::new(0),
            }),
            state: Mutex::new(RegistryState {
                services: HashMap::new(),
                executors: Vec::new(),
                next_peer: 1,
            }),
        })
    }

    /// The transports' time: since creation, or the manual clock.
    pub fn now(&self) -> Duration {
        self.shared.clock.now()
    }

    /// Moves manual time forward by `d`. Every timer that falls due on the
    /// way fires at its deadline, one at a time in deadline order (timers
    /// created on the way included), and every executor runs idle after
    /// each. Returns with every executor idle.
    ///
    /// Panics on a real-time registry, when called on one of the registry's
    /// executor threads (it would wait for itself), or when a callback
    /// panicked.
    pub fn advance(&self, d: Duration) {
        let Clock::Manual(now) = &self.shared.clock else {
            panic!("MemRegistry::advance needs a registry made with with_manual_time");
        };
        let target = lock(now).saturating_add(d);
        loop {
            self.settle();
            let next = self
                .executors()
                .into_iter()
                .filter_map(|e| e.earliest().map(|k| (k, e)))
                .min_by_key(|(k, _)| *k);
            match next {
                Some((key, ex)) if key.0 <= target => {
                    let mut t = lock(now);
                    if key.0 > *t {
                        *t = key.0;
                    }
                    drop(t);
                    ex.fire(key);
                }
                _ => break,
            }
        }
        *lock(now) = target;
        self.settle();
    }

    /// Waits until no executor of this registry has work queued or running
    /// (pending timers do not count).
    ///
    /// Panics when called on one of the registry's executor threads, or when
    /// a callback panicked (with that panic's message), so test failures
    /// inside callbacks surface in the test.
    pub fn settle(&self) {
        let me = Arc::as_ptr(&self.shared) as usize;
        assert!(
            EXECUTOR_OF.with(Cell::get) != me,
            "MemRegistry::settle or advance called from a transport callback"
        );
        if let Some(msg) = self.shared.activity.wait_idle() {
            panic!("a mem transport callback panicked: {msg}");
        }
    }

    /// Binds a server to `service`. The name is known from now on; the
    /// server serves it once started. Binding a name again makes the newer
    /// server serve it once started; the older one keeps its peers until it
    /// is dropped.
    pub fn bind(self: &Arc<Self>, service: &str) -> MemServer {
        lock(&self.state).services.entry(service.to_owned()).or_default();
        let core = Arc::new(ServerCore {
            registry: self.clone(),
            service: service.to_owned(),
            exec: self.executor("odipc-mem-server"),
            handler: Mutex::new(None),
            peers: Mutex::new(HashMap::new()),
            alive: AtomicBool::new(true),
        });
        MemServer { core }
    }

    /// A client of `service`, connecting as `euid` and `pid`.
    pub fn client(self: &Arc<Self>, service: &str, euid: u32, pid: i32) -> MemClient {
        MemClient {
            inner: Arc::new(ClientInner {
                registry: self.clone(),
                service: service.to_owned(),
                euid,
                pid,
                exec: self.executor("odipc-mem-client"),
                conn: Mutex::new(None),
            }),
        }
    }

    fn executor(&self, name: &str) -> Arc<Executor> {
        let ex = Executor::spawn(name, &self.shared);
        let mut st = lock(&self.state);
        st.executors.retain(|w| w.strong_count() > 0);
        st.executors.push(Arc::downgrade(&ex));
        ex
    }

    fn executors(&self) -> Vec<Arc<Executor>> {
        lock(&self.state).executors.iter().filter_map(Weak::upgrade).collect()
    }

    fn lookup(&self, service: &str) -> Lookup {
        match lock(&self.state).services.get(service) {
            None => Lookup::Unknown,
            Some(w) => match w.upgrade().filter(|s| s.alive.load(Ordering::Acquire)) {
                Some(s) => Lookup::Server(s),
                None => Lookup::NoServer,
            },
        }
    }

    fn next_peer(&self) -> u64 {
        let mut st = lock(&self.state);
        let id = st.next_peer;
        st.next_peer += 1;
        id
    }

    fn activate(&self, core: &Arc<ServerCore>) {
        lock(&self.state).services.insert(core.service.clone(), Arc::downgrade(core));
    }

    fn deactivate(&self, core: &Arc<ServerCore>) {
        if let Some(w) = lock(&self.state).services.get_mut(&core.service) {
            if std::ptr::eq(w.as_ptr(), Arc::as_ptr(core)) {
                *w = Weak::new();
            }
        }
    }
}

// --- Server ----------------------------------------------------------------

/// A server bound to a service name. Dropping it is the daemon dying.
pub struct MemServer {
    core: Arc<ServerCore>,
}

struct ServerCore {
    registry: Arc<MemRegistry>,
    service: String,
    exec: Arc<Executor>,
    handler: Mutex<Option<Arc<dyn ServerHandler>>>,
    peers: Mutex<HashMap<u64, Peer>>,
    alive: AtomicBool,
}

struct Peer {
    conn: Weak<Conn>,
    /// Set once `on_peer` accepted it; messages before that are not
    /// possible (the executor is serial) and after a refusal are dropped.
    accepted: bool,
}

impl ServerCore {
    fn handler(&self) -> Option<Arc<dyn ServerHandler>> {
        lock(&self.handler).clone()
    }

    /// A client connection reached this server (client thread).
    fn attach(self: &Arc<Self>, info: PeerInfo, conn: &Arc<Conn>) {
        lock(&self.peers).insert(info.id, Peer { conn: Arc::downgrade(conn), accepted: false });
        let core = self.clone();
        self.exec.post(Box::new(move || core.admit(info)));
    }

    /// Asks the handler about a new peer (server executor).
    fn admit(&self, info: PeerInfo) {
        if !lock(&self.peers).contains_key(&info.id) {
            return;
        }
        let ok = self.handler().is_some_and(|h| h.on_peer(info));
        let refused = {
            let mut peers = lock(&self.peers);
            if ok {
                if let Some(p) = peers.get_mut(&info.id) {
                    p.accepted = true;
                }
                None
            } else {
                peers.remove(&info.id).and_then(|p| p.conn.upgrade())
            }
        };
        if let Some(conn) = refused {
            conn.interrupt(info.id);
        }
    }

    /// Queues a message from `peer` (client thread).
    fn dispatch(self: &Arc<Self>, peer: u64, kv: Kv, request: Option<(u64, Weak<Conn>)>) {
        let core = self.clone();
        self.exec.post(Box::new(move || core.handle(peer, kv, request)));
    }

    /// Delivers a message to the handler and sends any reply (server
    /// executor).
    fn handle(&self, peer: u64, kv: Kv, request: Option<(u64, Weak<Conn>)>) {
        if !lock(&self.peers).get(&peer).is_some_and(|p| p.accepted) {
            return;
        }
        // Undecodable messages and unknown ops are ignored.
        let Ok(m) = decode_to_daemon(&kv) else { return };
        let Some(h) = self.handler() else { return };
        match request {
            Some((id, conn)) => {
                if let Some(reply) = h.on_request(peer, m) {
                    if let Some(conn) = conn.upgrade() {
                        conn.reply(id, encode_to_plugin(&reply));
                    }
                }
            }
            None => h.on_message(peer, m),
        }
    }

    /// The client side of `peer` went away (any thread).
    fn peer_gone(self: &Arc<Self>, peer: u64) {
        let core = self.clone();
        self.exec.post(Box::new(move || {
            let accepted = lock(&core.peers).remove(&peer).is_some_and(|p| p.accepted);
            if accepted {
                if let Some(h) = core.handler() {
                    h.on_peer_gone(peer);
                }
            }
        }));
    }
}

impl ServerTransport for MemServer {
    fn start(&self, h: Arc<dyn ServerHandler>) -> io::Result<()> {
        {
            let mut slot = lock(&self.core.handler);
            if slot.is_some() {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "server already started"));
            }
            *slot = Some(h);
        }
        self.core.registry.activate(&self.core);
        Ok(())
    }

    fn send(&self, peer: u64, m: ToPlugin) {
        let conn =
            lock(&self.core.peers).get(&peer).filter(|p| p.accepted).and_then(|p| p.conn.upgrade());
        if let Some(conn) = conn {
            conn.push(peer, encode_to_plugin(&m));
        }
    }
}

impl Drop for MemServer {
    fn drop(&mut self) {
        let core = &self.core;
        core.alive.store(false, Ordering::Release);
        core.registry.deactivate(core);
        core.exec.stop();
        let handler = lock(&core.handler).take();
        let peers: Vec<(u64, Peer)> = lock(&core.peers).drain().collect();
        for (id, p) in peers {
            if let Some(conn) = p.conn.upgrade() {
                conn.interrupt(id);
            }
        }
        drop(handler);
    }
}

// --- Client ----------------------------------------------------------------

/// A client of a service name.
pub struct MemClient {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    registry: Arc<MemRegistry>,
    service: String,
    euid: u32,
    pid: i32,
    exec: Arc<Executor>,
    conn: Mutex<Option<Arc<Conn>>>,
}

/// One connection, from `connect` to its cancellation.
struct Conn {
    registry: Arc<MemRegistry>,
    service: String,
    euid: u32,
    pid: i32,
    exec: Arc<Executor>,
    state: Mutex<ConnState>,
}

struct ConnState {
    /// `None` once cancelled.
    handler: Option<Arc<dyn ClientHandler>>,
    link: Link,
    pending: HashMap<u64, Pending>,
    next_request: u64,
}

enum Link {
    /// Not attached to a server: never used, or interrupted.
    Idle,
    Attached {
        server: Weak<ServerCore>,
        peer: u64,
    },
    /// The service does not exist; dead until the next `connect`.
    Invalid,
    Cancelled,
}

struct Pending {
    /// The peer the request went to; `None` if it was lost.
    peer: Option<u64>,
    reply: ReplyFn,
}

impl Conn {
    /// The handler, unless the connection was cancelled.
    fn live_handler(&self) -> Option<Arc<dyn ClientHandler>> {
        lock(&self.state).handler.clone()
    }

    /// Sends `kv`, as a request if `reply` is set (client thread).
    fn deliver(self: &Arc<Self>, kv: Kv, reply: Option<(ReplyFn, Duration)>) {
        let mut st = lock(&self.state);
        let request = reply.map(|(f, timeout)| {
            let id = st.next_request;
            st.next_request += 1;
            st.pending.insert(id, Pending { peer: None, reply: f });
            let conn = Arc::downgrade(self);
            self.exec.post_after(
                timeout,
                Box::new(move || {
                    if let Some(c) = conn.upgrade() {
                        c.complete(id, Err(TransportError::Timeout));
                    }
                }),
            );
            id
        });
        loop {
            match &st.link {
                Link::Cancelled | Link::Invalid => {
                    let failed = request.and_then(|id| st.pending.remove(&id));
                    drop(st);
                    if let Some(p) = failed {
                        self.exec.post(Box::new(move || (p.reply)(Err(TransportError::Invalid))));
                    }
                    return;
                }
                Link::Attached { server, peer } => {
                    let peer = *peer;
                    match server.upgrade().filter(|s| s.alive.load(Ordering::Acquire)) {
                        Some(server) => {
                            if let Some(p) = request.and_then(|id| st.pending.get_mut(&id)) {
                                p.peer = Some(peer);
                            }
                            drop(st);
                            server.dispatch(peer, kv, request.map(|id| (id, Arc::downgrade(self))));
                            return;
                        }
                        // The server died and its interruption has not
                        // arrived yet: deliver it now, before this message
                        // goes to the next server.
                        None => {
                            let failed = Self::detach(&mut st, peer);
                            self.post_interrupted(failed);
                        }
                    }
                }
                Link::Idle => match self.registry.lookup(&self.service) {
                    Lookup::Unknown => {
                        st.link = Link::Invalid;
                        let failed: Vec<ReplyFn> =
                            st.pending.drain().map(|(_, p)| p.reply).collect();
                        drop(st);
                        let conn = self.clone();
                        self.exec.post(Box::new(move || {
                            for f in failed {
                                f(Err(TransportError::Invalid));
                            }
                            if let Some(h) = conn.live_handler() {
                                h.on_invalid();
                            }
                        }));
                        return;
                    }
                    // Lost: no daemon is running. A request times out.
                    Lookup::NoServer => return,
                    Lookup::Server(server) => {
                        let peer = self.registry.next_peer();
                        st.link = Link::Attached { server: Arc::downgrade(&server), peer };
                        server.attach(PeerInfo { id: peer, euid: self.euid, pid: self.pid }, self);
                    }
                },
            }
        }
    }

    /// The server side of `peer` went away (any thread).
    fn interrupt(self: &Arc<Self>, peer: u64) {
        let failed = {
            let mut st = lock(&self.state);
            if !matches!(st.link, Link::Attached { peer: p, .. } if p == peer) {
                return;
            }
            Self::detach(&mut st, peer)
        };
        self.post_interrupted(failed);
    }

    /// Detaches from `peer`; returns the requests that went to it.
    fn detach(st: &mut ConnState, peer: u64) -> Vec<ReplyFn> {
        st.link = Link::Idle;
        let ids: Vec<u64> =
            st.pending.iter().filter(|(_, p)| p.peer == Some(peer)).map(|(id, _)| *id).collect();
        ids.into_iter().filter_map(|id| st.pending.remove(&id)).map(|p| p.reply).collect()
    }

    /// Fails `failed` with `Interrupted`, then tells the handler.
    fn post_interrupted(self: &Arc<Self>, failed: Vec<ReplyFn>) {
        let conn = self.clone();
        self.exec.post(Box::new(move || {
            for f in failed {
                f(Err(TransportError::Interrupted));
            }
            if let Some(h) = conn.live_handler() {
                h.on_interrupted();
            }
        }));
    }

    /// The reply to request `id` (server executor).
    fn reply(&self, id: u64, kv: Kv) {
        let pending = lock(&self.state).pending.remove(&id);
        if let Some(p) = pending {
            self.exec.post(Box::new(move || {
                (p.reply)(decode_to_plugin(kv).map_err(TransportError::Decode));
            }));
        }
    }

    /// An unsolicited message from `peer` (any thread).
    fn push(self: &Arc<Self>, peer: u64, kv: Kv) {
        let conn = self.clone();
        self.exec.post(Box::new(move || {
            let handler = {
                let st = lock(&conn.state);
                match st.link {
                    Link::Attached { peer: p, .. } if p == peer => st.handler.clone(),
                    _ => None,
                }
            };
            // Undecodable messages and unknown ops are ignored.
            if let (Some(h), Ok(m)) = (handler, decode_to_plugin(kv)) {
                h.on_message(m);
            }
        }));
    }

    /// Ends request `id` with `result` unless it already ended (client
    /// executor).
    fn complete(&self, id: u64, result: Result<ToPlugin, TransportError>) {
        let pending = lock(&self.state).pending.remove(&id);
        if let Some(p) = pending {
            (p.reply)(result);
        }
    }

    /// Cancels the connection (any thread).
    fn cancel(&self) {
        let (link, failed, handler) = {
            let mut st = lock(&self.state);
            let link = std::mem::replace(&mut st.link, Link::Cancelled);
            let failed: Vec<ReplyFn> = st.pending.drain().map(|(_, p)| p.reply).collect();
            (link, failed, st.handler.take())
        };
        // Dropped outside the lock: the handler's drop may call back in.
        drop(handler);
        if !failed.is_empty() {
            self.exec.post(Box::new(move || {
                for f in failed {
                    f(Err(TransportError::Invalid));
                }
            }));
        }
        if let Link::Attached { server, peer } = link {
            if let Some(server) = server.upgrade() {
                server.peer_gone(peer);
            }
        }
    }
}

impl MemClient {
    fn current(&self) -> Option<Arc<Conn>> {
        lock(&self.inner.conn).clone()
    }
}

impl ClientTransport for MemClient {
    fn connect(&self, h: Arc<dyn ClientHandler>) {
        let i = &self.inner;
        let conn = Arc::new(Conn {
            registry: i.registry.clone(),
            service: i.service.clone(),
            euid: i.euid,
            pid: i.pid,
            exec: i.exec.clone(),
            state: Mutex::new(ConnState {
                handler: Some(h),
                link: Link::Idle,
                pending: HashMap::new(),
                next_request: 0,
            }),
        });
        let old = lock(&i.conn).replace(conn);
        if let Some(old) = old {
            old.cancel();
        }
    }

    fn cancel(&self) {
        let old = lock(&self.inner.conn).take();
        if let Some(old) = old {
            old.cancel();
        }
    }

    fn send(&self, m: ToDaemon) {
        if let Some(conn) = self.current() {
            conn.deliver(encode_to_daemon(&m), None);
        }
    }

    fn request(
        &self,
        m: ToDaemon,
        timeout: Duration,
        reply: Box<dyn FnOnce(Result<ToPlugin, TransportError>) + Send>,
    ) {
        match self.current() {
            Some(conn) => conn.deliver(encode_to_daemon(&m), Some((reply, timeout))),
            None => self.inner.exec.post(Box::new(move || reply(Err(TransportError::Invalid)))),
        }
    }

    fn after(&self, delay: Duration, f: Box<dyn FnOnce() + Send>) {
        self.inner.exec.post_after(delay, f);
    }

    fn run(&self, f: Box<dyn FnOnce() + Send>) {
        self.inner.exec.post(f);
    }
}

impl Drop for MemClient {
    fn drop(&mut self) {
        self.cancel();
        self.inner.exec.stop();
    }
}
