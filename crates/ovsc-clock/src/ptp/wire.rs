//! PTPv1 (IEEE 1588-2002) message encoding and decoding.
//!
//! Only the four messages a slave needs are supported: Sync, Delay_Req,
//! Follow_Up and Delay_Resp. Management messages, PTPv2 packets (which share
//! UDP ports 319/320 with PTPv1) and malformed packets are reported as
//! [`DecodeError`]s so the caller can skip them.
//!
//! All multi-byte fields are big-endian. Offsets in the comments below are
//! from the start of the UDP payload.

use std::fmt;

/// Length of the common PTPv1 header.
pub const HEADER_LEN: usize = 40;
/// Length of a Sync message.
pub const SYNC_LEN: usize = 124;
/// Length of a Delay_Req message (same layout as Sync).
pub const DELAY_REQ_LEN: usize = 124;
/// Length of a Follow_Up message.
pub const FOLLOW_UP_LEN: usize = 52;
/// Length of a Delay_Resp message.
pub const DELAY_RESP_LEN: usize = 60;

/// `versionPTP` of IEEE 1588-2002.
pub const VERSION_PTP: u16 = 1;
/// `versionNetwork` of IEEE 1588-2002.
pub const VERSION_NETWORK: u16 = 1;

/// `messageType` of Sync and Delay_Req.
pub const MESSAGE_TYPE_EVENT: u8 = 1;
/// `messageType` of Follow_Up, Delay_Resp and Management.
pub const MESSAGE_TYPE_GENERAL: u8 = 2;

/// Communication technology code for Ethernet.
pub const COMM_TECH_ETHERNET: u8 = 1;

/// Number of bytes in a subdomain name.
pub const SUBDOMAIN_LEN: usize = 16;

/// The `control` field: which message this is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Control {
    Sync = 0,
    DelayReq = 1,
    FollowUp = 2,
    DelayResp = 3,
    Management = 4,
}

impl Control {
    fn from_u8(v: u8) -> Option<Control> {
        Some(match v {
            0 => Control::Sync,
            1 => Control::DelayReq,
            2 => Control::FollowUp,
            3 => Control::DelayResp,
            4 => Control::Management,
            _ => return None,
        })
    }

    /// The `messageType` that goes with this control value.
    pub fn message_type(self) -> u8 {
        match self {
            Control::Sync | Control::DelayReq => MESSAGE_TYPE_EVENT,
            _ => MESSAGE_TYPE_GENERAL,
        }
    }
}

/// The header `flags` field.
///
/// In IEEE 1588-2002 the defined flags all live in the low byte (byte 35 of
/// the packet).
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Flags(pub u16);

impl Flags {
    /// The last minute of the current UTC day has 61 seconds.
    pub const LI_61: Flags = Flags(0x0001);
    /// The last minute of the current UTC day has 59 seconds.
    pub const LI_59: Flags = Flags(0x0002);
    /// The sender is a boundary clock.
    pub const BOUNDARY_CLOCK: Flags = Flags(0x0004);
    /// Two-step clock: the precise origin time follows in a Follow_Up.
    pub const ASSIST: Flags = Flags(0x0008);
    /// External synchronisation.
    pub const EXT_SYNC: Flags = Flags(0x0010);
    /// Parent statistics are valid.
    pub const PARENT_STATS: Flags = Flags(0x0020);
    /// Sync burst in progress.
    pub const SYNC_BURST: Flags = Flags(0x0040);

    /// No flags set.
    pub const fn empty() -> Flags {
        Flags(0)
    }

    /// Whether all bits of `other` are set in `self`.
    pub const fn contains(self, other: Flags) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for Flags {
    type Output = Flags;
    fn bitor(self, rhs: Flags) -> Flags {
        Flags(self.0 | rhs.0)
    }
}

impl fmt::Debug for Flags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Flags({:#06x})", self.0)
    }
}

/// A PTPv1 subdomain name: up to 16 ASCII bytes, NUL-padded.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Subdomain(pub [u8; SUBDOMAIN_LEN]);

impl Subdomain {
    /// The default subdomain, `_DFLT`, used by Dante.
    pub const DEFAULT: Subdomain = Subdomain(*b"_DFLT\0\0\0\0\0\0\0\0\0\0\0");

    /// Encodes `name`, or returns `None` if it does not fit in 16 bytes.
    pub fn new(name: &str) -> Option<Subdomain> {
        let bytes = name.as_bytes();
        if bytes.len() > SUBDOMAIN_LEN {
            return None;
        }
        let mut out = [0u8; SUBDOMAIN_LEN];
        out[..bytes.len()].copy_from_slice(bytes);
        Some(Subdomain(out))
    }

    /// The name without NUL padding (lossy for non-UTF-8 names).
    pub fn name(&self) -> String {
        let end = self.0.iter().position(|&b| b == 0).unwrap_or(SUBDOMAIN_LEN);
        String::from_utf8_lossy(&self.0[..end]).into_owned()
    }
}

impl Default for Subdomain {
    fn default() -> Self {
        Subdomain::DEFAULT
    }
}

impl fmt::Debug for Subdomain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Subdomain({:?})", self.name())
    }
}

/// Identity of a PTP port: the clock UUID (normally a MAC address) plus the
/// port number on that clock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PortIdentity {
    pub uuid: [u8; 6],
    pub port_id: u16,
}

/// A PTPv1 `TimeRepresentation`.
///
/// The sign of `nanoseconds` gives the sign of the whole value; timestamps on
/// the wire are always non-negative.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Timestamp {
    pub seconds: u32,
    pub nanoseconds: i32,
}

impl Timestamp {
    /// Converts a non-negative time in nanoseconds. Seconds wrap after 2^32
    /// (year 2106 for an epoch of 1970).
    pub fn from_ns(ns: u64) -> Timestamp {
        Timestamp { seconds: (ns / 1_000_000_000) as u32, nanoseconds: (ns % 1_000_000_000) as i32 }
    }

    /// The time in nanoseconds (negative if `nanoseconds` is negative).
    pub fn to_ns(self) -> i64 {
        let magnitude = self.seconds as i64 * 1_000_000_000 + (self.nanoseconds as i64).abs();
        if self.nanoseconds < 0 { -magnitude } else { magnitude }
    }
}

/// The common header of every PTPv1 message.
///
/// `versionPTP`, `messageType` and `control` are not stored: they are
/// implied by the message body and filled in by [`Message::encode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// `versionNetwork` (1 for IEEE 1588-2002).
    pub version_network: u16,
    pub subdomain: Subdomain,
    pub source_communication_technology: u8,
    /// `sourceUuid` and `sourcePortId`.
    pub source: PortIdentity,
    pub sequence_id: u16,
    pub flags: Flags,
}

impl Header {
    /// A header for messages sent from `source` on Ethernet.
    pub fn new(
        subdomain: Subdomain,
        source: PortIdentity,
        sequence_id: u16,
        flags: Flags,
    ) -> Header {
        Header {
            version_network: VERSION_NETWORK,
            subdomain,
            source_communication_technology: COMM_TECH_ETHERNET,
            source,
            sequence_id,
            flags,
        }
    }
}

/// Body of Sync and Delay_Req messages.
///
/// Besides the origin timestamp, a Sync carries the sender's view of the
/// grandmaster and its own clock quality, which slaves use for master
/// selection. A Delay_Req has the same layout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncBody {
    pub origin_timestamp: Timestamp,
    pub epoch_number: u16,
    pub current_utc_offset: i16,
    pub grandmaster_communication_technology: u8,
    pub grandmaster_clock_uuid: [u8; 6],
    pub grandmaster_port_id: u16,
    pub grandmaster_sequence_id: u16,
    pub grandmaster_clock_stratum: u8,
    /// Four ASCII characters, e.g. `ATOM`, `GPS\0`, `DFLT`.
    pub grandmaster_clock_identifier: [u8; 4],
    pub grandmaster_clock_variance: i16,
    pub grandmaster_preferred: bool,
    pub grandmaster_is_boundary_clock: bool,
    /// log2 of the Sync interval in seconds.
    pub sync_interval: i8,
    pub local_clock_variance: i16,
    pub local_steps_removed: u16,
    pub local_clock_stratum: u8,
    pub local_clock_identifier: [u8; 4],
    pub parent_communication_technology: u8,
    pub parent_uuid: [u8; 6],
    pub parent_port_field: u16,
    pub estimated_master_variance: i16,
    pub estimated_master_drift: i32,
    pub utc_reasonable: bool,
}

/// Body of a Follow_Up message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FollowUpBody {
    /// `sequenceId` of the Sync this Follow_Up belongs to.
    pub associated_sequence_id: u16,
    /// When the Sync actually left the master.
    pub precise_origin_timestamp: Timestamp,
}

/// Body of a Delay_Resp message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DelayRespBody {
    /// When the master received the Delay_Req.
    pub delay_receipt_timestamp: Timestamp,
    pub requesting_source_communication_technology: u8,
    /// `requestingSourceUuid` and `requestingSourcePortId`.
    pub requesting_source: PortIdentity,
    pub requesting_source_sequence_id: u16,
}

/// A message body, which also determines the `control` field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Body {
    Sync(SyncBody),
    DelayReq(SyncBody),
    FollowUp(FollowUpBody),
    DelayResp(DelayRespBody),
}

impl Body {
    /// The `control` value of this body.
    pub fn control(&self) -> Control {
        match self {
            Body::Sync(_) => Control::Sync,
            Body::DelayReq(_) => Control::DelayReq,
            Body::FollowUp(_) => Control::FollowUp,
            Body::DelayResp(_) => Control::DelayResp,
        }
    }

    /// Encoded length of the whole message with this body.
    pub fn encoded_len(&self) -> usize {
        match self {
            Body::Sync(_) => SYNC_LEN,
            Body::DelayReq(_) => DELAY_REQ_LEN,
            Body::FollowUp(_) => FOLLOW_UP_LEN,
            Body::DelayResp(_) => DELAY_RESP_LEN,
        }
    }
}

/// A complete PTPv1 message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Message {
    pub header: Header,
    pub body: Body,
}

/// Why a packet could not be decoded as a supported PTPv1 message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("packet too short: {len} bytes, need {needed}")]
    TooShort { len: usize, needed: usize },
    #[error("PTPv2 packet")]
    PtpV2,
    #[error("unknown PTP version bytes {0:#04x} {1:#04x}")]
    UnknownVersion(u8, u8),
    #[error("PTPv1 management message (not supported)")]
    Management,
    #[error("unknown PTPv1 control value {0}")]
    UnknownControl(u8),
}

/// Whether `buf` looks like a PTPv1 packet (`versionPTP == 1`).
pub fn is_ptp_v1(buf: &[u8]) -> bool {
    buf.len() >= 2 && buf[0] == 0 && buf[1] == 1
}

/// Whether `buf` looks like a PTPv2 packet (`versionPTP` nibble == 2).
pub fn is_ptp_v2(buf: &[u8]) -> bool {
    buf.len() >= 2 && buf[1] & 0x0f == 2
}

impl Message {
    /// The `control` value of this message.
    pub fn control(&self) -> Control {
        self.body.control()
    }

    /// Decodes a UDP payload. Trailing bytes beyond the message are ignored.
    pub fn decode(buf: &[u8]) -> Result<Message, DecodeError> {
        if buf.len() < 2 {
            return Err(DecodeError::TooShort { len: buf.len(), needed: HEADER_LEN });
        }
        if !is_ptp_v1(buf) {
            return Err(if is_ptp_v2(buf) {
                DecodeError::PtpV2
            } else {
                DecodeError::UnknownVersion(buf[0], buf[1])
            });
        }
        if buf.len() < HEADER_LEN {
            return Err(DecodeError::TooShort { len: buf.len(), needed: HEADER_LEN });
        }
        let r = Reader(buf);
        let control = Control::from_u8(buf[32]).ok_or(DecodeError::UnknownControl(buf[32]))?;
        let needed = match control {
            Control::Sync | Control::DelayReq => SYNC_LEN,
            Control::FollowUp => FOLLOW_UP_LEN,
            Control::DelayResp => DELAY_RESP_LEN,
            Control::Management => return Err(DecodeError::Management),
        };
        if buf.len() < needed {
            return Err(DecodeError::TooShort { len: buf.len(), needed });
        }
        let header = Header {
            version_network: r.u16(2),
            subdomain: Subdomain(r.array(4)),
            // byte 20, messageType, is implied by `control`.
            source_communication_technology: r.u8(21),
            source: PortIdentity { uuid: r.array(22), port_id: r.u16(28) },
            sequence_id: r.u16(30),
            flags: Flags(r.u16(34)),
        };
        let body = match control {
            Control::Sync => Body::Sync(decode_sync_body(&r)),
            Control::DelayReq => Body::DelayReq(decode_sync_body(&r)),
            Control::FollowUp => Body::FollowUp(FollowUpBody {
                associated_sequence_id: r.u16(42),
                precise_origin_timestamp: r.timestamp(44),
            }),
            Control::DelayResp => Body::DelayResp(DelayRespBody {
                delay_receipt_timestamp: r.timestamp(40),
                requesting_source_communication_technology: r.u8(49),
                requesting_source: PortIdentity { uuid: r.array(50), port_id: r.u16(56) },
                requesting_source_sequence_id: r.u16(58),
            }),
            Control::Management => unreachable!("rejected above"),
        };
        Ok(Message { header, body })
    }

    /// Encodes the message. Reserved bytes are zero.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer(vec![0u8; self.body.encoded_len()]);
        let h = &self.header;
        let control = self.control();
        w.u16(0, VERSION_PTP);
        w.u16(2, h.version_network);
        w.bytes(4, &h.subdomain.0);
        w.u8(20, control.message_type());
        w.u8(21, h.source_communication_technology);
        w.bytes(22, &h.source.uuid);
        w.u16(28, h.source.port_id);
        w.u16(30, h.sequence_id);
        w.u8(32, control as u8);
        w.u16(34, h.flags.0);
        match &self.body {
            Body::Sync(b) | Body::DelayReq(b) => encode_sync_body(&mut w, b),
            Body::FollowUp(b) => {
                w.u16(42, b.associated_sequence_id);
                w.timestamp(44, b.precise_origin_timestamp);
            }
            Body::DelayResp(b) => {
                w.timestamp(40, b.delay_receipt_timestamp);
                w.u8(49, b.requesting_source_communication_technology);
                w.bytes(50, &b.requesting_source.uuid);
                w.u16(56, b.requesting_source.port_id);
                w.u16(58, b.requesting_source_sequence_id);
            }
        }
        w.0
    }
}

fn decode_sync_body(r: &Reader<'_>) -> SyncBody {
    SyncBody {
        origin_timestamp: r.timestamp(40),
        epoch_number: r.u16(48),
        current_utc_offset: r.u16(50) as i16,
        grandmaster_communication_technology: r.u8(53),
        grandmaster_clock_uuid: r.array(54),
        grandmaster_port_id: r.u16(60),
        grandmaster_sequence_id: r.u16(62),
        grandmaster_clock_stratum: r.u8(67),
        grandmaster_clock_identifier: r.array(68),
        grandmaster_clock_variance: r.u16(74) as i16,
        grandmaster_preferred: r.u8(77) != 0,
        grandmaster_is_boundary_clock: r.u8(79) != 0,
        sync_interval: r.u8(83) as i8,
        local_clock_variance: r.u16(86) as i16,
        local_steps_removed: r.u16(90),
        local_clock_stratum: r.u8(95),
        local_clock_identifier: r.array(96),
        parent_communication_technology: r.u8(101),
        parent_uuid: r.array(102),
        parent_port_field: r.u16(110),
        estimated_master_variance: r.u16(114) as i16,
        estimated_master_drift: r.u32(116) as i32,
        utc_reasonable: r.u8(123) != 0,
    }
}

fn encode_sync_body(w: &mut Writer, b: &SyncBody) {
    w.timestamp(40, b.origin_timestamp);
    w.u16(48, b.epoch_number);
    w.u16(50, b.current_utc_offset as u16);
    w.u8(53, b.grandmaster_communication_technology);
    w.bytes(54, &b.grandmaster_clock_uuid);
    w.u16(60, b.grandmaster_port_id);
    w.u16(62, b.grandmaster_sequence_id);
    w.u8(67, b.grandmaster_clock_stratum);
    w.bytes(68, &b.grandmaster_clock_identifier);
    w.u16(74, b.grandmaster_clock_variance as u16);
    w.u8(77, b.grandmaster_preferred as u8);
    w.u8(79, b.grandmaster_is_boundary_clock as u8);
    w.u8(83, b.sync_interval as u8);
    w.u16(86, b.local_clock_variance as u16);
    w.u16(90, b.local_steps_removed);
    w.u8(95, b.local_clock_stratum);
    w.bytes(96, &b.local_clock_identifier);
    w.u8(101, b.parent_communication_technology);
    w.bytes(102, &b.parent_uuid);
    w.u16(110, b.parent_port_field);
    w.u16(114, b.estimated_master_variance as u16);
    w.u32(116, b.estimated_master_drift as u32);
    w.u8(123, b.utc_reasonable as u8);
}

/// Big-endian field reader. Callers check the length first.
struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn u8(&self, at: usize) -> u8 {
        self.0[at]
    }
    fn u16(&self, at: usize) -> u16 {
        u16::from_be_bytes(self.array(at))
    }
    fn u32(&self, at: usize) -> u32 {
        u32::from_be_bytes(self.array(at))
    }
    fn array<const N: usize>(&self, at: usize) -> [u8; N] {
        let mut out = [0u8; N];
        out.copy_from_slice(&self.0[at..at + N]);
        out
    }
    fn timestamp(&self, at: usize) -> Timestamp {
        Timestamp { seconds: self.u32(at), nanoseconds: self.u32(at + 4) as i32 }
    }
}

/// Big-endian field writer over a zeroed buffer of the final length.
struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, at: usize, v: u8) {
        self.0[at] = v;
    }
    fn u16(&mut self, at: usize, v: u16) {
        self.bytes(at, &v.to_be_bytes());
    }
    fn u32(&mut self, at: usize, v: u32) {
        self.bytes(at, &v.to_be_bytes());
    }
    fn bytes(&mut self, at: usize, v: &[u8]) {
        self.0[at..at + v.len()].copy_from_slice(v);
    }
    fn timestamp(&mut self, at: usize, t: Timestamp) {
        self.u32(at, t.seconds);
        self.u32(at + 4, t.nanoseconds as u32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(seq: u16, flags: Flags) -> Header {
        Header::new(
            Subdomain::DEFAULT,
            PortIdentity { uuid: [0x00, 0x1d, 0xc1, 0x12, 0x34, 0x56], port_id: 1 },
            seq,
            flags,
        )
    }

    fn sample_sync_body() -> SyncBody {
        SyncBody {
            origin_timestamp: Timestamp { seconds: 1_700_000_000, nanoseconds: 123_456_789 },
            epoch_number: 3,
            current_utc_offset: 37,
            grandmaster_communication_technology: COMM_TECH_ETHERNET,
            grandmaster_clock_uuid: [0x00, 0x1d, 0xc1, 0xaa, 0xbb, 0xcc],
            grandmaster_port_id: 1,
            grandmaster_sequence_id: 4242,
            grandmaster_clock_stratum: 4,
            grandmaster_clock_identifier: *b"GPS\0",
            grandmaster_clock_variance: -4000,
            grandmaster_preferred: true,
            grandmaster_is_boundary_clock: true,
            sync_interval: -2,
            local_clock_variance: -3000,
            local_steps_removed: 1,
            local_clock_stratum: 5,
            local_clock_identifier: *b"DFLT",
            parent_communication_technology: COMM_TECH_ETHERNET,
            parent_uuid: [0x00, 0x1d, 0xc1, 0xaa, 0xbb, 0xcc],
            parent_port_field: 1,
            estimated_master_variance: -100,
            estimated_master_drift: -123_456,
            utc_reasonable: true,
        }
    }

    fn round_trip(msg: Message, expected_len: usize) {
        let bytes = msg.encode();
        assert_eq!(bytes.len(), expected_len);
        assert!(is_ptp_v1(&bytes));
        assert_eq!(bytes[20], msg.control().message_type());
        assert_eq!(bytes[32], msg.control() as u8);
        assert_eq!(Message::decode(&bytes), Ok(msg));
    }

    #[test]
    fn sync_round_trip() {
        round_trip(
            Message { header: header(7, Flags::ASSIST), body: Body::Sync(sample_sync_body()) },
            SYNC_LEN,
        );
    }

    #[test]
    fn delay_req_round_trip() {
        round_trip(
            Message {
                header: header(65535, Flags::empty()),
                body: Body::DelayReq(sample_sync_body()),
            },
            DELAY_REQ_LEN,
        );
    }

    #[test]
    fn follow_up_round_trip() {
        round_trip(
            Message {
                header: header(8, Flags::empty()),
                body: Body::FollowUp(FollowUpBody {
                    associated_sequence_id: 7,
                    precise_origin_timestamp: Timestamp { seconds: 42, nanoseconds: 999_999_999 },
                }),
            },
            FOLLOW_UP_LEN,
        );
    }

    #[test]
    fn delay_resp_round_trip() {
        round_trip(
            Message {
                header: header(9, Flags::empty()),
                body: Body::DelayResp(DelayRespBody {
                    delay_receipt_timestamp: Timestamp { seconds: 43, nanoseconds: 1 },
                    requesting_source_communication_technology: COMM_TECH_ETHERNET,
                    requesting_source: PortIdentity { uuid: [1, 2, 3, 4, 5, 6], port_id: 1 },
                    requesting_source_sequence_id: 77,
                }),
            },
            DELAY_RESP_LEN,
        );
    }

    /// A two-step Sync written out byte by byte, as it would appear in a
    /// packet capture.
    #[rustfmt::skip]
    const ASSIST_SYNC: [u8; SYNC_LEN] = [
        0x00, 0x01, 0x00, 0x01,                         //  0 versionPTP 1, versionNetwork 1
        b'_', b'D', b'F', b'L', b'T', 0, 0, 0,          //  4 subdomain "_DFLT"
        0, 0, 0, 0, 0, 0, 0, 0,
        0x01, 0x01,                                     // 20 messageType Event, Ethernet
        0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0c,             // 22 sourceUuid
        0x00, 0x01,                                     // 28 sourcePortId 1
        0x12, 0x34,                                     // 30 sequenceId 0x1234
        0x00, 0x00,                                     // 32 control Sync, reserved
        0x00, 0x08,                                     // 34 flags ASSIST
        0x00, 0x00, 0x00, 0x00,                         // 36 reserved
        0x65, 0x53, 0xf1, 0x00,                         // 40 originTimestamp.seconds 1700000000
        0x1d, 0xcd, 0x65, 0x00,                         // 44 originTimestamp.nanoseconds 500000000
        0x00, 0x00, 0x00, 0x25,                         // 48 epoch 0, currentUTCOffset 37
        0x00, 0x01,                                     // 52 reserved, gm comm tech Ethernet
        0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0c,             // 54 grandmasterClockUuid
        0x00, 0x01, 0x12, 0x30,                         // 60 gm port id 1, gm sequence 0x1230
        0x00, 0x00, 0x00, 0x03,                         // 64 reserved, gm stratum 3
        b'D', b'F', b'L', b'T',                         // 68 gm identifier
        0x00, 0x00, 0xf0, 0x60,                         // 72 reserved, gm variance -4000
        0x00, 0x00, 0x00, 0x00,                         // 76 preferred false, boundary false
        0x00, 0x00, 0x00, 0xfe,                         // 80 reserved, syncInterval -2
        0x00, 0x00, 0xf0, 0x60,                         // 84 reserved, local variance -4000
        0x00, 0x00, 0x00, 0x00,                         // 88 reserved, stepsRemoved 0
        0x00, 0x00, 0x00, 0x03,                         // 92 reserved, local stratum 3
        b'D', b'F', b'L', b'T',                         // 96 local identifier
        0x00, 0x01,                                     // 100 reserved, parent comm tech
        0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0c,             // 102 parentUuid
        0x00, 0x00, 0x00, 0x01,                         // 108 reserved, parent port 1
        0x00, 0x00, 0x00, 0x00,                         // 112 reserved, est. master variance 0
        0x00, 0x00, 0x00, 0x00,                         // 116 est. master drift 0
        0x00, 0x00, 0x00, 0x00,                         // 120 reserved, utcReasonable false
    ];

    #[test]
    fn decodes_handwritten_assist_sync() {
        let msg = Message::decode(&ASSIST_SYNC).unwrap();
        assert_eq!(msg.header.subdomain, Subdomain::DEFAULT);
        assert_eq!(msg.header.source.uuid, [0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0c]);
        assert_eq!(msg.header.source.port_id, 1);
        assert_eq!(msg.header.sequence_id, 0x1234);
        assert!(msg.header.flags.contains(Flags::ASSIST));
        assert!(!msg.header.flags.contains(Flags::BOUNDARY_CLOCK));
        let Body::Sync(b) = msg.body else { panic!("not a Sync: {msg:?}") };
        assert_eq!(
            b.origin_timestamp,
            Timestamp { seconds: 1_700_000_000, nanoseconds: 500_000_000 }
        );
        assert_eq!(b.origin_timestamp.to_ns(), 1_700_000_000_500_000_000);
        assert_eq!(b.current_utc_offset, 37);
        assert_eq!(b.grandmaster_clock_uuid, [0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0c]);
        assert_eq!(b.grandmaster_sequence_id, 0x1230);
        assert_eq!(b.grandmaster_clock_stratum, 3);
        assert_eq!(&b.grandmaster_clock_identifier, b"DFLT");
        assert_eq!(b.grandmaster_clock_variance, -4000);
        assert!(!b.grandmaster_preferred);
        assert_eq!(b.sync_interval, -2);
        assert_eq!(b.local_clock_stratum, 3);
        assert_eq!(b.parent_port_field, 1);
        // Re-encoding reproduces the capture exactly.
        assert_eq!(msg.encode(), ASSIST_SYNC);
    }

    #[test]
    fn rejects_short_packets() {
        assert_eq!(Message::decode(&[]), Err(DecodeError::TooShort { len: 0, needed: HEADER_LEN }));
        assert_eq!(
            Message::decode(&ASSIST_SYNC[..30]),
            Err(DecodeError::TooShort { len: 30, needed: HEADER_LEN })
        );
        assert_eq!(
            Message::decode(&ASSIST_SYNC[..SYNC_LEN - 1]),
            Err(DecodeError::TooShort { len: SYNC_LEN - 1, needed: SYNC_LEN })
        );
        // A Follow_Up header with a truncated body.
        let mut fu = ASSIST_SYNC[..48].to_vec();
        fu[32] = Control::FollowUp as u8;
        assert_eq!(
            Message::decode(&fu),
            Err(DecodeError::TooShort { len: 48, needed: FOLLOW_UP_LEN })
        );
    }

    #[test]
    fn rejects_other_versions_and_messages() {
        // PTPv2 Sync: transportSpecific/messageType 0x00, versionPTP 0x02.
        let mut v2 = [0u8; 44];
        v2[1] = 0x02;
        assert_eq!(Message::decode(&v2), Err(DecodeError::PtpV2));
        // PTPv2.1 sets minorVersionPTP in the high nibble.
        v2[1] = 0x12;
        assert_eq!(Message::decode(&v2), Err(DecodeError::PtpV2));
        assert_eq!(
            Message::decode(&[0x47, 0x11, 0, 0]),
            Err(DecodeError::UnknownVersion(0x47, 0x11))
        );

        let mut mgmt = ASSIST_SYNC;
        mgmt[32] = Control::Management as u8;
        assert_eq!(Message::decode(&mgmt), Err(DecodeError::Management));
        mgmt[32] = 9;
        assert_eq!(Message::decode(&mgmt), Err(DecodeError::UnknownControl(9)));
    }

    #[test]
    fn timestamp_conversions() {
        let t = Timestamp::from_ns(1_700_000_000_123_456_789);
        assert_eq!(t, Timestamp { seconds: 1_700_000_000, nanoseconds: 123_456_789 });
        assert_eq!(t.to_ns(), 1_700_000_000_123_456_789);
        assert_eq!(Timestamp { seconds: 1, nanoseconds: -5 }.to_ns(), -1_000_000_005);
    }

    #[test]
    fn subdomain_names() {
        assert_eq!(Subdomain::new("_DFLT"), Some(Subdomain::DEFAULT));
        assert_eq!(Subdomain::DEFAULT.name(), "_DFLT");
        assert_eq!(Subdomain::new("0123456789abcdef").unwrap().name(), "0123456789abcdef");
        assert!(Subdomain::new("0123456789abcdefg").is_none());
    }
}
