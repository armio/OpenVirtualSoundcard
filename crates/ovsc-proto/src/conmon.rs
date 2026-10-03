//! Conmon: device information, notifications and heartbeats.
//!
//! These messages share a 32-byte header and travel over UDP:
//! * controllers send *requests* unicast to the device's settings port 8700;
//! * devices publish *status notifications* to 224.0.0.231:8702 (both in
//!   reply to requests and spontaneously, e.g. after a channel rename);
//! * devices send a *heartbeat* to 224.0.0.233:8708 every second.
//!
//! ```text
//!  0      2      4      6      8              16             24             32
//!  +------+------+------+------+--------------+--------------+--------------+----
//!  |start | len  | seq  | proc | device id    | vendor id    | opcode       | content
//!  +------+------+------+------+--------------+--------------+--------------+----
//! ```
//!
//! Status opcodes have the form `07 2a HH LL 00 00 00 00` where `HHLL` is a
//! notification id (see [`notification`]); requests use the same form with
//! the "query" id, which is the notification id plus one for most messages.

use std::net::Ipv4Addr;

use crate::wire::{Reader, Writer, put_fixed_str};
use crate::{DeviceId, Error, Result};

pub const HEADER_LEN: usize = 32;
/// Port devices listen on for info/settings requests.
pub const SETTINGS_PORT: u16 = 8700;
/// Multicast group and port of status notifications.
pub const INFO_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 231);
pub const INFO_PORT: u16 = 8702;
/// Multicast group and port of heartbeats.
pub const HEARTBEAT_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 233);
pub const HEARTBEAT_PORT: u16 = 8708;

/// Start code of status notifications and requests.
pub const START_INFO: u16 = 0xffff;
/// Start code of heartbeats.
pub const START_HEARTBEAT: u16 = 0xfffe;

/// Value of the vendor-id header field.
///
/// This is a protocol identifier that Dante peers check before accepting a
/// message; using it is required for interoperability and implies no
/// endorsement by Audinate.
pub const VENDOR_ID: [u8; 8] = *b"Audinate";

/// Opcode of heartbeat messages.
pub const HEARTBEAT_OPCODE: [u8; 8] = [0x00, 0x08, 0x00, 0x01, 0x10, 0x00, 0x00, 0x00];

/// Notification ids (bytes 2..4 of status opcodes).
pub mod notification {
    pub const INTERFACE_STATUS: u16 = 0x0011;
    pub const INTERFACE_QUERY: u16 = 0x0013;
    pub const CLOCKING_STATUS: u16 = 0x0020;
    pub const CLOCKING_QUERY: u16 = 0x0021;
    pub const VERSIONS_STATUS: u16 = 0x0060;
    pub const VERSIONS_QUERY: u16 = 0x0061;
    pub const CLEAR_CONFIG_QUERY: u16 = 0x0077;
    pub const CLEAR_CONFIG_STATUS: u16 = 0x0078;
    pub const SAMPLE_RATE_STATUS: u16 = 0x0080;
    pub const SAMPLE_RATE_QUERY: u16 = 0x0081;
    pub const ENCODING_STATUS: u16 = 0x0082;
    pub const ENCODING_QUERY: u16 = 0x0083;
    pub const MANUFACTURER_VERSIONS_STATUS: u16 = 0x00c0;
    pub const MANUFACTURER_VERSIONS_QUERY: u16 = 0x00c1;
    pub const ROUTING_READY: u16 = 0x0100;
    pub const TX_CHANNEL_CHANGE: u16 = 0x0101;
    pub const RX_CHANNEL_CHANGE: u16 = 0x0102;
    pub const TX_LABEL_CHANGE: u16 = 0x0103;
    pub const TX_FLOW_CHANGE: u16 = 0x0104;
    pub const RX_FLOW_CHANGE: u16 = 0x0105;
}

/// The status opcode for notification `id`.
pub fn status_opcode(id: u16) -> [u8; 8] {
    let [hi, lo] = id.to_be_bytes();
    [0x07, 0x2a, hi, lo, 0, 0, 0, 0]
}

/// Extracts the notification/query id from an opcode of the `07 xx HH LL`
/// form.
pub fn opcode_id(opcode: &[u8; 8]) -> Option<u16> {
    (opcode[0] == 0x07).then(|| u16::from_be_bytes([opcode[2], opcode[3]]))
}

/// The 32-byte message header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConmonHeader {
    pub start_code: u16,
    pub seq: u16,
    pub process_id: u16,
    pub device_id: DeviceId,
    pub vendor: [u8; 8],
    pub opcode: [u8; 8],
}

impl ConmonHeader {
    pub fn encode(&self, content: &[u8]) -> Vec<u8> {
        let mut w = Writer::with_capacity(HEADER_LEN + content.len());
        w.u16(self.start_code);
        w.u16((HEADER_LEN + content.len()) as u16);
        w.u16(self.seq);
        w.u16(self.process_id);
        w.bytes(&self.device_id);
        w.bytes(&self.vendor);
        w.bytes(&self.opcode);
        w.bytes(content);
        w.into_vec()
    }

    /// Parses a message into its header and content.
    pub fn decode(packet: &[u8]) -> Result<(Self, &[u8])> {
        let mut r = Reader::new(packet);
        let start_code = r.u16()?;
        let len = r.u16()? as usize;
        let header = Self {
            start_code,
            seq: r.u16()?,
            process_id: r.u16()?,
            device_id: r.array()?,
            vendor: r.array()?,
            opcode: r.array()?,
        };
        if len < HEADER_LEN || len > packet.len() {
            return Err(Error::Invalid("conmon length field"));
        }
        Ok((header, &packet[HEADER_LEN..len]))
    }
}

/// Capability bits of [`versions_status`] (the "primary capabilities").
pub mod capability {
    /// The sample rate can be set with `SAMPLE_RATE_QUERY`.
    pub const SAMPLE_RATE: u32 = 0x0000_0008;
    /// The encoding can be set with `ENCODING_QUERY`.
    pub const ENCODING: u32 = 0x0000_0010;
    /// The device has a manufacturer name (`MANUFACTURER_VERSIONS_STATUS`).
    pub const MANUFACTURER_NAME: u32 = 0x0000_1000;
}

/// Length of the name fields of [`versions_status`] and
/// [`manufacturer_versions_status`], NUL included.
const NAME_FIELD: usize = 128;

/// Firmware/hardware versions and capability flags (`VERSIONS_STATUS`).
///
/// Several bytes are copied from observed devices; in particular the
/// monitoring capabilities (byte `0xbb`) must be non-zero or Dante
/// Controller re-requests this message every second.
pub fn versions_status(board_name: &str, capabilities: u32) -> Vec<u8> {
    let mut c = vec![0u8; 200];
    c[0..4].copy_from_slice(&[4, 1, 0, 6]); // firmware version
    c[4..8].copy_from_slice(&[4, 1, 0, 3]); // hardware version
    put_fixed_str(&mut c, 12, 8, board_name);
    c[0x14..0x18].copy_from_slice(&capabilities.to_be_bytes());
    c[0x23] = 2;
    c[0x27] = 1;
    c[0x28..0x2c].copy_from_slice(&[1, 0, 0, 0]); // boot version
    put_name(&mut c, 0x38, board_name);
    c[0xbb] = 0x1f;
    c
}

/// Manufacturer, model and software version (`MANUFACTURER_VERSIONS_STATUS`).
pub fn manufacturer_versions_status(
    manufacturer: &str,
    board_name: &str,
    model_name: &str,
    version: [u8; 4],
) -> Vec<u8> {
    let mut c = vec![0u8; 336];
    put_fixed_str(&mut c, 0, 8, manufacturer);
    put_fixed_str(&mut c, 8, 8, board_name);
    c[0x1c..0x20].copy_from_slice(&version);
    put_name(&mut c, 0x2c, manufacturer);
    put_name(&mut c, 0xac, model_name);
    c
}

/// Writes `s` into a [`NAME_FIELD`] at `offset`, always NUL-terminated.
fn put_name(c: &mut [u8], offset: usize, s: &str) {
    put_fixed_str(c, offset, NAME_FIELD - 1, s);
    c[offset + NAME_FIELD - 1] = 0;
}

/// Network interface parameters (`INTERFACE_STATUS`).
pub fn interface_status(
    link_speed_mbps: u16,
    mac: [u8; 6],
    ip: Ipv4Addr,
    netmask: Ipv4Addr,
    gateway: Ipv4Addr,
) -> Vec<u8> {
    let mut w = Writer::with_capacity(64);
    w.bytes(&[0x00, 0x01, 0x00, 0x00, 0x00, 0x00]);
    w.u16(link_speed_mbps);
    w.u16(1);
    w.bytes(&mac);
    w.bytes(&ip.octets());
    w.bytes(&netmask.octets());
    w.bytes(&gateway.octets());
    w.bytes(&gateway.octets()); // DNS server; not used by peers
    w.bytes(&[0x00, 0x18, 0x00, 0x30]);
    w.zeros(28);
    w.into_vec()
}

/// The clock's state in [`clocking_status`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sync {
    /// Following a master, synchronised: Dante Controller shows "Locked".
    Locked,
    /// Following a master, not synchronised yet.
    Locking,
    /// No master to follow.
    Unlocked,
    /// Running on a local clock, following nothing.
    FreeRunning,
}

impl Sync {
    /// The clock state (0 none, 1 passive, 2 undisciplined, 3 disciplined)
    /// and the servo state (0 faulty, 1 reset, 2 synchronising,
    /// 3 synchronised, 4 unknown, 5 delay reset, 6 none).
    fn codes(self) -> (u16, u16) {
        match self {
            Sync::Locked => (3, 3),
            Sync::Locking => (2, 2),
            Sync::Unlocked => (2, 1),
            Sync::FreeRunning => (2, 6),
        }
    }
}

/// Clock synchronisation state (`CLOCKING_STATUS`).
///
/// `master_id` is the EUI-64 of the PTP master (see [`eui64_from_mac`]),
/// all zero without one.
pub fn clocking_status(
    sync: Sync,
    freq_offset_ppb: i32,
    mac: [u8; 6],
    master_id: [u8; 8],
) -> Vec<u8> {
    let (clock, servo) = sync.codes();
    let mut w = Writer::with_capacity(128);
    w.u16(clock);
    w.u16(servo);
    w.bytes(&[0x00, 0x00, 0x00, 0x9f]);
    w.i32(freq_offset_ppb);
    w.bytes(&mac);
    w.u16(0);
    w.bytes(&master_id);
    w.bytes(&master_id);
    w.zeros(76);
    w.into_vec()
}

/// A setting with a list of allowed values, such as the sample rate
/// (`SAMPLE_RATE_STATUS`) or the encoding (`ENCODING_STATUS`): the current
/// value, the one waiting to apply (0 for none), whether controllers may
/// change it, and the allowed values.
pub fn configurable_status(current: u32, pending: u32, writable: bool, values: &[u32]) -> Vec<u8> {
    let mut w = Writer::with_capacity(16 + 4 * values.len());
    // The values start 0x18 bytes into the record (8 header bytes before the
    // content).
    w.u16(0x18);
    w.u16(values.len() as u16);
    w.u32(current);
    w.u32(pending);
    w.u16(if writable { 2 } else { 0 });
    w.u16(0);
    for &v in values {
        w.u32(v);
    }
    w.into_vec()
}

/// A controller's `SAMPLE_RATE_QUERY` or `ENCODING_QUERY`: `Some(value)` to
/// set it, `None` to only ask for the status.
pub fn decode_configurable_request(content: &[u8]) -> Option<u32> {
    let mut r = Reader::new(content);
    let mode = r.u32().ok()?;
    let value = r.u32().ok()?;
    (mode == 1 && value != 0).then_some(value)
}

/// Reply to `CLEAR_CONFIG_QUERY`.
pub fn clear_config_status() -> Vec<u8> {
    vec![0, 0, 0, 3, 0, 0, 0, 0]
}

/// Notification that the given 0-based channels changed (e.g. were renamed).
pub fn channel_change(channel_indices: impl IntoIterator<Item = usize>) -> Vec<u8> {
    let mut mask: Vec<u8> = vec![0];
    for ch in channel_indices {
        let byte = ch / 8;
        if byte >= mask.len() {
            mask.resize(byte + 1, 0);
        }
        mask[byte] |= 1 << (ch % 8);
    }
    let mut c = (mask.len() as u16).to_be_bytes().to_vec();
    c.extend_from_slice(&mask);
    c
}

/// Builder for heartbeat content: a sequence of typed blocks.
#[derive(Clone, Debug, Default)]
pub struct Heartbeat {
    w: Writer,
}

impl Heartbeat {
    pub fn new() -> Self {
        Self::default()
    }

    /// Block `0x8001`: clock frequency offset in ppb. Only present while the
    /// device has a clock. Shaped as a Dante AVIO sends it: a 16-byte
    /// payload, the offset first and the rest zero.
    pub fn clock(mut self, seq: u16, freq_offset_ppb: i32) -> Self {
        let w = &mut self.w;
        w.u16(28);
        w.u16(0x8001);
        w.u16(4);
        w.u16(16);
        w.u16(seq);
        w.u16(0);
        w.i32(freq_offset_ppb);
        w.zeros(12);
        self
    }

    /// Block `0x8000`: each network interface's traffic over the last
    /// second, as a Dante AVIO sends it.
    pub fn interface_traffic(mut self, seq: u16, interfaces: &[InterfaceTraffic]) -> Self {
        let n = interfaces.len() as u16;
        let w = &mut self.w;
        w.u16(20 + 16 * n);
        w.u16(0x8000);
        w.u16(4);
        w.u16(4);
        w.u16(seq);
        w.u16(0);
        w.u16(16);
        w.u16(0);
        w.u16(n);
        w.u16(16); // bytes per entry
        for i in interfaces {
            w.u32(i.tx_bytes_per_s);
            w.u32(i.rx_bytes_per_s);
            w.u32(i.tx_errors);
            w.u32(i.rx_errors);
        }
        self
    }

    /// Block `0x8002`: the peak level of each transmit and receive channel,
    /// one byte each (see [`level_byte`]).
    pub fn levels(mut self, seq: u16, tx: &[u8], rx: &[u8]) -> Self {
        let payload = (12 + tx.len() + rx.len() + 3) & !3;
        let w = &mut self.w;
        w.u16((12 + payload) as u16);
        w.u16(0x8002);
        w.u16(4);
        w.u16(payload as u16);
        w.u16(seq);
        w.u16(0);
        w.u16(tx.len() as u16);
        w.u16(0);
        w.u16(rx.len() as u16);
        w.u16(0);
        w.u16(24); // the levels' offset in the block
        w.u16(0);
        w.bytes(tx);
        w.bytes(rx);
        w.zeros(payload - 12 - tx.len() - rx.len());
        self
    }

    /// Block `0x8003`: the longest a packet of each receive flow took to
    /// arrive since the last heartbeat, in samples. Real receivers send one
    /// entry per receive flow they could have (per network interface), 0
    /// for those without packets.
    pub fn rx_latency(mut self, seq: u16, sample_rate: u32, latencies: &[u32]) -> Self {
        let n = latencies.len() as u16;
        let w = &mut self.w;
        w.u16(24 + 4 * n);
        w.u16(0x8003);
        w.u16(4);
        w.u16(12 + 4 * n);
        w.u16(seq);
        w.u16(0);
        w.u16(n);
        w.u16(0);
        w.u16(24);
        w.u16(0);
        w.u32(sample_rate);
        for &l in latencies {
            w.u32(l);
        }
        self
    }

    /// Block `0x8004`: the late packets each receive flow has counted so
    /// far, indexed like [`Heartbeat::rx_latency`].
    pub fn late_packets(mut self, seq: u16, counts: &[u32]) -> Self {
        let n = counts.len() as u16;
        let w = &mut self.w;
        w.u16(20 + 4 * n);
        w.u16(0x8004);
        w.u16(4);
        w.u16(8 + 4 * n);
        w.u16(seq);
        w.u16(0);
        w.u16(n);
        w.u16(0);
        w.u16(20);
        w.u16(0);
        for &c in counts {
            w.u32(c);
        }
        self
    }

    pub fn into_content(self) -> Vec<u8> {
        self.w.into_vec()
    }
}

/// One network interface's traffic for [`Heartbeat::interface_traffic`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InterfaceTraffic {
    pub tx_bytes_per_s: u32,
    pub rx_bytes_per_s: u32,
    /// Errors since the device started.
    pub tx_errors: u32,
    pub rx_errors: u32,
}

/// A peak level for [`Heartbeat::levels`]: attenuation from full scale in
/// 0.5 dB steps, 0 at full scale and 255 for silence. `peak` is the
/// largest magnitude of 32-bit samples.
pub fn level_byte(peak: u32) -> u8 {
    if peak == 0 {
        return 0xff;
    }
    let db = 20.0 * (f64::from(peak) / 2_147_483_648.0).log10();
    (-2.0 * db).round().clamp(0.0, 255.0) as u8
}

/// Maps a 48-bit MAC address (or PTPv1 UUID) to the EUI-64 form Dante uses
/// for device and clock ids.
pub fn eui64_from_mac(mac: [u8; 6]) -> [u8; 8] {
    [mac[0], mac[1], mac[2], 0xff, 0xfe, mac[3], mac[4], mac[5]]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(opcode: [u8; 8]) -> ConmonHeader {
        ConmonHeader {
            start_code: START_INFO,
            seq: 9,
            process_id: 2,
            device_id: [0, 0, 192, 168, 1, 5, 0, 2],
            vendor: VENDOR_ID,
            opcode,
        }
    }

    #[test]
    fn header_round_trip() {
        let h = header(status_opcode(notification::VERSIONS_STATUS));
        let content = versions_status("OpenVirtualSoundcard", capability::MANUFACTURER_NAME);
        let pkt = h.encode(&content);
        assert_eq!(pkt.len(), 232);
        assert_eq!(&pkt[24..32], &[0x07, 0x2a, 0x00, 0x60, 0, 0, 0, 0]);
        let (h2, c2) = ConmonHeader::decode(&pkt).unwrap();
        assert_eq!(h2, h);
        assert_eq!(c2, &content[..]);
        assert_eq!(opcode_id(&h2.opcode), Some(notification::VERSIONS_STATUS));
        assert_eq!(c2[0xbb], 0x1f);
        assert_eq!(&c2[12..20], b"OpenVirt");
        assert_eq!(&c2[0x14..0x18], &[0, 0, 0x10, 0]);
    }

    #[test]
    fn names_are_not_cut_short() {
        let model = "Open Virtual Soundcard";
        let c = manufacturer_versions_status(
            "OpenVirtualSoundcard",
            "OpenVirtualSoundcard",
            model,
            [0, 1, 0, 0],
        );
        assert_eq!(c.len(), 336);
        assert_eq!(&c[0xac..0xac + model.len()], model.as_bytes());
        assert_eq!(c[0xac + model.len()], 0);
        let long = "x".repeat(200);
        let c = manufacturer_versions_status(&long, "b", &long, [0; 4]);
        assert_eq!(c[0x2c + 127], 0);
        assert_eq!(c[0xac + 127], 0);
        let c = versions_status(model, 0);
        assert_eq!(&c[0x38..0x38 + model.len()], model.as_bytes());
    }

    #[test]
    fn configurable_settings() {
        // As lx-dante reports its sample rate.
        let c = configurable_status(44_100, 0, true, &[44_100, 48_000]);
        assert_eq!(
            c,
            [
                0, 0x18, 0, 2, 0, 0, 0xac, 0x44, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0xac, 0x44, 0, 0,
                0xbb, 0x80
            ]
        );
        assert_eq!(decode_configurable_request(&[0, 0, 0, 0, 0, 0, 0, 0]), None);
        assert_eq!(decode_configurable_request(&[0, 0, 0, 1, 0, 0, 0xbb, 0x80]), Some(48_000));
        assert_eq!(decode_configurable_request(&[0, 0, 0, 1]), None);
    }

    #[test]
    fn request_ids() {
        assert_eq!(opcode_id(&[0x07, 0x38, 0x00, 0x21, 0, 0, 0, 0x64]), Some(0x21));
        assert_eq!(opcode_id(&HEARTBEAT_OPCODE), None);
    }

    #[test]
    fn channel_change_mask() {
        assert_eq!(channel_change([]), vec![0, 1, 0]);
        assert_eq!(channel_change([0, 9]), vec![0, 2, 0b1, 0b10]);
    }

    #[test]
    fn heartbeat_blocks() {
        let c = Heartbeat::new().clock(7, -1234).rx_latency(7, 48_000, &[48, 96]).into_content();
        assert_eq!(&c[0..4], &[0, 28, 0x80, 0x01]);
        assert_eq!(i32::from_be_bytes(c[12..16].try_into().unwrap()), -1234);
        assert_eq!(&c[28..32], &[0, 32, 0x80, 0x03]);
        assert_eq!(c.len(), 28 + 32);
    }

    /// The records of a Dante AVIO-DAI2's heartbeat (captured), rebuilt.
    #[test]
    fn heartbeat_blocks_match_an_avio() {
        let c = Heartbeat::new()
            .clock(0x2ac4, 0)
            .interface_traffic(
                0x2ac4,
                &[InterfaceTraffic {
                    tx_bytes_per_s: 0x6f3cf,
                    rx_bytes_per_s: 0x7f4,
                    ..Default::default()
                }],
            )
            .levels(0x2ac4, &[0x10, 0x15], &[])
            .into_content();
        let avio = "001c800100040010 2ac4000000000000 0000000000000000 00000000 \
                    0024800000040004 2ac4000000100000 0001001000 06f3cf000007f4 0000000000000000 \
                    001c800200040010 2ac4000000020000 0000000000180000 10150000";
        let avio: String = avio.chars().filter(char::is_ascii_hexdigit).collect();
        assert_eq!(hex::encode(&c), avio);
    }

    /// A real Dante receiver's latency and late-packet records (from
    /// netaudio's fixture `heartbeat_connection_health/sequence-41132`),
    /// rebuilt.
    #[test]
    fn receiver_records_match_a_real_receiver() {
        let c = Heartbeat::new()
            .rx_latency(0xa0ac, 48_000, &[1006, 0])
            .late_packets(0xa0ac, &[825, 0])
            .into_content();
        let real = "00208003000400 14a0ac000000020000 001800000000bb80 000003ee00000000 \
                    001c800400040010 a0ac000000020000 0014000000000339 00000000";
        let real: String = real.chars().filter(char::is_ascii_hexdigit).collect();
        assert_eq!(hex::encode(&c), real);
    }

    #[test]
    fn levels_are_half_decibel_steps_below_full_scale() {
        assert_eq!(level_byte(0), 0xff);
        assert_eq!(level_byte(i32::MAX as u32), 0);
        // −6.02 dB is 12 half-decibel steps.
        assert_eq!(level_byte(1 << 30), 12);
        assert_eq!(level_byte(1), 0xff);
        // Lengths stay multiples of four, padded after the levels.
        let c = Heartbeat::new().levels(1, &[1, 2, 3], &[4, 5]).into_content();
        assert_eq!(&c[0..8], &[0, 32, 0x80, 0x02, 0, 4, 0, 20]);
        assert_eq!(&c[24..], &[1, 2, 3, 4, 5, 0, 0, 0]);
    }

    #[test]
    fn interface_and_clock_status_sizes() {
        let mac = [0x02, 0, 0, 0xaa, 0xbb, 0xcc];
        let ip = Ipv4Addr::new(10, 0, 0, 2);
        let mask = Ipv4Addr::new(255, 255, 255, 0);
        assert_eq!(interface_status(1000, mac, ip, mask, ip).len(), 64);
        let c = clocking_status(Sync::Locked, 5, mac, eui64_from_mac(mac));
        assert_eq!(&c[0..4], &[0, 3, 0, 3]);
        assert_eq!(&c[12..18], &mac);
        assert_eq!(&c[20..28], &[0x02, 0, 0, 0xff, 0xfe, 0xaa, 0xbb, 0xcc]);
        assert_eq!(c.len(), 112);
        let c = clocking_status(Sync::Locking, 5, mac, [0; 8]);
        assert_eq!(&c[0..4], &[0, 2, 0, 2]);
    }
}
