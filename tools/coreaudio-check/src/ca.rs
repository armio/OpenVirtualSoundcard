//! The few Core Audio client calls this tool needs. Constants and layouts
//! are checked against the SDK by `abi_check.c`.

#![allow(non_upper_case_globals, non_snake_case, clippy::upper_case_acronyms)]

use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;

pub type OSStatus = i32;
pub type AudioObjectID = u32;
type CFStringRef = *const c_void;

pub const fn fourcc(s: &[u8; 4]) -> u32 {
    (s[0] as u32) << 24 | (s[1] as u32) << 16 | (s[2] as u32) << 8 | s[3] as u32
}

pub const kAudioObjectSystemObject: AudioObjectID = 1;
pub const kAudioHardwarePropertyDevices: u32 = fourcc(b"dev#");
pub const kAudioHardwarePropertyTranslateUIDToDevice: u32 = fourcc(b"uidd");
pub const kAudioObjectPropertyName: u32 = fourcc(b"lnam");
pub const kAudioObjectPropertyManufacturer: u32 = fourcc(b"lmak");
pub const kAudioDevicePropertyDeviceUID: u32 = fourcc(b"uid ");
pub const kAudioDevicePropertyModelUID: u32 = fourcc(b"muid");
pub const kAudioDevicePropertyNominalSampleRate: u32 = fourcc(b"nsrt");
pub const kAudioDevicePropertyActualSampleRate: u32 = fourcc(b"asrt");
pub const kAudioDevicePropertyAvailableNominalSampleRates: u32 = fourcc(b"nsr#");
pub const kAudioDevicePropertyStreamConfiguration: u32 = fourcc(b"slay");
pub const kAudioDevicePropertyBufferFrameSize: u32 = fourcc(b"fsiz");
pub const kAudioDevicePropertyLatency: u32 = fourcc(b"ltnc");
pub const kAudioDevicePropertySafetyOffset: u32 = fourcc(b"saft");
pub const kAudioDevicePropertyDeviceIsAlive: u32 = fourcc(b"livn");
pub const kAudioDevicePropertyDeviceIsRunningSomewhere: u32 = fourcc(b"gone");
pub const kAudioDevicePropertyTransportType: u32 = fourcc(b"tran");
pub const kAudioDevicePropertyClockDomain: u32 = fourcc(b"clkd");
pub const kAudioDevicePropertyZeroTimeStampPeriod: u32 = fourcc(b"ring");
pub const kAudioDevicePropertyClockAlgorithm: u32 = fourcc(b"clok");
pub const kAudioDevicePropertyIsHidden: u32 = fourcc(b"hidn");
pub const kAudioDevicePropertyStreams: u32 = fourcc(b"stm#");
pub const kAudioStreamPropertyVirtualFormat: u32 = fourcc(b"sfmt");

pub const kAudioDevicePropertyBufferFrameSizeRange: u32 = fourcc(b"fsz#");
pub const kAudioObjectPropertyElementName: u32 = fourcc(b"lchn");
pub const kAudioObjectPropertyOwnedObjects: u32 = fourcc(b"ownd");
pub const kAudioObjectPropertyBaseClass: u32 = fourcc(b"bcls");
pub const kAudioObjectPropertyClass: u32 = fourcc(b"clas");
pub const kAudioObjectPropertyOwner: u32 = fourcc(b"stdv");
pub const kAudioObjectPropertyControlList: u32 = fourcc(b"ctrl");
pub const kAudioObjectPropertyCustomPropertyInfoList: u32 = fourcc(b"cust");
pub const kAudioDevicePropertyDeviceIsRunning: u32 = fourcc(b"goin");
pub const kAudioDevicePropertyRelatedDevices: u32 = fourcc(b"akin");
pub const kAudioDevicePropertyClockIsStable: u32 = fourcc(b"cstb");
pub const kAudioDevicePropertyPreferredChannelsForStereo: u32 = fourcc(b"dch2");
pub const kAudioDevicePropertyPreferredChannelLayout: u32 = fourcc(b"srnd");
pub const kAudioDevicePropertyDeviceCanBeDefaultDevice: u32 = fourcc(b"dflt");
pub const kAudioDevicePropertyDeviceCanBeDefaultSystemDevice: u32 = fourcc(b"sflt");
pub const kAudioStreamPropertyPhysicalFormat: u32 = fourcc(b"pft ");
pub const kAudioStreamPropertyAvailableVirtualFormats: u32 = fourcc(b"sfma");
pub const kAudioStreamPropertyAvailablePhysicalFormats: u32 = fourcc(b"pfta");
pub const kAudioStreamPropertyDirection: u32 = fourcc(b"sdir");
pub const kAudioStreamPropertyTerminalType: u32 = fourcc(b"term");
pub const kAudioStreamPropertyStartingChannel: u32 = fourcc(b"schn");
pub const kAudioStreamPropertyIsActive: u32 = fourcc(b"sact");

pub const kAudioObjectPropertyScopeGlobal: u32 = fourcc(b"glob");
pub const kAudioObjectPropertyScopeInput: u32 = fourcc(b"inpt");
pub const kAudioObjectPropertyScopeOutput: u32 = fourcc(b"outp");
pub const kAudioObjectPropertyElementMain: u32 = 0;

pub const kAudioFormatLinearPCM: u32 = fourcc(b"lpcm");
pub const kAudioFormatFlagIsFloat: u32 = 1;

pub const kAudioTimeStampSampleTimeValid: u32 = 1;
pub const kAudioTimeStampHostTimeValid: u32 = 2;

const kCFStringEncodingUTF8: u32 = 0x0800_0100;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioObjectPropertyAddress {
    pub mSelector: u32,
    pub mScope: u32,
    pub mElement: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct SMPTETime {
    pub mSubframes: i16,
    pub mSubframeDivisor: i16,
    pub mCounter: u32,
    pub mType: u32,
    pub mFlags: u32,
    pub mHours: i16,
    pub mMinutes: i16,
    pub mSeconds: i16,
    pub mFrames: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct AudioTimeStamp {
    pub mSampleTime: f64,
    pub mHostTime: u64,
    pub mRateScalar: f64,
    pub mWordClockTime: u64,
    pub mSMPTETime: SMPTETime,
    pub mFlags: u32,
    pub mReserved: u32,
}

#[repr(C)]
pub struct AudioBuffer {
    pub mNumberChannels: u32,
    pub mDataByteSize: u32,
    pub mData: *mut c_void,
}

/// Variable length: `mNumberBuffers` buffers follow.
#[repr(C)]
pub struct AudioBufferList {
    pub mNumberBuffers: u32,
    pub mBuffers: [AudioBuffer; 1],
}

impl AudioBufferList {
    /// # Safety
    /// `list` must point to a valid buffer list.
    pub unsafe fn buffers<'a>(list: *const AudioBufferList) -> &'a [AudioBuffer] {
        if list.is_null() {
            return &[];
        }
        unsafe {
            std::slice::from_raw_parts((*list).mBuffers.as_ptr(), (*list).mNumberBuffers as usize)
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct AudioStreamBasicDescription {
    pub mSampleRate: f64,
    pub mFormatID: u32,
    pub mFormatFlags: u32,
    pub mBytesPerPacket: u32,
    pub mFramesPerPacket: u32,
    pub mBytesPerFrame: u32,
    pub mChannelsPerFrame: u32,
    pub mBitsPerChannel: u32,
    pub mReserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AudioValueRange {
    pub mMinimum: f64,
    pub mMaximum: f64,
}

pub type AudioDeviceIOProc = unsafe extern "C" fn(
    device: AudioObjectID,
    now: *const AudioTimeStamp,
    input: *const AudioBufferList,
    input_time: *const AudioTimeStamp,
    output: *mut AudioBufferList,
    output_time: *const AudioTimeStamp,
    client: *mut c_void,
) -> OSStatus;

pub type AudioDeviceIOProcID = Option<AudioDeviceIOProc>;

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectHasProperty(id: AudioObjectID, address: *const AudioObjectPropertyAddress) -> u8;
    fn AudioObjectGetPropertyDataSize(
        id: AudioObjectID,
        address: *const AudioObjectPropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        out_size: *mut u32,
    ) -> OSStatus;
    fn AudioObjectGetPropertyData(
        id: AudioObjectID,
        address: *const AudioObjectPropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        io_size: *mut u32,
        out: *mut c_void,
    ) -> OSStatus;
    fn AudioObjectSetPropertyData(
        id: AudioObjectID,
        address: *const AudioObjectPropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: u32,
        data: *const c_void,
    ) -> OSStatus;
    pub fn AudioDeviceCreateIOProcID(
        device: AudioObjectID,
        proc_: AudioDeviceIOProc,
        client: *mut c_void,
        out_id: *mut AudioDeviceIOProcID,
    ) -> OSStatus;
    pub fn AudioDeviceDestroyIOProcID(device: AudioObjectID, id: AudioDeviceIOProcID) -> OSStatus;
    pub fn AudioDeviceStart(device: AudioObjectID, id: AudioDeviceIOProcID) -> OSStatus;
    pub fn AudioDeviceStop(device: AudioObjectID, id: AudioDeviceIOProcID) -> OSStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(alloc: *const c_void, s: *const c_char, enc: u32) -> CFStringRef;
    fn CFStringGetCString(s: CFStringRef, buf: *mut c_char, size: isize, enc: u32) -> u8;
    pub fn CFRelease(obj: *const c_void);
}

#[repr(C)]
#[derive(Default)]
struct MachTimebase {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_timebase_info(info: *mut MachTimebase) -> i32;
}

/// Host ticks (mach_absolute_time) to nanoseconds.
pub fn ticks_to_ns(ticks: u64) -> u64 {
    let mut tb = MachTimebase::default();
    unsafe { mach_timebase_info(&mut tb) };
    (ticks as u128 * tb.numer as u128 / tb.denom.max(1) as u128) as u64
}

pub fn status_str(s: OSStatus) -> String {
    let b = (s as u32).to_be_bytes();
    if b.iter().all(|c| c.is_ascii_graphic() || *c == b' ') {
        format!("'{}' ({s})", String::from_utf8_lossy(&b))
    } else {
        s.to_string()
    }
}

fn addr(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    addr_el(selector, scope, kAudioObjectPropertyElementMain)
}

fn addr_el(selector: u32, scope: u32, element: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress { mSelector: selector, mScope: scope, mElement: element }
}

/// A fourcc as text, e.g. `'nsrt'`.
pub fn fourcc_str(v: u32) -> String {
    String::from_utf8_lossy(&v.to_be_bytes()).into_owned()
}

/// Parses four characters ("nsrt", "uid ") into a fourcc.
pub fn parse_fourcc(s: &str) -> Option<u32> {
    let b = s.as_bytes();
    (b.len() == 4).then(|| fourcc(&[b[0], b[1], b[2], b[3]]))
}

pub fn has_el(id: AudioObjectID, selector: u32, scope: u32, element: u32) -> bool {
    let a = addr_el(selector, scope, element);
    unsafe { AudioObjectHasProperty(id, &a) != 0 }
}

/// The reported data size of a property.
pub fn size_el(
    id: AudioObjectID,
    selector: u32,
    scope: u32,
    element: u32,
) -> Result<u32, OSStatus> {
    let a = addr_el(selector, scope, element);
    let mut size = 0u32;
    let s = unsafe { AudioObjectGetPropertyDataSize(id, &a, 0, ptr::null(), &mut size) };
    if s == 0 { Ok(size) } else { Err(s) }
}

/// Reads a property into a buffer of `capacity` bytes (8-byte aligned) and
/// returns the bytes the object wrote.
pub fn get_raw_el(
    id: AudioObjectID,
    selector: u32,
    scope: u32,
    element: u32,
    capacity: u32,
) -> Result<Vec<u8>, OSStatus> {
    let a = addr_el(selector, scope, element);
    let mut buf = vec![0u64; (capacity as usize).div_ceil(8).max(1)];
    let mut size = capacity;
    let s = unsafe {
        AudioObjectGetPropertyData(id, &a, 0, ptr::null(), &mut size, buf.as_mut_ptr().cast())
    };
    if s != 0 {
        return Err(s);
    }
    let bytes: Vec<u8> = buf.iter().flat_map(|w| w.to_ne_bytes()).take(size as usize).collect();
    Ok(bytes)
}

/// Reads a CFString property of an element.
pub fn get_string_el(
    id: AudioObjectID,
    selector: u32,
    scope: u32,
    element: u32,
) -> Result<String, OSStatus> {
    let bytes =
        get_raw_el(id, selector, scope, element, std::mem::size_of::<CFStringRef>() as u32)?;
    if bytes.len() < std::mem::size_of::<CFStringRef>() {
        return Err(fourcc(b"!siz") as i32);
    }
    let mut p = [0u8; std::mem::size_of::<CFStringRef>()];
    p.copy_from_slice(&bytes[..std::mem::size_of::<CFStringRef>()]);
    let s = usize::from_ne_bytes(p) as CFStringRef;
    if s.is_null() {
        return Ok(String::new());
    }
    let out = cfstring_to_string(s);
    unsafe { CFRelease(s) };
    Ok(out)
}

/// Reads a fixed-size property.
pub fn get<T: Copy>(id: AudioObjectID, selector: u32, scope: u32) -> Result<T, OSStatus> {
    let a = addr(selector, scope);
    let mut value = std::mem::MaybeUninit::<T>::zeroed();
    let mut size = std::mem::size_of::<T>() as u32;
    let s = unsafe {
        AudioObjectGetPropertyData(id, &a, 0, ptr::null(), &mut size, value.as_mut_ptr().cast())
    };
    if s != 0 {
        return Err(s);
    }
    Ok(unsafe { value.assume_init() })
}

/// Writes a fixed-size property.
pub fn set<T: Copy>(
    id: AudioObjectID,
    selector: u32,
    scope: u32,
    value: T,
) -> Result<(), OSStatus> {
    let a = addr(selector, scope);
    let size = std::mem::size_of::<T>() as u32;
    let s = unsafe {
        AudioObjectSetPropertyData(id, &a, 0, ptr::null(), size, (&value as *const T).cast())
    };
    if s == 0 { Ok(()) } else { Err(s) }
}

/// Reads a variable-size property as raw bytes (8-byte aligned).
fn get_bytes(id: AudioObjectID, selector: u32, scope: u32) -> Result<Vec<u64>, OSStatus> {
    let a = addr(selector, scope);
    let mut size = 0u32;
    let s = unsafe { AudioObjectGetPropertyDataSize(id, &a, 0, ptr::null(), &mut size) };
    if s != 0 {
        return Err(s);
    }
    let mut buf = vec![0u64; (size as usize).div_ceil(8).max(1)];
    let s = unsafe {
        AudioObjectGetPropertyData(id, &a, 0, ptr::null(), &mut size, buf.as_mut_ptr().cast())
    };
    if s != 0 {
        return Err(s);
    }
    buf.truncate((size as usize).div_ceil(8));
    // Callers use only the first `size` bytes.
    Ok(buf)
}

fn get_array<T: Copy>(id: AudioObjectID, selector: u32, scope: u32) -> Result<Vec<T>, OSStatus> {
    let a = addr(selector, scope);
    let mut size = 0u32;
    let s = unsafe { AudioObjectGetPropertyDataSize(id, &a, 0, ptr::null(), &mut size) };
    if s != 0 {
        return Err(s);
    }
    let n = size as usize / std::mem::size_of::<T>();
    let mut v: Vec<T> = Vec::with_capacity(n);
    let s = unsafe {
        AudioObjectGetPropertyData(id, &a, 0, ptr::null(), &mut size, v.as_mut_ptr().cast())
    };
    if s != 0 {
        return Err(s);
    }
    unsafe { v.set_len(size as usize / std::mem::size_of::<T>()) };
    Ok(v)
}

fn cfstring_to_string(s: CFStringRef) -> String {
    let mut buf = vec![0 as c_char; 1024];
    let ok = unsafe {
        CFStringGetCString(s, buf.as_mut_ptr(), buf.len() as isize, kCFStringEncodingUTF8)
    };
    if ok == 0 {
        return String::new();
    }
    unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
}

pub fn get_string(id: AudioObjectID, selector: u32, scope: u32) -> Result<String, OSStatus> {
    let s: CFStringRef = get(id, selector, scope)?;
    if s.is_null() {
        return Ok(String::new());
    }
    let out = cfstring_to_string(s);
    unsafe { CFRelease(s) };
    Ok(out)
}

pub fn devices() -> Vec<AudioObjectID> {
    get_array(
        kAudioObjectSystemObject,
        kAudioHardwarePropertyDevices,
        kAudioObjectPropertyScopeGlobal,
    )
    .unwrap_or_default()
}

pub fn device_by_uid(uid: &str) -> Option<AudioObjectID> {
    let c = CString::new(uid).ok()?;
    let cf = unsafe { CFStringCreateWithCString(ptr::null(), c.as_ptr(), kCFStringEncodingUTF8) };
    if cf.is_null() {
        return None;
    }
    let a = addr(kAudioHardwarePropertyTranslateUIDToDevice, kAudioObjectPropertyScopeGlobal);
    let mut id: AudioObjectID = 0;
    let mut size = 4u32;
    let s = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject,
            &a,
            std::mem::size_of::<CFStringRef>() as u32,
            (&cf as *const CFStringRef).cast(),
            &mut size,
            (&mut id as *mut AudioObjectID).cast(),
        )
    };
    unsafe { CFRelease(cf) };
    (s == 0 && id != 0).then_some(id)
}

/// Channels per buffer in the device's stream configuration for `scope`.
pub fn stream_channels(id: AudioObjectID, scope: u32) -> Result<Vec<u32>, OSStatus> {
    let bytes = get_bytes(id, kAudioDevicePropertyStreamConfiguration, scope)?;
    let list = bytes.as_ptr() as *const AudioBufferList;
    Ok(unsafe { AudioBufferList::buffers(list) }.iter().map(|b| b.mNumberChannels).collect())
}

pub fn streams(id: AudioObjectID, scope: u32) -> Vec<AudioObjectID> {
    get_array(id, kAudioDevicePropertyStreams, scope).unwrap_or_default()
}

pub fn owned_objects(id: AudioObjectID) -> Vec<AudioObjectID> {
    get_array(id, kAudioObjectPropertyOwnedObjects, kAudioObjectPropertyScopeGlobal)
        .unwrap_or_default()
}

pub fn available_rates(id: AudioObjectID) -> Vec<AudioValueRange> {
    get_array(id, kAudioDevicePropertyAvailableNominalSampleRates, kAudioObjectPropertyScopeGlobal)
        .unwrap_or_default()
}
