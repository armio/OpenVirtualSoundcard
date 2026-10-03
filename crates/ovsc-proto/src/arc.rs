//! ARC – "Audio Routing Control", UDP port 4440.
//!
//! Controllers (Dante Controller, `netaudio`, `ovsc route`) use ARC to
//! read a device's channels and flows and to change routing (subscriptions)
//! and channel names. Every message uses the frame from [`crate::frame`].
//!
//! Both directions are implemented: `encode_*_request` / `decode_*_response`
//! for the controller side, `decode_*_request` / `encode_*_response` for the
//! device side, so that each pair can be tested against the other.
//!
//! ## Paged lists
//! Channel and flow lists are paged. A request carries
//! `[0x00, 0x01, first(u16, 1-based), last(u16) or 0]`. The response payload
//! starts with `[capacity, count]` followed by `capacity` fixed-size slots
//! (only the first `count` are meaningful) and then a heap holding strings and
//! nested descriptors referenced by packet-absolute offsets. A result code of
//! [`result::MORE_PAGES`] tells the controller to ask for the next page.

use std::net::Ipv4Addr;

use crate::frame::{Frame, Header, PacketBuilder, encode, protocol, respond, result};
use crate::wire::{Reader, Writer, u16_at, u32_at};
use crate::{Error, Result};

/// Well-known ARC port (advertised in `_netaudio-arc._udp`).
pub const PORT: u16 = 4440;

/// ARC opcodes.
pub mod opcode {
    pub const CHANNEL_COUNTS: u16 = 0x1000;
    pub const SET_DEVICE_NAME: u16 = 0x1001;
    pub const DEVICE_NAME: u16 = 0x1002;
    pub const DEVICE_INFO: u16 = 0x1003;
    /// Read device properties ([`property`]): latency, sample rate, …
    pub const PROPERTIES_1100: u16 = 0x1100;
    /// Write device properties: Dante Controller sets the latency this way.
    pub const PROPERTIES_1101: u16 = 0x1101;
    /// Queried by Dante Controller; meaning not fully understood.
    pub const PROPERTIES_1102: u16 = 0x1102;
    pub const TX_CHANNELS: u16 = 0x2000;
    pub const TX_CHANNEL_NAMES: u16 = 0x2010;
    pub const RENAME_TX_CHANNELS: u16 = 0x2013;
    pub const TX_FLOWS: u16 = 0x2200;
    pub const CREATE_MULTICAST_TX_FLOW: u16 = 0x2201;
    pub const DELETE_TX_FLOWS: u16 = 0x2202;
    /// Transmit flow labels: Dante Controller reads them in Device View.
    pub const TX_FLOW_LABELS: u16 = 0x2204;
    /// Queried by Dante Controller; devices without the feature answer 0x30.
    pub const UNKNOWN_2320: u16 = 0x2320;
    pub const RX_CHANNELS: u16 = 0x3000;
    pub const RENAME_RX_CHANNELS: u16 = 0x3001;
    pub const SET_SUBSCRIPTIONS: u16 = 0x3010;
    pub const REMOVE_SUBSCRIPTIONS: u16 = 0x3014;
    pub const RX_FLOWS: u16 = 0x3200;
    /// Receive port ranges. Dante Controller reports a clock-domain mismatch
    /// when a device doesn't answer it.
    pub const RX_PORT_RANGES: u16 = 0x3300;
}

/// Records per page of [`opcode::TX_CHANNELS`] and [`opcode::TX_CHANNEL_NAMES`].
pub const TX_CHANNELS_PER_PAGE: usize = 32;
/// Records per page of [`opcode::RX_CHANNELS`] (real devices use 16).
pub const RX_CHANNELS_PER_PAGE: usize = 16;
/// Pointer slots per page of flow listings.
pub const FLOWS_PER_PAGE: usize = 16;
/// Port ranges real receivers report for [`opcode::RX_PORT_RANGES`]; unicast
/// flows are received on ports from the first range.
pub const RX_PORT_RANGES: [(u16, u16); 2] = [(0x3800, 0x397f), (0x3980, 0x39ff)];
/// Minimum packet offset of the string table in subscription requests.
const SUBSCRIPTION_STRINGS_MIN_OFFSET: usize = 52;

// ---------------------------------------------------------------------------
// Shared descriptors
// ---------------------------------------------------------------------------

/// Audio format shared by a group of channels (16 bytes on the wire).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelFormat {
    pub sample_rate: u32,
    pub bits_per_sample: u16,
    /// PCM encoding id (`0x0e` on current devices, 4 on older ones).
    pub pcm_type: u16,
}

impl ChannelFormat {
    pub const SIZE: usize = 16;

    fn write(&self, w: &mut Writer) -> u16 {
        let at = w.u32(self.sample_rate);
        w.u8(1);
        w.u8(1);
        w.u16(self.bits_per_sample);
        w.u16(0x0400);
        w.u16(self.bits_per_sample);
        w.u16(self.bits_per_sample);
        w.u16(self.pcm_type);
        at
    }

    fn read(buf: &[u8], offset: usize) -> Result<Self> {
        let mut r = Reader::at(buf, offset);
        let sample_rate = r.u32()?;
        r.skip(2)?;
        let bits_per_sample = r.u16()?;
        r.skip(6)?;
        let pcm_type = r.u16()?;
        Ok(Self { sample_rate, bits_per_sample, pcm_type })
    }
}

/// `[len = 8, family = 2 (AF_INET), port, ipv4]` – a truncated
/// `sockaddr_in` (8 bytes). Inferno writes `80 02` here; real devices `08 02`.
fn write_socket_descriptor(w: &mut Writer, addr: Ipv4Addr, port: u16) -> u16 {
    let at = w.u16(0x0802);
    w.u16(port);
    w.bytes(&addr.octets());
    at
}

/// Reads a socket descriptor (accepting the `80 02` variant too).
fn read_socket_descriptor(buf: &[u8], at: usize) -> Result<(Ipv4Addr, u16)> {
    let mut r = Reader::at(buf, at);
    let kind = r.u16()?;
    if kind != 0x0802 && kind != 0x8002 {
        return Err(Error::Invalid("socket descriptor"));
    }
    let port = r.u16()?;
    Ok((Ipv4Addr::from(r.array::<4>()?), port))
}

// ---------------------------------------------------------------------------
// Paging
// ---------------------------------------------------------------------------

/// Encodes a paged-list request starting at 0-based item `start`.
pub fn encode_paged_request(proto: u16, seq: u16, opcode: u16, start: usize) -> Vec<u8> {
    let first = u16::try_from(start + 1).unwrap_or(u16::MAX);
    let mut payload = [0u8; 6];
    payload[1] = 1;
    payload[2..4].copy_from_slice(&first.to_be_bytes());
    encode(proto, seq, opcode, result::REQUEST, &payload)
}

/// Returns the 0-based first item requested by a paged-list request.
pub fn decode_paged_request(frame: &Frame<'_>) -> Result<usize> {
    let p = frame.payload();
    let first = u16_at(p, 2)?;
    if first == 0 {
        return Err(Error::Invalid("paged request starting at item 0"));
    }
    Ok(first as usize - 1)
}

/// Builds one page of a paged-list response.
///
/// The page holds up to `page_size` items starting at `start`. Channel lists
/// reserve exactly as many slots as they fill (`capacity == count`); flow
/// lists always reserve `page_size` pointer slots (`fixed_capacity`), as real
/// devices do. "More pages" is only signalled on full pages.
///
/// `write_item` appends whatever heap data the item needs and returns the
/// item's slot bytes (exactly `slot_size` long).
fn encode_paged_response<T>(
    request: &Header,
    items: &[T],
    start: usize,
    page_size: usize,
    fixed_capacity: bool,
    slot_size: usize,
    mut write_item: impl FnMut(&mut Writer, &T, usize) -> Vec<u8>,
) -> Vec<u8> {
    let count = items.len().saturating_sub(start).min(page_size);
    let capacity = if fixed_capacity && count > 0 { page_size } else { count };
    let mut b = PacketBuilder::new();
    b.w.u8(capacity as u8);
    b.w.u8(count as u8);
    let slots_at = b.w.zeros(capacity * slot_size) as usize;
    for (n, (index, item)) in items.iter().enumerate().skip(start).take(count).enumerate() {
        let slot = write_item(&mut b.w, item, index);
        debug_assert_eq!(slot.len(), slot_size);
        for (i, byte) in slot.into_iter().enumerate() {
            b.w.patch_u8(slots_at + n * slot_size + i, byte);
        }
    }
    let more = start + count < items.len();
    b.finish_response(request, if more { result::MORE_PAGES } else { result::SUCCESS })
}

/// Iterates over the used slots of a paged response.
fn paged_slots<'a>(frame: &Frame<'a>, slot_size: usize) -> Result<Vec<&'a [u8]>> {
    let p = frame.payload();
    let count = *p.get(1).ok_or(Error::Truncated { offset: 11, needed: 1, len: p.len() + 10 })?;
    let mut r = Reader::at(p, 2);
    (0..count).map(|_| r.bytes(slot_size)).collect()
}

/// Whether a paged response says that more pages are available.
pub fn has_more_pages(frame: &Frame<'_>) -> bool {
    frame.header.result == result::MORE_PAGES
}

fn be16(v: u16) -> [u8; 2] {
    v.to_be_bytes()
}

// ---------------------------------------------------------------------------
// 0x1000 channel counts
// ---------------------------------------------------------------------------

/// Response to [`opcode::CHANNEL_COUNTS`]: device capabilities.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChannelCounts {
    pub tx_channels: u16,
    pub rx_channels: u16,
    pub max_channels_per_flow: u16,
    pub max_tx_flows: u16,
    pub max_rx_flows: u16,
    pub supports_tx_rename: bool,
    pub supports_tx_multicast: bool,
}

impl ChannelCounts {
    pub const SIZE: usize = 34;

    pub fn encode_response(&self, request: &Header) -> Vec<u8> {
        let mut w = Writer::with_capacity(Self::SIZE);
        w.u8(0);
        w.u8(((self.supports_tx_rename as u8) << 4) | ((self.supports_tx_multicast as u8) << 5));
        w.u16(self.tx_channels);
        w.u16(self.rx_channels);
        w.u16(4);
        w.u16(self.max_channels_per_flow);
        w.u16(8);
        w.u16(self.max_tx_flows);
        w.u16(self.max_rx_flows);
        // Some devices report the total channel count here, others not.
        w.u16(self.tx_channels.saturating_add(self.rx_channels));
        w.u16(1);
        w.u16(1);
        w.zeros(12);
        respond(request, result::SUCCESS, w.as_slice())
    }

    pub fn decode_response(frame: &Frame<'_>) -> Result<Self> {
        let mut r = Reader::new(frame.payload());
        r.u8()?;
        let flags = r.u8()?;
        let tx_channels = r.u16()?;
        let rx_channels = r.u16()?;
        r.u16()?;
        let max_channels_per_flow = r.u16()?;
        r.u16()?;
        let max_tx_flows = r.u16()?;
        let max_rx_flows = r.u16()?;
        Ok(Self {
            tx_channels,
            rx_channels,
            max_channels_per_flow,
            max_tx_flows,
            max_rx_flows,
            supports_tx_rename: flags & 0x10 != 0,
            supports_tx_multicast: flags & 0x20 != 0,
        })
    }
}

/// Encodes a request with an empty payload (e.g. channel counts, device name).
pub fn encode_simple_request(seq: u16, opcode: u16) -> Vec<u8> {
    encode(protocol::ARC, seq, opcode, result::REQUEST, &[])
}

// ---------------------------------------------------------------------------
// 0x1001 / 0x1002 / 0x1003 names
// ---------------------------------------------------------------------------

/// Response to [`opcode::DEVICE_NAME`]: the NUL-terminated device name.
pub fn encode_device_name_response(request: &Header, name: &str) -> Vec<u8> {
    let mut w = Writer::new();
    w.cstr(name);
    respond(request, result::SUCCESS, w.as_slice())
}

pub fn decode_device_name_response(frame: &Frame<'_>) -> Result<String> {
    frame.string_at(crate::frame::HEADER_LEN as u16)
}

/// Request to rename a device. `None` resets it to the factory name.
/// Dante Controller sends this with protocol id `0x2809`.
pub fn encode_set_device_name_request(seq: u16, name: Option<&str>) -> Vec<u8> {
    let mut w = Writer::new();
    if let Some(name) = name {
        w.cstr(name);
    }
    encode(protocol::ARC_2809, seq, opcode::SET_DEVICE_NAME, result::REQUEST, w.as_slice())
}

/// Decodes a rename request: `Some(name)`, or `None` for "reset to factory".
pub fn decode_set_device_name_request(frame: &Frame<'_>) -> Result<Option<String>> {
    match frame.payload() {
        [] | [0] | [0, 0] => Ok(None),
        _ => frame.string_at(crate::frame::HEADER_LEN as u16).map(Some),
    }
}

/// Response to [`opcode::DEVICE_INFO`]: names and firmware revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceNames {
    pub friendly_name: String,
    pub factory_name: String,
    pub board_name: String,
    pub revision: String,
}

impl DeviceNames {
    /// Layout captured from a real device: a 38-byte table of pointers and
    /// words, with a "name block" at content offset 10 and a "version block"
    /// at content offset 22, followed by the strings.
    const HEADER_SIZE: usize = 38;
    const NAME_BLOCK: u16 = 10 + 10;
    const VERSION_BLOCK: u16 = 10 + 22;

    pub fn encode_response(&self, request: &Header) -> Vec<u8> {
        let mut b = PacketBuilder::new();
        let hdr = b.w.zeros(Self::HEADER_SIZE) as usize;
        let friendly = b.w.cstr(&self.friendly_name);
        let factory = b.w.cstr(&self.factory_name);
        let board = b.w.cstr(&self.board_name);
        let revision = b.w.cstr(&self.revision);
        let words: [(usize, u16); 17] = [
            (2, Self::NAME_BLOCK),
            (4, Self::VERSION_BLOCK),
            (6, board),
            (8, revision),
            // Name block.
            (10, 0x0500),
            (12, friendly),
            (14, factory),
            (16, friendly),
            (18, 0),
            // Version block: ARC version 2.7.41, minimum 0.2.4, DBCP ids.
            (22, 0x0400),
            (24, 0),
            (26, 0x0400),
            (28, 0x0100),
            (30, protocol::ARC_2729),
            (32, 0x0204),
            (34, protocol::DBCP),
            (36, 0x1004),
        ];
        for (off, v) in words {
            b.w.patch_u16(hdr + off, v);
        }
        b.finish_response(request, result::SUCCESS)
    }

    pub fn decode_response(frame: &Frame<'_>) -> Result<Self> {
        let p = frame.payload();
        // Follow the name-block pointer when present (some devices place the
        // block elsewhere); fall back to the common fixed position.
        let block = match u16_at(p, 2)? {
            0 => Self::NAME_BLOCK as usize,
            ptr => ptr as usize,
        };
        let packet = frame.packet;
        Ok(Self {
            board_name: frame.string_at(u16_at(p, 6)?)?,
            revision: frame.string_at(u16_at(p, 8)?)?,
            friendly_name: frame.string_at(u16_at(packet, block + 2)?)?,
            factory_name: frame.string_at(u16_at(packet, block + 4)?)?,
        })
    }
}

// ---------------------------------------------------------------------------
// 0x2000 / 0x2010 transmit channels
// ---------------------------------------------------------------------------

/// A transmit channel as listed by [`opcode::TX_CHANNELS`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxChannelInfo {
    /// 1-based channel id.
    pub id: u16,
    pub factory_name: String,
    pub format: Option<ChannelFormat>,
}

/// Encodes one page of [`opcode::TX_CHANNELS`]; `names` are factory names.
pub fn encode_tx_channels_response(
    request: &Header,
    start: usize,
    format: ChannelFormat,
    names: &[String],
) -> Vec<u8> {
    let mut format_at = 0u16;
    encode_paged_response(
        request,
        names,
        start,
        TX_CHANNELS_PER_PAGE,
        false,
        8,
        |w, name, index| {
            if format_at == 0 {
                format_at = format.write(w);
            }
            let name_at = w.cstr(name);
            [be16(index as u16 + 1), be16(7), be16(format_at), be16(name_at)].concat()
        },
    )
}

pub fn decode_tx_channels_response(frame: &Frame<'_>) -> Result<Vec<TxChannelInfo>> {
    paged_slots(frame, 8)?
        .into_iter()
        .map(|slot| {
            let format_at = u16_at(slot, 4)?;
            Ok(TxChannelInfo {
                id: u16_at(slot, 0)?,
                factory_name: frame.string_at(u16_at(slot, 6)?)?,
                format: match format_at {
                    0 => None,
                    at => Some(ChannelFormat::read(frame.packet, at as usize)?),
                },
            })
        })
        .collect()
}

/// Encodes one page of [`opcode::TX_CHANNEL_NAMES`] (user-assigned names).
pub fn encode_tx_channel_names_response(
    request: &Header,
    start: usize,
    names: &[String],
) -> Vec<u8> {
    let mut wrote_prefix = false;
    encode_paged_response(
        request,
        names,
        start,
        TX_CHANNELS_PER_PAGE,
        false,
        6,
        |w, name, index| {
            if !wrote_prefix {
                w.u32(0);
                wrote_prefix = true;
            }
            let id = index as u16 + 1;
            let name_at = w.cstr(name);
            [be16(id), be16(id), be16(name_at)].concat()
        },
    )
}

/// Decodes [`opcode::TX_CHANNEL_NAMES`] into `(channel id, friendly name)`.
pub fn decode_tx_channel_names_response(frame: &Frame<'_>) -> Result<Vec<(u16, String)>> {
    paged_slots(frame, 6)?
        .into_iter()
        .map(|slot| Ok((u16_at(slot, 0)?, frame.string_at(u16_at(slot, 4)?)?)))
        .collect()
}

// ---------------------------------------------------------------------------
// 0x3000 receive channels
// ---------------------------------------------------------------------------

/// Subscription state of a receive channel as reported to controllers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubscriptionStatus {
    /// No subscription.
    None,
    /// Subscribed, but the transmitter could not be found.
    Unresolved,
    /// Subscription is being set up.
    InProgress,
    /// The transmitter refused: it has no free flows.
    TxNoFlows,
    /// The transmitter refused or failed for another reason.
    TxFail,
    /// Audio is arriving over a unicast flow.
    ReceivingUnicast,
    /// Audio is arriving over a multicast flow.
    ReceivingMulticast,
    /// A status code OpenVirtualSoundcard does not know.
    Other(u32),
}

impl SubscriptionStatus {
    pub fn code(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Unresolved => 1,
            Self::InProgress => 8,
            Self::TxNoFlows => 0x14,
            Self::TxFail => 0x15,
            Self::ReceivingUnicast => 0x0101_0009,
            Self::ReceivingMulticast => 0x0101_000a,
            Self::Other(code) => code,
        }
    }

    pub fn from_code(code: u32) -> Self {
        match code {
            0 => Self::None,
            1 => Self::Unresolved,
            8 => Self::InProgress,
            0x14 => Self::TxNoFlows,
            0x15 => Self::TxFail,
            // Flow records carry only the low 16 bits.
            0x0101_0009 | 0x0009 => Self::ReceivingUnicast,
            0x0101_000a | 0x000a => Self::ReceivingMulticast,
            other => Self::Other(other),
        }
    }

    pub fn is_receiving(self) -> bool {
        matches!(self, Self::ReceivingUnicast | Self::ReceivingMulticast)
    }
}

/// A receive channel as listed by [`opcode::RX_CHANNELS`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RxChannelInfo {
    /// 1-based channel id.
    pub id: u16,
    pub name: String,
    /// `(tx channel, tx device)` this channel is subscribed to.
    pub subscription: Option<(String, String)>,
    pub status: SubscriptionStatus,
}

/// Encodes one page of [`opcode::RX_CHANNELS`]. Channel ids are taken from
/// the items, which must be in id order.
pub fn encode_rx_channels_response(
    request: &Header,
    start: usize,
    format: ChannelFormat,
    channels: &[RxChannelInfo],
) -> Vec<u8> {
    let mut format_at = 0u16;
    encode_paged_response(request, channels, start, RX_CHANNELS_PER_PAGE, false, 20, |w, ch, _| {
        if format_at == 0 {
            format_at = format.write(w);
        }
        let (tx_channel_at, tx_device_at) = match &ch.subscription {
            Some((channel, device)) => (w.cstr(channel), w.cstr(device)),
            None => (0, 0),
        };
        let name_at = w.cstr(&ch.name);
        let status = ch.status.code().to_be_bytes();
        [
            &be16(ch.id)[..],
            &be16(6),
            &be16(format_at),
            &be16(tx_channel_at),
            &be16(tx_device_at),
            &be16(name_at),
            &status,
            &[0; 4],
        ]
        .concat()
    })
}

pub fn decode_rx_channels_response(frame: &Frame<'_>) -> Result<Vec<RxChannelInfo>> {
    paged_slots(frame, 20)?
        .into_iter()
        .map(|slot| {
            let tx_channel = frame.opt_string_at(u16_at(slot, 6)?)?;
            let tx_device = frame.opt_string_at(u16_at(slot, 8)?)?;
            Ok(RxChannelInfo {
                id: u16_at(slot, 0)?,
                name: frame.string_at(u16_at(slot, 10)?)?,
                subscription: tx_channel.zip(tx_device),
                status: SubscriptionStatus::from_code(u32_at(slot, 12)?),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 0x2013 / 0x3001 channel renames
// ---------------------------------------------------------------------------

/// Encodes a request renaming transmit channels (`(id, new name)`).
pub fn encode_rename_tx_channels_request(seq: u16, renames: &[(u16, &str)]) -> Vec<u8> {
    encode_rename_request(seq, opcode::RENAME_TX_CHANNELS, renames, true)
}

/// Encodes a request renaming receive channels (`(id, new name)`).
pub fn encode_rename_rx_channels_request(seq: u16, renames: &[(u16, &str)]) -> Vec<u8> {
    encode_rename_request(seq, opcode::RENAME_RX_CHANNELS, renames, false)
}

fn encode_rename_request(seq: u16, op: u16, renames: &[(u16, &str)], tx: bool) -> Vec<u8> {
    let slot = if tx { 6 } else { 4 };
    let mut b = PacketBuilder::new();
    b.w.u8(0);
    b.w.u8(renames.len() as u8);
    let slots = b.w.zeros(slot * renames.len()) as usize;
    for (i, (id, name)) in renames.iter().enumerate() {
        let name_at = b.w.cstr(name);
        let at = slots + i * slot + if tx { 2 } else { 0 };
        b.w.patch_u16(at, *id);
        b.w.patch_u16(at + 2, name_at);
    }
    b.finish(protocol::ARC, seq, op, result::REQUEST)
}

/// Decodes a transmit-channel rename request into `(id, new name)` pairs.
pub fn decode_rename_tx_channels_request(frame: &Frame<'_>) -> Result<Vec<(u16, String)>> {
    decode_rename_request(frame, 6, 2)
}

/// Decodes a receive-channel rename request into `(id, new name)` pairs.
pub fn decode_rename_rx_channels_request(frame: &Frame<'_>) -> Result<Vec<(u16, String)>> {
    decode_rename_request(frame, 4, 0)
}

fn decode_rename_request(
    frame: &Frame<'_>,
    slot_size: usize,
    id_at: usize,
) -> Result<Vec<(u16, String)>> {
    paged_slots(frame, slot_size)?
        .into_iter()
        .map(|slot| Ok((u16_at(slot, id_at)?, frame.string_at(u16_at(slot, id_at + 2)?)?)))
        .collect()
}

// ---------------------------------------------------------------------------
// 0x3010 / 0x3014 subscriptions
// ---------------------------------------------------------------------------

/// One entry of a [`opcode::SET_SUBSCRIPTIONS`] request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubscriptionRequest {
    /// 1-based receive channel id on the device being configured.
    pub rx_channel: u16,
    /// `(tx channel name, tx device name)`; `None` removes the subscription.
    /// A device name of `"."` refers to the receiving device itself.
    pub source: Option<(String, String)>,
}

pub fn encode_set_subscriptions_request(seq: u16, subs: &[SubscriptionRequest]) -> Vec<u8> {
    let mut b = PacketBuilder::new();
    b.w.u8(2);
    b.w.u8(subs.len() as u8);
    let slots = b.w.zeros(6 * subs.len()) as usize;
    if b.w.len() < SUBSCRIPTION_STRINGS_MIN_OFFSET {
        b.w.zeros(SUBSCRIPTION_STRINGS_MIN_OFFSET - b.w.len());
    }
    for (i, sub) in subs.iter().enumerate() {
        let at = slots + i * 6;
        b.w.patch_u16(at, sub.rx_channel);
        if let Some((channel, device)) = &sub.source {
            let channel_at = b.w.cstr(channel);
            let device_at = b.w.cstr(device);
            b.w.patch_u16(at + 2, channel_at);
            b.w.patch_u16(at + 4, device_at);
        }
    }
    b.finish(protocol::ARC, seq, opcode::SET_SUBSCRIPTIONS, result::REQUEST)
}

pub fn decode_set_subscriptions_request(frame: &Frame<'_>) -> Result<Vec<SubscriptionRequest>> {
    paged_slots(frame, 6)?
        .into_iter()
        .map(|slot| {
            let channel = frame.opt_string_at(u16_at(slot, 2)?)?;
            let device = frame.opt_string_at(u16_at(slot, 4)?)?;
            Ok(SubscriptionRequest {
                rx_channel: u16_at(slot, 0)?,
                source: channel.zip(device).filter(|(c, d)| !c.is_empty() && !d.is_empty()),
            })
        })
        .collect()
}

/// Request removing the subscriptions of the given receive channels.
pub fn encode_remove_subscriptions_request(seq: u16, rx_channels: &[u16]) -> Vec<u8> {
    let mut w = Writer::new();
    w.u16(rx_channels.len() as u16);
    for &ch in rx_channels {
        w.u32(ch as u32);
    }
    encode(protocol::ARC, seq, opcode::REMOVE_SUBSCRIPTIONS, result::REQUEST, w.as_slice())
}

pub fn decode_remove_subscriptions_request(frame: &Frame<'_>) -> Result<Vec<u16>> {
    let mut r = Reader::new(frame.payload());
    let count = r.u16()?;
    (0..count)
        .map(|_| {
            let ch = r.u32()?;
            u16::try_from(ch).map_err(|_| Error::Invalid("receive channel id"))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 0x2200 / 0x3200 flows
// ---------------------------------------------------------------------------

/// A transmit flow as reported by [`opcode::TX_FLOWS`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxFlowInfo {
    /// 1-based flow id.
    pub id: u16,
    pub local_name: String,
    /// Receiver device and flow name; `None` for multicast flows.
    pub remote: Option<(String, String)>,
    pub dst_addr: Ipv4Addr,
    pub dst_port: u16,
    /// Transmit channel ids carried by the flow (0 = empty slot).
    pub channels: Vec<u16>,
    /// Frames per packet.
    pub fpp: u16,
}

/// Encodes one page of [`opcode::TX_FLOWS`]. Each record is written header
/// first, followed by its socket descriptor, extension and strings.
pub fn encode_tx_flows_response(
    request: &Header,
    start: usize,
    sample_rate: u32,
    bits_per_sample: u16,
    flows: &[TxFlowInfo],
) -> Vec<u8> {
    encode_paged_response(request, flows, start, FLOWS_PER_PAGE, true, 2, |w, flow, _| {
        w.align(4);
        let record = w.u16(flow.id);
        w.u16(if flow.remote.is_some() { 0x11 } else { 0x02 });
        w.u32(sample_rate);
        w.u16(0);
        w.u16(bits_per_sample);
        w.u16(1); // destinations
        w.u16(flow.channels.len() as u16);
        let socket_ptr = w.u16(0) as usize;
        for &ch in &flow.channels {
            w.u16(ch);
        }
        let ext_ptr = w.u16(0) as usize;

        w.align(4);
        let socket = write_socket_descriptor(w, flow.dst_addr, flow.dst_port);
        let ext = w.u8(0x0a); // length in 16-bit words
        w.u8(0);
        w.u16(1);
        let host_ptr = w.u16(0) as usize;
        let remote_name_ptr = w.u16(0) as usize;
        w.u16(flow.fpp);
        let local_name_ptr = w.u16(0) as usize;
        w.u32(0); // latency, set on multicast flows
        w.u16(0); // media class: native
        w.u16(0);
        let local_name = w.cstr(&flow.local_name);
        if let Some((host, name)) = &flow.remote {
            let host_at = w.cstr(host);
            let name_at = w.cstr(name);
            w.patch_u16(host_ptr, host_at);
            w.patch_u16(remote_name_ptr, name_at);
        }
        w.patch_u16(socket_ptr, socket);
        w.patch_u16(ext_ptr, ext);
        w.patch_u16(local_name_ptr, local_name);
        be16(record).to_vec()
    })
}

pub fn decode_tx_flows_response(frame: &Frame<'_>) -> Result<Vec<TxFlowInfo>> {
    let p = frame.packet;
    paged_slots(frame, 2)?
        .into_iter()
        .map(|slot| u16_at(slot, 0))
        .filter(|ptr| !matches!(ptr, Ok(0)))
        .map(|ptr| {
            let at = ptr? as usize;
            let mut r = Reader::at(p, at);
            let id = r.u16()?;
            r.skip(10)?;
            let destinations = r.u16()? as usize;
            let n = r.u16()? as usize;
            let sockets = (0..destinations).map(|_| r.u16()).collect::<Result<Vec<_>>>()?;
            let channels = (0..n).map(|_| r.u16()).collect::<Result<Vec<_>>>()?;
            let ext = r.u16()? as usize;
            let (dst_addr, dst_port) = read_socket_descriptor(
                p,
                *sockets.first().ok_or(Error::Invalid("no destination"))? as usize,
            )?;
            let remote_host = frame.opt_string_at(u16_at(p, ext + 4)?)?;
            let remote_name = frame.opt_string_at(u16_at(p, ext + 6)?)?;
            Ok(TxFlowInfo {
                id,
                local_name: frame.opt_string_at(u16_at(p, ext + 10)?)?.unwrap_or_default(),
                remote: remote_host.zip(remote_name),
                dst_addr,
                dst_port,
                channels,
                fpp: u16_at(p, ext + 8)?,
            })
        })
        .collect()
}

/// A receive flow as reported by [`opcode::RX_FLOWS`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RxFlowInfo {
    /// 1-based flow id.
    pub id: u16,
    /// Our address and the port the flow is received on.
    pub addr: Ipv4Addr,
    pub port: u16,
    pub latency_ns: u32,
    pub status: SubscriptionStatus,
    /// For each channel slot in the flow, the 0-based local receive channels
    /// it feeds.
    pub slots: Vec<Vec<u16>>,
}

/// Encodes one page of [`opcode::RX_FLOWS`] in the layout real devices use:
/// record header, channel bitmaps, endpoint, status, back to back.
pub fn encode_rx_flows_response(
    request: &Header,
    start: usize,
    sample_rate: u32,
    bits_per_sample: u16,
    rx_channels: usize,
    flows: &[RxFlowInfo],
) -> Vec<u8> {
    let words = rx_channels.div_ceil(16).max(1);
    encode_paged_response(request, flows, start, FLOWS_PER_PAGE, true, 2, |w, flow, _| {
        w.align(4);
        let record = w.u16(flow.id);
        w.u16(1);
        w.u32(sample_rate);
        w.u32(bits_per_sample as u32);
        w.u16(1); // interfaces
        w.u16(flow.slots.len() as u16);
        w.u16(words as u16);
        let endpoint_ptr = w.u16(0) as usize;
        let mask_ptrs = w.zeros(2 * flow.slots.len()) as usize;
        let status_ptr = w.u16(0) as usize;

        for (k, targets) in flow.slots.iter().enumerate() {
            let at = w.offset();
            for word in 0..words {
                let mut bits = 0u16;
                for &t in targets {
                    if t as usize / 16 == word {
                        bits |= 1 << (t % 16);
                    }
                }
                w.u16(bits);
            }
            w.patch_u16(mask_ptrs + 2 * k, at);
        }
        w.align(4);
        let endpoint = write_socket_descriptor(w, flow.addr, flow.port);
        let status = w.u16((flow.status.code() & 0xffff) as u16);
        w.u16(flow.status.is_receiving() as u16); // interfaces receiving
        w.u16(0x0800);
        w.u16(0);
        w.u32(flow.latency_ns);
        w.u16(0); // transport: native
        w.u16(0);
        w.patch_u16(endpoint_ptr, endpoint);
        w.patch_u16(status_ptr, status);
        be16(record).to_vec()
    })
}

pub fn decode_rx_flows_response(frame: &Frame<'_>) -> Result<Vec<RxFlowInfo>> {
    let p = frame.packet;
    paged_slots(frame, 2)?
        .into_iter()
        .map(|slot| u16_at(slot, 0))
        .filter(|ptr| !matches!(ptr, Ok(0)))
        .map(|ptr| {
            let mut r = Reader::at(p, ptr? as usize);
            let id = r.u16()?;
            r.skip(10)?;
            let interfaces = r.u16()? as usize;
            let n = r.u16()? as usize;
            let words = r.u16()? as usize;
            let endpoints = (0..interfaces).map(|_| r.u16()).collect::<Result<Vec<_>>>()?;
            let masks = (0..n).map(|_| r.u16()).collect::<Result<Vec<_>>>()?;
            let status_at = r.u16()? as usize;
            let (addr, port) = read_socket_descriptor(
                p,
                *endpoints.first().ok_or(Error::Invalid("no endpoint"))? as usize,
            )?;
            let slots = masks
                .iter()
                .map(|&m| {
                    let mut r = Reader::at(p, m as usize);
                    let mut targets = Vec::new();
                    for word in 0..words {
                        let bits = r.u16()?;
                        targets.extend(
                            (0..16)
                                .filter(|b| bits & (1 << b) != 0)
                                .map(|b| (word * 16 + b) as u16),
                        );
                    }
                    Ok(targets)
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(RxFlowInfo {
                id,
                addr,
                port,
                latency_ns: u32_at(p, status_at + 8)?,
                status: SubscriptionStatus::from_code(u16_at(p, status_at)? as u32),
                slots,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Fixed answers to opcodes whose content is not understood
// ---------------------------------------------------------------------------

/// Answer to [`opcode::RX_PORT_RANGES`].
pub fn encode_rx_port_ranges_response(request: &Header) -> Vec<u8> {
    let mut w = Writer::new();
    for (first, last) in RX_PORT_RANGES {
        w.u16(first);
        w.u16(last);
    }
    respond(request, result::SUCCESS, w.as_slice())
}

// 0x1100 / 0x1101 device properties

/// Device property ids, read with [`opcode::PROPERTIES_1100`] and written
/// with [`opcode::PROPERTIES_1101`]. With bit 15 set the value is a u32,
/// stored after the records and referenced by its packet offset; clear, an
/// inline u16.
pub mod property {
    pub const SAMPLE_RATE: u16 = 0x8020;
    /// The "default" (transmit flow) latency, ns.
    pub const DEFAULT_LATENCY: u16 = 0x8204;
    /// The latency the device is configured for, ns.
    pub const CONFIGURED_LATENCY: u16 = 0x8205;
    /// Frames per packet of unicast flows.
    pub const UNICAST_FPP: u16 = 0x0211;
    /// The latency receive flows run at, ns.
    pub const RX_LATENCY: u16 = 0x8301;
    pub const MAX_LATENCY: u16 = 0x8302;
    pub const MIN_LATENCY: u16 = 0x8306;
    /// Frames per packet of receive flows.
    pub const RX_FPP: u16 = 0x0310;

    /// Whether `id`'s value is a u32 held by pointer.
    pub fn is_u32(id: u16) -> bool {
        id & 0x8000 != 0
    }
}

/// The properties a [`opcode::PROPERTIES_1100`] request asks for: `None`
/// for all (empty content), else the ids of `[u8 0][u8 n][n × u16 id]`.
pub fn decode_read_properties_request(frame: &Frame<'_>) -> Result<Option<Vec<u16>>> {
    let payload = frame.payload();
    if payload.is_empty() {
        return Ok(None);
    }
    let mut r = Reader::new(payload);
    r.u8()?;
    let n = r.u8()? as usize;
    (0..n).map(|_| r.u16()).collect::<Result<Vec<_>>>().map(Some)
}

/// Answer to a properties read or write: `[u8 2][u8 n][n × (u16 id, u16
/// value or pointer)]` and then the u32 values. A value of `None` encodes
/// the record `(0, id)`: property `id` is unavailable.
///
/// The first byte is 0x12 to 0x24 on the devices captured and 2 from
/// netaudio's virtual device; its meaning is unknown.
pub fn encode_properties_response(request: &Header, records: &[(u16, Option<u32>)]) -> Vec<u8> {
    let mut b = PacketBuilder::new();
    b.w.u8(2);
    b.w.u8(records.len() as u8);
    let mut at = crate::frame::HEADER_LEN + 2 + 4 * records.len();
    let mut pool = Vec::new();
    for &(id, value) in records {
        match value {
            None => {
                b.w.u16(0);
                b.w.u16(id);
            }
            Some(v) if property::is_u32(id) => {
                b.w.u16(id);
                b.w.u16(at as u16);
                pool.push(v);
                at += 4;
            }
            Some(v) => {
                b.w.u16(id);
                b.w.u16(v as u16);
            }
        }
    }
    for v in pool {
        b.w.u32(v);
    }
    b.finish_response(request, result::SUCCESS)
}

/// Decodes an answer of [`encode_properties_response`]'s shape.
pub fn decode_properties_response(frame: &Frame<'_>) -> Result<Vec<(u16, Option<u32>)>> {
    let mut r = Reader::new(frame.payload());
    r.u8()?;
    let n = r.u8()? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let (id, v) = (r.u16()?, r.u16()?);
        out.push(match id {
            0 => (v, None),
            id if property::is_u32(id) => (id, Some(u32_at(frame.packet, v as usize)?)),
            id => (id, Some(v as u32)),
        });
    }
    Ok(out)
}

/// The records of a [`opcode::PROPERTIES_1101`] request, `[u8 n][u8 ?][n ×
/// (u16 id, u16 value or pointer)]` and the u32 values. A pointer outside
/// the packet gives `None`: Dante Controller's latency write ends with
/// `(0x8302, 0x8306)`, which points nowhere.
pub fn decode_write_properties_request(frame: &Frame<'_>) -> Result<Vec<(u16, Option<u32>)>> {
    let mut r = Reader::new(frame.payload());
    let n = r.u8()? as usize;
    r.u8()?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let (id, v) = (r.u16()?, r.u16()?);
        let value = if property::is_u32(id) {
            u32_at(frame.packet, v as usize).ok()
        } else {
            Some(v as u32)
        };
        out.push((id, value));
    }
    Ok(out)
}

/// Dante Controller's "set latency" request, byte for byte (protocol
/// 0x27FF): configured and receive latency `latency_ns`, unicast and
/// receive frames per packet `fpp`, and a final `(0x8302, 0x8306)` record.
pub fn encode_set_latency_request(seq: u16, latency_ns: u32, fpp: u16) -> Vec<u8> {
    let mut b = PacketBuilder::new();
    b.w.u8(5);
    b.w.u8(4);
    for (id, value) in [
        (property::CONFIGURED_LATENCY, 32),
        (property::UNICAST_FPP, fpp),
        (property::RX_LATENCY, 36),
        (property::RX_FPP, fpp),
        (property::MAX_LATENCY, property::MIN_LATENCY),
    ] {
        b.w.u16(id);
        b.w.u16(value);
    }
    b.w.u32(latency_ns);
    b.w.u32(latency_ns);
    b.finish(protocol::ARC, seq, opcode::PROPERTIES_1101, result::REQUEST)
}

/// Answer to [`opcode::PROPERTIES_1102`]: all-zero property table.
pub fn encode_properties_1102_response(request: &Header) -> Vec<u8> {
    respond(request, result::SUCCESS, &[0u8; 94])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses a packet built in the test; leaking keeps the frame borrow simple.
    fn leak_parse(pkt: Vec<u8>) -> Frame<'static> {
        Frame::parse(Box::leak(pkt.into_boxed_slice())).unwrap()
    }
    use crate::frame::HEADER_LEN;

    fn request(opcode: u16, start: usize) -> Vec<u8> {
        encode_paged_request(protocol::ARC, 0x4242, opcode, start)
    }

    fn fmt() -> ChannelFormat {
        ChannelFormat { sample_rate: 48_000, bits_per_sample: 24, pcm_type: 0x0e }
    }

    #[test]
    fn set_latency_is_dante_controllers_packet() {
        let pkt = encode_set_latency_request(0x1234, 250_000, 4);
        let want = [
            0x27, 0xff, 0x00, 0x28, 0x12, 0x34, 0x11, 0x01, 0x00, 0x00, // header
            0x05, 0x04, 0x82, 0x05, 0x00, 0x20, 0x02, 0x11, 0x00, 0x04, //
            0x83, 0x01, 0x00, 0x24, 0x03, 0x10, 0x00, 0x04, 0x83, 0x02, //
            0x83, 0x06, 0x00, 0x03, 0xd0, 0x90, 0x00, 0x03, 0xd0, 0x90,
        ];
        assert_eq!(pkt, want);
        assert_eq!(
            decode_write_properties_request(&leak_parse(pkt)).unwrap(),
            vec![
                (property::CONFIGURED_LATENCY, Some(250_000)),
                (property::UNICAST_FPP, Some(4)),
                (property::RX_LATENCY, Some(250_000)),
                (property::RX_FPP, Some(4)),
                (property::MAX_LATENCY, None),
            ]
        );
    }

    #[test]
    fn properties_answer_round_trips() {
        let req = leak_parse(encode_simple_request(7, opcode::PROPERTIES_1100));
        let records = [
            (property::SAMPLE_RATE, Some(48_000)),
            (property::CONFIGURED_LATENCY, Some(4_000_000)),
            (property::RX_FPP, Some(16)),
            (0x8218, None),
            (property::MIN_LATENCY, Some(1_000_000)),
        ];
        let pkt = encode_properties_response(&req.header, &records);
        // Five records, then the three u32 values from offset 32.
        assert_eq!(&pkt[10..14], &[2, 5, 0x80, 0x20]);
        assert_eq!(u16_at(&pkt, 14).unwrap(), 32);
        assert_eq!(&pkt[24..28], &[0, 0, 0x82, 0x18]);
        assert_eq!(pkt.len(), 32 + 12);
        assert_eq!(decode_properties_response(&leak_parse(pkt)).unwrap(), records);
    }

    #[test]
    fn read_properties_requests_list_ids_or_none() {
        let all = leak_parse(encode_simple_request(1, opcode::PROPERTIES_1100));
        assert_eq!(decode_read_properties_request(&all).unwrap(), None);
        let content = [0, 2, 0x82, 0x05, 0x83, 0x06];
        let pkt = encode(protocol::ARC, 2, opcode::PROPERTIES_1100, result::REQUEST, &content);
        assert_eq!(
            decode_read_properties_request(&leak_parse(pkt)).unwrap(),
            Some(vec![property::CONFIGURED_LATENCY, property::MIN_LATENCY])
        );
    }

    #[test]
    fn paged_request_matches_netaudio_layout() {
        // netaudio's "receivers page 1" request: channels start at 17.
        let pkt = request(opcode::RX_CHANNELS, 16);
        assert_eq!(hex::encode(&pkt), "27ff0010424230000000000100110000");
        let f = Frame::parse(&pkt).unwrap();
        assert_eq!(decode_paged_request(&f).unwrap(), 16);
    }

    #[test]
    fn channel_counts_round_trip() {
        let req = leak_parse(encode_simple_request(1, opcode::CHANNEL_COUNTS));
        let counts = ChannelCounts {
            tx_channels: 8,
            rx_channels: 16,
            max_channels_per_flow: 8,
            max_tx_flows: 32,
            max_rx_flows: 32,
            supports_tx_rename: true,
            supports_tx_multicast: false,
        };
        let pkt = counts.encode_response(&req.header);
        assert_eq!(pkt.len(), HEADER_LEN + ChannelCounts::SIZE);
        let resp = Frame::parse(&pkt).unwrap();
        assert_eq!(resp.payload()[1], 0x10);
        assert_eq!(ChannelCounts::decode_response(&resp).unwrap(), counts);
    }

    #[test]
    fn device_name_and_info_round_trip() {
        let req = leak_parse(encode_simple_request(2, opcode::DEVICE_NAME));
        let resp = leak_parse(encode_device_name_response(&req.header, "studio-mac"));
        assert_eq!(decode_device_name_response(&resp).unwrap(), "studio-mac");

        let names = DeviceNames {
            friendly_name: "studio-mac".into(),
            factory_name: "ovsc-0a1b2c".into(),
            board_name: "OpenVirtualSoundcard".into(),
            revision: ":705".into(),
        };
        let req = leak_parse(encode_simple_request(3, opcode::DEVICE_INFO));
        let resp = leak_parse(names.encode_response(&req.header));
        assert_eq!(DeviceNames::decode_response(&resp).unwrap(), names);
    }

    #[test]
    fn set_device_name_matches_controller_bytes() {
        // Captured from Dante Controller (via netaudio's test suite).
        let pkt = encode_set_device_name_request(0x261b, Some("avio-bt-11"));
        assert_eq!(hex::encode(&pkt), "28090015261b100100006176696f2d62742d313100");
        let f = Frame::parse(&pkt).unwrap();
        assert_eq!(decode_set_device_name_request(&f).unwrap().as_deref(), Some("avio-bt-11"));
        let reset = leak_parse(encode_set_device_name_request(1, None));
        assert_eq!(decode_set_device_name_request(&reset).unwrap(), None);
    }

    #[test]
    fn tx_channels_pages() {
        let names: Vec<String> = (1..=40).map(|i| format!("{i:02}")).collect();
        let req = leak_parse(request(opcode::TX_CHANNELS, 0));
        let resp_pkt = encode_tx_channels_response(&req.header, 0, fmt(), &names);
        let resp = Frame::parse(&resp_pkt).unwrap();
        assert!(has_more_pages(&resp));
        let page1 = decode_tx_channels_response(&resp).unwrap();
        assert_eq!(page1.len(), 32);
        assert_eq!(
            page1[0],
            TxChannelInfo { id: 1, factory_name: "01".into(), format: Some(fmt()) }
        );

        let req = leak_parse(request(opcode::TX_CHANNELS, 32));
        let start = decode_paged_request(&req).unwrap();
        let resp_pkt = encode_tx_channels_response(&req.header, start, fmt(), &names);
        let resp = Frame::parse(&resp_pkt).unwrap();
        assert!(!has_more_pages(&resp));
        let page2 = decode_tx_channels_response(&resp).unwrap();
        assert_eq!(page2.len(), 8);
        assert_eq!(page2[7].id, 40);
        assert_eq!(page2[7].factory_name, "40");
    }

    #[test]
    fn empty_list_is_two_zero_bytes() {
        let req = leak_parse(request(opcode::TX_CHANNELS, 0));
        let resp = leak_parse(encode_tx_channels_response(&req.header, 0, fmt(), &[]));
        assert_eq!(resp.payload(), &[0, 0]);
        assert!(decode_tx_channels_response(&resp).unwrap().is_empty());
    }

    #[test]
    fn tx_channel_names_round_trip() {
        let names = vec!["Left".to_owned(), "Right".to_owned()];
        let req = leak_parse(request(opcode::TX_CHANNEL_NAMES, 0));
        let resp = leak_parse(encode_tx_channel_names_response(&req.header, 0, &names));
        assert_eq!(
            decode_tx_channel_names_response(&resp).unwrap(),
            vec![(1, "Left".to_owned()), (2, "Right".to_owned())]
        );
    }

    #[test]
    fn rx_channels_round_trip() {
        let channels = vec![
            RxChannelInfo {
                id: 1,
                name: "In 1".into(),
                subscription: Some(("Out 3".into(), "stagebox".into())),
                status: SubscriptionStatus::ReceivingUnicast,
            },
            RxChannelInfo {
                id: 2,
                name: "In 2".into(),
                subscription: None,
                status: SubscriptionStatus::None,
            },
        ];
        let req = leak_parse(request(opcode::RX_CHANNELS, 0));
        let resp = leak_parse(encode_rx_channels_response(&req.header, 0, fmt(), &channels));
        assert_eq!(decode_rx_channels_response(&resp).unwrap(), channels);
        // Status of the first slot sits at payload offset 2 + 12.
        assert_eq!(u32_at(resp.payload(), 14).unwrap(), 0x0101_0009);
    }

    #[test]
    fn subscriptions_match_netaudio_layout() {
        let subs = vec![
            SubscriptionRequest { rx_channel: 1, source: Some(("01".into(), "avio".into())) },
            SubscriptionRequest { rx_channel: 2, source: None },
        ];
        let pkt = encode_set_subscriptions_request(9, &subs);
        // Strings start at the fixed minimum offset used by Dante Controller.
        assert_eq!(u16_at(&pkt, 14).unwrap(), 52);
        assert_eq!(&pkt[10..12], &[2, 2]);
        let f = Frame::parse(&pkt).unwrap();
        assert_eq!(decode_set_subscriptions_request(&f).unwrap(), subs);
    }

    #[test]
    fn remove_subscriptions_matches_capture() {
        // From Inferno's notes: netaudio removing the subscription of channel 2.
        let f = leak_parse(hex::decode("27ff00104a1c30140000000100000002").unwrap());
        assert_eq!(decode_remove_subscriptions_request(&f).unwrap(), vec![2]);
        let pkt = encode_remove_subscriptions_request(0x4a1c, &[2]);
        assert_eq!(hex::encode(pkt), "27ff00104a1c30140000000100000002");
    }

    #[test]
    fn renames_round_trip() {
        let pkt = encode_rename_tx_channels_request(1, &[(3, "Kick"), (4, "Snare")]);
        let f = Frame::parse(&pkt).unwrap();
        assert_eq!(
            decode_rename_tx_channels_request(&f).unwrap(),
            vec![(3, "Kick".to_owned()), (4, "Snare".to_owned())]
        );
        let pkt = encode_rename_rx_channels_request(1, &[(1, "Vox")]);
        let f = Frame::parse(&pkt).unwrap();
        assert_eq!(decode_rename_rx_channels_request(&f).unwrap(), vec![(1, "Vox".to_owned())]);
    }

    #[test]
    fn tx_flows_round_trip() {
        let flows = vec![
            TxFlowInfo {
                id: 1,
                local_name: "1_77".into(),
                remote: Some(("console".into(), "rx-flow".into())),
                dst_addr: Ipv4Addr::new(192, 168, 1, 20),
                dst_port: 14336,
                channels: vec![1, 2, 0, 4],
                fpp: 16,
            },
            TxFlowInfo {
                id: 3,
                local_name: "3_77".into(),
                remote: None,
                dst_addr: Ipv4Addr::new(239, 255, 1, 2),
                dst_port: 4321,
                channels: vec![5],
                fpp: 32,
            },
        ];
        let req = leak_parse(request(opcode::TX_FLOWS, 0));
        let resp = leak_parse(encode_tx_flows_response(&req.header, 0, 48_000, 24, &flows));
        assert_eq!(&resp.payload()[..2], &[16, 2]);
        assert_eq!(decode_tx_flows_response(&resp).unwrap(), flows);
    }

    #[test]
    fn decodes_real_multicast_tx_flow() {
        // Captured from a real device (netaudio test vector, public domain).
        let pkt = hex::decode(
            "2729006f286c220000011001002c0000000000000000000000000000000000000000000000000000\
             00000000002000020002ee0000000018000100080050001000000000000000000000000000000058\
             080210e1efff45670a000001000000000010006c000f424000000000333200",
        )
        .unwrap();
        let flows = decode_tx_flows_response(&leak_parse(pkt)).unwrap();
        assert_eq!(
            flows,
            vec![TxFlowInfo {
                id: 32,
                local_name: "32".into(),
                remote: None,
                dst_addr: Ipv4Addr::new(239, 255, 69, 103),
                dst_port: 4321,
                channels: vec![16, 0, 0, 0, 0, 0, 0, 0],
                fpp: 16,
            }]
        );
    }

    #[test]
    fn rx_flows_round_trip_header_first() {
        let flows = vec![RxFlowInfo {
            id: 2,
            addr: Ipv4Addr::new(10, 0, 0, 5),
            port: 14337,
            latency_ns: 1_000_000,
            status: SubscriptionStatus::ReceivingUnicast,
            slots: vec![vec![0], vec![1, 17]],
        }];
        let req = leak_parse(request(opcode::RX_FLOWS, 0));
        let resp = leak_parse(encode_rx_flows_response(&req.header, 0, 48_000, 24, 32, &flows));
        assert_eq!(decode_rx_flows_response(&resp).unwrap(), flows);
        // Sub-objects follow the record header, as on real devices.
        let record = u16_at(resp.payload(), 2).unwrap() as usize;
        let endpoint = u16_at(resp.packet, record + 18).unwrap() as usize;
        assert!(endpoint > record);
        assert_eq!(&resp.packet[endpoint..endpoint + 4], &[0x08, 0x02, 0x38, 0x01]);
    }

    #[test]
    fn decodes_real_rx_flows() {
        // lx-dante answering Dante Controller (netaudio test vector).
        let pkt = hex::decode(concat!(
            "2729017c033a320000011004002c008000d401280000000000000000000000000000000000000000",
            "00000000000100010000bb8000000018000100020008006800460056007000000010000000000000",
            "00000000000000000020000000000000000000000000000008023813c0a8016c0009000108000000",
            "000f424000000000000200010000bb800000001800010002000800bc009a00aa00c4000000010000",
            "0000000000000000000000000002000000000000000000000000000008023803c0a8016c00090001",
            "08000000000f424000000000000300010000bb8000000018000100020008011000ee00fe01184000",
            "000000000000000000000000000080000000000000000000000000000000000008023829c0a8016c",
            "0009000108000000001e848000000000000500010000bb8000000018000100020008016401420152",
            "016c04000000000000000000000000000000080000000000000000000000000000000000080210e1",
            "efffff38000a000008000000000f424000000000",
        ))
        .unwrap();
        let flows = decode_rx_flows_response(&leak_parse(pkt)).unwrap();
        assert_eq!(flows.len(), 4);
        assert_eq!(flows[0].id, 1);
        assert_eq!((flows[0].addr, flows[0].port), (Ipv4Addr::new(192, 168, 1, 108), 0x3813));
        // RX channels 21 and 22 (1-based): a stereo pair.
        assert_eq!(flows[0].slots, vec![vec![20], vec![21]]);
        assert_eq!(flows[0].status, SubscriptionStatus::ReceivingUnicast);
        assert_eq!(flows[2].latency_ns, 2_000_000);
        assert_eq!(flows[3].status, SubscriptionStatus::ReceivingMulticast);
        assert_eq!(flows[3].addr, Ipv4Addr::new(239, 255, 255, 56));
    }

    #[test]
    fn decodes_real_device_info_and_matches_its_layout() {
        // lx-dante (netaudio test vector).
        let pkt = hex::decode(concat!(
            "27ff00820000100300010000001400200070007d0500003000500030000000000400000004000100",
            "27290204110210046c782d64616e7465000000000000000000000000000000000000000000000000",
            "4c582d44414e54452d3038313235380000000000000000000000000000000000417564696e617465",
            "2044434d003a37303200",
        ))
        .unwrap();
        let real = leak_parse(pkt);
        let names = DeviceNames::decode_response(&real).unwrap();
        assert_eq!(names.friendly_name, "lx-dante");
        assert_eq!(names.factory_name, "LX-DANTE-081258");
        assert_eq!(names.board_name, "Audinate DCM");
        assert_eq!(names.revision, ":702");
        // Our encoding uses the same pointer table and version words.
        let ours = leak_parse(names.encode_response(&real.header));
        assert_eq!(&ours.payload()[2..6], &real.payload()[2..6]);
        assert_eq!(&ours.payload()[10..12], &[0x05, 0x00]);
        assert_eq!(&ours.payload()[22..38], &real.payload()[22..38]);
    }

    #[test]
    fn decodes_real_rx_channels() {
        // AVIO USB adapter with two subscribed inputs (netaudio test vector).
        let pkt = hex::decode(concat!(
            "27ff007b630930000001020200010006004c00340041005c010100090000000000020006004c0034",
            "0041006b01010009000000006d69632d6d69782d68696768006c782d64616e7465003a720000bb80",
            "0101001804000018001800046d69632d6d69782d31004c656674006d69632d6d69782d3200526967",
            "687400",
        ))
        .unwrap();
        let channels = decode_rx_channels_response(&leak_parse(pkt)).unwrap();
        assert_eq!(channels.len(), 2);
        // The heap also holds stale "Left"/"Right" strings nothing points to.
        assert_eq!(channels[0].name, "mic-mix-1");
        assert_eq!(channels[1].name, "mic-mix-2");
        assert_eq!(channels[0].subscription, Some(("mic-mix-high".into(), "lx-dante".into())));
        assert_eq!(channels[0].status, SubscriptionStatus::ReceivingUnicast);
    }

    #[test]
    fn rx_channel_pages_hold_sixteen_with_capacity_equal_to_count() {
        let channels: Vec<RxChannelInfo> = (1..=20)
            .map(|id| RxChannelInfo {
                id,
                name: format!("{id:02}"),
                subscription: None,
                status: SubscriptionStatus::None,
            })
            .collect();
        let req = leak_parse(request(opcode::RX_CHANNELS, 0));
        let page1 = leak_parse(encode_rx_channels_response(&req.header, 0, fmt(), &channels));
        assert_eq!(&page1.payload()[..2], &[16, 16]);
        assert!(has_more_pages(&page1));
        let page2 = leak_parse(encode_rx_channels_response(&req.header, 16, fmt(), &channels));
        assert_eq!(&page2.payload()[..2], &[4, 4]);
        assert!(!has_more_pages(&page2));
        assert_eq!(decode_rx_channels_response(&page2).unwrap()[3].id, 20);
    }

    #[test]
    fn rx_port_ranges_match_real_device() {
        let req = leak_parse(hex::decode("2729000a033c33000000").unwrap());
        assert_eq!(
            hex::encode(encode_rx_port_ranges_response(&req.header)),
            "27290012033c330000013800397f398039ff"
        );
    }
}
