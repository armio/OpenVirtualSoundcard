//! The IO engine: zero time stamps and DoIOOperation on the daemon's clock
//! and shared region (design sections 7.4, 8.2, 8.3, 11 and 13).
//!
//! GetZeroTimeStamp reads the daemon's clock block and status from the
//! current attachment and runs the device timeline: the timeline turns the
//! daemon's media clock into a continuous device sample clock plus a ring
//! offset `off` (ring index = device sample time + off), and decides whether
//! audio may flow at all (the gate). It publishes `off`, the gate and a copy
//! of its model for the IO thread, and converts its zero time stamp to host
//! ticks for the HAL.
//!
//! DoIOOperation then only moves samples: ReadInput copies RX ring slots
//! `t + off - read_delay` into the HAL's buffer and WriteMix stores the
//! mixed output at TX ring slots `t + off`, both for as long as the gate is
//! open for the attachment they hold. Otherwise input is silence and output
//! is dropped.
//!
//! The real-time paths (`zero_timestamp`, `will_do`, `do_io`) never
//! allocate, lock, log or call the host. Their only system call is reading
//! the host clock. Concurrent GetZeroTimeStamp callers never wait: the one
//! that gets the timeline computes, the others return the last time stamp.

#![deny(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects
)]

mod attach;
mod trace;
mod zts;

use crate::atomic::update_u32;
use std::ffi::c_void;
use std::slice;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};

use ovsc_ipc::region::MappedRegion;
use ovsc_shm::clock::{ClockRead, ClockRecord, READ_TRIES};
use ovsc_shm::layout::{IO_FRAMES_CAP, LayoutError};
use ovsc_shm::sample::{from_f32, to_f32};
use ovsc_shm::status::{
    AudioWord, ChannelsWord, DAEMON_ENGINE_RUNNING, DAEMON_SHUTTING_DOWN, PluginStatus,
    REGIME_FOLLOWING, REGIME_HOLDOVER,
};
use ovsc_shm::timeline::{ClockInput, DeviceTimeline, Regime, TimelineParams, TimelineStatus, Zts};

pub use attach::{AttachGuard, AttachSlot, Attachment, CLOCK_BASE_TOLERANCE_NS, Retired};
pub use zts::ZtsTriple;

use crate::abi::*;
use crate::model::DriverConfig;
use crate::platform::{Platform, Timebase};
use trace::{Trace, TraceOp};
use zts::{ModelCell, TryLock, ZtsCache};

/// The most frames per packet the daemon's TX sends (ovsc-core's
/// `FPP_MAX`); output must be in the ring that much before its deadline.
pub const FPP_MAX: u32 = 32;

/// The TX guard assumed until the daemon reports its own, microseconds.
const DEFAULT_TX_GUARD_US: u64 = 500;

/// An IO cycle whose time is at least this far from the device's time in
/// the safe direction (input from that long ago, output for that far ahead)
/// is counted as far: half a zero time stamp period, so a HAL that reads
/// our time stamps one period off shows up.
const FAR_MARGIN: i64 = TimelineParams::DEFAULT.period as i64 / 2;

/// The timeline tuning for a configuration: the defaults, plus the
/// configuration's debug jitter.
fn timeline_params(cfg: &DriverConfig) -> TimelineParams {
    TimelineParams { debug_zts_jitter_ns: cfg.debug_zts_jitter_ns, ..TimelineParams::DEFAULT }
}

/// The IO paths' copy of the configuration.
struct IoConfig {
    sample_rate: AtomicU32,
    input_channels: AtomicU32,
    output_channels: AtomicU32,
    /// Frames ReadInput reads behind the input time.
    read_delay: AtomicU32,
    /// L, the receive latency in frames, for the input margin. Estimated
    /// from the configuration, then the daemon's value.
    latency_samples: AtomicU32,
    /// The daemon's TX guard in frames, for the output margin. Estimated,
    /// then the daemon's value.
    guard: AtomicU32,
    fpp_max: AtomicU32,
}

impl IoConfig {
    fn new(cfg: &DriverConfig) -> Self {
        let c = Self {
            sample_rate: AtomicU32::new(0),
            input_channels: AtomicU32::new(0),
            output_channels: AtomicU32::new(0),
            read_delay: AtomicU32::new(0),
            latency_samples: AtomicU32::new(0),
            guard: AtomicU32::new(0),
            fpp_max: AtomicU32::new(FPP_MAX),
        };
        c.set(cfg);
        c
    }

    fn set(&self, cfg: &DriverConfig) {
        self.sample_rate.store(cfg.sample_rate, Ordering::Release);
        self.input_channels.store(cfg.input_channels, Ordering::Release);
        self.output_channels.store(cfg.output_channels, Ordering::Release);
        self.read_delay.store(cfg.input_read_delay, Ordering::Release);
        // In latency mode the read delay is L; otherwise the output latency
        // is, unless overridden. Only the margins use it.
        let latency =
            if cfg.input_read_delay > 0 { cfg.input_read_delay } else { cfg.output_latency };
        self.latency_samples.store(latency, Ordering::Relaxed);
        let guard = DEFAULT_TX_GUARD_US
            .saturating_mul(u64::from(cfg.sample_rate))
            .checked_div(1_000_000)
            .unwrap_or(0);
        self.guard.store(u32::try_from(guard).unwrap_or(u32::MAX), Ordering::Relaxed);
    }
}

/// What only the holder of the timeline lock touches.
struct Clocked {
    timeline: DeviceTimeline,
    /// The last consistent clock record, and the daemon generation it came
    /// from. Used when a read of the clock block finds it contended.
    last_record: Option<(u64, ClockRecord)>,
    /// The last zero time stamp the timeline gave, and the triple handed out
    /// for it.
    last_knot: Option<Zts>,
    last_triple: ZtsTriple,
    clock_read_failures: u64,
}

impl Clocked {
    fn new(params: TimelineParams) -> Self {
        Self {
            timeline: DeviceTimeline::new(params),
            last_record: None,
            last_knot: None,
            last_triple: (0.0, 0, 1),
            clock_read_failures: 0,
        }
    }

    /// The triple for `knot`: host time in ticks, rounded up, never later
    /// than `now_ticks`, and strictly increasing within a seed. A new knot
    /// that cannot be both waits for a later call, which returns the
    /// previous triple meanwhile.
    fn triple(&mut self, knot: Zts, tb: Timebase, now_ticks: u64) -> ZtsTriple {
        if self.last_knot == Some(knot) {
            return self.last_triple;
        }
        let (_, prev_ticks, prev_seed) = self.last_triple;
        let mut ticks = tb.ns_to_ticks_ceil(knot.host_ns).min(now_ticks);
        if self.last_knot.is_some() && prev_seed == knot.seed && ticks <= prev_ticks {
            if prev_ticks >= now_ticks {
                return self.last_triple;
            }
            ticks = prev_ticks.saturating_add(1);
        }
        self.last_knot = Some(knot);
        self.last_triple = (knot.sample_time as f64, ticks, knot.seed);
        self.last_triple
    }
}

/// What the zero time stamp path last published, for snapshots.
struct ZtsStatus {
    seed: AtomicU64,
    regime: AtomicU64,
    absorbs: AtomicU64,
    seed_bumps: AtomicU64,
    phase_error_ns: AtomicI64,
    device_rate_ppm_milli: AtomicI64,
    clock_read_failures: AtomicU64,
}

impl ZtsStatus {
    const fn new() -> Self {
        Self {
            seed: AtomicU64::new(1),
            regime: AtomicU64::new(0),
            absorbs: AtomicU64::new(0),
            seed_bumps: AtomicU64::new(0),
            phase_error_ns: AtomicI64::new(0),
            device_rate_ppm_milli: AtomicI64::new(0),
            clock_read_failures: AtomicU64::new(0),
        }
    }

    fn store(&self, s: &TimelineStatus, clock_read_failures: u64) {
        self.seed.store(s.seed, Ordering::Relaxed);
        self.regime.store(s.regime.code(), Ordering::Relaxed);
        self.absorbs.store(s.absorbs, Ordering::Relaxed);
        self.seed_bumps.store(s.seed_bumps, Ordering::Relaxed);
        self.phase_error_ns.store(s.phase_error_ns, Ordering::Relaxed);
        self.device_rate_ppm_milli.store(s.device_rate_ppm_milli, Ordering::Relaxed);
        self.clock_read_failures.store(clock_read_failures, Ordering::Relaxed);
    }
}

/// The IO thread's counters (plug-in status line 0), kept here so they
/// survive a change of attachment and copied into the region after every
/// operation.
struct IoCounters {
    read_calls: AtomicU64,
    write_calls: AtomicU64,
    input_frames: AtomicU64,
    input_missing: AtomicU64,
    output_frames: AtomicU64,
    silenced_cycles: AtomicU64,
    late_output_cycles: AtomicU64,
    early_input_cycles: AtomicU64,
    min_output_margin: AtomicI64,
    min_input_margin: AtomicI64,
    max_output_margin: AtomicI64,
    max_input_margin: AtomicI64,
    far_output_cycles: AtomicU64,
    far_input_cycles: AtomicU64,
    max_frames: AtomicU64,
    last_frames: AtomicU64,
    io_heartbeat_ns: AtomicU64,
    frames_capped: AtomicU64,
}

impl IoCounters {
    const fn new() -> Self {
        Self {
            read_calls: AtomicU64::new(0),
            write_calls: AtomicU64::new(0),
            input_frames: AtomicU64::new(0),
            input_missing: AtomicU64::new(0),
            output_frames: AtomicU64::new(0),
            silenced_cycles: AtomicU64::new(0),
            late_output_cycles: AtomicU64::new(0),
            early_input_cycles: AtomicU64::new(0),
            min_output_margin: AtomicI64::new(i64::MAX),
            min_input_margin: AtomicI64::new(i64::MAX),
            max_output_margin: AtomicI64::new(i64::MIN),
            max_input_margin: AtomicI64::new(i64::MIN),
            far_output_cycles: AtomicU64::new(0),
            far_input_cycles: AtomicU64::new(0),
            max_frames: AtomicU64::new(0),
            last_frames: AtomicU64::new(0),
            io_heartbeat_ns: AtomicU64::new(0),
            frames_capped: AtomicU64::new(0),
        }
    }

    fn reset_margins(&self) {
        self.min_output_margin.store(i64::MAX, Ordering::Relaxed);
        self.min_input_margin.store(i64::MAX, Ordering::Relaxed);
        self.max_output_margin.store(i64::MIN, Ordering::Relaxed);
        self.max_input_margin.store(i64::MIN, Ordering::Relaxed);
    }

    /// Copies the counters into plug-in status line 0.
    fn publish(&self, p: &PluginStatus, faulted: bool) {
        let copy = |from: &AtomicU64, to: &AtomicU64| {
            to.store(from.load(Ordering::Relaxed), Ordering::Relaxed);
        };
        copy(&self.read_calls, &p.read_calls);
        copy(&self.write_calls, &p.write_calls);
        copy(&self.input_frames, &p.input_frames);
        copy(&self.input_missing, &p.input_missing);
        copy(&self.output_frames, &p.output_frames);
        copy(&self.silenced_cycles, &p.silenced_cycles);
        copy(&self.late_output_cycles, &p.late_output_cycles);
        copy(&self.early_input_cycles, &p.early_input_cycles);
        let min_out = self.min_output_margin.load(Ordering::Relaxed);
        p.min_output_margin.store(min_out as u64, Ordering::Relaxed);
        let min_in = self.min_input_margin.load(Ordering::Relaxed);
        p.min_input_margin.store(min_in as u64, Ordering::Relaxed);
        copy(&self.max_frames, &p.max_frames);
        copy(&self.last_frames, &p.last_frames);
        copy(&self.io_heartbeat_ns, &p.io_heartbeat_ns);
        p.faulted.store(u64::from(faulted), Ordering::Relaxed);
        copy(&self.frames_capped, &p.frames_capped);
    }
}

/// The engine's state for the status property and tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoSnapshot {
    /// StartIO minus StopIO.
    pub io_clients: u32,
    pub sample_rate: u32,
    pub seed: u64,
    pub zts_calls: u64,
    pub regime: Regime,
    /// Discontinuities that moved the ring offset.
    pub absorbs: u64,
    pub seed_bumps: u64,
    /// The device's rate against the host clock, in thousandths of a ppm.
    pub device_rate_ppm_milli: i64,
    /// Ring index minus device sample time.
    pub media_offset: i64,
    /// Whether audio flows through the current attachment.
    pub gate: bool,
    /// The generation of the daemon whose region is attached.
    pub attached_generation: Option<u64>,
    pub read_calls: u64,
    pub write_calls: u64,
    /// Channel-frames read as silence because the ring had no sample.
    pub input_missing: u64,
    /// IO operations zero-filled or dropped because the gate was closed.
    pub silenced_cycles: u64,
    pub late_output_cycles: u64,
    pub early_input_cycles: u64,
    /// Smallest output margin since StartIO, frames (`i64::MAX` if none).
    pub min_output_margin: i64,
    /// Smallest input margin since StartIO, frames (`i64::MAX` if none).
    pub min_input_margin: i64,
    /// Largest output margin since StartIO, frames (`i64::MIN` if none).
    pub max_output_margin: i64,
    /// Largest input margin since StartIO, frames (`i64::MIN` if none).
    pub max_input_margin: i64,
    /// Output cycles at least `FAR_MARGIN` ahead of the device's time.
    pub far_output_cycles: u64,
    /// Input cycles at least `FAR_MARGIN` behind the device's time.
    pub far_input_cycles: u64,
    /// IO operations longer than `IO_FRAMES_CAP`.
    pub frames_capped: u64,
    /// Reads of the clock block that found it contended.
    pub clock_read_failures: u64,
    /// The daemon's TX underruns, 0 when detached.
    pub tx_underruns: u64,
    pub faulted: bool,
}

/// The regime with status code `code`.
fn regime_of(code: u64) -> Regime {
    match code {
        REGIME_FOLLOWING => Regime::Following,
        REGIME_HOLDOVER => Regime::Holdover,
        _ => Regime::Synthetic,
    }
}

/// The zero time stamps and IO of the one device.
pub struct IoEngine {
    platform: &'static dyn Platform,
    config: IoConfig,
    clocked: TryLock<Clocked>,
    /// The last zero time stamp handed out.
    cache: ZtsCache,
    /// The device model, for the IO thread's margins.
    model: ModelCell,
    /// Ring index minus device sample time.
    off: AtomicI64,
    /// The daemon generation for which audio may flow; 0 when the gate is
    /// closed. Keyed by generation, so a new attachment starts closed until
    /// a zero time stamp has checked its clock.
    gate: AtomicU64,
    status: ZtsStatus,
    zts_calls: AtomicU64,
    io_clients: AtomicU32,
    faulted: AtomicBool,
    counters: IoCounters,
    trace: Trace,
    slot: AttachSlot,
}

impl IoEngine {
    /// An engine for `cfg`, detached. The timeline is reset by Initialize.
    pub fn new(cfg: &DriverConfig, platform: &'static dyn Platform) -> Self {
        Self {
            platform,
            config: IoConfig::new(cfg),
            clocked: TryLock::new(Clocked::new(timeline_params(cfg))),
            cache: ZtsCache::new(),
            model: ModelCell::new(),
            off: AtomicI64::new(0),
            gate: AtomicU64::new(0),
            status: ZtsStatus::new(),
            zts_calls: AtomicU64::new(0),
            io_clients: AtomicU32::new(0),
            faulted: AtomicBool::new(false),
            counters: IoCounters::new(),
            trace: Trace::new(),
            slot: AttachSlot::new(),
        }
    }

    /// Takes the channel counts, rate, read delay and timeline tuning of
    /// `cfg`. Only called while the HAL has IO stopped (Initialize and
    /// PerformDeviceConfigurationChange); a new rate also needs
    /// [`IoEngine::reset_timeline`].
    pub fn apply_config(&self, cfg: &DriverConfig) {
        self.config.set(cfg);
        let mut st = self.clocked.lock_spin();
        st.timeline.set_params(timeline_params(cfg));
    }

    /// [`Attachment::new_at`] on this engine's clock: the clock-base check
    /// compares the daemon's heartbeat with the clock the gate checks it
    /// against.
    pub fn new_attachment(&self, mapped: MappedRegion) -> Result<Box<Attachment>, LayoutError> {
        Attachment::new_at(mapped, self.now_ns())
    }

    /// Installs `a` as the current attachment and returns the previous one
    /// for retiring.
    ///
    /// # Retiring
    /// A real-time path that entered before the swap may still be using the
    /// returned attachment. Neutralize its mapping at once, but drop it only
    /// once [`IoEngine::quiescent`] has returned true after this call
    /// (design section 11): dropping it earlier frees memory an IO thread
    /// may be reading. The signature fixed in design section 5.3 cannot
    /// enforce this; [`AttachSlot::swap`] does, for code that holds a slot.
    pub fn attach(&self, a: Option<Box<Attachment>>) -> Option<Box<Attachment>> {
        let old = self.slot.swap(a)?;
        Some(match old.into_box(&self.slot) {
            Ok(old) => old,
            // SAFETY: a reader may still hold it; the caller keeps it until
            // quiescent() (the contract above).
            Err(old) => unsafe { old.into_box_unchecked() },
        })
    }

    /// Whether no real-time path is using an attachment right now. Seen
    /// after [`IoEngine::attach`], the previous attachment may be freed.
    pub fn quiescent(&self) -> bool {
        self.slot.quiescent()
    }

    /// The generation of the attached daemon region.
    pub fn attached_generation(&self) -> Option<u64> {
        self.slot.enter().get().map(|a| a.generation)
    }

    /// StartIO. The first client prefaults the active rings, restarts the
    /// device time near 0 (as Apple's NullAudio does; the ring offset keeps
    /// the ring mapping), resets the margin minima and starts an IO trace
    /// session. Never waits on the daemon.
    ///
    /// Returns whether this was the first client, so the device's 'goin'
    /// changed.
    pub fn start_io(&self) -> bool {
        if self.io_clients.fetch_add(1, Ordering::AcqRel) != 0 {
            return false;
        }
        self.counters.reset_margins();
        let mut st = self.clocked.lock_spin();
        {
            let guard = self.slot.enter();
            let a = guard.get();
            if let Some(a) = a {
                let input = self.config.input_channels.load(Ordering::Acquire);
                let output = self.config.output_channels.load(Ordering::Acquire);
                a.touch(input, output);
                self.counters.publish(a.view.plugin(), self.faulted.load(Ordering::Acquire));
            }
            self.trace.start_session(a);
            let now_ns = self.now_ns();
            let input = a.and_then(|a| self.clock_input(&mut st, a));
            st.timeline.start(now_ns, input.as_ref());
        }
        // The session's first zero time stamp, so that a caller that finds
        // the timeline busy never gets the previous session's.
        self.tick(&mut st);
        true
    }

    /// StopIO; extra calls are ignored. Returns whether this was the last
    /// client, so the device's 'goin' changed.
    pub fn stop_io(&self) -> bool {
        let clients =
            update_u32(&self.io_clients, Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
        clients == Ok(1)
    }

    /// While no client runs IO, keeps the timeline following the daemon's
    /// clock. The status then shows the clock the device would follow, a
    /// daemon that goes away leaves the device in holdover at the rate it
    /// last had, and IO starts with next to nothing to absorb. The link
    /// calls it about once a second, off the real-time paths. Does nothing
    /// while IO runs (every zero time stamp updates the timeline then) or
    /// while another caller holds the timeline.
    pub fn idle_tick(&self) {
        let Some(mut st) = self.clocked.try_lock() else { return };
        // Checked under the lock: StartIO counts its client before taking
        // it, then updates the timeline itself.
        if self.io_clients.load(Ordering::Acquire) != 0 {
            return;
        }
        let guard = self.slot.enter();
        let a = guard.get();
        let now_ns = self.now_ns();
        let input = a.and_then(|a| self.clock_input(&mut st, a));
        st.timeline.update(now_ns, input.as_ref());
        let gate = match &input {
            Some(i) if st.timeline.gate(now_ns, Some(i)) => i.generation,
            _ => 0,
        };
        let s = self.publish(&st, gate);
        if let Some(a) = a {
            let triple = st.last_triple;
            self.write_zts_status(a.view.plugin(), &s, triple, gate != 0, st.clock_read_failures);
        }
    }

    /// Restarts the timeline at `sample_rate` from frame 0, on its own
    /// clock (Initialize and Perform). Every reset after the first changes
    /// the seed.
    pub fn reset_timeline(&self, sample_rate: u32) {
        self.config.sample_rate.store(sample_rate, Ordering::Release);
        let mut st = self.clocked.lock_spin();
        let now_ns = self.now_ns();
        st.timeline.reset(sample_rate, now_ns);
        self.publish(&st, 0);
    }

    /// GetZeroTimeStamp: (sample time, host ticks, seed). Real-time safe.
    ///
    /// Sample times are multiples of the period, host times are ticks
    /// rounded up from the timeline's nanoseconds, never later than now and
    /// strictly increasing for a seed. If another caller is computing, this
    /// returns the last time stamp handed out.
    pub fn zero_timestamp(&self) -> ZtsTriple {
        self.zts_calls.fetch_add(1, Ordering::Relaxed);
        match self.clocked.try_lock() {
            Some(mut st) => self.tick(&mut st),
            None => self.cache.load(),
        }
    }

    /// The last zero time stamp handed out (sample time 0 at host time 0,
    /// seed 1, before the first). Real-time safe.
    pub fn cached_zero_timestamp(&self) -> ZtsTriple {
        self.cache.load()
    }

    /// One zero time stamp, under the timeline lock: read the daemon, run
    /// the timeline, publish what the IO thread needs.
    fn tick(&self, st: &mut Clocked) -> ZtsTriple {
        let now_ticks = self.platform.now_ticks();
        let tb = self.platform.timebase();
        let now_ns = tb.ticks_to_ns(now_ticks);
        let guard = self.slot.enter();
        let a = guard.get();
        let input = a.and_then(|a| self.clock_input(st, a));
        st.timeline.update(now_ns, input.as_ref());
        let knot = st.timeline.zero_timestamp(now_ns);
        let gate = match &input {
            Some(i) if st.timeline.gate(now_ns, Some(i)) => i.generation,
            _ => 0,
        };
        let before = st.last_triple;
        let triple = st.triple(knot, tb, now_ticks);
        if triple != before {
            self.cache.store(triple);
        }
        let s = self.publish(st, gate);
        if let Some(a) = a {
            self.write_zts_status(a.view.plugin(), &s, triple, gate != 0, st.clock_read_failures);
        }
        triple
    }

    /// What the daemon's region says about its clock, as the timeline's
    /// input: `None` without a clock record. Also takes the daemon's
    /// latency and TX guard for the margins.
    fn clock_input(&self, st: &mut Clocked, a: &Attachment) -> Option<ClockInput> {
        let d = a.view.daemon();
        let heartbeat_ns = d.heartbeat_ns.load(Ordering::Acquire);
        let flags = d.flags.load(Ordering::Acquire);
        let audio = AudioWord::unpack(d.audio_word.load(Ordering::Acquire));
        if audio.sample_rate != 0 {
            let channels = ChannelsWord::unpack(d.channels_word.load(Ordering::Relaxed));
            self.config.latency_samples.store(channels.latency_samples, Ordering::Relaxed);
            let guard = u32::try_from(d.tx_guard_samples.load(Ordering::Relaxed));
            self.config.guard.store(guard.unwrap_or(u32::MAX), Ordering::Relaxed);
        }
        let record = match a.view.clock().read_bounded(READ_TRIES) {
            ClockRead::Record(r) => {
                st.last_record = Some((a.generation, r));
                r
            }
            ClockRead::NeverWritten => return None,
            ClockRead::Contended => {
                st.clock_read_failures = st.clock_read_failures.wrapping_add(1);
                match st.last_record {
                    Some((generation, r)) if generation == a.generation => r,
                    _ => return None,
                }
            }
        };
        let running = flags & DAEMON_ENGINE_RUNNING != 0 && flags & DAEMON_SHUTTING_DOWN == 0;
        Some(ClockInput {
            record,
            generation: a.generation,
            heartbeat_ns,
            daemon_rate: audio.sample_rate,
            engine_running: running,
        })
    }

    /// Publishes the timeline's offset, model and status, and `gate`, for
    /// the IO thread and snapshots.
    fn publish(&self, st: &Clocked, gate: u64) -> TimelineStatus {
        let s = st.timeline.status();
        // The offset before the gate: an IO thread that sees the gate open
        // also sees the offset it was opened for.
        self.off.store(s.media_offset, Ordering::Release);
        self.gate.store(gate, Ordering::Release);
        self.model.store(&st.timeline.model());
        self.status.store(&s, st.clock_read_failures);
        s
    }

    /// Plug-in status line 1, written only under the timeline lock.
    fn write_zts_status(
        &self,
        p: &PluginStatus,
        s: &TimelineStatus,
        triple: ZtsTriple,
        gate: bool,
        clock_read_failures: u64,
    ) {
        let r = Ordering::Relaxed;
        p.zts_calls.store(self.zts_calls.load(r), r);
        p.seed.store(s.seed, r);
        p.media_offset.store(s.media_offset as u64, r);
        p.regime.store(s.regime.code(), r);
        p.absorbs.store(s.absorbs, r);
        p.seed_bumps.store(s.seed_bumps, r);
        p.last_zts_sample.store(triple.0 as u64, r);
        p.last_zts_host_ticks.store(triple.1, r);
        p.device_rate_ppm_milli.store(s.device_rate_ppm_milli as u64, r);
        p.phase_error_ns.store(s.phase_error_ns as u64, r);
        p.gate.store(u64::from(gate), r);
        p.clock_read_failures.store(clock_read_failures, r);
    }

    /// WillDoIOOperation: (will do, in place). The device reads input and
    /// writes the final mix, both in place.
    pub fn will_do(&self, op: u32) -> (bool, bool) {
        match op {
            kAudioServerPlugInIOOperationReadInput => {
                (self.config.input_channels.load(Ordering::Acquire) > 0, true)
            }
            kAudioServerPlugInIOOperationWriteMix => {
                (self.config.output_channels.load(Ordering::Acquire) > 0, true)
            }
            _ => (false, true),
        }
    }

    /// DoIOOperation (design sections 8.2 and 8.3). Real-time safe.
    ///
    /// ReadInput fills `main` from the RX rings at input time + off - read
    /// delay; WriteMix stores `main` into the TX rings at output time + off.
    /// While the gate is closed, the sample time is not finite or the engine
    /// is faulted, input is silence and output is dropped. At most
    /// `IO_FRAMES_CAP` frames move; input beyond that is silence.
    ///
    /// # Safety
    /// `main` must be null or the HAL's buffer for this operation: `frames`
    /// interleaved Float32 frames at the stream's channel count, which is
    /// the count of the last [`IoEngine::apply_config`].
    pub unsafe fn do_io(
        &self,
        stream: u32,
        op: u32,
        frames: u32,
        cycle: &IOCycleInfo,
        main: *mut c_void,
    ) -> OSStatus {
        let input = match op {
            kAudioServerPlugInIOOperationReadInput => true,
            kAudioServerPlugInIOOperationWriteMix => false,
            _ => return kAudioHardwareNoError,
        };
        let guard = self.slot.enter();
        let a = guard.get();
        let gate = self.gate.load(Ordering::Acquire);
        let off = self.off.load(Ordering::Acquire);
        let faulted = self.faulted.load(Ordering::Acquire);
        let open = a.filter(|a| gate != 0 && a.generation == gate && !faulted);
        let n = (frames as usize).min(IO_FRAMES_CAP);
        let c = &self.counters;
        if n < frames as usize {
            c.frames_capped.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: the caller's guarantee for `main`.
        let moved = unsafe {
            if input {
                self.read_input(open, off, frames, n, cycle, main)
            } else {
                self.write_mix(open, off, frames, n, cycle, main)
            }
        };

        let now_ticks = self.platform.now_ticks();
        let now_ns = self.platform.timebase().ticks_to_ns(now_ticks);
        if moved {
            self.margins(input, n, cycle, now_ns);
        } else {
            c.silenced_cycles.fetch_add(1, Ordering::Relaxed);
        }
        let frames64 = u64::from(frames);
        if input {
            c.read_calls.fetch_add(1, Ordering::Relaxed);
            c.input_frames.fetch_add(frames64, Ordering::Relaxed);
        } else {
            c.write_calls.fetch_add(1, Ordering::Relaxed);
            c.output_frames.fetch_add(frames64, Ordering::Relaxed);
        }
        c.max_frames.fetch_max(frames64, Ordering::Relaxed);
        c.last_frames.store(frames64, Ordering::Relaxed);
        c.io_heartbeat_ns.store(now_ns, Ordering::Relaxed);
        if let Some(a) = a {
            let op = TraceOp { op, stream, frames, cycle, done_ticks: now_ticks };
            self.trace.record(a, &op);
            c.publish(a.view.plugin(), faulted);
        }
        drop(guard);
        kAudioHardwareNoError
    }

    /// ReadInput. Returns whether audio was read; if not, the buffer is
    /// silence.
    ///
    /// # Safety
    /// As for [`IoEngine::do_io`].
    unsafe fn read_input(
        &self,
        open: Option<&Attachment>,
        off: i64,
        frames: u32,
        n: usize,
        cycle: &IOCycleInfo,
        main: *mut c_void,
    ) -> bool {
        let channels = self.config.input_channels.load(Ordering::Acquire) as usize;
        let len = (frames as usize).saturating_mul(channels);
        if main.is_null() || len == 0 {
            return false;
        }
        // SAFETY: the HAL's ReadInput buffer holds `frames` frames of
        // `channels` floats (the caller's guarantee).
        let buf = unsafe { slice::from_raw_parts_mut(main.cast::<f32>(), len) };
        let t = cycle.mInputTime.mSampleTime;
        let Some(a) = open.filter(|_| t.is_finite()) else {
            buf.fill(0.0);
            return false;
        };
        let read_delay = i64::from(self.config.read_delay.load(Ordering::Relaxed));
        let m0 = (t.floor() as i64).wrapping_add(off).wrapping_sub(read_delay) as u64;
        let mut missing = 0u64;
        // Channel by channel, so each ring is read in order.
        for ch in 0..channels {
            let column = buf.iter_mut().skip(ch).step_by(channels).take(n);
            let Some(ring) = a.view.rx(ch) else {
                column.for_each(|x| *x = 0.0);
                missing = missing.wrapping_add(n as u64);
                continue;
            };
            let mut m = m0;
            for x in column {
                *x = match ring.read_one(m) {
                    Some(s) => to_f32(s),
                    None => {
                        missing = missing.wrapping_add(1);
                        0.0
                    }
                };
                m = m.wrapping_add(1);
            }
        }
        if let Some(rest) = buf.get_mut(n.saturating_mul(channels)..) {
            rest.fill(0.0);
        }
        self.counters.input_missing.fetch_add(missing, Ordering::Relaxed);
        true
    }

    /// WriteMix. Returns whether audio was written; if not, it is dropped.
    ///
    /// # Safety
    /// As for [`IoEngine::do_io`].
    unsafe fn write_mix(
        &self,
        open: Option<&Attachment>,
        off: i64,
        frames: u32,
        n: usize,
        cycle: &IOCycleInfo,
        main: *mut c_void,
    ) -> bool {
        let channels = self.config.output_channels.load(Ordering::Acquire) as usize;
        let len = (frames as usize).saturating_mul(channels);
        let t = cycle.mOutputTime.mSampleTime;
        let Some(a) = open.filter(|_| t.is_finite() && !main.is_null() && len > 0) else {
            return false;
        };
        // SAFETY: the HAL's WriteMix buffer holds `frames` frames of
        // `channels` floats (the caller's guarantee).
        let buf = unsafe { slice::from_raw_parts(main.cast::<f32>().cast_const(), len) };
        let m0 = (t.floor() as i64).wrapping_add(off) as u64;
        for ch in 0..channels {
            let Some(ring) = a.view.tx(ch) else {
                continue;
            };
            let mut m = m0;
            for &x in buf.iter().skip(ch).step_by(channels).take(n) {
                ring.write_one(m, from_f32(x));
                m = m.wrapping_add(1);
            }
        }
        true
    }

    /// The margins of design section 8.3, against the device time at
    /// `now_ns`, after the operation:
    ///
    /// * output: `(T_out - FPP_MAX + 1 + guard) - T(now)`, the frames left
    ///   before the daemon sends the first frame written;
    /// * input: `(T(now) - L + read_delay) - (T_in + frames)`, the frames the
    ///   last frame read had been in the ring.
    ///
    /// A negative margin counts a late output or early input cycle.
    fn margins(&self, input: bool, n: usize, cycle: &IOCycleInfo, now_ns: u64) {
        let Some(model) = self.model.load() else {
            return;
        };
        let (t_now, _) = model.device_time_at(now_ns);
        let c = &self.counters;
        let cfg = &self.config;
        if input {
            let t_in = cycle.mInputTime.mSampleTime.floor() as i64;
            let latency = i64::from(cfg.latency_samples.load(Ordering::Relaxed))
                .saturating_sub(i64::from(cfg.read_delay.load(Ordering::Relaxed)));
            let margin =
                t_now.saturating_sub(latency).saturating_sub(t_in.saturating_add(n as i64));
            c.min_input_margin.fetch_min(margin, Ordering::Relaxed);
            c.max_input_margin.fetch_max(margin, Ordering::Relaxed);
            if margin < 0 {
                c.early_input_cycles.fetch_add(1, Ordering::Relaxed);
            } else if margin >= FAR_MARGIN {
                c.far_input_cycles.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            let t_out = cycle.mOutputTime.mSampleTime.floor() as i64;
            let lead = i64::from(cfg.fpp_max.load(Ordering::Relaxed))
                .saturating_sub(1)
                .saturating_sub(i64::from(cfg.guard.load(Ordering::Relaxed)));
            let margin = t_out.saturating_sub(lead).saturating_sub(t_now);
            c.min_output_margin.fetch_min(margin, Ordering::Relaxed);
            c.max_output_margin.fetch_max(margin, Ordering::Relaxed);
            if margin < 0 {
                c.late_output_cycles.fetch_add(1, Ordering::Relaxed);
            } else if margin >= FAR_MARGIN {
                c.far_output_cycles.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Marks the engine faulted: IO is silent from now on, and the daemon
    /// sees it in the plug-in status. Real-time safe; called on the IO path
    /// of a driver that caught a panic.
    pub fn set_faulted(&self) {
        self.faulted.store(true, Ordering::Release);
        let guard = self.slot.enter();
        if let Some(a) = guard.get() {
            a.view.plugin().faulted.store(1, Ordering::Relaxed);
        }
    }

    /// The engine's counters and state, for the status property. Not
    /// real-time.
    pub fn snapshot(&self) -> IoSnapshot {
        let guard = self.slot.enter();
        let a = guard.get();
        let gate = self.gate.load(Ordering::Acquire);
        let r = Ordering::Relaxed;
        let c = &self.counters;
        let s = &self.status;
        IoSnapshot {
            io_clients: self.io_clients.load(Ordering::Acquire),
            sample_rate: self.config.sample_rate.load(Ordering::Acquire),
            seed: s.seed.load(r),
            zts_calls: self.zts_calls.load(r),
            regime: regime_of(s.regime.load(r)),
            absorbs: s.absorbs.load(r),
            seed_bumps: s.seed_bumps.load(r),
            device_rate_ppm_milli: s.device_rate_ppm_milli.load(r),
            media_offset: self.off.load(Ordering::Acquire),
            gate: a.is_some_and(|a| gate != 0 && a.generation == gate),
            attached_generation: a.map(|a| a.generation),
            read_calls: c.read_calls.load(r),
            write_calls: c.write_calls.load(r),
            input_missing: c.input_missing.load(r),
            silenced_cycles: c.silenced_cycles.load(r),
            late_output_cycles: c.late_output_cycles.load(r),
            early_input_cycles: c.early_input_cycles.load(r),
            min_output_margin: c.min_output_margin.load(r),
            min_input_margin: c.min_input_margin.load(r),
            max_output_margin: c.max_output_margin.load(r),
            max_input_margin: c.max_input_margin.load(r),
            far_output_cycles: c.far_output_cycles.load(r),
            far_input_cycles: c.far_input_cycles.load(r),
            frames_capped: c.frames_capped.load(r),
            clock_read_failures: s.clock_read_failures.load(r),
            tx_underruns: a.map_or(0, |a| a.view.daemon().tx_underruns.load(r)),
            faulted: self.faulted.load(Ordering::Acquire),
        }
    }

    fn now_ns(&self) -> u64 {
        self.platform.timebase().ticks_to_ns(self.platform.now_ticks())
    }
}

impl Drop for IoEngine {
    fn drop(&mut self) {
        drop(self.attach(None));
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects, clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use std::sync::Arc;

    use ovsc_ipc::region::SharedRegion;
    use ovsc_shm::clock::ClockRecord;
    use ovsc_shm::layout::{HOST_ARCH, HeaderInit, REGION_SIZE, RegionRef};
    use ovsc_shm::time::{ClockSnapshot, ClockState};

    use super::*;
    use crate::platform::stub::StubPlatform;

    const PERIOD_NS_48K: u64 = 16384 * 1_000_000_000 / 48_000;

    fn engine(numer: u32, denom: u32) -> (&'static StubPlatform, IoEngine) {
        let p = StubPlatform::new().leak();
        p.set_timebase(numer, denom);
        p.set_now_ticks(1_000);
        let io = IoEngine::new(&DriverConfig::fallback(), p);
        io.reset_timeline(48_000);
        (p, io)
    }

    /// A daemon region of generation `generation` whose clock runs at media =
    /// host time, alive at `now_ns`, with the engine running at 48 kHz.
    fn daemon(generation: u64, now_ns: u64) -> (Arc<SharedRegion>, RegionRef<'static>) {
        let r = SharedRegion::create(REGION_SIZE).unwrap();
        let h = HeaderInit {
            daemon_generation: generation,
            daemon_pid: 1,
            arch: HOST_ARCH,
            timebase: Timebase::NANOS,
            created_host_ns: now_ns,
            daemon_version: HeaderInit::version_bytes("test"),
        };
        let view = unsafe { RegionRef::init(r.as_ptr(), r.len(), &h) }.unwrap();
        let d = view.daemon();
        d.heartbeat_ns.store(now_ns, Ordering::Release);
        d.audio_word
            .store(AudioWord { sample_rate: 48_000, config_gen: 1 }.pack(), Ordering::Release);
        d.flags.store(DAEMON_ENGINE_RUNNING, Ordering::Release);
        view.clock().publish(&ClockRecord {
            snapshot: ClockSnapshot { local_ref_ns: 0, media_ref_ns: 0, rate: 1.0 },
            valid: true,
            state: ClockState::FreeRunning,
            step_gen: 0,
            grandmaster: 0,
            publish_ns: now_ns,
        });
        (r, view)
    }

    /// Attaches `r`. Safe to drop the previous attachment at once: no IO
    /// runs concurrently in these tests.
    fn attach(io: &IoEngine, r: &Arc<SharedRegion>) {
        let a = io.new_attachment(r.handle().map().unwrap()).unwrap();
        drop(io.attach(Some(a)));
    }

    #[test]
    fn detached_zero_timestamps_follow_the_host_clock() {
        for (numer, denom) in [(1, 1), (125, 3)] {
            let (p, io) = engine(numer, denom);
            io.start_io();
            let (s0, h0, seed) = io.zero_timestamp();
            assert_eq!((s0, h0, seed), (0.0, 1_000, 1));
            let mut last = (s0, h0);
            for k in 1..=100u64 {
                p.advance_ns(PERIOD_NS_48K + 1);
                let (s, h, seed) = io.zero_timestamp();
                assert_eq!((s, seed), (k as f64 * 16384.0, 1));
                assert!(h > last.1 && h <= p.now_ticks());
                let dt_ns = p.timebase().ticks_to_ns(h - 1_000);
                assert!(dt_ns.abs_diff(k * PERIOD_NS_48K) <= 100, "k {k} dt {dt_ns}");
                last = (s, h);
            }
            // Once per period: an early call repeats the last time stamp.
            assert_eq!(io.zero_timestamp().0, last.0);
            assert_eq!(io.cached_zero_timestamp(), io.zero_timestamp());
            assert_eq!(io.snapshot().regime, Regime::Synthetic);
        }
    }

    #[test]
    fn triples_round_up_and_strictly_increase() {
        // 1 tick = 1000 ns, so nearby knots can round to the same tick.
        let tb = Timebase { numer: 1000, denom: 1 };
        let mut c = Clocked::new(TimelineParams::DEFAULT);
        let knot = |k: u64, host_ns: u64, seed: u64| Zts { sample_time: k * 16384, host_ns, seed };
        assert_eq!(c.triple(knot(0, 1_500, 1), tb, 10), (0.0, 2, 1));
        // The same knot again: the same triple, whatever the time.
        assert_eq!(c.triple(knot(0, 1_500, 1), tb, 99), (0.0, 2, 1));
        // Rounds to tick 2 as well: moved to tick 3, which is not after now.
        assert_eq!(c.triple(knot(1, 1_900, 1), tb, 3), (16384.0, 3, 1));
        // Would round to tick 3 again, but now is tick 3: not due yet.
        assert_eq!(c.triple(knot(2, 2_100, 1), tb, 3), (16384.0, 3, 1));
        assert_eq!(c.triple(knot(2, 2_100, 1), tb, 4), (32768.0, 4, 1));
        // Never after now.
        assert_eq!(c.triple(knot(3, 9_000, 1), tb, 6), (49152.0, 6, 1));
        // A new seed starts afresh.
        assert_eq!(c.triple(knot(0, 1_000, 2), tb, 7), (0.0, 1, 2));
    }

    #[test]
    fn seed_changes_only_on_a_later_reset() {
        let (_, io) = engine(1, 1);
        assert_eq!(io.zero_timestamp().2, 1);
        io.reset_timeline(96_000);
        assert_eq!(io.zero_timestamp().2, 2);
        assert_eq!(io.snapshot().sample_rate, 96_000);
        assert_eq!(io.snapshot().seed, 2);
    }

    #[test]
    fn will_do_reads_and_writes_in_place() {
        let (_, io) = engine(1, 1);
        assert_eq!(io.will_do(kAudioServerPlugInIOOperationReadInput), (true, true));
        assert_eq!(io.will_do(kAudioServerPlugInIOOperationWriteMix), (true, true));
        assert_eq!(io.will_do(kAudioServerPlugInIOOperationMixOutput), (false, true));
        assert_eq!(io.will_do(0), (false, true));
    }

    #[test]
    fn detached_io_is_silent() {
        let (_, io) = engine(1, 1);
        let mut buf = vec![1.0f32; 8 * 64];
        let cycle = IOCycleInfo::default();
        let op = kAudioServerPlugInIOOperationReadInput;
        let st = unsafe { io.do_io(3, op, 64, &cycle, buf.as_mut_ptr() as *mut c_void) };
        assert_eq!(st, 0);
        assert!(buf.iter().all(|&x| x == 0.0));
        let op = kAudioServerPlugInIOOperationWriteMix;
        assert_eq!(unsafe { io.do_io(4, op, 64, &cycle, buf.as_mut_ptr().cast()) }, 0);
        let op = kAudioServerPlugInIOOperationMixOutput;
        assert_eq!(unsafe { io.do_io(4, op, 64, &cycle, buf.as_mut_ptr().cast()) }, 0);
        let s = io.snapshot();
        assert_eq!((s.read_calls, s.write_calls, s.silenced_cycles), (1, 1, 2));
    }

    #[test]
    fn clients_are_counted() {
        let (_, io) = engine(1, 1);
        // Only the first start and the last stop change whether IO runs.
        assert!(io.start_io());
        assert!(!io.start_io());
        assert!(!io.stop_io());
        assert_eq!(io.snapshot().io_clients, 1);
        assert!(io.stop_io());
        assert!(!io.stop_io());
        assert_eq!(io.snapshot().io_clients, 0);
        assert!(io.start_io());
    }

    #[test]
    fn attachments_are_checked_on_the_engine_clock() {
        // The stub clock is at 10 s, nowhere near the production clock in
        // general, so only the engine's own clock accepts this region.
        let (p, io) = engine(1, 1);
        p.set_now_ticks(10_000_000_000);
        let (r, _v) = daemon(4, p.now_ticks());
        assert_eq!(io.new_attachment(r.handle().map().unwrap()).unwrap().generation, 4);
        p.set_now_ticks(30_000_000_000);
        let e = io.new_attachment(r.handle().map().unwrap()).err();
        assert!(matches!(e, Some(LayoutError::ClockBase { .. })), "{e:?}");
    }

    #[test]
    fn the_gate_opens_for_the_attachment_it_checked() {
        let (p, io) = engine(1, 1);
        p.set_now_ticks(10_000_000_000);
        let now = p.now_ticks();
        let (r1, _v1) = daemon(1, now);
        attach(&io, &r1);
        io.start_io();
        let s = io.snapshot();
        assert_eq!((s.regime, s.gate, s.attached_generation), (Regime::Following, true, Some(1)));
        // A daemon restart: the new region starts with the gate closed,
        // until a zero time stamp checks its clock.
        let (r2, v2) = daemon(2, now);
        attach(&io, &r2);
        assert!(!io.snapshot().gate);
        io.zero_timestamp();
        assert!(io.snapshot().gate);
        assert_eq!(v2.plugin().gate.load(Ordering::Relaxed), 1);
        assert_eq!(v2.plugin().regime.load(Ordering::Relaxed), REGIME_FOLLOWING);
        // Faulted: closed for IO, and reported.
        io.set_faulted();
        assert!(io.snapshot().faulted);
        assert_eq!(v2.plugin().faulted.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn idle_ticks_follow_the_daemon_until_io_runs() {
        let (p, io) = engine(1, 1);
        p.set_now_ticks(10_000_000_000);
        let (r, v) = daemon(1, p.now_ticks());
        attach(&io, &r);
        // Attached, IO stopped: only an idle tick looks at the daemon.
        assert_eq!(io.snapshot().regime, Regime::Synthetic);
        io.idle_tick();
        let s = io.snapshot();
        assert_eq!((s.regime, s.gate, s.zts_calls), (Regime::Following, true, 0));
        assert_eq!(v.plugin().regime.load(Ordering::Relaxed), REGIME_FOLLOWING);
        // The daemon goes quiet: holdover, gate closed.
        p.advance_ns(5_000_000_000);
        io.idle_tick();
        assert_eq!((io.snapshot().regime, io.snapshot().gate), (Regime::Holdover, false));
        // A reset forgets the clock; the next idle tick follows it again.
        v.daemon().heartbeat_ns.store(p.now_ticks(), Ordering::Release);
        io.reset_timeline(48_000);
        assert_eq!(io.snapshot().regime, Regime::Synthetic);
        io.idle_tick();
        assert_eq!(io.snapshot().regime, Regime::Following);
        // While IO runs, idle ticks leave the timeline to the zero time
        // stamps.
        io.start_io();
        io.reset_timeline(48_000);
        io.idle_tick();
        assert_eq!(io.snapshot().regime, Regime::Synthetic);
    }

    #[test]
    fn cycles_a_period_off_are_counted_as_far() {
        let (p, io) = engine(1, 1);
        p.set_now_ticks(10_000_000_000);
        let (r, _v) = daemon(9, p.now_ticks());
        attach(&io, &r);
        io.start_io();
        io.zero_timestamp();
        // Half a second into the session: the device is at about 24,000.
        p.advance_ns(500_000_000);
        let mut buf = vec![0.0f32; 8 * 4];
        let mut run = |op: u32, input: f64, output: f64| {
            let mut cycle = IOCycleInfo::default();
            cycle.mInputTime.mSampleTime = input;
            cycle.mOutputTime.mSampleTime = output;
            assert_eq!(unsafe { io.do_io(3, op, 4, &cycle, buf.as_mut_ptr().cast()) }, 0);
        };
        let (read, write) =
            (kAudioServerPlugInIOOperationReadInput, kAudioServerPlugInIOOperationWriteMix);
        run(read, 23_800.0, 0.0);
        run(write, 0.0, 24_200.0);
        let s = io.snapshot();
        assert_eq!((s.far_input_cycles, s.far_output_cycles), (0, 0));
        // A period behind for input, a period ahead for output.
        run(read, 24_000.0 - 16_384.0, 0.0);
        run(write, 0.0, 24_000.0 + 16_384.0);
        let s = io.snapshot();
        assert_eq!((s.far_input_cycles, s.far_output_cycles), (1, 1));
        assert_eq!((s.early_input_cycles, s.late_output_cycles), (0, 0));
        assert!(s.max_input_margin >= 16_000 && s.min_input_margin < 1_000, "{s:?}");
        assert!(s.max_output_margin >= 16_000 && s.min_output_margin < 1_000, "{s:?}");
        // StartIO resets the margins, not the counts.
        io.stop_io();
        io.start_io();
        let s = io.snapshot();
        assert_eq!((s.min_input_margin, s.max_input_margin), (i64::MAX, i64::MIN));
        assert_eq!(s.far_input_cycles, 1);
    }

    #[test]
    fn io_moves_samples_at_ring_offset() {
        let (p, io) = engine(1, 1);
        p.set_now_ticks(10_000_000_000);
        let now = p.now_ticks();
        let (r, v) = daemon(9, now);
        attach(&io, &r);
        io.start_io();
        io.zero_timestamp();
        // Media = host time: 480,000 frames at 10 s. The device time ran
        // from 1 us and restarts near 0 at StartIO, so 479,999 whole frames
        // went into the offset.
        let off = io.snapshot().media_offset;
        assert_eq!(off, 479_999);

        let mut cycle = IOCycleInfo::default();
        cycle.mInputTime.mSampleTime = 100.0;
        cycle.mOutputTime.mSampleTime = 300.0;
        for c in 0..8 {
            v.rx(c).unwrap().write(off as u64 + 100, &[c as i32 * 256, -(c as i32) * 256 - 256]);
        }
        let mut buf = vec![9.0f32; 8 * 4];
        let op = kAudioServerPlugInIOOperationReadInput;
        assert_eq!(unsafe { io.do_io(3, op, 4, &cycle, buf.as_mut_ptr().cast()) }, 0);
        for c in 0..8 {
            assert_eq!(buf[c], to_f32(c as i32 * 256));
            assert_eq!(buf[8 + c], to_f32(-(c as i32) * 256 - 256));
            assert_eq!((buf[16 + c], buf[24 + c]), (0.0, 0.0));
        }
        assert_eq!(io.snapshot().input_missing, 16);

        let mix: Vec<f32> = (0..8 * 4).map(|i| i as f32 / 64.0).collect();
        let op = kAudioServerPlugInIOOperationWriteMix;
        assert_eq!(unsafe { io.do_io(4, op, 4, &cycle, mix.as_ptr().cast_mut().cast()) }, 0);
        for c in 0..8 {
            for i in 0..4 {
                let got = v.tx(c).unwrap().read_one(off as u64 + 300 + i as u64);
                assert_eq!(got, Some(from_f32(mix[i * 8 + c])));
            }
        }
        // Non-finite times: silence, nothing written.
        cycle.mInputTime.mSampleTime = f64::NAN;
        let op = kAudioServerPlugInIOOperationReadInput;
        assert_eq!(unsafe { io.do_io(3, op, 4, &cycle, buf.as_mut_ptr().cast()) }, 0);
        assert!(buf.iter().all(|&x| x == 0.0));
        let s = io.snapshot();
        assert_eq!((s.read_calls, s.write_calls, s.silenced_cycles), (2, 1, 1));
        assert_eq!(v.plugin().read_calls.load(Ordering::Relaxed), 2);
        // The trace has the three operations of this session.
        let (header, entries) = v.io_trace();
        assert_eq!(header.session.load(Ordering::Relaxed), 1);
        assert_eq!(header.next.load(Ordering::Relaxed), 3);
        let op_stream = entries[1].op_stream.load(Ordering::Relaxed);
        assert_eq!(op_stream, u64::from(kAudioServerPlugInIOOperationWriteMix) | 4 << 32);
    }

    #[test]
    fn every_region_gets_a_trace_session() {
        let (p, io) = engine(1, 1);
        p.set_now_ticks(10_000_000_000);
        let now = p.now_ticks();
        let cycle = IOCycleInfo::default();
        let mut buf = vec![0.0f32; 8 * 4];
        let op = kAudioServerPlugInIOOperationReadInput;
        // IO starts before the first welcome: the session starts in the
        // first region IO runs on, from entry 0.
        io.start_io();
        assert_eq!(unsafe { io.do_io(3, op, 4, &cycle, buf.as_mut_ptr().cast()) }, 0);
        let (r1, v1) = daemon(1, now);
        attach(&io, &r1);
        for _ in 0..2 {
            assert_eq!(unsafe { io.do_io(3, op, 4, &cycle, buf.as_mut_ptr().cast()) }, 0);
        }
        let (h1, _) = v1.io_trace();
        assert_eq!((h1.session.load(Ordering::Relaxed), h1.next.load(Ordering::Relaxed)), (1, 2));
        // The daemon restarts mid-session: its region gets a session of its
        // own, again from entry 0, and the old one is left alone.
        let (r2, v2) = daemon(2, now);
        attach(&io, &r2);
        assert_eq!(unsafe { io.do_io(3, op, 4, &cycle, buf.as_mut_ptr().cast()) }, 0);
        let (h2, e2) = v2.io_trace();
        assert_eq!((h2.session.load(Ordering::Relaxed), h2.next.load(Ordering::Relaxed)), (1, 1));
        assert_eq!(e2[0].op_stream.load(Ordering::Relaxed), u64::from(op) | 3 << 32);
        assert_eq!((h1.session.load(Ordering::Relaxed), h1.next.load(Ordering::Relaxed)), (1, 2));
        // The next IO session starts in the current region.
        io.stop_io();
        io.start_io();
        assert_eq!(unsafe { io.do_io(3, op, 4, &cycle, buf.as_mut_ptr().cast()) }, 0);
        assert_eq!((h2.session.load(Ordering::Relaxed), h2.next.load(Ordering::Relaxed)), (2, 1));
    }

    #[test]
    fn oversized_operations_are_capped() {
        let (p, io) = engine(1, 1);
        p.set_now_ticks(10_000_000_000);
        let now = p.now_ticks();
        let (r, _v) = daemon(9, now);
        attach(&io, &r);
        io.start_io();
        let frames = IO_FRAMES_CAP as u32 + 16;
        let mut buf = vec![1.0f32; 8 * frames as usize];
        let cycle = IOCycleInfo::default();
        let op = kAudioServerPlugInIOOperationReadInput;
        assert_eq!(unsafe { io.do_io(3, op, frames, &cycle, buf.as_mut_ptr().cast()) }, 0);
        assert!(buf.iter().all(|&x| x == 0.0));
        let s = io.snapshot();
        assert_eq!((s.frames_capped, s.silenced_cycles), (1, 0));
        assert_eq!(s.input_missing, 8 * IO_FRAMES_CAP as u64);
    }
}
