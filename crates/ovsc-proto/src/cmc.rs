//! CMC – control and monitoring, UDP port 8800.
//!
//! Controllers ask a device to "advertise" itself, which tells them the
//! device's identity and where to send settings/info requests.

use std::net::Ipv4Addr;

use crate::frame::{Frame, Header, encode, protocol, respond, result};
use crate::wire::Reader;
use crate::{DeviceId, Result};

/// Well-known CMC port (advertised in `_netaudio-cmc._udp`).
pub const PORT: u16 = 8800;

pub mod opcode {
    pub const DEVICE_ADVERTISEMENT: u16 = 0x1001;
}

/// Response to [`opcode::DEVICE_ADVERTISEMENT`] (22 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceAdvertisement {
    pub process_id: u16,
    pub device_id: DeviceId,
    pub ip: Ipv4Addr,
    /// Port of the conmon/settings service (normally 8700).
    pub info_port: u16,
}

impl DeviceAdvertisement {
    pub const SIZE: usize = 22;

    pub fn encode_request(seq: u16) -> Vec<u8> {
        encode(protocol::CMC, seq, opcode::DEVICE_ADVERTISEMENT, result::REQUEST, &[])
    }

    pub fn encode_response(&self, request: &Header) -> Vec<u8> {
        let mut w = crate::wire::Writer::with_capacity(Self::SIZE);
        w.u16(self.process_id);
        w.bytes(&self.device_id);
        w.u16(1);
        w.u16(0);
        w.bytes(&self.ip.octets());
        w.u16(self.info_port);
        w.u16(0);
        respond(request, result::SUCCESS, w.as_slice())
    }

    pub fn decode_response(frame: &Frame<'_>) -> Result<Self> {
        let mut r = Reader::new(frame.payload());
        let process_id = r.u16()?;
        let device_id = r.array()?;
        r.skip(4)?;
        let ip = Ipv4Addr::from(r.array::<4>()?);
        let info_port = r.u16()?;
        Ok(Self { process_id, device_id, ip, info_port })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses a packet built in the test; leaking keeps the frame borrow simple.
    fn leak_parse(pkt: Vec<u8>) -> Frame<'static> {
        Frame::parse(Box::leak(pkt.into_boxed_slice())).unwrap()
    }

    #[test]
    fn advertisement_round_trip() {
        let req = leak_parse(DeviceAdvertisement::encode_request(5));
        assert_eq!(req.header.protocol, protocol::CMC);
        let adv = DeviceAdvertisement {
            process_id: 3,
            device_id: [0x00, 0x1d, 0xc1, 0xff, 0xfe, 0x12, 0x34, 0x56],
            ip: Ipv4Addr::new(192, 168, 7, 9),
            info_port: 8700,
        };
        let pkt = adv.encode_response(&req.header);
        assert_eq!(pkt.len(), 10 + DeviceAdvertisement::SIZE);
        let resp = Frame::parse(&pkt).unwrap();
        assert_eq!(resp.header.seq, 5);
        assert_eq!(DeviceAdvertisement::decode_response(&resp).unwrap(), adv);
    }
}
