//! The XPC transports (macOS): [`XpcClient`] for the driver, [`XpcServer`]
//! for the daemon, over the C shim in `c/ovshim.c`.
//!
//! Every callback runs on the shim's serial queue, `org.openvirtualsoundcard.ipc`; so
//! do [`XpcClient::after`](ClientTransport::after) and
//! [`XpcClient::run`](ClientTransport::run). Messages are XPC dictionaries
//! converted from and to [`Kv`]: `U64` is a uint64, `I64` an int64, `Bool` a
//! bool, `Str` a string, `StrList` an array of strings, `Dict` a dictionary
//! and `Region` an `xpc_shmem` (sent under the key `region`).
//!
//! Context lifetimes: each connection holds one reference to its Rust
//! context, set with `xpc_connection_set_context` and dropped by the
//! connection's finalizer, which XPC runs after the connection's last event.
//! Replies and timers hold their own references and drop them when they run,
//! which XPC and dispatch guarantee happens exactly once.
//!
//! Unanswered requests: XPC tells a client that a reply object was released
//! unsent by interrupting its request, but the transport contract is that
//! the client times out. So the server keeps the reply objects of requests
//! its handler leaves unanswered, up to [`HELD_REPLIES`] per peer (the oldest
//! goes first, long after its client gave up), until the peer goes away.

#![allow(non_camel_case_types)]

use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::log::{Level, log};
use crate::protocol::{
    Kv, ToDaemon, ToPlugin, Value, decode_to_daemon, decode_to_plugin, encode_to_daemon,
    encode_to_plugin,
};
use crate::region::{RegionHandle, XpcShmem};
use crate::transport::{
    ClientHandler, ClientTransport, PeerInfo, ReplyFn, ServerHandler, ServerTransport,
    TransportError,
};

type xpc_object_t = *mut c_void;
type xpc_connection_t = *mut c_void;

type CEventFn = unsafe extern "C" fn(ctx: *mut c_void, conn: xpc_connection_t, event: xpc_object_t);
type CReplyFn = unsafe extern "C" fn(ctx: *mut c_void, reply: xpc_object_t);
type CWorkFn = unsafe extern "C" fn(ctx: *mut c_void);
type CApplyFn =
    unsafe extern "C" fn(ctx: *mut c_void, key: *const c_char, value: xpc_object_t) -> bool;
type CFinalizerFn = unsafe extern "C" fn(ctx: *mut c_void);

// ovshim_kind results (ovshim.h).
const KIND_DICTIONARY: c_int = 1;
const KIND_INTERRUPTED: c_int = 2;
const KIND_INVALID: c_int = 3;
const KIND_SHMEM: c_int = 6;
const KIND_ARRAY: c_int = 7;
const KIND_STRING: c_int = 8;
const KIND_CONNECTION: c_int = 9;
const KIND_UINT64: c_int = 10;
const KIND_INT64: c_int = 11;
const KIND_BOOL: c_int = 12;

/// `XPC_ARRAY_APPEND`.
const ARRAY_APPEND: usize = usize::MAX;

/// How deeply received dictionaries may nest (ours nest once).
const MAX_DEPTH: u32 = 4;

unsafe extern "C" {
    fn ovshim_kind(object: xpc_object_t) -> c_int;
    fn ovshim_connect(
        service: *const c_char,
        flags: u64,
        f: CEventFn,
        ctx: *mut c_void,
    ) -> xpc_connection_t;
    fn ovshim_listen(service: *const c_char, f: CEventFn, ctx: *mut c_void) -> xpc_connection_t;
    fn ovshim_listen_anonymous(
        f: CEventFn,
        ctx: *mut c_void,
        out_endpoint: *mut xpc_object_t,
    ) -> xpc_connection_t;
    fn ovshim_connect_endpoint(
        endpoint: xpc_object_t,
        f: CEventFn,
        ctx: *mut c_void,
    ) -> xpc_connection_t;
    fn ovshim_send_with_reply(
        conn: xpc_connection_t,
        msg: xpc_object_t,
        f: CReplyFn,
        ctx: *mut c_void,
    );
    fn ovshim_async(f: CWorkFn, ctx: *mut c_void);
    fn ovshim_after(delay_ns: u64, f: CWorkFn, ctx: *mut c_void);
    fn ovshim_dict_apply(dict: xpc_object_t, f: CApplyFn, ctx: *mut c_void) -> bool;

    fn xpc_dictionary_create(
        keys: *const *const c_char,
        values: *const xpc_object_t,
        count: usize,
    ) -> xpc_object_t;
    fn xpc_dictionary_create_reply(original: xpc_object_t) -> xpc_object_t;
    fn xpc_dictionary_set_uint64(d: xpc_object_t, key: *const c_char, value: u64);
    fn xpc_dictionary_set_int64(d: xpc_object_t, key: *const c_char, value: i64);
    fn xpc_dictionary_set_bool(d: xpc_object_t, key: *const c_char, value: bool);
    fn xpc_dictionary_set_string(d: xpc_object_t, key: *const c_char, value: *const c_char);
    fn xpc_dictionary_set_value(d: xpc_object_t, key: *const c_char, value: xpc_object_t);
    fn xpc_array_create(objects: *const xpc_object_t, count: usize) -> xpc_object_t;
    fn xpc_array_set_string(a: xpc_object_t, index: usize, value: *const c_char);
    fn xpc_array_get_count(a: xpc_object_t) -> usize;
    fn xpc_array_get_value(a: xpc_object_t, index: usize) -> xpc_object_t;
    fn xpc_uint64_get_value(o: xpc_object_t) -> u64;
    fn xpc_int64_get_value(o: xpc_object_t) -> i64;
    fn xpc_bool_get_value(o: xpc_object_t) -> bool;
    fn xpc_string_get_string_ptr(o: xpc_object_t) -> *const c_char;
    fn xpc_connection_send_message(conn: xpc_connection_t, msg: xpc_object_t);
    fn xpc_connection_cancel(conn: xpc_connection_t);
    fn xpc_connection_get_euid(conn: xpc_connection_t) -> libc::uid_t;
    fn xpc_connection_get_pid(conn: xpc_connection_t) -> libc::pid_t;
    fn xpc_connection_set_context(conn: xpc_connection_t, ctx: *mut c_void);
    fn xpc_connection_set_finalizer_f(conn: xpc_connection_t, f: Option<CFinalizerFn>);
    fn xpc_retain(o: xpc_object_t) -> xpc_object_t;
    fn xpc_release(o: xpc_object_t);
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `s` up to its first NUL, as a C string (XPC strings cannot hold NULs).
fn c_text(s: &str) -> CString {
    let end = s.find('\0').unwrap_or(s.len());
    CString::new(&s[..end]).unwrap_or_default()
}

fn duration_ns(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// Runs `f`, logging instead of unwinding into C (which would abort).
fn guarded(what: &str, f: impl FnOnce()) {
    if catch_unwind(AssertUnwindSafe(f)).is_err() {
        log(Level::Fault, &format!("panic in an XPC {what} callback"));
    }
}

/// An XPC object reference owned by Rust.
struct Owned(xpc_object_t);

// SAFETY: XPC objects are reference-counted thread-safely, and every
// connection call used here is thread-safe.
unsafe impl Send for Owned {}
// SAFETY: as above.
unsafe impl Sync for Owned {}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: releases the reference `self` owns.
        unsafe { xpc_release(self.0) };
    }
}

/// Gives `conn` one reference to `ctx`, dropped by its finalizer.
///
/// # Safety
/// `conn` must be a live connection whose context is not yet set.
unsafe fn give_context<T: Send + Sync>(conn: xpc_connection_t, ctx: &Arc<T>) {
    unsafe extern "C" fn release<T>(p: *mut c_void) {
        // SAFETY: `p` is the reference handed over below; the finalizer runs
        // once.
        drop(unsafe { Arc::from_raw(p.cast_const().cast::<T>()) });
    }
    // SAFETY: the caller guarantees a live connection.
    unsafe {
        xpc_connection_set_context(conn, Arc::into_raw(ctx.clone()).cast_mut().cast());
        xpc_connection_set_finalizer_f(conn, Some(release::<T>));
    }
}

// --- Kv <-> XPC dictionaries ----------------------------------------------

/// A new XPC dictionary (owned) holding `kv`.
fn kv_to_dict(kv: &Kv) -> Owned {
    // SAFETY: creating an empty dictionary has no preconditions.
    let d = Owned(unsafe { xpc_dictionary_create(ptr::null(), ptr::null(), 0) });
    fill_dict(d.0, kv);
    d
}

/// Adds every entry of `kv` to the dictionary `d`.
fn fill_dict(d: xpc_object_t, kv: &Kv) {
    for (k, v) in kv {
        let key_text = c_text(k);
        let key = key_text.as_ptr();
        // SAFETY: `d` is a live dictionary and every string is
        // NUL-terminated; XPC copies or retains what it stores.
        unsafe {
            match v {
                Value::U64(x) => xpc_dictionary_set_uint64(d, key, *x),
                Value::I64(x) => xpc_dictionary_set_int64(d, key, *x),
                Value::Bool(x) => xpc_dictionary_set_bool(d, key, *x),
                Value::Str(s) => xpc_dictionary_set_string(d, key, c_text(s).as_ptr()),
                Value::StrList(list) => {
                    let a = Owned(xpc_array_create(ptr::null(), 0));
                    for s in list {
                        xpc_array_set_string(a.0, ARRAY_APPEND, c_text(s).as_ptr());
                    }
                    xpc_dictionary_set_value(d, key, a.0);
                }
                Value::Dict(sub) => xpc_dictionary_set_value(d, key, kv_to_dict(sub).0),
                Value::Region(RegionHandle::Xpc(shmem)) => {
                    xpc_dictionary_set_value(d, key, shmem.as_raw());
                }
                Value::Region(RegionHandle::Local(_)) => {
                    log(Level::Error, &format!("{k}: an in-process region cannot travel over XPC"));
                }
            }
        }
    }
}

/// The entries of a received dictionary. Entries of types the protocol
/// does not use are left out (the decoder then reports them missing).
///
/// # Safety
/// `d` must be a live XPC dictionary.
unsafe fn dict_to_kv(d: xpc_object_t, depth: u32) -> Kv {
    struct Walk {
        kv: Kv,
        depth: u32,
    }
    unsafe extern "C" fn entry(ctx: *mut c_void, key: *const c_char, value: xpc_object_t) -> bool {
        catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: `ctx` is the Walk below, borrowed for the walk; XPC
            // passes a NUL-terminated key and a live value.
            let walk = unsafe { &mut *ctx.cast::<Walk>() };
            let key = unsafe { CStr::from_ptr(key) }.to_string_lossy().into_owned();
            if let Some(v) = unsafe { to_value(value, walk.depth) } {
                walk.kv.insert(key, v);
            }
        }))
        .is_ok()
    }
    let mut walk = Walk { kv: Kv::new(), depth };
    // SAFETY: `walk` outlives the synchronous walk.
    unsafe { ovshim_dict_apply(d, entry, (&mut walk as *mut Walk).cast()) };
    walk.kv
}

/// # Safety
/// `s` must be a live XPC string.
unsafe fn string_of(s: xpc_object_t) -> Option<String> {
    let p = unsafe { xpc_string_get_string_ptr(s) };
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

/// # Safety
/// `o` must be a live XPC object.
unsafe fn to_value(o: xpc_object_t, depth: u32) -> Option<Value> {
    // SAFETY (whole body): `o` is live and each accessor matches its kind.
    unsafe {
        match ovshim_kind(o) {
            KIND_UINT64 => Some(Value::U64(xpc_uint64_get_value(o))),
            KIND_INT64 => Some(Value::I64(xpc_int64_get_value(o))),
            KIND_BOOL => Some(Value::Bool(xpc_bool_get_value(o))),
            KIND_STRING => string_of(o).map(Value::Str),
            KIND_ARRAY => {
                let n = xpc_array_get_count(o);
                let mut list = Vec::with_capacity(n.min(256));
                for i in 0..n {
                    let e = xpc_array_get_value(o, i);
                    if ovshim_kind(e) != KIND_STRING {
                        return None;
                    }
                    list.push(string_of(e)?);
                }
                Some(Value::StrList(list))
            }
            KIND_DICTIONARY if depth < MAX_DEPTH => Some(Value::Dict(dict_to_kv(o, depth + 1))),
            KIND_SHMEM => {
                NonNull::new(o).map(|p| Value::Region(RegionHandle::Xpc(XpcShmem::retain(p))))
            }
            _ => None,
        }
    }
}

// --- Queue work --------------------------------------------------------------

type Work = Box<dyn FnOnce() + Send>;

unsafe extern "C" fn run_work(ctx: *mut c_void) {
    // SAFETY: `ctx` is the box leaked by queue_run or queue_after; dispatch
    // runs each work item once.
    let f = unsafe { Box::from_raw(ctx.cast::<Work>()) };
    guarded("work", *f);
}

/// Runs `f` on the IPC queue.
pub fn queue_run(f: Box<dyn FnOnce() + Send>) {
    let ctx = Box::into_raw(Box::new(f));
    // SAFETY: run_work takes the box back.
    unsafe { ovshim_async(run_work, ctx.cast()) };
}

/// Runs `f` on the IPC queue after `delay` (capped at a year).
pub fn queue_after(delay: Duration, f: Box<dyn FnOnce() + Send>) {
    let ctx = Box::into_raw(Box::new(f));
    // SAFETY: run_work takes the box back.
    unsafe { ovshim_after(duration_ns(delay), run_work, ctx.cast()) };
}

// --- Client ------------------------------------------------------------------

/// The driver's XPC connection to the daemon.
pub struct XpcClient {
    target: Target,
    conn: Mutex<Option<Connection>>,
}

enum Target {
    Service(CString),
    Endpoint(Owned),
}

/// A live connection: cancelled and released on drop.
struct Connection {
    conn: Owned,
    ctx: Arc<ClientCtx>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Detach first, so no later event reaches the handler.
        let handler = lock(&self.ctx.handler).take();
        drop(handler);
        // SAFETY: the connection is live; `self.conn` releases it after.
        unsafe { xpc_connection_cancel(self.conn.0) };
    }
}

struct ClientCtx {
    handler: Mutex<Option<Arc<dyn ClientHandler>>>,
}

impl ClientCtx {
    fn handler(&self) -> Option<Arc<dyn ClientHandler>> {
        lock(&self.handler).clone()
    }
}

unsafe extern "C" fn client_event(ctx: *mut c_void, _conn: xpc_connection_t, event: xpc_object_t) {
    guarded("client event", || {
        // SAFETY: the connection holds a reference to its ClientCtx until
        // its finalizer runs, after its last event.
        let ctx = unsafe { &*ctx.cast_const().cast::<ClientCtx>() };
        let Some(h) = ctx.handler() else { return };
        // SAFETY: XPC passes a live event object.
        match unsafe { ovshim_kind(event) } {
            KIND_DICTIONARY => match decode_to_plugin(unsafe { dict_to_kv(event, 0) }) {
                Ok(m) => h.on_message(m),
                Err(e) => log(Level::Debug, &format!("ignored a message from the daemon: {e}")),
            },
            KIND_INTERRUPTED => h.on_interrupted(),
            KIND_INVALID => h.on_invalid(),
            _ => {}
        }
    });
}

/// One request's reply callback, taken by whichever comes first: the reply
/// or the timeout.
struct ReplySlot(Mutex<Option<ReplyFn>>);

impl ReplySlot {
    fn finish(&self, r: Result<ToPlugin, TransportError>) {
        let f = lock(&self.0).take();
        if let Some(f) = f {
            f(r);
        }
    }
}

unsafe extern "C" fn reply_event(ctx: *mut c_void, reply: xpc_object_t) {
    // SAFETY: the reference handed to ovshim_send_with_reply; XPC calls the
    // reply handler exactly once.
    let slot = unsafe { Arc::from_raw(ctx.cast_const().cast::<ReplySlot>()) };
    guarded("reply", || {
        // SAFETY: XPC passes a live reply or error object.
        let r = match unsafe { ovshim_kind(reply) } {
            KIND_DICTIONARY => {
                decode_to_plugin(unsafe { dict_to_kv(reply, 0) }).map_err(TransportError::Decode)
            }
            KIND_INTERRUPTED => Err(TransportError::Interrupted),
            _ => Err(TransportError::Invalid),
        };
        slot.finish(r);
    });
}

unsafe extern "C" fn reply_timeout(ctx: *mut c_void) {
    // SAFETY: the reference handed to ovshim_after, which runs once.
    let slot = unsafe { Arc::from_raw(ctx.cast_const().cast::<ReplySlot>()) };
    guarded("timeout", || slot.finish(Err(TransportError::Timeout)));
}

impl XpcClient {
    /// A client of the Mach service `service`. It connects on
    /// [`ClientTransport::connect`].
    pub fn new(service: &str) -> XpcClient {
        XpcClient { target: Target::Service(c_text(service)), conn: Mutex::new(None) }
    }
}

impl ClientTransport for XpcClient {
    fn connect(&self, h: Arc<dyn ClientHandler>) {
        let ctx = Arc::new(ClientCtx { handler: Mutex::new(Some(h)) });
        let raw = Arc::as_ptr(&ctx).cast_mut().cast::<c_void>();
        // SAFETY: the strings and the endpoint are live; `raw` stays valid
        // through the context reference given below.
        let conn = unsafe {
            match &self.target {
                Target::Service(name) => ovshim_connect(name.as_ptr(), 0, client_event, raw),
                Target::Endpoint(ep) => ovshim_connect_endpoint(ep.0, client_event, raw),
            }
        };
        let new = (!conn.is_null()).then(|| {
            // SAFETY: a new, live connection.
            unsafe { give_context(conn, &ctx) };
            Connection { conn: Owned(conn), ctx: ctx.clone() }
        });
        let old = std::mem::replace(&mut *lock(&self.conn), new);
        drop(old);
        if conn.is_null() {
            log(Level::Error, "cannot create an XPC connection");
            queue_run(Box::new(move || {
                if let Some(h) = ctx.handler() {
                    h.on_invalid();
                }
            }));
        }
    }

    fn cancel(&self) {
        let old = lock(&self.conn).take();
        drop(old);
    }

    fn send(&self, m: ToDaemon) {
        let msg = kv_to_dict(&encode_to_daemon(&m));
        if let Some(c) = &*lock(&self.conn) {
            // SAFETY: both objects are live.
            unsafe { xpc_connection_send_message(c.conn.0, msg.0) };
        }
    }

    fn request(
        &self,
        m: ToDaemon,
        timeout: Duration,
        reply: Box<dyn FnOnce(Result<ToPlugin, TransportError>) + Send>,
    ) {
        let slot = Arc::new(ReplySlot(Mutex::new(Some(reply))));
        let msg = kv_to_dict(&encode_to_daemon(&m));
        {
            let conn = lock(&self.conn);
            let Some(c) = &*conn else {
                drop(conn);
                queue_run(Box::new(move || slot.finish(Err(TransportError::Invalid))));
                return;
            };
            let ctx = Arc::into_raw(slot.clone()).cast_mut().cast();
            // SAFETY: both objects are live; reply_event takes the reference.
            unsafe { ovshim_send_with_reply(c.conn.0, msg.0, reply_event, ctx) };
        }
        let ctx = Arc::into_raw(slot).cast_mut().cast();
        // SAFETY: reply_timeout takes the reference.
        unsafe { ovshim_after(duration_ns(timeout), reply_timeout, ctx) };
    }

    fn after(&self, delay: Duration, f: Box<dyn FnOnce() + Send>) {
        queue_after(delay, f);
    }

    fn run(&self, f: Box<dyn FnOnce() + Send>) {
        queue_run(f);
    }
}

impl Drop for XpcClient {
    fn drop(&mut self) {
        self.cancel();
    }
}

// --- Server ------------------------------------------------------------------

/// The daemon's XPC listener.
pub struct XpcServer {
    target: ServerTarget,
    ctx: Arc<ServerCtx>,
}

enum ServerTarget {
    Service(CString),
    /// Made by [`anonymous_pair`], listening from the start.
    Anonymous,
}

struct ServerCtx {
    /// The service name, for log messages ("" when anonymous).
    service: String,
    handler: Mutex<Option<Arc<dyn ServerHandler>>>,
    /// Held while the listener is created, so its events wait for it.
    listener: Mutex<Option<Owned>>,
    peers: Mutex<Peers>,
    next_peer: AtomicU64,
    listener_invalid: AtomicBool,
    closing: AtomicBool,
}

#[derive(Default)]
struct Peers {
    by_id: HashMap<u64, PeerEntry>,
    /// Connection address to ID. Each connection is retained while listed,
    /// so its address cannot be reused meanwhile.
    by_conn: HashMap<usize, u64>,
}

struct PeerEntry {
    conn: Owned,
    accepted: bool,
    /// Reply objects of requests left unanswered, oldest first.
    held: VecDeque<Owned>,
}

/// How many unanswered requests' reply objects a peer keeps.
const HELD_REPLIES: usize = 8;

impl ServerCtx {
    fn new(service: &str) -> Arc<Self> {
        Arc::new(ServerCtx {
            service: service.to_owned(),
            handler: Mutex::new(None),
            listener: Mutex::new(None),
            peers: Mutex::new(Peers::default()),
            next_peer: AtomicU64::new(1),
            listener_invalid: AtomicBool::new(false),
            closing: AtomicBool::new(false),
        })
    }

    fn handler(&self) -> Option<Arc<dyn ServerHandler>> {
        lock(&self.handler).clone()
    }

    fn is_listener(&self, conn: xpc_connection_t) -> bool {
        lock(&self.listener).as_ref().is_some_and(|l| l.0 == conn)
    }

    /// A new peer: ask the handler, keep or cancel it.
    ///
    /// # Safety
    /// `this` must be the context's Arc pointer and `conn` a live peer.
    unsafe fn new_peer(this: *const ServerCtx, conn: xpc_connection_t) {
        // SAFETY: the caller's guarantee. The peer's events use the context
        // too, so the peer gets a reference of its own.
        let ctx = unsafe {
            Arc::increment_strong_count(this);
            let arc = Arc::from_raw(this);
            give_context(conn, &arc);
            drop(arc);
            &*this
        };
        // SAFETY: a live connection.
        let (euid, pid) = unsafe { (xpc_connection_get_euid(conn), xpc_connection_get_pid(conn)) };
        let id = ctx.next_peer.fetch_add(1, Ordering::Relaxed);
        {
            let mut peers = lock(&ctx.peers);
            // SAFETY: retained for as long as it is listed.
            let owned = Owned(unsafe { xpc_retain(conn) });
            peers
                .by_id
                .insert(id, PeerEntry { conn: owned, accepted: false, held: VecDeque::new() });
            peers.by_conn.insert(conn as usize, id);
        }
        let ok = ctx.handler().is_some_and(|h| h.on_peer(PeerInfo { id, euid, pid }));
        let refused = {
            let mut peers = lock(&ctx.peers);
            if ok {
                if let Some(p) = peers.by_id.get_mut(&id) {
                    p.accepted = true;
                }
                None
            } else {
                peers.by_conn.remove(&(conn as usize));
                peers.by_id.remove(&id)
            }
        };
        if let Some(p) = refused {
            // SAFETY: the peer is live; `p` releases it after.
            unsafe { xpc_connection_cancel(p.conn.0) };
        }
    }

    /// A message from a peer.
    ///
    /// # Safety
    /// `conn` must be a live peer and `event` a dictionary it sent.
    unsafe fn peer_message(&self, conn: xpc_connection_t, event: xpc_object_t) {
        let id = {
            let peers = lock(&self.peers);
            match peers.by_conn.get(&(conn as usize)) {
                Some(id) if peers.by_id.get(id).is_some_and(|p| p.accepted) => *id,
                _ => return,
            }
        };
        // SAFETY: the caller's guarantee.
        let kv = unsafe { dict_to_kv(event, 0) };
        let m = match decode_to_daemon(&kv) {
            Ok(m) => m,
            Err(e) => {
                log(Level::Debug, &format!("ignored a message from peer {id}: {e}"));
                return;
            }
        };
        let Some(h) = self.handler() else { return };
        // SAFETY: `event` is a live dictionary; null means no reply is
        // expected.
        let reply = unsafe { xpc_dictionary_create_reply(event) };
        if reply.is_null() {
            h.on_message(id, m);
            return;
        }
        let reply = Owned(reply);
        match h.on_request(id, m) {
            Some(r) => {
                fill_dict(reply.0, &encode_to_plugin(&r));
                // SAFETY: both objects are live.
                unsafe { xpc_connection_send_message(conn, reply.0) };
            }
            None => self.hold(id, reply),
        }
    }

    /// Keeps the reply object of a request left unanswered, so its client
    /// times out rather than being interrupted.
    fn hold(&self, id: u64, reply: Owned) {
        let released = {
            let mut peers = lock(&self.peers);
            match peers.by_id.get_mut(&id) {
                Some(p) => {
                    p.held.push_back(reply);
                    (p.held.len() > HELD_REPLIES).then(|| p.held.pop_front()).flatten()
                }
                // Gone meanwhile: nobody waits for the reply.
                None => Some(reply),
            }
        };
        drop(released);
    }

    /// A peer's connection ended.
    fn peer_closed(&self, conn: xpc_connection_t, interrupted: bool) {
        let gone = {
            let mut peers = lock(&self.peers);
            let id = peers.by_conn.remove(&(conn as usize));
            id.and_then(|id| peers.by_id.remove(&id).map(|p| (id, p)))
        };
        let Some((id, p)) = gone else { return };
        if interrupted {
            // SAFETY: the peer is live; `p` releases it after.
            unsafe { xpc_connection_cancel(p.conn.0) };
        }
        if p.accepted {
            if let Some(h) = self.handler() {
                h.on_peer_gone(id);
            }
        }
    }

    fn listener_event(&self, kind: c_int) {
        if kind != KIND_INVALID {
            return;
        }
        self.listener_invalid.store(true, Ordering::Release);
        if !self.closing.load(Ordering::Acquire) && !self.service.is_empty() {
            log(
                Level::Error,
                &format!(
                    "XPC listener for {} is invalid: the daemon was not launched by launchd \
                     (load its LaunchDaemon with launchctl bootstrap)",
                    self.service
                ),
            );
        }
    }
}

unsafe extern "C" fn server_event(ctx: *mut c_void, conn: xpc_connection_t, event: xpc_object_t) {
    guarded("server event", || {
        let this = ctx.cast_const().cast::<ServerCtx>();
        // SAFETY: the listener and every peer hold a reference to the
        // context until their finalizers run, after their last events.
        let server = unsafe { &*this };
        // SAFETY: XPC passes live objects.
        let kind = unsafe { ovshim_kind(event) };
        if kind == KIND_CONNECTION {
            // SAFETY: `this` is the context's Arc pointer; `conn` a new peer.
            unsafe { ServerCtx::new_peer(this, conn) };
        } else if server.is_listener(conn) {
            server.listener_event(kind);
        } else {
            match kind {
                // SAFETY: a live peer and its message.
                KIND_DICTIONARY => unsafe { server.peer_message(conn, event) },
                KIND_INTERRUPTED => server.peer_closed(conn, true),
                KIND_INVALID => server.peer_closed(conn, false),
                _ => {}
            }
        }
    });
}

impl XpcServer {
    /// A server for the Mach service `service`, which launchd must hold for
    /// this process (the daemon's plist lists it under `MachServices`). It
    /// listens once started.
    pub fn new(service: &str) -> XpcServer {
        XpcServer { target: ServerTarget::Service(c_text(service)), ctx: ServerCtx::new(service) }
    }

    /// Whether the listener reported itself invalid, which for a Mach
    /// service means the process was not launched by launchd with that
    /// service.
    pub fn listener_invalid(&self) -> bool {
        self.ctx.listener_invalid.load(Ordering::Acquire)
    }
}

impl ServerTransport for XpcServer {
    fn start(&self, h: Arc<dyn ServerHandler>) -> io::Result<()> {
        {
            let mut slot = lock(&self.ctx.handler);
            if slot.is_some() {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "server already started"));
            }
            *slot = Some(h);
        }
        let ServerTarget::Service(name) = &self.target else {
            return Ok(());
        };
        let mut listener = lock(&self.ctx.listener);
        let raw = Arc::as_ptr(&self.ctx).cast_mut().cast();
        // SAFETY: `name` is NUL-terminated; `raw` stays valid through the
        // context reference given below.
        let conn = unsafe { ovshim_listen(name.as_ptr(), server_event, raw) };
        if conn.is_null() {
            return Err(io::Error::other("cannot create the XPC listener"));
        }
        // SAFETY: a new, live connection.
        unsafe { give_context(conn, &self.ctx) };
        *listener = Some(Owned(conn));
        Ok(())
    }

    fn send(&self, peer: u64, m: ToPlugin) {
        let msg = kv_to_dict(&encode_to_plugin(&m));
        let peers = lock(&self.ctx.peers);
        if let Some(p) = peers.by_id.get(&peer).filter(|p| p.accepted) {
            // SAFETY: both objects are live.
            unsafe { xpc_connection_send_message(p.conn.0, msg.0) };
        }
    }
}

impl Drop for XpcServer {
    fn drop(&mut self) {
        self.ctx.closing.store(true, Ordering::Release);
        let listener = lock(&self.ctx.listener).take();
        if let Some(l) = &listener {
            // SAFETY: the listener is live; `l` releases it after.
            unsafe { xpc_connection_cancel(l.0) };
        }
        let peers = std::mem::take(&mut *lock(&self.ctx.peers));
        for p in peers.by_id.values() {
            // SAFETY: the peer is live; dropping `peers` releases it.
            unsafe { xpc_connection_cancel(p.conn.0) };
        }
        let handler = lock(&self.ctx.handler).take();
        drop((listener, peers, handler));
    }
}

/// A server on an anonymous listener and a client connected to it through
/// its endpoint, in this process. For tests: the server must be started
/// before the client sends anything.
pub fn anonymous_pair() -> (XpcServer, XpcClient) {
    let ctx = ServerCtx::new("");
    let mut endpoint: xpc_object_t = ptr::null_mut();
    {
        let mut listener = lock(&ctx.listener);
        let raw = Arc::as_ptr(&ctx).cast_mut().cast();
        // SAFETY: `raw` stays valid through the context reference given
        // below; `endpoint` is a valid out-pointer.
        let conn = unsafe { ovshim_listen_anonymous(server_event, raw, &mut endpoint) };
        assert!(!conn.is_null() && !endpoint.is_null(), "cannot create an anonymous XPC listener");
        // SAFETY: a new, live connection.
        unsafe { give_context(conn, &ctx) };
        *listener = Some(Owned(conn));
    }
    let server = XpcServer { target: ServerTarget::Anonymous, ctx };
    let client = XpcClient { target: Target::Endpoint(Owned(endpoint)), conn: Mutex::new(None) };
    (server, client)
}
