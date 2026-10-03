//! The driver's life as the HAL sees it, through the extern "C" factory and
//! vtable only: factory, IUnknown, Initialize with a fake host, plug-in
//! properties, the device calls, and panic containment. Every property of
//! every object is walked in `property_conformance.rs`.

use std::ffi::c_void;
use std::ptr;

use ovsc_hal::abi::*;
use ovsc_hal::host::HostSlot;
use ovsc_hal::platform::stub::{self, PanicAt, StubPlatform};
use ovsc_hal::testing::{self, FakeHost, HostCall};
use ovsc_hal::{OpenVirtualSoundcard_Factory, new_driver_object};

const DEVICE: AudioObjectID = 2;
const INPUT_STREAM: AudioObjectID = 3;

struct Hal {
    platform: &'static StubPlatform,
    driver: *mut c_void,
    vt: &'static DriverInterface,
}

impl Hal {
    /// A driver object of its own, on a manual clock.
    fn new() -> Hal {
        let platform = StubPlatform::new().leak();
        let driver = new_driver_object(platform, testing::null_link_factory());
        let vt = unsafe { testing::interface(driver) };
        Hal { platform, driver, vt }
    }

    fn initialized(host: &'static FakeHost) -> Hal {
        let hal = Hal::new();
        assert_eq!(unsafe { (hal.vt.Initialize)(hal.driver, host.host_ref()) }, 0);
        hal
    }

    fn has(&self, obj: AudioObjectID, selector: u32) -> bool {
        unsafe { (self.vt.HasProperty)(self.driver, obj, 1, &global(selector)) != 0 }
    }

    fn settable(&self, obj: AudioObjectID, selector: u32) -> Result<bool, OSStatus> {
        let mut out: Boolean = 7;
        match unsafe {
            (self.vt.IsPropertySettable)(self.driver, obj, 1, &global(selector), &mut out)
        } {
            0 => Ok(out != 0),
            e => Err(e),
        }
    }

    fn size(&self, obj: AudioObjectID, selector: u32) -> Result<u32, OSStatus> {
        let mut size = 0xDEAD;
        let a = global(selector);
        match unsafe {
            (self.vt.GetPropertyDataSize)(self.driver, obj, 1, &a, 0, ptr::null(), &mut size)
        } {
            0 => Ok(size),
            e => Err(e),
        }
    }

    /// GetPropertyData into a buffer of `capacity` bytes, with an optional
    /// CFString qualifier.
    fn get(
        &self,
        obj: AudioObjectID,
        selector: u32,
        capacity: u32,
        qualifier: Option<CFStringRef>,
    ) -> Result<Vec<u8>, OSStatus> {
        let mut buf = vec![0xAAu8; capacity as usize];
        let mut used = 0xDEAD;
        let (qsize, qdata) = match &qualifier {
            Some(q) => (size_of::<CFStringRef>() as u32, q as *const CFStringRef as *const c_void),
            None => (0, ptr::null()),
        };
        let status = unsafe {
            (self.vt.GetPropertyData)(
                self.driver,
                obj,
                1,
                &global(selector),
                qsize,
                qdata,
                capacity,
                &mut used,
                buf.as_mut_ptr() as *mut c_void,
            )
        };
        match status {
            0 => {
                buf.truncate(used as usize);
                Ok(buf)
            }
            e => Err(e),
        }
    }

    fn get_u32(&self, obj: AudioObjectID, selector: u32) -> Result<u32, OSStatus> {
        let b = self.get(obj, selector, 4, None)?;
        Ok(u32::from_ne_bytes(b.try_into().expect("4 bytes")))
    }

    /// A CFString property, released after reading as the HAL would.
    fn get_string(&self, obj: AudioObjectID, selector: u32) -> Result<String, OSStatus> {
        let b = self.get(obj, selector, size_of::<CFStringRef>() as u32, None)?;
        assert_eq!(b.len(), size_of::<CFStringRef>());
        let s = unsafe { b.as_ptr().cast::<CFStringRef>().read_unaligned() };
        let text = unsafe { stub::read_string(s) }.expect("a stub CFString");
        unsafe { stub::cf_free(s) };
        Ok(text)
    }

    fn set(&self, obj: AudioObjectID, selector: u32, data: &[u8]) -> OSStatus {
        unsafe {
            (self.vt.SetPropertyData)(
                self.driver,
                obj,
                1,
                &global(selector),
                0,
                ptr::null(),
                data.len() as u32,
                data.as_ptr() as *const c_void,
            )
        }
    }

    fn zts(&self) -> Result<(f64, u64, u64), OSStatus> {
        let (mut s, mut h, mut seed) = (0.0, 0, 0);
        match unsafe {
            (self.vt.GetZeroTimeStamp)(self.driver, DEVICE, 0, &mut s, &mut h, &mut seed)
        } {
            0 => Ok((s, h, seed)),
            e => Err(e),
        }
    }

    /// ReadInput into `buf` (8 channels).
    fn read_input(&self, buf: &mut [f32]) -> OSStatus {
        let cycle = IOCycleInfo::default();
        unsafe {
            (self.vt.DoIOOperation)(
                self.driver,
                DEVICE,
                INPUT_STREAM,
                0,
                kAudioServerPlugInIOOperationReadInput,
                (buf.len() / 8) as u32,
                &cycle,
                buf.as_mut_ptr() as *mut c_void,
                ptr::null_mut(),
            )
        }
    }
}

fn global(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

fn query(vt: &DriverInterface, driver: *mut c_void, uuid: [u8; 16]) -> (HRESULT, *mut c_void) {
    let mut out = ptr::dangling_mut::<c_void>();
    let r = unsafe { (vt.QueryInterface)(driver, CFUUIDBytes(uuid), &mut out) };
    (r, out)
}

/// CFUUIDs the factory's platform can read: real ones on macOS, where the
/// factory uses CoreFoundation, stub objects elsewhere.
#[cfg(target_os = "macos")]
mod cf {
    use super::*;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFUUIDCreateFromUUIDBytes(alloc: CFAllocatorRef, bytes: CFUUIDBytes) -> CFUUIDRef;
        fn CFRelease(cf: *const c_void);
    }

    pub fn uuid(bytes: [u8; 16]) -> CFUUIDRef {
        unsafe { CFUUIDCreateFromUUIDBytes(ptr::null(), CFUUIDBytes(bytes)) }
    }

    pub fn release(o: CFUUIDRef) {
        unsafe { CFRelease(o) }
    }
}

#[cfg(not(target_os = "macos"))]
mod cf {
    use super::*;

    pub fn uuid(bytes: [u8; 16]) -> CFUUIDRef {
        stub::cf_uuid(bytes)
    }

    pub fn release(o: CFUUIDRef) {
        unsafe { stub::cf_free(o) }
    }
}

#[test]
fn factory_answers_only_the_plugin_type() {
    let plugin_type = cf::uuid(kAudioServerPlugInTypeUUID);
    let other_type = cf::uuid(kAudioServerPlugInDriverInterfaceUUID);
    unsafe {
        let d = OpenVirtualSoundcard_Factory(ptr::null(), plugin_type);
        assert!(!d.is_null());
        // One instance per process, retained for each caller.
        assert_eq!(OpenVirtualSoundcard_Factory(ptr::null(), plugin_type), d);
        assert!(OpenVirtualSoundcard_Factory(ptr::null(), other_type).is_null());
        assert!(OpenVirtualSoundcard_Factory(ptr::null(), ptr::null()).is_null());

        // It is a working driver object. (It is never initialized here, so
        // the test process keeps its panic hook.)
        let vt = testing::interface(d);
        let (r, out) = query(vt, d, kAudioServerPlugInDriverInterfaceUUID);
        assert_eq!((r, out), (S_OK, d));
        assert_eq!((vt.Release)(d), 2);
        assert!(!testing::faulted(d));
    }
    cf::release(plugin_type);
    cf::release(other_type);
}

#[test]
fn query_interface_hands_out_the_same_object() {
    let hal = Hal::new();
    for uuid in [IUnknownUUID, kAudioServerPlugInDriverInterfaceUUID] {
        assert_eq!(query(hal.vt, hal.driver, uuid), (S_OK, hal.driver));
    }
    let mut odd = kAudioServerPlugInDriverInterfaceUUID;
    odd[15] ^= 1;
    for uuid in [kAudioServerPlugInTypeUUID, odd, [0; 16]] {
        assert_eq!(query(hal.vt, hal.driver, uuid), (E_NOINTERFACE, ptr::null_mut()));
    }
    let r =
        unsafe { (hal.vt.QueryInterface)(hal.driver, CFUUIDBytes(IUnknownUUID), ptr::null_mut()) };
    assert_ne!(r, S_OK);
    // Two successful queries took two references on top of the first.
    assert_eq!(unsafe { (hal.vt.Release)(hal.driver) }, 2);
}

#[test]
fn add_ref_and_release_count() {
    let hal = Hal::new();
    unsafe {
        assert_eq!((hal.vt.AddRef)(hal.driver), 2);
        assert_eq!((hal.vt.AddRef)(hal.driver), 3);
        assert_eq!((hal.vt.Release)(hal.driver), 2);
        assert_eq!((hal.vt.Release)(hal.driver), 1);
        assert_eq!((hal.vt.Release)(hal.driver), 0);
        // The count stops at zero and the object is never freed.
        assert_eq!((hal.vt.Release)(hal.driver), 0);
        assert_eq!((hal.vt.AddRef)(hal.driver), 1);
        assert!(hal.has(kAudioObjectPlugInObject, kAudioObjectPropertyManufacturer));
        // A null reference is harmless.
        assert_eq!((hal.vt.AddRef)(ptr::null_mut()), 0);
    }
}

#[test]
fn initialize_stores_the_host_and_starts_the_timeline() {
    let host = FakeHost::new();
    let hal = Hal::new();
    hal.platform.set_now_ticks(5_000_000);
    assert_eq!(unsafe { testing::initialized_ns(hal.driver) }, 0);
    assert_eq!(unsafe { (hal.vt.Initialize)(hal.driver, host.host_ref()) }, 0);
    assert_eq!(unsafe { testing::initialized_ns(hal.driver) }, 5_000_000);
    assert!(hal.platform.logged("initialized"));
    // The only host call reads the stored configuration.
    assert_eq!(
        host.calls(),
        vec![HostCall::CopyFromStorage { key: "org.openvirtualsoundcard.config.v1".into() }]
    );
    assert!(!unsafe { testing::faulted(hal.driver) });
}

#[test]
fn plugin_properties() {
    let host = FakeHost::new();
    let hal = Hal::initialized(host);
    let plugin = kAudioObjectPlugInObject;

    for sel in [
        kAudioObjectPropertyBaseClass,
        kAudioObjectPropertyClass,
        kAudioObjectPropertyOwner,
        kAudioObjectPropertyManufacturer,
        kAudioObjectPropertyOwnedObjects,
        kAudioPlugInPropertyDeviceList,
        kAudioPlugInPropertyTranslateUIDToDevice,
        kAudioPlugInPropertyBoxList,
        kAudioPlugInPropertyTranslateUIDToBox,
        kAudioPlugInPropertyResourceBundle,
    ] {
        assert!(hal.has(plugin, sel), "{sel:08x}");
        assert_eq!(hal.settable(plugin, sel), Ok(false));
        let size = hal.size(plugin, sel).expect("size");
        if sel != kAudioObjectPropertyManufacturer && sel != kAudioPlugInPropertyResourceBundle {
            assert_eq!(hal.get(plugin, sel, 64, None).expect("data").len() as u32, size);
        }
    }
    assert_eq!(hal.get_u32(plugin, kAudioObjectPropertyBaseClass), Ok(kAudioObjectClassID));
    assert_eq!(hal.get_u32(plugin, kAudioObjectPropertyClass), Ok(kAudioPlugInClassID));
    assert_eq!(hal.get_u32(plugin, kAudioObjectPropertyOwner), Ok(kAudioObjectUnknown));
    assert_eq!(
        hal.get_string(plugin, kAudioObjectPropertyManufacturer).as_deref(),
        Ok("OpenVirtualSoundcard")
    );
    assert_eq!(hal.get_string(plugin, kAudioPlugInPropertyResourceBundle).as_deref(), Ok(""));
    // The plug-in owns the one device.
    assert_eq!(
        hal.get(plugin, kAudioPlugInPropertyDeviceList, 64, None),
        Ok(DEVICE.to_ne_bytes().to_vec())
    );
    assert_eq!(hal.size(plugin, kAudioObjectPropertyOwnedObjects), Ok(4));
    let uid = stub::cf_string("org.openvirtualsoundcard.vsc");
    let b = hal.get(plugin, kAudioPlugInPropertyTranslateUIDToDevice, 4, Some(uid));
    assert_eq!(b, Ok(DEVICE.to_ne_bytes().to_vec()));
    unsafe { stub::cf_free(uid) };

    // A fixed-size value that does not fit.
    assert_eq!(
        hal.get(plugin, kAudioObjectPropertyClass, 2, None),
        Err(kAudioHardwareBadPropertySizeError)
    );
    // Nothing on the plug-in is settable.
    assert_eq!(
        hal.set(plugin, kAudioObjectPropertyManufacturer, &[0; 8]),
        kAudioHardwareIllegalOperationError
    );
}

#[test]
fn unknown_selectors_and_objects() {
    let host = FakeHost::new();
    let hal = Hal::initialized(host);
    let plugin = kAudioObjectPlugInObject;
    for sel in
        [u32::from_be_bytes(*b"nope"), u32::from_be_bytes(*b"odxx"), kAudioDevicePropertyLatency, 0]
    {
        assert!(!hal.has(plugin, sel));
        assert_eq!(hal.settable(plugin, sel), Err(kAudioHardwareUnknownPropertyError));
        assert_eq!(hal.size(plugin, sel), Err(kAudioHardwareUnknownPropertyError));
        assert_eq!(hal.get(plugin, sel, 64, None), Err(kAudioHardwareUnknownPropertyError));
        assert_eq!(hal.set(plugin, sel, &[0; 4]), kAudioHardwareUnknownPropertyError);
    }
    for obj in [0, 9, u32::MAX] {
        assert!(!hal.has(obj, kAudioObjectPropertyClass));
        assert_eq!(hal.size(obj, kAudioObjectPropertyClass), Err(kAudioHardwareBadObjectError));
        assert_eq!(
            hal.get(obj, kAudioObjectPropertyClass, 4, None),
            Err(kAudioHardwareBadObjectError)
        );
    }
}

#[test]
fn null_pointers_are_refused() {
    let hal = Hal::new();
    let a = global(kAudioObjectPropertyClass);
    let mut size = 0;
    unsafe {
        assert_eq!((hal.vt.HasProperty)(hal.driver, 1, 1, ptr::null()), 0);
        let r = (hal.vt.GetPropertyDataSize)(hal.driver, 1, 1, &a, 0, ptr::null(), ptr::null_mut());
        assert_eq!(r, kAudioHardwareIllegalOperationError);
        let r = (hal.vt.GetPropertyData)(
            hal.driver,
            1,
            1,
            &a,
            0,
            ptr::null(),
            4,
            &mut size,
            ptr::null_mut(),
        );
        assert_eq!(r, kAudioHardwareIllegalOperationError);
        let r = (hal.vt.Initialize)(ptr::null_mut(), ptr::null());
        assert_eq!(r, kAudioHardwareBadObjectError);
    }
}

#[test]
fn device_calls_in_the_skeleton() {
    let host = FakeHost::new();
    let hal = Hal::initialized(host);
    let vt = hal.vt;
    let d = hal.driver;
    unsafe {
        let mut id = 0;
        assert_eq!(
            (vt.CreateDevice)(d, ptr::null(), ptr::null(), &mut id),
            kAudioHardwareUnsupportedOperationError
        );
        assert_eq!((vt.DestroyDevice)(d, DEVICE), kAudioHardwareUnsupportedOperationError);
        assert_eq!((vt.AddDeviceClient)(d, DEVICE, ptr::null()), 0);
        assert_eq!((vt.RemoveDeviceClient)(d, DEVICE, ptr::null()), 0);
        assert_eq!((vt.AddDeviceClient)(d, 7, ptr::null()), kAudioHardwareBadObjectError);
        // Nothing is pending, so a configuration change has nothing to do.
        assert_eq!((vt.PerformDeviceConfigurationChange)(d, DEVICE, 1, ptr::null_mut()), 0);
        assert_eq!((vt.AbortDeviceConfigurationChange)(d, DEVICE, 1, ptr::null_mut()), 0);
        assert_eq!(
            (vt.PerformDeviceConfigurationChange)(d, 1, 1, ptr::null_mut()),
            kAudioHardwareBadObjectError
        );
        assert_eq!((vt.StartIO)(d, 7, 0), kAudioHardwareBadObjectError);
        assert_eq!((vt.StartIO)(d, DEVICE, 0), 0);

        let (mut will, mut in_place) = (9, 9);
        let read = kAudioServerPlugInIOOperationReadInput;
        assert_eq!((vt.WillDoIOOperation)(d, DEVICE, 0, read, &mut will, &mut in_place), 0);
        assert_eq!((will, in_place), (1, 1));
        let mix = kAudioServerPlugInIOOperationMixOutput;
        assert_eq!((vt.WillDoIOOperation)(d, DEVICE, 0, mix, &mut will, &mut in_place), 0);
        assert_eq!(will, 0);
        let cycle = IOCycleInfo::default();
        assert_eq!((vt.BeginIOOperation)(d, DEVICE, 0, read, 512, &cycle), 0);
        let mut buf = vec![0.5f32; 8 * 512];
        assert_eq!(hal.read_input(&mut buf), 0);
        assert!(buf.iter().all(|&x| x == 0.0));
        assert_eq!((vt.EndIOOperation)(d, DEVICE, 0, read, 512, &cycle), 0);
        assert_eq!((vt.StopIO)(d, DEVICE, 0), 0);
    }
}

#[test]
fn zero_timestamps_run_on_the_host_clock() {
    let host = FakeHost::new();
    let hal = Hal::new();
    // Apple silicon's timebase: 125 ns per 3 ticks.
    hal.platform.set_timebase(125, 3);
    hal.platform.set_now_ticks(3_000);
    assert_eq!(unsafe { (hal.vt.Initialize)(hal.driver, host.host_ref()) }, 0);
    assert_eq!(unsafe { (hal.vt.StartIO)(hal.driver, DEVICE, 0) }, 0);
    // 16384 frames at 48 kHz = 8,192,000 ticks of 125/3 ns. The clock moves
    // a tick more per period, so each time stamp is due; their host times
    // are the timeline's nanoseconds rounded up to ticks.
    let period = 8_192_000;
    assert_eq!(hal.zts(), Ok((0.0, 3_000, 1)));
    for k in 1..=10u64 {
        hal.platform.advance_ticks(period + 1);
        let (sample, host, seed) = hal.zts().expect("zts");
        assert_eq!((sample, seed), (k as f64 * 16384.0, 1));
        assert!(host.abs_diff(3_000 + k * period) <= 1, "k {k}: {host}");
        assert!(host <= 3_000 + k * (period + 1));
    }
    assert_eq!(
        unsafe { (hal.vt.GetZeroTimeStamp)(hal.driver, 9, 0, &mut 0.0, &mut 0, &mut 0) },
        kAudioHardwareBadObjectError
    );
}

#[test]
fn a_panic_in_a_property_getter_faults_the_driver() {
    let host = FakeHost::new();
    let hal = Hal::initialized(host);
    let plugin = kAudioObjectPlugInObject;

    hal.platform.inject_panic(PanicAt::CfStringCreate);
    let r = hal.get(plugin, kAudioObjectPropertyManufacturer, 8, None);
    assert_eq!(r, Err(kAudioHardwareUnspecifiedError));
    assert!(unsafe { testing::faulted(hal.driver) });
    // The process lives on and the properties still answer.
    assert_eq!(
        hal.get_string(plugin, kAudioObjectPropertyManufacturer).as_deref(),
        Ok("OpenVirtualSoundcard")
    );
    assert_eq!(hal.get_u32(plugin, kAudioObjectPropertyClass), Ok(kAudioPlugInClassID));
}

#[test]
fn a_faulted_driver_keeps_time_and_stays_silent() {
    let host = FakeHost::new();
    let hal = Hal::initialized(host);
    assert_eq!(unsafe { (hal.vt.StartIO)(hal.driver, DEVICE, 0) }, 0);
    let first = hal.zts().expect("zts");

    // A panic in GetZeroTimeStamp hands out the previous time stamp again.
    hal.platform.inject_panic(PanicAt::NowTicks);
    assert_eq!(hal.zts(), Ok(first));
    assert!(unsafe { testing::faulted(hal.driver) });

    // Faulted IO is silent.
    let mut buf = vec![0.25f32; 8 * 64];
    assert_eq!(hal.read_input(&mut buf), 0);
    assert!(buf.iter().all(|&x| x == 0.0));
}

#[test]
fn host_wrappers_reach_the_host() {
    let host = FakeHost::new();
    let platform = StubPlatform::new();
    let slot = HostSlot::new();
    assert_eq!(slot.request_config_change(DEVICE, 1), kAudioHardwareIllegalOperationError);
    assert_eq!(slot.copy_from_storage(&platform, "k"), None);
    slot.set(host.host_ref());
    assert!(slot.is_set());

    assert_eq!(
        slot.write_to_storage(&platform, "org.openvirtualsoundcard.config.v1", "rate=48000\n"),
        0
    );
    assert_eq!(host.storage("org.openvirtualsoundcard.config.v1").as_deref(), Some("rate=48000\n"));
    assert_eq!(
        slot.copy_from_storage(&platform, "org.openvirtualsoundcard.config.v1").as_deref(),
        Some("rate=48000\n")
    );
    assert_eq!(slot.copy_from_storage(&platform, "missing"), None);

    let lchn = AudioObjectPropertyAddress {
        mSelector: kAudioObjectPropertyElementName,
        mScope: kAudioObjectPropertyScopeInput,
        mElement: kAudioObjectPropertyElementWildcard,
    };
    assert_eq!(slot.properties_changed(DEVICE, &[lchn]), 0);
    host.set_request_status(kAudioHardwareNotRunningError);
    assert_eq!(slot.request_config_change(DEVICE, 1), kAudioHardwareNotRunningError);

    assert_eq!(
        host.take_calls(),
        vec![
            HostCall::WriteToStorage {
                key: "org.openvirtualsoundcard.config.v1".into(),
                value: Some("rate=48000\n".into())
            },
            HostCall::CopyFromStorage { key: "org.openvirtualsoundcard.config.v1".into() },
            HostCall::CopyFromStorage { key: "missing".into() },
            HostCall::PropertiesChanged { object: DEVICE, addresses: vec![lchn] },
            HostCall::RequestDeviceConfigurationChange { device: DEVICE, action: 1 },
        ]
    );
    assert!(host.calls().is_empty());
}
