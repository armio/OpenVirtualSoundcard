//! A minimal PTPv1 master, **for development and testing only**.
//!
//! [`TestMaster`] lets OpenVirtualSoundcard run without any Dante hardware: it sends
//! two-step Sync + Follow_Up messages at a fixed interval and answers
//! Delay_Req messages, using any time source (typically
//! [`crate::system_clock`]).
//!
//! It is deliberately not a real PTP master and must never take part in a
//! real network's clock election:
//!
//! * it advertises the worst possible clock quality (stratum 255, identifier
//!   `DFLT`, maximum variance, not preferred), so every real master beats it;
//! * it goes silent for good, with a warning, as soon as it hears a Sync
//!   from any other master (PTPv1 in any subdomain, or PTPv2 Sync/Announce).
//!
//! The second rule also means two test masters on one network silence each
//! other: run at most one.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};

use super::follower::fmt_uuid;
use super::net::{Endpoints, RECV_BUF_LEN, source_ipv4};
use super::parse_subdomain;
use super::wire::{
    self, Body, COMM_TECH_ETHERNET, DelayRespBody, Flags, FollowUpBody, Header, Message,
    PortIdentity, Subdomain, SyncBody, Timestamp,
};
use crate::MediaClock;

/// Length of the PTPv2 common header.
const PTP_V2_HEADER_LEN: usize = 34;

/// Configuration of a [`TestMaster`].
#[derive(Clone, Debug)]
pub struct TestMasterConfig {
    /// IPv4 address of the interface to send on.
    pub interface: Ipv4Addr,
    /// The master's PTPv1 UUID. Must differ from every follower's.
    pub uuid: [u8; 6],
    /// PTPv1 subdomain name.
    pub subdomain: String,
    /// UDP port of event messages. Standard: 319.
    pub event_port: u16,
    /// UDP port of general messages. Standard: 320.
    pub general_port: u16,
    /// Multicast group. Standard: 224.0.1.129.
    pub multicast_group: Ipv4Addr,
    /// Interval between Sync messages.
    pub sync_interval: Duration,
}

impl TestMasterConfig {
    /// Standard ports and group, a Sync every 250 ms.
    pub fn new(interface: Ipv4Addr, uuid: [u8; 6]) -> Self {
        TestMasterConfig {
            interface,
            uuid,
            subdomain: "_DFLT".to_owned(),
            event_port: 319,
            general_port: 320,
            multicast_group: Ipv4Addr::new(224, 0, 1, 129),
            sync_interval: Duration::from_millis(250),
        }
    }
}

/// A running development-only PTPv1 master. See the [module
/// documentation](self) for its safety rules.
///
/// Dropping it (or calling [`shutdown`](Self::shutdown)) stops it.
#[derive(Debug)]
pub struct TestMaster {
    task: tokio::task::JoinHandle<()>,
    silenced: Arc<AtomicBool>,
}

impl TestMaster {
    /// Starts a master that distributes the time of `clock` (e.g.
    /// [`crate::system_clock`]). Must be called within a tokio runtime.
    pub async fn start(config: TestMasterConfig, clock: MediaClock) -> io::Result<TestMaster> {
        Self::start_with_time_source(config, move || clock.now_ns()).await
    }

    /// Starts a master whose time, in nanoseconds, is returned by `now`.
    /// While `now` returns `None` no Sync messages are sent.
    pub async fn start_with_time_source<F>(
        config: TestMasterConfig,
        now: F,
    ) -> io::Result<TestMaster>
    where
        F: Fn() -> Option<u64> + Send + 'static,
    {
        let subdomain = parse_subdomain(&config.subdomain)?;
        if config.sync_interval.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sync interval must not be zero",
            ));
        }
        let endpoints = Endpoints {
            interface: config.interface,
            group: config.multicast_group,
            event_port: config.event_port,
            general_port: config.general_port,
        };
        let (event, general) = endpoints.bind()?;
        let silenced = Arc::new(AtomicBool::new(false));
        warn!(
            uuid = %fmt_uuid(&config.uuid),
            interface = %config.interface,
            "starting the development PTPv1 test master; do not use it on a production Dante network"
        );
        let master = MasterLoop {
            identity: PortIdentity { uuid: config.uuid, port_id: 1 },
            subdomain,
            sync_interval: config.sync_interval,
            endpoints,
            event,
            general,
            now,
            silenced: silenced.clone(),
            sync_seq: 0,
            general_seq: 0,
        };
        let task = tokio::spawn(master.run());
        Ok(TestMaster { task, silenced })
    }

    /// Whether the master has gone silent because another master appeared.
    pub fn is_silenced(&self) -> bool {
        self.silenced.load(Ordering::Relaxed)
    }

    /// Stops the master.
    pub fn shutdown(self) {
        drop(self);
    }
}

impl Drop for TestMaster {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Logs a receive error and backs off briefly, so a persistent error cannot
/// spin the task.
async fn recv_error(e: io::Error) {
    debug!("test master receive error: {e}");
    if e.kind() != io::ErrorKind::ConnectionReset {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

struct MasterLoop<F> {
    identity: PortIdentity,
    subdomain: Subdomain,
    sync_interval: Duration,
    endpoints: Endpoints,
    event: UdpSocket,
    general: UdpSocket,
    now: F,
    silenced: Arc<AtomicBool>,
    sync_seq: u16,
    general_seq: u16,
}

impl<F: Fn() -> Option<u64> + Send + 'static> MasterLoop<F> {
    async fn run(mut self) {
        let mut ticker = tokio::time::interval(self.sync_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut event_buf = vec![0u8; RECV_BUF_LEN];
        let mut general_buf = vec![0u8; RECV_BUF_LEN];
        loop {
            tokio::select! {
                biased;
                r = self.event.recv_from(&mut event_buf) => {
                    // Receive timestamp (t4 for Delay_Req): right after recv.
                    let t4 = (self.now)();
                    match r {
                        Ok((n, src)) => self.on_event(&event_buf[..n], src, t4).await,
                        Err(e) => recv_error(e).await,
                    }
                }
                r = self.general.recv_from(&mut general_buf) => match r {
                    Ok((n, src)) => {
                        self.check_foreign_master(&general_buf[..n], src);
                    }
                    Err(e) => recv_error(e).await,
                },
                _ = ticker.tick() => self.send_sync().await,
            }
        }
    }

    fn silent(&self) -> bool {
        self.silenced.load(Ordering::Relaxed)
    }

    /// Goes silent if `buf` is a Sync (or PTPv2 Announce) from another
    /// master. Returns whether it was one.
    fn check_foreign_master(&self, buf: &[u8], src: SocketAddr) -> bool {
        let foreign = if wire::is_ptp_v1(buf) {
            matches!(
                Message::decode(buf),
                Ok(Message { header, body: Body::Sync(_) }) if header.source.uuid != self.identity.uuid
            )
        } else if wire::is_ptp_v2(buf) && buf.len() >= PTP_V2_HEADER_LEN {
            // PTPv2 messageType: 0x0 Sync, 0xB Announce.
            matches!(buf[0] & 0x0f, 0x0 | 0xb)
        } else {
            false
        };
        if foreign && !self.silenced.swap(true, Ordering::Relaxed) {
            warn!(
                from = ?source_ipv4(src),
                "another PTP master is active; the test master goes silent for good"
            );
        }
        foreign
    }

    async fn on_event(&mut self, buf: &[u8], src: SocketAddr, t4: Option<u64>) {
        if self.check_foreign_master(buf, src) || self.silent() {
            return;
        }
        let Ok(Message { header, body: Body::DelayReq(_) }) = Message::decode(buf) else { return };
        if header.subdomain != self.subdomain || header.source.uuid == self.identity.uuid {
            return;
        }
        let Some(t4) = t4 else { return };
        self.general_seq = self.general_seq.wrapping_add(1);
        let resp = Message {
            header: Header::new(self.subdomain, self.identity, self.general_seq, Flags::empty()),
            body: Body::DelayResp(DelayRespBody {
                delay_receipt_timestamp: Timestamp::from_ns(t4),
                requesting_source_communication_technology: header.source_communication_technology,
                requesting_source: header.source,
                requesting_source_sequence_id: header.sequence_id,
            }),
        };
        if let Err(e) = self.general.send_to(&resp.encode(), self.endpoints.general_dest()).await {
            warn!("test master cannot send Delay_Resp: {e}");
        }
    }

    async fn send_sync(&mut self) {
        if self.silent() {
            return;
        }
        self.sync_seq = self.sync_seq.wrapping_add(1);
        let seq = self.sync_seq;
        // Origin timestamp: read right before sending, like the follower's
        // Delay_Req, so software latencies are symmetric.
        let Some(t1) = (self.now)() else { return };
        let sync = Message {
            header: Header::new(self.subdomain, self.identity, seq, Flags::ASSIST),
            body: Body::Sync(self.sync_body(seq, t1)),
        };
        if let Err(e) = self.event.send_to(&sync.encode(), self.endpoints.event_dest()).await {
            warn!("test master cannot send Sync: {e}");
            return;
        }
        self.general_seq = self.general_seq.wrapping_add(1);
        let follow_up = Message {
            header: Header::new(self.subdomain, self.identity, self.general_seq, Flags::empty()),
            body: Body::FollowUp(FollowUpBody {
                associated_sequence_id: seq,
                precise_origin_timestamp: Timestamp::from_ns(t1),
            }),
        };
        if let Err(e) =
            self.general.send_to(&follow_up.encode(), self.endpoints.general_dest()).await
        {
            warn!("test master cannot send Follow_Up: {e}");
        }
        if seq == 1 {
            info!(uuid = %fmt_uuid(&self.identity.uuid), "test master sending Sync messages");
        }
    }

    /// A Sync body advertising the worst possible clock, so that any real
    /// master wins the election.
    fn sync_body(&self, seq: u16, t1: u64) -> SyncBody {
        let log_interval = self.sync_interval.as_secs_f64().log2().round().clamp(-7.0, 7.0) as i8;
        SyncBody {
            origin_timestamp: Timestamp::from_ns(t1),
            epoch_number: 0,
            current_utc_offset: 0,
            grandmaster_communication_technology: COMM_TECH_ETHERNET,
            grandmaster_clock_uuid: self.identity.uuid,
            grandmaster_port_id: self.identity.port_id,
            grandmaster_sequence_id: seq,
            grandmaster_clock_stratum: 255,
            grandmaster_clock_identifier: *b"DFLT",
            grandmaster_clock_variance: i16::MAX,
            grandmaster_preferred: false,
            grandmaster_is_boundary_clock: false,
            sync_interval: log_interval,
            local_clock_variance: i16::MAX,
            local_steps_removed: 0,
            local_clock_stratum: 255,
            local_clock_identifier: *b"DFLT",
            parent_communication_technology: COMM_TECH_ETHERNET,
            parent_uuid: self.identity.uuid,
            parent_port_field: self.identity.port_id,
            estimated_master_variance: 0,
            estimated_master_drift: 0,
            utc_reasonable: false,
        }
    }
}
