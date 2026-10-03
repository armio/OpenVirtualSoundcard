//! The service the driver connects to (design sections 10 and 14.3).
//!
//! [`HalServer`] answers each driver instance's hello with a welcome (the
//! region and the current configuration) or a reject, keeps a table of the
//! instances attached, and pushes a new configuration to all of them
//! whenever a change of the engine changes it. It also owns the engine
//! words of the region's daemon status, the status task and the power
//! assertion.
//!
//! Transport callbacks run on the transport's queue and only hold the
//! server's state lock briefly; the daemon never waits on a driver.

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use ovsc_clock::local_now_ns;
use ovsc_core::{Device, DeviceObserver};
use ovsc_ipc::protocol::{
    ConfigApplied, DriverConfig, Hello, PROTO_MAJOR, PROTO_MINOR, Reject, RejectReason, ToDaemon,
    ToPlugin, Welcome,
};
use ovsc_ipc::transport::{PeerInfo, ServerHandler, ServerTransport};
use ovsc_shm::layout::{HOST_ARCH, LAYOUT_HASH, LAYOUT_VERSION, REGION_SIZE};
use ovsc_shm::status::{AudioWord, ChannelsWord, DAEMON_ENGINE_RUNNING, DAEMON_SHUTTING_DOWN};

use crate::config::{EngineInfo, HalOptions, driver_config};
use crate::power::PowerAssertion;
use crate::region::HalRegion;
use crate::status::{self, HalStatus};

/// How long [`HalServer::shutdown`] gives its byes to leave before closing
/// the connections.
const BYE_GRACE: Duration = Duration::from_millis(100);

/// The reason given with the power assertion.
const POWER_REASON: &str = "OpenVirtualSoundcard streaming";

/// The daemon's driver service. Dropping it stops the service without the
/// courtesies of [`HalServer::shutdown`], as if the daemon had died: the
/// drivers see their connections interrupted and the heartbeat go stale.
pub struct HalServer {
    inner: Arc<Inner>,
    /// The only strong reference, so that dropping it closes the service.
    transport: Option<Arc<dyn ServerTransport>>,
    status_task: Option<JoinHandle<()>>,
}

/// What the transport's callbacks, the status task and the device watcher
/// share.
pub(crate) struct Inner {
    pub(crate) opts: HalOptions,
    pub(crate) region: Arc<HalRegion>,
    /// The daemon's version, as in the region header.
    version: String,
    runtime: Handle,
    transport: OnceLock<Weak<dyn ServerTransport>>,
    state: Mutex<State>,
}

struct State {
    /// The engine the configuration describes.
    engine: EngineInfo,
    /// Whether `engine` passed `driver_config`.
    engine_ok: bool,
    /// The configuration offered in welcomes and pushed on changes.
    config: DriverConfig,
    /// A device runs (between `engine_started` and `engine_stopped`).
    running: bool,
    /// Bumped on every engine start and stop, so a watcher of an older
    /// device never applies its names.
    epoch: u64,
    observer: Option<DeviceObserver>,
    watcher: Option<JoinHandle<()>>,
    power: Option<PowerAssertion>,
    peers: BTreeMap<u64, Peer>,
    /// Shutting down: new peers and hellos are refused.
    closing: bool,
}

/// An accepted driver connection.
struct Peer {
    info: PeerInfo,
    /// The last compatible hello.
    hello: Option<Hello>,
    /// Got a welcome: it maps the region and receives configurations.
    welcomed: bool,
    /// The configuration generation last re-sent because it reported an
    /// older one applied.
    resent_gen: u64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl HalServer {
    /// Starts serving `region` on `t`, offering a configuration built from
    /// `initial` until an engine starts. Must be called within a tokio
    /// runtime, which runs the status task and the device watcher.
    ///
    /// Fails if `initial` cannot be offered (see [`driver_config`]) or the
    /// transport cannot start.
    pub fn start(
        o: HalOptions,
        region: Arc<HalRegion>,
        t: Box<dyn ServerTransport>,
        initial: EngineInfo,
    ) -> io::Result<HalServer> {
        let runtime = Handle::try_current()
            .map_err(|_| io::Error::other("the driver service needs a tokio runtime"))?;
        let config = driver_config(&initial, 1, &o)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let view = region.view();
        let d = view.daemon();
        d.flags.store(0, Ordering::Release);
        d.audio_word.store(0, Ordering::Release);
        d.peers.store(0, Ordering::Relaxed);
        d.heartbeat_ns.store(local_now_ns(), Ordering::Release);
        let inner = Arc::new(Inner {
            version: view.header().daemon_version().to_owned(),
            opts: o,
            region: region.clone(),
            runtime,
            transport: OnceLock::new(),
            state: Mutex::new(State {
                engine: initial,
                engine_ok: true,
                config,
                running: false,
                epoch: 0,
                observer: None,
                watcher: None,
                power: None,
                peers: BTreeMap::new(),
                closing: false,
            }),
        });
        let transport: Arc<dyn ServerTransport> = Arc::from(t);
        // The handler holds only a weak reference to the transport (the
        // transport holds the handler).
        let _ = inner.transport.set(Arc::downgrade(&transport));
        transport.start(inner.clone())?;
        let status_task = inner.runtime.spawn(status::run(Arc::downgrade(&inner)));
        info!(
            "hal: serving {} (region generation {:016x}, {} bytes)",
            inner.opts.service_name,
            region.generation(),
            REGION_SIZE
        );
        Ok(HalServer { inner, transport: Some(transport), status_task: Some(status_task) })
    }

    /// The engine is running on `device`, whose rings are the region's:
    /// offers its configuration (pushed to attached drivers if it changed),
    /// writes the engine words and sets ENGINE_RUNNING, watches the device
    /// for renames, and takes the power assertion if configured.
    pub fn engine_started(&self, device: &Device) {
        // Subscribed before the names are read, so no change is missed.
        let changes = device.changes();
        let engine = EngineInfo::from_device(device);
        let observer = device.observer();
        let inner = &self.inner;
        let mut st = inner.lock();
        if st.closing {
            return;
        }
        st.epoch += 1;
        if let Some(w) = st.watcher.take() {
            w.abort();
        }
        st.running = true;
        st.observer = Some(observer.clone());
        info!(
            "hal: engine started: {} Hz, {} in, {} out, latency {} samples",
            engine.sample_rate,
            engine.rx_names.len(),
            engine.tx_names.len(),
            engine.latency_samples
        );
        inner.update_engine(&mut st, engine.clone());
        if inner.opts.prevent_idle_sleep && st.power.is_none() {
            match PowerAssertion::take(POWER_REASON) {
                Ok(a) => st.power = Some(a),
                Err(e) => warn!("hal: cannot keep the system awake: {e}"),
            }
        }
        let watcher = watch_device(Arc::downgrade(inner), st.epoch, observer, engine, changes);
        st.watcher = Some(inner.runtime.spawn(watcher));
    }

    /// The engine stopped: clears ENGINE_RUNNING and the audio word, stops
    /// watching the device and releases the power assertion. The
    /// configuration stays on offer.
    pub fn engine_stopped(&self) {
        let mut st = self.inner.lock();
        st.epoch += 1;
        if let Some(w) = st.watcher.take() {
            w.abort();
        }
        st.observer = None;
        st.power = None;
        if std::mem::replace(&mut st.running, false) {
            info!("hal: engine stopped");
        }
        self.inner.write_engine_words(&st);
    }

    /// The driver link at a glance.
    pub fn status(&self) -> HalStatus {
        self.inner.status()
    }

    /// Stops the service politely: sets SHUTTING_DOWN, clears
    /// ENGINE_RUNNING, sends every driver a bye, releases the power
    /// assertion, then closes the connections.
    pub async fn shutdown(mut self) {
        let peers = {
            let mut st = self.inner.lock();
            st.closing = true;
            st.epoch += 1;
            st.running = false;
            if let Some(w) = st.watcher.take() {
                w.abort();
            }
            st.observer = None;
            st.power = None;
            let d = self.inner.region.view().daemon();
            d.flags.fetch_or(DAEMON_SHUTTING_DOWN, Ordering::Release);
            self.inner.write_engine_words(&st);
            for id in st.peers.keys() {
                self.inner.send(*id, ToPlugin::Bye { reason: "daemon shutting down".to_owned() });
            }
            st.peers.len()
        };
        if let Some(t) = self.status_task.take() {
            t.abort();
        }
        if peers > 0 {
            tokio::time::sleep(BYE_GRACE).await;
        }
        self.transport = None;
        info!("hal: service stopped");
    }
}

impl Drop for HalServer {
    fn drop(&mut self) {
        if let Some(t) = self.status_task.take() {
            t.abort();
        }
        {
            let mut st = self.inner.lock();
            st.epoch += 1;
            if let Some(w) = st.watcher.take() {
                w.abort();
            }
            st.observer = None;
            st.power = None;
        }
        // Closes the listener and every connection; the transport drops its
        // reference to the handler.
        self.transport = None;
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Sends `m` to `peer`, unless the service is gone.
    fn send(&self, peer: u64, m: ToPlugin) {
        if let Some(t) = self.transport.get().and_then(Weak::upgrade) {
            t.send(peer, m);
        }
    }

    /// Takes a new description of the running engine. When the
    /// configuration built from it differs from the current one, it gets
    /// the next generation and goes to every attached driver. The engine
    /// words are rewritten either way.
    fn update_engine(&self, st: &mut State, e: EngineInfo) {
        let mut changed = false;
        match driver_config(&e, st.config.config_gen, &self.opts) {
            Ok(cfg) => {
                st.engine_ok = true;
                if cfg != st.config {
                    let structural = !cfg.structural_eq(&st.config);
                    let config_gen = st.config.config_gen + 1;
                    st.config = DriverConfig { config_gen, ..cfg };
                    changed = true;
                    if structural {
                        info!(
                            "hal: driver configuration {config_gen}: {} Hz, {} in, {} out",
                            st.config.sample_rate,
                            st.config.input_channels,
                            st.config.output_channels
                        );
                    } else {
                        info!("hal: driver configuration {config_gen}: names changed");
                    }
                }
            }
            Err(err) => {
                st.engine_ok = false;
                error!("hal: the engine cannot be offered to the plug-in: {err}");
            }
        }
        st.engine = e;
        // The words first, so that a driver applying the new configuration
        // already finds the engine they describe.
        self.write_engine_words(st);
        if changed {
            for (id, p) in &st.peers {
                if p.welcomed {
                    self.send(*id, ToPlugin::Config(st.config.clone()));
                }
            }
        }
    }

    /// Writes the engine words of the daemon status: the audio, channel and
    /// guard words and ENGINE_RUNNING while a usable engine runs; a zero
    /// audio word and the flag cleared otherwise.
    fn write_engine_words(&self, st: &State) {
        let d = self.region.view().daemon();
        if st.running && st.engine_ok && !st.closing {
            let e = &st.engine;
            let channels = ChannelsWord {
                rx: u16::try_from(e.rx_names.len()).unwrap_or(u16::MAX),
                tx: u16::try_from(e.tx_names.len()).unwrap_or(u16::MAX),
                latency_samples: e.latency_samples,
            };
            let audio =
                AudioWord { sample_rate: e.sample_rate, config_gen: st.config.config_gen as u32 };
            d.tx_guard_samples.store(e.tx_guard_samples as u64, Ordering::Relaxed);
            d.channels_word.store(channels.pack(), Ordering::Relaxed);
            d.audio_word.store(audio.pack(), Ordering::Release);
            d.flags.fetch_or(DAEMON_ENGINE_RUNNING, Ordering::Release);
        } else {
            d.flags.fetch_and(!DAEMON_ENGINE_RUNNING, Ordering::Release);
            d.audio_word.store(0, Ordering::Release);
        }
    }

    /// One heartbeat: the device's counters, the peer count and the time.
    pub(crate) fn beat(&self) {
        let observer = {
            let st = self.lock();
            self.store_peers(&st);
            st.observer.clone()
        };
        let d = self.region.view().daemon();
        if let Some(stats) = observer.and_then(|o| o.stats()) {
            d.tx_packets.store(stats.tx_packets, Ordering::Relaxed);
            d.tx_underruns.store(stats.tx_underruns, Ordering::Relaxed);
            d.rx_packets.store(stats.rx_packets, Ordering::Relaxed);
            d.rx_late_packets.store(stats.rx_late_packets, Ordering::Relaxed);
        }
        d.heartbeat_ns.store(local_now_ns(), Ordering::Release);
    }

    pub(crate) fn status(&self) -> HalStatus {
        let (peers, running, config_gen) = {
            let st = self.lock();
            (welcomed(&st), st.running && st.engine_ok, st.config.config_gen)
        };
        HalStatus::read(self.region.view(), peers, running, config_gen)
    }

    fn store_peers(&self, st: &State) {
        self.region.view().daemon().peers.store(welcomed(st) as u64, Ordering::Relaxed);
    }

    fn hello(&self, peer: u64, h: Hello) -> Option<ToPlugin> {
        let mut st = self.lock();
        if st.closing {
            return None;
        }
        let config = st.config.clone();
        let p = st.peers.get_mut(&peer)?;
        let pid = p.info.pid;
        let refusal = if h.proto_major != PROTO_MAJOR {
            Some((
                RejectReason::Proto,
                format!(
                    "the daemon speaks protocol {PROTO_MAJOR}.{PROTO_MINOR}, the plug-in {}.{}",
                    h.proto_major, h.proto_minor
                ),
            ))
        } else if h.layout_version != LAYOUT_VERSION || h.layout_hash != LAYOUT_HASH {
            Some((
                RejectReason::Layout,
                format!(
                    "the daemon's shared memory layout is {LAYOUT_VERSION}/{LAYOUT_HASH:016x}, \
                     the plug-in's {}/{:016x}",
                    h.layout_version, h.layout_hash
                ),
            ))
        } else {
            None
        };
        if let Some((reason, message)) = refusal {
            warn!("hal: refused plug-in {} (pid {pid}): {message}", h.plugin_version);
            p.welcomed = false;
            self.store_peers(&st);
            return Some(ToPlugin::Reject(Reject {
                reason,
                proto_major: PROTO_MAJOR,
                layout_version: LAYOUT_VERSION,
                layout_hash: LAYOUT_HASH,
                message,
            }));
        }
        if h.arch != HOST_ARCH {
            info!("hal: plug-in (pid {pid}) runs on another architecture ({})", h.arch);
        }
        info!(
            "hal: plug-in attached: pid {pid}, euid {}, instance {:016x}, version {}, \
             configuration {}",
            p.info.euid, h.instance, h.plugin_version, config.config_gen
        );
        p.welcomed = true;
        p.hello = Some(h);
        self.store_peers(&st);
        Some(ToPlugin::Welcome(Welcome {
            proto_major: PROTO_MAJOR,
            proto_minor: PROTO_MINOR,
            daemon_version: self.version.clone(),
            daemon_generation: self.region.generation(),
            region: self.region.handle(),
            region_size: REGION_SIZE as u64,
            config,
        }))
    }

    fn config_applied(&self, peer: u64, a: ConfigApplied) {
        let mut st = self.lock();
        let current = st.config.config_gen;
        let generation = self.region.generation();
        let Some(p) = st.peers.get_mut(&peer) else { return };
        info!(
            "hal: plug-in (pid {}) applied configuration {}: {} Hz, {} in, {} out",
            p.info.pid, a.config_gen, a.sample_rate, a.input_channels, a.output_channels
        );
        // A configuration pushed while the welcome was on its way can
        // arrive before it; the driver then applies the welcome's older one.
        // Send the current one again, once per generation.
        if a.daemon_generation == generation
            && a.config_gen < current
            && p.welcomed
            && p.resent_gen < current
        {
            p.resent_gen = current;
            debug!(
                "hal: plug-in (pid {}) is behind; resending configuration {current}",
                p.info.pid
            );
            self.send(peer, ToPlugin::Config(st.config.clone()));
        }
    }
}

/// Driver connections that got a welcome.
fn welcomed(st: &State) -> usize {
    st.peers.values().filter(|p| p.welcomed).count()
}

impl ServerHandler for Inner {
    fn on_peer(&self, p: PeerInfo) -> bool {
        if !self.opts.allowed_uids.contains(&p.euid) {
            warn!("hal: refused a connection from pid {} (euid {} not allowed)", p.pid, p.euid);
            return false;
        }
        let mut st = self.lock();
        if st.closing {
            return false;
        }
        debug!("hal: connection {} from pid {} (euid {})", p.id, p.pid, p.euid);
        st.peers.insert(p.id, Peer { info: p, hello: None, welcomed: false, resent_gen: 0 });
        true
    }

    fn on_request(&self, peer: u64, m: ToDaemon) -> Option<ToPlugin> {
        match m {
            ToDaemon::Hello(h) => self.hello(peer, h),
            ToDaemon::ConfigApplied(a) => {
                self.config_applied(peer, a);
                None
            }
        }
    }

    fn on_message(&self, peer: u64, m: ToDaemon) {
        match m {
            ToDaemon::ConfigApplied(a) => self.config_applied(peer, a),
            // Only answered when sent with a reply.
            ToDaemon::Hello(_) => debug!("hal: ignored a hello without a reply from {peer}"),
        }
    }

    fn on_peer_gone(&self, peer: u64) {
        let mut st = self.lock();
        if let Some(p) = st.peers.remove(&peer) {
            match (&p.hello, p.welcomed) {
                (Some(h), true) => {
                    info!("hal: plug-in detached: pid {}, instance {:016x}", p.info.pid, h.instance)
                }
                _ => debug!("hal: connection {peer} closed"),
            }
        }
        self.store_peers(&st);
    }
}

/// Follows `observer`'s device: on every change of its names, rebuilds the
/// configuration (pushed only if it changed). Ends with the device, the
/// server, or the next engine start or stop.
async fn watch_device(
    server: Weak<Inner>,
    epoch: u64,
    observer: DeviceObserver,
    engine: EngineInfo,
    mut changes: watch::Receiver<u64>,
) {
    while changes.changed().await.is_ok() {
        let (Some((rx_names, tx_names)), Some(device_name)) =
            (observer.channel_names(), observer.name())
        else {
            return;
        };
        let Some(inner) = server.upgrade() else { return };
        let mut st = inner.lock();
        if st.epoch != epoch {
            return;
        }
        let e = EngineInfo { rx_names, tx_names, device_name, ..engine.clone() };
        inner.update_engine(&mut st, e);
    }
}
