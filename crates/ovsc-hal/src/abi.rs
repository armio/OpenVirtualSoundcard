//! The subset of the AudioServerPlugIn C ABI the driver uses.
//!
//! Copied from `tools/macos-probe/plugin/src/abi.rs` and extended. Every
//! constant, size and offset is checked against the SDK headers by
//! `abi_check.c` (`clang -fsyntax-only abi_check.c`), and
//! `tests/abi_consistency.rs` checks that every four-character code here has
//! a line there. Names follow the C headers. Nothing here calls into Apple
//! code, so the module builds on every OS.

#![allow(non_camel_case_types, non_snake_case, clippy::upper_case_acronyms)]

use std::ffi::c_void;
use std::mem::{offset_of, size_of};

pub type OSStatus = i32;
pub type HRESULT = i32;
pub type ULONG = u32;
pub type Boolean = u8;
pub type AudioObjectID = u32;
/// `pid_t` is an `int` on every Apple platform; defined here so the ABI
/// mirror does not need `libc` on Windows.
pub type pid_t = i32;
pub type CFAllocatorRef = *const c_void;
pub type CFUUIDRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFPropertyListRef = *const c_void;
/// `AudioServerPlugInHostRef`: a pointer to the host's interface table.
pub type HostRef = *const AudioServerPlugInHostInterface;
/// `AudioServerPlugInDriverRef`: a pointer to a pointer to the interface.
pub type DriverRefPtr = *mut c_void;

/// The big-endian four-character code of `s`, as the C headers spell `'abcd'`.
pub const fn fourcc(s: &[u8; 4]) -> u32 {
    (s[0] as u32) << 24 | (s[1] as u32) << 16 | (s[2] as u32) << 8 | s[3] as u32
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CFUUIDBytes(pub [u8; 16]);

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioObjectPropertyAddress {
    pub mSelector: u32,
    pub mScope: u32,
    pub mElement: u32,
}

/// The name the driver's internal interfaces use for a property address.
pub type PropertyAddress = AudioObjectPropertyAddress;

/// `AudioServerPlugInClientInfo`.
#[repr(C)]
pub struct ClientInfo {
    pub mClientID: u32,
    pub mProcessID: pid_t,
    pub mIsNativeEndian: Boolean,
    pub mBundleID: CFStringRef,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
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
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AudioTimeStamp {
    pub mSampleTime: f64,
    pub mHostTime: u64,
    pub mRateScalar: f64,
    pub mWordClockTime: u64,
    pub mSMPTETime: SMPTETime,
    pub mFlags: u32,
    pub mReserved: u32,
}

/// `AudioServerPlugInIOCycleInfo`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
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
#[derive(Clone, Copy, Debug, Default, PartialEq)]
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
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AudioValueRange {
    pub mMinimum: f64,
    pub mMaximum: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AudioStreamRangedDescription {
    pub mFormat: AudioStreamBasicDescription,
    pub mSampleRateRange: AudioValueRange,
}

/// The fixed part of `AudioChannelLayout`. In C the struct ends with a
/// one-element `mChannelDescriptions` array; the descriptions follow this
/// header directly, `mNumberChannelDescriptions` of them.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioChannelLayoutHeader {
    pub mChannelLayoutTag: u32,
    pub mChannelBitmap: u32,
    pub mNumberChannelDescriptions: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AudioChannelDescription {
    pub mChannelLabel: u32,
    pub mChannelFlags: u32,
    pub mCoordinates: [f32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioServerPlugInCustomPropertyInfo {
    pub mSelector: u32,
    pub mPropertyDataType: u32,
    pub mQualifierDataType: u32,
}

/// The table the host passes to Initialize. Every entry is optional here
/// only so that reading a null slot is not undefined behaviour; the HAL fills
/// all of them.
#[repr(C)]
pub struct AudioServerPlugInHostInterface {
    pub PropertiesChanged: Option<
        unsafe extern "C" fn(
            HostRef,
            AudioObjectID,
            u32,
            *const AudioObjectPropertyAddress,
        ) -> OSStatus,
    >,
    pub CopyFromStorage:
        Option<unsafe extern "C" fn(HostRef, CFStringRef, *mut CFPropertyListRef) -> OSStatus>,
    pub WriteToStorage:
        Option<unsafe extern "C" fn(HostRef, CFStringRef, CFPropertyListRef) -> OSStatus>,
    pub DeleteFromStorage: Option<unsafe extern "C" fn(HostRef, CFStringRef) -> OSStatus>,
    pub RequestDeviceConfigurationChange:
        Option<unsafe extern "C" fn(HostRef, AudioObjectID, u64, *mut c_void) -> OSStatus>,
}

/// `AudioServerPlugInDriverInterface`: IUnknown followed by the driver
/// entries, 23 pointer-sized slots: the reserved one and 22 function
/// pointers.
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
        pid_t,
        *const AudioObjectPropertyAddress,
    ) -> Boolean,
    pub IsPropertySettable: unsafe extern "C" fn(
        DriverRefPtr,
        AudioObjectID,
        pid_t,
        *const AudioObjectPropertyAddress,
        *mut Boolean,
    ) -> OSStatus,
    pub GetPropertyDataSize: unsafe extern "C" fn(
        DriverRefPtr,
        AudioObjectID,
        pid_t,
        *const AudioObjectPropertyAddress,
        u32,
        *const c_void,
        *mut u32,
    ) -> OSStatus,
    pub GetPropertyData: unsafe extern "C" fn(
        DriverRefPtr,
        AudioObjectID,
        pid_t,
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
        pid_t,
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

pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;

// Objects.
pub const kAudioObjectUnknown: AudioObjectID = 0;
pub const kAudioObjectPlugInObject: AudioObjectID = 1;

// Classes.
pub const kAudioObjectClassID: u32 = fourcc(b"aobj");
pub const kAudioPlugInClassID: u32 = fourcc(b"aplg");
pub const kAudioDeviceClassID: u32 = fourcc(b"adev");
pub const kAudioStreamClassID: u32 = fourcc(b"astr");

// Scopes and elements.
pub const kAudioObjectPropertyScopeGlobal: u32 = fourcc(b"glob");
pub const kAudioObjectPropertyScopeInput: u32 = fourcc(b"inpt");
pub const kAudioObjectPropertyScopeOutput: u32 = fourcc(b"outp");
pub const kAudioObjectPropertyScopeWildcard: u32 = fourcc(b"****");
pub const kAudioObjectPropertySelectorWildcard: u32 = fourcc(b"****");
pub const kAudioObjectPropertyElementMain: u32 = 0;
pub const kAudioObjectPropertyElementWildcard: u32 = 0xFFFF_FFFF;

// Properties of every object.
pub const kAudioObjectPropertyBaseClass: u32 = fourcc(b"bcls");
pub const kAudioObjectPropertyClass: u32 = fourcc(b"clas");
pub const kAudioObjectPropertyOwner: u32 = fourcc(b"stdv");
pub const kAudioObjectPropertyName: u32 = fourcc(b"lnam");
pub const kAudioObjectPropertyManufacturer: u32 = fourcc(b"lmak");
pub const kAudioObjectPropertyElementName: u32 = fourcc(b"lchn");
pub const kAudioObjectPropertyOwnedObjects: u32 = fourcc(b"ownd");
pub const kAudioObjectPropertyControlList: u32 = fourcc(b"ctrl");
pub const kAudioObjectPropertyCustomPropertyInfoList: u32 = fourcc(b"cust");

// Plug-in properties.
pub const kAudioPlugInPropertyBoxList: u32 = fourcc(b"box#");
pub const kAudioPlugInPropertyTranslateUIDToBox: u32 = fourcc(b"uidb");
pub const kAudioPlugInPropertyDeviceList: u32 = fourcc(b"dev#");
pub const kAudioPlugInPropertyTranslateUIDToDevice: u32 = fourcc(b"uidd");
pub const kAudioPlugInPropertyResourceBundle: u32 = fourcc(b"rsrc");

// Device properties.
pub const kAudioDevicePropertyDeviceUID: u32 = fourcc(b"uid ");
pub const kAudioDevicePropertyModelUID: u32 = fourcc(b"muid");
pub const kAudioDevicePropertyTransportType: u32 = fourcc(b"tran");
pub const kAudioDevicePropertyRelatedDevices: u32 = fourcc(b"akin");
pub const kAudioDevicePropertyClockDomain: u32 = fourcc(b"clkd");
pub const kAudioDevicePropertyDeviceIsAlive: u32 = fourcc(b"livn");
pub const kAudioDevicePropertyDeviceIsRunning: u32 = fourcc(b"goin");
pub const kAudioDevicePropertyDeviceCanBeDefaultDevice: u32 = fourcc(b"dflt");
pub const kAudioDevicePropertyDeviceCanBeDefaultSystemDevice: u32 = fourcc(b"sflt");
pub const kAudioDevicePropertyLatency: u32 = fourcc(b"ltnc");
pub const kAudioDevicePropertyStreams: u32 = fourcc(b"stm#");
pub const kAudioDevicePropertySafetyOffset: u32 = fourcc(b"saft");
pub const kAudioDevicePropertyNominalSampleRate: u32 = fourcc(b"nsrt");
pub const kAudioDevicePropertyAvailableNominalSampleRates: u32 = fourcc(b"nsr#");
pub const kAudioDevicePropertyIsHidden: u32 = fourcc(b"hidn");
pub const kAudioDevicePropertyPreferredChannelsForStereo: u32 = fourcc(b"dch2");
pub const kAudioDevicePropertyPreferredChannelLayout: u32 = fourcc(b"srnd");
pub const kAudioDevicePropertyZeroTimeStampPeriod: u32 = fourcc(b"ring");
pub const kAudioDevicePropertyClockAlgorithm: u32 = fourcc(b"clok");
pub const kAudioDevicePropertyClockIsStable: u32 = fourcc(b"cstb");
// Declared only by the macOS 26 SDK, so they are literals here and their
// check in abi_check.c is guarded (design section 8.5).
pub const kAudioDevicePropertyWantsControlsRestored: u32 = fourcc(b"resc");
pub const kAudioDevicePropertyWantsStreamFormatsRestored: u32 = fourcc(b"resf");
// Synthesized by the HAL, not implemented by the driver (design section 9).
pub const kAudioDevicePropertyDeviceIsRunningSomewhere: u32 = fourcc(b"gone");
pub const kAudioDevicePropertyHogMode: u32 = fourcc(b"oink");
pub const kAudioDevicePropertyBufferFrameSize: u32 = fourcc(b"fsiz");
pub const kAudioDevicePropertyBufferFrameSizeRange: u32 = fourcc(b"fsz#");
pub const kAudioDevicePropertyActualSampleRate: u32 = fourcc(b"asrt");
pub const kAudioDevicePropertyIOThreadOSWorkgroup: u32 = fourcc(b"oswg");

// Device property values.
pub const kAudioDeviceTransportTypeVirtual: u32 = fourcc(b"virt");
pub const kAudioDeviceClockAlgorithmRaw: u32 = fourcc(b"raww");
pub const kAudioDeviceClockAlgorithmSimpleIIR: u32 = fourcc(b"iirf");
pub const kAudioDeviceClockAlgorithm12PtMovingWindowAverage: u32 = fourcc(b"mavg");

// Stream properties.
pub const kAudioStreamPropertyIsActive: u32 = fourcc(b"sact");
pub const kAudioStreamPropertyDirection: u32 = fourcc(b"sdir");
pub const kAudioStreamPropertyTerminalType: u32 = fourcc(b"term");
pub const kAudioStreamPropertyStartingChannel: u32 = fourcc(b"schn");
pub const kAudioStreamPropertyLatency: u32 = fourcc(b"ltnc");
pub const kAudioStreamPropertyVirtualFormat: u32 = fourcc(b"sfmt");
pub const kAudioStreamPropertyAvailableVirtualFormats: u32 = fourcc(b"sfma");
pub const kAudioStreamPropertyPhysicalFormat: u32 = fourcc(b"pft ");
pub const kAudioStreamPropertyAvailablePhysicalFormats: u32 = fourcc(b"pfta");
pub const kAudioStreamTerminalTypeLine: u32 = fourcc(b"line");

// Custom property data types.
pub const kAudioServerPlugInCustomPropertyDataTypeNone: u32 = 0;
pub const kAudioServerPlugInCustomPropertyDataTypeCFString: u32 = fourcc(b"cfst");
pub const kAudioServerPlugInCustomPropertyDataTypeCFPropertyList: u32 = fourcc(b"plst");

// Formats and channel layouts.
pub const kAudioFormatLinearPCM: u32 = fourcc(b"lpcm");
pub const kAudioFormatFlagIsFloat: u32 = 1 << 0;
pub const kAudioFormatFlagIsBigEndian: u32 = 1 << 1;
pub const kAudioFormatFlagIsPacked: u32 = 1 << 3;
pub const kAudioFormatFlagIsNonInterleaved: u32 = 1 << 5;
/// Float | Packed | native (little) endian: 9 on arm64 and x86_64.
pub const kAudioFormatFlagsNativeFloatPacked: u32 =
    kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked;
pub const kAudioChannelLayoutTag_UseChannelDescriptions: u32 = 0;
/// Discrete channel `i` is labelled `kAudioChannelLabel_Discrete_0 | i`.
pub const kAudioChannelLabel_Discrete_0: u32 = 1 << 16;
pub const kAudioChannelLabel_Unknown: u32 = 0xFFFF_FFFF;

// IO operations.
pub const kAudioServerPlugInIOOperationThread: u32 = fourcc(b"thrd");
pub const kAudioServerPlugInIOOperationCycle: u32 = fourcc(b"cycl");
pub const kAudioServerPlugInIOOperationReadInput: u32 = fourcc(b"read");
pub const kAudioServerPlugInIOOperationConvertInput: u32 = fourcc(b"cinp");
pub const kAudioServerPlugInIOOperationProcessInput: u32 = fourcc(b"pinp");
pub const kAudioServerPlugInIOOperationProcessOutput: u32 = fourcc(b"pout");
pub const kAudioServerPlugInIOOperationMixOutput: u32 = fourcc(b"mixo");
pub const kAudioServerPlugInIOOperationProcessMix: u32 = fourcc(b"pmix");
pub const kAudioServerPlugInIOOperationConvertMix: u32 = fourcc(b"cmix");
pub const kAudioServerPlugInIOOperationWriteMix: u32 = fourcc(b"rite");

// Errors.
pub const kAudioHardwareNoError: OSStatus = 0;
pub const kAudioHardwareNotRunningError: OSStatus = fourcc(b"stop") as i32;
pub const kAudioHardwareUnspecifiedError: OSStatus = fourcc(b"what") as i32;
pub const kAudioHardwareUnknownPropertyError: OSStatus = fourcc(b"who?") as i32;
pub const kAudioHardwareBadPropertySizeError: OSStatus = fourcc(b"!siz") as i32;
pub const kAudioHardwareIllegalOperationError: OSStatus = fourcc(b"nope") as i32;
pub const kAudioHardwareBadObjectError: OSStatus = fourcc(b"!obj") as i32;
pub const kAudioHardwareBadDeviceError: OSStatus = fourcc(b"!dev") as i32;
pub const kAudioHardwareBadStreamError: OSStatus = fourcc(b"!str") as i32;
pub const kAudioHardwareUnsupportedOperationError: OSStatus = fourcc(b"unop") as i32;
pub const kAudioHardwareNotReadyError: OSStatus = fourcc(b"nrdy") as i32;
pub const kAudioDeviceUnsupportedFormatError: OSStatus = fourcc(b"!dat") as i32;
pub const kAudioDevicePermissionsError: OSStatus = fourcc(b"!hog") as i32;

// Sizes and offsets from the SDK, as checked by abi_check.c.
const PTR: usize = size_of::<*const c_void>();
const _: () = assert!(size_of::<CFUUIDBytes>() == 16);
const _: () = assert!(size_of::<AudioObjectPropertyAddress>() == 12);
const _: () = assert!(size_of::<ClientInfo>() == 8 + 2 * PTR);
const _: () = assert!(offset_of!(ClientInfo, mIsNativeEndian) == 8);
const _: () = assert!(offset_of!(ClientInfo, mBundleID) == 8 + PTR);
const _: () = assert!(size_of::<SMPTETime>() == 24);
const _: () = assert!(size_of::<AudioTimeStamp>() == 64);
const _: () = assert!(offset_of!(AudioTimeStamp, mSMPTETime) == 32);
const _: () = assert!(offset_of!(AudioTimeStamp, mFlags) == 56);
const _: () = assert!(size_of::<IOCycleInfo>() == 224);
const _: () = assert!(offset_of!(IOCycleInfo, mCurrentTime) == 16);
const _: () = assert!(offset_of!(IOCycleInfo, mInputTime) == 80);
const _: () = assert!(offset_of!(IOCycleInfo, mOutputTime) == 144);
const _: () = assert!(offset_of!(IOCycleInfo, mMainHostTicksPerFrame) == 208);
const _: () = assert!(offset_of!(IOCycleInfo, mDeviceHostTicksPerFrame) == 216);
const _: () = assert!(size_of::<AudioStreamBasicDescription>() == 40);
const _: () = assert!(offset_of!(AudioStreamBasicDescription, mFormatID) == 8);
const _: () = assert!(offset_of!(AudioStreamBasicDescription, mReserved) == 36);
const _: () = assert!(size_of::<AudioValueRange>() == 16);
const _: () = assert!(size_of::<AudioStreamRangedDescription>() == 56);
const _: () = assert!(offset_of!(AudioStreamRangedDescription, mSampleRateRange) == 40);
const _: () = assert!(size_of::<AudioChannelLayoutHeader>() == 12);
const _: () = assert!(size_of::<AudioChannelDescription>() == 20);
const _: () = assert!(offset_of!(AudioChannelDescription, mCoordinates) == 8);
const _: () = assert!(size_of::<AudioServerPlugInCustomPropertyInfo>() == 12);
const _: () = assert!(size_of::<AudioServerPlugInHostInterface>() == 5 * PTR);
const _: () = assert!(
    offset_of!(AudioServerPlugInHostInterface, RequestDeviceConfigurationChange) == 4 * PTR
);
const _: () = assert!(size_of::<DriverInterface>() == 23 * PTR);
const _: () = assert!(offset_of!(DriverInterface, EndIOOperation) == 22 * PTR);
