//! OvscProbe.driver: a Core Audio server plug-in that publishes no
//! device. When the host loads it, it runs `probe::run` on a background
//! thread and reports which channels to a separate daemon the driver
//! sandbox allows.
//!
//! It is built as a static library and linked into an MH_BUNDLE with clang,
//! the same way the real OpenVirtualSoundcard driver is built, so a successful load also
//! shows that a Rust-built, ad-hoc signed bundle loads.

#![cfg(target_os = "macos")]
#![allow(non_upper_case_globals, non_snake_case)]

mod abi;
mod probe;

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use abi::*;

static REFS: AtomicU32 = AtomicU32::new(0);
static HOST: Mutex<usize> = Mutex::new(0);
static PROBE_STARTED: OnceLock<()> = OnceLock::new();

/// The CFPlugIn factory named in Info.plist.
///
/// # Safety
/// Called by CFPlugIn with a valid UUID.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn OvscProbe_Create(
    _allocator: CFAllocatorRef,
    requested_type: CFUUIDRef,
) -> *mut c_void {
    probe::log("factory called");
    if requested_type.is_null() {
        return std::ptr::null_mut();
    }
    let bytes = unsafe { CFUUIDGetUUIDBytes(requested_type) };
    if bytes.0 != kAudioServerPlugInTypeUUID {
        probe::log("factory: not the AudioServerPlugIn type");
        return std::ptr::null_mut();
    }
    REFS.fetch_add(1, Ordering::SeqCst);
    driver_ref()
}

fn driver_ref() -> *mut c_void {
    &DRIVER_REF as *const DriverRef as *mut c_void
}

unsafe extern "C" fn query_interface(
    _driver: *mut c_void,
    uuid: CFUUIDBytes,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return kAudioHardwareIllegalOperationError;
    }
    if uuid.0 == IUnknownUUID || uuid.0 == kAudioServerPlugInDriverInterfaceUUID {
        REFS.fetch_add(1, Ordering::SeqCst);
        unsafe { *out = driver_ref() };
        S_OK
    } else {
        unsafe { *out = std::ptr::null_mut() };
        E_NOINTERFACE
    }
}

unsafe extern "C" fn add_ref(_driver: *mut c_void) -> ULONG {
    REFS.fetch_add(1, Ordering::SeqCst) + 1
}

unsafe extern "C" fn release(_driver: *mut c_void) -> ULONG {
    let prev = REFS.load(Ordering::SeqCst);
    if prev == 0 {
        return 0;
    }
    REFS.fetch_sub(1, Ordering::SeqCst) - 1
}

unsafe extern "C" fn initialize(_driver: DriverRefPtr, host: HostRef) -> OSStatus {
    *HOST.lock().unwrap() = host as usize;
    probe::log("Initialize called");
    PROBE_STARTED.get_or_init(|| {
        std::thread::Builder::new()
            .name("ovprobe".into())
            .spawn(probe::run)
            .map(drop)
            .unwrap_or_else(|e| probe::log(&format!("cannot spawn probe thread: {e}")));
    });
    0
}

unsafe extern "C" fn create_device(
    _driver: DriverRefPtr,
    _description: CFDictionaryRef,
    _client: *const ClientInfo,
    _out: *mut AudioObjectID,
) -> OSStatus {
    kAudioHardwareUnsupportedOperationError
}

unsafe extern "C" fn destroy_device(_driver: DriverRefPtr, _device: AudioObjectID) -> OSStatus {
    kAudioHardwareUnsupportedOperationError
}

unsafe extern "C" fn device_client(
    _driver: DriverRefPtr,
    _device: AudioObjectID,
    _client: *const ClientInfo,
) -> OSStatus {
    kAudioHardwareBadObjectError
}

unsafe extern "C" fn configuration_change(
    _driver: DriverRefPtr,
    _device: AudioObjectID,
    _action: u64,
    _info: *mut c_void,
) -> OSStatus {
    kAudioHardwareBadObjectError
}

fn plugin_has(selector: u32) -> bool {
    matches!(
        selector,
        kAudioObjectPropertyBaseClass
            | kAudioObjectPropertyClass
            | kAudioObjectPropertyOwner
            | kAudioObjectPropertyManufacturer
            | kAudioObjectPropertyOwnedObjects
            | kAudioPlugInPropertyBoxList
            | kAudioPlugInPropertyTranslateUIDToBox
            | kAudioPlugInPropertyDeviceList
            | kAudioPlugInPropertyTranslateUIDToDevice
            | kAudioPlugInPropertyResourceBundle
    )
}

unsafe extern "C" fn has_property(
    _driver: DriverRefPtr,
    object: AudioObjectID,
    _pid: libc::pid_t,
    address: *const AudioObjectPropertyAddress,
) -> Boolean {
    if object != kAudioObjectPlugInObject || address.is_null() {
        return 0;
    }
    plugin_has(unsafe { (*address).mSelector }) as Boolean
}

unsafe extern "C" fn is_property_settable(
    _driver: DriverRefPtr,
    object: AudioObjectID,
    _pid: libc::pid_t,
    address: *const AudioObjectPropertyAddress,
    out: *mut Boolean,
) -> OSStatus {
    if object != kAudioObjectPlugInObject {
        return kAudioHardwareBadObjectError;
    }
    if address.is_null() || out.is_null() {
        return kAudioHardwareIllegalOperationError;
    }
    if !plugin_has(unsafe { (*address).mSelector }) {
        return kAudioHardwareUnknownPropertyError;
    }
    unsafe { *out = 0 };
    0
}

fn plugin_size(selector: u32) -> Option<u32> {
    Some(match selector {
        kAudioObjectPropertyBaseClass | kAudioObjectPropertyClass => 4,
        kAudioObjectPropertyOwner => 4,
        kAudioObjectPropertyManufacturer | kAudioPlugInPropertyResourceBundle => {
            std::mem::size_of::<CFStringRef>() as u32
        }
        kAudioObjectPropertyOwnedObjects
        | kAudioPlugInPropertyBoxList
        | kAudioPlugInPropertyDeviceList => 0,
        kAudioPlugInPropertyTranslateUIDToBox | kAudioPlugInPropertyTranslateUIDToDevice => 4,
        _ => return None,
    })
}

unsafe extern "C" fn get_property_data_size(
    _driver: DriverRefPtr,
    object: AudioObjectID,
    _pid: libc::pid_t,
    address: *const AudioObjectPropertyAddress,
    _qualifier_size: u32,
    _qualifier: *const c_void,
    out_size: *mut u32,
) -> OSStatus {
    if object != kAudioObjectPlugInObject {
        return kAudioHardwareBadObjectError;
    }
    if address.is_null() || out_size.is_null() {
        return kAudioHardwareIllegalOperationError;
    }
    match plugin_size(unsafe { (*address).mSelector }) {
        Some(size) => {
            unsafe { *out_size = size };
            0
        }
        None => kAudioHardwareUnknownPropertyError,
    }
}

unsafe extern "C" fn get_property_data(
    _driver: DriverRefPtr,
    object: AudioObjectID,
    _pid: libc::pid_t,
    address: *const AudioObjectPropertyAddress,
    _qualifier_size: u32,
    _qualifier: *const c_void,
    data_size: u32,
    out_size: *mut u32,
    out: *mut c_void,
) -> OSStatus {
    if object != kAudioObjectPlugInObject {
        return kAudioHardwareBadObjectError;
    }
    if address.is_null() || out_size.is_null() {
        return kAudioHardwareIllegalOperationError;
    }
    let selector = unsafe { (*address).mSelector };
    let Some(size) = plugin_size(selector) else {
        return kAudioHardwareUnknownPropertyError;
    };
    if size == 0 {
        unsafe { *out_size = 0 };
        return 0;
    }
    if data_size < size || out.is_null() {
        return kAudioHardwareBadPropertySizeError;
    }
    unsafe {
        match selector {
            kAudioObjectPropertyBaseClass => *(out as *mut u32) = kAudioObjectClassID,
            kAudioObjectPropertyClass => *(out as *mut u32) = kAudioPlugInClassID,
            kAudioObjectPropertyOwner
            | kAudioPlugInPropertyTranslateUIDToBox
            | kAudioPlugInPropertyTranslateUIDToDevice => *(out as *mut u32) = kAudioObjectUnknown,
            kAudioObjectPropertyManufacturer => *(out as *mut CFStringRef) = cfstring("OpenVirtualSoundcard"),
            kAudioPlugInPropertyResourceBundle => *(out as *mut CFStringRef) = cfstring(""),
            _ => return kAudioHardwareUnknownPropertyError,
        }
        *out_size = size;
    }
    0
}

unsafe extern "C" fn set_property_data(
    _driver: DriverRefPtr,
    object: AudioObjectID,
    _pid: libc::pid_t,
    _address: *const AudioObjectPropertyAddress,
    _qualifier_size: u32,
    _qualifier: *const c_void,
    _data_size: u32,
    _data: *const c_void,
) -> OSStatus {
    if object != kAudioObjectPlugInObject {
        return kAudioHardwareBadObjectError;
    }
    kAudioHardwareUnknownPropertyError
}

unsafe extern "C" fn io_start_stop(
    _driver: DriverRefPtr,
    _device: AudioObjectID,
    _client: u32,
) -> OSStatus {
    kAudioHardwareBadObjectError
}

unsafe extern "C" fn get_zero_time_stamp(
    _driver: DriverRefPtr,
    _device: AudioObjectID,
    _client: u32,
    _sample_time: *mut f64,
    _host_time: *mut u64,
    _seed: *mut u64,
) -> OSStatus {
    kAudioHardwareBadObjectError
}

unsafe extern "C" fn will_do_io_operation(
    _driver: DriverRefPtr,
    _device: AudioObjectID,
    _client: u32,
    _operation: u32,
    _will_do: *mut Boolean,
    _in_place: *mut Boolean,
) -> OSStatus {
    kAudioHardwareBadObjectError
}

unsafe extern "C" fn begin_end_io_operation(
    _driver: DriverRefPtr,
    _device: AudioObjectID,
    _client: u32,
    _operation: u32,
    _frames: u32,
    _cycle: *const IOCycleInfo,
) -> OSStatus {
    kAudioHardwareBadObjectError
}

unsafe extern "C" fn do_io_operation(
    _driver: DriverRefPtr,
    _device: AudioObjectID,
    _stream: AudioObjectID,
    _client: u32,
    _operation: u32,
    _frames: u32,
    _cycle: *const IOCycleInfo,
    _main: *mut c_void,
    _secondary: *mut c_void,
) -> OSStatus {
    kAudioHardwareBadObjectError
}

static INTERFACE: DriverInterface = DriverInterface {
    _reserved: std::ptr::null_mut(),
    QueryInterface: query_interface,
    AddRef: add_ref,
    Release: release,
    Initialize: initialize,
    CreateDevice: create_device,
    DestroyDevice: destroy_device,
    AddDeviceClient: device_client,
    RemoveDeviceClient: device_client,
    PerformDeviceConfigurationChange: configuration_change,
    AbortDeviceConfigurationChange: configuration_change,
    HasProperty: has_property,
    IsPropertySettable: is_property_settable,
    GetPropertyDataSize: get_property_data_size,
    GetPropertyData: get_property_data,
    SetPropertyData: set_property_data,
    StartIO: io_start_stop,
    StopIO: io_start_stop,
    GetZeroTimeStamp: get_zero_time_stamp,
    WillDoIOOperation: will_do_io_operation,
    BeginIOOperation: begin_end_io_operation,
    DoIOOperation: do_io_operation,
    EndIOOperation: begin_end_io_operation,
};

static DRIVER_REF: DriverRef = DriverRef(&INTERFACE);
