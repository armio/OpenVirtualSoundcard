//! The connection to the daemon (design sections 10.6, 11 and 12).
//!
//! The link says hello to the daemon's Mach service, maps the shared region
//! the welcome carries, hands it to the IO engine and retires the previous
//! one; it stages the configurations the daemon sends, asks the HAL for a
//! configuration change when one alters the device's structure, and
//! reconnects when the daemon goes away. It reaches the driver only through
//! a [`LinkSink`].
//!
//! Everything the link does runs on its transport's serial queue (the XPC
//! shim's `org.openvirtualsoundcard.ipc` in the plug-in): transport events, its timers
//! and the work its public methods post. Those methods only post, so the
//! HAL's threads never wait for the daemon. The link holds none of its locks
//! while it calls the sink or the transport, so the host calls the sink
//! makes (PropertiesChanged, RequestDeviceConfigurationChange, storage) run
//! with no lock held.
//!
//! The connection state machine is in `state`, the configuration staging in
//! `storage` and the retire protocol in `retire`.

mod retire;
mod state;
mod storage;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use ovsc_ipc::protocol::{
    ConfigApplied, Hello, PROTO_MAJOR, PROTO_MINOR, RejectReason, ToDaemon, ToPlugin, Welcome,
};
use ovsc_ipc::region::MappedRegion;
use ovsc_ipc::transport::{ClientHandler, TransportError};
use ovsc_shm::layout::{
    HOST_ARCH, LAYOUT_HASH, LAYOUT_VERSION, LayoutError, REGION_SIZE, RegionRef,
};
use ovsc_shm::status::AppliedWord;

/// The client side of a transport to the daemon.
pub use ovsc_ipc::transport::ClientTransport;

pub use retire::{RETIRE_GRACE_NS, RETIRE_POLL};
pub use state::{FAST_EVERY, HELLO_TIMEOUT, INVALID_EVERY, REFUSED_EVERY};
pub use storage::{REQUEST_DELAY_NS, REQUEST_RETRY_NS};

use crate::abi::OSStatus;
use crate::entry::LinkFactory;
use crate::io::Attachment;
use crate::model::DriverConfig;
use crate::platform::{self, LOG_DEFAULT, LOG_ERROR, LOG_INFO, Timebase};
use retire::Retirement;
use state::{Machine, Plan};
use storage::{Request, Staging};

/// How long the link keeps a daemon's region after losing the daemon
/// without a new welcome; then it releases the region, which frees the dead
/// daemon's memory.
pub const DETACH_AFTER: Duration = Duration::from_secs(30);

/// How often the link calls [`LinkSink::idle_tick`].
pub const IDLE_TICK: Duration = Duration::from_secs(1);

/// A transport that never connects. Nothing posted to it ever runs, so a
/// link on it never starts and stays absent.
pub struct NullTransport;

impl ClientTransport for NullTransport {
    fn connect(&self, _h: Arc<dyn ClientHandler>) {}

    fn cancel(&self) {}

    fn send(&self, _m: ToDaemon) {}

    // Only a started link sends requests, and none starts on this transport.
    fn request(
        &self,
        _m: ToDaemon,
        _timeout: Duration,
        _reply: Box<dyn FnOnce(Result<ToPlugin, TransportError>) + Send>,
    ) {
    }

    fn after(&self, _delay: Duration, _f: Box<dyn FnOnce() + Send>) {}

    fn run(&self, _f: Box<dyn FnOnce() + Send>) {}
}

/// The transport factory the HAL-loaded driver uses: an XPC client of the
/// daemon's Mach service on macOS. There is no daemon to reach elsewhere,
/// so there the transport never connects.
pub(crate) fn production_link_factory() -> LinkFactory {
    #[cfg(target_os = "macos")]
    {
        use ovsc_ipc::protocol::SERVICE_NAME;
        use ovsc_ipc::xpc::XpcClient;
        Box::new(|| Arc::new(XpcClient::new(SERVICE_NAME)) as Arc<dyn ClientTransport>)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Box::new(|| Arc::new(NullTransport) as Arc<dyn ClientTransport>)
    }
}

/// What the link needs from the driver. Every call comes from the link's
/// serial queue, with none of the link's locks held.
pub trait LinkSink: Send + Sync {
    /// Installs `a` as the IO engine's attachment and returns the previous
    /// one. The link neutralizes and retires what comes back.
    fn attach(&self, a: Option<Box<Attachment>>) -> Option<Box<Attachment>>;
    /// Whether no real-time path is using an attachment right now.
    fn quiescent(&self) -> bool;
    /// The generation of the attached daemon region.
    fn current_generation(&self) -> Option<u64>;
    /// The configuration the device publishes.
    fn published_config(&self) -> DriverConfig;
    /// Takes a valid configuration from the daemon: publishes it at once if
    /// only its names (or nothing but its generation) differ, or keeps it
    /// pending for PerformDeviceConfigurationChange if its structure does.
    fn stage(&self, cfg: DriverConfig) -> ConfigPlan;
    /// RequestDeviceConfigurationChange for the device.
    fn request_config_change(&self) -> OSStatus;
    /// PropertiesChanged for the channel names of both directions.
    fn names_changed(&self);
    /// Persists `cfg` in host storage.
    fn store(&self, cfg: &DriverConfig);
    /// The link's state changed.
    fn set_status(&self, s: LinkStatus);
    /// Host time in nanoseconds: the clock the IO engine checks heartbeats
    /// against.
    fn now_ns(&self) -> u64;

    /// Validates a freshly mapped region (design section 10.6, steps 1 and
    /// 2), checking its clock base against [`LinkSink::now_ns`]: the clock
    /// the IO engine checks heartbeats against, also in tests on a manual
    /// clock.
    fn new_attachment(&self, mapped: MappedRegion) -> Result<Box<Attachment>, LayoutError> {
        Attachment::new_at(mapped, self.now_ns())
    }

    /// The host clock's tick rate, which the hello reports.
    fn timebase(&self) -> Timebase {
        platform::production().timebase()
    }

    /// StartIO minus StopIO, for the plug-in status.
    fn io_clients(&self) -> u32 {
        0
    }

    /// Whether the device runs IO changed: PropertiesChanged for the
    /// device's 'goin'.
    fn running_changed(&self) {}

    /// About every [`IDLE_TICK`]: lets the IO engine follow the daemon's
    /// clock while no client runs IO.
    fn idle_tick(&self) {}

    /// Logs one line at `level` (a `platform::LOG_*` constant).
    fn log(&self, level: u8, msg: &str) {
        let _ = (level, msg);
    }
}

/// How a configuration from the daemon differs from the published one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigPlan {
    /// In nothing but, perhaps, its generation.
    Same,
    /// In its names only, which are published at once.
    NamesOnly,
    /// In its structure, which needs a HAL configuration change.
    Structural,
}

impl ConfigPlan {
    /// How `cfg` differs from `published`.
    pub fn of(published: &DriverConfig, cfg: &DriverConfig) -> ConfigPlan {
        storage::plan(published, cfg)
    }
}

/// The link's state, for the status property.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkStatus {
    /// Not started, or the daemon's service does not exist.
    Absent,
    /// Saying hello: at start, or after losing the daemon.
    Connecting,
    /// Welcomed by the daemon of this generation, whose region is attached.
    Attached { generation: u64 },
    /// The daemon refused this plug-in, or its region or reply is unusable
    /// here; the reason is one word.
    Incompatible(String),
}

/// What only the link's queue changes.
struct Inner {
    machine: Machine,
    /// The connection whose handler events count.
    conn: u64,
    /// Identifies the running countdown to releasing the region.
    detach_epoch: u64,
    /// The current attachment's view and generation, for plug-in status
    /// line 2. The attachment stays alive while it is current, and only the
    /// queue replaces it.
    view: Option<(RegionRef<'static>, u64)>,
    /// The generation of the daemon whose configuration was staged last.
    source: u64,
    /// What config_applied last reported: (daemon generation, config_gen).
    applied: Option<(u64, u64)>,
    staging: Staging,
    retiring: Retirement<Box<Attachment>>,
}

/// The driver's connection to the daemon. Created at Initialize; lives as
/// long as its driver.
pub struct Link {
    transport: Arc<dyn ClientTransport>,
    sink: Arc<dyn LinkSink>,
    instance: u64,
    pid: i32,
    me: Weak<Link>,
    inner: Mutex<Inner>,
    status: Mutex<LinkStatus>,
    /// Regions attached so far.
    attach_count: AtomicU64,
}

/// The handler of one connection.
struct Handler {
    link: Weak<Link>,
    conn: u64,
}

impl Handler {
    /// The link, if this is still its connection.
    fn live(&self) -> Option<Arc<Link>> {
        let link = self.link.upgrade()?;
        let current = link.lock().conn == self.conn;
        current.then_some(link)
    }
}

impl ClientHandler for Handler {
    fn on_message(&self, m: ToPlugin) {
        if let Some(link) = self.live() {
            link.on_message(m);
        }
    }

    fn on_interrupted(&self) {
        if let Some(link) = self.live() {
            link.on_interrupted();
        }
    }

    fn on_invalid(&self) {
        if let Some(link) = self.live() {
            let plan = link.lock().machine.invalid(None);
            link.invalid(plan);
        }
    }
}

impl Link {
    /// A link to the daemon over `t`, for the driver behind `sink`.
    /// `instance` identifies this driver object to the daemon; `pid` is this
    /// process's. Nothing happens until [`Link::start`].
    pub fn new(
        t: Arc<dyn ClientTransport>,
        sink: Arc<dyn LinkSink>,
        instance: u64,
        pid: i32,
    ) -> Arc<Link> {
        let start_ns = sink.now_ns();
        Arc::new_cyclic(|me| Link {
            transport: t,
            sink,
            instance,
            pid,
            me: me.clone(),
            inner: Mutex::new(Inner {
                machine: Machine::new(),
                conn: 0,
                detach_epoch: 0,
                view: None,
                source: 0,
                applied: None,
                staging: Staging::new(start_ns),
                retiring: Retirement::new(),
            }),
            status: Mutex::new(LinkStatus::Absent),
            attach_count: AtomicU64::new(0),
        })
    }

    /// Connects and says hello, from the queue. Only the first call counts.
    pub fn start(self: &Arc<Self>) {
        self.post(Link::on_start);
    }

    /// Called from PerformDeviceConfigurationChange once `cfg` is applied:
    /// sends config_applied and stores `cfg`, from the queue.
    pub fn performed(self: &Arc<Self>, cfg: &DriverConfig) {
        let cfg = cfg.clone();
        self.post(move |l| l.on_performed(cfg));
    }

    /// Called from AbortDeviceConfigurationChange, and from a Perform that
    /// found nothing pending: the request is over. A structural
    /// configuration still waiting is asked for again after 1 s.
    pub fn aborted(self: &Arc<Self>) {
        self.post(Link::on_aborted);
    }

    /// Called from the first StartIO and the last StopIO, on the HAL's
    /// thread: posts PropertiesChanged on the device's 'goin' from the IPC
    /// queue (design section 13) and updates the plug-in status.
    pub fn io_running_changed(self: &Arc<Self>) {
        self.post(|l| {
            l.sink.running_changed();
            l.write_status();
        });
    }

    /// The link's state, for the status property. Callable from any thread.
    pub fn status(&self) -> LinkStatus {
        self.status.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// How many daemon regions this link has attached.
    pub fn attach_count(&self) -> u64 {
        self.attach_count.load(Ordering::Relaxed)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn log(&self, level: u8, msg: &str) {
        self.sink.log(level, msg);
    }

    /// Runs `f` on the queue.
    fn post(&self, f: impl FnOnce(&Link) + Send + 'static) {
        let me = self.me.clone();
        self.transport.run(Box::new(move || {
            if let Some(l) = me.upgrade() {
                f(&l);
            }
        }));
    }

    /// Runs `f` on the queue after `delay`.
    fn after(&self, delay: Duration, f: impl FnOnce(&Link) + Send + 'static) {
        let me = self.me.clone();
        self.transport.after(
            delay,
            Box::new(move || {
                if let Some(l) = me.upgrade() {
                    f(&l);
                }
            }),
        );
    }

    fn set_status(&self, s: LinkStatus) {
        let changed = {
            let mut status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
            let changed = *status != s;
            if changed {
                *status = s.clone();
            }
            changed
        };
        if changed {
            self.sink.set_status(s);
        }
    }

    // --- Connection ---------------------------------------------------------

    fn on_start(&self) {
        let plan = self.lock().machine.start();
        if plan.is_empty() {
            return;
        }
        self.set_status(LinkStatus::Connecting);
        self.execute(plan);
        self.after(IDLE_TICK, Link::on_idle_tick);
    }

    /// Runs every [`IDLE_TICK`] from the start on.
    fn on_idle_tick(&self) {
        self.sink.idle_tick();
        self.after(IDLE_TICK, Link::on_idle_tick);
    }

    /// Carries out a plan of the state machine.
    fn execute(&self, plan: Plan) {
        if plan.cancel {
            self.next_conn();
            self.transport.cancel();
        }
        if plan.connect {
            let conn = self.next_conn();
            self.transport.connect(Arc::new(Handler { link: self.me.clone(), conn }));
        }
        if plan.detached {
            self.count_down_to_detach();
        }
        if let Some(n) = plan.hello {
            self.hello(n);
        }
        if let Some((delay, epoch)) = plan.timer {
            self.after(delay, move |l| {
                let plan = l.lock().machine.on_timer(epoch);
                l.execute(plan);
            });
        }
    }

    /// Moves to a new connection: events of the previous one stop counting.
    fn next_conn(&self) -> u64 {
        let mut i = self.lock();
        i.conn = i.conn.wrapping_add(1);
        i.conn
    }

    /// Sends hello number `n`, describing what the device publishes.
    fn hello(&self, n: u64) {
        let cfg = self.sink.published_config();
        let applied = self.lock().applied;
        let (applied_daemon_generation, applied_config_gen) =
            applied.unwrap_or((0, cfg.config_gen));
        let tb = self.sink.timebase();
        let hello = Hello {
            proto_major: PROTO_MAJOR,
            proto_minor: PROTO_MINOR,
            layout_version: LAYOUT_VERSION,
            layout_hash: LAYOUT_HASH,
            plugin_version: env!("CARGO_PKG_VERSION").to_owned(),
            instance: self.instance,
            pid: self.pid,
            applied_daemon_generation,
            applied_config_gen,
            sample_rate: cfg.sample_rate,
            input_channels: cfg.input_channels,
            output_channels: cfg.output_channels,
            timebase_numer: tb.numer,
            timebase_denom: tb.denom,
            arch: HOST_ARCH,
        };
        let me = self.me.clone();
        self.transport.request(
            ToDaemon::Hello(hello),
            HELLO_TIMEOUT,
            Box::new(move |r| {
                if let Some(l) = me.upgrade() {
                    l.on_reply(n, r);
                }
            }),
        );
    }

    /// The outcome of hello number `n`. Any welcome is taken while not
    /// attached; timeouts and interruptions leave the schedule to retry.
    fn on_reply(&self, n: u64, r: Result<ToPlugin, TransportError>) {
        match r {
            Ok(ToPlugin::Welcome(w)) => self.on_welcome(w),
            Ok(ToPlugin::Reject(r)) => {
                if !self.lock().machine.takes_welcome() {
                    return;
                }
                let reason = match r.reason {
                    RejectReason::Proto => "proto",
                    RejectReason::Layout => "layout",
                };
                self.log(
                    LOG_ERROR,
                    &format!(
                        "the daemon refused this plug-in ({reason}): {}; trying again every {} s",
                        r.message,
                        REFUSED_EVERY.as_secs()
                    ),
                );
                self.refuse(reason);
            }
            Ok(ToPlugin::Config(_) | ToPlugin::Bye { .. }) => {
                self.log(LOG_INFO, "ignored a reply to hello that is no welcome");
            }
            Err(TransportError::Invalid) => {
                let plan = self.lock().machine.invalid(Some(n));
                self.invalid(plan);
            }
            Err(TransportError::Decode(e)) => {
                if self.lock().machine.takes_welcome() {
                    self.log(LOG_ERROR, &format!("the daemon's reply to hello is unusable: {e}"));
                    self.refuse("reply");
                }
            }
            Err(TransportError::Timeout | TransportError::Interrupted) => {}
        }
    }

    fn on_message(&self, m: ToPlugin) {
        match m {
            ToPlugin::Config(cfg) => {
                let attached = {
                    let mut i = self.lock();
                    let g = i.machine.attached();
                    if let Some(g) = g {
                        i.source = g;
                    }
                    g.is_some()
                };
                if attached {
                    self.stage(cfg);
                }
            }
            ToPlugin::Bye { reason } => {
                let plan = self.lock().machine.bye();
                if plan.is_empty() {
                    return;
                }
                self.log(LOG_DEFAULT, &format!("the daemon is going away ({reason})"));
                self.set_status(LinkStatus::Connecting);
                self.execute(plan);
            }
            ToPlugin::Welcome(_) | ToPlugin::Reject(_) => {}
        }
    }

    fn on_interrupted(&self) {
        let plan = self.lock().machine.interrupted();
        if plan.is_empty() {
            return;
        }
        self.log(LOG_DEFAULT, "the daemon went away; reconnecting");
        self.set_status(LinkStatus::Connecting);
        self.execute(plan);
    }

    /// Carries out the state machine's plan for an invalid connection.
    fn invalid(&self, plan: Plan) {
        if plan.is_empty() {
            return;
        }
        self.log(
            LOG_INFO,
            &format!(
                "the daemon's service does not exist; trying again every {} s",
                INVALID_EVERY.as_secs()
            ),
        );
        self.set_status(LinkStatus::Absent);
        self.execute(plan);
    }

    /// The daemon refused this plug-in, or what it sent is unusable here:
    /// incompatible, with a one-word `reason`.
    fn refuse(&self, reason: &str) {
        let plan = self.lock().machine.refused();
        if plan.is_empty() {
            return;
        }
        self.set_status(LinkStatus::Incompatible(reason.to_owned()));
        self.execute(plan);
    }

    fn on_welcome(&self, w: Welcome) {
        if !self.lock().machine.takes_welcome() {
            return;
        }
        let generation = w.daemon_generation;
        if w.proto_major != PROTO_MAJOR {
            self.log(
                LOG_ERROR,
                &format!(
                    "the daemon speaks protocol {}.{}, this plug-in {PROTO_MAJOR}.{PROTO_MINOR}",
                    w.proto_major, w.proto_minor
                ),
            );
            return self.refuse("proto");
        }
        if w.region_size != REGION_SIZE as u64 {
            self.log(
                LOG_ERROR,
                &format!(
                    "the daemon's region is {} bytes, this plug-in's layout {REGION_SIZE}",
                    w.region_size
                ),
            );
            return self.refuse("region");
        }
        // The same generation is the same region: nothing to remap.
        if self.sink.current_generation() != Some(generation) {
            if let Err(reason) = self.attach(&w) {
                return self.refuse(reason);
            }
        }
        {
            let mut i = self.lock();
            i.machine.welcomed(generation);
            i.source = generation;
            // The countdown to releasing the region stops.
            i.detach_epoch = i.detach_epoch.wrapping_add(1);
        }
        self.set_status(LinkStatus::Attached { generation });
        self.write_status();
        self.stage(w.config);
    }

    /// Maps the welcome's region, validates it, commits the pages the IO
    /// paths will use, swaps it in and retires the previous attachment.
    /// Returns the reason it is unusable, as one word.
    fn attach(&self, w: &Welcome) -> Result<(), &'static str> {
        let mapped = w.region.map().map_err(|e| {
            self.log(LOG_ERROR, &format!("cannot map the daemon's region: {e}"));
            "map"
        })?;
        let a = self.sink.new_attachment(mapped).map_err(|e| {
            self.log(LOG_ERROR, &format!("the daemon's region is unusable: {e}"));
            match e {
                LayoutError::ClockBase { .. } => "clock",
                _ => "layout",
            }
        })?;
        if a.generation != w.daemon_generation {
            self.log(
                LOG_ERROR,
                &format!(
                    "the daemon's welcome is for generation {:016x}, its region {:016x}",
                    w.daemon_generation, a.generation
                ),
            );
            return Err("generation");
        }
        let (theirs, ours) = (a.daemon_timebase(), self.sink.timebase());
        if theirs != ours {
            self.log(
                LOG_INFO,
                &format!(
                    "the daemon's timebase is {}/{}, ours {}/{}; the region carries nanoseconds",
                    theirs.numer, theirs.denom, ours.numer, ours.denom
                ),
            );
        }
        let cfg = self.sink.published_config();
        a.touch(
            cfg.input_channels.max(w.config.input_channels),
            cfg.output_channels.max(w.config.output_channels),
        );
        let view = (a.view, a.generation);
        let old = self.sink.attach(Some(a));
        let count = self.attach_count.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        self.lock().view = Some(view);
        self.log(
            LOG_DEFAULT,
            &format!(
                "attached to daemon {} (generation {:016x}, attachment {count})",
                w.daemon_version, w.daemon_generation
            ),
        );
        self.retire(old);
        Ok(())
    }

    /// Releases the region `DETACH_AFTER` from now, unless a welcome comes
    /// first.
    fn count_down_to_detach(&self) {
        let epoch = {
            let mut i = self.lock();
            i.detach_epoch = i.detach_epoch.wrapping_add(1);
            i.detach_epoch
        };
        self.after(DETACH_AFTER, move |l| l.on_detach(epoch));
    }

    fn on_detach(&self, epoch: u64) {
        {
            let i = self.lock();
            if i.detach_epoch != epoch || i.machine.attached().is_some() {
                return;
            }
        }
        let old = self.sink.attach(None);
        self.lock().view = None;
        if let Some(a) = &old {
            self.log(
                LOG_DEFAULT,
                &format!(
                    "no daemon for {} s: released the region of generation {:016x}",
                    DETACH_AFTER.as_secs(),
                    a.generation
                ),
            );
        }
        self.retire(old);
    }

    // --- Retiring -----------------------------------------------------------

    /// Neutralizes `old`'s mapping at once and frees it once the IO engine
    /// has been seen quiescent and `RETIRE_GRACE_NS` has passed.
    fn retire(&self, old: Option<Box<Attachment>>) {
        let Some(old) = old else { return };
        if let Err(e) = old.mapped.neutralize() {
            self.log(
                LOG_ERROR,
                &format!("cannot neutralize the old region's mapping, keeping it: {e}"),
            );
        }
        let now = self.sink.now_ns();
        let poll = self.lock().retiring.push(old, now);
        if poll {
            self.after(RETIRE_POLL, Link::poll_retiring);
        }
    }

    fn poll_retiring(&self) {
        // Read after every swap of the attachments waiting: this poll was
        // scheduled after the newest of them.
        let quiescent = self.sink.quiescent();
        let now = self.sink.now_ns();
        let (done, again) = self.lock().retiring.poll(quiescent, now);
        for a in done {
            self.log(LOG_INFO, &format!("freed the region of generation {:016x}", a.generation));
            drop(a);
        }
        if again {
            self.after(RETIRE_POLL, Link::poll_retiring);
        }
    }

    // --- Configuration ------------------------------------------------------

    /// Takes a configuration from the daemon (design section 10.6, step 3).
    fn stage(&self, cfg: DriverConfig) {
        if let Err(e) = cfg.validate() {
            if self.lock().staging.rejected(cfg.config_gen) {
                self.log(
                    LOG_ERROR,
                    &format!("configuration {} from the daemon ignored: {e}", cfg.config_gen),
                );
            }
            return;
        }
        match self.sink.stage(cfg.clone()) {
            ConfigPlan::Same => {
                self.lock().staging.settle();
                self.applied(&cfg);
            }
            ConfigPlan::NamesOnly => {
                self.lock().staging.settle();
                self.log(LOG_INFO, &format!("configuration {}: names changed", cfg.config_gen));
                self.sink.names_changed();
                self.applied(&cfg);
                self.sink.store(&cfg);
            }
            ConfigPlan::Structural => {
                self.log(
                    LOG_DEFAULT,
                    &format!(
                        "configuration {}: {} Hz, {} in, {} out; asking for a device change",
                        cfg.config_gen, cfg.sample_rate, cfg.input_channels, cfg.output_channels
                    ),
                );
                self.lock().staging.want(cfg);
                self.request();
            }
        }
        self.write_status();
    }

    /// Sends config_applied for `cfg` to the daemon it came from, if still
    /// attached to it, and remembers it for the next hello.
    fn applied(&self, cfg: &DriverConfig) {
        let to = {
            let mut i = self.lock();
            let source = i.source;
            i.applied = Some((source, cfg.config_gen));
            i.machine.attached().filter(|g| *g == source)
        };
        if let Some(daemon_generation) = to {
            self.transport.send(ToDaemon::ConfigApplied(ConfigApplied {
                daemon_generation,
                config_gen: cfg.config_gen,
                sample_rate: cfg.sample_rate,
                input_channels: cfg.input_channels,
                output_channels: cfg.output_channels,
            }));
        }
    }

    /// Asks the HAL for a configuration change if one is wanted and allowed
    /// now, or arranges to ask later.
    fn request(&self) {
        let now = self.sink.now_ns();
        let next = self.lock().staging.next(now);
        match next {
            Request::Nothing => {}
            Request::Later(delay) => self.after(delay, |l| {
                l.lock().staging.timer_fired();
                l.request();
            }),
            Request::Now => {
                let status = self.sink.request_config_change();
                let first_failure = self.lock().staging.requested(status == 0, now);
                if status != 0 {
                    if first_failure {
                        self.log(
                            LOG_ERROR,
                            &format!(
                                "RequestDeviceConfigurationChange failed ({status}); trying \
                                 again every second"
                            ),
                        );
                    }
                    self.request();
                }
            }
        }
    }

    fn on_performed(&self, cfg: DriverConfig) {
        self.lock().staging.performed(&cfg);
        self.log(
            LOG_DEFAULT,
            &format!(
                "configuration {} applied: {} Hz, {} in, {} out",
                cfg.config_gen, cfg.sample_rate, cfg.input_channels, cfg.output_channels
            ),
        );
        self.applied(&cfg);
        self.sink.store(&cfg);
        self.write_status();
        // A newer structural configuration may have arrived meanwhile.
        self.request();
    }

    fn on_aborted(&self) {
        let now = self.sink.now_ns();
        let wanted = {
            let mut i = self.lock();
            i.staging.aborted(now);
            i.staging.wanted()
        };
        if wanted {
            self.log(LOG_INFO, "the configuration change was aborted; asking again in 1 s");
        }
        self.request();
    }

    // --- Status ---------------------------------------------------------------

    /// Plug-in status line 2 of the current region (design section 6.5).
    fn write_status(&self) {
        let Some((view, generation)) = self.lock().view else { return };
        let cfg = self.sink.published_config();
        let applied =
            AppliedWord { sample_rate: cfg.sample_rate, config_gen: cfg.config_gen as u32 };
        let p = view.plugin();
        let r = Ordering::Relaxed;
        p.plugin_instance.store(self.instance, r);
        p.plugin_pid.store(u64::try_from(self.pid).unwrap_or(0), r);
        p.applied_word.store(applied.pack(), r);
        p.io_clients.store(u64::from(self.sink.io_clients()), r);
        p.attach_count.store(self.attach_count(), r);
        p.attached_generation.store(generation, Ordering::Release);
    }
}
