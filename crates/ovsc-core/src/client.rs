//! Request/response client for the 10-byte-header protocols (ARC, CMC,
//! flow control), used both by the device itself (to request flows) and by
//! controller tools (`ovsc route`, `ovsc info`).

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::time::{Instant, timeout_at};
use tracing::trace;

use ovsc_proto::frame::{Frame, result};
use ovsc_proto::{arc, dbcp};

use crate::{Error, Result};

static NEXT_SEQ: AtomicU16 = AtomicU16::new(1);

/// A fresh transaction id.
pub fn next_seq() -> u16 {
    NEXT_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Sends `packet` to `dest` and waits for the response with the same
/// sequence number and opcode, retrying on timeout.
///
/// `bind_ip` selects the local interface (use `0.0.0.0` for "any").
pub async fn transact(
    bind_ip: Ipv4Addr,
    dest: SocketAddr,
    packet: &[u8],
    per_try: Duration,
    tries: u32,
) -> Result<Vec<u8>> {
    let request = Frame::parse(packet)?;
    let socket = UdpSocket::bind(SocketAddr::new(bind_ip.into(), 0)).await?;
    let mut buf = vec![0u8; 2048];
    for attempt in 0..tries.max(1) {
        trace!(?dest, attempt, opcode = request.header.opcode, "sending request");
        socket.send_to(packet, dest).await?;
        let deadline = Instant::now() + per_try;
        loop {
            let (len, from) = match timeout_at(deadline, socket.recv_from(&mut buf)).await {
                Ok(r) => r?,
                Err(_) => break,
            };
            if from.ip() != dest.ip() {
                continue;
            }
            let Ok(resp) = Frame::parse(&buf[..len]) else { continue };
            if resp.header.seq == request.header.seq && resp.header.opcode == request.header.opcode
            {
                return Ok(buf[..len].to_vec());
            }
        }
    }
    Err(Error::Timeout(format!("response from {dest}")))
}

/// Like [`transact`] but fails unless the response reports success.
pub async fn transact_ok(
    bind_ip: Ipv4Addr,
    dest: SocketAddr,
    packet: &[u8],
    per_try: Duration,
    tries: u32,
) -> Result<Vec<u8>> {
    let resp = transact(bind_ip, dest, packet, per_try, tries).await?;
    let code = Frame::parse(&resp)?.header.result;
    if code == result::SUCCESS || code == result::MORE_PAGES {
        Ok(resp)
    } else {
        Err(Error::Refused(code))
    }
}

/// Asks the transmitter at `dest` (its flow-control port) for a flow.
pub async fn request_flow(
    bind_ip: Ipv4Addr,
    dest: SocketAddr,
    request: &dbcp::FlowRequest,
) -> Result<dbcp::FlowHandle> {
    let packet = request.encode(next_seq());
    let resp = transact_ok(bind_ip, dest, &packet, Duration::from_millis(700), 3).await?;
    Ok(dbcp::decode_flow_created(&Frame::parse(&resp)?)?)
}

/// Asks the transmitter at `dest` to stop a flow (best effort).
pub async fn stop_flow(
    bind_ip: Ipv4Addr,
    dest: SocketAddr,
    handle: dbcp::FlowHandle,
) -> Result<()> {
    let packet = dbcp::encode_stop_flow(next_seq(), handle);
    transact_ok(bind_ip, dest, &packet, Duration::from_millis(500), 2).await.map(drop)
}

/// Asks the transmitter at `dest` to change the channels of a flow.
pub async fn update_flow(
    bind_ip: Ipv4Addr,
    dest: SocketAddr,
    handle: dbcp::FlowHandle,
    channels: &[u16],
) -> Result<()> {
    let packet = dbcp::encode_update_flow(next_seq(), handle, channels);
    transact_ok(bind_ip, dest, &packet, Duration::from_millis(700), 3).await.map(drop)
}

/// Fetches every page of a paged ARC list and decodes it with `decode`.
pub async fn arc_paged<T>(
    bind_ip: Ipv4Addr,
    dest: SocketAddr,
    opcode: u16,
    decode: impl Fn(&Frame<'_>) -> ovsc_proto::Result<Vec<T>>,
) -> Result<Vec<T>> {
    let mut items = Vec::new();
    loop {
        let packet = arc::encode_paged_request(
            ovsc_proto::frame::protocol::ARC,
            next_seq(),
            opcode,
            items.len(),
        );
        let resp = transact_ok(bind_ip, dest, &packet, Duration::from_millis(700), 3).await?;
        let frame = Frame::parse(&resp)?;
        let page = decode(&frame)?;
        let more = arc::has_more_pages(&frame) && !page.is_empty();
        items.extend(page);
        if !more {
            return Ok(items);
        }
    }
}
