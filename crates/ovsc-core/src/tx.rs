//! Transmit side: the flow-control server receivers ask for audio, and the
//! real-time thread that sends it.
//!
//! Packets are paced by the media clock: the packet stamped `t` (carrying
//! frames `t .. t + fpp`) is sent once media time reaches `t + guard`. The
//! guard keeps receivers from seeing timestamps from their future when our
//! clock runs slightly ahead of theirs.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket as StdUdpSocket};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc as std_mpsc;

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use ovsc_clock::{local_now_ns, ns_to_samples, samples_to_ns};
use ovsc_proto::arc::TxFlowInfo;
use ovsc_proto::audio::{self, SampleFormat};
use ovsc_proto::dbcp::{self, FlowHandle, FlowRequest};
use ovsc_proto::frame::{Frame, Header, respond, result};

use crate::info::{FPP_LIMIT, MAX_TX_FLOWS};
use crate::net::bind_udp;
use crate::state::Shared;

/// Unicast flows without keepalives for this long are dropped.
const KEEPALIVE_TIMEOUT_NS: u64 = 4_000_000_000;
/// Largest UDP payload we send.
const MAX_DATAGRAM: usize = 1472;
/// Longest the transmit thread sleeps, so it sees commands and keepalives
/// promptly.
const MAX_WAIT_NS: u64 = 20_000_000;
/// How long the transmit thread sleeps while the media clock has no time.
const NO_CLOCK_WAIT_NS: u64 = 50_000_000;

pub enum TxCommand {
    Add(TxFlow),
    Remove(u16),
    SetChannels(u16, Vec<Option<usize>>),
    Shutdown,
}

pub enum TxEvent {
    Expired(u16),
}

/// A flow as owned by the transmit thread.
pub struct TxFlow {
    id: u16,
    socket: StdUdpSocket,
    /// Local transmit channel per slot in the packet.
    channels: Vec<Option<usize>>,
    fpp: u64,
    format: SampleFormat,
    unicast: bool,
    next_ts: Option<u64>,
    last_heard_ns: u64,
    expired: bool,
}

/// Starts the transmit thread.
pub fn spawn_engine(
    shared: Arc<Shared>,
) -> (std_mpsc::Sender<TxCommand>, mpsc::UnboundedReceiver<TxEvent>, std::thread::JoinHandle<()>) {
    let (cmd_tx, cmd_rx) = std_mpsc::channel();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    let thread = std::thread::Builder::new()
        .name("ovsc-tx".into())
        .spawn(move || engine(shared, cmd_rx, ev_tx))
        .expect("spawn transmit thread");
    (cmd_tx, ev_rx, thread)
}

fn engine(
    shared: Arc<Shared>,
    commands: std_mpsc::Receiver<TxCommand>,
    events: mpsc::UnboundedSender<TxEvent>,
) {
    crate::rt::raise_priority("transmit", crate::rt::RtClass::Transmit);
    let rate = shared.info.sample_rate;
    let guard = ns_to_samples(shared.info.tx_guard_ns, rate);
    // Beyond this much lag (or lead), resynchronise instead of catching up.
    let resync = rate as u64 / 10;
    let mut flows: Vec<TxFlow> = Vec::new();
    let mut packet = vec![0u8; MAX_DATAGRAM];
    let mut scratch = [0u8; 64];
    // Local time (ovsc_clock::local_now_ns) of the next wake-up.
    let mut wake_ns = local_now_ns() + 10_000_000;
    let mut send_errors = 0u64;
    let counters = &shared.counters;

    loop {
        // Every turn blocks here until the next packet is due, at most
        // MAX_WAIT_NS. The thread must keep blocking: XNU demotes a
        // real-time thread that runs for about a second without doing so.
        crate::rt::sleep_until(wake_ns);
        loop {
            match commands.try_recv() {
                Ok(TxCommand::Add(flow)) => {
                    debug!("tx flow {} started", flow.id);
                    flows.retain(|f| f.id != flow.id);
                    flows.push(flow);
                }
                Ok(TxCommand::Remove(id)) => flows.retain(|f| f.id != id),
                Ok(TxCommand::SetChannels(id, channels)) => {
                    if let Some(f) = flows.iter_mut().find(|f| f.id == id) {
                        f.channels = channels;
                        f.last_heard_ns = local_now_ns();
                        f.expired = false;
                    }
                }
                Ok(TxCommand::Shutdown) | Err(std_mpsc::TryRecvError::Disconnected) => return,
                Err(std_mpsc::TryRecvError::Empty) => break,
            }
        }

        let Some(snap) = shared.clock.snapshot() else {
            wake_ns = local_now_ns() + NO_CLOCK_WAIT_NS;
            continue;
        };
        let local = local_now_ns();
        let now = ns_to_samples(snap.media_ns_at(local), rate);
        let mut next_due: Option<u64> = None;

        for flow in flows.iter_mut().filter(|f| !f.expired) {
            // Anything the receiver sends us counts as a keepalive.
            while flow.socket.recv(&mut scratch).is_ok() {
                flow.last_heard_ns = local;
            }
            if flow.unicast && local.saturating_sub(flow.last_heard_ns) > KEEPALIVE_TIMEOUT_NS {
                info!("tx flow {} expired (no keepalives)", flow.id);
                flow.expired = true;
                let _ = events.send(TxEvent::Expired(flow.id));
                continue;
            }

            let fpp = flow.fpp;
            let start = (now.saturating_sub(guard) / fpp) * fpp;
            let next = flow.next_ts.get_or_insert(start);
            if now > *next + guard + resync || *next > now + resync {
                warn!("tx flow {}: media clock jumped, resynchronising", flow.id);
                *next = start;
            }
            while *next + guard <= now {
                let ts = *next;
                let missing = flow.channels.iter().flatten().any(|&ch| {
                    shared.tx_rings[ch].read_one(ts).is_none()
                        || shared.tx_rings[ch].read_one(ts + fpp - 1).is_none()
                });
                if missing {
                    let underruns = counters.tx_underruns.fetch_add(1, Ordering::Relaxed) + 1;
                    if underruns.is_power_of_two() {
                        debug!(
                            "tx underrun: no audio from the backend for {ts} ({underruns} so far)"
                        );
                    }
                }
                let len = audio::encode_packet(
                    &mut packet,
                    ts,
                    rate,
                    flow.format,
                    flow.channels.len(),
                    fpp as usize,
                    |frame, slot| match flow.channels[slot] {
                        Some(ch) => shared.tx_rings[ch].read_one(ts + frame as u64).unwrap_or(0),
                        None => 0,
                    },
                );
                match flow.socket.send(&packet[..len]) {
                    Ok(_) => {
                        counters.tx_packets.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        send_errors += 1;
                        if send_errors.is_power_of_two() {
                            debug!("tx flow {} send error ({send_errors} so far): {e}", flow.id);
                        }
                    }
                }
                *next += fpp;
            }
            next_due = Some(next_due.map_or(*next + guard, |d: u64| d.min(*next + guard)));
        }

        // An absolute deadline: on macOS the thread waits for it with
        // mach_wait_until, which wakes it more precisely than a relative sleep.
        let latest = local_now_ns() + MAX_WAIT_NS;
        wake_ns = match next_due {
            Some(ts) => snap.local_ns_at(samples_to_ns(ts, rate)).min(latest),
            None => latest,
        };
    }
}

/// Book-keeping for a flow on the control side.
struct ServerFlow {
    cookie: u32,
    dst: SocketAddrV4,
    fpp: u16,
    format: SampleFormat,
    info: TxFlowInfo,
}

fn handle_of(id: u16, cookie: u32) -> FlowHandle {
    let mut h = [0u8; 6];
    h[..2].copy_from_slice(&id.to_be_bytes());
    h[2..].copy_from_slice(&cookie.to_be_bytes());
    h
}

fn split_handle(h: FlowHandle) -> (u16, u32) {
    (u16::from_be_bytes([h[0], h[1]]), u32::from_be_bytes([h[2], h[3], h[4], h[5]]))
}

/// The flow-control server (port 4455 by default).
pub struct FlowControl {
    shared: Arc<Shared>,
    commands: std_mpsc::Sender<TxCommand>,
    flows: BTreeMap<u16, ServerFlow>,
}

impl FlowControl {
    pub fn new(shared: Arc<Shared>, commands: std_mpsc::Sender<TxCommand>) -> Self {
        Self { shared, commands, flows: BTreeMap::new() }
    }

    pub async fn run(mut self, socket: UdpSocket, mut events: mpsc::UnboundedReceiver<TxEvent>) {
        let mut buf = vec![0u8; 2048];
        loop {
            tokio::select! {
                r = socket.recv_from(&mut buf) => {
                    let Ok((len, src)) = r else { continue };
                    let Ok(frame) = Frame::parse(&buf[..len]) else {
                        debug!("malformed flow-control packet from {src}");
                        continue;
                    };
                    if let Some(resp) = self.handle(&frame) {
                        let _ = socket.send_to(&resp, src).await;
                    }
                }
                ev = events.recv() => match ev {
                    Some(TxEvent::Expired(id)) => self.remove(id),
                    None => return,
                }
            }
        }
    }

    fn publish(&self) {
        let infos = self.flows.values().map(|f| f.info.clone()).collect();
        self.shared.state().tx_flows = infos;
    }

    fn remove(&mut self, id: u16) {
        if self.flows.remove(&id).is_some() {
            let _ = self.commands.send(TxCommand::Remove(id));
            self.publish();
        }
    }

    fn find(&self, handle: FlowHandle) -> Option<u16> {
        let (id, cookie) = split_handle(handle);
        self.flows.get(&id).filter(|f| f.cookie == cookie).map(|_| id)
    }

    /// Validates 1-based channel ids and maps them to ring indices.
    fn map_channels(&self, ids: &[u16]) -> Option<Vec<Option<usize>>> {
        let n = self.shared.info.tx_channels.len();
        ids.iter()
            .map(|&id| match id {
                0 => Some(None),
                id if (id as usize) <= n => Some(Some(id as usize - 1)),
                _ => None,
            })
            .collect()
    }

    pub fn handle(&mut self, frame: &Frame<'_>) -> Option<Vec<u8>> {
        let h = &frame.header;
        if h.result != result::REQUEST {
            return None;
        }
        Some(match h.opcode {
            dbcp::opcode::REQUEST_FLOW => match FlowRequest::decode(frame) {
                Ok(req) => self.request_flow(h, req),
                Err(e) => {
                    warn!("malformed flow request: {e}");
                    respond(h, dbcp::error::INVALID_PARAMETER, &[])
                }
            },
            dbcp::opcode::STOP_FLOW => {
                let handle = dbcp::decode_stop_flow(frame).ok()?;
                match self.find(handle) {
                    Some(id) => {
                        info!("tx flow {id} stopped by receiver");
                        self.remove(id);
                        respond(h, result::SUCCESS, &[])
                    }
                    None => respond(h, dbcp::error::FLOW_NOT_FOUND, &[]),
                }
            }
            dbcp::opcode::UPDATE_FLOW => {
                let (handle, ids) = dbcp::decode_update_flow(frame).ok()?;
                let Some(id) = self.find(handle) else {
                    return Some(respond(h, dbcp::error::FLOW_NOT_FOUND, &[]));
                };
                let Some(channels) = self.map_channels(&ids) else {
                    return Some(respond(h, dbcp::error::INVALID_PARAMETER, &[]));
                };
                let f = self.flows.get_mut(&id)?;
                if packet_size(f.fpp, channels.len(), f.format) > MAX_DATAGRAM {
                    return Some(respond(h, dbcp::error::INVALID_PARAMETER, &[]));
                }
                info!("tx flow {id} now carries channels {ids:?}");
                f.info.channels = ids;
                let _ = self.commands.send(TxCommand::SetChannels(id, channels));
                self.publish();
                respond(h, result::SUCCESS, &[])
            }
            other => {
                debug!("unhandled flow-control opcode {other:#06x}");
                return None;
            }
        })
    }

    fn request_flow(&mut self, h: &Header, req: FlowRequest) -> Vec<u8> {
        let info = &self.shared.info;
        info!(
            "{} requests flow {:?}: channels {:?} at {} Hz/{} bit, {} fpp, to {}:{}",
            req.rx_device,
            req.flow_name,
            req.channels,
            req.sample_rate,
            req.bits_per_sample,
            req.fpp,
            req.rx_addr,
            req.rx_port
        );
        if req.sample_rate != info.sample_rate {
            warn!("refusing flow: sample rate {} != {}", req.sample_rate, info.sample_rate);
            return respond(h, dbcp::error::SAMPLE_RATE_MISMATCH, &[]);
        }
        let Some(format) = SampleFormat::from_bits(req.bits_per_sample) else {
            return respond(h, dbcp::error::INVALID_PARAMETER, &[]);
        };
        let Some(channels) = self.map_channels(&req.channels) else {
            return respond(h, dbcp::error::INVALID_PARAMETER, &[]);
        };
        let size = packet_size(req.fpp, channels.len(), format);
        if req.fpp == 0 || req.fpp > FPP_LIMIT || channels.is_empty() || size > MAX_DATAGRAM {
            return respond(h, dbcp::error::INVALID_PARAMETER, &[]);
        }

        let dst = SocketAddrV4::new(req.rx_addr, req.rx_port);
        if let Some((&id, f)) = self.flows.iter_mut().find(|(_, f)| f.dst == dst) {
            // A receiver re-requesting a flow it already has (e.g. after a
            // restart on its side): reuse it.
            f.info.channels = req.channels.clone();
            let handle = handle_of(id, f.cookie);
            let _ = self.commands.send(TxCommand::SetChannels(id, channels));
            self.publish();
            return dbcp::encode_flow_created(h, handle);
        }
        let Some(id) = (1..=MAX_TX_FLOWS as u16).find(|id| !self.flows.contains_key(id)) else {
            warn!("refusing flow: all {MAX_TX_FLOWS} transmit flows in use");
            return respond(h, dbcp::error::TOO_MANY_FLOWS, &[]);
        };
        let socket = match open_flow_socket(info.iface.ip, dst) {
            Ok(s) => s,
            Err(e) => {
                warn!("cannot open flow socket to {dst}: {e}");
                return respond(h, dbcp::error::INVALID_PARAMETER, &[]);
            }
        };
        let cookie: u32 = rand::random();
        let flow = TxFlow {
            id,
            socket,
            channels,
            fpp: req.fpp as u64,
            format,
            unicast: !req.rx_addr.is_multicast(),
            next_ts: None,
            last_heard_ns: local_now_ns(),
            expired: false,
        };
        if self.commands.send(TxCommand::Add(flow)).is_err() {
            return respond(h, result::FAILURE, &[]);
        }
        self.flows.insert(
            id,
            ServerFlow {
                cookie,
                dst,
                fpp: req.fpp,
                format,
                info: TxFlowInfo {
                    id,
                    local_name: format!("{id}_{}", info.process_id),
                    remote: Some((req.rx_device, req.flow_name)),
                    dst_addr: req.rx_addr,
                    dst_port: req.rx_port,
                    channels: req.channels,
                    fpp: req.fpp,
                },
            },
        );
        self.publish();
        dbcp::encode_flow_created(h, handle_of(id, cookie))
    }
}

fn packet_size(fpp: u16, channels: usize, format: SampleFormat) -> usize {
    audio::HEADER_LEN + fpp as usize * channels * format.bytes()
}

fn open_flow_socket(local_ip: Ipv4Addr, dst: SocketAddrV4) -> std::io::Result<StdUdpSocket> {
    let socket = bind_udp(local_ip, 0)?;
    socket.connect(SocketAddr::V4(dst))?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}
