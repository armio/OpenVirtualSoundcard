//! The driver behind one driver object: the HAL's calls, already checked
//! for null pointers by `entry`, routed to the model, the IO engine and the
//! link (design sections 5.3, 12 and 13).
//!
//! The driver is the link's [`LinkSink`]: the link, on its serial queue,
//! hands it the daemon's regions and configurations, and asks it for the
//! host calls a configuration needs. Those host calls are made from that
//! queue only, with no driver lock held.

use crate::atomic::update_u32;
use std::ffi::c_void;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use ovsc_ipc::protocol::STORAGE_KEY;
use ovsc_ipc::region::MappedRegion;
use ovsc_shm::layout::LayoutError;
use ovsc_shm::timeline::Regime;

use crate::abi::*;
use crate::entry::LinkFactory;
use crate::ffi;
use crate::host::HostSlot;
use crate::io::{Attachment, IoEngine, IoSnapshot};
use crate::link::{ConfigPlan, Link, LinkSink, LinkStatus};
use crate::model::{
    DEVICE_OBJECT, DriverConfig, Live, Model, Qualifier, STATUS_SELECTOR, SetEffect,
};
use crate::platform::{LOG_DEFAULT, LOG_ERROR, LOG_INFO, Platform, Timebase};

/// The action of the configuration changes the driver requests.
const CONFIG_CHANGE_ACTION: u64 = 1;

pub(crate) struct Driver {
    pub(crate) platform: &'static dyn Platform,
    pub(crate) host: HostSlot,
    pub(crate) model: Model,
    pub(crate) io: IoEngine,
    pub(crate) link: OnceLock<Arc<Link>>,
    /// A configuration waiting for PerformDeviceConfigurationChange. Never
    /// held across a host call; held while a configuration is compared with
    /// the published one and published.
    pub(crate) pending: Mutex<Option<DriverConfig>>,
    /// AddDeviceClient minus RemoveDeviceClient.
    pub(crate) clients: AtomicU32,
    /// Set when an entry point caught a panic; IO is silent from then on.
    pub(crate) faulted: AtomicBool,
    /// Host time of Initialize, in ns.
    pub(crate) init_ns: AtomicU64,
    /// Makes the link's transport.
    pub(crate) link_factory: LinkFactory,
    /// Whether Initialize silences the panic hook: only in the instance the
    /// HAL loads through the factory, never in tests.
    quiet_panics: bool,
    /// The published input channel count, for silencing input once faulted
    /// without touching the model on the IO thread. Set with every publish.
    input_channels: AtomicU32,
}

impl Driver {
    pub(crate) fn new(
        platform: &'static dyn Platform,
        link_factory: LinkFactory,
        quiet_panics: bool,
    ) -> Self {
        let cfg = DriverConfig::fallback();
        Self {
            platform,
            host: HostSlot::new(),
            model: Model::new(&cfg),
            io: IoEngine::new(&cfg, platform),
            link: OnceLock::new(),
            pending: Mutex::new(None),
            clients: AtomicU32::new(0),
            faulted: AtomicBool::new(false),
            init_ns: AtomicU64::new(0),
            link_factory,
            quiet_panics,
            input_channels: AtomicU32::new(cfg.input_channels),
        }
    }

    /// Publishes `cfg` to the model. Every publish goes through here, so the
    /// IO path's copy of the channel count stays in step.
    pub(crate) fn publish_config(&self, cfg: DriverConfig) {
        self.input_channels.store(cfg.input_channels, Ordering::Release);
        self.model.publish(cfg);
    }

    /// Host time in ns.
    pub(crate) fn now_ns(&self) -> u64 {
        self.platform.timebase().ticks_to_ns(self.platform.now_ticks())
    }

    /// Initialize: publishes the configuration host storage kept from the
    /// last run, or the fallback, so the device shows its last channel
    /// layout and names before the daemon answers, then starts the link on
    /// its queue (design section 10.6). A second Initialize starts no second
    /// link.
    pub(crate) fn initialize(&'static self, host: HostRef) -> OSStatus {
        self.host.set(host);
        if self.quiet_panics {
            ffi::install_quiet_panic_hook();
        }
        self.init_ns.store(self.now_ns(), Ordering::Release);
        let cfg = self.restore_config();
        let rate = cfg.sample_rate;
        self.io.apply_config(&cfg);
        self.publish_config(cfg);
        self.io.reset_timeline(rate);
        self.platform.log(LOG_DEFAULT, "initialized");
        if self.link.get().is_none() {
            let transport = (self.link_factory)();
            let sink = Arc::new(DriverSink(self));
            let link = Link::new(transport, sink, self.platform.random_u64(), self.platform.pid());
            if self.link.set(link.clone()).is_ok() {
                link.start();
            }
        }
        kAudioHardwareNoError
    }

    /// The configuration in host storage, or the fallback if there is none
    /// or it does not parse and validate.
    fn restore_config(&self) -> DriverConfig {
        let Some(text) = self.host.copy_from_storage(self.platform, STORAGE_KEY) else {
            return DriverConfig::fallback();
        };
        match DriverConfig::from_storage_string(&text) {
            Ok(cfg) => {
                let msg = format!(
                    "restored configuration {}: {} Hz, {} in, {} out",
                    cfg.config_gen, cfg.sample_rate, cfg.input_channels, cfg.output_channels
                );
                self.platform.log(LOG_INFO, &msg);
                cfg
            }
            Err(e) => {
                self.platform.log(LOG_ERROR, &format!("stored configuration ignored: {e}"));
                DriverConfig::fallback()
            }
        }
    }

    /// The device is published by the driver, never created by the HAL.
    pub(crate) fn create_device(&self) -> OSStatus {
        kAudioHardwareUnsupportedOperationError
    }

    pub(crate) fn destroy_device(&self, _device: AudioObjectID) -> OSStatus {
        kAudioHardwareUnsupportedOperationError
    }

    pub(crate) fn add_device_client(&self, device: AudioObjectID) -> OSStatus {
        if device != DEVICE_OBJECT {
            return kAudioHardwareBadObjectError;
        }
        self.clients.fetch_add(1, Ordering::AcqRel);
        kAudioHardwareNoError
    }

    pub(crate) fn remove_device_client(&self, device: AudioObjectID) -> OSStatus {
        if device != DEVICE_OBJECT {
            return kAudioHardwareBadObjectError;
        }
        let _ =
            update_u32(&self.clients, Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
        kAudioHardwareNoError
    }

    /// Applies the pending configuration. The HAL has stopped IO for this
    /// call (design section 12): the IO engine and the model take the
    /// configuration, a new rate restarts the timeline, and the link reports
    /// it to the daemon and stores it.
    pub(crate) fn perform_config_change(&self, device: AudioObjectID, _action: u64) -> OSStatus {
        if device != DEVICE_OBJECT {
            return kAudioHardwareBadObjectError;
        }
        // Held until the configuration is published, so that a configuration
        // the link stages meanwhile is compared with this one.
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(cfg) = pending.take() else {
            drop(pending);
            // A newer configuration made the requested change unnecessary;
            // the request is over all the same.
            if let Some(link) = self.link.get() {
                link.aborted();
            }
            return kAudioHardwareNoError;
        };
        let old_rate = self.model.published().config.sample_rate;
        self.io.apply_config(&cfg);
        self.publish_config(cfg.clone());
        if cfg.sample_rate != old_rate {
            self.io.reset_timeline(cfg.sample_rate);
        }
        drop(pending);
        if let Some(link) = self.link.get() {
            link.performed(&cfg);
        }
        kAudioHardwareNoError
    }

    /// The HAL dropped a requested change; the pending configuration stays
    /// and the link asks again later.
    pub(crate) fn abort_config_change(&self, device: AudioObjectID, _action: u64) -> OSStatus {
        if device != DEVICE_OBJECT {
            return kAudioHardwareBadObjectError;
        }
        if let Some(link) = self.link.get() {
            link.aborted();
        }
        kAudioHardwareNoError
    }

    /// Decodes the qualifier of a property call. Only the UID translations
    /// take one: a CFString.
    pub(crate) fn qualifier(&self, selector: u32, data: &[u8]) -> Qualifier {
        const N: usize = size_of::<CFStringRef>();
        if !matches!(
            selector,
            kAudioPlugInPropertyTranslateUIDToDevice | kAudioPlugInPropertyTranslateUIDToBox
        ) {
            return Qualifier::None;
        }
        let Some(bytes) = data.get(..N) else {
            return Qualifier::None;
        };
        // SAFETY: `bytes` is N readable bytes. Reading them as a pointer, not
        // as an integer, keeps the pointer valid to dereference.
        let s = unsafe { bytes.as_ptr().cast::<CFStringRef>().read_unaligned() };
        self.platform.cfstring_read(s).map_or(Qualifier::None, Qualifier::Str)
    }

    /// Takes a valid configuration from the daemon (the link's
    /// [`LinkSink::stage`]). One that differs from the published one only
    /// in its names or generation is published at once; one whose structure
    /// differs waits for PerformDeviceConfigurationChange, replacing any
    /// that waited before. Either way the newest configuration wins.
    pub(crate) fn stage(&self, cfg: DriverConfig) -> ConfigPlan {
        // Held while comparing and publishing: a Perform on the HAL's thread
        // publishes under the same lock, so the comparison is with what the
        // device ends up publishing.
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        let published = self.model.published();
        let plan = ConfigPlan::of(&published.config, &cfg);
        match plan {
            ConfigPlan::Structural => *pending = Some(cfg),
            ConfigPlan::Same | ConfigPlan::NamesOnly => {
                *pending = None;
                if cfg != published.config {
                    self.publish_config(cfg);
                }
            }
        }
        plan
    }

    /// The live state properties report for a call on `selector`. Only the
    /// status property shows the status line, so only it pays for building
    /// one.
    pub(crate) fn live(&self, selector: u32) -> Live {
        let io = self.io.snapshot();
        let status_line =
            if selector == STATUS_SELECTOR { self.status_line(&io) } else { String::new() };
        Live { io_running: io.io_clients > 0, status_line }
    }

    /// The status property's text (design section 9): the link's state,
    /// then what is published and the engine's counters.
    pub(crate) fn status_line(&self, io: &IoSnapshot) -> String {
        let (link, attach) =
            self.link.get().map_or((LinkStatus::Absent, 0), |l| (l.status(), l.attach_count()));
        let published = self.model.published();
        let cfg = &published.config;
        let mut s = String::with_capacity(128);
        // Writing to a String cannot fail.
        let _ = match &link {
            LinkStatus::Absent => write!(s, "daemon=absent"),
            LinkStatus::Connecting => write!(s, "daemon=connecting"),
            LinkStatus::Attached { generation } => write!(s, "daemon=attached gen={generation:x}"),
            LinkStatus::Incompatible(reason) => write!(s, "daemon=incompatible({reason})"),
        };
        let clock = match io.regime {
            Regime::Synthetic => "synthetic",
            Regime::Following => "following",
            Regime::Holdover => "holdover",
        };
        let _ = write!(
            s,
            " clock={clock} ppm={:+.3} rate={} in={} out={} absorbs={} seed={} gate={} \
             late_out={} early_in={} far_out={} far_in={} out_margin={} in_margin={} \
             silenced={} in_missing={} tx_underruns={} io={} zts={} attach={} faulted={} v={}",
            io.device_rate_ppm_milli as f64 / 1000.0,
            cfg.sample_rate,
            cfg.input_channels,
            cfg.output_channels,
            io.absorbs,
            io.seed,
            u8::from(io.gate),
            io.late_output_cycles,
            io.early_input_cycles,
            io.far_output_cycles,
            io.far_input_cycles,
            Margins(io.min_output_margin, io.max_output_margin),
            Margins(io.min_input_margin, io.max_input_margin),
            io.silenced_cycles,
            io.input_missing,
            io.tx_underruns,
            io.io_clients,
            io.zts_calls,
            attach,
            u8::from(self.faulted.load(Ordering::Acquire) || io.faulted),
            env!("CARGO_PKG_VERSION"),
        );
        s
    }

    pub(crate) fn has_property(&self, obj: AudioObjectID, a: &PropertyAddress) -> bool {
        self.model.has(obj, a)
    }

    pub(crate) fn is_property_settable(
        &self,
        obj: AudioObjectID,
        a: &PropertyAddress,
    ) -> Result<bool, OSStatus> {
        self.model.is_settable(obj, a)
    }

    pub(crate) fn property_data_size(
        &self,
        obj: AudioObjectID,
        a: &PropertyAddress,
        q: &Qualifier,
    ) -> Result<u32, OSStatus> {
        self.model.data_size(obj, a, q, &self.live(a.mSelector))
    }

    pub(crate) fn property_data(
        &self,
        obj: AudioObjectID,
        a: &PropertyAddress,
        q: &Qualifier,
        out: &mut [u8],
    ) -> Result<u32, OSStatus> {
        self.model.get(obj, a, q, &self.live(a.mSelector), out, self.platform)
    }

    pub(crate) fn set_property_data(
        &self,
        obj: AudioObjectID,
        a: &PropertyAddress,
        data: &[u8],
    ) -> OSStatus {
        match self.model.set(obj, a, data, self.platform) {
            Ok(SetEffect::NoChange) => kAudioHardwareNoError,
            Ok(SetEffect::StreamActive { input, active }) => {
                // Stored and reported only: IO runs on both streams anyway.
                let stream = if input { "input" } else { "output" };
                self.platform.log(LOG_INFO, &format!("{stream} stream active: {active}"));
                kAudioHardwareNoError
            }
            Err(e) => e,
        }
    }

    pub(crate) fn start_io(&self, device: AudioObjectID) -> OSStatus {
        if device != DEVICE_OBJECT {
            return kAudioHardwareBadObjectError;
        }
        if self.io.start_io() {
            self.io_running_changed();
        }
        kAudioHardwareNoError
    }

    pub(crate) fn stop_io(&self, device: AudioObjectID) -> OSStatus {
        if device != DEVICE_OBJECT {
            return kAudioHardwareBadObjectError;
        }
        if self.io.stop_io() {
            self.io_running_changed();
        }
        kAudioHardwareNoError
    }

    /// The first StartIO or the last StopIO changed the device's 'goin'.
    /// StartIO's thread is not the IPC queue, so the link posts the
    /// PropertiesChanged from there (design section 13).
    fn io_running_changed(&self) {
        if let Some(link) = self.link.get() {
            link.io_running_changed();
        }
    }

    /// GetZeroTimeStamp: (sample time, host ticks, seed). Real-time.
    pub(crate) fn zero_timestamp(
        &self,
        device: AudioObjectID,
    ) -> Result<(f64, u64, u64), OSStatus> {
        if device != DEVICE_OBJECT {
            return Err(kAudioHardwareBadObjectError);
        }
        Ok(self.io.zero_timestamp())
    }

    /// The last zero time stamp handed out, returned again if
    /// GetZeroTimeStamp panics. Real-time.
    pub(crate) fn last_zero_timestamp(&self) -> (f64, u64, u64) {
        self.io.cached_zero_timestamp()
    }

    pub(crate) fn will_do_io(
        &self,
        device: AudioObjectID,
        op: u32,
    ) -> Result<(bool, bool), OSStatus> {
        if device != DEVICE_OBJECT {
            return Err(kAudioHardwareBadObjectError);
        }
        Ok(self.io.will_do(op))
    }

    /// BeginIOOperation and EndIOOperation have nothing to do.
    pub(crate) fn begin_end_io(&self, device: AudioObjectID) -> OSStatus {
        if device != DEVICE_OBJECT {
            return kAudioHardwareBadObjectError;
        }
        kAudioHardwareNoError
    }

    /// DoIOOperation. Real-time. Once faulted, input is silence and output
    /// is dropped.
    ///
    /// # Safety
    /// `main` must be null or the HAL's buffer for this operation: `frames`
    /// interleaved Float32 frames of the stream's channel count.
    pub(crate) unsafe fn do_io(
        &self,
        device: AudioObjectID,
        stream: AudioObjectID,
        op: u32,
        frames: u32,
        cycle: &IOCycleInfo,
        main: *mut c_void,
    ) -> OSStatus {
        if device != DEVICE_OBJECT {
            return kAudioHardwareBadObjectError;
        }
        if self.faulted.load(Ordering::Acquire) {
            self.io.set_faulted();
            // SAFETY: as for this function.
            unsafe { self.silence(op, frames, main) };
            return kAudioHardwareNoError;
        }
        // SAFETY: as for this function.
        unsafe { self.io.do_io(stream, op, frames, cycle, main) }
    }

    /// Fills an input buffer with silence; output needs nothing.
    ///
    /// # Safety
    /// As for [`Driver::do_io`].
    pub(crate) unsafe fn silence(&self, op: u32, frames: u32, main: *mut c_void) {
        if op != kAudioServerPlugInIOOperationReadInput || main.is_null() {
            return;
        }
        let channels = self.input_channels.load(Ordering::Acquire) as usize;
        // SAFETY: the buffer holds `frames` frames of `channels` floats.
        unsafe { std::ptr::write_bytes(main as *mut f32, 0, frames as usize * channels) };
    }
}

/// The driver as its link sees it. Driver objects are never freed, so the
/// link may keep the driver for the rest of the process.
struct DriverSink(&'static Driver);

impl LinkSink for DriverSink {
    fn attach(&self, a: Option<Box<Attachment>>) -> Option<Box<Attachment>> {
        self.0.io.attach(a)
    }

    fn quiescent(&self) -> bool {
        self.0.io.quiescent()
    }

    fn current_generation(&self) -> Option<u64> {
        self.0.io.attached_generation()
    }

    fn published_config(&self) -> DriverConfig {
        self.0.model.published().config.clone()
    }

    fn stage(&self, cfg: DriverConfig) -> ConfigPlan {
        self.0.stage(cfg)
    }

    fn request_config_change(&self) -> OSStatus {
        self.0.host.request_config_change(DEVICE_OBJECT, CONFIG_CHANGE_ACTION)
    }

    fn names_changed(&self) {
        let names = |scope| AudioObjectPropertyAddress {
            mSelector: kAudioObjectPropertyElementName,
            mScope: scope,
            mElement: kAudioObjectPropertyElementWildcard,
        };
        let addresses =
            [names(kAudioObjectPropertyScopeInput), names(kAudioObjectPropertyScopeOutput)];
        let status = self.0.host.properties_changed(DEVICE_OBJECT, &addresses);
        if status != kAudioHardwareNoError {
            self.0.platform.log(LOG_ERROR, &format!("PropertiesChanged failed ({status})"));
        }
    }

    fn store(&self, cfg: &DriverConfig) {
        let d = self.0;
        let status = d.host.write_to_storage(d.platform, STORAGE_KEY, &cfg.to_storage_string());
        if status != kAudioHardwareNoError {
            d.platform.log(
                LOG_ERROR,
                &format!("cannot store configuration {} ({status})", cfg.config_gen),
            );
        }
    }

    fn set_status(&self, s: LinkStatus) {
        let text = match &s {
            LinkStatus::Absent => "absent".to_owned(),
            LinkStatus::Connecting => "connecting".to_owned(),
            LinkStatus::Attached { generation } => format!("attached, generation {generation:x}"),
            LinkStatus::Incompatible(reason) => format!("incompatible ({reason})"),
        };
        self.0.platform.log(LOG_INFO, &format!("daemon link: {text}"));
    }

    fn now_ns(&self) -> u64 {
        self.0.now_ns()
    }

    fn new_attachment(&self, mapped: MappedRegion) -> Result<Box<Attachment>, LayoutError> {
        self.0.io.new_attachment(mapped)
    }

    fn timebase(&self) -> Timebase {
        self.0.platform.timebase()
    }

    fn io_clients(&self) -> u32 {
        self.0.io.snapshot().io_clients
    }

    fn idle_tick(&self) {
        self.0.io.idle_tick();
    }

    fn running_changed(&self) {
        let goin = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyDeviceIsRunning,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let status = self.0.host.properties_changed(DEVICE_OBJECT, &[goin]);
        if status != kAudioHardwareNoError {
            self.0.platform.log(LOG_ERROR, &format!("PropertiesChanged failed ({status})"));
        }
    }

    fn log(&self, level: u8, msg: &str) {
        self.0.platform.log(level, msg);
    }
}

/// The smallest and largest margin since StartIO, as `min..max` frames, or
/// `-` before the first IO cycle.
struct Margins(i64, i64);

impl std::fmt::Display for Margins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0 > self.1 { f.write_str("-") } else { write!(f, "{}..{}", self.0, self.1) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::stub::StubPlatform;
    use crate::testing::null_link_factory;

    #[test]
    fn faulted_input_follows_the_published_channel_count() {
        let d = Driver::new(StubPlatform::new().leak(), null_link_factory(), false);
        let cfg = DriverConfig { input_channels: 2, ..DriverConfig::fallback() };
        *d.pending.lock().unwrap() = Some(cfg);
        assert_eq!(d.perform_config_change(DEVICE_OBJECT, 0), 0);
        assert_eq!(d.model.published().config.input_channels, 2);
        d.faulted.store(true, Ordering::Release);

        // Two channels of 16 frames are zeroed; the rest of the buffer is not
        // the HAL's and stays untouched.
        let mut buf = vec![0.5f32; 8 * 16];
        let cycle = IOCycleInfo::default();
        let op = kAudioServerPlugInIOOperationReadInput;
        let main = buf.as_mut_ptr().cast();
        assert_eq!(unsafe { d.do_io(DEVICE_OBJECT, 0, op, 16, &cycle, main) }, 0);
        assert!(buf[..2 * 16].iter().all(|&x| x == 0.0));
        assert!(buf[2 * 16..].iter().all(|&x| x == 0.5));
    }
}
