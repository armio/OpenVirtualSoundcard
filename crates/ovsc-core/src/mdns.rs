//! A small mDNS responder and resolver tailored to Dante discovery.
//!
//! General-purpose mDNS libraries make assumptions Dante peers don't share,
//! so this one follows observed Dante behaviour instead:
//!
//! * queries for a service *instance* (`01@desk._netaudio-chan._udp.local`)
//!   are answered directly, not only PTR browses for the service type;
//! * all related records (PTR, SRV, TXT, A) go into the *answer* section;
//! * instance names keep `@`, spaces and case exactly as advertised.
//!
//! Names are claimed as RFC 6762 §8.1 describes: [`Mdns::probe`] asks
//! whether another host already uses a device's names before they are
//! announced, and the caller takes another name if one does. A conflict
//! that appears once a name is announced does *not* rename the device, as
//! RFC 6762 §9 would: every subscription controllers made to the device
//! would break, mid-show. It is logged and reported instead
//! ([`Mdns::conflicts`]), and our records are announced once more, so that
//! peers' caches hold them again and a host that does follow §9 gives the
//! name up. Not implemented: the tie-break between two hosts that probe for
//! the same name at the same moment (RFC 6762 §8.2); both then announce it
//! and report the conflict.

use std::collections::HashSet;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::{Notify, watch};
use tracing::{debug, error, info, trace, warn};

use ovsc_proto::discovery::{self, ChannelTxt, MAX_NAME_LEN};
use ovsc_proto::dns::{Message, Name, Question, RData, Record, rtype};

use crate::directory::ResolvedChannel;
use crate::net::{bind_multicast, into_tokio};
use crate::{Error, Result};

pub const MDNS_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
pub const MDNS_PORT: u16 = 5353;
/// Keep packets well below a typical Ethernet MTU.
const MAX_PACKET: usize = 1400;
/// TTL of host-related records (SRV, A), per RFC 6762.
pub const TTL_HOST: u32 = 120;
/// TTL of other records (PTR, TXT), as used by Dante devices.
pub const TTL_OTHER: u32 = 4500;
/// RFC 6762 §8.1: three probes, 250 ms apart, after a random delay of up to
/// 250 ms. A name nobody claimed within 250 ms of the last probe is ours.
const PROBES: usize = 3;
const PROBE_INTERVAL: Duration = Duration::from_millis(250);
/// How long a conflict lasts after the other host last announced the name:
/// the TTL of host records, after which peers forget its address and SRV
/// records, the ones subscriptions follow.
const CONFLICT_LIFETIME: Duration = Duration::from_secs(TTL_HOST as u64);
/// How often conflicts are checked: expired ones cleared, the names of those
/// about to expire asked about again (as RFC 6762 §5.2 has a cache refresh
/// its records), so that a host still using our name stays reported.
const CONFLICT_SWEEP: Duration = Duration::from_secs(5);
/// When, before a conflict expires, its name is asked about: two windows of
/// one sweep each.
const CONFLICT_REFRESH: [(Duration, Duration); 2] = [
    (Duration::from_secs(20), Duration::from_secs(25)),
    (Duration::from_secs(5), Duration::from_secs(10)),
];
/// RFC 6762 §10.2's one second: a record announced with cache-flush makes
/// the cached records of its name and type obsolete if they were received
/// more than this long before it, and they expire this long after it.
const FLUSH_WINDOW: Duration = Duration::from_secs(1);

/// A service instance found by [`Mdns::browse`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceInstance {
    /// Instance label, e.g. `studio-mac` or `01@studio-mac`.
    pub instance: String,
    pub host: Name,
    pub addr: Option<Ipv4Addr>,
    pub port: u16,
    pub txt: Vec<Vec<u8>>,
}

/// Another host announcing a record under one of our names (RFC 6762 §9).
/// Peers' caches then flip between its records and ours, so a subscription
/// to the name may reach either host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    /// The name, e.g. `studio._netaudio-arc._udp.local.`.
    pub name: Name,
    /// The host that announced it.
    pub peer: Ipv4Addr,
}

/// Another device on the network uses this device's name, or did when this
/// device tried to take it. For status displays.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameConflict {
    /// The device name concerned.
    pub name: String,
    /// The other device's address.
    pub peer: Ipv4Addr,
    /// The name this device took instead, having found `name` taken before
    /// announcing it. `None`: both devices announce `name` right now.
    pub renamed_to: Option<String>,
}

impl fmt::Display for NameConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.renamed_to {
            Some(new) => write!(
                f,
                "the device at {} already uses the name {}, so this device took the name {new}",
                self.peer, self.name
            ),
            None => write!(
                f,
                "the device at {} also uses the name {}: subscriptions to that name may reach \
                 either device until one of them is renamed",
                self.peer, self.name
            ),
        }
    }
}

struct CachedRecord {
    record: Record,
    expires: Instant,
    inserted: Instant,
    /// The host that sent it.
    source: Ipv4Addr,
    /// A newer record of its name and type, announced with cache-flush,
    /// made it obsolete: it expires within [`FLUSH_WINDOW`] unless announced
    /// again, and lookups take it after the others ([`cached_records`]).
    flushed: bool,
}

/// The records we are authoritative for.
#[derive(Default)]
struct Own {
    records: Vec<Record>,
    /// Bumped whenever `records` change, so that announcing an older set
    /// stops: a superseded record set announced after its goodbye would
    /// sit in peers' caches for its whole TTL.
    generation: u64,
}

/// A probe in progress: the names asked about, and the first other host
/// found using one.
struct Probe {
    proposed: Vec<Record>,
    found: Option<Conflict>,
}

struct Inner {
    socket: UdpSocket,
    /// Where queries and announcements go: the mDNS group (tests: a unicast
    /// address).
    dest: SocketAddr,
    task: Mutex<Option<tokio::task::AbortHandle>>,
    own: Mutex<Own>,
    cache: Mutex<Vec<CachedRecord>>,
    updated: Notify,
    probe: Mutex<Option<Probe>>,
    /// Conflicts with our announced names, and when each expires.
    conflicts: Mutex<Vec<(Conflict, Instant)>>,
    conflicts_seen: watch::Sender<Vec<Conflict>>,
}

/// Handle to the mDNS service. Cheap to clone.
#[derive(Clone)]
pub struct Mdns {
    inner: Arc<Inner>,
}

impl Mdns {
    /// Starts the responder/resolver on the interface with address
    /// `iface_ip`. Must be called inside a tokio runtime.
    pub fn start(iface_ip: Ipv4Addr) -> Result<Self> {
        let socket = into_tokio(bind_multicast(MDNS_GROUP, MDNS_PORT, iface_ip)?)?;
        Ok(Self::with_socket(socket, multicast_dest()))
    }

    fn with_socket(socket: UdpSocket, dest: SocketAddr) -> Self {
        let inner = Arc::new(Inner {
            socket,
            dest,
            task: Mutex::new(None),
            own: Mutex::new(Own::default()),
            cache: Mutex::new(Vec::new()),
            updated: Notify::new(),
            probe: Mutex::new(None),
            conflicts: Mutex::new(Vec::new()),
            conflicts_seen: watch::channel(Vec::new()).0,
        });
        let task = tokio::spawn(run(inner.clone()));
        *lock(&inner.task) = Some(task.abort_handle());
        Self { inner }
    }

    /// Stops answering and resolving. Call [`Mdns::goodbye`] first to
    /// withdraw our records from peers' caches.
    pub fn stop(&self) {
        lock(&self.inner.own).generation += 1;
        if let Some(task) = lock(&self.inner.task).take() {
            task.abort();
        }
    }

    /// Replaces the set of records we are authoritative for. New records are
    /// announced; records that disappeared get a goodbye (TTL 0). Probe new
    /// names first ([`Mdns::probe`]).
    pub async fn set_records(&self, records: Vec<Record>) {
        let (old, generation) = {
            let mut own = lock(&self.inner.own);
            own.generation += 1;
            (std::mem::replace(&mut own.records, records.clone()), own.generation)
        };
        // Conflicts over names we no longer hold are over.
        retain_conflicts(&self.inner, |c| is_unique_name(&records, &c.name));
        let gone: Vec<Record> = old
            .into_iter()
            .filter(|r| !records.contains(r))
            .map(|mut r| {
                r.ttl = 0;
                r
            })
            .collect();
        if !gone.is_empty() {
            self.send_unsolicited(gone).await;
        }
        let mdns = self.clone();
        tokio::spawn(async move {
            // RFC 6762 §8.3: announce at least twice, one second apart.
            for _ in 0..2 {
                for msg in split_response(records.clone(), Message::response()) {
                    if lock(&mdns.inner.own).generation != generation {
                        return;
                    }
                    send(&mdns.inner.socket, &msg, mdns.inner.dest).await;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }

    /// Withdraws all our records.
    pub async fn goodbye(&self) {
        let records = {
            let mut own = lock(&self.inner.own);
            own.generation += 1;
            std::mem::take(&mut own.records)
        };
        retain_conflicts(&self.inner, |_| false);
        let gone = records
            .into_iter()
            .map(|mut r| {
                r.ttl = 0;
                r
            })
            .collect();
        self.send_unsolicited(gone).await;
    }

    /// Asks the network whether another host already uses the names of
    /// `records` that identify a device, before they are announced (RFC
    /// 6762 §8.1), and returns the first such host. Those names are the
    /// host names and the device's own service instances. Channel instances
    /// (`<channel>@<device>`) are left out: a host can only hold one of
    /// them if it holds the device name too.
    pub async fn probe(&self, records: &[Record]) -> Option<Conflict> {
        let names = probe_names(records);
        if names.is_empty() {
            return None;
        }
        let proposed: Vec<Record> = records
            .iter()
            .filter(|r| names.iter().any(|n| n.eq_ignore_case(&r.name)))
            .cloned()
            .collect();
        *lock(&self.inner.probe) = Some(Probe { proposed: proposed.clone(), found: None });
        let _clear = ProbeGuard(&self.inner);
        // RFC 6762 §8.1: questions of type ANY, with the records we propose
        // in the authority section for the other host's tie-break.
        let mut query = Message::query(
            names
                .into_iter()
                .map(|name| Question { name, qtype: rtype::ANY, unicast_response: false })
                .collect(),
        );
        query.authorities = proposed;
        let delay = rand::random::<u64>() % PROBE_INTERVAL.as_millis() as u64;
        if let Some(c) = self.probe_result(Duration::from_millis(delay)).await {
            return Some(c);
        }
        for _ in 0..PROBES {
            send(&self.inner.socket, &query, self.inner.dest).await;
            if let Some(c) = self.probe_result(PROBE_INTERVAL).await {
                return Some(c);
            }
        }
        None
    }

    /// Waits up to `wait` for a host to claim a name being probed.
    async fn probe_result(&self, wait: Duration) -> Option<Conflict> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let notified = self.inner.updated.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let found = lock(&self.inner.probe).as_ref().and_then(|p| p.found.clone());
            if found.is_some() {
                return found;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return lock(&self.inner.probe).as_ref().and_then(|p| p.found.clone());
            }
        }
    }

    /// The other hosts announcing records under our names right now. A
    /// conflict ends when that host withdraws its record, when it has not
    /// announced it for two minutes (the name is asked about again before
    /// then), or when we give the name up.
    pub fn conflicts(&self) -> watch::Receiver<Vec<Conflict>> {
        self.inner.conflicts_seen.subscribe()
    }

    /// Whether another host currently uses one of our names.
    pub fn has_conflicts(&self) -> bool {
        !lock(&self.inner.conflicts).is_empty()
    }

    async fn send_unsolicited(&self, records: Vec<Record>) {
        announce(&self.inner, records).await;
    }

    async fn query(&self, questions: Vec<Question>) {
        send(&self.inner.socket, &Message::query(questions), self.inner.dest).await;
    }

    /// Finds the transmit channel `channel@device`.
    pub async fn resolve_channel(
        &self,
        channel: &str,
        device: &str,
        timeout: Duration,
    ) -> Result<ResolvedChannel> {
        let service = discovery::service_name(discovery::CHAN_SERVICE);
        let instance = Name::instance(&discovery::channel_instance(channel, device), &service);
        let deadline = Instant::now() + timeout;
        let mut retry = Duration::from_millis(250);
        loop {
            let notified = self.inner.updated.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let (srv, txt) = self.lookup_service(&instance);
            if let Some((port, host)) = &srv {
                let addrs = self.lookup_addrs(host);
                if let (Some(&addr), Some(txt)) = (addrs.first(), txt.as_deref()) {
                    let txt = ChannelTxt::from_entries(txt)?;
                    return Ok(ResolvedChannel {
                        device: device.to_owned(),
                        channel: channel.to_owned(),
                        addr,
                        addrs,
                        flow_control_port: *port,
                        txt,
                    });
                }
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(Error::NotFound(format!("{channel}@{device} (mDNS)")));
            }
            let mut questions = vec![
                Question { name: instance.clone(), qtype: rtype::SRV, unicast_response: false },
                Question { name: instance.clone(), qtype: rtype::TXT, unicast_response: false },
            ];
            if let Some((_, host)) = srv {
                questions.push(Question { name: host, qtype: rtype::A, unicast_response: false });
            }
            self.query(questions).await;
            let wait = retry.min(deadline - now);
            retry = (retry * 2).min(Duration::from_secs(1));
            let _ = tokio::time::timeout(wait, notified).await;
        }
    }

    /// Finds the transmit channel `channel@device` as *another* host
    /// announces it, for a `device` that is our own name too: the channel of
    /// a device that uses our name. Returns `None` at once while no host
    /// conflicts with our names; otherwise asks the network for up to
    /// `timeout`.
    pub async fn resolve_foreign_channel(
        &self,
        channel: &str,
        device: &str,
        timeout: Duration,
    ) -> Option<ResolvedChannel> {
        if lock(&self.inner.conflicts).is_empty() {
            return None;
        }
        let service = discovery::service_name(discovery::CHAN_SERVICE);
        let instance = Name::instance(&discovery::channel_instance(channel, device), &service);
        let deadline = Instant::now() + timeout;
        loop {
            let notified = self.inner.updated.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if let Some((port, addrs, txt)) = self.lookup_foreign_service(&instance) {
                let addr = addrs[0];
                match ChannelTxt::from_entries(&txt) {
                    Ok(txt) => {
                        return Some(ResolvedChannel {
                            device: device.to_owned(),
                            channel: channel.to_owned(),
                            addr,
                            addrs,
                            flow_control_port: port,
                            txt,
                        });
                    }
                    Err(e) => debug!("{channel}@{device} at {addr}: unusable TXT record: {e}"),
                }
            }

            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            self.query(vec![
                Question { name: instance.clone(), qtype: rtype::SRV, unicast_response: false },
                Question { name: instance.clone(), qtype: rtype::TXT, unicast_response: false },
            ])
            .await;
            let _ = tokio::time::timeout(PROBE_INTERVAL.min(deadline - now), notified).await;
        }
    }

    /// Lists instances of `service` (e.g. [`discovery::ARC_SERVICE`]) seen
    /// within `wait`.
    pub async fn browse(&self, service: &str, wait: Duration) -> Vec<ServiceInstance> {
        let service = discovery::service_name(service);
        let ptr = Question { name: service.clone(), qtype: rtype::PTR, unicast_response: false };
        self.query(vec![ptr.clone()]).await;
        tokio::time::sleep(wait / 2).await;
        self.query(vec![ptr]).await;
        tokio::time::sleep(wait / 2).await;

        let targets: Vec<Name> = {
            let cache = lock(&self.inner.cache);
            let mut seen = HashSet::new();
            cache
                .iter()
                .filter(|c| c.record.rtype == rtype::PTR && c.record.name.eq_ignore_case(&service))
                .filter_map(|c| match &c.record.data {
                    RData::Ptr(t) if seen.insert(t.to_string().to_ascii_lowercase()) => {
                        Some(t.clone())
                    }
                    _ => None,
                })
                .collect()
        };
        let mut out = Vec::new();
        for target in targets {
            let (srv, txt) = self.lookup_service(&target);
            if let Some((port, host)) = srv {
                out.push(ServiceInstance {
                    instance: target.first_label().unwrap_or_default().to_owned(),
                    addr: self.lookup_a(&host),
                    host,
                    port,
                    txt: txt.unwrap_or_default(),
                });
            }
        }
        out.sort_by_key(|a| a.instance.to_lowercase());
        out
    }

    /// Our own records plus cached ones that match, obsolete ones last (see
    /// [`cached_records`]).
    fn find(&self, name: &Name, rtype: u16) -> Vec<Record> {
        let now = Instant::now();
        let own = lock(&self.inner.own)
            .records
            .iter()
            .filter(|r| r.rtype == rtype && r.name.eq_ignore_case(name))
            .cloned()
            .collect::<Vec<_>>();
        let cache = lock(&self.inner.cache);
        let cached = cached_records(&cache, name, rtype, now).map(|c| c.record.clone());
        own.into_iter().chain(cached).collect()
    }

    /// The SRV `(port, host)` and TXT entries of a service instance.
    #[allow(clippy::type_complexity)]
    fn lookup_service(&self, instance: &Name) -> (Option<(u16, Name)>, Option<Vec<Vec<u8>>>) {
        let srv = self.find(instance, rtype::SRV).into_iter().find_map(|r| match r.data {
            RData::Srv { port, target, .. } => Some((port, target)),
            _ => None,
        });
        let txt = self.find(instance, rtype::TXT).into_iter().find_map(|r| match r.data {
            RData::Txt(t) => Some(t),
            _ => None,
        });
        (srv, txt)
    }

    fn lookup_a(&self, host: &Name) -> Option<Ipv4Addr> {
        self.find(host, rtype::A).into_iter().find_map(|r| match r.data {
            RData::A(a) => Some(a),
            _ => None,
        })
    }

    /// Every address of `host`, each once, starting with the one
    /// [`Mdns::lookup_a`] returns. A computer on two interfaces of this
    /// link, Wi-Fi and Ethernet say, has one per interface (RFC 6762 §14).
    fn lookup_addrs(&self, host: &Name) -> Vec<Ipv4Addr> {
        let mut addrs = Vec::new();
        for r in self.find(host, rtype::A) {
            match r.data {
                RData::A(a) if !addrs.contains(&a) => addrs.push(a),
                _ => {}
            }
        }
        addrs
    }

    /// The SRV port, addresses and TXT entries of `instance` as another host
    /// announces it. Its SRV record must differ from ours (our own records
    /// come back to us); its TXT and first address records are taken from
    /// the same host, where they may well equal ours. The other addresses
    /// are those of the SRV target that are not ours.
    fn lookup_foreign_service(
        &self,
        instance: &Name,
    ) -> Option<(u16, Vec<Ipv4Addr>, Vec<Vec<u8>>)> {
        let now = Instant::now();
        let own = lock(&self.inner.own);
        let cache = lock(&self.inner.cache);
        let not_ours = |c: &CachedRecord| !is_own_record(&own.records, &c.record);
        let srvs = cached_records(&cache, instance, rtype::SRV, now);
        srvs.filter(|c| not_ours(c)).find_map(|srv| {
            let RData::Srv { port, target, .. } = &srv.record.data else { return None };
            // From the same host, or else anything that isn't ours.
            let pick = |name: &Name, t: u16| {
                let candidates = cached_records(&cache, name, t, now);
                candidates
                    .clone()
                    .find(|c| c.source == srv.source)
                    .or_else(|| candidates.clone().find(|c| not_ours(c)))
            };
            let addr = match pick(target, rtype::A)?.record.data {
                RData::A(a) => a,
                _ => return None,
            };
            let txt = match &pick(instance, rtype::TXT)?.record.data {
                RData::Txt(t) => t.clone(),
                _ => return None,
            };
            let mut addrs = vec![addr];
            for c in cached_records(&cache, target, rtype::A, now).filter(|c| not_ours(c)) {
                match c.record.data {
                    RData::A(a) if !addrs.contains(&a) => addrs.push(a),
                    _ => {}
                }
            }
            Some((*port, addrs, txt))
        })
    }
}

/// Clears the probe in progress when [`Mdns::probe`] returns or is
/// cancelled.
struct ProbeGuard<'a>(&'a Inner);

impl Drop for ProbeGuard<'_> {
    fn drop(&mut self) {
        *lock(&self.0.probe) = None;
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The live records of `name` and `rtype` in `cache`. Those a newer record
/// made obsolete, kept for their last second (RFC 6762 §10.2), come last: a
/// lookup that takes the first match gets current data, while one that
/// gathers a host's addresses still finds them.
fn cached_records<'c>(
    cache: &'c [CachedRecord],
    name: &Name,
    rtype: u16,
    now: Instant,
) -> impl Iterator<Item = &'c CachedRecord> + Clone {
    let live = move |c: &&CachedRecord| {
        c.expires > now && c.record.rtype == rtype && c.record.name.eq_ignore_case(name)
    };
    let current = cache.iter().filter(live).filter(|c| !c.flushed);
    current.chain(cache.iter().filter(live).filter(|c| c.flushed))
}

fn multicast_dest() -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(MDNS_GROUP, MDNS_PORT))
}

async fn send(socket: &UdpSocket, msg: &Message, dest: SocketAddr) {
    if let Err(e) = socket.send_to(&msg.encode(), dest).await {
        debug!("mDNS send to {dest} failed: {e}");
    }
}

async fn announce(inner: &Inner, records: Vec<Record>) {
    for msg in split_response(records, Message::response()) {
        send(&inner.socket, &msg, inner.dest).await;
    }
}

async fn run(inner: Arc<Inner>) {
    let mut buf = vec![0u8; 9000];
    let mut sweep = tokio::time::interval(CONFLICT_SWEEP);
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let received = tokio::select! {
            r = inner.socket.recv_from(&mut buf) => r,
            _ = sweep.tick() => {
                let questions = sweep_conflicts(&inner)
                    .into_iter()
                    .map(|name| Question { name, qtype: rtype::ANY, unicast_response: false })
                    .collect();
                // A device using our name shadows a few names per transmit
                // channel: hundreds of questions, more than one packet holds.
                for query in split_query(questions) {
                    send(&inner.socket, &query, inner.dest).await;
                }
                continue;
            }
        };
        let (len, src) = match received {
            Ok(r) => r,
            Err(e) => {
                warn!("mDNS receive error: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let msg = match Message::decode(&buf[..len]) {
            Ok(m) => m,
            Err(e) => {
                trace!("ignoring malformed mDNS packet from {src}: {e}");
                continue;
            }
        };
        if msg.is_response() {
            let IpAddr::V4(from) = src.ip() else { continue };
            if cache_response(&inner, &msg, from) {
                // Peers that took the other host's records (they carry
                // cache-flush) get ours back, and a host that follows RFC
                // 6762 §9 re-probes and gives the name up.
                let records = lock(&inner.own).records.clone();
                announce(&inner, records).await;
            }
        } else {
            answer_query(&inner, &msg, src).await;
        }
    }
}

/// Caches the records of a response from `src`. Returns whether it shows a
/// host newly claiming our names.
fn cache_response(inner: &Inner, msg: &Message, src: Ipv4Addr) -> bool {
    let now = Instant::now();
    // Copies of our own records (multicast loopback, reflectors, a device
    // with our name whose TXT equals ours). Ours are known without the
    // cache, so they must not flush another host's records under our names
    // from it: those are what a name conflict is resolved from.
    let ours: Vec<bool> = {
        let own = lock(&inner.own);
        msg.records().map(|rec| is_own_record(&own.records, rec)).collect()
    };
    {
        let mut cache = lock(&inner.cache);
        cache.retain(|c| c.expires > now);
        for (rec, ours) in msg.records().zip(ours) {
            if rec.cache_flush && !ours {
                // RFC 6762 §10.2: older records with the same name and type
                // are obsolete, but only those received more than a second
                // ago, and they expire a second from now rather than at
                // once. Records received within the last second belong to
                // the same announcement: a set too large for one packet, or
                // a computer on Wi-Fi and Ethernet on this link, which
                // announces each interface's address on that interface, in
                // packets of their own. The delay lets such a host announce
                // again, before it expires, an address that its other
                // interface's announcement flushed (§10.2, §14).
                for c in cache.iter_mut() {
                    if c.record.rtype == rec.rtype
                        && c.record.name.eq_ignore_case(&rec.name)
                        && now.duration_since(c.inserted) > FLUSH_WINDOW
                    {
                        c.expires = c.expires.min(now + FLUSH_WINDOW);
                        c.flushed = true;
                    }
                }
            }
            // One copy per sender, which tells devices with one name apart.
            cache.retain(|c| {
                !(c.record.rtype == rec.rtype
                    && c.record.name.eq_ignore_case(&rec.name)
                    && c.record.data == rec.data
                    && c.source == src)
            });
            if rec.ttl > 0 {
                cache.push(CachedRecord {
                    record: rec.clone(),
                    expires: now + Duration::from_secs(rec.ttl as u64),
                    inserted: now,
                    source: src,
                    flushed: false,
                });
            }
        }
    }
    let fresh = note_conflicts(inner, msg, src, now);
    inner.updated.notify_waiters();
    fresh
}

/// Notes what a response from `src` says about the names we probe and the
/// names we hold. Returns whether `src` is a host that did not claim any of
/// the names we hold before.
fn note_conflicts(inner: &Inner, msg: &Message, src: Ipv4Addr, now: Instant) -> bool {
    if let Some(probe) = lock(&inner.probe).as_mut() {
        if probe.found.is_none() {
            if let Some(rec) = msg.records().find(|r| conflicts_with(&probe.proposed, r)) {
                probe.found = Some(Conflict { name: rec.name.clone(), peer: src });
            }
        }
    }
    let own = lock(&inner.own);
    let mut entries = lock(&inner.conflicts);
    // A host new to the list: a device with our name announces dozens of
    // records over several packets, and we answer the first one only.
    let known_peer = entries.iter().any(|(c, _)| c.peer == src);
    let mut fresh = false;
    let mut changed = false;
    for rec in msg.records().filter(|r| is_unique_name(&own.records, &r.name)) {
        let known =
            entries.iter().position(|(c, _)| c.peer == src && c.name.eq_ignore_case(&rec.name));
        if rec.ttl == 0 {
            // A goodbye: that host gave the name up.
            if let Some(i) = known {
                entries.remove(i);
                changed = true;
            }
        } else if conflicts_with(&own.records, rec) {
            let expires = now + Duration::from_secs(rec.ttl.into()).min(CONFLICT_LIFETIME);
            match known {
                Some(i) => entries[i].1 = entries[i].1.max(expires),
                None => {
                    entries.push((Conflict { name: rec.name.clone(), peer: src }, expires));
                    fresh = !known_peer;
                    changed = true;
                }
            }
        }
    }
    if changed {
        publish_conflicts(inner, &entries);
    }
    fresh
}

/// Drops expired conflicts, and returns the names of those that expire soon,
/// to ask about again.
fn sweep_conflicts(inner: &Inner) -> Vec<Name> {
    let now = Instant::now();
    retain_conflicts_where(inner, |_, expires| expires > now);
    let mut names: Vec<Name> = Vec::new();
    for (c, expires) in lock(&inner.conflicts).iter() {
        let left = expires.saturating_duration_since(now);
        let due = CONFLICT_REFRESH.iter().any(|&(from, to)| left > from && left <= to);
        if due && !names.iter().any(|n| n.eq_ignore_case(&c.name)) {
            names.push(c.name.clone());
        }
    }
    names
}

fn retain_conflicts(inner: &Inner, keep: impl Fn(&Conflict) -> bool) {
    retain_conflicts_where(inner, |c, _| keep(c));
}

fn retain_conflicts_where(inner: &Inner, keep: impl Fn(&Conflict, Instant) -> bool) {
    let mut entries = lock(&inner.conflicts);
    let before = entries.len();
    entries.retain(|(c, expires)| keep(c, *expires));
    if entries.len() != before {
        publish_conflicts(inner, &entries);
    }
}

/// Publishes the conflicts, logging each host that starts or stops using
/// our names once, however many of our records it shadows (a device with
/// our name has one per transmit channel).
fn publish_conflicts(inner: &Inner, entries: &[(Conflict, Instant)]) {
    let list: Vec<Conflict> = entries.iter().map(|(c, _)| c.clone()).collect();
    inner.conflicts_seen.send_if_modified(|seen| {
        if *seen == list {
            return false;
        }
        let peers = |l: &[Conflict]| {
            let mut peers: Vec<Ipv4Addr> = Vec::new();
            for c in l {
                if !peers.contains(&c.peer) {
                    peers.push(c.peer);
                }
            }
            peers
        };
        let (before, after) = (peers(seen), peers(&list));
        for peer in after.iter().filter(|p| !before.contains(p)) {
            let name = list.iter().find(|c| c.peer == *peer).map(|c| &c.name);
            error!(
                "mDNS: the device at {peer} also announces {}, a name of this device: \
                 subscriptions to this device may reach that one instead. Rename one of them.",
                name.map(ToString::to_string).unwrap_or_default()
            );
        }
        for peer in before.iter().filter(|p| !after.contains(p)) {
            info!("mDNS: the device at {peer} no longer announces names of this device");
        }
        *seen = list;
        true
    });
}

/// Whether `name` is one of the unique names of `own`: a host name or a
/// service instance, whose records carry cache-flush. Shared names (the
/// service types our PTR records hang off) are not.
fn is_unique_name(own: &[Record], name: &Name) -> bool {
    own.iter().any(|r| r.cache_flush && r.name.eq_ignore_case(name))
}

/// Whether `rec`, from a response, claims one of the unique names of `own`
/// for another host (RFC 6762 §8.1, §9): any record under that name that is
/// not one of ours. Our own records coming back to us (multicast loopback,
/// mDNS reflectors) are identical, so they don't count; goodbyes don't
/// either.
fn conflicts_with(own: &[Record], rec: &Record) -> bool {
    rec.ttl > 0 && is_unique_name(own, &rec.name) && !is_own_record(own, rec)
}

/// Whether `rec` is one of `own`, whatever its TTL.
fn is_own_record(own: &[Record], rec: &Record) -> bool {
    own.iter().any(|r| {
        r.rtype == rec.rtype && r.name.eq_ignore_case(&rec.name) && same_data(&r.data, &rec.data)
    })
}

/// Record data equality, with names compared as DNS does: ignoring case.
fn same_data(a: &RData, b: &RData) -> bool {
    match (a, b) {
        (
            RData::Srv { priority: p1, weight: w1, port: o1, target: t1 },
            RData::Srv { priority: p2, weight: w2, port: o2, target: t2 },
        ) => p1 == p2 && w1 == w2 && o1 == o2 && t1.eq_ignore_case(t2),
        (RData::Ptr(x), RData::Ptr(y)) => x.eq_ignore_case(y),
        _ => a == b,
    }
}

/// The names [`Mdns::probe`] asks about: unique names, except channel
/// instances.
fn probe_names(records: &[Record]) -> Vec<Name> {
    let mut names: Vec<Name> = Vec::new();
    for r in records.iter().filter(|r| r.cache_flush) {
        let channel = r.rtype != rtype::A && r.name.first_label().is_some_and(|l| l.contains('@'));
        if !channel && !names.iter().any(|n| n.eq_ignore_case(&r.name)) {
            names.push(r.name.clone());
        }
    }
    names
}

/// The `n`th name to try (from 2 on) when `wanted` is taken: `wanted-n`,
/// shortened so that it stays a valid device name. Always appended, never
/// counted on from a number `wanted` ends with: `stage-1` must not turn
/// into `stage-2`, the name of another device.
pub fn alternative_name(wanted: &str, n: u32) -> String {
    let suffix = format!("-{n}");
    let room = MAX_NAME_LEN.saturating_sub(suffix.len());
    let base: String = wanted.chars().take(room).collect();
    format!("{}{suffix}", base.trim_end_matches('-'))
}

async fn answer_query(inner: &Inner, msg: &Message, src: SocketAddr) {
    let records = lock(&inner.own).records.clone();
    let mut answers: Vec<Record> = Vec::new();
    let mut unicast = src.port() != MDNS_PORT;
    for q in &msg.questions {
        unicast |= q.unicast_response;
        for rec in related_records(&records, q) {
            if !answers.contains(&rec) {
                answers.push(rec);
            }
        }
    }
    if answers.is_empty() {
        return;
    }
    let legacy = src.port() != MDNS_PORT;
    let mut template = Message::response();
    if legacy {
        // RFC 6762 §6.7: legacy unicast responses echo the id and questions
        // and use short TTLs.
        template.id = msg.id;
        template.questions = msg.questions.clone();
        for a in &mut answers {
            a.ttl = a.ttl.min(10);
        }
    }
    let dest = if unicast { src } else { inner.dest };
    for out in split_response(answers, template) {
        send(&inner.socket, &out, dest).await;
    }
}

/// Records answering `q`, plus the records needed to use them (SRV and TXT
/// for PTR targets, A for SRV targets), all destined for the answer section.
fn related_records(records: &[Record], q: &Question) -> Vec<Record> {
    let matches = |name: &Name, t: u16| -> Vec<Record> {
        records
            .iter()
            .filter(|r| r.name.eq_ignore_case(name) && (t == rtype::ANY || r.rtype == t))
            .cloned()
            .collect()
    };
    let mut out: Vec<Record> = matches(&q.name, q.qtype);
    let mut i = 0;
    while i < out.len() {
        let extra: Vec<Record> = match &out[i].data {
            RData::Ptr(target) => matches(target, rtype::ANY),
            RData::Srv { target, .. } => matches(target, rtype::A),
            _ => Vec::new(),
        };
        for r in extra {
            if !out.contains(&r) {
                out.push(r);
            }
        }
        i += 1;
    }
    out
}

/// Packs records into as few messages as fit in [`MAX_PACKET`] bytes.
fn split_response(records: Vec<Record>, template: Message) -> Vec<Message> {
    let base = template.encode().len();
    let mut out = Vec::new();
    let mut current = template.clone();
    let mut size = base;
    for rec in records {
        let mut single = Message::response();
        single.answers.push(rec.clone());
        let rec_size = single.encode().len() - 12;
        if size + rec_size > MAX_PACKET && !current.answers.is_empty() {
            out.push(std::mem::replace(&mut current, template.clone()));
            size = base;
        }
        size += rec_size;
        current.answers.push(rec);
    }
    if !current.answers.is_empty() {
        out.push(current);
    }
    out
}

/// Packs questions into as few queries as fit in [`MAX_PACKET`] bytes. A
/// larger query would be fragmented, and a lost fragment loses all of it.
fn split_query(questions: Vec<Question>) -> Vec<Message> {
    let base = Message::query(Vec::new()).encode().len();
    let mut out = Vec::new();
    let mut current = Vec::new();
    let mut size = base;
    for q in questions {
        // Without name compression: never less than in the packet.
        let q_size = Message::query(vec![q.clone()]).encode().len() - base;
        if size + q_size > MAX_PACKET && !current.is_empty() {
            out.push(Message::query(std::mem::take(&mut current)));
            size = base;
        }
        size += q_size;
        current.push(q);
    }
    if !current.is_empty() {
        out.push(Message::query(current));
    }
    out
}

/// What a device advertises.
pub struct Advertisement<'a> {
    pub name: &'a str,
    /// Host name (`<host>.local`) of the A record that the SRV records
    /// point at: the factory name, as real devices do (docs/PROTOCOL.md
    /// §2.1). Unlike the device name it never changes, and it is not this
    /// computer's own host name, which the device is often named after: the
    /// computer's responder announces that name with addresses of its own.
    pub host: &'a str,
    pub ip: Ipv4Addr,
    pub arc_port: u16,
    pub cmc_port: u16,
    pub flow_control_port: u16,
    pub device_id: &'a ovsc_proto::DeviceId,
    pub process_id: u16,
    pub manufacturer: &'a str,
    pub board_name: &'a str,
    pub model: &'a str,
    /// `(factory name, current name, txt)` per transmit channel.
    pub tx_channels: Vec<(&'a str, &'a str, ChannelTxt)>,
}

/// Builds the full record set of a device.
pub fn device_records(ad: &Advertisement<'_>) -> Vec<Record> {
    let host = Name::from_labels([ad.host, "local"]);
    let mut records = vec![Record::a(host.clone(), TTL_HOST, ad.ip)];
    let mut service = |service: &str, instance: &str, port: u16, txt: Vec<Vec<u8>>| {
        let stype = discovery::service_name(service);
        let inst = Name::instance(instance, &stype);
        records.push(Record::ptr(stype, TTL_OTHER, inst.clone()));
        records.push(Record::srv(inst.clone(), TTL_HOST, port, host.clone()));
        records.push(Record::txt(inst, TTL_OTHER, txt));
    };
    service(
        discovery::ARC_SERVICE,
        ad.name,
        ad.arc_port,
        discovery::arc_txt(ad.board_name, ad.manufacturer, ad.model),
    );
    service(
        discovery::CMC_SERVICE,
        ad.name,
        ad.cmc_port,
        discovery::cmc_txt(ad.device_id, ad.process_id, ad.manufacturer, ad.model),
    );
    for (factory, current, txt) in &ad.tx_channels {
        let mut default = txt.clone();
        default.is_default_name = true;
        service(
            discovery::CHAN_SERVICE,
            &discovery::channel_instance(factory, ad.name),
            ad.flow_control_port,
            default.to_entries(),
        );
        if !factory.eq_ignore_ascii_case(current) {
            let mut alias = txt.clone();
            alias.is_default_name = false;
            service(
                discovery::CHAN_SERVICE,
                &discovery::channel_instance(current, ad.name),
                ad.flow_control_port,
                alias.to_entries(),
            );
        }
    }
    records
}

#[cfg(test)]
mod tests {
    use super::*;

    fn txt(id: u16) -> ChannelTxt {
        ChannelTxt {
            id,
            sample_rate: 48_000,
            bits_per_sample: 24,
            pcm_type: 0x0e,
            latency_ns: 4_000_000,
            fpp_max: 32,
            fpp_min: 2,
            nchan: 8,
            dbcp1: 0x1102,
            is_default_name: true,
            multicast: None,
        }
    }

    fn advertised(name: &str, host: &str, ip: Ipv4Addr) -> Vec<Record> {
        device_records(&Advertisement {
            name,
            host,
            ip,
            arc_port: 4440,
            cmc_port: 8800,
            flow_control_port: 4455,
            device_id: &[0; 8],
            process_id: 0,
            manufacturer: "OpenVirtualSoundcard",
            board_name: "OpenVirtualSoundcard",
            model: "_000000000000000b",
            tx_channels: vec![("01", "Left", txt(1)), ("02", "02", txt(2))],
        })
    }

    fn records() -> Vec<Record> {
        advertised("studio", "ovsc-0a0b0c", Ipv4Addr::new(10, 0, 0, 9))
    }

    /// The same device name on another computer: its own host name and
    /// address.
    fn twin() -> Vec<Record> {
        advertised("studio", "ovsc-1d2e3f", Ipv4Addr::new(10, 0, 0, 12))
    }

    fn response(records: Vec<Record>) -> Message {
        let mut msg = Message::response();
        msg.answers = records;
        msg
    }

    /// An `Mdns` that sends to a unicast socket of the test's instead of the
    /// mDNS group, so that tests see what it sends and need no multicast.
    async fn unicast_mdns() -> (Mdns, UdpSocket) {
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        (Mdns::with_socket(socket, peer.local_addr().unwrap()), peer)
    }

    async fn receive(peer: &UdpSocket, wait: Duration) -> Option<Message> {
        let mut buf = vec![0u8; 9000];
        let (len, _) = tokio::time::timeout(wait, peer.recv_from(&mut buf)).await.ok()?.ok()?;
        Some(Message::decode(&buf[..len]).unwrap())
    }

    #[test]
    fn record_set_contents() {
        let recs = records();
        // A + 3 per service: arc, cmc, 01, Left (alias), 02.
        assert_eq!(recs.len(), 1 + 3 * 5);
        let chan = discovery::service_name(discovery::CHAN_SERVICE);
        let alias = Name::instance("Left@studio", &chan);
        let txt = recs
            .iter()
            .find(|r| r.rtype == rtype::TXT && r.name == alias)
            .and_then(|r| match &r.data {
                RData::Txt(t) => Some(ChannelTxt::from_entries(t).unwrap()),
                _ => None,
            })
            .unwrap();
        assert_eq!(txt.id, 1);
        assert!(!txt.is_default_name);
    }

    #[test]
    fn services_point_at_the_factory_host_name() {
        let recs = records();
        let host = Name::parse("ovsc-0a0b0c.local");
        assert!(recs.iter().any(|r| r.name == host && r.data == RData::A([10, 0, 0, 9].into())));
        for r in recs.iter().filter(|r| r.rtype == rtype::SRV) {
            assert!(matches!(&r.data, RData::Srv { target, .. } if *target == host), "{r:?}");
        }
        // Nothing is announced under `studio.local`, the computer's own name
        // when the device is named after it.
        assert!(!recs.iter().any(|r| r.name.eq_ignore_case(&Name::parse("studio.local"))));
    }

    #[test]
    fn instance_query_gets_srv_txt_and_address() {
        let recs = records();
        let chan = discovery::service_name(discovery::CHAN_SERVICE);
        let q = Question {
            name: Name::instance("01@STUDIO", &chan),
            qtype: rtype::SRV,
            unicast_response: false,
        };
        let answers = related_records(&recs, &q);
        assert_eq!(answers.len(), 2);
        assert_eq!(answers[0].rtype, rtype::SRV);
        assert_eq!(answers[1].data, RData::A(Ipv4Addr::new(10, 0, 0, 9)));
    }

    #[test]
    fn browse_query_gets_everything_about_each_instance() {
        let recs = records();
        let q = Question {
            name: discovery::service_name(discovery::CHAN_SERVICE),
            qtype: rtype::PTR,
            unicast_response: false,
        };
        let answers = related_records(&recs, &q);
        // 3 instances × (PTR + SRV + TXT) + one shared A record.
        assert_eq!(answers.len(), 3 * 3 + 1);
    }

    #[test]
    fn large_answer_sets_are_split() {
        let mut recs = Vec::new();
        for i in 0..200 {
            recs.push(Record::txt(Name::parse(&format!("x{i}.local")), 120, vec![vec![b'a'; 40]]));
        }
        let msgs = split_response(recs, Message::response());
        assert!(msgs.len() > 1);
        assert!(msgs.iter().all(|m| m.encode().len() <= MAX_PACKET));
        assert_eq!(msgs.iter().map(|m| m.answers.len()).sum::<usize>(), 200);
    }

    /// The names of a 64-channel device using ours, asked about again
    /// before its conflict expires, take several queries.
    #[test]
    fn large_queries_are_split() {
        let chan = discovery::service_name(discovery::CHAN_SERVICE);
        let questions: Vec<Question> = (1..=128)
            .map(|i| Question {
                name: Name::instance(&format!("Channel {i:03}@studio"), &chan),
                qtype: rtype::ANY,
                unicast_response: false,
            })
            .collect();
        let msgs = split_query(questions.clone());
        assert!(msgs.len() > 1);
        assert!(msgs.iter().all(|m| !m.is_response() && m.encode().len() <= MAX_PACKET));
        let sent: Vec<Question> = msgs.into_iter().flat_map(|m| m.questions).collect();
        assert_eq!(sent, questions);
        assert!(split_query(Vec::new()).is_empty());
    }

    #[test]
    fn what_counts_as_a_conflict() {
        let own = records();
        // Our own records coming back, in any case.
        for r in &own {
            assert!(!conflicts_with(&own, r), "{r:?}");
        }
        let mut shouted = own.iter().find(|r| r.rtype == rtype::SRV).unwrap().clone();
        if let RData::Srv { target, .. } = &mut shouted.data {
            *target = Name::parse("OVSC-0A0B0C.LOCAL");
        }
        assert!(!conflicts_with(&own, &shouted));
        // Another computer named like us: its service instances point at
        // its own host, and its channel TXT records may equal ours.
        let other = twin();
        let arc = discovery::service_name(discovery::ARC_SERVICE);
        let instance = Name::instance("studio", &arc);
        let srv = other.iter().find(|r| r.rtype == rtype::SRV && r.name == instance).unwrap();
        assert!(conflicts_with(&own, srv));
        let mut goodbye = srv.clone();
        goodbye.ttl = 0;
        assert!(!conflicts_with(&own, &goodbye));
        // Any type under a name of ours.
        let aaaa = Record {
            name: instance.clone(),
            rtype: rtype::AAAA,
            cache_flush: true,
            ttl: 120,
            data: RData::Other(vec![0; 16]),
        };
        assert!(conflicts_with(&own, &aaaa));
        // Shared names (service types) and other devices' names are not ours.
        let ptr = other.iter().find(|r| r.rtype == rtype::PTR).unwrap();
        assert!(!conflicts_with(&own, ptr));
        let elsewhere = advertised("desk", "desk", Ipv4Addr::new(10, 0, 0, 12));
        assert!(elsewhere.iter().all(|r| !conflicts_with(&own, r)));
    }

    #[test]
    fn probes_ask_for_the_device_names_only() {
        let names = probe_names(&records());
        let arc = discovery::service_name(discovery::ARC_SERVICE);
        let cmc = discovery::service_name(discovery::CMC_SERVICE);
        assert_eq!(
            names,
            [
                Name::parse("ovsc-0a0b0c.local"),
                Name::instance("studio", &arc),
                Name::instance("studio", &cmc),
            ]
        );
    }

    #[test]
    fn alternative_names_stay_valid() {
        assert_eq!(alternative_name("studio", 2), "studio-2");
        // Appended, not counted on.
        assert_eq!(alternative_name("stage-1", 2), "stage-1-2");
        let long = "a".repeat(31);
        assert_eq!(alternative_name(&long, 12), format!("{}-12", "a".repeat(28)));
        // No double hyphen where the cut falls after one.
        let hyphen = format!("{}-bcd", "a".repeat(28));
        assert_eq!(alternative_name(&hyphen, 3), format!("{}-3", "a".repeat(28)));
        for n in 2..200 {
            discovery::validate_device_name(&alternative_name(&long, n)).unwrap();
            discovery::validate_device_name(&alternative_name(&hyphen, n)).unwrap();
        }
    }

    #[test]
    fn name_conflicts_read_well() {
        let live =
            NameConflict { name: "studio".into(), peer: [10, 0, 0, 12].into(), renamed_to: None };
        assert!(live.to_string().contains("10.0.0.12 also uses the name studio"));
        let renamed = NameConflict { renamed_to: Some("studio-2".into()), ..live };
        assert!(renamed.to_string().ends_with("took the name studio-2"));
    }

    /// Another host announcing our names is a conflict until it says
    /// goodbye; our own records coming back are not.
    #[tokio::test]
    async fn conflicts_are_tracked() {
        let (mdns, _peer) = unicast_mdns().await;
        mdns.set_records(records()).await;
        let mut conflicts = mdns.conflicts();
        let inner = &mdns.inner;
        let us = Ipv4Addr::new(10, 0, 0, 9);
        let them = Ipv4Addr::new(10, 0, 0, 12);

        assert!(!cache_response(inner, &response(records()), us));
        assert!(!cache_response(inner, &response(records()), them), "a reflector");
        assert!(conflicts.borrow_and_update().is_empty());

        // Its announcement may span several packets: the host is new once.
        let all = twin();
        let (first, rest) = all.split_at(8);
        assert!(cache_response(inner, &response(first.to_vec()), them));
        assert!(!cache_response(inner, &response(rest.to_vec()), them));
        assert!(conflicts.has_changed().unwrap());
        let seen = conflicts.borrow_and_update().clone();
        assert!(seen.iter().all(|c| c.peer == them));
        let arc = Name::instance("studio", &discovery::service_name(discovery::ARC_SERVICE));
        assert!(seen.iter().any(|c| c.name == arc));
        // Seen again: not new.
        assert!(!cache_response(inner, &response(twin()), them));

        let goodbye = twin()
            .into_iter()
            .map(|mut r| {
                r.ttl = 0;
                r
            })
            .collect();
        assert!(!cache_response(inner, &response(goodbye), them));
        assert!(conflicts.borrow_and_update().is_empty());

        // Giving our names up ends a conflict too.
        assert!(cache_response(inner, &response(twin()), them));
        mdns.set_records(advertised("studio-2", "ovsc-0a0b0c", us)).await;
        assert!(conflicts.borrow_and_update().is_empty());
        mdns.stop();
    }

    /// A conflict lasts as long as the other host's address records, and its
    /// name is asked about again before that, so that a host still using it
    /// renews it.
    #[tokio::test]
    async fn conflicts_expire_unless_renewed() {
        let (mdns, _peer) = unicast_mdns().await;
        mdns.set_records(records()).await;
        let inner = &mdns.inner;
        let them = Ipv4Addr::new(10, 0, 0, 12);
        cache_response(inner, &response(twin()), them);
        // TXT records live 4500 s; the conflict only as long as SRV and A.
        let now = Instant::now();
        assert!(lock(&inner.conflicts).iter().all(|(_, e)| *e <= now + CONFLICT_LIFETIME));
        assert!(sweep_conflicts(inner).is_empty());

        let name = |label: &str| Name::instance(label, &discovery::service_name("_x._udp.local"));
        let at = |secs: u64| Instant::now() + Duration::from_secs(secs);
        *lock(&inner.conflicts) = vec![
            (Conflict { name: name("soon"), peer: them }, at(22)),
            (Conflict { name: name("later"), peer: them }, at(60)),
            (Conflict { name: name("last"), peer: them }, at(7)),
        ];
        assert_eq!(sweep_conflicts(inner), [name("soon"), name("last")]);
        *lock(&inner.conflicts) =
            vec![(Conflict { name: name("gone"), peer: them }, Instant::now())];
        assert!(sweep_conflicts(inner).is_empty());
        assert!(lock(&inner.conflicts).is_empty());
        assert!(mdns.conflicts().borrow().is_empty());
        mdns.stop();
    }

    #[tokio::test]
    async fn a_probe_finds_a_name_in_use() {
        let (mdns, peer) = unicast_mdns().await;
        let ours = records();
        let probing = tokio::spawn({
            let mdns = mdns.clone();
            let ours = ours.clone();
            async move { mdns.probe(&ours).await }
        });
        // The probe: a query for our names, with the records we propose.
        let probe = receive(&peer, Duration::from_secs(2)).await.expect("no probe");
        assert!(!probe.is_response());
        assert_eq!(probe.questions.len(), 3);
        assert!(probe.questions.iter().all(|q| q.qtype == rtype::ANY));
        assert_eq!(probe.authorities.len(), 5);
        // Another device named `studio` answers.
        let them = Ipv4Addr::new(10, 0, 0, 12);
        cache_response(&mdns.inner, &response(twin()), them);
        let found = probing.await.unwrap().expect("conflict not found");
        assert_eq!(found.peer, them);
        assert!(lock(&mdns.inner.probe).is_none());
        // Nobody answers for this one.
        let started = Instant::now();
        assert_eq!(mdns.probe(&advertised("desk", "ovsc-0a0b0c", them)).await, None);
        assert!(started.elapsed() >= PROBE_INTERVAL * PROBES as u32);
        // Answers that are our own proposed records don't count.
        let probing = tokio::spawn({
            let mdns = mdns.clone();
            async move { mdns.probe(&ours).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        cache_response(&mdns.inner, &response(records()), them);
        assert_eq!(probing.await.unwrap(), None);
        mdns.stop();
    }

    /// A record set replaced within its announcement period is not
    /// announced again after its goodbye.
    #[tokio::test]
    async fn superseded_records_are_not_announced_again() {
        let (mdns, peer) = unicast_mdns().await;
        mdns.set_records(records()).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        mdns.set_records(advertised("studio-2", "ovsc-0a0b0c", [10, 0, 0, 9].into())).await;
        let old = |r: &Record| r.name.first_label() == Some("studio");
        let mut said_goodbye = false;
        while let Some(msg) = receive(&peer, Duration::from_millis(1500)).await {
            for r in msg.answers.iter().filter(|r| old(r)) {
                if r.ttl == 0 {
                    said_goodbye = true;
                } else {
                    assert!(!said_goodbye, "{r:?} announced after its goodbye");
                }
            }
        }
        assert!(said_goodbye);
        mdns.stop();
    }

    #[tokio::test]
    async fn foreign_channels_resolve_only_during_a_conflict() {
        let (mdns, _peer) = unicast_mdns().await;
        mdns.set_records(records()).await;
        let them = Ipv4Addr::new(10, 0, 0, 12);
        let wait = Duration::from_millis(300);
        // Our own records coming back are not another device's.
        cache_response(&mdns.inner, &response(records()), them);
        assert_eq!(mdns.resolve_foreign_channel("01", "studio", wait).await, None);

        cache_response(&mdns.inner, &response(twin()), them);
        // Our own records coming back afterwards, cache-flush bit and all,
        // leave the other device's in the cache.
        cache_response(&mdns.inner, &response(records()), Ipv4Addr::new(10, 0, 0, 9));
        let theirs = mdns.resolve_foreign_channel("01", "studio", wait).await.unwrap();
        assert_eq!((theirs.addr, theirs.flow_control_port, theirs.txt.id), (them, 4455, 1));
        // A channel only we have.
        assert_eq!(mdns.resolve_foreign_channel("03", "studio", wait).await, None);
        // The ordinary lookup still finds ours.
        let ours = mdns.resolve_channel("01", "studio", wait).await.unwrap();
        assert_eq!(ours.addr, Ipv4Addr::new(10, 0, 0, 9));
        mdns.stop();
    }

    /// The records a computer announces for one of its channels on one
    /// interface: SRV, TXT and that interface's address, all with
    /// cache-flush.
    fn announced_on(addr: Ipv4Addr) -> Message {
        let host = Name::parse("dvs-studio.local");
        let instance =
            Name::instance("01@dvs-studio", &discovery::service_name(discovery::CHAN_SERVICE));
        response(vec![
            Record::srv(instance.clone(), TTL_HOST, 4455, host.clone()),
            Record::txt(instance, TTL_OTHER, txt(1).to_entries()),
            Record::a(host, TTL_HOST, addr),
        ])
    }

    /// A computer on Wi-Fi and Ethernet on one link announces its records on
    /// each interface, with that interface's address, in packets of their
    /// own (RFC 6762 §14). Within a second of each other, neither flushes
    /// the other (§10.2): both addresses stay for their whole TTL, and a
    /// lookup finds them both.
    #[tokio::test]
    async fn a_host_keeps_the_address_of_each_interface() {
        let (mdns, _peer) = unicast_mdns().await;
        let inner = &mdns.inner;
        let host = Name::parse("dvs-studio.local");
        let wired = Ipv4Addr::new(10, 0, 0, 21);
        let wifi = Ipv4Addr::new(10, 0, 0, 22);
        cache_response(inner, &announced_on(wired), wired);
        cache_response(inner, &announced_on(wifi), wifi);

        let ttl = Instant::now() + Duration::from_secs(TTL_HOST as u64 - 1);
        {
            let cache = lock(&inner.cache);
            let a: Vec<_> = cache.iter().filter(|c| c.record.rtype == rtype::A).collect();
            assert_eq!(a.len(), 2);
            assert!(a.iter().all(|c| !c.flushed && c.expires > ttl));
        }
        assert_eq!(mdns.lookup_addrs(&host), [wired, wifi]);
        let r = mdns.resolve_channel("01", "dvs-studio", Duration::ZERO).await.unwrap();
        assert_eq!((r.addr, r.addrs), (wired, vec![wired, wifi]));
        mdns.stop();
    }

    /// A record announced with cache-flush makes those of its name and type
    /// received more than a second earlier obsolete: they expire a second
    /// later (RFC 6762 §10.2), and lookups take the new one first meanwhile.
    /// One announced again within that second stays, as when a computer on
    /// Wi-Fi and Ethernet puts back the address that its other interface's
    /// announcement flushed (§14).
    #[tokio::test]
    async fn older_records_expire_a_second_after_a_cache_flush() {
        let (mdns, _peer) = unicast_mdns().await;
        let inner = &mdns.inner;
        let host = Name::parse("dvs-studio.local");
        let wired = Ipv4Addr::new(10, 0, 0, 21);
        let wifi = Ipv4Addr::new(10, 0, 0, 22);
        let a = |addr| response(vec![Record::a(host.clone(), TTL_HOST, addr)]);
        let state = |addr: Ipv4Addr| {
            let cache = lock(&inner.cache);
            let c = cache.iter().find(|c| c.record.data == RData::A(addr)).unwrap();
            (c.flushed, c.expires.saturating_duration_since(Instant::now()))
        };
        // Received two seconds ago, as far as the cache can tell.
        let two_seconds_ago = || {
            for c in lock(&inner.cache).iter_mut() {
                c.inserted -= Duration::from_secs(2);
            }
        };
        cache_response(inner, &a(wired), wired);
        two_seconds_ago();
        cache_response(inner, &a(wifi), wifi);
        let (flushed, left) = state(wired);
        assert!(flushed && left <= FLUSH_WINDOW, "{left:?}");
        assert_eq!(mdns.lookup_a(&host), Some(wifi));
        assert_eq!(mdns.lookup_addrs(&host), [wifi, wired]);

        // Announced again within the second: current again, for its TTL.
        cache_response(inner, &a(wired), wired);
        let (flushed, left) = state(wired);
        assert!(!flushed && left > Duration::from_secs(100), "{left:?}");
        assert_eq!(mdns.lookup_addrs(&host), [wifi, wired]);

        // Not announced again: gone once its second is over.
        two_seconds_ago();
        cache_response(inner, &a(wifi), wifi);
        assert_eq!(mdns.lookup_addrs(&host), [wifi, wired]);
        tokio::time::sleep(FLUSH_WINDOW + Duration::from_millis(50)).await;
        assert_eq!(mdns.lookup_addrs(&host), [wifi]);
        mdns.stop();
    }
}
