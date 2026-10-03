//! Byte-level helpers shared by all codecs.
//!
//! Dante packets are big-endian throughout. Many of them consist of a block of
//! fixed-size descriptors followed by a "heap" of NUL-terminated strings and
//! nested descriptors, referenced by `u16` offsets measured from the start of
//! the whole packet. [`Writer`] supports that pattern by handing out the
//! offset of everything it writes and allowing earlier fields to be patched.

use crate::{Error, Result};

/// Bounds-checked big-endian reader over a byte slice.
#[derive(Clone, Debug)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// A reader positioned at `pos`.
    pub fn at(buf: &'a [u8], pos: usize) -> Self {
        Self { buf, pos }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&end| end <= self.buf.len())
            .ok_or(Error::Truncated { offset: self.pos, needed: n, len: self.buf.len() })?;
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.bytes(N)?.try_into().expect("length checked"))
    }

    pub fn skip(&mut self, n: usize) -> Result<()> {
        self.bytes(n).map(|_| ())
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    pub fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_be_bytes(self.array()?))
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    pub fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_be_bytes(self.array()?))
    }
}

/// Reads a big-endian `u16` at `offset`.
pub fn u16_at(buf: &[u8], offset: usize) -> Result<u16> {
    Reader::at(buf, offset).u16()
}

/// Reads a big-endian `u32` at `offset`.
pub fn u32_at(buf: &[u8], offset: usize) -> Result<u32> {
    Reader::at(buf, offset).u32()
}

/// Reads a NUL-terminated string starting at `offset`.
///
/// Invalid UTF-8 is replaced rather than rejected: names typed on a device's
/// front panel are not guaranteed to be valid UTF-8 and should still show up.
pub fn cstr_at(buf: &[u8], offset: usize) -> Result<String> {
    let tail = buf.get(offset..).ok_or(Error::Truncated { offset, needed: 1, len: buf.len() })?;
    let end = tail.iter().position(|&b| b == 0).ok_or(Error::Invalid("unterminated string"))?;
    Ok(String::from_utf8_lossy(&tail[..end]).into_owned())
}

/// Like [`cstr_at`], but offset 0 means "no string".
pub fn opt_cstr_at(buf: &[u8], offset: u16) -> Result<Option<String>> {
    match offset {
        0 => Ok(None),
        off => cstr_at(buf, off as usize).map(Some),
    }
}

/// Growable big-endian writer. Every write method returns the offset at which
/// the value was written, which is what heap-style packets reference.
#[derive(Clone, Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self { buf: Vec::with_capacity(cap) }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Current length as a `u16` packet offset.
    ///
    /// # Panics
    /// If the packet already exceeds 64 KiB, which no valid packet does.
    pub fn offset(&self) -> u16 {
        u16::try_from(self.buf.len()).expect("packet larger than 64 KiB")
    }

    pub fn u8(&mut self, v: u8) -> u16 {
        self.bytes(&[v])
    }

    pub fn u16(&mut self, v: u16) -> u16 {
        self.bytes(&v.to_be_bytes())
    }

    pub fn i16(&mut self, v: i16) -> u16 {
        self.bytes(&v.to_be_bytes())
    }

    pub fn u32(&mut self, v: u32) -> u16 {
        self.bytes(&v.to_be_bytes())
    }

    pub fn i32(&mut self, v: i32) -> u16 {
        self.bytes(&v.to_be_bytes())
    }

    pub fn bytes(&mut self, v: &[u8]) -> u16 {
        let at = self.offset();
        self.buf.extend_from_slice(v);
        at
    }

    pub fn zeros(&mut self, n: usize) -> u16 {
        let at = self.offset();
        self.buf.resize(self.buf.len() + n, 0);
        at
    }

    /// Writes `s` followed by a NUL byte and returns its offset.
    pub fn cstr(&mut self, s: &str) -> u16 {
        let at = self.bytes(s.as_bytes());
        self.u8(0);
        at
    }

    /// Writes `s` as a NUL-terminated string, or returns 0 for `None`.
    pub fn opt_cstr(&mut self, s: Option<&str>) -> u16 {
        s.map_or(0, |s| self.cstr(s))
    }

    /// Writes `s` into a fixed-size, NUL-padded field (truncating if needed).
    pub fn fixed_str(&mut self, s: &str, len: usize) -> u16 {
        let at = self.offset();
        let bytes = s.as_bytes();
        let n = bytes.len().min(len);
        self.buf.extend_from_slice(&bytes[..n]);
        self.zeros(len - n);
        at
    }

    /// Pads with zeros until the length is a multiple of `alignment`.
    pub fn align(&mut self, alignment: usize) -> u16 {
        let rem = self.buf.len() % alignment;
        if rem != 0 {
            self.zeros(alignment - rem);
        }
        self.offset()
    }

    pub fn patch_u8(&mut self, at: usize, v: u8) {
        self.buf[at] = v;
    }

    pub fn patch_u16(&mut self, at: usize, v: u16) {
        self.buf[at..at + 2].copy_from_slice(&v.to_be_bytes());
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

/// Writes `s` into a fixed-size, NUL-padded field of `buf` at `offset`.
pub fn put_fixed_str(buf: &mut [u8], offset: usize, len: usize, s: &str) {
    let field = &mut buf[offset..offset + len];
    field.fill(0);
    let n = s.len().min(len);
    field[..n].copy_from_slice(&s.as_bytes()[..n]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_reads_big_endian_and_checks_bounds() {
        let buf = [0x12, 0x34, 0xde, 0xad, 0xbe, 0xef, 0xff];
        let mut r = Reader::new(&buf);
        assert_eq!(r.u16().unwrap(), 0x1234);
        assert_eq!(r.u32().unwrap(), 0xdead_beef);
        assert_eq!(r.remaining(), 1);
        assert_eq!(r.i16(), Err(Error::Truncated { offset: 6, needed: 2, len: 7 }));
        assert_eq!(r.u8().unwrap(), 0xff);
    }

    #[test]
    fn strings_round_trip_through_heap_offsets() {
        let mut w = Writer::new();
        w.zeros(4);
        let a = w.cstr("Left");
        let b = w.cstr("Right");
        w.patch_u16(0, a);
        w.patch_u16(2, b);
        let buf = w.into_vec();
        assert_eq!(cstr_at(&buf, u16_at(&buf, 0).unwrap() as usize).unwrap(), "Left");
        assert_eq!(cstr_at(&buf, u16_at(&buf, 2).unwrap() as usize).unwrap(), "Right");
        assert_eq!(opt_cstr_at(&buf, 0).unwrap(), None);
    }

    #[test]
    fn unterminated_string_is_an_error() {
        assert_eq!(cstr_at(b"abc", 0), Err(Error::Invalid("unterminated string")));
        assert!(cstr_at(b"abc\0", 9).is_err());
    }

    #[test]
    fn align_and_fixed_strings() {
        let mut w = Writer::new();
        w.u8(1);
        assert_eq!(w.align(4), 4);
        assert_eq!(w.align(4), 4);
        w.fixed_str("OpenVirtualSoundcardDevice", 8);
        assert_eq!(&w.as_slice()[4..], b"OpenVirt");
        let mut buf = [0xffu8; 6];
        put_fixed_str(&mut buf, 1, 4, "ab");
        assert_eq!(buf, [0xff, b'a', b'b', 0, 0, 0xff]);
    }
}
