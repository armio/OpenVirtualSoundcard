//! Receive side: turns subscriptions into flows and flows into samples.
//!
//! The [`RxManager`] is a reconcile loop. Whenever subscriptions change (and
//! once a second anyway) it compares what controllers asked for with the
//! flows it has, and fixes the difference:
//!
//! 1. routes that no longer match a subscription are removed;
//! 2. flows that carry nothing, or stopped delivering packets, are stopped;
//! 3. subscribed channels already carried by a flow from the right
//!    transmitter are routed to it;
//! 4. the remaining ones are resolved (mDNS), grouped per transmitter and
//!    requested as new unicast flows over flow control;
//! 5. per-channel statuses are published for ARC listings.
//!
//! Each flow has a small tokio task that receives packets, writes the samples
//! into the receive rings at their media timestamp and sends keepalives back
//! to the transmitter.

use std::collections::{BTreeMap, HashMap};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use ovsc_clock::local_now_ns;
use ovsc_proto::arc::{RxFlowInfo, SubscriptionStatus};
use ovsc_proto::audio::{self, AudioPacket, Sample, SampleFormat};
use ovsc_proto::dbcp::{self, FlowHandle, FlowRequest};
use ovsc_proto::discovery::ChannelTxt;

use crate::buffer::TimedRing;
use crate::client;
use crate::directory::{Directory, ResolvedChannel};
use crate::info::{FPP_MAX, FPP_MIN, MAX_CHANNELS_PER_TX_FLOW, MAX_RX_FLOWS, PCM_TYPE};
use crate::net::bind_udp;
use crate::state::{Counters, FlowStats, Notify, Shared};
use crate::{Error, Result};

const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(250);
/// A flow that delivers nothing for this long is torn down and re-requested.
const FLOW_TIMEOUT_NS: u64 = 3_000_000_000;
const RETRY_UNRESOLVED: Duration = Duration::from_secs(2);
const RETRY_REFUSED: Duration = Duration::from_secs(5);

/// Slot → local receive channels fed by that slot.
type Routing = Arc<RwLock<Vec<Vec<usize>>>>;
/// A resolved transmit channel and the local channels that want it.
type Pending = (ResolvedChannel, Vec<usize>);

struct RxFlow {
    id: u16,
    tx_device: String,
    control: SocketAddr,
    handle: FlowHandle,
    /// Transmit channel name requested for each slot.
    slot_names: Vec<String>,
    /// Transmit channel id carried in each slot.
    tx_ids: Vec<u16>,
    /// Most slots this flow may grow to.
    max_slots: usize,
    routing: Routing,
    port: u16,
    /// Local time of the last packet, 0 if none yet.
    last_packet_ns: Arc<AtomicU64>,
    /// When its packets arrive.
    stats: Arc<FlowStats>,
    created_ns: u64,
    worker: Worker,
}

impl RxFlow {
    fn routes(&self) -> Vec<Vec<usize>> {
        self.routing.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn is_receiving(&self, now_ns: u64) -> bool {
        let last = self.last_packet_ns.load(Ordering::Relaxed);
        last != 0 && now_ns.saturating_sub(last) < 1_000_000_000
    }

    fn is_dead(&self, now_ns: u64) -> bool {
        let last = self.last_packet_ns.load(Ordering::Relaxed).max(self.created_ns);
        now_ns.saturating_sub(last) > FLOW_TIMEOUT_NS
    }
}

pub struct RxManager {
    shared: Arc<Shared>,
    directory: Directory,
    flows: Vec<RxFlow>,
    /// Failed channels: status to report and when to try again.
    failures: HashMap<usize, (SubscriptionStatus, tokio::time::Instant)>,
    /// Cloned into every receive thread, which keeps it until it exits; see
    /// [`RxManager::new`].
    threads: mpsc::Sender<()>,
}

impl RxManager {
    /// `threads` is never sent on: its receiver sees the channel close once
    /// the manager and every receive thread it started are gone, and with
    /// them every reference they held to the receive rings.
    pub fn new(shared: Arc<Shared>, directory: Directory, threads: mpsc::Sender<()>) -> Self {
        Self { shared, directory, flows: Vec::new(), failures: HashMap::new(), threads }
    }

    pub async fn run(mut self, mut shutdown: oneshot::Receiver<oneshot::Sender<()>>) {
        let mut watch = self.shared.watch();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = watch.changed() => {
                    // Controllers often send related changes back to back;
                    // batch them into one pass so they can share flows.
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    watch.borrow_and_update();
                }
                _ = tick.tick() => {}
                done = &mut shutdown => {
                    self.stop_all().await;
                    if let Ok(done) = done {
                        let _ = done.send(());
                    }
                    return;
                }
            }
            self.reconcile().await;
        }
    }

    async fn stop_all(&mut self) {
        let ip = self.shared.info.iface.ip;
        for flow in self.flows.drain(..) {
            flow.worker.stop();
            let _ = client::stop_flow(ip, flow.control, flow.handle).await;
        }
        self.shared.state().rx_flows.clear();
    }

    /// The subscriptions controllers asked for, per receive channel.
    fn desired(&self) -> Vec<Option<(String, String)>> {
        self.shared
            .state()
            .subscriptions
            .iter()
            .map(|s| s.as_ref().map(|s| (s.tx_channel.clone(), s.tx_device.clone())))
            .collect()
    }

    pub async fn reconcile(&mut self) {
        let desired = self.desired();
        let now = local_now_ns();
        let matches = |ch: usize, tx_channel: &str, tx_device: &str| {
            desired[ch].as_ref().is_some_and(|(c, d)| {
                c.eq_ignore_ascii_case(tx_channel) && d.eq_ignore_ascii_case(tx_device)
            })
        };

        // 1. Remove stale routes.
        for flow in &self.flows {
            let mut routing = flow.routing.write().unwrap_or_else(|e| e.into_inner());
            for (slot, targets) in routing.iter_mut().enumerate() {
                targets.retain(|&ch| matches(ch, &flow.slot_names[slot], &flow.tx_device));
            }
        }

        // 2. Stop flows that carry nothing or died.
        let ip = self.shared.info.iface.ip;
        let mut kept = Vec::new();
        for flow in self.flows.drain(..) {
            let empty = flow.routes().iter().all(Vec::is_empty);
            let dead = flow.is_dead(now);
            if empty || dead {
                if dead {
                    warn!("rx flow {} from {} stopped delivering audio", flow.id, flow.tx_device);
                } else {
                    info!("rx flow {} from {} no longer needed", flow.id, flow.tx_device);
                }
                flow.worker.stop();
                let (control, handle) = (flow.control, flow.handle);
                tokio::spawn(async move {
                    let _ = client::stop_flow(ip, control, handle).await;
                });
            } else {
                kept.push(flow);
            }
        }
        self.flows = kept;

        // 3. Route channels to existing flows where possible.
        let routed = |flows: &[RxFlow], ch: usize| {
            flows.iter().any(|f| f.routes().iter().any(|t| t.contains(&ch)))
        };
        let mut pending: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
        let tnow = tokio::time::Instant::now();
        self.failures.retain(|&ch, _| desired[ch].is_some());
        for (ch, want) in desired.iter().enumerate() {
            let Some((tx_channel, tx_device)) = want else { continue };
            if routed(&self.flows, ch) {
                continue;
            }
            let existing = self.flows.iter().find_map(|f| {
                if !f.tx_device.eq_ignore_ascii_case(tx_device) {
                    return None;
                }
                let slot = f.slot_names.iter().position(|n| n.eq_ignore_ascii_case(tx_channel))?;
                Some((f.routing.clone(), slot))
            });
            if let Some((routing, slot)) = existing {
                routing.write().unwrap_or_else(|e| e.into_inner())[slot].push(ch);
                continue;
            }
            if self.failures.get(&ch).is_some_and(|(_, retry)| *retry > tnow) {
                continue;
            }
            let key = (tx_channel.to_ascii_lowercase(), tx_device.to_ascii_lowercase());
            pending.entry(key).or_default().push(ch);
        }

        // 4. Resolve and request new flows.
        if !pending.is_empty() {
            self.subscribe(pending).await;
        }

        // 5. Publish statuses and flow info.
        self.publish(&desired);
    }

    async fn subscribe(&mut self, pending: BTreeMap<(String, String), Vec<usize>>) {
        let own = self.shared.name();
        let mut lookups = JoinSet::new();
        for ((channel, device), chans) in pending {
            // Use the spelling from the subscription for lookups.
            let (channel, device) = self.desired()[chans[0]].clone().unwrap_or((channel, device));
            if device.eq_ignore_ascii_case(&own) {
                let r = self.resolve_self(&channel);
                lookups.spawn(async move { (chans, r) });
            } else {
                let directory = self.directory.clone();
                lookups.spawn(async move { (chans, directory.resolve(&channel, &device).await) });
            }
        }

        let mut per_tx: BTreeMap<(Ipv4Addr, u16), Vec<Pending>> = BTreeMap::new();
        let retry = tokio::time::Instant::now();
        while let Some(joined) = lookups.join_next().await {
            let Ok((chans, result)) = joined else { continue };
            match result {
                Ok(r) if r.txt.sample_rate != self.shared.info.sample_rate => {
                    warn!(
                        "{}@{} runs at {} Hz, we run at {} Hz",
                        r.channel, r.device, r.txt.sample_rate, self.shared.info.sample_rate
                    );
                    for ch in chans {
                        self.failures
                            .insert(ch, (SubscriptionStatus::TxFail, retry + RETRY_REFUSED));
                    }
                }
                Ok(r) => per_tx.entry((r.addr, r.flow_control_port)).or_default().push((r, chans)),
                Err(e) => {
                    debug!("cannot resolve channel: {e}");
                    for ch in chans {
                        self.failures
                            .insert(ch, (SubscriptionStatus::Unresolved, retry + RETRY_UNRESOLVED));
                    }
                }
            }
        }

        for ((addr, port), mut entries) in per_tx {
            let control = SocketAddr::from((addr, port));
            self.grow_flows(control, &mut entries).await;
            if entries.is_empty() {
                continue;
            }
            let per_flow = self.slots_per_flow(&entries[0].0.txt);
            for chunk in entries.chunks(per_flow) {
                let chans: Vec<usize> = chunk.iter().flat_map(|(_, c)| c.clone()).collect();
                match self.create_flow(control, chunk, per_flow).await {
                    Ok(flow) => {
                        info!(
                            "rx flow {} from {} ({} channels) established",
                            flow.id,
                            flow.tx_device,
                            flow.slot_names.iter().filter(|n| !n.is_empty()).count()
                        );
                        for ch in &chans {
                            self.failures.remove(ch);
                        }
                        self.flows.push(flow);
                    }
                    Err(e) => {
                        warn!("flow request to {addr}:{port} failed: {e}");
                        let status = match e {
                            Error::Refused(dbcp::error::TOO_MANY_FLOWS) => {
                                SubscriptionStatus::TxNoFlows
                            }
                            _ => SubscriptionStatus::TxFail,
                        };
                        for ch in chans {
                            self.failures.insert(ch, (status, retry + RETRY_REFUSED));
                        }
                    }
                }
            }
        }
    }

    fn slots_per_flow(&self, txt: &ChannelTxt) -> usize {
        txt.nchan.max(1).min(self.shared.info.max_channels_per_rx_flow) as usize
    }

    /// Adds channels to existing flows from the same transmitter, reusing
    /// slots nobody listens to any more and appending up to the flow's
    /// limit, so that transmitters (which may only have a few flows) aren't
    /// asked for a new flow per channel. Entries placed are removed; those a
    /// transmitter refuses to add stay, for a flow of their own.
    async fn grow_flows(&mut self, control: SocketAddr, entries: &mut Vec<Pending>) {
        let ip = self.shared.info.iface.ip;
        for flow in self.flows.iter_mut().filter(|f| f.control == control) {
            if entries.is_empty() {
                return;
            }
            let mut ids = flow.tx_ids.clone();
            let mut names = flow.slot_names.clone();
            let mut routes = flow.routes();
            let mut placed = Vec::new();
            let mut taken = Vec::new();
            while !entries.is_empty() {
                let slot = match routes.iter().position(Vec::is_empty) {
                    Some(slot) => slot,
                    None if ids.len() < flow.max_slots => {
                        ids.push(0);
                        names.push(String::new());
                        routes.push(Vec::new());
                        ids.len() - 1
                    }
                    None => break,
                };
                let (r, chans) = entries.remove(0);
                ids[slot] = r.txt.id;
                names[slot] = r.channel.clone();
                routes[slot] = chans.clone();
                placed.extend(chans.iter().copied());
                taken.push((r, chans));
            }
            if placed.is_empty() {
                continue;
            }
            match client::update_flow(ip, control, flow.handle, &ids).await {
                Ok(()) => {
                    info!("rx flow {} from {} now carries {:?}", flow.id, flow.tx_device, ids);
                    flow.tx_ids = ids;
                    flow.slot_names = names;
                    *flow.routing.write().unwrap_or_else(|e| e.into_inner()) = routes;
                    for ch in &placed {
                        self.failures.remove(ch);
                    }
                }
                Err(e) => {
                    warn!(
                        "{} refused to extend rx flow {} ({e}); asking for a new flow",
                        flow.tx_device, flow.id
                    );
                    entries.extend(taken);
                }
            }
        }
    }

    /// Describes one of our own transmit channels, for self-subscriptions.
    fn resolve_self(&self, channel: &str) -> Result<ResolvedChannel> {
        let info = &self.shared.info;
        let state = self.shared.state();
        let index = state
            .tx_channel_index(info, channel)
            .ok_or_else(|| Error::NotFound(format!("own channel {channel}")))?;
        Ok(ResolvedChannel {
            device: state.name.clone(),
            channel: channel.to_owned(),
            addr: info.iface.ip,
            flow_control_port: info.ports.flow_control,
            txt: own_channel_txt(info, index as u16 + 1),
        })
    }

    async fn create_flow(
        &self,
        control: SocketAddr,
        entries: &[Pending],
        max_slots: usize,
    ) -> Result<RxFlow> {
        let info = &self.shared.info;
        let id = (1..=MAX_RX_FLOWS as u16)
            .find(|id| self.flows.iter().all(|f| f.id != *id))
            .ok_or_else(|| Error::Config("all receive flows in use".into()))?;
        let txt = &entries[0].0.txt;
        let format = SampleFormat::from_bits(txt.bits_per_sample as u32).ok_or_else(|| {
            Error::Config(format!("unsupported encoding {}", txt.bits_per_sample))
        })?;
        // Sized for the largest the flow may grow to, so packets always fit.
        let fpp = choose_fpp(info.latency_samples, txt, max_slots, format);
        // Every slot the flow may hold, unused ones 0, as Dante receivers
        // request them (1, 0, 0, 0): a transmitter fixes a flow's slot count
        // when it creates the flow, and 0x0102 then fills those slots.
        let mut channels: Vec<u16> = entries.iter().map(|(r, _)| r.txt.id).collect();
        let slots = max_slots.max(channels.len());
        channels.resize(slots, 0);
        let mut slot_routes: Vec<Vec<usize>> = entries.iter().map(|(_, c)| c.clone()).collect();
        slot_routes.resize(slots, Vec::new());
        let mut slot_names: Vec<String> = entries.iter().map(|(r, _)| r.channel.clone()).collect();
        slot_names.resize(slots, String::new());

        let socket = bind_rx_port(info.iface.ip)?;
        let port = socket.local_addr()?.port();
        let request = FlowRequest {
            rx_device: self.shared.name(),
            flow_name: format!("{id}_{}", info.process_id),
            sample_rate: info.sample_rate,
            bits_per_sample: txt.bits_per_sample as u32,
            fpp,
            channels,
            rx_addr: info.iface.ip,
            rx_port: port,
        };
        let handle = client::request_flow(info.iface.ip, control, &request).await?;

        let routing: Routing = Arc::new(RwLock::new(slot_routes));
        let last_packet_ns = Arc::new(AtomicU64::new(0));
        let stats = Arc::new(FlowStats::default());
        let worker = Worker::spawn(
            id,
            self.shared.clock.clone(),
            info.latency_samples,
            socket,
            routing.clone(),
            self.shared.rx_rings.clone(),
            fpp as usize,
            format,
            info.sample_rate,
            Probes {
                last_packet_ns: last_packet_ns.clone(),
                counters: self.shared.counters.clone(),
                stats: stats.clone(),
                override_gate: self.shared.rx_override.clone(),
            },
            self.threads.clone(),
        )?;
        Ok(RxFlow {
            id,
            tx_device: entries[0].0.device.clone(),
            control,
            handle,
            slot_names,
            tx_ids: request.channels.clone(),
            max_slots,
            routing,
            port,
            last_packet_ns,
            stats,
            created_ns: local_now_ns(),
            worker,
        })
    }

    fn publish(&self, desired: &[Option<(String, String)>]) {
        let now = local_now_ns();
        let mut status = vec![SubscriptionStatus::None; desired.len()];
        for (ch, want) in desired.iter().enumerate() {
            if want.is_some() {
                status[ch] =
                    self.failures.get(&ch).map_or(SubscriptionStatus::Unresolved, |(s, _)| *s);
            }
        }
        let mut flows_info = Vec::new();
        for flow in &self.flows {
            let routes = flow.routes();
            let receiving = flow.is_receiving(now);
            for targets in &routes {
                for &ch in targets {
                    status[ch] = if receiving {
                        SubscriptionStatus::ReceivingUnicast
                    } else {
                        SubscriptionStatus::InProgress
                    };
                }
            }
            flows_info.push(RxFlowInfo {
                id: flow.id,
                addr: self.shared.info.iface.ip,
                port: flow.port,
                latency_ns: self.shared.info.latency_ns,
                status: if receiving {
                    SubscriptionStatus::ReceivingUnicast
                } else {
                    SubscriptionStatus::InProgress
                },
                slots: routes.iter().map(|t| t.iter().map(|&c| c as u16).collect()).collect(),
            });
        }

        let mut changed = Vec::new();
        {
            let mut state = self.shared.state();
            for (ch, sub) in state.subscriptions.iter_mut().enumerate() {
                if let Some(sub) = sub {
                    if sub.status != status[ch] {
                        debug!("rx {} status {:?} -> {:?}", ch + 1, sub.status, status[ch]);
                        sub.status = status[ch];
                        changed.push(ch);
                    }
                }
            }
            state.rx_flows = flows_info;
            state.rx_flow_stats = self.flows.iter().map(|f| (f.id, f.stats.clone())).collect();
        }
        if !changed.is_empty() {
            self.shared.notify(Notify::RxChannelsChanged(changed));
        }
    }
}

/// Binds a receive socket in the unicast port range real receivers report
/// (ARC 0x3300), falling back to an ephemeral port.
fn bind_rx_port(ip: Ipv4Addr) -> std::io::Result<std::net::UdpSocket> {
    let (first, last) = ovsc_proto::arc::RX_PORT_RANGES[0];
    let span = (last - first + 1) as u32;
    let offset = rand::random::<u32>() % span;
    for i in 0..span {
        let port = first + ((offset + i) % span) as u16;
        if let Ok(s) = bind_udp(ip, port) {
            return Ok(s);
        }
    }
    bind_udp(ip, 0)
}

/// Frames per packet: about a quarter of our latency, within what the
/// transmitter supports and what fits in one datagram.
fn choose_fpp(
    latency_samples: u64,
    txt: &ChannelTxt,
    channels: usize,
    format: SampleFormat,
) -> u16 {
    let target = (latency_samples / 4).clamp(1, u16::MAX as u64) as u16;
    let mtu_limit = audio::max_fpp(channels, format).min(u16::MAX as usize) as u16;
    let max = txt.fpp_max.max(1).min(mtu_limit);
    let min = txt.fpp_min.max(1).min(max);
    target.clamp(min, max)
}

/// The TXT record we advertise for our transmit channel `id`.
pub fn own_channel_txt(info: &crate::info::DeviceInfo, id: u16) -> ChannelTxt {
    ChannelTxt {
        id,
        sample_rate: info.sample_rate,
        bits_per_sample: info.format.bits(),
        pcm_type: PCM_TYPE,
        latency_ns: info.latency_ns,
        fpp_max: FPP_MAX,
        fpp_min: FPP_MIN,
        nchan: MAX_CHANNELS_PER_TX_FLOW.min(info.tx_channels.len() as u16).max(1),
        dbcp1: ovsc_proto::frame::protocol::DBCP,
        is_default_name: true,
        multicast: None,
    }
}

/// The real-time thread receiving one flow.
///
/// Packets are written into the rings by timestamp, so receiving them late
/// only matters once they are older than the receive latency. That makes
/// scheduling delays the main cause of dropouts, hence a dedicated
/// high-priority thread per flow rather than an async task.
///
/// The thread blocks in `recv` with a timeout (the keepalive interval) on
/// every turn and must keep doing so: XNU demotes a real-time thread that
/// runs for about a second without blocking.
struct Worker {
    stop: Arc<AtomicBool>,
}

/// What a receive thread reports about its flow.
struct Probes {
    /// Local time of the last packet.
    last_packet_ns: Arc<AtomicU64>,
    /// The device's packet counters.
    counters: Arc<Counters>,
    /// The flow's arrival times.
    stats: Arc<FlowStats>,
    override_gate: Arc<RwLock<bool>>,
}

impl Worker {
    #[allow(clippy::too_many_arguments)]
    fn spawn(
        id: u16,
        clock: ovsc_clock::MediaClock,
        latency_samples: u64,
        socket: std::net::UdpSocket,
        routing: Routing,
        rings: Vec<Arc<TimedRing>>,
        fpp: usize,
        format: SampleFormat,
        sample_rate: u32,
        probes: Probes,
        alive: mpsc::Sender<()>,
    ) -> std::io::Result<Self> {
        socket.set_read_timeout(Some(KEEPALIVE_INTERVAL))?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        std::thread::Builder::new().name(format!("ovsc-rx{id}")).spawn(move || {
            // Dropped last, once `receive` has dropped the rings.
            let _alive = alive;
            crate::rt::raise_priority("receive", crate::rt::RtClass::Receive);
            receive(
                id,
                socket,
                routing,
                rings,
                fpp,
                format,
                sample_rate,
                (clock, latency_samples),
                probes,
                flag,
            )
        })?;
        Ok(Self { stop })
    }

    /// Asks the thread to exit; it does within one keepalive interval.
    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Receives one flow's packets into the rings and keeps the flow alive.
#[allow(clippy::too_many_arguments)]
fn receive(
    id: u16,
    socket: std::net::UdpSocket,
    routing: Routing,
    rings: Vec<Arc<TimedRing>>,
    fpp: usize,
    format: SampleFormat,
    sample_rate: u32,
    (clock, latency_samples): (ovsc_clock::MediaClock, u64),
    Probes { last_packet_ns, counters, stats, override_gate }: Probes,
    stop: Arc<AtomicBool>,
) {
    let mut buf = vec![0u8; 2048];
    let mut samples: Vec<Sample> = vec![0; 2048];
    let mut source: Option<SocketAddr> = None;
    let mut next_keepalive = local_now_ns();
    let interval = KEEPALIVE_INTERVAL.as_nanos() as u64;
    // Diagnostics: where the next packet should start, and how late packets
    // arrive relative to their timestamp.
    let mut expected: Option<u64> = None;
    let latency = latency_samples;
    while !stop.load(Ordering::Relaxed) {
        if let Ok((len, src)) = socket.recv_from(&mut buf) {
            // The channel count follows from the size: it changes when the
            // flow is updated, and packets in flight still have the old
            // layout.
            let slots = len.saturating_sub(audio::HEADER_LEN) / (fpp * format.bytes());
            if let Ok(packet) = AudioPacket::parse(&buf[..len], slots, format) {
                source = Some(src);
                counters.rx_packets.fetch_add(1, Ordering::Relaxed);
                let ts = packet.timestamp(sample_rate);
                let frames = packet.frames() as u64;
                if let Some(exp) = expected.filter(|&e| e != ts) {
                    debug!(
                        "rx{id}: timestamp discontinuity of {} samples (lost packets or \
                         transmitter pause)",
                        ts as i64 - exp as i64
                    );
                }
                expected = Some(ts + frames);
                if let Some(now) = clock.now_samples(sample_rate) {
                    // Its first sample plays `latency` samples after its
                    // timestamp, so the packet must be here by then.
                    let arrival = now.saturating_sub(ts);
                    stats
                        .max_arrival
                        .fetch_max(arrival.min(u64::from(u32::MAX)) as u32, Ordering::Relaxed);
                    if arrival > latency {
                        counters.rx_late_packets.fetch_add(1, Ordering::Relaxed);
                        stats.late_packets.fetch_add(1, Ordering::Relaxed);
                        debug!(
                            "rx{id}: packet arrived {arrival} samples after its timestamp \
                             (latency {latency})"
                        );
                    }
                }
                // Hold the read guard through writing, so handoff to playback
                // cannot race an in-flight live packet into the shared rings.
                let overridden = override_gate.read().unwrap_or_else(|e| e.into_inner());
                let routes = routing.read().unwrap_or_else(|e| e.into_inner());
                for (slot, targets) in
                    routes.iter().enumerate().take(slots).filter(|_| !*overridden)
                {
                    if targets.is_empty() {
                        continue;
                    }
                    let n = packet.read_channel(slot, &mut samples);
                    for &ch in targets {
                        if let Some(ring) = rings.get(ch) {
                            ring.write(ts, &samples[..n]);
                        }
                    }
                }
                last_packet_ns.store(local_now_ns(), Ordering::Relaxed);
            }
        }
        let now = local_now_ns();
        if now >= next_keepalive {
            if let Some(src) = source {
                let _ = socket.send_to(&audio::KEEPALIVE, src);
            }
            next_keepalive = now + interval;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fpp_choice() {
        let txt = ChannelTxt {
            id: 1,
            sample_rate: 48_000,
            bits_per_sample: 24,
            pcm_type: 0x0e,
            latency_ns: 1_000_000,
            fpp_max: 32,
            fpp_min: 4,
            nchan: 8,
            dbcp1: 0x1102,
            is_default_name: true,
            multicast: None,
        };
        // 4 ms at 48 kHz → 48, capped by the transmitter's 32.
        assert_eq!(choose_fpp(192, &txt, 2, SampleFormat::S24), 32);
        // 0.25 ms → 3, raised to the transmitter's minimum.
        assert_eq!(choose_fpp(12, &txt, 2, SampleFormat::S24), 4);
        // 1 ms → 12.
        assert_eq!(choose_fpp(48, &txt, 2, SampleFormat::S24), 12);
        // Wide flows are limited by the datagram size.
        let wide = ChannelTxt { fpp_max: 256, ..txt };
        assert_eq!(choose_fpp(4800, &wide, 64, SampleFormat::S32), 5);
    }
}
