//! A simulated Core Audio HAL: one driver object of the `ovsc-hal`
//! rlib, driven only through its extern "C" vtable, the way coreaudiod's IO
//! thread drives the bundle.
//!
//! [`Driver`] creates and initializes the object with a fake host and the
//! stub platform on the daemon's host clock (`CLOCK_MONOTONIC` on Linux,
//! `CLOCK_UPTIME_RAW` on macOS) at Apple silicon's timebase, and reads its
//! properties as the HAL would. [`HalThread`] is the IO
//! thread: for each IO session (StartIO to StopIO) it calls GetZeroTimeStamp
//! every cycle, estimates the device rate from the last two time stamps
//! (the Raw clock algorithm uses them as they are), sleeps until the next
//! wake of N frames, and runs ReadInput at `T - N - S_in` and WriteMix at
//! `T + N + S_out`, with the safety offsets the device reports (the test
//! may play with a smaller output offset for a while). WriteMix
//! plays the PRBS24 signal of device time; every input frame is compared
//! with what was played at the same device time. A configuration change the
//! driver requests is performed as the HAL does it: IO stops, Perform runs,
//! IO starts again with the new layout.
//!
//! Everything the thread sees goes into a [`Record`] for the test to check:
//! one summary per cycle, the zero time stamps, the configuration changes,
//! the host calls and every way the driver broke what the HAL relies on.
//! The thread is real-time, as the HAL's IO thread is, and never waits for
//! the test to read the record.

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::thread::{self, JoinHandle};

use ovsc_clock::local_now_ns;
use ovsc_core::rt::{self, RtClass};
use ovsc_hal::LinkFactory;
use ovsc_hal::abi::*;
use ovsc_hal::model::DriverConfig;
use ovsc_hal::new_driver_object;
use ovsc_hal::platform::stub::{self, StubPlatform};
use ovsc_hal::platform::{Platform, Timebase};
use ovsc_hal::testing::{self, FakeHost, HostCall};
use ovsc_ipc::protocol::STORAGE_KEY;

use crate::prbs;

pub const DEVICE: AudioObjectID = 2;
pub const INPUT_STREAM: AudioObjectID = 3;
pub const OUTPUT_STREAM: AudioObjectID = 4;
/// The driver's zero time stamp period, frames.
pub const PERIOD: f64 = 16384.0;
/// Apple silicon's timebase: host times go through a real tick conversion.
pub const TIMEBASE: Timebase = Timebase { numer: 125, denom: 3 };
/// `kAudioTimeStampSampleTimeValid | kAudioTimeStampHostTimeValid`.
const SAMPLE_AND_HOST_TIME_VALID: u32 = 3;
/// The client ID the IO calls carry.
const CLIENT: u32 = 1;
/// The most HAL errors kept; later ones are only counted.
const MAX_ERRORS: usize = 100;

/// How the device looks to the HAL: what it reads before starting IO.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub sample_rate: u32,
    pub inputs: usize,
    pub outputs: usize,
    /// The device's input and output safety offsets, frames.
    pub input_safety: i64,
    pub output_safety: i64,
}

/// A driver object as the HAL holds it.
pub struct Driver {
    object: *mut c_void,
    vt: &'static DriverInterface,
    pub host: &'static FakeHost,
    pub platform: &'static StubPlatform,
}

// SAFETY: the HAL calls a driver object from several threads at once, and
// everything in it is thread-safe; so are the leaked host and platform.
unsafe impl Send for Driver {}
// SAFETY: as above.
unsafe impl Sync for Driver {}

fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

impl Driver {
    /// A new driver object whose link reaches the daemon through `link`,
    /// initialized with `stored` in the host's storage, as a previous run of
    /// the driver would have left it (nothing at a first install).
    pub fn new(link: LinkFactory, stored: Option<&DriverConfig>) -> Arc<Driver> {
        let platform = StubPlatform::with_clock(local_now_ns).leak();
        platform.set_timebase(TIMEBASE.numer, TIMEBASE.denom);
        let host = FakeHost::new();
        if let Some(cfg) = stored {
            host.set_storage(STORAGE_KEY, &cfg.to_storage_string());
        }
        let object = new_driver_object(platform, link);
        // SAFETY: the object comes from new_driver_object.
        let vt = unsafe { testing::interface(object) };
        // SAFETY: a live driver object and a host for the life of the process.
        let status = unsafe { (vt.Initialize)(object, host.host_ref()) };
        assert_eq!(status, 0, "Initialize");
        Arc::new(Driver { object, vt, host, platform })
    }

    /// GetPropertyData of a value of type `T` (an integer, a float, an
    /// ASBD, a CFString reference).
    fn get<T: Copy>(&self, obj: AudioObjectID, selector: u32, scope: u32) -> Result<T, OSStatus> {
        let a = address(selector, scope);
        let mut buf = vec![0u8; size_of::<T>()];
        let mut used = 0u32;
        // SAFETY: a live driver object; the buffer holds size_of::<T>() bytes.
        let status = unsafe {
            (self.vt.GetPropertyData)(
                self.object,
                obj,
                1,
                &a,
                0,
                ptr::null(),
                buf.len() as u32,
                &mut used,
                buf.as_mut_ptr().cast(),
            )
        };
        if status != 0 {
            return Err(status);
        }
        assert_eq!(used as usize, buf.len(), "size of {:?}", selector.to_be_bytes());
        // SAFETY: the driver wrote a whole T; T is plain data.
        Ok(unsafe { buf.as_ptr().cast::<T>().read_unaligned() })
    }

    fn get_u32(&self, obj: AudioObjectID, selector: u32, scope: u32) -> u32 {
        self.get(obj, selector, scope).unwrap_or_else(|e| panic!("property {selector:#x}: {e}"))
    }

    /// The status property ('ovst'), released after reading as the HAL
    /// would.
    pub fn ovst(&self) -> String {
        let s: CFStringRef = self
            .get(DEVICE, fourcc(b"ovst"), kAudioObjectPropertyScopeGlobal)
            .expect("the status property");
        // SAFETY: the stub platform made the string, and it is ours (+1).
        let text = unsafe { stub::read_string(s) }.expect("a stub CFString");
        // SAFETY: as above; nothing uses it afterwards.
        unsafe { stub::cf_free(s) };
        text
    }

    /// What the HAL reads about the device before starting IO.
    pub fn layout(&self) -> Layout {
        let rate: f64 = self
            .get(DEVICE, kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal)
            .expect("the nominal sample rate");
        let channels = |stream| {
            let f: AudioStreamBasicDescription = self
                .get(stream, kAudioStreamPropertyVirtualFormat, kAudioObjectPropertyScopeGlobal)
                .expect("the stream format");
            assert_eq!(f.mSampleRate, rate, "stream {stream} rate");
            f.mChannelsPerFrame as usize
        };
        let safety =
            |scope| i64::from(self.get_u32(DEVICE, kAudioDevicePropertySafetyOffset, scope));
        Layout {
            sample_rate: rate as u32,
            inputs: channels(INPUT_STREAM),
            outputs: channels(OUTPUT_STREAM),
            input_safety: safety(kAudioObjectPropertyScopeInput),
            output_safety: safety(kAudioObjectPropertyScopeOutput),
        }
    }

    /// Whether the driver caught a panic.
    pub fn faulted(&self) -> bool {
        // SAFETY: a live driver object.
        unsafe { testing::faulted(self.object) }
    }

    /// Host time of Initialize, ns.
    pub fn initialized_ns(&self) -> u64 {
        // SAFETY: a live driver object.
        unsafe { testing::initialized_ns(self.object) }
    }

    /// The driver's last `n` log lines, for failure messages.
    pub fn log_tail(&self, n: usize) -> String {
        let logs = self.platform.logs();
        let start = logs.len().saturating_sub(n);
        logs[start..].iter().map(|(_, line)| format!("  {line}\n")).collect()
    }

    /// The status line and the log tail, for failure messages.
    pub fn diagnostics(&self) -> String {
        format!("ovst: {}\ndriver log:\n{}", self.ovst(), self.log_tail(30))
    }

    fn add_client(&self) -> OSStatus {
        // SAFETY: a live driver object; the driver ignores the client info.
        unsafe { (self.vt.AddDeviceClient)(self.object, DEVICE, ptr::null()) }
    }

    fn remove_client(&self) -> OSStatus {
        // SAFETY: as above.
        unsafe { (self.vt.RemoveDeviceClient)(self.object, DEVICE, ptr::null()) }
    }

    fn start_io(&self) -> OSStatus {
        // SAFETY: a live driver object.
        unsafe { (self.vt.StartIO)(self.object, DEVICE, CLIENT) }
    }

    fn stop_io(&self) -> OSStatus {
        // SAFETY: a live driver object.
        unsafe { (self.vt.StopIO)(self.object, DEVICE, CLIENT) }
    }

    fn perform(&self, action: u64) -> OSStatus {
        // SAFETY: a live driver object; the driver ignores the change info.
        unsafe {
            (self.vt.PerformDeviceConfigurationChange)(self.object, DEVICE, action, ptr::null_mut())
        }
    }

    fn zero_timestamp(&self) -> Result<(f64, u64, u64), OSStatus> {
        let (mut sample, mut host, mut seed) = (0.0, 0, 0);
        // SAFETY: a live driver object and valid out-pointers.
        let status = unsafe {
            (self.vt.GetZeroTimeStamp)(
                self.object,
                DEVICE,
                CLIENT,
                &mut sample,
                &mut host,
                &mut seed,
            )
        };
        if status == 0 { Ok((sample, host, seed)) } else { Err(status) }
    }

    fn will_do(&self, op: u32) -> Result<(bool, bool), OSStatus> {
        let (mut will, mut in_place) = (0, 0);
        // SAFETY: a live driver object and valid out-pointers.
        let status = unsafe {
            (self.vt.WillDoIOOperation)(self.object, DEVICE, CLIENT, op, &mut will, &mut in_place)
        };
        if status == 0 { Ok((will != 0, in_place != 0)) } else { Err(status) }
    }

    /// Begin, Do and End of one IO operation on `buf`, which must hold
    /// `frames` frames of the stream's channels.
    fn io(
        &self,
        stream: AudioObjectID,
        op: u32,
        frames: u32,
        cycle: &IOCycleInfo,
        buf: &mut [f32],
    ) -> OSStatus {
        let main = buf.as_mut_ptr().cast::<c_void>();
        // SAFETY: a live driver object; `buf` is the operation's buffer, as
        // the HAL passes it.
        unsafe {
            let begin = (self.vt.BeginIOOperation)(self.object, DEVICE, CLIENT, op, frames, cycle);
            let done = (self.vt.DoIOOperation)(
                self.object,
                DEVICE,
                stream,
                CLIENT,
                op,
                frames,
                cycle,
                main,
                ptr::null_mut(),
            );
            let end = (self.vt.EndIOOperation)(self.object, DEVICE, CLIENT, op, frames, cycle);
            [begin, done, end].into_iter().find(|s| *s != 0).unwrap_or(0)
        }
    }
}

/// One IO cycle, as the HAL thread saw it.
#[derive(Clone, Debug)]
pub struct Cycle {
    /// The IO session, counted from 1.
    pub session: u32,
    /// Host time the cycle was due, ns.
    pub due_ns: u64,
    /// How late the thread woke for it, ns.
    pub late_ns: u64,
    /// How long the cycle took, from the wake to the end of WriteMix, ns.
    pub busy_ns: u64,
    /// Device sample time of the wake.
    pub time: i64,
    /// Device sample time of the first frame WriteMix wrote.
    pub output_time: i64,
    pub frames: u32,
    pub inputs: usize,
    /// Input channel-frames carrying the output of the same device time.
    pub matched: u64,
    /// Others that were silence.
    pub silent: u64,
    /// Others: audio from another time or channel, or garbage.
    pub wrong: u64,
    /// Input channel-frames that were not silence, matched or not.
    pub nonzero: u64,
    /// The first wrong channel-frame: (channel, device time, value).
    pub first_wrong: Option<(usize, i64, f32)>,
    /// The device times of the first and the last input frame with a
    /// channel that did not match.
    pub unmatched: Option<(i64, i64)>,
}

impl Cycle {
    /// Every input frame of every channel carried what was played at its
    /// device time: bit-exact at device-time delay 0.
    pub fn exact(&self) -> bool {
        self.matched == u64::from(self.frames) * self.inputs as u64
    }
}

/// A zero time stamp the driver handed out for the first time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Knot {
    pub session: u32,
    pub sample: f64,
    pub host_ns: u64,
    pub seed: u64,
}

/// An IO session: StartIO until StopIO.
#[derive(Clone, Copy, Debug)]
pub struct Session {
    pub started_ns: u64,
    pub layout: Layout,
}

/// A configuration change the driver asked for, and its Perform.
#[derive(Clone, Copy, Debug)]
pub struct Perform {
    pub at_ns: u64,
    pub action: u64,
    pub status: OSStatus,
    pub before: Layout,
    pub after: Layout,
}

/// Everything the HAL thread saw.
#[derive(Debug, Default)]
pub struct Record {
    pub sessions: Vec<Session>,
    pub cycles: Vec<Cycle>,
    pub knots: Vec<Knot>,
    pub performs: Vec<Perform>,
    pub host_calls: Vec<HostCall>,
    /// What broke the HAL's expectations: failed calls, zero time stamps
    /// that are not consecutive, in the future or moved, and the like.
    pub errors: Vec<String>,
    /// Errors beyond the first `MAX_ERRORS`.
    pub more_errors: u64,
}

impl Record {
    fn error(&mut self, e: String) {
        if self.errors.len() < MAX_ERRORS {
            self.errors.push(e);
        } else {
            self.more_errors += 1;
        }
    }

    /// Moves everything in `newer` to the end of this record.
    fn append(&mut self, newer: &mut Record) {
        self.sessions.append(&mut newer.sessions);
        self.cycles.append(&mut newer.cycles);
        self.knots.append(&mut newer.knots);
        self.performs.append(&mut newer.performs);
        self.host_calls.append(&mut newer.host_calls);
        for e in newer.errors.drain(..) {
            self.error(e);
        }
        self.more_errors += std::mem::take(&mut newer.more_errors);
    }

    /// The cycles due in `[from_ns, to_ns)`.
    pub fn cycles_between(&self, from_ns: u64, to_ns: u64) -> impl Iterator<Item = &Cycle> {
        self.cycles.iter().filter(move |c| (from_ns..to_ns).contains(&c.due_ns))
    }

    /// The zero time stamps first handed out in `[from_ns, to_ns)`.
    pub fn knots_between(&self, from_ns: u64, to_ns: u64) -> impl Iterator<Item = &Knot> {
        self.knots.iter().filter(move |k| (from_ns..to_ns).contains(&k.host_ns))
    }

    /// The latest host time a cycle was due, 0 if none.
    pub fn last_due_ns(&self) -> u64 {
        self.cycles.last().map_or(0, |c| c.due_ns)
    }
}

/// `Shared::output_safety` when WriteMix plays with the device's offset.
const DEVICE_OUTPUT_SAFETY: i64 = i64::MIN;

struct Shared {
    stop: AtomicBool,
    /// The output safety offset WriteMix plays with instead of the
    /// device's, frames.
    output_safety: AtomicI64,
    record: Mutex<Record>,
}

impl Shared {
    fn record(&self) -> MutexGuard<'_, Record> {
        self.record.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Hands what the IO thread saw since the last time to the record,
    /// unless the test is reading it: the IO thread never waits for the
    /// test, and tries again next cycle.
    fn offer(&self, seen: &mut Record) {
        match self.record.try_lock() {
            Ok(mut r) => r.append(seen),
            Err(TryLockError::Poisoned(e)) => e.into_inner().append(seen),
            Err(TryLockError::WouldBlock) => {}
        }
    }
}

/// The simulated HAL IO thread of one driver.
pub struct HalThread {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl HalThread {
    /// Adds a client and starts IO with buffers of `frames` frames.
    pub fn start(driver: Arc<Driver>, frames: u32) -> HalThread {
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            output_safety: AtomicI64::new(DEVICE_OUTPUT_SAFETY),
            record: Mutex::default(),
        });
        let s = shared.clone();
        let thread = thread::Builder::new()
            .name("hal-io".into())
            .spawn(move || run(&driver, &s, frames))
            .expect("spawn the HAL thread");
        HalThread { shared, thread: Some(thread) }
    }

    /// What the thread has seen so far.
    pub fn record(&self) -> MutexGuard<'_, Record> {
        self.shared.record()
    }

    /// From the next cycle on, WriteMix plays `frames` ahead of the output
    /// time instead of the device's output safety offset, or with the
    /// device's again (`None`). Frames the device's offset already covered
    /// are written again; frames a smaller offset skips are never written.
    pub fn set_output_safety(&self, frames: Option<i64>) {
        let frames = frames.unwrap_or(DEVICE_OUTPUT_SAFETY);
        self.shared.output_safety.store(frames, Ordering::Relaxed);
    }

    /// Stops IO, removes the client and ends the thread. Returns everything
    /// it saw.
    pub fn stop(mut self) -> Record {
        self.halt();
        std::mem::take(&mut *self.shared.record())
    }

    fn halt(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let panicked = t.join().is_err();
            if panicked && !thread::panicking() {
                panic!("the HAL thread panicked");
            }
        }
    }
}

impl Drop for HalThread {
    fn drop(&mut self) {
        self.halt();
    }
}

/// The HAL's model of the device clock: the last two zero time stamps.
struct HalClock {
    seed: u64,
    prev: Option<(f64, f64)>,
    /// (sample time, host ns) of the latest.
    last: (f64, f64),
    /// Frames per ns until two time stamps give the rate.
    nominal: f64,
}

impl HalClock {
    fn new(sample: f64, host_ns: u64, seed: u64, sample_rate: u32) -> Self {
        let nominal = f64::from(sample_rate) / 1e9;
        HalClock { seed, prev: None, last: (sample, host_ns as f64), nominal }
    }

    /// Device frames per host ns.
    fn rate(&self) -> f64 {
        match self.prev {
            Some((s0, h0)) => (self.last.0 - s0) / (self.last.1 - h0),
            None => self.nominal,
        }
    }

    /// The host time, ns, at which the device reaches `sample`.
    fn host_ns_at(&self, sample: f64) -> f64 {
        self.last.1 + (sample - self.last.0) / self.rate()
    }

    /// Takes a zero time stamp. Returns whether it is a new one, or what is
    /// wrong with it: one already handed out must not move, a new one must
    /// be the next period, later in host time. A new seed starts a new
    /// timeline, as the HAL re-anchors on it.
    fn take(&mut self, sample: f64, host_ns: u64, seed: u64) -> Result<bool, String> {
        let host = host_ns as f64;
        if seed != self.seed {
            *self = HalClock { seed, prev: None, last: (sample, host), nominal: self.nominal };
            return Ok(true);
        }
        if sample == self.last.0 {
            return if host == self.last.1 {
                Ok(false)
            } else {
                Err(format!("the time stamp at {sample} moved from {} to {host} ns", self.last.1))
            };
        }
        let previous = self.last;
        self.prev = Some(previous);
        self.last = (sample, host);
        if sample != previous.0 + PERIOD {
            return Err(format!("time stamp {sample} follows {}: not consecutive", previous.0));
        }
        if host <= previous.1 {
            return Err(format!("time stamp {sample} at {host} ns, not after {} ns", previous.1));
        }
        Ok(true)
    }
}

/// An AudioTimeStamp with a sample and a host time.
fn timestamp(sample: f64, host_ns: f64, rate_scalar: f64) -> AudioTimeStamp {
    AudioTimeStamp {
        mSampleTime: sample,
        mHostTime: TIMEBASE.ns_to_ticks_ceil(host_ns.max(0.0) as u64),
        mRateScalar: rate_scalar,
        mFlags: SAMPLE_AND_HOST_TIME_VALID,
        ..Default::default()
    }
}

/// Sleeps until the host clock reads `due_ns`, never waking before.
fn sleep_until(due_ns: u64) {
    while local_now_ns() < due_ns {
        rt::sleep_until(due_ns);
    }
}

/// The IO thread. Like the HAL's it runs real-time, with the scheduling
/// the device engine gives its own audio threads (best effort). It keeps
/// what it sees to itself until the record is free, so it never waits for
/// the test.
fn run(d: &Driver, shared: &Shared, n: u32) {
    rt::raise_priority("simulated HAL IO", RtClass::Transmit);
    let mut seen = Record::default();
    io_loop(d, shared, n, &mut seen);
    if d.remove_client() != 0 {
        seen.error("RemoveDeviceClient failed".into());
    }
    shared.record().append(&mut seen);
}

fn io_loop(d: &Driver, shared: &Shared, n: u32, seen: &mut Record) {
    if d.add_client() != 0 {
        seen.error("AddDeviceClient failed".into());
    }
    let nf = n as usize;
    let n64 = i64::from(n);
    let mut session = 0u32;
    let mut counter = 0u64;
    'sessions: while !shared.stop.load(Ordering::Acquire) {
        session += 1;
        let layout = d.layout();
        let status = d.start_io();
        if status != 0 {
            seen.error(format!("StartIO failed: {status}"));
            break;
        }
        let read = kAudioServerPlugInIOOperationReadInput;
        let mix = kAudioServerPlugInIOOperationWriteMix;
        for (op, wanted) in [(read, layout.inputs > 0), (mix, layout.outputs > 0)] {
            match d.will_do(op) {
                Ok(w) if w == (wanted, true) => {}
                other => seen.error(format!("WillDoIOOperation {:?}: {other:?}", op.to_be_bytes())),
            }
        }
        let started_ns = local_now_ns();
        seen.sessions.push(Session { started_ns, layout });

        let (sample, ticks, seed) = match d.zero_timestamp() {
            Ok(z) => z,
            Err(e) => {
                seen.error(format!("GetZeroTimeStamp failed: {e}"));
                if d.stop_io() != 0 {
                    seen.error("StopIO failed".into());
                }
                break;
            }
        };
        let host_ns = TIMEBASE.ticks_to_ns(ticks);
        let mut clock = HalClock::new(sample, host_ns, seed, layout.sample_rate);
        seen.knots.push(Knot { session, sample, host_ns, seed });

        let (inputs, outputs) = (layout.inputs, layout.outputs);
        let mut input = vec![0f32; nf * inputs];
        let mut output = vec![0f32; nf * outputs];
        let mut c = 1i64;
        loop {
            if shared.stop.load(Ordering::Acquire) {
                if d.stop_io() != 0 {
                    seen.error("StopIO failed".into());
                }
                break 'sessions;
            }

            // The driver asks for configuration changes from its own
            // queue; the HAL stops IO, performs them and starts IO again.
            let calls = d.host.take_calls();
            let requests: Vec<u64> = calls
                .iter()
                .filter_map(|call| match call {
                    HostCall::RequestDeviceConfigurationChange { device, action } => {
                        if *device != DEVICE {
                            seen.error(format!(
                                "configuration change requested for object {device}"
                            ));
                        }
                        Some(*action)
                    }
                    _ => None,
                })
                .collect();
            seen.host_calls.extend(calls);
            if !requests.is_empty() {
                if d.stop_io() != 0 {
                    seen.error("StopIO failed".into());
                }
                for action in requests {
                    let status = d.perform(action);
                    let after = d.layout();
                    let at_ns = local_now_ns();
                    let p = Perform { at_ns, action, status, before: layout, after };
                    seen.performs.push(p);
                }
                continue 'sessions;
            }

            // Wake when the device reaches T = c * N.
            let t = c * n64;
            let due = clock.host_ns_at(t as f64);
            let due_ns = due.ceil() as u64;
            sleep_until(due_ns);
            let woke_ns = local_now_ns();
            let late_ns = woke_ns.saturating_sub(due_ns);

            match d.zero_timestamp() {
                Ok((sample, ticks, seed)) => {
                    let now_ticks = d.platform.now_ticks();
                    if ticks > now_ticks {
                        seen.error(format!(
                            "time stamp {sample} at tick {ticks}, after now {now_ticks}"
                        ));
                    }
                    let host_ns = TIMEBASE.ticks_to_ns(ticks);
                    match clock.take(sample, host_ns, seed) {
                        Ok(true) => seen.knots.push(Knot { session, sample, host_ns, seed }),
                        Ok(false) => {}
                        Err(e) => seen.error(e),
                    }
                }
                Err(e) => seen.error(format!("GetZeroTimeStamp failed: {e}")),
            }

            let rate = clock.rate();
            let scalar = rate / clock.nominal;
            let output_safety = match shared.output_safety.load(Ordering::Relaxed) {
                DEVICE_OUTPUT_SAFETY => layout.output_safety,
                frames => frames,
            };
            let t_in = t - n64 - layout.input_safety;
            let t_out = t + n64 + output_safety;
            let ticks_per_frame = f64::from(TIMEBASE.denom) / (f64::from(TIMEBASE.numer) * rate);
            counter += 1;
            let cycle = IOCycleInfo {
                mIOCycleCounter: counter,
                mNominalIOBufferFrameSize: n,
                mCurrentTime: timestamp(t as f64, due, scalar),
                mInputTime: timestamp(t_in as f64, clock.host_ns_at(t_in as f64), scalar),
                mOutputTime: timestamp(t_out as f64, clock.host_ns_at(t_out as f64), scalar),
                mMainHostTicksPerFrame: ticks_per_frame,
                mDeviceHostTicksPerFrame: ticks_per_frame,
            };

            // ReadInput, into a buffer the driver must fill completely.
            input.fill(f32::NAN);
            if inputs > 0 && d.io(INPUT_STREAM, read, n, &cycle, &mut input) != 0 {
                seen.error(format!("ReadInput failed at {t}"));
            }
            let mut result = Cycle {
                session,
                due_ns,
                late_ns,
                busy_ns: 0,
                time: t,
                output_time: t_out,
                frames: n,
                inputs,
                matched: 0,
                silent: 0,
                wrong: 0,
                nonzero: 0,
                first_wrong: None,
                unmatched: None,
            };
            for (i, frame) in input.chunks_exact(inputs.max(1)).enumerate() {
                let td = t_in + i as i64;
                let mut matched = true;
                for (ch, &x) in frame.iter().enumerate() {
                    if x != 0.0 {
                        result.nonzero += 1;
                    }
                    if td >= 0 && x.to_bits() == prbs::sample(ch, td as u64).to_bits() {
                        result.matched += 1;
                        continue;
                    }
                    matched = false;
                    if x == 0.0 {
                        result.silent += 1;
                    } else {
                        result.wrong += 1;
                        result.first_wrong.get_or_insert((ch, td, x));
                    }
                }
                if !matched {
                    let first = result.unmatched.map_or(td, |(first, _)| first);
                    result.unmatched = Some((first, td));
                }
            }

            // WriteMix: the signal of device time, on every channel.
            for (i, frame) in output.chunks_exact_mut(outputs.max(1)).enumerate() {
                let td = (t_out + i as i64) as u64;
                for (ch, x) in frame.iter_mut().enumerate() {
                    *x = prbs::sample(ch, td);
                }
            }
            if outputs > 0 && d.io(OUTPUT_STREAM, mix, n, &cycle, &mut output) != 0 {
                seen.error(format!("WriteMix failed at {t}"));
            }
            result.busy_ns = local_now_ns().saturating_sub(woke_ns);
            seen.cycles.push(result);
            shared.offer(seen);
            c += 1;
        }
    }
}
