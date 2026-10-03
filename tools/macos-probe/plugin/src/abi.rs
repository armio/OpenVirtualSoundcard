//! The subset of the AudioServerPlugIn C ABI the probe needs. Checked
//! against the SDK headers by `bundle/abi_check.c`. Names follow the C headers.

#![allow(clippy::upper_case_acronyms)]

use std::ffi::c_void;

pub type OSStatus = i32;
pub type HRESULT = i32;
pub type ULONG = u32;
pub type Boolean = u8;
pub type AudioObjectID = u32;
pub type CFAllocatorRef = *const c_void;
pub type CFUUIDRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type HostRef = *const c_void;
/// `AudioServerPlugInDriverRef`: a pointer to a pointer to the interface.
pub type DriverRefPtr = *mut c_void;

pub const fn fourcc(s: &[u8; 4]) -> u32 {
    (s[0] as u32) << 24 | (s[1] as u32) << 16 | (s[2] as u32) << 8 | s[3] as u32
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CFUUIDBytes(pub [u8; 16]);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioObjectPropertyAddress {
    pub mSelector: u32,
    pub mScope: u32,
    pub mElement: u32,
}

#[repr(C)]
pub struct ClientInfo {
    pub mClientID: u32,
    pub mProcessID: libc::pid_t,
    pub mIsNativeEndian: Boolean,
    pub mBundleID: CFStringRef,
}

#[repr(C)]
#[derive(Clone, Copy)]
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
#[derive(Clone, Copy)]
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
pub struct IOCycleInfo {
    pub mIOCycleCounter: u64,
    pub mNominalIOBufferFrameSize: u32,
    pub mCurrentTime: AudioTimeStamp,
    pub mInputTime: AudioTimeStamp,
    pub mOutputTime: AudioTimeStamp,
    pub mMainHostTicksPerFrame: f64,
    pub mDeviceHostTicksPerFrame: f64,
}

#[repr(C)]
pub struct DriverInterface {
    pub _reserved: *mut c_void,
    pub QueryInterface: unsafe extern "C" fn(*mut c_void, CFUUIDBytes, *mut *mut c_void) -> HRESULT,
    pub AddRef: unsafe extern "C" fn(*mut c_void) -> ULONG,
    pub Release: unsafe extern "C" fn(*mut c_void) -> ULONG,
    pub Initialize: unsafe extern "C" fn(DriverRefPtr, HostRef) -> OSStatus,
    pub CreateDevice: unsafe extern "C" fn(
        DriverRefPtr,
        CFDictionaryRef,
        *const ClientInfo,
        *mut AudioObjectID,
    ) -> OSStatus,
    pub DestroyDevice: unsafe extern "C" fn(DriverRefPtr, AudioObjectID) -> OSStatus,
    pub AddDeviceClient:
        unsafe extern "C" fn(DriverRefPtr, AudioObjectID, *const ClientInfo) -> OSStatus,
    pub RemoveDeviceClient:
        unsafe extern "C" fn(DriverRefPtr, AudioObjectID, *const ClientInfo) -> OSStatus,
    pub PerformDeviceConfigurationChange:
        unsafe extern "C" fn(DriverRefPtr, AudioObjectID, u64, *mut c_void) -> OSStatus,
    pub AbortDeviceConfigurationChange:
        unsafe extern "C" fn(DriverRefPtr, AudioObjectID, u64, *mut c_void) -> OSStatus,
    pub HasProperty: unsafe extern "C" fn(
        DriverRefPtr,
        AudioObjectID,
        libc::pid_t,
        *const AudioObjectPropertyAddress,
    ) -> Boolean,
    pub IsPropertySettable: unsafe extern "C" fn(
        DriverRefPtr,
        AudioObjectID,
        libc::pid_t,
        *const AudioObjectPropertyAddress,
        *mut Boolean,
    ) -> OSStatus,
    pub GetPropertyDataSize: unsafe extern "C" fn(
        DriverRefPtr,
        AudioObjectID,
        libc::pid_t,
        *const AudioObjectPropertyAddress,
        u32,
        *const c_void,
        *mut u32,
    ) -> OSStatus,
    pub GetPropertyData: unsafe extern "C" fn(
        DriverRefPtr,
        AudioObjectID,
        libc::pid_t,
        *const AudioObjectPropertyAddress,
        u32,
        *const c_void,
        u32,
        *mut u32,
        *mut c_void,
    ) -> OSStatus,
    pub SetPropertyData: unsafe extern "C" fn(
        DriverRefPtr,
        AudioObjectID,
        libc::pid_t,
        *const AudioObjectPropertyAddress,
        u32,
        *const c_void,
        u32,
        *const c_void,
    ) -> OSStatus,
    pub StartIO: unsafe extern "C" fn(DriverRefPtr, AudioObjectID, u32) -> OSStatus,
    pub StopIO: unsafe extern "C" fn(DriverRefPtr, AudioObjectID, u32) -> OSStatus,
    pub GetZeroTimeStamp: unsafe extern "C" fn(
        DriverRefPtr,
        AudioObjectID,
        u32,
        *mut f64,
        *mut u64,
        *mut u64,
    ) -> OSStatus,
    pub WillDoIOOperation: unsafe extern "C" fn(
        DriverRefPtr,
        AudioObjectID,
        u32,
        u32,
        *mut Boolean,
        *mut Boolean,
    ) -> OSStatus,
    pub BeginIOOperation: unsafe extern "C" fn(
        DriverRefPtr,
        AudioObjectID,
        u32,
        u32,
        u32,
        *const IOCycleInfo,
    ) -> OSStatus,
    pub DoIOOperation: unsafe extern "C" fn(
        DriverRefPtr,
        AudioObjectID,
        AudioObjectID,
        u32,
        u32,
        u32,
        *const IOCycleInfo,
        *mut c_void,
        *mut c_void,
    ) -> OSStatus,
    pub EndIOOperation: unsafe extern "C" fn(
        DriverRefPtr,
        AudioObjectID,
        u32,
        u32,
        u32,
        *const IOCycleInfo,
    ) -> OSStatus,
}

// The interface table is immutable after construction.
unsafe impl Sync for DriverInterface {}

/// What the host holds: a pointer to this is an `AudioServerPlugInDriverRef`.
#[repr(transparent)]
pub struct DriverRef(pub &'static DriverInterface);

pub const kAudioServerPlugInTypeUUID: [u8; 16] = [
    0x44, 0x3A, 0xBA, 0xB8, 0xE7, 0xB3, 0x49, 0x1A, 0xB9, 0x85, 0xBE, 0xB9, 0x18, 0x70, 0x30, 0xDB,
];
pub const kAudioServerPlugInDriverInterfaceUUID: [u8; 16] = [
    0xEE, 0xA5, 0x77, 0x3D, 0xCC, 0x43, 0x49, 0xF1, 0x8E, 0x00, 0x8F, 0x96, 0xE7, 0xD2, 0x3B, 0x17,
];
pub const IUnknownUUID: [u8; 16] = [
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46,
];

pub const S_OK: HRESULT = 0;
pub const E_NOINTERFACE: HRESULT = 0x8000_0004_u32 as i32;

pub const kAudioObjectPlugInObject: AudioObjectID = 1;
pub const kAudioObjectUnknown: AudioObjectID = 0;

pub const kAudioObjectClassID: u32 = fourcc(b"aobj");
pub const kAudioPlugInClassID: u32 = fourcc(b"aplg");

pub const kAudioObjectPropertyBaseClass: u32 = fourcc(b"bcls");
pub const kAudioObjectPropertyClass: u32 = fourcc(b"clas");
pub const kAudioObjectPropertyOwner: u32 = fourcc(b"stdv");
pub const kAudioObjectPropertyManufacturer: u32 = fourcc(b"lmak");
pub const kAudioObjectPropertyOwnedObjects: u32 = fourcc(b"ownd");
pub const kAudioPlugInPropertyBoxList: u32 = fourcc(b"box#");
pub const kAudioPlugInPropertyTranslateUIDToBox: u32 = fourcc(b"uidb");
pub const kAudioPlugInPropertyDeviceList: u32 = fourcc(b"dev#");
pub const kAudioPlugInPropertyTranslateUIDToDevice: u32 = fourcc(b"uidd");
pub const kAudioPlugInPropertyResourceBundle: u32 = fourcc(b"rsrc");

pub const kAudioHardwareUnknownPropertyError: OSStatus = fourcc(b"who?") as i32;
pub const kAudioHardwareBadPropertySizeError: OSStatus = fourcc(b"!siz") as i32;
pub const kAudioHardwareIllegalOperationError: OSStatus = fourcc(b"nope") as i32;
pub const kAudioHardwareBadObjectError: OSStatus = fourcc(b"!obj") as i32;
pub const kAudioHardwareUnsupportedOperationError: OSStatus = fourcc(b"unop") as i32;

pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;

unsafe extern "C" {
    pub fn CFUUIDGetUUIDBytes(uuid: CFUUIDRef) -> CFUUIDBytes;
    pub fn CFStringCreateWithCString(
        alloc: CFAllocatorRef,
        cstr: *const std::ffi::c_char,
        encoding: u32,
    ) -> CFStringRef;
}

/// A +1 CFString (the host releases strings returned as property data).
pub fn cfstring(s: &str) -> CFStringRef {
    let c = std::ffi::CString::new(s).unwrap_or_default();
    unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), kCFStringEncodingUTF8) }
}

// Sizes from the SDK, as checked by bundle/abi_check.c.
const _: () = assert!(std::mem::size_of::<DriverInterface>() == 23 * std::mem::size_of::<usize>());
const _: () = assert!(std::mem::size_of::<AudioObjectPropertyAddress>() == 12);
const _: () = assert!(std::mem::size_of::<AudioTimeStamp>() == 64);
const _: () = assert!(std::mem::size_of::<ClientInfo>() == 24);
const _: () = assert!(std::mem::size_of::<IOCycleInfo>() == 224);
