//! The HAL object model and its properties (design section 9).
//!
//! The driver publishes four objects: the plug-in (1), one device (2) and
//! the device's input (3) and output (4) streams; no box and no controls.
//! What the device looks like comes from the published [`DriverConfig`],
//! swapped as a whole and read lock-free, so a property call never waits
//! for a configuration change. The properties themselves are rows of a
//! table (`table`), and the bytes each value occupies follow one set of
//! rules (`encode`):
//!
//! * an unknown object is `!obj`; a property the object lacks, in that
//!   scope or on that element, is `who?`;
//! * a fixed-size value that does not fit the caller's buffer is `!siz`; a
//!   list is truncated to the whole elements that fit;
//! * CFStrings are created for the caller (+1), through the [`Platform`];
//! * setting a read-only property is `nope`. The network decides the rate
//!   and format, so only the current ones can be set.
//!
//! The model is not real-time and never calls the host.

mod encode;
mod table;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use arc_swap::ArcSwap;

use crate::abi::*;
use crate::platform::Platform;
use table::{Ctx, Kind, StreamFlags};

/// What the daemon tells the driver to publish.
pub use ovsc_ipc::protocol::DriverConfig;

/// The plug-in object.
pub const PLUGIN_OBJECT: AudioObjectID = kAudioObjectPlugInObject;
/// The one device.
pub const DEVICE_OBJECT: AudioObjectID = 2;
/// The device's input stream (Dante receive channels).
pub const INPUT_STREAM_OBJECT: AudioObjectID = 3;
/// The device's output stream (Dante transmit channels).
pub const OUTPUT_STREAM_OBJECT: AudioObjectID = 4;

/// The device's custom status property, `'ovst'`: a CFString with the
/// driver's state on one line.
pub const STATUS_SELECTOR: u32 = fourcc(b"ovst");

/// What the property getters read: replaced as a whole, read lock-free.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Published {
    pub config: DriverConfig,
}

impl Published {
    /// The channel count of `scope`'s direction: 0 for any scope but input
    /// and output.
    pub fn channels(&self, scope: u32) -> u32 {
        match scope {
            kAudioObjectPropertyScopeInput => self.config.input_channels,
            kAudioObjectPropertyScopeOutput => self.config.output_channels,
            _ => 0,
        }
    }

    /// The Dante channel names of `scope`'s direction: none for any scope
    /// but input and output.
    pub fn names(&self, scope: u32) -> &[String] {
        match scope {
            kAudioObjectPropertyScopeInput => &self.config.input_names,
            kAudioObjectPropertyScopeOutput => &self.config.output_names,
            _ => &[],
        }
    }

    /// The input or output stream's format: interleaved, packed,
    /// native-endian Float32 at the nominal rate (design section 8.1). The
    /// virtual and physical formats are the same.
    pub fn stream_format(&self, input: bool) -> AudioStreamBasicDescription {
        let channels = if input { self.config.input_channels } else { self.config.output_channels };
        let bytes = channels.saturating_mul(size_of::<f32>() as u32);
        AudioStreamBasicDescription {
            mSampleRate: f64::from(self.config.sample_rate),
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagsNativeFloatPacked,
            mBytesPerPacket: bytes,
            mFramesPerPacket: 1,
            mBytesPerFrame: bytes,
            mChannelsPerFrame: channels,
            mBitsPerChannel: 32,
            mReserved: 0,
        }
    }
}

/// A property qualifier the driver understands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Qualifier {
    None,
    /// A CFString qualifier (TranslateUIDToDevice and TranslateUIDToBox).
    Str(String),
}

/// Live state some properties report, built by the driver for each call.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Live {
    /// DeviceIsRunning: some client has IO running.
    pub io_running: bool,
    /// The text of the status property.
    pub status_line: String,
}

/// What a successful SetPropertyData changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetEffect {
    /// The value set was the current one.
    NoChange,
    /// A stream's IsActive was stored.
    StreamActive { input: bool, active: bool },
}

/// The published objects and their properties. Not real-time; never calls
/// the host.
pub struct Model {
    published: ArcSwap<Published>,
    streams: StreamFlags,
}

impl Model {
    pub fn new(cfg: &DriverConfig) -> Self {
        Self {
            published: ArcSwap::from_pointee(Published { config: cfg.clone() }),
            streams: StreamFlags::new(),
        }
    }

    /// Replaces what the properties report. The caller validates `cfg` and
    /// tells the host what changed.
    pub fn publish(&self, cfg: DriverConfig) {
        self.published.store(Arc::new(Published { config: cfg }));
    }

    pub fn published(&self) -> Arc<Published> {
        self.published.load_full()
    }

    /// The IsActive flag of the input or output stream.
    pub fn stream_active(&self, input: bool) -> bool {
        self.streams.get(input).load(Ordering::Acquire)
    }

    pub fn has(&self, obj: u32, a: &PropertyAddress) -> bool {
        self.with_row(obj, a, &Qualifier::None, &Live::default(), |_, _| Ok(())).is_ok()
    }

    pub fn is_settable(&self, obj: u32, a: &PropertyAddress) -> Result<bool, OSStatus> {
        self.with_row(obj, a, &Qualifier::None, &Live::default(), |row, _| Ok(row.settable()))
    }

    pub fn data_size(
        &self,
        obj: u32,
        a: &PropertyAddress,
        q: &Qualifier,
        live: &Live,
    ) -> Result<u32, OSStatus> {
        self.with_row(obj, a, q, live, |row, c| Ok((row.value)(c).size()))
    }

    /// Writes the value into `out`; returns the bytes written.
    pub fn get(
        &self,
        obj: u32,
        a: &PropertyAddress,
        q: &Qualifier,
        live: &Live,
        out: &mut [u8],
        p: &dyn Platform,
    ) -> Result<u32, OSStatus> {
        self.with_row(obj, a, q, live, |row, c| encode::write(&(row.value)(c), out, p))
    }

    pub fn set(
        &self,
        obj: u32,
        a: &PropertyAddress,
        data: &[u8],
        _p: &dyn Platform,
    ) -> Result<SetEffect, OSStatus> {
        self.with_row(obj, a, &Qualifier::None, &Live::default(), |row, c| match row.set {
            Some(set) => set(c, data),
            None => Err(kAudioHardwareIllegalOperationError),
        })
    }

    /// Runs `f` on the row of the property at `a` on `obj`, with what it
    /// needs to make or set the value.
    fn with_row<T>(
        &self,
        obj: u32,
        a: &PropertyAddress,
        q: &Qualifier,
        live: &Live,
        f: impl FnOnce(&table::Row, &Ctx<'_>) -> Result<T, OSStatus>,
    ) -> Result<T, OSStatus> {
        let kind = Kind::of(obj).ok_or(kAudioHardwareBadObjectError)?;
        let published = self.published.load();
        let c = Ctx {
            obj,
            scope: a.mScope,
            element: a.mElement,
            published: &published,
            qualifier: q,
            live,
            streams: &self.streams,
        };
        let row = table::find(kind, a.mSelector)
            .filter(|row| row.allows(&c))
            .ok_or(kAudioHardwareUnknownPropertyError)?;
        f(row, &c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::stub::{self, StubPlatform};

    fn address(selector: u32, scope: u32, element: u32) -> PropertyAddress {
        PropertyAddress { mSelector: selector, mScope: scope, mElement: element }
    }

    fn global(selector: u32) -> PropertyAddress {
        address(selector, kAudioObjectPropertyScopeGlobal, 0)
    }

    fn get(m: &Model, obj: u32, a: &PropertyAddress, cap: usize) -> Result<Vec<u8>, OSStatus> {
        let p = StubPlatform::new();
        let mut buf = vec![0; cap];
        let n = m.get(obj, a, &Qualifier::None, &Live::default(), &mut buf, &p)?;
        buf.truncate(n as usize);
        Ok(buf)
    }

    fn get_u32(m: &Model, obj: u32, a: &PropertyAddress) -> Result<u32, OSStatus> {
        Ok(u32::from_ne_bytes(get(m, obj, a, 4)?.try_into().unwrap()))
    }

    #[test]
    fn publish_replaces_what_properties_report() {
        let m = Model::new(&DriverConfig::fallback());
        let a = address(kAudioObjectPropertyElementName, kAudioObjectPropertyScopeInput, 3);
        assert!(m.has(DEVICE_OBJECT, &a));
        let cfg = DriverConfig {
            sample_rate: 96_000,
            input_channels: 2,
            input_names: vec!["L".into(), "R".into()],
            clock_algorithm: kAudioDeviceClockAlgorithmSimpleIIR,
            ..DriverConfig::fallback()
        };
        m.publish(cfg.clone());
        assert_eq!(m.published().config, cfg);
        assert!(!m.has(DEVICE_OBJECT, &a));
        assert_eq!(
            get(&m, DEVICE_OBJECT, &global(kAudioDevicePropertyNominalSampleRate), 8),
            Ok(96_000f64.to_ne_bytes().to_vec())
        );
        assert_eq!(
            get_u32(&m, DEVICE_OBJECT, &global(kAudioDevicePropertyClockAlgorithm)),
            Ok(kAudioDeviceClockAlgorithmSimpleIIR)
        );
        let name = address(kAudioObjectPropertyElementName, kAudioObjectPropertyScopeInput, 2);
        let b = get(&m, DEVICE_OBJECT, &name, 8).unwrap();
        let s = unsafe { b.as_ptr().cast::<CFStringRef>().read_unaligned() };
        assert_eq!(unsafe { stub::read_string(s) }.as_deref(), Some("R"));
        unsafe { stub::cf_free(s) };
    }

    #[test]
    fn stream_activity_survives_a_publish() {
        let m = Model::new(&DriverConfig::fallback());
        let p = StubPlatform::new();
        let sact = global(kAudioStreamPropertyIsActive);
        assert_eq!(
            m.set(OUTPUT_STREAM_OBJECT, &sact, &0u32.to_ne_bytes(), &p),
            Ok(SetEffect::StreamActive { input: false, active: false })
        );
        m.publish(DriverConfig::fallback());
        assert!(!m.stream_active(false));
        assert!(m.stream_active(true));
        assert_eq!(get_u32(&m, OUTPUT_STREAM_OBJECT, &sact), Ok(0));
        assert_eq!(get_u32(&m, INPUT_STREAM_OBJECT, &sact), Ok(1));
    }

    #[test]
    fn formats_follow_the_channel_counts() {
        let cfg = DriverConfig { output_channels: 3, ..DriverConfig::fallback() };
        let published = Published { config: cfg };
        let f = published.stream_format(false);
        assert_eq!((f.mChannelsPerFrame, f.mBytesPerFrame, f.mBytesPerPacket), (3, 12, 12));
        assert_eq!(f.mFormatFlags, 9);
        assert_eq!(published.stream_format(true).mChannelsPerFrame, 8);
        assert_eq!(published.channels(kAudioObjectPropertyScopeGlobal), 0);
        assert!(published.names(kAudioObjectPropertyScopeWildcard).is_empty());
    }

    #[test]
    fn a_missing_name_reads_as_empty() {
        // A configuration is validated before it is published; if names were
        // ever missing, the property would still answer.
        let m = Model::new(&DriverConfig { input_names: vec![], ..DriverConfig::fallback() });
        let a = address(kAudioObjectPropertyElementName, kAudioObjectPropertyScopeInput, 1);
        let b = get(&m, DEVICE_OBJECT, &a, 8).unwrap();
        let s = unsafe { b.as_ptr().cast::<CFStringRef>().read_unaligned() };
        assert_eq!(unsafe { stub::read_string(s) }.as_deref(), Some(""));
        unsafe { stub::cf_free(s) };
    }
}
