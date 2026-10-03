//! Media (audio) packets.
//!
//! ```text
//!  0     1               5               9
//!  +-----+---------------+---------------+------------------------------
//!  | ver | seconds (u32) | subsec (u32)  | frame 0: ch0 ch1 … | frame 1 …
//!  +-----+---------------+---------------+------------------------------
//! ```
//!
//! The timestamp is the media-clock position of the first frame: whole PTP
//! seconds plus the sample index within that second. Samples are big-endian
//! signed PCM (16, 24 or 32 bit), interleaved frame by frame. Receivers play
//! the frame stamped `t` at media time `t + latency`.
//!
//! Inside OpenVirtualSoundcard samples are `i32` with the audio left-justified (a 24-bit
//! sample occupies the top 24 bits), so formats convert by byte truncation.

use crate::{Error, Result};

/// Length of the media packet header.
pub const HEADER_LEN: usize = 9;
/// Value of the first header byte observed on the wire.
pub const VERSION: u8 = 2;
/// Destination port used for multicast flows.
pub const MULTICAST_PORT: u16 = 4321;
/// Conservative limit for the audio payload of one packet.
pub const MAX_PAYLOAD: usize = 1400;
/// Payload receivers send back to a unicast transmitter to keep a flow alive.
/// Transmitters treat any datagram from the receiver as a keepalive.
pub const KEEPALIVE: [u8; 2] = [0x13, 0x37];

/// A left-justified signed 32-bit sample.
pub type Sample = i32;

/// On-the-wire sample encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SampleFormat {
    S16,
    S24,
    S32,
}

impl SampleFormat {
    pub fn from_bits(bits: u32) -> Option<Self> {
        match bits {
            16 => Some(Self::S16),
            24 => Some(Self::S24),
            32 => Some(Self::S32),
            _ => None,
        }
    }

    pub fn bits(self) -> u16 {
        self.bytes() as u16 * 8
    }

    pub fn bytes(self) -> usize {
        match self {
            Self::S16 => 2,
            Self::S24 => 3,
            Self::S32 => 4,
        }
    }

    #[inline]
    pub fn write(self, out: &mut [u8], sample: Sample) {
        let n = self.bytes();
        out[..n].copy_from_slice(&sample.to_be_bytes()[..n]);
    }

    #[inline]
    pub fn read(self, input: &[u8]) -> Sample {
        let mut b = [0u8; 4];
        let n = self.bytes();
        b[..n].copy_from_slice(&input[..n]);
        i32::from_be_bytes(b)
    }
}

/// Splits a media-clock sample index into the wire timestamp fields.
#[inline]
pub fn split_timestamp(samples: u64, sample_rate: u32) -> (u32, u32) {
    let rate = sample_rate as u64;
    ((samples / rate) as u32, (samples % rate) as u32)
}

/// Joins wire timestamp fields into a media-clock sample index.
#[inline]
pub fn join_timestamp(seconds: u32, subsec: u32, sample_rate: u32) -> u64 {
    seconds as u64 * sample_rate as u64 + subsec as u64
}

/// The largest frames-per-packet value that keeps a packet within
/// [`MAX_PAYLOAD`].
pub fn max_fpp(channels: usize, format: SampleFormat) -> usize {
    MAX_PAYLOAD / (channels.max(1) * format.bytes())
}

/// Encodes a media packet into `out` and returns its length.
///
/// `sample(frame, channel)` supplies each sample. `out` must hold at least
/// `HEADER_LEN + frames * channels * format.bytes()` bytes.
pub fn encode_packet(
    out: &mut [u8],
    timestamp: u64,
    sample_rate: u32,
    format: SampleFormat,
    channels: usize,
    frames: usize,
    mut sample: impl FnMut(usize, usize) -> Sample,
) -> usize {
    let (sec, sub) = split_timestamp(timestamp, sample_rate);
    out[0] = VERSION;
    out[1..5].copy_from_slice(&sec.to_be_bytes());
    out[5..9].copy_from_slice(&sub.to_be_bytes());
    let width = format.bytes();
    let mut pos = HEADER_LEN;
    for frame in 0..frames {
        for ch in 0..channels {
            format.write(&mut out[pos..pos + width], sample(frame, ch));
            pos += width;
        }
    }
    pos
}

/// A parsed media packet.
#[derive(Clone, Copy, Debug)]
pub struct AudioPacket<'a> {
    pub seconds: u32,
    pub subsec: u32,
    data: &'a [u8],
    channels: usize,
    format: SampleFormat,
}

impl<'a> AudioPacket<'a> {
    /// Parses a packet carrying `channels` channels in `format`. Trailing
    /// bytes that don't form a whole frame are ignored.
    pub fn parse(packet: &'a [u8], channels: usize, format: SampleFormat) -> Result<Self> {
        if packet.len() < HEADER_LEN {
            return Err(Error::Truncated { offset: 0, needed: HEADER_LEN, len: packet.len() });
        }
        if channels == 0 {
            return Err(Error::Invalid("zero channels"));
        }
        Ok(Self {
            seconds: u32::from_be_bytes(packet[1..5].try_into().expect("checked")),
            subsec: u32::from_be_bytes(packet[5..9].try_into().expect("checked")),
            data: &packet[HEADER_LEN..],
            channels,
            format,
        })
    }

    /// Media-clock sample index of the first frame.
    pub fn timestamp(&self, sample_rate: u32) -> u64 {
        join_timestamp(self.seconds, self.subsec, sample_rate)
    }

    pub fn frames(&self) -> usize {
        self.data.len() / (self.channels * self.format.bytes())
    }

    #[inline]
    pub fn sample(&self, frame: usize, channel: usize) -> Sample {
        let width = self.format.bytes();
        let at = (frame * self.channels + channel) * width;
        self.format.read(&self.data[at..at + width])
    }

    /// Copies one channel's samples into `out` (up to `out.len()` frames) and
    /// returns how many were copied.
    pub fn read_channel(&self, channel: usize, out: &mut [Sample]) -> usize {
        let n = self.frames().min(out.len());
        for (frame, slot) in out[..n].iter_mut().enumerate() {
            *slot = self.sample(frame, channel);
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_formats_truncate_and_extend() {
        let s: Sample = 0x1234_5678;
        let mut buf = [0u8; 4];
        SampleFormat::S24.write(&mut buf, s);
        assert_eq!(&buf[..3], &[0x12, 0x34, 0x56]);
        assert_eq!(SampleFormat::S24.read(&buf), 0x1234_5600);
        SampleFormat::S16.write(&mut buf, -1);
        assert_eq!(SampleFormat::S16.read(&buf), -65536);
        SampleFormat::S32.write(&mut buf, i32::MIN);
        assert_eq!(SampleFormat::S32.read(&buf), i32::MIN);
        assert_eq!(SampleFormat::from_bits(24), Some(SampleFormat::S24));
        assert_eq!(SampleFormat::from_bits(20), None);
    }

    #[test]
    fn timestamps() {
        let idx = 1_700_000_000u64 * 48_000 + 12_345;
        assert_eq!(split_timestamp(idx, 48_000), (1_700_000_000, 12_345));
        assert_eq!(join_timestamp(1_700_000_000, 12_345, 48_000), idx);
    }

    #[test]
    fn packet_round_trip() {
        let mut buf = [0u8; 1500];
        let channels = 3;
        let frames = 16;
        let ts = 96_000 * 10 + 7;
        let len =
            encode_packet(&mut buf, ts, 96_000, SampleFormat::S24, channels, frames, |f, c| {
                ((f * 10 + c) as i32) << 8
            });
        assert_eq!(len, HEADER_LEN + frames * channels * 3);
        assert_eq!(buf[0], VERSION);
        let p = AudioPacket::parse(&buf[..len], channels, SampleFormat::S24).unwrap();
        assert_eq!(p.timestamp(96_000), ts);
        assert_eq!(p.frames(), frames);
        assert_eq!(p.sample(5, 2), (52 << 8));
        let mut ch1 = [0; 32];
        assert_eq!(p.read_channel(1, &mut ch1), frames);
        assert_eq!(ch1[15], (151 << 8));
    }

    #[test]
    fn rejects_short_packets() {
        assert!(AudioPacket::parse(&[2, 0, 0], 2, SampleFormat::S16).is_err());
        assert_eq!(max_fpp(8, SampleFormat::S24), 58);
    }
}
