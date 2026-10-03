//! Test support: a fake HAL host and hooks into driver objects.
//!
//! These run the driver through its extern "C" entry points without Core
//! Audio, on any OS, together with [`crate::platform::stub::StubPlatform`]
//! (the fake host reads and makes stub CoreFoundation objects).

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::abi::*;
use crate::entry::{LinkFactory, object};
use crate::link::{ClientTransport, NullTransport};
use crate::platform::stub;

/// One call the driver made on the host.
#[derive(Clone, Debug, PartialEq)]
pub enum HostCall {
    PropertiesChanged { object: AudioObjectID, addresses: Vec<AudioObjectPropertyAddress> },
    CopyFromStorage { key: String },
    WriteToStorage { key: String, value: Option<String> },
    DeleteFromStorage { key: String },
    RequestDeviceConfigurationChange { device: AudioObjectID, action: u64 },
}

#[derive(Default)]
struct FakeState {
    calls: Vec<HostCall>,
    storage: BTreeMap<String, String>,
    request_status: OSStatus,
}

/// A host for Initialize: the host interface table, followed by a record of
/// every call and an in-memory storage. CopyFromStorage of a missing key
/// returns 0 and null.
#[repr(C)]
pub struct FakeHost {
    interface: AudioServerPlugInHostInterface,
    state: Mutex<FakeState>,
}

impl FakeHost {
    /// A new host, leaked: a driver keeps its host for the life of the
    /// process.
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> &'static FakeHost {
        Box::leak(Box::new(FakeHost {
            interface: AudioServerPlugInHostInterface {
                PropertiesChanged: Some(properties_changed),
                CopyFromStorage: Some(copy_from_storage),
                WriteToStorage: Some(write_to_storage),
                DeleteFromStorage: Some(delete_from_storage),
                RequestDeviceConfigurationChange: Some(request_config_change),
            },
            state: Mutex::new(FakeState::default()),
        }))
    }

    /// The reference to pass to Initialize. It points at the interface, the
    /// first field, but is derived from the whole host so that the host
    /// callbacks may read the rest of it.
    pub fn host_ref(&'static self) -> HostRef {
        (self as *const FakeHost).cast()
    }

    fn state(&self) -> MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Every call so far, oldest first.
    pub fn calls(&self) -> Vec<HostCall> {
        self.state().calls.clone()
    }

    /// Every call so far, clearing the record.
    pub fn take_calls(&self) -> Vec<HostCall> {
        std::mem::take(&mut self.state().calls)
    }

    /// The text stored under `key`.
    pub fn storage(&self, key: &str) -> Option<String> {
        self.state().storage.get(key).cloned()
    }

    /// Stores `value` under `key`, as a previous run of the driver would have.
    pub fn set_storage(&self, key: &str, value: &str) {
        self.state().storage.insert(key.to_owned(), value.to_owned());
    }

    /// What RequestDeviceConfigurationChange returns (0 by default).
    pub fn set_request_status(&self, status: OSStatus) {
        self.state().request_status = status;
    }
}

/// The FakeHost behind a host reference.
///
/// # Safety
/// `host` must come from [`FakeHost::host_ref`].
unsafe fn fake<'a>(host: HostRef) -> Option<&'a FakeHost> {
    // The interface is the first field of the repr(C) FakeHost.
    unsafe { (host as *const FakeHost).as_ref() }
}

unsafe extern "C" fn properties_changed(
    host: HostRef,
    object: AudioObjectID,
    count: u32,
    addresses: *const AudioObjectPropertyAddress,
) -> OSStatus {
    let Some(h) = (unsafe { fake(host) }) else {
        return kAudioHardwareIllegalOperationError;
    };
    let addresses = if addresses.is_null() {
        Vec::new()
    } else {
        // SAFETY: the driver passes `count` addresses.
        unsafe { std::slice::from_raw_parts(addresses, count as usize) }.to_vec()
    };
    h.state().calls.push(HostCall::PropertiesChanged { object, addresses });
    kAudioHardwareNoError
}

unsafe extern "C" fn copy_from_storage(
    host: HostRef,
    key: CFStringRef,
    out: *mut CFPropertyListRef,
) -> OSStatus {
    let Some(h) = (unsafe { fake(host) }) else {
        return kAudioHardwareIllegalOperationError;
    };
    if out.is_null() {
        return kAudioHardwareIllegalOperationError;
    }
    let key = unsafe { stub::read_string(key) }.unwrap_or_default();
    let mut state = h.state();
    let value = state.storage.get(&key).map_or(ptr::null(), |v| stub::cf_string(v));
    state.calls.push(HostCall::CopyFromStorage { key });
    unsafe { *out = value };
    kAudioHardwareNoError
}

unsafe extern "C" fn write_to_storage(
    host: HostRef,
    key: CFStringRef,
    value: CFPropertyListRef,
) -> OSStatus {
    let Some(h) = (unsafe { fake(host) }) else {
        return kAudioHardwareIllegalOperationError;
    };
    let key = unsafe { stub::read_string(key) }.unwrap_or_default();
    let value = unsafe { stub::read_string(value) };
    let mut state = h.state();
    if let Some(v) = &value {
        state.storage.insert(key.clone(), v.clone());
    }
    state.calls.push(HostCall::WriteToStorage { key, value });
    kAudioHardwareNoError
}

unsafe extern "C" fn delete_from_storage(host: HostRef, key: CFStringRef) -> OSStatus {
    let Some(h) = (unsafe { fake(host) }) else {
        return kAudioHardwareIllegalOperationError;
    };
    let key = unsafe { stub::read_string(key) }.unwrap_or_default();
    let mut state = h.state();
    state.storage.remove(&key);
    state.calls.push(HostCall::DeleteFromStorage { key });
    kAudioHardwareNoError
}

unsafe extern "C" fn request_config_change(
    host: HostRef,
    device: AudioObjectID,
    action: u64,
    _info: *mut c_void,
) -> OSStatus {
    let Some(h) = (unsafe { fake(host) }) else {
        return kAudioHardwareIllegalOperationError;
    };
    let mut state = h.state();
    state.calls.push(HostCall::RequestDeviceConfigurationChange { device, action });
    state.request_status
}

/// The vtable of a driver object.
///
/// # Safety
/// `driver` must come from the factory or `new_driver_object`.
pub unsafe fn interface(driver: *mut c_void) -> &'static DriverInterface {
    // SAFETY: a driver reference points at a pointer to the vtable.
    unsafe { *(driver as *const &'static DriverInterface) }
}

/// Whether the driver object caught a panic.
///
/// # Safety
/// As for [`interface`].
pub unsafe fn faulted(driver: *mut c_void) -> bool {
    unsafe { object(driver) }.is_some_and(|o| o.driver.faulted.load(Ordering::Acquire))
}

/// Host time of the driver's Initialize in ns, 0 before it.
///
/// # Safety
/// As for [`interface`].
pub unsafe fn initialized_ns(driver: *mut c_void) -> u64 {
    unsafe { object(driver) }.map_or(0, |o| o.driver.init_ns.load(Ordering::Acquire))
}

/// A link factory whose transport never connects, for drivers that run
/// without a daemon.
pub fn null_link_factory() -> LinkFactory {
    Box::new(|| Arc::new(NullTransport) as Arc<dyn ClientTransport>)
}
