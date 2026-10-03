//! The property table (design section 9): one row per object kind and
//! selector, saying in which scopes and elements the property exists,
//! whether it is settable, and how its value is made.
//!
//! Properties the HAL synthesizes itself (BufferFrameSize and its range,
//! ActualSampleRate, IsRunningSomewhere, HogMode, IOThreadOSWorkgroup) have
//! no row, so they answer `who?`.

use std::sync::atomic::{AtomicBool, Ordering};

use ovsc_ipc::protocol::{DEVICE_UID, MODEL_UID};
use ovsc_shm::timeline::TimelineParams;

use super::encode::{self, Value};
use super::{
    DEVICE_OBJECT, INPUT_STREAM_OBJECT, Live, OUTPUT_STREAM_OBJECT, PLUGIN_OBJECT, Published,
    Qualifier, STATUS_SELECTOR, SetEffect,
};
use crate::abi::*;

use Kind::{Device, PlugIn, Stream};

/// The kinds of object the driver publishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    PlugIn,
    Device,
    Stream,
}

impl Kind {
    /// The kind of object `obj`, or `None` if the driver has no such object.
    pub(crate) fn of(obj: AudioObjectID) -> Option<Kind> {
        match obj {
            PLUGIN_OBJECT => Some(Kind::PlugIn),
            DEVICE_OBJECT => Some(Kind::Device),
            INPUT_STREAM_OBJECT | OUTPUT_STREAM_OBJECT => Some(Kind::Stream),
            _ => None,
        }
    }
}

/// The scopes a property exists in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scopes {
    /// Every scope: the property belongs to the whole object.
    Any,
    /// Input and output only: the property has a value per direction.
    InOut,
}

/// The elements a property exists on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Elements {
    /// Every element: the element is not looked at.
    Any,
    /// The channels of the scope's direction, 1 to N.
    Channels,
}

/// The input and output streams' IsActive flags: the only state the HAL can
/// change. They survive a publish.
#[derive(Debug)]
pub(crate) struct StreamFlags {
    pub(crate) input: AtomicBool,
    pub(crate) output: AtomicBool,
}

impl StreamFlags {
    pub(crate) const fn new() -> Self {
        Self { input: AtomicBool::new(true), output: AtomicBool::new(true) }
    }

    pub(crate) fn get(&self, input: bool) -> &AtomicBool {
        if input { &self.input } else { &self.output }
    }
}

/// Everything a row needs to make or set a value: the call's object and
/// address and what was published.
pub(crate) struct Ctx<'a> {
    pub(crate) obj: AudioObjectID,
    pub(crate) scope: u32,
    pub(crate) element: u32,
    pub(crate) published: &'a Published,
    pub(crate) qualifier: &'a Qualifier,
    pub(crate) live: &'a Live,
    pub(crate) streams: &'a StreamFlags,
}

impl Ctx<'_> {
    /// Whether the call's scope is the input side. For a stream, whether it
    /// is the input stream.
    fn input(&self) -> bool {
        if Kind::of(self.obj) == Some(Kind::Stream) {
            self.obj == INPUT_STREAM_OBJECT
        } else {
            self.scope == kAudioObjectPropertyScopeInput
        }
    }

    /// The channel count of the call's scope: 0 outside input and output.
    fn scope_channels(&self) -> u32 {
        self.published.channels(self.scope)
    }

    /// The format of the stream the call is on.
    fn stream_format(&self) -> AudioStreamBasicDescription {
        self.published.stream_format(self.input())
    }
}

type ValueFn = for<'a> fn(&Ctx<'a>) -> Value<'a>;
type SetFn = fn(&Ctx<'_>, &[u8]) -> Result<SetEffect, OSStatus>;

/// One property of one kind of object.
pub(crate) struct Row {
    pub(crate) kind: Kind,
    pub(crate) selector: u32,
    pub(crate) scopes: Scopes,
    pub(crate) elements: Elements,
    /// The value, which also gives the size (see `encode`).
    pub(crate) value: ValueFn,
    /// Applies set data; `None` if the property is not settable.
    pub(crate) set: Option<SetFn>,
}

impl Row {
    pub(crate) fn settable(&self) -> bool {
        self.set.is_some()
    }

    /// Whether the property exists at the call's scope and element.
    pub(crate) fn allows(&self, c: &Ctx<'_>) -> bool {
        let scope = match self.scopes {
            Scopes::Any => true,
            Scopes::InOut => {
                matches!(c.scope, kAudioObjectPropertyScopeInput | kAudioObjectPropertyScopeOutput)
            }
        };
        let element = match self.elements {
            Elements::Any => true,
            Elements::Channels => (1..=c.scope_channels()).contains(&c.element),
        };
        scope && element
    }
}

/// The row of `selector` on objects of `kind`.
pub(crate) fn find(kind: Kind, selector: u32) -> Option<&'static Row> {
    ROWS.iter().find(|r| r.kind == kind && r.selector == selector)
}

/// The device's name in audio applications.
const NAME: &str = "Open Virtual Soundcard";
/// The driver's and the device's manufacturer.
const MANUFACTURER: &str = "OpenVirtualSoundcard";
const NO_IDS: &[AudioObjectID] = &[];
const DEVICE_IDS: &[AudioObjectID] = &[DEVICE_OBJECT];
const STREAM_IDS: &[AudioObjectID] = &[INPUT_STREAM_OBJECT, OUTPUT_STREAM_OBJECT];
const INPUT_IDS: &[AudioObjectID] = &[INPUT_STREAM_OBJECT];
const OUTPUT_IDS: &[AudioObjectID] = &[OUTPUT_STREAM_OBJECT];
const CUSTOM: &[AudioServerPlugInCustomPropertyInfo] = &[AudioServerPlugInCustomPropertyInfo {
    mSelector: STATUS_SELECTOR,
    mPropertyDataType: kAudioServerPlugInCustomPropertyDataTypeCFString,
    mQualifierDataType: kAudioServerPlugInCustomPropertyDataTypeNone,
}];

/// A property that exists in every scope and element and is read-only.
const fn row(kind: Kind, selector: u32, value: ValueFn) -> Row {
    Row { kind, selector, scopes: Scopes::Any, elements: Elements::Any, value, set: None }
}

/// A device property with a value per direction, read-only.
const fn in_out(selector: u32, value: ValueFn) -> Row {
    Row {
        kind: Kind::Device,
        selector,
        scopes: Scopes::InOut,
        elements: Elements::Any,
        value,
        set: None,
    }
}

/// A settable property in every scope and element.
const fn settable(kind: Kind, selector: u32, value: ValueFn, set: SetFn) -> Row {
    Row { kind, selector, scopes: Scopes::Any, elements: Elements::Any, value, set: Some(set) }
}

fn bool32(b: bool) -> Value<'static> {
    Value::U32(u32::from(b))
}

static ROWS: &[Row] = &[
    // The plug-in.
    row(PlugIn, kAudioObjectPropertyBaseClass, |_| Value::U32(kAudioObjectClassID)),
    row(PlugIn, kAudioObjectPropertyClass, |_| Value::U32(kAudioPlugInClassID)),
    row(PlugIn, kAudioObjectPropertyOwner, |_| Value::U32(kAudioObjectUnknown)),
    row(PlugIn, kAudioObjectPropertyManufacturer, |_| Value::Str(MANUFACTURER)),
    row(PlugIn, kAudioObjectPropertyOwnedObjects, |_| Value::Ids(DEVICE_IDS)),
    row(PlugIn, kAudioPlugInPropertyDeviceList, |_| Value::Ids(DEVICE_IDS)),
    row(PlugIn, kAudioPlugInPropertyTranslateUIDToDevice, |c| {
        // An unknown UID, or none, is kAudioObjectUnknown, not an error.
        Value::U32(match c.qualifier {
            Qualifier::Str(uid) if uid == DEVICE_UID => DEVICE_OBJECT,
            _ => kAudioObjectUnknown,
        })
    }),
    row(PlugIn, kAudioPlugInPropertyBoxList, |_| Value::Ids(NO_IDS)),
    row(PlugIn, kAudioPlugInPropertyTranslateUIDToBox, |_| Value::U32(kAudioObjectUnknown)),
    row(PlugIn, kAudioPlugInPropertyResourceBundle, |_| Value::Str("")),
    // The device: properties of the whole device.
    row(Device, kAudioObjectPropertyBaseClass, |_| Value::U32(kAudioObjectClassID)),
    row(Device, kAudioObjectPropertyClass, |_| Value::U32(kAudioDeviceClassID)),
    row(Device, kAudioObjectPropertyOwner, |_| Value::U32(PLUGIN_OBJECT)),
    row(Device, kAudioObjectPropertyName, |_| Value::Str(NAME)),
    row(Device, kAudioObjectPropertyManufacturer, |_| Value::Str(MANUFACTURER)),
    row(Device, kAudioDevicePropertyDeviceUID, |_| Value::Str(DEVICE_UID)),
    row(Device, kAudioDevicePropertyModelUID, |_| Value::Str(MODEL_UID)),
    row(Device, kAudioDevicePropertyTransportType, |_| {
        Value::U32(kAudioDeviceTransportTypeVirtual)
    }),
    row(Device, kAudioDevicePropertyRelatedDevices, |_| Value::Ids(DEVICE_IDS)),
    row(Device, kAudioDevicePropertyClockDomain, |c| Value::U32(c.published.config.clock_domain)),
    row(Device, kAudioDevicePropertyDeviceIsAlive, |_| bool32(true)),
    row(Device, kAudioDevicePropertyDeviceIsRunning, |c| bool32(c.live.io_running)),
    row(Device, kAudioDevicePropertyIsHidden, |_| bool32(false)),
    row(Device, kAudioObjectPropertyOwnedObjects, streams),
    row(Device, kAudioDevicePropertyStreams, streams),
    row(Device, kAudioObjectPropertyControlList, |_| Value::Ids(NO_IDS)),
    settable(
        Device,
        kAudioDevicePropertyNominalSampleRate,
        |c| Value::F64(f64::from(c.published.config.sample_rate)),
        set_nominal_rate,
    ),
    row(Device, kAudioDevicePropertyAvailableNominalSampleRates, |c| {
        let rate = f64::from(c.published.config.sample_rate);
        Value::Range(AudioValueRange { mMinimum: rate, mMaximum: rate })
    }),
    row(Device, kAudioDevicePropertyZeroTimeStampPeriod, |_| {
        Value::U32(TimelineParams::DEFAULT.period)
    }),
    row(Device, kAudioDevicePropertyClockAlgorithm, |c| {
        Value::U32(c.published.config.clock_algorithm)
    }),
    row(Device, kAudioDevicePropertyClockIsStable, |_| bool32(true)),
    row(Device, kAudioDevicePropertyWantsStreamFormatsRestored, |_| bool32(false)),
    row(Device, kAudioDevicePropertyWantsControlsRestored, |_| bool32(false)),
    row(Device, kAudioObjectPropertyCustomPropertyInfoList, |_| Value::Custom(CUSTOM)),
    row(Device, STATUS_SELECTOR, |c| Value::Str(&c.live.status_line)),
    // The device: properties per direction.
    in_out(kAudioDevicePropertyDeviceCanBeDefaultDevice, |_| bool32(true)),
    // Keeps alert sounds off the network.
    in_out(kAudioDevicePropertyDeviceCanBeDefaultSystemDevice, |_| bool32(false)),
    in_out(kAudioDevicePropertyLatency, |c| {
        let cfg = &c.published.config;
        Value::U32(if c.input() { cfg.input_latency } else { cfg.output_latency })
    }),
    in_out(kAudioDevicePropertySafetyOffset, |c| {
        let cfg = &c.published.config;
        Value::U32(if c.input() { cfg.input_safety_offset } else { cfg.output_safety_offset })
    }),
    in_out(kAudioDevicePropertyPreferredChannelsForStereo, |c| {
        Value::Pair(if c.scope_channels() == 1 { [1, 1] } else { [1, 2] })
    }),
    in_out(kAudioDevicePropertyPreferredChannelLayout, |c| Value::Layout(c.scope_channels())),
    Row {
        kind: Device,
        selector: kAudioObjectPropertyElementName,
        scopes: Scopes::InOut,
        elements: Elements::Channels,
        value: element_name,
        set: None,
    },
    // The streams.
    row(Stream, kAudioObjectPropertyBaseClass, |_| Value::U32(kAudioObjectClassID)),
    row(Stream, kAudioObjectPropertyClass, |_| Value::U32(kAudioStreamClassID)),
    row(Stream, kAudioObjectPropertyOwner, |_| Value::U32(DEVICE_OBJECT)),
    row(Stream, kAudioObjectPropertyOwnedObjects, |_| Value::Ids(NO_IDS)),
    row(Stream, kAudioObjectPropertyName, |c| {
        Value::Str(if c.input() {
            "Open Virtual Soundcard Input"
        } else {
            "Open Virtual Soundcard Output"
        })
    }),
    settable(
        Stream,
        kAudioStreamPropertyIsActive,
        |c| bool32(c.streams.get(c.input()).load(Ordering::Acquire)),
        set_stream_active,
    ),
    row(Stream, kAudioStreamPropertyDirection, |c| bool32(c.input())),
    row(Stream, kAudioStreamPropertyTerminalType, |_| Value::U32(kAudioStreamTerminalTypeLine)),
    row(Stream, kAudioStreamPropertyStartingChannel, |_| Value::U32(1)),
    row(Stream, kAudioStreamPropertyLatency, |_| Value::U32(0)),
    settable(
        Stream,
        kAudioStreamPropertyVirtualFormat,
        |c| Value::Format(c.stream_format()),
        set_format,
    ),
    settable(
        Stream,
        kAudioStreamPropertyPhysicalFormat,
        |c| Value::Format(c.stream_format()),
        set_format,
    ),
    row(Stream, kAudioStreamPropertyAvailableVirtualFormats, ranged_format),
    row(Stream, kAudioStreamPropertyAvailablePhysicalFormats, ranged_format),
];

/// The device's streams, filtered by scope: both for global (or the
/// wildcard), one for input or output, none for any other scope.
fn streams<'a>(c: &Ctx<'a>) -> Value<'a> {
    Value::Ids(match c.scope {
        kAudioObjectPropertyScopeInput => INPUT_IDS,
        kAudioObjectPropertyScopeOutput => OUTPUT_IDS,
        kAudioObjectPropertyScopeGlobal | kAudioObjectPropertyScopeWildcard => STREAM_IDS,
        _ => NO_IDS,
    })
}

/// The Dante name of channel `element` (from 1) of the scope's direction.
fn element_name<'a>(c: &Ctx<'a>) -> Value<'a> {
    let names = c.published.names(c.scope);
    let name = c.element.checked_sub(1).and_then(|i| names.get(i as usize));
    Value::Str(name.map_or("", String::as_str))
}

/// The stream's one format, at the one nominal rate.
fn ranged_format<'a>(c: &Ctx<'a>) -> Value<'a> {
    let format = c.stream_format();
    let range = AudioValueRange { mMinimum: format.mSampleRate, mMaximum: format.mSampleRate };
    Value::RangedFormat(AudioStreamRangedDescription { mFormat: format, mSampleRateRange: range })
}

/// The network sets the rate: only the current one is accepted.
fn set_nominal_rate(c: &Ctx<'_>, data: &[u8]) -> Result<SetEffect, OSStatus> {
    let rate = encode::read_f64(data).ok_or(kAudioHardwareBadPropertySizeError)?;
    if rate == f64::from(c.published.config.sample_rate) {
        Ok(SetEffect::NoChange)
    } else {
        Err(kAudioDeviceUnsupportedFormatError)
    }
}

/// There is one format per stream: only it is accepted.
fn set_format(c: &Ctx<'_>, data: &[u8]) -> Result<SetEffect, OSStatus> {
    let f = encode::read_format(data).ok_or(kAudioHardwareBadPropertySizeError)?;
    let current = c.stream_format();
    let same = f.mSampleRate == current.mSampleRate
        && f.mFormatID == current.mFormatID
        && f.mFormatFlags == current.mFormatFlags
        && f.mBytesPerPacket == current.mBytesPerPacket
        && f.mFramesPerPacket == current.mFramesPerPacket
        && f.mBytesPerFrame == current.mBytesPerFrame
        && f.mChannelsPerFrame == current.mChannelsPerFrame
        && f.mBitsPerChannel == current.mBitsPerChannel;
    if same { Ok(SetEffect::NoChange) } else { Err(kAudioDeviceUnsupportedFormatError) }
}

/// IsActive is stored and reported, and changes nothing else.
fn set_stream_active(c: &Ctx<'_>, data: &[u8]) -> Result<SetEffect, OSStatus> {
    let active = encode::read_u32(data).ok_or(kAudioHardwareBadPropertySizeError)? != 0;
    let input = c.input();
    c.streams.get(input).store(active, Ordering::Release);
    Ok(SetEffect::StreamActive { input, active })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_unique() {
        for (i, a) in ROWS.iter().enumerate() {
            for b in &ROWS[i + 1..] {
                assert!(
                    a.kind != b.kind || a.selector != b.selector,
                    "{:?} '{}' twice",
                    a.kind,
                    String::from_utf8_lossy(&a.selector.to_be_bytes())
                );
            }
        }
    }

    #[test]
    fn exactly_the_designed_properties_are_settable() {
        let settable: Vec<(Kind, u32)> =
            ROWS.iter().filter(|r| r.settable()).map(|r| (r.kind, r.selector)).collect();
        assert_eq!(
            settable,
            [
                (Kind::Device, kAudioDevicePropertyNominalSampleRate),
                (Kind::Stream, kAudioStreamPropertyIsActive),
                (Kind::Stream, kAudioStreamPropertyVirtualFormat),
                (Kind::Stream, kAudioStreamPropertyPhysicalFormat),
            ]
        );
    }

    #[test]
    fn synthesized_properties_have_no_row() {
        for sel in [
            kAudioDevicePropertyBufferFrameSize,
            kAudioDevicePropertyBufferFrameSizeRange,
            kAudioDevicePropertyActualSampleRate,
            kAudioDevicePropertyDeviceIsRunningSomewhere,
            kAudioDevicePropertyHogMode,
            kAudioDevicePropertyIOThreadOSWorkgroup,
        ] {
            assert!(find(Kind::Device, sel).is_none());
        }
    }
}
