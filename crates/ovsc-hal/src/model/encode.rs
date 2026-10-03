//! Property values and their bytes (design section 9): what each value
//! occupies, how it is written into the HAL's buffer, and how set data is
//! read back.
//!
//! A value is either fixed-size, which must fit the buffer whole or the call
//! fails with `!siz`, or a list, which is truncated to the whole elements
//! that fit. A CFString is created (+1) only once its size check has passed,
//! so a failed call never leaks one.

use std::mem::size_of;

use crate::abi::*;
use crate::platform::Platform;

/// The value of one property.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Value<'a> {
    /// A UInt32: also class IDs, object IDs, Booleans and four-character
    /// codes.
    U32(u32),
    /// A Float64.
    F64(f64),
    /// A CFString, created for the caller, who releases it.
    Str(&'a str),
    /// Two UInt32s, as PreferredChannelsForStereo has.
    Pair([u32; 2]),
    /// An AudioStreamBasicDescription.
    Format(AudioStreamBasicDescription),
    /// An AudioChannelLayout of this many discrete channels, described one
    /// by one: channel `i` (from 0) is labelled `Discrete_0 | i`.
    Layout(u32),
    /// A list of object IDs.
    Ids(&'static [AudioObjectID]),
    /// A list of one sample-rate range.
    Range(AudioValueRange),
    /// A list of one format with its sample-rate range.
    RangedFormat(AudioStreamRangedDescription),
    /// A list of custom property descriptions.
    Custom(&'static [AudioServerPlugInCustomPropertyInfo]),
}

const U32: u32 = size_of::<u32>() as u32;
const F64: u32 = size_of::<f64>() as u32;
const PTR: u32 = size_of::<CFStringRef>() as u32;
const ASBD: u32 = size_of::<AudioStreamBasicDescription>() as u32;
const RANGE: u32 = size_of::<AudioValueRange>() as u32;
const RANGED: u32 = size_of::<AudioStreamRangedDescription>() as u32;
const CUSTOM: u32 = size_of::<AudioServerPlugInCustomPropertyInfo>() as u32;
const LAYOUT_HEADER: u32 = size_of::<AudioChannelLayoutHeader>() as u32;
const CHANNEL: u32 = size_of::<AudioChannelDescription>() as u32;

impl Value<'_> {
    /// For a list, the size of one element; `None` for a fixed-size value.
    pub(crate) fn element_size(&self) -> Option<u32> {
        match self {
            Value::Ids(_) => Some(U32),
            Value::Range(_) => Some(RANGE),
            Value::RangedFormat(_) => Some(RANGED),
            Value::Custom(_) => Some(CUSTOM),
            _ => None,
        }
    }

    /// The whole value's size in bytes.
    pub(crate) fn size(&self) -> u32 {
        match self {
            Value::U32(_) => U32,
            Value::F64(_) => F64,
            Value::Str(_) => PTR,
            Value::Pair(_) => 2 * U32,
            Value::Format(_) => ASBD,
            Value::Layout(n) => LAYOUT_HEADER.saturating_add(n.saturating_mul(CHANNEL)),
            Value::Ids(ids) => U32 * ids.len() as u32,
            Value::Range(_) => RANGE,
            Value::RangedFormat(_) => RANGED,
            Value::Custom(infos) => CUSTOM * infos.len() as u32,
        }
    }
}

/// Writes `v` into `out` and returns the number of bytes written: the whole
/// value, or for a list the whole elements that fit. A fixed-size value that
/// does not fit is `!siz`.
pub(crate) fn write(v: &Value<'_>, out: &mut [u8], p: &dyn Platform) -> Result<u32, OSStatus> {
    let capacity = u32::try_from(out.len()).unwrap_or(u32::MAX);
    let size = v.size();
    // Elements of a list to write; for a fixed-size value, whether it fits.
    let count = match v.element_size() {
        Some(element) => capacity.min(size) / element,
        None if capacity >= size => 1,
        None => return Err(kAudioHardwareBadPropertySizeError),
    };
    let mut w = Writer { buf: out, pos: 0 };
    match v {
        Value::U32(x) => w.u32(*x),
        Value::F64(x) => w.f64(*x),
        Value::Str(s) => {
            let cf = p.cfstring_create(s);
            if cf.is_null() {
                return Err(kAudioHardwareUnspecifiedError);
            }
            w.ptr(cf);
        }
        Value::Pair(pair) => pair.iter().for_each(|&x| w.u32(x)),
        Value::Format(f) => w.format(f),
        Value::Layout(n) => {
            w.u32(kAudioChannelLayoutTag_UseChannelDescriptions);
            w.u32(0); // mChannelBitmap
            w.u32(*n);
            for i in 0..*n {
                w.u32(kAudioChannelLabel_Discrete_0 | i);
                w.u32(0); // mChannelFlags
                (0..3).for_each(|_| w.f32(0.0)); // mCoordinates
            }
        }
        Value::Ids(ids) => ids.iter().take(count as usize).for_each(|&id| w.u32(id)),
        Value::Range(r) => {
            if count > 0 {
                w.range(r);
            }
        }
        Value::RangedFormat(rf) => {
            if count > 0 {
                w.format(&rf.mFormat);
                w.range(&rf.mSampleRateRange);
            }
        }
        Value::Custom(infos) => infos.iter().take(count as usize).for_each(|info| {
            w.u32(info.mSelector);
            w.u32(info.mPropertyDataType);
            w.u32(info.mQualifierDataType);
        }),
    }
    Ok(w.pos as u32)
}

/// Sequential native-endian writes into a buffer already known to be large
/// enough; a write past the end is dropped rather than panicking.
struct Writer<'b> {
    buf: &'b mut [u8],
    pos: usize,
}

impl Writer<'_> {
    fn bytes(&mut self, b: &[u8]) {
        let end = self.pos + b.len();
        if let Some(dst) = self.buf.get_mut(self.pos..end) {
            dst.copy_from_slice(b);
            self.pos = end;
        }
    }

    fn u32(&mut self, x: u32) {
        self.bytes(&x.to_ne_bytes());
    }

    fn f32(&mut self, x: f32) {
        self.bytes(&x.to_ne_bytes());
    }

    fn f64(&mut self, x: f64) {
        self.bytes(&x.to_ne_bytes());
    }

    fn ptr(&mut self, p: CFStringRef) {
        const N: usize = size_of::<CFStringRef>();
        if let Some(dst) = self.buf.get_mut(self.pos..self.pos + N) {
            // SAFETY: `dst` is N writable bytes. Writing the pointer itself,
            // not its address as an integer, keeps it valid for the reader.
            unsafe { dst.as_mut_ptr().cast::<CFStringRef>().write_unaligned(p) };
            self.pos += N;
        }
    }

    fn format(&mut self, f: &AudioStreamBasicDescription) {
        self.f64(f.mSampleRate);
        for x in [
            f.mFormatID,
            f.mFormatFlags,
            f.mBytesPerPacket,
            f.mFramesPerPacket,
            f.mBytesPerFrame,
            f.mChannelsPerFrame,
            f.mBitsPerChannel,
            f.mReserved,
        ] {
            self.u32(x);
        }
    }

    fn range(&mut self, r: &AudioValueRange) {
        self.f64(r.mMinimum);
        self.f64(r.mMaximum);
    }
}

/// Set data holding exactly one UInt32.
pub(crate) fn read_u32(data: &[u8]) -> Option<u32> {
    Some(u32::from_ne_bytes(data.try_into().ok()?))
}

/// Set data holding exactly one Float64.
pub(crate) fn read_f64(data: &[u8]) -> Option<f64> {
    Some(f64::from_ne_bytes(data.try_into().ok()?))
}

/// Set data holding exactly one AudioStreamBasicDescription.
pub(crate) fn read_format(data: &[u8]) -> Option<AudioStreamBasicDescription> {
    if data.len() != ASBD as usize {
        return None;
    }
    let (rate, rest) = data.split_first_chunk::<8>()?;
    let mut words = rest.chunks_exact(4).map(|c| c.try_into().map_or(0, u32::from_ne_bytes));
    let mut next = || words.next().unwrap_or(0);
    Some(AudioStreamBasicDescription {
        mSampleRate: f64::from_ne_bytes(*rate),
        mFormatID: next(),
        mFormatFlags: next(),
        mBytesPerPacket: next(),
        mFramesPerPacket: next(),
        mBytesPerFrame: next(),
        mChannelsPerFrame: next(),
        mBitsPerChannel: next(),
        mReserved: next(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::stub::{self, StubPlatform};

    fn format() -> AudioStreamBasicDescription {
        AudioStreamBasicDescription {
            mSampleRate: 48_000.0,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagsNativeFloatPacked,
            mBytesPerPacket: 8,
            mFramesPerPacket: 1,
            mBytesPerFrame: 8,
            mChannelsPerFrame: 2,
            mBitsPerChannel: 32,
            mReserved: 0,
        }
    }

    /// The value written into a buffer of `capacity` bytes.
    fn written(v: Value<'_>, capacity: usize) -> Result<Vec<u8>, OSStatus> {
        let p = StubPlatform::new();
        let mut buf = vec![0xAA; capacity];
        let n = write(&v, &mut buf, &p)?;
        buf.truncate(n as usize);
        Ok(buf)
    }

    #[test]
    fn sizes_follow_the_sdk_structures() {
        assert_eq!(Value::U32(0).size(), 4);
        assert_eq!(Value::F64(0.0).size(), 8);
        assert_eq!(Value::Str("").size() as usize, size_of::<usize>());
        assert_eq!(Value::Pair([1, 2]).size(), 8);
        assert_eq!(Value::Format(format()).size(), 40);
        assert_eq!(Value::Layout(8).size(), 12 + 20 * 8);
        assert_eq!(Value::Ids(&[3, 4]).size(), 8);
        assert_eq!(Value::Range(AudioValueRange::default()).size(), 16);
        assert_eq!(Value::RangedFormat(AudioStreamRangedDescription::default()).size(), 56);
        const INFO: &[AudioServerPlugInCustomPropertyInfo] =
            &[AudioServerPlugInCustomPropertyInfo {
                mSelector: 1,
                mPropertyDataType: 2,
                mQualifierDataType: 3,
            }];
        assert_eq!(Value::Custom(INFO).size(), 12);
        assert_eq!(written(Value::Custom(INFO), 12).map(|b| b.len()), Ok(12));
        assert_eq!(written(Value::Custom(INFO), 11), Ok(vec![]));
    }

    #[test]
    fn fixed_values_must_fit_whole() {
        assert_eq!(written(Value::U32(7), 4), Ok(7u32.to_ne_bytes().to_vec()));
        assert_eq!(written(Value::U32(7), 64).map(|b| b.len()), Ok(4));
        assert_eq!(written(Value::U32(7), 3), Err(kAudioHardwareBadPropertySizeError));
        assert_eq!(written(Value::Pair([1, 2]), 7), Err(kAudioHardwareBadPropertySizeError));
        assert_eq!(written(Value::Layout(2), 51), Err(kAudioHardwareBadPropertySizeError));
        assert_eq!(written(Value::Format(format()), 39), Err(kAudioHardwareBadPropertySizeError));
        assert_eq!(written(Value::Str("x"), 7), Err(kAudioHardwareBadPropertySizeError));
    }

    #[test]
    fn lists_truncate_to_whole_elements() {
        assert_eq!(written(Value::Ids(&[3, 4]), 7).map(|b| b.len()), Ok(4));
        assert_eq!(written(Value::Ids(&[3, 4]), 3), Ok(vec![]));
        assert_eq!(written(Value::Ids(&[]), 0), Ok(vec![]));
        let r = AudioValueRange { mMinimum: 48_000.0, mMaximum: 48_000.0 };
        assert_eq!(written(Value::Range(r), 15), Ok(vec![]));
        assert_eq!(written(Value::Range(r), 100).map(|b| b.len()), Ok(16));
    }

    #[test]
    fn formats_and_layouts_have_the_c_layout() {
        let f = format();
        let b = written(Value::Format(f), 40).unwrap();
        assert_eq!(read_format(&b), Some(f));
        assert_eq!(&b[8..12], &kAudioFormatLinearPCM.to_ne_bytes());
        assert_eq!(&b[12..16], &9u32.to_ne_bytes());

        let rf = AudioStreamRangedDescription {
            mFormat: f,
            mSampleRateRange: AudioValueRange { mMinimum: 1.0, mMaximum: 2.0 },
        };
        let b = written(Value::RangedFormat(rf), 56).unwrap();
        assert_eq!(read_format(&b[..40]), Some(f));
        assert_eq!(&b[40..48], &1.0f64.to_ne_bytes());
        assert_eq!(&b[48..56], &2.0f64.to_ne_bytes());

        let b = written(Value::Layout(3), 72).unwrap();
        assert_eq!(b.len(), 72);
        let word = |off: usize| u32::from_ne_bytes(b[off..off + 4].try_into().unwrap());
        assert_eq!((word(0), word(4), word(8)), (0, 0, 3));
        for i in 0..3 {
            let d = 12 + 20 * i;
            assert_eq!(word(d), 0x1_0000 + i as u32);
            assert!(b[d + 4..d + 20].iter().all(|&x| x == 0));
        }
    }

    #[test]
    fn strings_are_new_cfstrings() {
        let b = written(Value::Str("OpenVirtualSoundcard"), 8).unwrap();
        let s = unsafe { b.as_ptr().cast::<CFStringRef>().read_unaligned() };
        assert_eq!(unsafe { stub::read_string(s) }.as_deref(), Some("OpenVirtualSoundcard"));
        unsafe { stub::cf_free(s) };
    }

    #[test]
    fn set_data_must_have_the_exact_size() {
        assert_eq!(read_u32(&1u32.to_ne_bytes()), Some(1));
        assert_eq!(read_u32(&[0; 8]), None);
        assert_eq!(read_f64(&48_000f64.to_ne_bytes()), Some(48_000.0));
        assert_eq!(read_f64(&[0; 4]), None);
        assert_eq!(read_format(&[0; 39]), None);
        assert_eq!(read_format(&[0; 41]), None);
    }
}
