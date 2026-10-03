//! The 10-byte request/response frame shared by ARC (port 4440), CMC (port
//! 8800) and flow control / DBCP (port 4455).
//!
//! ```text
//!  0      2      4      6      8      10
//!  +------+------+------+------+------+-------------...
//!  | prot | len  | seq  | op   | res  | payload
//!  +------+------+------+------+------+-------------...
//! ```
//!
//! * `prot` – protocol identifier / version (e.g. `0x27FF` for ARC).
//! * `len` – total packet length including this header.
//! * `seq` – transaction id chosen by the requester, echoed in the response.
//! * `op` – opcode.
//! * `res` – `0` in requests, a result code in responses.
//!
//! Offsets embedded in payloads are measured from the start of the packet
//! (i.e. they include these 10 header bytes).

use crate::wire::{Writer, cstr_at, opt_cstr_at, u16_at};
use crate::{Error, Result};

/// Length of the frame header.
pub const HEADER_LEN: usize = 10;

/// Protocol identifiers seen in the first header field.
pub mod protocol {
    /// Default ARC protocol id used by most controllers and devices.
    pub const ARC: u16 = 0x27FF;
    /// Older ARC protocol id, still used for some requests by Dante Controller.
    pub const ARC_2729: u16 = 0x2729;
    /// Newer ARC protocol ids seen from Dante Controller.
    pub const ARC_2801: u16 = 0x2801;
    pub const ARC_2809: u16 = 0x2809;
    pub const ARC_280C: u16 = 0x280C;
    pub const ARC_280F: u16 = 0x280F;
    /// CMC protocol id.
    pub const CMC: u16 = 0x1200;
    /// Flow control protocol id (advertised as `dbcp1=0x1102` in mDNS).
    pub const DBCP: u16 = 0x1102;
}

/// Result codes found in the last header field of responses.
pub mod result {
    /// Field value in requests.
    pub const REQUEST: u16 = 0x0000;
    pub const SUCCESS: u16 = 0x0001;
    pub const ERROR: u16 = 0x0022;
    pub const FRONTEND_UNAVAILABLE: u16 = 0x0030;
    /// Success, and more items are available on further pages.
    pub const MORE_PAGES: u16 = 0x8112;
    /// Generic failure used by OpenVirtualSoundcard when no better code is known.
    pub const FAILURE: u16 = 0xFFFF;
}

/// A parsed frame header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub protocol: u16,
    pub length: u16,
    pub seq: u16,
    pub opcode: u16,
    pub result: u16,
}

/// A received packet: header plus a view of the whole datagram.
#[derive(Clone, Copy, Debug)]
pub struct Frame<'a> {
    pub header: Header,
    /// The whole packet, header included. Embedded offsets index into this.
    pub packet: &'a [u8],
}

impl<'a> Frame<'a> {
    /// Parses the header of `packet`. Trailing bytes beyond the declared
    /// length are ignored; a declared length larger than the datagram is an
    /// error.
    pub fn parse(packet: &'a [u8]) -> Result<Self> {
        if packet.len() < HEADER_LEN {
            return Err(Error::Truncated { offset: 0, needed: HEADER_LEN, len: packet.len() });
        }
        let header = Header {
            protocol: u16_at(packet, 0)?,
            length: u16_at(packet, 2)?,
            seq: u16_at(packet, 4)?,
            opcode: u16_at(packet, 6)?,
            result: u16_at(packet, 8)?,
        };
        let len = header.length as usize;
        if len < HEADER_LEN || len > packet.len() {
            return Err(Error::Invalid("frame length field"));
        }
        Ok(Self { header, packet: &packet[..len] })
    }

    /// Bytes following the header.
    pub fn payload(&self) -> &'a [u8] {
        &self.packet[HEADER_LEN..]
    }

    /// Reads a NUL-terminated string at a packet-absolute offset.
    pub fn string_at(&self, offset: u16) -> Result<String> {
        if (offset as usize) < HEADER_LEN {
            return Err(Error::Invalid("string offset points into header"));
        }
        cstr_at(self.packet, offset as usize)
    }

    /// Like [`Frame::string_at`] but offset 0 means "absent".
    pub fn opt_string_at(&self, offset: u16) -> Result<Option<String>> {
        if offset != 0 && (offset as usize) < HEADER_LEN {
            return Err(Error::Invalid("string offset points into header"));
        }
        opt_cstr_at(self.packet, offset)
    }
}

/// Builds a packet whose payload may reference itself by absolute offsets.
///
/// The header is reserved up front so that [`Writer::offset`] values are
/// already packet-absolute.
#[derive(Debug)]
pub struct PacketBuilder {
    pub w: Writer,
}

impl Default for PacketBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl PacketBuilder {
    pub fn new() -> Self {
        let mut w = Writer::with_capacity(256);
        w.zeros(HEADER_LEN);
        Self { w }
    }

    /// Fills in the header and returns the finished packet.
    pub fn finish(mut self, protocol: u16, seq: u16, opcode: u16, result: u16) -> Vec<u8> {
        let len = self.w.offset();
        self.w.patch_u16(0, protocol);
        self.w.patch_u16(2, len);
        self.w.patch_u16(4, seq);
        self.w.patch_u16(6, opcode);
        self.w.patch_u16(8, result);
        self.w.into_vec()
    }

    /// Finishes a response to `request`, echoing its protocol, sequence and
    /// opcode.
    pub fn finish_response(self, request: &Header, result: u16) -> Vec<u8> {
        self.finish(request.protocol, request.seq, request.opcode, result)
    }
}

/// Builds a packet with a payload that contains no self-references.
pub fn encode(protocol: u16, seq: u16, opcode: u16, result: u16, payload: &[u8]) -> Vec<u8> {
    let mut b = PacketBuilder::new();
    b.w.bytes(payload);
    b.finish(protocol, seq, opcode, result)
}

/// Builds a response to `request` with a plain payload.
pub fn respond(request: &Header, result: u16, payload: &[u8]) -> Vec<u8> {
    encode(request.protocol, request.seq, request.opcode, result, payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses a packet built in the test; leaking keeps the frame borrow simple.
    fn leak_parse(pkt: Vec<u8>) -> Frame<'static> {
        Frame::parse(Box::leak(pkt.into_boxed_slice())).unwrap()
    }

    #[test]
    fn encode_and_parse() {
        let pkt = encode(protocol::ARC, 0x1234, 0x1002, result::REQUEST, &[0, 0]);
        assert_eq!(pkt, [0x27, 0xff, 0x00, 0x0c, 0x12, 0x34, 0x10, 0x02, 0x00, 0x00, 0x00, 0x00]);
        let f = Frame::parse(&pkt).unwrap();
        assert_eq!(
            f.header,
            Header { protocol: 0x27ff, length: 12, seq: 0x1234, opcode: 0x1002, result: 0 }
        );
        assert_eq!(f.payload(), &[0, 0]);
    }

    #[test]
    fn response_echoes_request() {
        let req = leak_parse(encode(protocol::ARC_2729, 7, 0x3000, 0, &[]));
        let resp = leak_parse(respond(&req.header, result::SUCCESS, b"hi"));
        assert_eq!(resp.header.protocol, protocol::ARC_2729);
        assert_eq!(resp.header.seq, 7);
        assert_eq!(resp.header.opcode, 0x3000);
        assert_eq!(resp.header.result, result::SUCCESS);
        assert_eq!(resp.payload(), b"hi");
    }

    #[test]
    fn rejects_bad_lengths() {
        assert!(Frame::parse(&[0x27, 0xff, 0x00]).is_err());
        // Declared length longer than the datagram.
        assert!(Frame::parse(&[0x27, 0xff, 0x00, 0x20, 0, 0, 0, 0, 0, 0]).is_err());
        // Declared length shorter than the header.
        assert!(Frame::parse(&[0x27, 0xff, 0x00, 0x04, 0, 0, 0, 0, 0, 0]).is_err());
        // Trailing garbage is ignored.
        let f = leak_parse(vec![0x27, 0xff, 0x00, 0x0a, 0, 0, 0, 0, 0, 0, 0xee]);
        assert!(f.payload().is_empty());
    }

    #[test]
    fn strings_cannot_point_into_header() {
        let pkt = encode(protocol::ARC, 0, 0x1001, 0, b"AVIO\0");
        let f = Frame::parse(&pkt).unwrap();
        assert_eq!(f.string_at(10).unwrap(), "AVIO");
        assert!(f.string_at(4).is_err());
        assert_eq!(f.opt_string_at(0).unwrap(), None);
    }
}
