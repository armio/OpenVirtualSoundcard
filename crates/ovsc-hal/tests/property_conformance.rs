//! Property conformance (design section 9).
//!
//! Every object (0 to 5), every selector the design names plus unknown ones,
//! every scope (global, input, output) and every element (0 to N+1) is
//! walked twice: through the model and through the driver's extern "C"
//! entry points. HasProperty, IsPropertySettable, GetPropertyDataSize and
//! GetPropertyData must agree with each other and with the design's table,
//! which this file restates independently of the driver's. Then the values
//! the design fixes are checked, again both ways.

use std::ffi::c_void;
use std::ptr;

use ovsc_hal::abi::*;
use ovsc_hal::model::{DriverConfig, Live, Model, Qualifier, STATUS_SELECTOR, SetEffect};
use ovsc_hal::new_driver_object;
use ovsc_hal::platform::stub::{self, StubPlatform};
use ovsc_hal::testing::{self, FakeHost};

const PLUGIN: AudioObjectID = 1;
const DEVICE: AudioObjectID = 2;
const INPUT: AudioObjectID = 3;
const OUTPUT: AudioObjectID = 4;
const GLOBAL: u32 = kAudioObjectPropertyScopeGlobal;
const IN: u32 = kAudioObjectPropertyScopeInput;
const OUT: u32 = kAudioObjectPropertyScopeOutput;
const PTR: u32 = size_of::<CFStringRef>() as u32;
const STORAGE_KEY: &str = "org.openvirtualsoundcard.config.v1";

const BAD_OBJECT: OSStatus = kAudioHardwareBadObjectError;
const UNKNOWN: OSStatus = kAudioHardwareUnknownPropertyError;
const BAD_SIZE: OSStatus = kAudioHardwareBadPropertySizeError;
const ILLEGAL: OSStatus = kAudioHardwareIllegalOperationError;
const UNSUPPORTED: OSStatus = kAudioDeviceUnsupportedFormatError;

// --- The design's table ----------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scopes {
    Any,
    InOut,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Elements {
    Any,
    /// 1 to the channel count of the scope's direction.
    Channels,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    /// Must fit whole.
    Fixed,
    /// A CFString, +1 for the caller.
    Cf,
    /// A list of elements of this size, truncated to whole elements.
    List(u32),
}

#[derive(Clone, Copy, Debug)]
struct Prop {
    selector: u32,
    scopes: Scopes,
    elements: Elements,
    settable: bool,
    shape: Shape,
}

impl Prop {
    const fn fixed(selector: u32) -> Prop {
        Prop {
            selector,
            scopes: Scopes::Any,
            elements: Elements::Any,
            settable: false,
            shape: Shape::Fixed,
        }
    }

    const fn cf(selector: u32) -> Prop {
        Prop { shape: Shape::Cf, ..Prop::fixed(selector) }
    }

    const fn list(selector: u32, element: u32) -> Prop {
        Prop { shape: Shape::List(element), ..Prop::fixed(selector) }
    }

    const fn in_out(self) -> Prop {
        Prop { scopes: Scopes::InOut, ..self }
    }

    const fn settable(self) -> Prop {
        Prop { settable: true, ..self }
    }

    /// Whether the property exists at `scope` and `element` with `channels`
    /// (input, output).
    fn exists(&self, scope: u32, element: u32, (nin, nout): (u32, u32)) -> bool {
        let scope_ok = self.scopes == Scopes::Any || scope == IN || scope == OUT;
        let n = match scope {
            IN => nin,
            OUT => nout,
            _ => 0,
        };
        let element_ok = self.elements == Elements::Any || (1..=n).contains(&element);
        scope_ok && element_ok
    }
}

const ID: u32 = 4;
const RANGE: u32 = 16;
const RANGED: u32 = 56;
const CUSTOM: u32 = 12;

const PLUGIN_PROPS: &[Prop] = &[
    Prop::fixed(kAudioObjectPropertyBaseClass),
    Prop::fixed(kAudioObjectPropertyClass),
    Prop::fixed(kAudioObjectPropertyOwner),
    Prop::cf(kAudioObjectPropertyManufacturer),
    Prop::list(kAudioObjectPropertyOwnedObjects, ID),
    Prop::list(kAudioPlugInPropertyDeviceList, ID),
    Prop::fixed(kAudioPlugInPropertyTranslateUIDToDevice),
    Prop::list(kAudioPlugInPropertyBoxList, ID),
    Prop::fixed(kAudioPlugInPropertyTranslateUIDToBox),
    Prop::cf(kAudioPlugInPropertyResourceBundle),
];

const DEVICE_PROPS: &[Prop] = &[
    Prop::fixed(kAudioObjectPropertyBaseClass),
    Prop::fixed(kAudioObjectPropertyClass),
    Prop::fixed(kAudioObjectPropertyOwner),
    Prop::cf(kAudioObjectPropertyName),
    Prop::cf(kAudioObjectPropertyManufacturer),
    Prop::cf(kAudioDevicePropertyDeviceUID),
    Prop::cf(kAudioDevicePropertyModelUID),
    Prop::fixed(kAudioDevicePropertyTransportType),
    Prop::list(kAudioDevicePropertyRelatedDevices, ID),
    Prop::fixed(kAudioDevicePropertyClockDomain),
    Prop::fixed(kAudioDevicePropertyDeviceIsAlive),
    Prop::fixed(kAudioDevicePropertyDeviceIsRunning),
    Prop::fixed(kAudioDevicePropertyIsHidden),
    Prop::list(kAudioObjectPropertyOwnedObjects, ID),
    Prop::list(kAudioDevicePropertyStreams, ID),
    Prop::list(kAudioObjectPropertyControlList, ID),
    Prop::fixed(kAudioDevicePropertyNominalSampleRate).settable(),
    Prop::list(kAudioDevicePropertyAvailableNominalSampleRates, RANGE),
    Prop::fixed(kAudioDevicePropertyZeroTimeStampPeriod),
    Prop::fixed(kAudioDevicePropertyClockAlgorithm),
    Prop::fixed(kAudioDevicePropertyClockIsStable),
    Prop::fixed(kAudioDevicePropertyWantsStreamFormatsRestored),
    Prop::fixed(kAudioDevicePropertyWantsControlsRestored),
    Prop::list(kAudioObjectPropertyCustomPropertyInfoList, CUSTOM),
    Prop::cf(STATUS_SELECTOR),
    Prop::fixed(kAudioDevicePropertyDeviceCanBeDefaultDevice).in_out(),
    Prop::fixed(kAudioDevicePropertyDeviceCanBeDefaultSystemDevice).in_out(),
    Prop::fixed(kAudioDevicePropertyLatency).in_out(),
    Prop::fixed(kAudioDevicePropertySafetyOffset).in_out(),
    Prop::fixed(kAudioDevicePropertyPreferredChannelsForStereo).in_out(),
    Prop::fixed(kAudioDevicePropertyPreferredChannelLayout).in_out(),
    Prop {
        selector: kAudioObjectPropertyElementName,
        scopes: Scopes::InOut,
        elements: Elements::Channels,
        settable: false,
        shape: Shape::Cf,
    },
];

const STREAM_PROPS: &[Prop] = &[
    Prop::fixed(kAudioObjectPropertyBaseClass),
    Prop::fixed(kAudioObjectPropertyClass),
    Prop::fixed(kAudioObjectPropertyOwner),
    Prop::list(kAudioObjectPropertyOwnedObjects, ID),
    Prop::cf(kAudioObjectPropertyName),
    Prop::fixed(kAudioStreamPropertyIsActive).settable(),
    Prop::fixed(kAudioStreamPropertyDirection),
    Prop::fixed(kAudioStreamPropertyTerminalType),
    Prop::fixed(kAudioStreamPropertyStartingChannel),
    Prop::fixed(kAudioStreamPropertyLatency),
    Prop::fixed(kAudioStreamPropertyVirtualFormat).settable(),
    Prop::fixed(kAudioStreamPropertyPhysicalFormat).settable(),
    Prop::list(kAudioStreamPropertyAvailableVirtualFormats, RANGED),
    Prop::list(kAudioStreamPropertyAvailablePhysicalFormats, RANGED),
];

/// Selectors no object has: four the HAL synthesizes, and one nobody
/// defines.
const UNKNOWN_SELECTORS: [u32; 5] = [
    kAudioDevicePropertyBufferFrameSize,
    kAudioDevicePropertyBufferFrameSizeRange,
    kAudioDevicePropertyActualSampleRate,
    kAudioDevicePropertyHogMode,
    fourcc(b"odxx"),
];

/// The properties of object `obj`, or `None` if there is no such object.
fn props_of(obj: AudioObjectID) -> Option<&'static [Prop]> {
    match obj {
        PLUGIN => Some(PLUGIN_PROPS),
        DEVICE => Some(DEVICE_PROPS),
        INPUT | OUTPUT => Some(STREAM_PROPS),
        _ => None,
    }
}

/// Every selector of every object, once each, then the unknown ones.
fn all_selectors() -> Vec<u32> {
    let mut all: Vec<u32> = Vec::new();
    for p in PLUGIN_PROPS.iter().chain(DEVICE_PROPS).chain(STREAM_PROPS) {
        if !all.contains(&p.selector) {
            all.push(p.selector);
        }
    }
    all.extend(UNKNOWN_SELECTORS);
    all
}

// --- Two ways to the properties --------------------------------------------

/// Property calls, through the model or through the vtable.
trait Props {
    fn has(&self, obj: AudioObjectID, a: &AudioObjectPropertyAddress) -> bool;
    fn settable(
        &self,
        obj: AudioObjectID,
        a: &AudioObjectPropertyAddress,
    ) -> Result<bool, OSStatus>;
    fn size(
        &self,
        obj: AudioObjectID,
        a: &AudioObjectPropertyAddress,
        q: Option<&str>,
    ) -> Result<u32, OSStatus>;
    /// What GetPropertyData writes into a buffer of `capacity` bytes. A
    /// CFString in it belongs to the caller.
    fn get(
        &self,
        obj: AudioObjectID,
        a: &AudioObjectPropertyAddress,
        q: Option<&str>,
        capacity: u32,
    ) -> Result<Vec<u8>, OSStatus>;
    fn set(
        &self,
        obj: AudioObjectID,
        a: &AudioObjectPropertyAddress,
        data: &[u8],
    ) -> Result<(), OSStatus>;
}

/// The model on its own, with a fixed live state.
struct ModelProps {
    model: Model,
    platform: StubPlatform,
    live: Live,
}

impl ModelProps {
    fn new(cfg: &DriverConfig) -> ModelProps {
        let live = Live { io_running: false, status_line: "model status".into() };
        ModelProps { model: Model::new(cfg), platform: StubPlatform::new(), live }
    }
}

fn qualifier(q: Option<&str>) -> Qualifier {
    q.map_or(Qualifier::None, |s| Qualifier::Str(s.to_owned()))
}

impl Props for ModelProps {
    fn has(&self, obj: AudioObjectID, a: &AudioObjectPropertyAddress) -> bool {
        self.model.has(obj, a)
    }

    fn settable(
        &self,
        obj: AudioObjectID,
        a: &AudioObjectPropertyAddress,
    ) -> Result<bool, OSStatus> {
        self.model.is_settable(obj, a)
    }

    fn size(
        &self,
        obj: AudioObjectID,
        a: &AudioObjectPropertyAddress,
        q: Option<&str>,
    ) -> Result<u32, OSStatus> {
        self.model.data_size(obj, a, &qualifier(q), &self.live)
    }

    fn get(
        &self,
        obj: AudioObjectID,
        a: &AudioObjectPropertyAddress,
        q: Option<&str>,
        capacity: u32,
    ) -> Result<Vec<u8>, OSStatus> {
        let mut buf = vec![0xAA; capacity as usize];
        let n = self.model.get(obj, a, &qualifier(q), &self.live, &mut buf, &self.platform)?;
        assert!(n <= capacity);
        buf.truncate(n as usize);
        Ok(buf)
    }

    fn set(
        &self,
        obj: AudioObjectID,
        a: &AudioObjectPropertyAddress,
        data: &[u8],
    ) -> Result<(), OSStatus> {
        self.model.set(obj, a, data, &self.platform).map(|_| ())
    }
}

/// A driver object, initialized with a fake host, called through its vtable.
struct Hal {
    platform: &'static StubPlatform,
    host: &'static FakeHost,
    driver: *mut c_void,
    vt: &'static DriverInterface,
}

impl Hal {
    /// A driver initialized with `stored` in host storage, if any.
    fn new(stored: Option<&str>) -> Hal {
        let platform = StubPlatform::new().leak();
        let host = FakeHost::new();
        if let Some(text) = stored {
            host.set_storage(STORAGE_KEY, text);
        }
        let driver = new_driver_object(platform, testing::null_link_factory());
        let vt = unsafe { testing::interface(driver) };
        assert_eq!(unsafe { (vt.Initialize)(driver, host.host_ref()) }, 0);
        Hal { platform, host, driver, vt }
    }

    fn with_config(cfg: &DriverConfig) -> Hal {
        Hal::new(Some(&cfg.to_storage_string()))
    }
}

/// A stub CFString for a qualifier, freed on drop.
struct CfQualifier(Option<CFStringRef>);

impl CfQualifier {
    fn new(q: Option<&str>) -> CfQualifier {
        CfQualifier(q.map(stub::cf_string))
    }

    /// The (size, pointer) pair the entry points take.
    fn raw(&self) -> (u32, *const c_void) {
        match &self.0 {
            Some(s) => (PTR, s as *const CFStringRef as *const c_void),
            None => (0, ptr::null()),
        }
    }
}

impl Drop for CfQualifier {
    fn drop(&mut self) {
        if let Some(s) = self.0 {
            unsafe { stub::cf_free(s) };
        }
    }
}

impl Props for Hal {
    fn has(&self, obj: AudioObjectID, a: &AudioObjectPropertyAddress) -> bool {
        unsafe { (self.vt.HasProperty)(self.driver, obj, 0, a) != 0 }
    }

    fn settable(
        &self,
        obj: AudioObjectID,
        a: &AudioObjectPropertyAddress,
    ) -> Result<bool, OSStatus> {
        let mut out: Boolean = 7;
        match unsafe { (self.vt.IsPropertySettable)(self.driver, obj, 0, a, &mut out) } {
            0 => {
                assert!(out <= 1);
                Ok(out != 0)
            }
            e => Err(e),
        }
    }

    fn size(
        &self,
        obj: AudioObjectID,
        a: &AudioObjectPropertyAddress,
        q: Option<&str>,
    ) -> Result<u32, OSStatus> {
        let q = CfQualifier::new(q);
        let (qsize, qdata) = q.raw();
        let mut size = u32::MAX;
        let status = unsafe {
            (self.vt.GetPropertyDataSize)(self.driver, obj, 0, a, qsize, qdata, &mut size)
        };
        match status {
            0 => Ok(size),
            e => Err(e),
        }
    }

    fn get(
        &self,
        obj: AudioObjectID,
        a: &AudioObjectPropertyAddress,
        q: Option<&str>,
        capacity: u32,
    ) -> Result<Vec<u8>, OSStatus> {
        let q = CfQualifier::new(q);
        let (qsize, qdata) = q.raw();
        let mut buf = vec![0xAAu8; capacity as usize];
        let mut used = u32::MAX;
        let status = unsafe {
            (self.vt.GetPropertyData)(
                self.driver,
                obj,
                0,
                a,
                qsize,
                qdata,
                capacity,
                &mut used,
                buf.as_mut_ptr().cast(),
            )
        };
        match status {
            0 => {
                assert!(used <= capacity, "wrote {used} of {capacity} bytes");
                buf.truncate(used as usize);
                Ok(buf)
            }
            e => Err(e),
        }
    }

    fn set(
        &self,
        obj: AudioObjectID,
        a: &AudioObjectPropertyAddress,
        data: &[u8],
    ) -> Result<(), OSStatus> {
        let status = unsafe {
            (self.vt.SetPropertyData)(
                self.driver,
                obj,
                0,
                a,
                0,
                ptr::null(),
                data.len() as u32,
                data.as_ptr().cast(),
            )
        };
        match status {
            0 => Ok(()),
            e => Err(e),
        }
    }
}

// --- Helpers ---------------------------------------------------------------

fn address(selector: u32, scope: u32, element: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress { mSelector: selector, mScope: scope, mElement: element }
}

fn global(selector: u32) -> AudioObjectPropertyAddress {
    address(selector, GLOBAL, kAudioObjectPropertyElementMain)
}

fn code(c: u32) -> String {
    String::from_utf8_lossy(&c.to_be_bytes()).into_owned()
}

/// Takes the CFString a getter returned: its text, after releasing it.
fn take_string(bytes: &[u8]) -> Option<String> {
    assert_eq!(bytes.len(), PTR as usize);
    let s = unsafe { bytes.as_ptr().cast::<CFStringRef>().read_unaligned() };
    let text = unsafe { stub::read_string(s) };
    unsafe { stub::cf_free(s) };
    text
}

fn words(bytes: &[u8]) -> Vec<u32> {
    bytes.chunks_exact(4).map(|c| u32::from_ne_bytes(c.try_into().unwrap())).collect()
}

fn floats(bytes: &[u8]) -> Vec<f64> {
    bytes.chunks_exact(8).map(|c| f64::from_ne_bytes(c.try_into().unwrap())).collect()
}

fn get_u32(p: &dyn Props, obj: AudioObjectID, a: &AudioObjectPropertyAddress) -> u32 {
    let b = p.get(obj, a, None, 4).unwrap_or_else(|e| panic!("{obj} '{}': {e}", code(a.mSelector)));
    words(&b)[0]
}

fn get_string(p: &dyn Props, obj: AudioObjectID, a: &AudioObjectPropertyAddress) -> String {
    let b =
        p.get(obj, a, None, PTR).unwrap_or_else(|e| panic!("{obj} '{}': {e}", code(a.mSelector)));
    take_string(&b).expect("a CFString")
}

/// The whole value of a list or structure property.
fn get_all(p: &dyn Props, obj: AudioObjectID, a: &AudioObjectPropertyAddress) -> Vec<u8> {
    let size = p.size(obj, a, None).unwrap();
    p.get(obj, a, None, size).unwrap()
}

/// An AudioStreamBasicDescription as (rate, format ID, flags, bytes per
/// packet, frames per packet, bytes per frame, channels, bits).
fn format_of(bytes: &[u8]) -> (f64, [u32; 7]) {
    assert!(bytes.len() >= 40);
    let w = words(&bytes[8..40]);
    (floats(&bytes[..8])[0], [w[0], w[1], w[2], w[3], w[4], w[5], w[6]])
}

fn expected_format(rate: u32, channels: u32) -> (f64, [u32; 7]) {
    (f64::from(rate), [kAudioFormatLinearPCM, 9, 4 * channels, 1, 4 * channels, channels, 32])
}

fn asbd_bytes(rate: f64, channels: u32) -> Vec<u8> {
    let mut b = rate.to_ne_bytes().to_vec();
    for w in [kAudioFormatLinearPCM, 9, 4 * channels, 1, 4 * channels, channels, 32, 0] {
        b.extend_from_slice(&w.to_ne_bytes());
    }
    b
}

// --- The walk ----------------------------------------------------------------

/// Walks every object, selector, scope and element; returns how many
/// properties exist.
fn walk(p: &dyn Props, channels: (u32, u32)) -> usize {
    let selectors = all_selectors();
    let max_element = channels.0.max(channels.1) + 1;
    let mut present = 0;
    for obj in 0..=5 {
        for &selector in &selectors {
            for scope in [GLOBAL, IN, OUT] {
                for element in 0..=max_element {
                    present +=
                        usize::from(check(p, obj, address(selector, scope, element), channels));
                }
            }
        }
    }
    present
}

/// Checks one property address; returns whether it exists.
fn check(
    p: &dyn Props,
    obj: AudioObjectID,
    a: AudioObjectPropertyAddress,
    channels: (u32, u32),
) -> bool {
    let what =
        format!("object {obj} '{}' '{}' element {}", code(a.mSelector), code(a.mScope), a.mElement);
    let Some(props) = props_of(obj) else {
        assert!(!p.has(obj, &a), "{what}");
        assert_eq!(p.settable(obj, &a), Err(BAD_OBJECT), "{what}");
        assert_eq!(p.size(obj, &a, None), Err(BAD_OBJECT), "{what}");
        assert_eq!(p.get(obj, &a, None, 256), Err(BAD_OBJECT), "{what}");
        assert_eq!(p.set(obj, &a, &[0; 4]), Err(BAD_OBJECT), "{what}");
        return false;
    };
    let expected = props
        .iter()
        .find(|prop| prop.selector == a.mSelector)
        .filter(|prop| prop.exists(a.mScope, a.mElement, channels));
    assert_eq!(p.has(obj, &a), expected.is_some(), "{what}: HasProperty");
    let Some(prop) = expected else {
        assert_eq!(p.settable(obj, &a), Err(UNKNOWN), "{what}");
        assert_eq!(p.size(obj, &a, None), Err(UNKNOWN), "{what}");
        assert_eq!(p.get(obj, &a, None, 256), Err(UNKNOWN), "{what}");
        assert_eq!(p.set(obj, &a, &[0; 4]), Err(UNKNOWN), "{what}");
        return false;
    };

    assert_eq!(p.settable(obj, &a), Ok(prop.settable), "{what}: IsPropertySettable");
    let size = p.size(obj, &a, None).unwrap_or_else(|e| panic!("{what}: size {e}"));
    let full = p.get(obj, &a, None, size).unwrap_or_else(|e| panic!("{what}: get {e}"));
    assert_eq!(full.len() as u32, size, "{what}: get wrote other than its size");
    let roomy = p.get(obj, &a, None, size + 64).unwrap_or_else(|e| panic!("{what}: get {e}"));
    assert_eq!(roomy.len() as u32, size, "{what}: get into a larger buffer");
    match prop.shape {
        Shape::Cf => {
            assert_eq!(size, PTR, "{what}");
            assert!(take_string(&full).is_some(), "{what}: not a CFString");
            assert!(take_string(&roomy).is_some(), "{what}: not a CFString");
        }
        Shape::Fixed => assert_eq!(full, roomy, "{what}"),
        Shape::List(element) => {
            assert_eq!(size % element, 0, "{what}: not whole elements");
            assert_eq!(full, roomy, "{what}");
        }
    }
    if size > 0 {
        let short = p.get(obj, &a, None, size - 1);
        match prop.shape {
            Shape::Fixed | Shape::Cf => assert_eq!(short, Err(BAD_SIZE), "{what}: short buffer"),
            Shape::List(element) => {
                let b = short.unwrap_or_else(|e| panic!("{what}: short list {e}"));
                assert_eq!(b.len() as u32, (size - 1) / element * element, "{what}");
                assert_eq!(b[..], full[..b.len()], "{what}: truncated list differs");
            }
        }
    }
    if !prop.settable {
        assert_eq!(p.set(obj, &a, &full), Err(ILLEGAL), "{what}: set");
    }
    true
}

/// How many properties the walk finds for `channels`: each property counts
/// once per (scope, element) it exists at.
fn expected_present(channels: (u32, u32)) -> usize {
    let elements = channels.0.max(channels.1) + 2;
    let mut n = 0;
    for obj in 1..=4 {
        for prop in props_of(obj).unwrap() {
            for scope in [GLOBAL, IN, OUT] {
                n += (0..elements).filter(|&e| prop.exists(scope, e, channels)).count();
            }
        }
    }
    n
}

// --- Values --------------------------------------------------------------------

/// Every value the design fixes, for a driver publishing `cfg`.
fn check_values(p: &dyn Props, cfg: &DriverConfig) {
    let (nin, nout) = (cfg.input_channels, cfg.output_channels);
    let ids = |obj, a: &AudioObjectPropertyAddress| words(&get_all(p, obj, a));

    // The plug-in.
    assert_eq!(get_u32(p, PLUGIN, &global(kAudioObjectPropertyBaseClass)), kAudioObjectClassID);
    assert_eq!(get_u32(p, PLUGIN, &global(kAudioObjectPropertyClass)), kAudioPlugInClassID);
    assert_eq!(get_u32(p, PLUGIN, &global(kAudioObjectPropertyOwner)), kAudioObjectUnknown);
    assert_eq!(
        get_string(p, PLUGIN, &global(kAudioObjectPropertyManufacturer)),
        "OpenVirtualSoundcard"
    );
    assert_eq!(ids(PLUGIN, &global(kAudioObjectPropertyOwnedObjects)), [DEVICE]);
    assert_eq!(ids(PLUGIN, &global(kAudioPlugInPropertyDeviceList)), [DEVICE]);
    assert_eq!(ids(PLUGIN, &global(kAudioPlugInPropertyBoxList)), [0u32; 0]);
    assert_eq!(get_string(p, PLUGIN, &global(kAudioPlugInPropertyResourceBundle)), "");
    let uidd = global(kAudioPlugInPropertyTranslateUIDToDevice);
    for (uid, expected) in [
        (Some("org.openvirtualsoundcard.vsc"), DEVICE),
        (Some("org.openvirtualsoundcard.vsc.model"), 0),
        (Some("org.openvirtualsoundcard.vs"), 0),
        (Some(""), 0),
        (None, 0),
    ] {
        assert_eq!(p.size(PLUGIN, &uidd, uid), Ok(4));
        assert_eq!(p.get(PLUGIN, &uidd, uid, 4), Ok(expected.to_ne_bytes().to_vec()), "{uid:?}");
    }
    let uidb = global(kAudioPlugInPropertyTranslateUIDToBox);
    assert_eq!(p.get(PLUGIN, &uidb, Some("org.openvirtualsoundcard.vsc"), 4), Ok(vec![0; 4]));

    // The device as a whole.
    let dev_u32 = |selector| get_u32(p, DEVICE, &global(selector));
    let dev_string = |selector| get_string(p, DEVICE, &global(selector));
    assert_eq!(dev_u32(kAudioObjectPropertyBaseClass), kAudioObjectClassID);
    assert_eq!(dev_u32(kAudioObjectPropertyClass), kAudioDeviceClassID);
    assert_eq!(dev_u32(kAudioObjectPropertyOwner), PLUGIN);
    assert_eq!(dev_string(kAudioObjectPropertyName), "Open Virtual Soundcard");
    assert_eq!(dev_string(kAudioObjectPropertyManufacturer), "OpenVirtualSoundcard");
    assert_eq!(dev_string(kAudioDevicePropertyDeviceUID), "org.openvirtualsoundcard.vsc");
    assert_eq!(dev_string(kAudioDevicePropertyModelUID), "org.openvirtualsoundcard.vsc.model");
    assert_eq!(dev_u32(kAudioDevicePropertyTransportType), kAudioDeviceTransportTypeVirtual);
    assert_eq!(ids(DEVICE, &global(kAudioDevicePropertyRelatedDevices)), [DEVICE]);
    assert_eq!(dev_u32(kAudioDevicePropertyClockDomain), cfg.clock_domain);
    assert_eq!(dev_u32(kAudioDevicePropertyDeviceIsAlive), 1);
    assert_eq!(dev_u32(kAudioDevicePropertyIsHidden), 0);
    assert_eq!(ids(DEVICE, &global(kAudioObjectPropertyControlList)), [0u32; 0]);
    let rate = f64::from(cfg.sample_rate);
    let nsrt = p.get(DEVICE, &global(kAudioDevicePropertyNominalSampleRate), None, 8).unwrap();
    assert_eq!(floats(&nsrt), [rate]);
    let rates = get_all(p, DEVICE, &global(kAudioDevicePropertyAvailableNominalSampleRates));
    assert_eq!(floats(&rates), [rate, rate]);
    assert_eq!(dev_u32(kAudioDevicePropertyZeroTimeStampPeriod), 16384);
    assert_eq!(dev_u32(kAudioDevicePropertyClockAlgorithm), cfg.clock_algorithm);
    assert_eq!(dev_u32(kAudioDevicePropertyClockIsStable), 1);
    assert_eq!(dev_u32(kAudioDevicePropertyWantsStreamFormatsRestored), 0);
    assert_eq!(dev_u32(kAudioDevicePropertyWantsControlsRestored), 0);
    let custom = get_all(p, DEVICE, &global(kAudioObjectPropertyCustomPropertyInfoList));
    assert_eq!(
        words(&custom),
        [STATUS_SELECTOR, kAudioServerPlugInCustomPropertyDataTypeCFString, 0]
    );
    assert!(!dev_string(STATUS_SELECTOR).is_empty());

    // The device's streams, filtered by scope.
    for selector in [kAudioObjectPropertyOwnedObjects, kAudioDevicePropertyStreams] {
        for (scope, expected) in [
            (GLOBAL, &[INPUT, OUTPUT][..]),
            (kAudioObjectPropertyScopeWildcard, &[INPUT, OUTPUT]),
            (IN, &[INPUT]),
            (OUT, &[OUTPUT]),
            (fourcc(b"ptru"), &[]),
        ] {
            assert_eq!(ids(DEVICE, &address(selector, scope, 0)), expected, "'{}'", code(scope));
        }
    }

    // The device per direction.
    for (scope, n, latency, safety, names) in [
        (IN, nin, cfg.input_latency, cfg.input_safety_offset, &cfg.input_names),
        (OUT, nout, cfg.output_latency, cfg.output_safety_offset, &cfg.output_names),
    ] {
        let at = |selector| address(selector, scope, 0);
        assert_eq!(get_u32(p, DEVICE, &at(kAudioDevicePropertyDeviceCanBeDefaultDevice)), 1);
        assert_eq!(get_u32(p, DEVICE, &at(kAudioDevicePropertyDeviceCanBeDefaultSystemDevice)), 0);
        assert_eq!(get_u32(p, DEVICE, &at(kAudioDevicePropertyLatency)), latency);
        assert_eq!(get_u32(p, DEVICE, &at(kAudioDevicePropertySafetyOffset)), safety);
        let stereo = get_all(p, DEVICE, &at(kAudioDevicePropertyPreferredChannelsForStereo));
        assert_eq!(words(&stereo), if n == 1 { [1, 1] } else { [1, 2] });

        let layout = get_all(p, DEVICE, &at(kAudioDevicePropertyPreferredChannelLayout));
        assert_eq!(layout.len() as u32, 12 + 20 * n);
        let w = words(&layout);
        assert_eq!(w[..3], [kAudioChannelLayoutTag_UseChannelDescriptions, 0, n]);
        for i in 0..n as usize {
            let d = &w[3 + 5 * i..8 + 5 * i];
            assert_eq!(d[0], 0x1_0000 + i as u32, "label of channel {i}");
            assert_eq!(d[1..], [0; 4], "flags and coordinates of channel {i}");
        }

        for e in 1..=n {
            let name = get_string(p, DEVICE, &address(kAudioObjectPropertyElementName, scope, e));
            assert_eq!(name, names[e as usize - 1]);
        }
        for e in [0, n + 1, kAudioObjectPropertyElementWildcard] {
            let a = address(kAudioObjectPropertyElementName, scope, e);
            assert!(!p.has(DEVICE, &a));
            assert_eq!(p.get(DEVICE, &a, None, PTR), Err(UNKNOWN));
        }
        // Per-direction properties do not exist at global scope.
        assert_eq!(p.size(DEVICE, &global(kAudioDevicePropertySafetyOffset), None), Err(UNKNOWN));
    }

    // The streams.
    for (obj, n, name, direction) in [
        (INPUT, nin, "Open Virtual Soundcard Input", 1),
        (OUTPUT, nout, "Open Virtual Soundcard Output", 0),
    ] {
        let s_u32 = |selector| get_u32(p, obj, &global(selector));
        assert_eq!(s_u32(kAudioObjectPropertyBaseClass), kAudioObjectClassID);
        assert_eq!(s_u32(kAudioObjectPropertyClass), kAudioStreamClassID);
        assert_eq!(s_u32(kAudioObjectPropertyOwner), DEVICE);
        assert_eq!(ids(obj, &global(kAudioObjectPropertyOwnedObjects)), [0u32; 0]);
        assert_eq!(get_string(p, obj, &global(kAudioObjectPropertyName)), name);
        assert_eq!(s_u32(kAudioStreamPropertyIsActive), 1);
        assert_eq!(s_u32(kAudioStreamPropertyDirection), direction);
        assert_eq!(s_u32(kAudioStreamPropertyTerminalType), kAudioStreamTerminalTypeLine);
        assert_eq!(s_u32(kAudioStreamPropertyStartingChannel), 1);
        assert_eq!(s_u32(kAudioStreamPropertyLatency), 0);
        for selector in [kAudioStreamPropertyVirtualFormat, kAudioStreamPropertyPhysicalFormat] {
            let f = get_all(p, obj, &global(selector));
            assert_eq!(f.len(), 40);
            assert_eq!(format_of(&f), expected_format(cfg.sample_rate, n));
        }
        for selector in [
            kAudioStreamPropertyAvailableVirtualFormats,
            kAudioStreamPropertyAvailablePhysicalFormats,
        ] {
            let list = get_all(p, obj, &global(selector));
            assert_eq!(list.len(), 56, "one ranged format");
            assert_eq!(format_of(&list[..40]), expected_format(cfg.sample_rate, n));
            assert_eq!(floats(&list[40..56]), [rate, rate]);
        }
    }
}

/// What can and cannot be set.
fn check_sets(p: &dyn Props, cfg: &DriverConfig) {
    let rate = f64::from(cfg.sample_rate);
    let other: f64 = if cfg.sample_rate == 44_100 { 48_000.0 } else { 44_100.0 };

    let nsrt = global(kAudioDevicePropertyNominalSampleRate);
    assert_eq!(p.set(DEVICE, &nsrt, &other.to_ne_bytes()), Err(UNSUPPORTED));
    assert_eq!(p.set(DEVICE, &nsrt, &rate.to_ne_bytes()), Ok(()));
    assert_eq!(p.set(DEVICE, &nsrt, &f64::NAN.to_ne_bytes()), Err(UNSUPPORTED));
    assert_eq!(p.set(DEVICE, &nsrt, &(rate as f32).to_ne_bytes()), Err(BAD_SIZE));
    assert_eq!(floats(&get_all(p, DEVICE, &nsrt)), [rate]);

    for (obj, n) in [(INPUT, cfg.input_channels), (OUTPUT, cfg.output_channels)] {
        for selector in [kAudioStreamPropertyVirtualFormat, kAudioStreamPropertyPhysicalFormat] {
            let a = global(selector);
            let current = get_all(p, obj, &a);
            assert_eq!(current, asbd_bytes(rate, n));
            assert_eq!(p.set(obj, &a, &current), Ok(()));
            assert_eq!(p.set(obj, &a, &asbd_bytes(other, n)), Err(UNSUPPORTED));
            assert_eq!(p.set(obj, &a, &asbd_bytes(rate, n + 1)), Err(UNSUPPORTED));
            let mut int16 = current.clone();
            int16[12..16].copy_from_slice(&12u32.to_ne_bytes());
            assert_eq!(p.set(obj, &a, &int16), Err(UNSUPPORTED));
            assert_eq!(p.set(obj, &a, &current[..39]), Err(BAD_SIZE));
            assert_eq!(get_all(p, obj, &a), current);
        }

        let sact = global(kAudioStreamPropertyIsActive);
        assert_eq!(p.set(obj, &sact, &0u32.to_ne_bytes()), Ok(()));
        assert_eq!(get_u32(p, obj, &sact), 0);
        assert_eq!(p.set(obj, &sact, &7u32.to_ne_bytes()), Ok(()));
        assert_eq!(get_u32(p, obj, &sact), 1);
        assert_eq!(p.set(obj, &sact, &[1]), Err(BAD_SIZE));
    }

    // Read-only, missing and unknown.
    assert_eq!(p.set(DEVICE, &global(kAudioObjectPropertyName), &[0; 8]), Err(ILLEGAL));
    assert_eq!(p.set(PLUGIN, &global(kAudioObjectPropertyManufacturer), &[0; 8]), Err(ILLEGAL));
    assert_eq!(p.set(DEVICE, &global(kAudioDevicePropertyLatency), &[0; 4]), Err(UNKNOWN));
    assert_eq!(p.set(DEVICE, &global(fourcc(b"odxx")), &[0; 4]), Err(UNKNOWN));
    assert_eq!(p.set(5, &nsrt, &rate.to_ne_bytes()), Err(BAD_OBJECT));
}

/// A configuration unlike the fallback: mono input, 96 kHz, SimpleIIR.
fn mono_config() -> DriverConfig {
    DriverConfig {
        config_gen: 7,
        sample_rate: 96_000,
        input_channels: 1,
        output_channels: 3,
        input_safety_offset: 432,
        output_safety_offset: 96,
        input_latency: 0,
        output_latency: 384,
        clock_algorithm: kAudioDeviceClockAlgorithmSimpleIIR,
        clock_domain: 5,
        input_names: vec!["Mic".into()],
        output_names: vec!["Left".into(), "Right".into(), "Centre, 100%".into()],
        ..DriverConfig::fallback()
    }
}

// --- Tests -------------------------------------------------------------------------

#[test]
fn the_fallback_is_what_the_design_says() {
    let cfg = DriverConfig::fallback();
    assert_eq!((cfg.sample_rate, cfg.input_channels, cfg.output_channels), (48_000, 8, 8));
    assert_eq!((cfg.input_safety_offset, cfg.output_safety_offset), (216, 55));
    assert_eq!((cfg.input_latency, cfg.output_latency), (0, 192));
    assert_eq!(cfg.clock_algorithm, kAudioDeviceClockAlgorithmRaw);
    let names: Vec<String> = (1..=8).map(|i| format!("{i:02}")).collect();
    assert_eq!(cfg.input_names, names);
    assert_eq!(cfg.output_names, names);
}

#[test]
fn model_walk() {
    for cfg in [DriverConfig::fallback(), mono_config()] {
        let p = ModelProps::new(&cfg);
        let channels = (cfg.input_channels, cfg.output_channels);
        assert_eq!(walk(&p, channels), expected_present(channels));
    }
}

#[test]
fn entry_point_walk() {
    for cfg in [DriverConfig::fallback(), mono_config()] {
        let hal = Hal::with_config(&cfg);
        let channels = (cfg.input_channels, cfg.output_channels);
        assert_eq!(walk(&hal, channels), expected_present(channels));
        assert!(!unsafe { testing::faulted(hal.driver) });
    }
}

#[test]
fn model_values() {
    for cfg in [DriverConfig::fallback(), mono_config()] {
        let p = ModelProps::new(&cfg);
        check_values(&p, &cfg);
        assert_eq!(get_string(&p, DEVICE, &global(STATUS_SELECTOR)), "model status");
        assert_eq!(get_u32(&p, DEVICE, &global(kAudioDevicePropertyDeviceIsRunning)), 0);
        check_sets(&p, &cfg);
    }
}

#[test]
fn entry_point_values() {
    for cfg in [DriverConfig::fallback(), mono_config()] {
        let hal = Hal::with_config(&cfg);
        check_values(&hal, &cfg);
        check_sets(&hal, &cfg);
        assert!(!unsafe { testing::faulted(hal.driver) });
    }
}

#[test]
fn the_fallback_device_through_the_entry_points() {
    // Nothing stored: the fallback is published.
    let hal = Hal::new(None);
    let cfg = DriverConfig::fallback();
    check_values(&hal, &cfg);

    // The values the design spells out.
    let rates = get_all(&hal, DEVICE, &global(kAudioDevicePropertyAvailableNominalSampleRates));
    assert_eq!(floats(&rates), [48_000.0, 48_000.0]);
    for e in 1..=8u32 {
        let a = address(kAudioObjectPropertyElementName, IN, e);
        assert_eq!(get_string(&hal, DEVICE, &a), format!("{e:02}"));
    }
    let layout = get_all(&hal, DEVICE, &address(kAudioDevicePropertyPreferredChannelLayout, IN, 0));
    assert_eq!(layout.len(), 12 + 20 * 8);
    let formats = get_all(&hal, INPUT, &global(kAudioStreamPropertyAvailablePhysicalFormats));
    assert_eq!(formats.len(), 56);
    assert_eq!(words(&formats[12..16]), [9]);
    let nsrt = global(kAudioDevicePropertyNominalSampleRate);
    assert_eq!(hal.set(DEVICE, &nsrt, &44_100f64.to_ne_bytes()), Err(UNSUPPORTED));
    assert_eq!(hal.set(DEVICE, &nsrt, &48_000f64.to_ne_bytes()), Ok(()));
    let sflt = address(kAudioDevicePropertyDeviceCanBeDefaultSystemDevice, OUT, 0);
    assert_eq!(get_u32(&hal, DEVICE, &sflt), 0);
    assert_eq!(get_u32(&hal, DEVICE, &global(kAudioDevicePropertyWantsStreamFormatsRestored)), 0);
}

#[test]
fn the_status_property_is_a_cfstring() {
    let hal = Hal::new(None);
    let status = get_string(&hal, DEVICE, &global(STATUS_SELECTOR));
    assert!(status.starts_with("daemon=absent "), "{status}");
    assert!(status.contains(" rate=48000 in=8 out=8 "), "{status}");
    assert!(status.contains(" faulted=0 "), "{status}");

    let cfg = mono_config();
    let hal = Hal::with_config(&cfg);
    let status = get_string(&hal, DEVICE, &global(STATUS_SELECTOR));
    assert!(status.contains(" rate=96000 in=1 out=3 "), "{status}");
}

#[test]
fn device_is_running_follows_io() {
    let hal = Hal::new(None);
    let goin = global(kAudioDevicePropertyDeviceIsRunning);
    assert_eq!(get_u32(&hal, DEVICE, &goin), 0);
    assert_eq!(unsafe { (hal.vt.StartIO)(hal.driver, DEVICE, 0) }, 0);
    assert_eq!(get_u32(&hal, DEVICE, &goin), 1);
    let status = get_string(&hal, DEVICE, &global(STATUS_SELECTOR));
    assert!(status.contains(" io=1 "), "{status}");
    assert_eq!(unsafe { (hal.vt.StopIO)(hal.driver, DEVICE, 0) }, 0);
    assert_eq!(get_u32(&hal, DEVICE, &goin), 0);
}

#[test]
fn a_stored_configuration_is_restored_at_initialize() {
    let cfg = mono_config();
    let hal = Hal::with_config(&cfg);
    assert_eq!(get_u32(&hal, DEVICE, &global(kAudioDevicePropertyClockDomain)), 5);
    let stereo =
        get_all(&hal, DEVICE, &address(kAudioDevicePropertyPreferredChannelsForStereo, IN, 0));
    assert_eq!(words(&stereo), [1, 1]);
    assert!(hal.platform.logged("restored configuration 7: 96000 Hz, 1 in, 3 out"));
    assert_eq!(
        hal.host.calls(),
        vec![testing::HostCall::CopyFromStorage { key: STORAGE_KEY.into() }]
    );
}

#[test]
fn an_unusable_stored_configuration_falls_back() {
    let invalid = DriverConfig { input_channels: 0, input_names: vec![], ..mono_config() };
    for stored in ["garbage".to_owned(), invalid.to_storage_string()] {
        let hal = Hal::new(Some(&stored));
        check_values(&hal, &DriverConfig::fallback());
        assert!(hal.platform.logged("stored configuration ignored"));
    }
}

#[test]
fn model_sets_report_their_effect() {
    let m = Model::new(&DriverConfig::fallback());
    let p = StubPlatform::new();
    let nsrt = global(kAudioDevicePropertyNominalSampleRate);
    assert_eq!(m.set(DEVICE, &nsrt, &48_000f64.to_ne_bytes(), &p), Ok(SetEffect::NoChange));
    let sact = global(kAudioStreamPropertyIsActive);
    assert_eq!(
        m.set(INPUT, &sact, &0u32.to_ne_bytes(), &p),
        Ok(SetEffect::StreamActive { input: true, active: false })
    );
    assert!(!m.stream_active(true));
    assert!(m.stream_active(false));
    assert_eq!(
        m.set(OUTPUT, &sact, &1u32.to_ne_bytes(), &p),
        Ok(SetEffect::StreamActive { input: false, active: true })
    );
    let format = global(kAudioStreamPropertyVirtualFormat);
    assert_eq!(m.set(OUTPUT, &format, &asbd_bytes(48_000.0, 8), &p), Ok(SetEffect::NoChange));
}

#[test]
fn properties_hold_in_every_scope_where_global() {
    // Properties of the whole object answer in any scope, the wildcard too,
    // as Apple's samples do; per-direction ones only in input and output.
    for p in [&ModelProps::new(&DriverConfig::fallback()) as &dyn Props, &Hal::new(None)] {
        for scope in [kAudioObjectPropertyScopeWildcard, fourcc(b"ptru")] {
            let a = address(kAudioDevicePropertyDeviceUID, scope, 0);
            assert!(p.has(DEVICE, &a));
            assert_eq!(get_string(p, DEVICE, &a), "org.openvirtualsoundcard.vsc");
            assert!(!p.has(DEVICE, &address(kAudioDevicePropertyLatency, scope, 0)));
        }
        assert!(p.has(INPUT, &address(kAudioStreamPropertyLatency, IN, 3)));
    }
}
