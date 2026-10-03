//! The PTPv1 follower: a pure protocol engine ([`Core`]) and the tokio task
//! that feeds it from the network ([`run`]).
//!
//! Keeping the protocol logic free of I/O lets the unit tests below drive it
//! with hand-made packets and arbitrary timestamps.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::UdpSocket;
use tracing::{debug, info, trace, warn};

use super::bmc::{MasterChange, MasterDataset, MasterSelector};
use super::net::{Endpoints, RECV_BUF_LEN, SplitMix64, recv_timestamped, source_ipv4};
use super::servo::{Servo, ServoConfig, ServoEvent, ServoState};
use super::wire::{
    Body, DecodeError, DelayRespBody, Flags, FollowUpBody, Header, Message, PortIdentity,
    Subdomain, SyncBody, Timestamp,
};
use crate::{ClockSnapshot, ClockState, ClockStatus, ClockWriter, MasterInfo, local_now_ns};

/// Our PTP port number. A follower has exactly one port.
pub(crate) const OUR_PORT_ID: u16 = 1;

/// Clock variance we advertise in Delay_Req messages (ptpd's PTPv1 default;
/// nobody uses this field of a Delay_Req).
const OUR_CLOCK_VARIANCE: i16 = -4000;

/// How often timers are checked when nothing else happens.
const HOUSEKEEPING_NS: u64 = 100_000_000;

/// Delay_Req interval while the servo wants measurements quickly.
const FAST_DELAY_REQ_INTERVAL_NS: u64 = 250_000_000;

/// Settings of the protocol engine.
#[derive(Clone, Debug)]
pub(crate) struct CoreConfig {
    pub uuid: [u8; 6],
    pub subdomain: Subdomain,
    pub delay_req_interval: Duration,
    pub master_timeout: Duration,
    pub holdover: Duration,
    pub servo: ServoConfig,
    /// Seed for the Delay_Req interval jitter.
    pub seed: u64,
}

/// What the engine wants done to the published clock.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Output {
    Publish(ClockSnapshot),
    Invalidate,
    Status(ClockStatus),
}

/// Local and master times of one Sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SyncTimes {
    t1: u64,
    t2: u64,
}

/// A two-step Sync waiting for its Follow_Up.
#[derive(Clone, Copy, Debug)]
struct PendingSync {
    seq: u16,
    t2: u64,
}

/// A Delay_Req waiting for its Delay_Resp.
#[derive(Clone, Copy, Debug)]
struct OutstandingDelayReq {
    seq: u16,
    t3: u64,
    /// The Sync the delay is computed against (the latest one before `t3`).
    sync: SyncTimes,
}

/// The follower's protocol engine.
pub(crate) struct Core {
    config: CoreConfig,
    selector: MasterSelector,
    servo: Servo,
    /// The master the servo's state belongs to.
    servo_master: Option<PortIdentity>,
    rng: SplitMix64,
    pending_sync: Option<PendingSync>,
    /// A Follow_Up that arrived before its Sync (they come in on different
    /// sockets, so the order in which we read them is not guaranteed):
    /// `(sequence id, precise origin time)`.
    early_follow_up: Option<(u16, u64)>,
    last_sync: Option<SyncTimes>,
    /// The current master's last Sync body; our Delay_Reqs echo its fields.
    sync_template: Option<(Header, SyncBody)>,
    delay_req_seq: u16,
    outstanding: Option<OutstandingDelayReq>,
    next_delay_req_ns: Option<u64>,
    /// When the published (holdover) snapshot must be invalidated, if no
    /// fresh one replaces it first.
    holdover_deadline_ns: Option<u64>,
    last_state: ClockState,
}

impl Core {
    pub fn new(config: CoreConfig) -> Core {
        Core {
            selector: MasterSelector::new(config.master_timeout),
            servo: Servo::new(config.servo.clone()),
            servo_master: None,
            rng: SplitMix64::new(config.seed),
            pending_sync: None,
            early_follow_up: None,
            last_sync: None,
            sync_template: None,
            delay_req_seq: 0,
            outstanding: None,
            next_delay_req_ns: None,
            holdover_deadline_ns: None,
            last_state: ClockState::Unlocked,
            config,
        }
    }

    fn our_identity(&self) -> PortIdentity {
        PortIdentity { uuid: self.config.uuid, port_id: OUR_PORT_ID }
    }

    fn is_current(&self, source: PortIdentity) -> bool {
        self.selector.current().is_some_and(|m| m.dataset.source == source)
    }

    /// Handles a datagram received at local time `t_recv`.
    pub fn handle_packet(
        &mut self,
        buf: &[u8],
        src: SocketAddr,
        t_recv: u64,
        out: &mut Vec<Output>,
    ) {
        let msg = match Message::decode(buf) {
            Ok(msg) => msg,
            Err(DecodeError::PtpV2) => {
                trace!(%src, "ignoring PTPv2 packet");
                return;
            }
            Err(e) => {
                debug!(%src, "ignoring packet: {e}");
                return;
            }
        };
        let h = msg.header;
        if h.subdomain != self.config.subdomain || h.source.uuid == self.config.uuid {
            return;
        }
        match msg.body {
            Body::Sync(body) => {
                let Some(addr) = source_ipv4(src) else { return };
                self.on_sync(&h, &body, addr, t_recv, out);
            }
            Body::FollowUp(body) => self.on_follow_up(&h, &body, t_recv, out),
            Body::DelayResp(body) => self.on_delay_resp(&h, &body, out),
            // Other followers' requests are none of our business.
            Body::DelayReq(_) => {}
        }
    }

    fn on_sync(
        &mut self,
        h: &Header,
        body: &SyncBody,
        addr: Ipv4Addr,
        t2: u64,
        out: &mut Vec<Output>,
    ) {
        let dataset = MasterDataset::from_sync(h, body);
        if let Some(change) = self.selector.on_sync(dataset, addr, t2) {
            self.on_master_change(change, t2, out);
        }
        if !self.is_current(h.source) {
            return;
        }
        self.sync_template = Some((*h, *body));
        if h.flags.contains(Flags::ASSIST) {
            // Two-step: the precise origin time comes in the Follow_Up.
            match self.early_follow_up.take() {
                Some((seq, t1)) if seq == h.sequence_id => {
                    self.pending_sync = None;
                    self.complete_sync(t1, t2, t2, out);
                }
                _ => self.pending_sync = Some(PendingSync { seq: h.sequence_id, t2 }),
            }
        } else {
            self.pending_sync = None;
            self.complete_sync(ts_ns(body.origin_timestamp), t2, t2, out);
        }
    }

    fn on_follow_up(
        &mut self,
        h: &Header,
        body: &FollowUpBody,
        t_recv: u64,
        out: &mut Vec<Output>,
    ) {
        if !self.is_current(h.source) {
            return;
        }
        let t1 = ts_ns(body.precise_origin_timestamp);
        match self.pending_sync {
            Some(p) if p.seq == body.associated_sequence_id => {
                self.pending_sync = None;
                self.complete_sync(t1, p.t2, t_recv, out);
            }
            _ => self.early_follow_up = Some((body.associated_sequence_id, t1)),
        }
    }

    fn complete_sync(&mut self, t1: u64, t2: u64, now: u64, out: &mut Vec<Output>) {
        let sync = SyncTimes { t1, t2 };
        self.last_sync = Some(sync);
        // Measure the path delay as soon as there is a Sync to pair with.
        self.next_delay_req_ns.get_or_insert(now);

        let event = self.servo.sync(t1, t2, now);
        trace!(t1, t2, ?event, "sync");
        let publish = match event {
            ServoEvent::Collecting | ServoEvent::Held { .. } => false,
            ServoEvent::Stepped { offset_ns: None } => {
                info!(
                    freq_offset_ppm = self.servo.freq_offset_ppb() / 1e3,
                    path_delay_ns = self.servo.path_delay_ns(),
                    "PTP clock acquired"
                );
                true
            }
            ServoEvent::Stepped { offset_ns: Some(e) } => {
                warn!(
                    offset_ns = e,
                    "PTP clock error above step threshold; stepping the media clock"
                );
                true
            }
            ServoEvent::Slewed { .. } => true,
        };
        if publish {
            if let Some(snap) = self.servo.snapshot() {
                self.holdover_deadline_ns = None;
                out.push(Output::Publish(snap));
            }
        }
        self.push_status(out);
    }

    fn on_delay_resp(&mut self, h: &Header, body: &DelayRespBody, out: &mut Vec<Output>) {
        if !self.is_current(h.source) || body.requesting_source != self.our_identity() {
            return;
        }
        let Some(req) = self.outstanding else { return };
        if req.seq != body.requesting_source_sequence_id {
            return;
        }
        self.outstanding = None;
        let t4 = ts_ns(body.delay_receipt_timestamp);
        let raw = self.servo.delay_measurement(req.sync.t1, req.sync.t2, req.t3, t4);
        trace!(raw_delay_ns = raw, filtered_ns = self.servo.path_delay_ns(), "delay response");
        self.push_status(out);
    }

    fn on_master_change(&mut self, change: MasterChange, now: u64, out: &mut Vec<Output>) {
        // Protocol state belongs to the previous master.
        self.pending_sync = None;
        self.early_follow_up = None;
        self.last_sync = None;
        self.sync_template = None;
        self.outstanding = None;
        self.next_delay_req_ns = None;

        match change.current.and_then(|_| self.selector.current().copied()) {
            Some(master) => {
                let id = master.dataset.source;
                info!(
                    master = %fmt_uuid(&id.uuid),
                    port = id.port_id,
                    addr = %master.addr,
                    stratum = master.dataset.stratum,
                    identifier = %String::from_utf8_lossy(&master.dataset.identifier),
                    "following PTP master"
                );
                if self.servo_master != Some(id) {
                    // A different clock: start over, but keep showing the old
                    // time until the new master has been acquired.
                    self.begin_holdover(now, out);
                    self.servo.reset();
                    self.servo_master = Some(id);
                }
            }
            None => {
                warn!(
                    timeout = ?self.config.master_timeout,
                    holdover = ?self.config.holdover,
                    "lost PTP master; media clock in holdover"
                );
                self.begin_holdover(now, out);
            }
        }
        self.push_status(out);
    }

    fn begin_holdover(&mut self, now: u64, out: &mut Vec<Output>) {
        if let Some(snap) = self.servo.enter_holdover(now) {
            out.push(Output::Publish(snap));
            self.holdover_deadline_ns
                .get_or_insert(now.saturating_add(duration_ns(self.config.holdover)));
        }
    }

    /// Runs timers: master timeout and holdover expiry.
    pub fn poll(&mut self, now: u64, out: &mut Vec<Output>) {
        if let Some(change) = self.selector.expire(now) {
            self.on_master_change(change, now, out);
        }
        if self.holdover_deadline_ns.is_some_and(|d| now >= d) {
            warn!("PTP holdover expired; media clock invalid");
            self.holdover_deadline_ns = None;
            if self.selector.current().is_none() {
                // Nobody to follow: start from scratch when a master appears.
                // (With a master, the servo is busy acquiring or tracking it
                // and will publish again as soon as it can.)
                self.servo.reset();
                self.servo_master = None;
            }
            out.push(Output::Invalidate);
            self.push_status(out);
        }
    }

    /// Local time by which [`poll`](Self::poll) or a Delay_Req is due.
    pub fn next_deadline(&self, now: u64) -> u64 {
        let housekeeping = now.saturating_add(HOUSEKEEPING_NS);
        self.next_delay_req_ns.map_or(housekeeping, |t| t.min(housekeeping))
    }

    /// Whether a Delay_Req should be sent now.
    pub fn delay_req_due(&self, now: u64) -> bool {
        self.next_delay_req_ns.is_some_and(|t| now >= t)
            && self.last_sync.is_some()
            && self.sync_template.is_some()
    }

    /// Builds the next Delay_Req, to be sent at local time `t3`.
    ///
    /// `t3` must be read immediately *before* the packet is handed to the
    /// socket: the receive side timestamps after the packet arrived, so
    /// timestamping before sending makes the software latencies of both
    /// directions bias the path delay in the same direction, where they
    /// largely cancel in the offset.
    pub fn make_delay_req(&mut self, t3: u64) -> Option<Vec<u8>> {
        let sync = self.last_sync?;
        let (master_header, template) = self.sync_template?;
        self.delay_req_seq = self.delay_req_seq.wrapping_add(1);
        let seq = self.delay_req_seq;
        self.outstanding = Some(OutstandingDelayReq { seq, t3, sync });

        // Until the servo has a few path delay measurements, ask faster: they
        // set the initial phase. Randomise the interval by ±25% so that
        // followers do not synchronise their requests.
        let mut interval = duration_ns(self.config.delay_req_interval);
        if self.servo.wants_fast_delay_requests() {
            interval = interval.min(FAST_DELAY_REQ_INTERVAL_NS);
        }
        let interval = interval as f64;
        let factor = 0.75 + 0.5 * self.rng.next_f64();
        self.next_delay_req_ns = Some(t3.saturating_add((interval * factor) as u64));

        let origin = self.servo.snapshot().map_or(0, |s| s.media_ns_at(t3));
        let body = SyncBody {
            origin_timestamp: Timestamp::from_ns(origin),
            local_clock_variance: OUR_CLOCK_VARIANCE,
            local_steps_removed: template.local_steps_removed.saturating_add(1),
            local_clock_stratum: 255,
            local_clock_identifier: *b"DFLT",
            // Our parent is the port we synchronise to.
            parent_communication_technology: master_header.source_communication_technology,
            parent_uuid: master_header.source.uuid,
            parent_port_field: master_header.source.port_id,
            // Epoch, UTC offset, grandmaster fields, sync interval and master
            // statistics are echoed from the master's Sync.
            ..template
        };
        let header = Header::new(self.config.subdomain, self.our_identity(), seq, Flags::empty());
        Some(Message { header, body: Body::DelayReq(body) }.encode())
    }

    pub fn status(&self) -> ClockStatus {
        let master = self.selector.current();
        ClockStatus {
            state: match (master, self.servo.state()) {
                (None, _) => ClockState::Unlocked,
                (Some(_), ServoState::Locked) => ClockState::Locked,
                (Some(_), _) => ClockState::Locking,
            },
            master: master.map(|m| MasterInfo {
                uuid: m.dataset.source.uuid,
                port_id: m.dataset.source.port_id,
                addr: m.addr,
            }),
            offset_ns: self.servo.offset_ns(),
            mean_path_delay_ns: self.servo.path_delay_ns().unwrap_or(0),
            freq_offset_ppb: self.servo.freq_offset_ppb(),
        }
    }

    fn push_status(&mut self, out: &mut Vec<Output>) {
        let status = self.status();
        if status.state != self.last_state {
            info!(from = ?self.last_state, to = ?status.state, offset_ns = status.offset_ns, "PTP clock state");
            self.last_state = status.state;
        }
        out.push(Output::Status(status));
    }
}

/// A PTP timestamp as non-negative nanoseconds.
fn ts_ns(ts: Timestamp) -> u64 {
    ts.to_ns().max(0) as u64
}

fn duration_ns(d: Duration) -> u64 {
    d.as_nanos().min(u64::MAX as u128) as u64
}

pub(crate) fn fmt_uuid(uuid: &[u8; 6]) -> String {
    uuid.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

/// The follower's handle on the [`ClockWriter`].
///
/// Once stopped, nothing more is published, even if the task is still
/// finishing its current iteration on another thread. The mutex is only
/// taken by the follower task and by shutdown, never by clock readers.
#[derive(Debug)]
pub(crate) struct ClockOutput {
    writer: ClockWriter,
    stopped: Mutex<bool>,
}

impl ClockOutput {
    pub fn new(writer: ClockWriter) -> ClockOutput {
        ClockOutput { writer, stopped: Mutex::new(false) }
    }

    pub fn apply(&self, output: Output) {
        let stopped = self.stopped.lock().unwrap_or_else(|e| e.into_inner());
        if *stopped {
            return;
        }
        match output {
            Output::Publish(snap) => self.writer.publish(snap),
            Output::Invalidate => self.writer.invalidate(),
            Output::Status(status) => self.writer.set_status(status),
        }
    }

    /// Invalidates the clock for good.
    pub fn stop(&self) {
        let mut stopped = self.stopped.lock().unwrap_or_else(|e| e.into_inner());
        *stopped = true;
        self.writer.invalidate();
        self.writer.set_status(ClockStatus::default());
    }
}

/// The follower task: receives packets, timestamps them, runs timers and
/// sends Delay_Reqs. Runs until aborted.
pub(crate) async fn run(
    mut core: Core,
    event: UdpSocket,
    general: UdpSocket,
    endpoints: Endpoints,
    output: Arc<ClockOutput>,
) {
    let mut event_buf = vec![0u8; RECV_BUF_LEN];
    let mut general_buf = vec![0u8; RECV_BUF_LEN];
    let mut out = Vec::new();
    loop {
        let now = local_now_ns();
        let wait = core.next_deadline(now).saturating_sub(now);
        let sleep = tokio::time::sleep(Duration::from_nanos(wait));
        let mut backoff = false;
        // `biased`: the event socket is polled first, so when both are ready
        // a Sync gets its receive timestamp before a Follow_Up is processed.
        tokio::select! {
            biased;
            r = recv_timestamped(&event, &mut event_buf) => match r {
                Ok((n, src, t)) => core.handle_packet(&event_buf[..n], src, t, &mut out),
                Err(e) => backoff = recv_error(e),
            },
            r = recv_timestamped(&general, &mut general_buf) => match r {
                Ok((n, src, t)) => core.handle_packet(&general_buf[..n], src, t, &mut out),
                Err(e) => backoff = recv_error(e),
            },
            () = sleep => {}
        }
        core.poll(local_now_ns(), &mut out);
        for o in out.drain(..) {
            output.apply(o);
        }

        if core.delay_req_due(local_now_ns()) {
            let t3 = local_now_ns();
            if let Some(packet) = core.make_delay_req(t3) {
                if let Err(e) = event.send_to(&packet, endpoints.event_dest()).await {
                    warn!("cannot send PTP Delay_Req: {e}");
                }
            }
        }
        if backoff {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Logs a receive error; returns whether to back off before retrying.
fn recv_error(e: io::Error) -> bool {
    // Windows reports ICMP port unreachable for earlier sends as a receive
    // error; it is harmless.
    if e.kind() == io::ErrorKind::ConnectionReset {
        debug!("PTP socket: {e}");
        false
    } else {
        warn!("PTP socket receive error: {e}");
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptp::wire::{COMM_TECH_ETHERNET, Control};

    const MS: u64 = 1_000_000;
    const OUR_UUID: [u8; 6] = [2, 0, 0, 0, 0, 1];
    const MASTER: PortIdentity = PortIdentity { uuid: [0, 0x1d, 0xc1, 1, 2, 3], port_id: 1 };
    const MEDIA0: u64 = 1_700_000_000_000_000_000;

    fn master_addr() -> SocketAddr {
        "192.168.1.10:319".parse().unwrap()
    }

    fn core() -> Core {
        Core::new(CoreConfig {
            uuid: OUR_UUID,
            subdomain: Subdomain::DEFAULT,
            delay_req_interval: Duration::from_secs(1),
            master_timeout: Duration::from_secs(5),
            holdover: Duration::from_secs(30),
            // These tests are about the protocol, not the servo: acquire
            // after a second of Syncs, with or without delay measurements.
            servo: ServoConfig {
                acquire_min_span_ns: 1_000_000_000,
                acquire_min_delay_samples: 0,
                ..ServoConfig::default()
            },
            seed: 1,
        })
    }

    fn sync_body(t1: u64) -> SyncBody {
        SyncBody {
            origin_timestamp: Timestamp::from_ns(t1),
            grandmaster_communication_technology: COMM_TECH_ETHERNET,
            grandmaster_clock_uuid: MASTER.uuid,
            grandmaster_port_id: 1,
            grandmaster_clock_stratum: 4,
            grandmaster_clock_identifier: *b"DFLT",
            sync_interval: -2,
            local_clock_stratum: 4,
            local_clock_identifier: *b"DFLT",
            parent_communication_technology: COMM_TECH_ETHERNET,
            parent_uuid: MASTER.uuid,
            parent_port_field: 1,
            ..SyncBody::default()
        }
    }

    fn packet(source: PortIdentity, seq: u16, flags: Flags, body: Body) -> Vec<u8> {
        Message { header: Header::new(Subdomain::DEFAULT, source, seq, flags), body }.encode()
    }

    fn two_step_sync(seq: u16, t1: u64) -> (Vec<u8>, Vec<u8>) {
        let sync = packet(MASTER, seq, Flags::ASSIST, Body::Sync(sync_body(t1 - 1_000)));
        let fu = packet(
            MASTER,
            seq,
            Flags::empty(),
            Body::FollowUp(FollowUpBody {
                associated_sequence_id: seq,
                precise_origin_timestamp: Timestamp::from_ns(t1),
            }),
        );
        (sync, fu)
    }

    fn publishes(out: &[Output]) -> usize {
        out.iter().filter(|o| matches!(o, Output::Publish(_))).count()
    }

    fn last_status(out: &[Output]) -> ClockStatus {
        out.iter()
            .rev()
            .find_map(|o| if let Output::Status(s) = o { Some(*s) } else { None })
            .expect("no status output")
    }

    #[test]
    fn one_step_syncs_acquire_the_clock() {
        let mut core = core();
        let mut out = Vec::new();
        for i in 0..5u64 {
            let t2 = 1_000 * MS + i * 250 * MS;
            let p = packet(MASTER, i as u16, Flags::empty(), Body::Sync(sync_body(MEDIA0 + t2)));
            core.handle_packet(&p, master_addr(), t2, &mut out);
        }
        assert_eq!(publishes(&out), 1, "{out:?}");
        let status = last_status(&out);
        assert_eq!(status.state, ClockState::Locking);
        let master = status.master.unwrap();
        assert_eq!((master.uuid, master.port_id), (MASTER.uuid, MASTER.port_id));
        assert_eq!(master.addr, Ipv4Addr::new(192, 168, 1, 10));
    }

    #[test]
    fn two_step_sync_pairs_with_follow_up_in_either_order() {
        let mut core = core();
        let mut out = Vec::new();
        for i in 0..5u64 {
            let t2 = 1_000 * MS + i * 250 * MS;
            let (sync, fu) = two_step_sync(i as u16, MEDIA0 + t2);
            if i % 2 == 0 {
                core.handle_packet(&sync, master_addr(), t2, &mut out);
                core.handle_packet(&fu, master_addr(), t2 + 100_000, &mut out);
            } else {
                core.handle_packet(&fu, master_addr(), t2 - 10_000, &mut out);
                core.handle_packet(&sync, master_addr(), t2, &mut out);
            }
        }
        assert_eq!(publishes(&out), 1);
        // The precise origin time (not the Sync's estimate) was used.
        assert_eq!(core.last_sync, Some(SyncTimes { t1: MEDIA0 + 2_000 * MS, t2: 2_000 * MS }));

        // A Follow_Up with the wrong sequence id does not complete a Sync.
        let (sync, _) = two_step_sync(10, MEDIA0 + 3_000 * MS);
        let (_, fu) = two_step_sync(11, MEDIA0 + 3_000 * MS);
        core.handle_packet(&sync, master_addr(), 3_000 * MS, &mut out);
        core.handle_packet(&fu, master_addr(), 3_000 * MS, &mut out);
        assert_eq!(core.last_sync.unwrap().t2, 2_000 * MS);
    }

    #[test]
    fn ignores_own_packets_other_subdomains_and_garbage() {
        let mut core = core();
        let mut out = Vec::new();
        let own = PortIdentity { uuid: OUR_UUID, port_id: 1 };
        core.handle_packet(
            &packet(own, 1, Flags::empty(), Body::Sync(sync_body(MEDIA0))),
            master_addr(),
            MS,
            &mut out,
        );
        let other = Message {
            header: Header::new(Subdomain::new("_ALT1").unwrap(), MASTER, 1, Flags::empty()),
            body: Body::Sync(sync_body(MEDIA0)),
        };
        core.handle_packet(&other.encode(), master_addr(), MS, &mut out);
        core.handle_packet(&[0, 2, 0, 0], master_addr(), MS, &mut out);
        core.handle_packet(&[0, 1], master_addr(), MS, &mut out);
        assert!(out.is_empty(), "{out:?}");
        assert!(core.selector.current().is_none());
    }

    #[test]
    fn delay_request_and_response() {
        let mut core = core();
        let mut out = Vec::new();
        let t2 = 1_000 * MS;
        let (sync, fu) = two_step_sync(7, MEDIA0 + t2 - 50_000);
        core.handle_packet(&sync, master_addr(), t2, &mut out);
        assert!(!core.delay_req_due(t2));
        core.handle_packet(&fu, master_addr(), t2 + 10_000, &mut out);
        assert!(core.delay_req_due(t2 + 20_000));

        let t3 = t2 + 20_000;
        let req = Message::decode(&core.make_delay_req(t3).unwrap()).unwrap();
        assert_eq!(req.control(), Control::DelayReq);
        assert_eq!(req.header.source, PortIdentity { uuid: OUR_UUID, port_id: OUR_PORT_ID });
        let Body::DelayReq(body) = req.body else { unreachable!() };
        assert_eq!(body.local_clock_stratum, 255);
        assert_eq!(&body.local_clock_identifier, b"DFLT");
        assert_eq!(body.grandmaster_clock_uuid, MASTER.uuid);
        assert_eq!(body.grandmaster_clock_stratum, 4);
        assert_eq!(body.parent_uuid, MASTER.uuid);
        assert_eq!(body.sync_interval, -2);
        // Without path delay measurements yet, the next request comes after
        // 250 ms ±25% instead of the configured second.
        assert!(!core.delay_req_due(t3 + 180 * MS));
        assert!(core.delay_req_due(t3 + 320 * MS));

        let resp = |requester: PortIdentity, seq: u16| {
            packet(
                MASTER,
                99,
                Flags::empty(),
                Body::DelayResp(DelayRespBody {
                    delay_receipt_timestamp: Timestamp::from_ns(MEDIA0 + t3 + 50_000),
                    requesting_source_communication_technology: COMM_TECH_ETHERNET,
                    requesting_source: requester,
                    requesting_source_sequence_id: seq,
                }),
            )
        };
        // Responses to someone else, or to another request, are ignored.
        let someone = PortIdentity { uuid: [9; 6], port_id: 1 };
        core.handle_packet(&resp(someone, req.header.sequence_id), master_addr(), t3, &mut out);
        core.handle_packet(
            &resp(req.header.source, req.header.sequence_id + 1),
            master_addr(),
            t3,
            &mut out,
        );
        assert_eq!(core.servo.path_delay_ns(), None);

        out.clear();
        core.handle_packet(
            &resp(req.header.source, req.header.sequence_id),
            master_addr(),
            t3,
            &mut out,
        );
        assert_eq!(last_status(&out).mean_path_delay_ns, 50_000);
    }

    #[test]
    fn master_loss_holds_over_then_invalidates() {
        let mut core = core();
        let mut out = Vec::new();
        let mut t2 = 0;
        for i in 0..8u64 {
            t2 = 1_000 * MS + i * 250 * MS;
            let p = packet(MASTER, i as u16, Flags::empty(), Body::Sync(sync_body(MEDIA0 + t2)));
            core.handle_packet(&p, master_addr(), t2, &mut out);
        }
        assert!(core.servo.snapshot().is_some());

        out.clear();
        core.poll(t2 + 4_000 * MS, &mut out);
        assert!(out.iter().all(|o| matches!(o, Output::Status(_))), "{out:?}");

        out.clear();
        let lost = t2 + 5_001 * MS;
        core.poll(lost, &mut out);
        assert_eq!(publishes(&out), 1, "holdover snapshot is published");
        assert_eq!(last_status(&out).state, ClockState::Unlocked);
        assert!(last_status(&out).master.is_none());

        out.clear();
        core.poll(lost + 29_000 * MS, &mut out);
        assert!(!out.contains(&Output::Invalidate));
        core.poll(lost + 30_000 * MS, &mut out);
        assert!(out.contains(&Output::Invalidate));
        assert!(core.servo.snapshot().is_none());
    }
}
