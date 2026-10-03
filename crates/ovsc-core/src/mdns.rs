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
//! Not implemented yet: probing and name-conflict resolution (RFC 6762 §8).

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::Notify;
use tracing::{debug, trace, warn};

use ovsc_proto::discovery::{self, ChannelTxt};
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

struct CachedRecord {
    record: Record,
    expires: Instant,
    inserted: Instant,
}

struct Inner {
    socket: UdpSocket,
    task: Mutex<Option<tokio::task::AbortHandle>>,
    records: Mutex<Vec<Record>>,
    cache: Mutex<Vec<CachedRecord>>,
    updated: Notify,
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
        let inner = Arc::new(Inner {
            socket,
            task: Mutex::new(None),
            records: Mutex::new(Vec::new()),
            cache: Mutex::new(Vec::new()),
            updated: Notify::new(),
        });
        let task = tokio::spawn(run(inner.clone()));
        *lock(&inner.task) = Some(task.abort_handle());
        Ok(Self { inner })
    }

    /// Stops answering and resolving. Call [`Mdns::goodbye`] first to
    /// withdraw our records from peers' caches.
    pub fn stop(&self) {
        if let Some(task) = lock(&self.inner.task).take() {
            task.abort();
        }
    }

    /// Replaces the set of records we are authoritative for. New records are
    /// announced; records that disappeared get a goodbye (TTL 0).
    pub async fn set_records(&self, records: Vec<Record>) {
        let old = std::mem::replace(&mut *lock(&self.inner.records), records.clone());
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
                mdns.send_unsolicited(records.clone()).await;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }

    /// Withdraws all our records.
    pub async fn goodbye(&self) {
        let records = std::mem::take(&mut *lock(&self.inner.records));
        let gone = records
            .into_iter()
            .map(|mut r| {
                r.ttl = 0;
                r
            })
            .collect();
        self.send_unsolicited(gone).await;
    }

    async fn send_unsolicited(&self, records: Vec<Record>) {
        for msg in split_response(records, Message::response()) {
            send(&self.inner.socket, &msg, multicast_dest()).await;
        }
    }

    async fn query(&self, questions: Vec<Question>) {
        send(&self.inner.socket, &Message::query(questions), multicast_dest()).await;
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
                if let Some(addr) = self.lookup_a(host) {
                    if let Some(txt) = txt.as_deref() {
                        let txt = ChannelTxt::from_entries(txt)?;
                        return Ok(ResolvedChannel {
                            device: device.to_owned(),
                            channel: channel.to_owned(),
                            addr,
                            flow_control_port: *port,
                            txt,
                        });
                    }
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

    /// Our own records plus cached ones that match.
    fn find(&self, name: &Name, rtype: u16) -> Vec<Record> {
        let now = Instant::now();
        let own = lock(&self.inner.records)
            .iter()
            .filter(|r| r.rtype == rtype && r.name.eq_ignore_case(name))
            .cloned()
            .collect::<Vec<_>>();
        let cached = lock(&self.inner.cache)
            .iter()
            .filter(|c| {
                c.expires > now && c.record.rtype == rtype && c.record.name.eq_ignore_case(name)
            })
            .map(|c| c.record.clone())
            .collect::<Vec<_>>();
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
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn multicast_dest() -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(MDNS_GROUP, MDNS_PORT))
}

async fn send(socket: &UdpSocket, msg: &Message, dest: SocketAddr) {
    if let Err(e) = socket.send_to(&msg.encode(), dest).await {
        debug!("mDNS send to {dest} failed: {e}");
    }
}

async fn run(inner: Arc<Inner>) {
    let mut buf = vec![0u8; 9000];
    loop {
        let (len, src) = match inner.socket.recv_from(&mut buf).await {
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
            cache_response(&inner, &msg);
        } else {
            answer_query(&inner, &msg, src).await;
        }
    }
}

fn cache_response(inner: &Inner, msg: &Message) {
    let now = Instant::now();
    let mut cache = lock(&inner.cache);
    cache.retain(|c| c.expires > now);
    for rec in msg.records() {
        if rec.cache_flush {
            // RFC 6762 §10.2: older records with the same name and type are
            // obsolete. Keep the ones added from this very packet.
            cache.retain(|c| {
                c.inserted == now
                    || c.record.rtype != rec.rtype
                    || !c.record.name.eq_ignore_case(&rec.name)
            });
        }
        cache.retain(|c| {
            !(c.record.rtype == rec.rtype
                && c.record.name.eq_ignore_case(&rec.name)
                && c.record.data == rec.data)
        });
        if rec.ttl > 0 {
            cache.push(CachedRecord {
                record: rec.clone(),
                expires: now + Duration::from_secs(rec.ttl as u64),
                inserted: now,
            });
        }
    }
    drop(cache);
    inner.updated.notify_waiters();
}

async fn answer_query(inner: &Inner, msg: &Message, src: SocketAddr) {
    let records = lock(&inner.records).clone();
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
    let dest = if unicast { src } else { multicast_dest() };
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

/// What a device advertises.
pub struct Advertisement<'a> {
    pub name: &'a str,
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
    let host = Name::from_labels([ad.name, "local"]);
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

    fn records() -> Vec<Record> {
        device_records(&Advertisement {
            name: "studio",
            ip: Ipv4Addr::new(10, 0, 0, 9),
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
}
