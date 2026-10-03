//! Flow control ("DBCP"), UDP port 4455.
//!
//! A receiver that wants audio from a transmitter asks it to create a unicast
//! flow: "send channels X, Y, Z at this sample rate and bit depth, N frames
//! per packet, to my address A port P". The transmitter answers with an
//! opaque 6-byte handle used to update or stop the flow later.
//!
//! The port comes from the SRV record of the transmitter's
//! `_netaudio-chan._udp` services and the protocol id from their `dbcp1` TXT
//! key (`0x1102`).

use std::net::Ipv4Addr;

use crate::frame::{Frame, Header, PacketBuilder, encode, protocol, respond, result};
use crate::wire::{Reader, Writer, u16_at};
use crate::{Error, Result};

/// Default flow-control port.
pub const PORT: u16 = 4455;

pub mod opcode {
    pub const REQUEST_FLOW: u16 = 0x0100;
    pub const STOP_FLOW: u16 = 0x0101;
    pub const UPDATE_FLOW: u16 = 0x0102;
}

/// Result codes returned by transmitters.
pub mod error {
    /// The handle is unknown (or the flow expired for lack of keepalives).
    pub const FLOW_NOT_FOUND: u16 = 0x0103;
    /// Requested sample rate differs from the transmitter's.
    pub const SAMPLE_RATE_MISMATCH: u16 = 0x0301;
    /// Invalid parameter (channel id, frames per packet, …). Guessed meaning.
    pub const INVALID_PARAMETER: u16 = 0x0302;
    /// The transmitter has no free flows.
    pub const TOO_MANY_FLOWS: u16 = 0x0315;
}

/// Opaque identifier of a flow, chosen by the transmitter.
pub type FlowHandle = [u8; 6];

/// A receiver's request for a new unicast flow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlowRequest {
    /// Receiving device's name.
    pub rx_device: String,
    /// Receiver-chosen flow name (used by controllers for display).
    pub flow_name: String,
    pub sample_rate: u32,
    pub bits_per_sample: u32,
    /// Frames (samples per channel) per packet.
    pub fpp: u16,
    /// 1-based transmit channel ids in packet order; 0 leaves the slot empty.
    pub channels: Vec<u16>,
    /// Where the transmitter must send the audio.
    pub rx_addr: Ipv4Addr,
    pub rx_port: u16,
}

impl FlowRequest {
    pub fn encode(&self, seq: u16) -> Vec<u8> {
        let n = self.channels.len();
        let strings_at = 48 + 2 * n;
        let mut b = PacketBuilder::new();
        let host_ptr = b.w.u16(0) as usize;
        b.w.u32(self.sample_rate);
        b.w.u32(self.bits_per_sample);
        b.w.u16(1);
        b.w.u16(n as u16);
        let socket_ptr = b.w.u16(0) as usize;
        for &ch in &self.channels {
            b.w.u16(ch);
        }
        let trailer_ptr = b.w.u16(0) as usize;
        let trailer_at = b.w.u16(0x0a00);
        b.w.u16(0x0002);
        b.w.u16(self.fpp);
        let name_ptr = b.w.u16(0) as usize;
        b.w.zeros(12);
        debug_assert_eq!(b.w.len(), strings_at);
        let host_at = b.w.cstr(&self.rx_device);
        let name_at = b.w.cstr(&self.flow_name);
        b.w.align(8);
        let socket_at = b.w.u16(0x0802);
        b.w.u16(self.rx_port);
        b.w.bytes(&self.rx_addr.octets());
        b.w.patch_u16(host_ptr, host_at);
        b.w.patch_u16(socket_ptr, socket_at);
        b.w.patch_u16(trailer_ptr, trailer_at);
        b.w.patch_u16(name_ptr, name_at);
        b.finish(protocol::DBCP, seq, opcode::REQUEST_FLOW, result::REQUEST)
    }

    pub fn decode(frame: &Frame<'_>) -> Result<Self> {
        let mut r = Reader::new(frame.payload());
        let host_at = r.u16()?;
        let sample_rate = r.u32()?;
        let bits_per_sample = r.u32()?;
        let _one = r.u16()?;
        let n = r.u16()? as usize;
        let socket_at = r.u16()? as usize;
        let channels = (0..n).map(|_| r.u16()).collect::<Result<Vec<_>>>()?;
        let _trailer_at = r.u16()?;
        let _a00 = r.u16()?;
        let _two = r.u16()?;
        let fpp = r.u16()?;
        let name_at = r.u16()?;

        let p = frame.packet;
        if u16_at(p, socket_at)? != 0x0802 {
            return Err(Error::Invalid("flow request socket descriptor"));
        }
        let rx_port = u16_at(p, socket_at + 2)?;
        let rx_addr = Ipv4Addr::from(Reader::at(p, socket_at + 4).array::<4>()?);
        Ok(Self {
            rx_device: frame.string_at(host_at)?,
            flow_name: frame.string_at(name_at)?,
            sample_rate,
            bits_per_sample,
            fpp,
            channels,
            rx_addr,
            rx_port,
        })
    }
}

/// Successful response to [`opcode::REQUEST_FLOW`].
pub fn encode_flow_created(request: &Header, handle: FlowHandle) -> Vec<u8> {
    respond(request, result::SUCCESS, &handle)
}

pub fn decode_flow_created(frame: &Frame<'_>) -> Result<FlowHandle> {
    Reader::new(frame.payload()).array()
}

/// Request to stop a flow.
pub fn encode_stop_flow(seq: u16, handle: FlowHandle) -> Vec<u8> {
    encode(protocol::DBCP, seq, opcode::STOP_FLOW, result::REQUEST, &handle)
}

pub fn decode_stop_flow(frame: &Frame<'_>) -> Result<FlowHandle> {
    Reader::new(frame.payload()).array()
}

/// Request to change the channels carried by an existing flow.
pub fn encode_update_flow(seq: u16, handle: FlowHandle, channels: &[u16]) -> Vec<u8> {
    let mut w = Writer::new();
    w.bytes(&handle);
    w.u16(channels.len() as u16);
    for &ch in channels {
        w.u16(ch);
    }
    encode(protocol::DBCP, seq, opcode::UPDATE_FLOW, result::REQUEST, w.as_slice())
}

pub fn decode_update_flow(frame: &Frame<'_>) -> Result<(FlowHandle, Vec<u16>)> {
    let mut r = Reader::new(frame.payload());
    let handle = r.array()?;
    let n = r.u16()?;
    let channels = (0..n).map(|_| r.u16()).collect::<Result<Vec<_>>>()?;
    Ok((handle, channels))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> FlowRequest {
        FlowRequest {
            rx_device: "studio-mac".into(),
            flow_name: "1_3".into(),
            sample_rate: 48_000,
            bits_per_sample: 24,
            fpp: 16,
            channels: vec![1, 2, 0, 8],
            rx_addr: Ipv4Addr::new(192, 168, 1, 50),
            rx_port: 41234,
        }
    }

    #[test]
    fn flow_request_round_trip_and_layout() {
        let req = sample();
        let pkt = req.encode(77);
        let f = Frame::parse(&pkt).unwrap();
        assert_eq!(f.header.protocol, protocol::DBCP);
        assert_eq!(f.header.opcode, opcode::REQUEST_FLOW);
        // Strings start right after the fixed part: 0x26 + 2n bytes of payload.
        assert_eq!(u16_at(&pkt, 10).unwrap() as usize, 10 + 0x26 + 2 * 4);
        // The trailer pointer points at the 0x0a00 marker that follows it.
        let trailer_ptr_at = 26 + 2 * 4;
        assert_eq!(u16_at(&pkt, trailer_ptr_at).unwrap() as usize, trailer_ptr_at + 2);
        assert_eq!(u16_at(&pkt, trailer_ptr_at + 2).unwrap(), 0x0a00);
        // Socket descriptor is 8-byte aligned.
        assert_eq!(u16_at(&pkt, 24).unwrap() % 8, 0);
        assert_eq!(FlowRequest::decode(&f).unwrap(), req);
    }

    #[test]
    fn stop_and_update_round_trip() {
        let handle = [0, 0, 0, 3, 0xab, 0xcd];
        let f_pkt = encode_stop_flow(1, handle);
        let f = Frame::parse(&f_pkt).unwrap();
        assert_eq!(decode_stop_flow(&f).unwrap(), handle);

        let u_pkt = encode_update_flow(2, handle, &[4, 5, 0]);
        let u = Frame::parse(&u_pkt).unwrap();
        assert_eq!(decode_update_flow(&u).unwrap(), (handle, vec![4, 5, 0]));

        let created = encode_flow_created(&f.header, handle);
        assert_eq!(decode_flow_created(&Frame::parse(&created).unwrap()).unwrap(), handle);
    }
}
