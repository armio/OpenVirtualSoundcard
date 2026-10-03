//! The CFPlugIn entry: the factory named in Info.plist, the driver object
//! the HAL holds, and the vtable of 23 entry points.
//!
//! Every entry point checks the HAL's pointers, then runs under
//! `ffi_guard`, so no panic can unwind into the HAL (design D13).

use crate::atomic::update_u32;
use std::ffi::c_void;
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use crate::abi::*;
use crate::driver::Driver;
use crate::ffi::ffi_guard;
use crate::link::{ClientTransport, production_link_factory};
use crate::platform::{self, LOG_DEFAULT, LOG_ERROR, Platform};

/// Makes the transport the driver's link uses to reach the daemon.
pub type LinkFactory = Box<dyn Fn() -> Arc<dyn ClientTransport> + Send + Sync>;

/// What an `AudioServerPlugInDriverRef` points at: the vtable pointer comes
/// first, as the HAL expects, followed by the reference count and the
/// driver. Objects are never freed.
#[repr(C)]
pub struct DriverObject {
    vtable: &'static DriverInterface,
    refcount: AtomicU32,
    pub(crate) driver: Driver,
}

impl DriverObject {
    fn leak(
        platform: &'static dyn Platform,
        link_factory: LinkFactory,
        quiet: bool,
    ) -> *mut c_void {
        let object = DriverObject {
            vtable: &DRIVER_INTERFACE,
            refcount: AtomicU32::new(1),
            driver: Driver::new(platform, link_factory, quiet),
        };
        Box::into_raw(Box::new(object)) as *mut c_void
    }

    fn add_ref(&self) -> ULONG {
        let prev = update_u32(&self.refcount, Ordering::SeqCst, Ordering::SeqCst, |n| {
            Some(n.saturating_add(1))
        });
        prev.unwrap_or(u32::MAX).saturating_add(1)
    }

    fn release(&self) -> ULONG {
        match update_u32(&self.refcount, Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)) {
            Ok(prev) => prev - 1,
            Err(_) => 0,
        }
    }
}

/// The driver object behind a reference from the HAL, or `None` for null.
///
/// # Safety
/// `this` must be null or come from [`OpenVirtualSoundcard_Factory`] or
/// [`new_driver_object`].
pub(crate) unsafe fn object<'a>(this: *mut c_void) -> Option<&'a DriverObject> {
    unsafe { (this as *const DriverObject).as_ref() }
}

/// Runs `f` on the driver behind `this`: `missing` for a null reference,
/// `on_panic` (and a faulted driver) if `f` panics. Driver objects are never
/// freed, so the driver lives for the rest of the process.
///
/// # Safety
/// As for [`object`].
unsafe fn with_driver<T>(
    this: *mut c_void,
    missing: T,
    on_panic: T,
    f: impl FnOnce(&'static Driver) -> T,
) -> T {
    match unsafe { object(this) } {
        Some(o) => ffi_guard(&o.driver.faulted, on_panic, || f(&o.driver)),
        None => missing,
    }
}

/// A byte view of a buffer the HAL passed as (size, pointer).
///
/// # Safety
/// `data` must be null or valid for `size` bytes for `'a`.
unsafe fn bytes<'a>(size: u32, data: *const c_void) -> &'a [u8] {
    if data.is_null() || size == 0 {
        &[]
    } else {
        unsafe { slice::from_raw_parts(data as *const u8, size as usize) }
    }
}

static FACTORY_FAULTED: AtomicBool = AtomicBool::new(false);
static FACTORY_INSTANCE: OnceLock<Instance> = OnceLock::new();

/// The factory's driver object, leaked.
struct Instance(*mut c_void);

// SAFETY: a driver object is shared between the HAL's threads anyway; all of
// its state is thread-safe.
unsafe impl Send for Instance {}
// SAFETY: as above.
unsafe impl Sync for Instance {}

/// The CFPlugIn factory named in Info.plist. For the AudioServerPlugIn type
/// it returns the process's one driver object, created on the first call
/// and retained once per call; for any other type, null.
///
/// # Safety
/// `requested_type` must be null or a live CFUUID.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn OpenVirtualSoundcard_Factory(
    _allocator: CFAllocatorRef,
    requested_type: CFUUIDRef,
) -> *mut c_void {
    let platform = platform::production();
    ffi_guard(&FACTORY_FAULTED, ptr::null_mut(), || {
        platform.log(LOG_DEFAULT, "factory called");
        if platform.uuid_bytes(requested_type) != kAudioServerPlugInTypeUUID {
            platform.log(LOG_ERROR, "factory: not the AudioServerPlugIn type");
            return ptr::null_mut();
        }
        let mut created = false;
        let this = FACTORY_INSTANCE
            .get_or_init(|| {
                created = true;
                Instance(DriverObject::leak(platform, production_link_factory(), true))
            })
            .0;
        if !created {
            // SAFETY: the instance is leaked, so it lives forever.
            if let Some(o) = unsafe { object(this) } {
                o.add_ref();
            }
        }
        this
    })
}

/// A new, independent driver object with a reference count of 1, as the
/// factory would return, on the given platform and link transport. For
/// tests and the self-test; the object is never freed.
pub fn new_driver_object(
    platform: &'static dyn Platform,
    link_factory: LinkFactory,
) -> *mut c_void {
    DriverObject::leak(platform, link_factory, false)
}

unsafe extern "C" fn query_interface(
    this: *mut c_void,
    uuid: CFUUIDBytes,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(o) = (unsafe { object(this) }) else {
        return E_NOINTERFACE;
    };
    ffi_guard(&o.driver.faulted, E_NOINTERFACE, || {
        if out.is_null() {
            return kAudioHardwareIllegalOperationError;
        }
        if uuid.0 == IUnknownUUID || uuid.0 == kAudioServerPlugInDriverInterfaceUUID {
            o.add_ref();
            unsafe { *out = this };
            S_OK
        } else {
            unsafe { *out = ptr::null_mut() };
            E_NOINTERFACE
        }
    })
}

unsafe extern "C" fn add_ref(this: *mut c_void) -> ULONG {
    match unsafe { object(this) } {
        Some(o) => ffi_guard(&o.driver.faulted, 0, || o.add_ref()),
        None => 0,
    }
}

unsafe extern "C" fn release(this: *mut c_void) -> ULONG {
    match unsafe { object(this) } {
        Some(o) => ffi_guard(&o.driver.faulted, 0, || o.release()),
        None => 0,
    }
}

unsafe extern "C" fn initialize(this: DriverRefPtr, host: HostRef) -> OSStatus {
    unsafe {
        with_driver(this, kAudioHardwareBadObjectError, kAudioHardwareUnspecifiedError, |d| {
            d.initialize(host)
        })
    }
}

unsafe extern "C" fn create_device(
    this: DriverRefPtr,
    _description: CFDictionaryRef,
    _client: *const ClientInfo,
    _out: *mut AudioObjectID,
) -> OSStatus {
    unsafe {
        with_driver(this, kAudioHardwareBadObjectError, kAudioHardwareUnspecifiedError, |d| {
            d.create_device()
        })
    }
}

unsafe extern "C" fn destroy_device(this: DriverRefPtr, device: AudioObjectID) -> OSStatus {
    unsafe {
        with_driver(this, kAudioHardwareBadObjectError, kAudioHardwareUnspecifiedError, |d| {
            d.destroy_device(device)
        })
    }
}

unsafe extern "C" fn add_device_client(
    this: DriverRefPtr,
    device: AudioObjectID,
    _client: *const ClientInfo,
) -> OSStatus {
    unsafe {
        with_driver(this, kAudioHardwareBadObjectError, kAudioHardwareUnspecifiedError, |d| {
            d.add_device_client(device)
        })
    }
}

unsafe extern "C" fn remove_device_client(
    this: DriverRefPtr,
    device: AudioObjectID,
    _client: *const ClientInfo,
) -> OSStatus {
    unsafe {
        with_driver(this, kAudioHardwareBadObjectError, kAudioHardwareUnspecifiedError, |d| {
            d.remove_device_client(device)
        })
    }
}

unsafe extern "C" fn perform_config_change(
    this: DriverRefPtr,
    device: AudioObjectID,
    action: u64,
    _info: *mut c_void,
) -> OSStatus {
    unsafe {
        with_driver(this, kAudioHardwareBadObjectError, kAudioHardwareUnspecifiedError, |d| {
            d.perform_config_change(device, action)
        })
    }
}

unsafe extern "C" fn abort_config_change(
    this: DriverRefPtr,
    device: AudioObjectID,
    action: u64,
    _info: *mut c_void,
) -> OSStatus {
    unsafe {
        with_driver(this, kAudioHardwareBadObjectError, kAudioHardwareUnspecifiedError, |d| {
            d.abort_config_change(device, action)
        })
    }
}

unsafe extern "C" fn has_property(
    this: DriverRefPtr,
    object: AudioObjectID,
    _pid: pid_t,
    address: *const AudioObjectPropertyAddress,
) -> Boolean {
    unsafe {
        with_driver(this, 0, 0, |d| match address.as_ref() {
            Some(a) => d.has_property(object, a) as Boolean,
            None => 0,
        })
    }
}

unsafe extern "C" fn is_property_settable(
    this: DriverRefPtr,
    object: AudioObjectID,
    _pid: pid_t,
    address: *const AudioObjectPropertyAddress,
    out: *mut Boolean,
) -> OSStatus {
    unsafe {
        with_driver(this, kAudioHardwareBadObjectError, kAudioHardwareUnspecifiedError, |d| {
            let Some(a) = address.as_ref() else {
                return kAudioHardwareIllegalOperationError;
            };
            if out.is_null() {
                return kAudioHardwareIllegalOperationError;
            }
            match d.is_property_settable(object, a) {
                Ok(settable) => {
                    *out = settable as Boolean;
                    kAudioHardwareNoError
                }
                Err(e) => e,
            }
        })
    }
}

unsafe extern "C" fn get_property_data_size(
    this: DriverRefPtr,
    object: AudioObjectID,
    _pid: pid_t,
    address: *const AudioObjectPropertyAddress,
    qualifier_size: u32,
    qualifier: *const c_void,
    out_size: *mut u32,
) -> OSStatus {
    unsafe {
        with_driver(this, kAudioHardwareBadObjectError, kAudioHardwareUnspecifiedError, |d| {
            let Some(a) = address.as_ref() else {
                return kAudioHardwareIllegalOperationError;
            };
            if out_size.is_null() {
                return kAudioHardwareIllegalOperationError;
            }
            let q = d.qualifier(a.mSelector, bytes(qualifier_size, qualifier));
            match d.property_data_size(object, a, &q) {
                Ok(size) => {
                    *out_size = size;
                    kAudioHardwareNoError
                }
                Err(e) => e,
            }
        })
    }
}

unsafe extern "C" fn get_property_data(
    this: DriverRefPtr,
    object: AudioObjectID,
    _pid: pid_t,
    address: *const AudioObjectPropertyAddress,
    qualifier_size: u32,
    qualifier: *const c_void,
    data_size: u32,
    out_size: *mut u32,
    out: *mut c_void,
) -> OSStatus {
    unsafe {
        with_driver(this, kAudioHardwareBadObjectError, kAudioHardwareUnspecifiedError, |d| {
            let Some(a) = address.as_ref() else {
                return kAudioHardwareIllegalOperationError;
            };
            if out_size.is_null() || (out.is_null() && data_size > 0) {
                return kAudioHardwareIllegalOperationError;
            }
            let q = d.qualifier(a.mSelector, bytes(qualifier_size, qualifier));
            let buf: &mut [u8] = if out.is_null() {
                &mut []
            } else {
                slice::from_raw_parts_mut(out as *mut u8, data_size as usize)
            };
            match d.property_data(object, a, &q, buf) {
                Ok(size) => {
                    *out_size = size;
                    kAudioHardwareNoError
                }
                Err(e) => e,
            }
        })
    }
}

unsafe extern "C" fn set_property_data(
    this: DriverRefPtr,
    object: AudioObjectID,
    _pid: pid_t,
    address: *const AudioObjectPropertyAddress,
    _qualifier_size: u32,
    _qualifier: *const c_void,
    data_size: u32,
    data: *const c_void,
) -> OSStatus {
    unsafe {
        with_driver(this, kAudioHardwareBadObjectError, kAudioHardwareUnspecifiedError, |d| {
            let Some(a) = address.as_ref() else {
                return kAudioHardwareIllegalOperationError;
            };
            if data.is_null() && data_size > 0 {
                return kAudioHardwareIllegalOperationError;
            }
            d.set_property_data(object, a, bytes(data_size, data))
        })
    }
}

unsafe extern "C" fn start_io(this: DriverRefPtr, device: AudioObjectID, _client: u32) -> OSStatus {
    unsafe {
        with_driver(this, kAudioHardwareBadObjectError, kAudioHardwareUnspecifiedError, |d| {
            d.start_io(device)
        })
    }
}

unsafe extern "C" fn stop_io(this: DriverRefPtr, device: AudioObjectID, _client: u32) -> OSStatus {
    unsafe {
        with_driver(this, kAudioHardwareBadObjectError, kAudioHardwareUnspecifiedError, |d| {
            d.stop_io(device)
        })
    }
}

/// On a panic, GetZeroTimeStamp hands out the previous time stamp again.
unsafe extern "C" fn get_zero_time_stamp(
    this: DriverRefPtr,
    device: AudioObjectID,
    _client: u32,
    sample_time: *mut f64,
    host_time: *mut u64,
    seed: *mut u64,
) -> OSStatus {
    let Some(o) = (unsafe { object(this) }) else {
        return kAudioHardwareBadObjectError;
    };
    if sample_time.is_null() || host_time.is_null() || seed.is_null() {
        return kAudioHardwareIllegalOperationError;
    }
    let d = &o.driver;
    let zts = match ffi_guard(&d.faulted, None, || Some(d.zero_timestamp(device))) {
        Some(Ok(zts)) => zts,
        Some(Err(e)) => return e,
        None => d.last_zero_timestamp(),
    };
    unsafe {
        *sample_time = zts.0;
        *host_time = zts.1;
        *seed = zts.2;
    }
    kAudioHardwareNoError
}

unsafe extern "C" fn will_do_io_operation(
    this: DriverRefPtr,
    device: AudioObjectID,
    _client: u32,
    operation: u32,
    will_do: *mut Boolean,
    in_place: *mut Boolean,
) -> OSStatus {
    unsafe {
        with_driver(this, kAudioHardwareBadObjectError, kAudioHardwareUnspecifiedError, |d| {
            if will_do.is_null() || in_place.is_null() {
                return kAudioHardwareIllegalOperationError;
            }
            match d.will_do_io(device, operation) {
                Ok((w, i)) => {
                    *will_do = w as Boolean;
                    *in_place = i as Boolean;
                    kAudioHardwareNoError
                }
                Err(e) => e,
            }
        })
    }
}

unsafe extern "C" fn begin_end_io_operation(
    this: DriverRefPtr,
    device: AudioObjectID,
    _client: u32,
    _operation: u32,
    _frames: u32,
    _cycle: *const IOCycleInfo,
) -> OSStatus {
    unsafe {
        with_driver(this, kAudioHardwareBadObjectError, kAudioHardwareUnspecifiedError, |d| {
            d.begin_end_io(device)
        })
    }
}

/// On a panic, DoIOOperation returns 0 with a silent input buffer.
unsafe extern "C" fn do_io_operation(
    this: DriverRefPtr,
    device: AudioObjectID,
    stream: AudioObjectID,
    _client: u32,
    operation: u32,
    frames: u32,
    cycle: *const IOCycleInfo,
    main: *mut c_void,
    _secondary: *mut c_void,
) -> OSStatus {
    let Some(o) = (unsafe { object(this) }) else {
        return kAudioHardwareBadObjectError;
    };
    let Some(cycle) = (unsafe { cycle.as_ref() }) else {
        return kAudioHardwareIllegalOperationError;
    };
    let d = &o.driver;
    // SAFETY: the HAL passes the buffer for this operation and stream.
    let status = ffi_guard(&d.faulted, None, || {
        Some(unsafe { d.do_io(device, stream, operation, frames, cycle, main) })
    });
    status.unwrap_or_else(|| {
        ffi_guard(&d.faulted, (), || unsafe { d.silence(operation, frames, main) });
        kAudioHardwareNoError
    })
}

static DRIVER_INTERFACE: DriverInterface = DriverInterface {
    _reserved: ptr::null_mut(),
    QueryInterface: query_interface,
    AddRef: add_ref,
    Release: release,
    Initialize: initialize,
    CreateDevice: create_device,
    DestroyDevice: destroy_device,
    AddDeviceClient: add_device_client,
    RemoveDeviceClient: remove_device_client,
    PerformDeviceConfigurationChange: perform_config_change,
    AbortDeviceConfigurationChange: abort_config_change,
    HasProperty: has_property,
    IsPropertySettable: is_property_settable,
    GetPropertyDataSize: get_property_data_size,
    GetPropertyData: get_property_data,
    SetPropertyData: set_property_data,
    StartIO: start_io,
    StopIO: stop_io,
    GetZeroTimeStamp: get_zero_time_stamp,
    WillDoIOOperation: will_do_io_operation,
    BeginIOOperation: begin_end_io_operation,
    DoIOOperation: do_io_operation,
    EndIOOperation: begin_end_io_operation,
};
