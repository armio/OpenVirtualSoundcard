//! The host interface the HAL hands to Initialize, behind safe wrappers.
//!
//! PropertiesChanged and RequestDeviceConfigurationChange may only be called
//! from non-real-time contexts with no driver lock held; the driver calls
//! them from its IPC queue (design section 13).

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

use crate::abi::{
    AudioObjectID, AudioObjectPropertyAddress, AudioServerPlugInHostInterface, CFPropertyListRef,
    HostRef, OSStatus, kAudioHardwareIllegalOperationError,
};
use crate::platform::Platform;

/// Where the driver keeps the host reference from Initialize.
#[derive(Debug, Default)]
pub struct HostSlot(AtomicPtr<AudioServerPlugInHostInterface>);

impl HostSlot {
    pub const fn new() -> Self {
        Self(AtomicPtr::new(ptr::null_mut()))
    }

    /// Stores the host from Initialize.
    pub fn set(&self, host: HostRef) {
        self.0.store(host as *mut AudioServerPlugInHostInterface, Ordering::Release);
    }

    /// The stored host, or null before Initialize.
    pub fn get(&self) -> HostRef {
        self.0.load(Ordering::Acquire)
    }

    pub fn is_set(&self) -> bool {
        !self.get().is_null()
    }

    fn interface(&self) -> Option<(HostRef, &AudioServerPlugInHostInterface)> {
        let host = self.get();
        // SAFETY: the HAL's host table lives as long as the plug-in.
        unsafe { host.as_ref() }.map(|iface| (host, iface))
    }

    /// Tells the host that `addresses` of `object` changed.
    pub fn properties_changed(
        &self,
        object: AudioObjectID,
        addresses: &[AudioObjectPropertyAddress],
    ) -> OSStatus {
        let Some((host, iface)) = self.interface() else {
            return kAudioHardwareIllegalOperationError;
        };
        let Some(f) = iface.PropertiesChanged else {
            return kAudioHardwareIllegalOperationError;
        };
        // SAFETY: `addresses` stays valid for the duration of the call.
        unsafe { f(host, object, addresses.len() as u32, addresses.as_ptr()) }
    }

    /// The text stored under `key` in the host's persistent storage, or
    /// `None` if nothing (or something that is not text) is stored there.
    pub fn copy_from_storage(&self, platform: &dyn Platform, key: &str) -> Option<String> {
        let (host, iface) = self.interface()?;
        let f = iface.CopyFromStorage?;
        let cf_key = platform.cfstring_create(key);
        if cf_key.is_null() {
            return None;
        }
        let mut data: CFPropertyListRef = ptr::null();
        // SAFETY: `cf_key` is a live CFString and `data` a valid out-pointer.
        let status = unsafe { f(host, cf_key, &mut data) };
        platform.cf_release(cf_key);
        // The caller owns the returned object, even alongside an error.
        let text = if status == 0 { platform.cfstring_read(data) } else { None };
        platform.cf_release(data);
        text
    }

    /// Stores `value` under `key` in the host's persistent storage.
    pub fn write_to_storage(&self, platform: &dyn Platform, key: &str, value: &str) -> OSStatus {
        let Some((host, iface)) = self.interface() else {
            return kAudioHardwareIllegalOperationError;
        };
        let Some(f) = iface.WriteToStorage else {
            return kAudioHardwareIllegalOperationError;
        };
        let cf_key = platform.cfstring_create(key);
        let cf_value = platform.cfstring_create(value);
        let status = if cf_key.is_null() || cf_value.is_null() {
            kAudioHardwareIllegalOperationError
        } else {
            // SAFETY: both are live CFStrings; the host retains what it keeps.
            unsafe { f(host, cf_key, cf_value) }
        };
        platform.cf_release(cf_key);
        platform.cf_release(cf_value);
        status
    }

    /// Asks the host to stop IO and call PerformDeviceConfigurationChange
    /// for `device` with `action`.
    pub fn request_config_change(&self, device: AudioObjectID, action: u64) -> OSStatus {
        let Some((host, iface)) = self.interface() else {
            return kAudioHardwareIllegalOperationError;
        };
        let Some(f) = iface.RequestDeviceConfigurationChange else {
            return kAudioHardwareIllegalOperationError;
        };
        // SAFETY: the change info is unused, so null is fine.
        unsafe { f(host, device, action, ptr::null_mut::<c_void>()) }
    }
}
