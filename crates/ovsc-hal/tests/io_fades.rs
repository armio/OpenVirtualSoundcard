//! What reaches the host and the console at the gate's edges, and from a
//! host whose output is not finite.
//!
//! A simulated HAL runs 32-frame IO cycles against a daemon region whose
//! clock runs at host time. RX carries a loud 50 Hz tone that never comes
//! near zero (0.1 to 0.9), and so does the host's output, so a hard cut to
//! or from silence anywhere is a step of at least 0.1; the tone itself
//! moves at most 0.0027 per frame. The test collects what ReadInput gives
//! the host and what the daemon's TX thread would send, frame by frame,
//! across the gate closing and opening again.

use std::f64::consts::TAU;
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use ovsc_hal::abi::*;
use ovsc_hal::io::{IoEngine, edge_ramp_frames};
use ovsc_hal::model::DriverConfig;
use ovsc_hal::platform::Platform;
use ovsc_hal::platform::stub::StubPlatform;
use ovsc_ipc::region::SharedRegion;
use ovsc_shm::clock::ClockRecord;
use ovsc_shm::layout::{HOST_ARCH, HeaderInit, REGION_SIZE, RegionRef};
use ovsc_shm::sample::{from_f32, to_f32};
use ovsc_shm::status::{AudioWord, ChannelsWord, DAEMON_ENGINE_RUNNING};
use ovsc_shm::time::{ClockSnapshot, ClockState, Timebase, ns_to_samples};

const FS: u32 = 48_000;
const N: i64 = 32;
const CHANNELS: usize = 8;
const SEC: u64 = 1_000_000_000;
const START_NS: u64 = 10 * SEC;
/// The daemon's TX guard, frames.
const GUARD: u64 = 24;
/// The fallback configuration's safety offsets.
const S_IN: i64 = 216;
const S_OUT: i64 = 55;
/// IO cycles per second.
const CYCLES_PER_SEC: usize = (FS as i64 / N) as usize;
/// The largest frame-to-frame change allowed: the tone's own (0.0027) plus
/// a ramp's step on a full-level tone (0.9 / 240).
const MAX_STEP: f32 = 0.01;

/// The tone at frame `x` (media index or device time).
fn tone(x: i64) -> f32 {
    (0.5 + 0.4 * (TAU * x.rem_euclid(960) as f64 / 960.0).sin()) as f32
}

/// The daemon's clock: media time runs at host time.
fn media_clock() -> ClockSnapshot {
    ClockSnapshot { local_ref_ns: START_NS, media_ref_ns: 1_700_000_000 * SEC, rate: 1.0 }
}

/// A daemon region of `generation`, alive and running at 48 kHz.
fn region(generation: u64) -> (Arc<SharedRegion>, RegionRef<'static>) {
    let r = SharedRegion::create(REGION_SIZE).unwrap();
    let h = HeaderInit {
        daemon_generation: generation,
        daemon_pid: 1,
        arch: HOST_ARCH,
        timebase: Timebase::NANOS,
        created_host_ns: START_NS,
        daemon_version: HeaderInit::version_bytes("io_fades"),
    };
    // SAFETY: a fresh region, laid out before anyone else sees it; the Arc
    // outlives every use of the view.
    let view = unsafe { RegionRef::init(r.as_ptr(), r.len(), &h) }.unwrap();
    let d = view.daemon();
    d.heartbeat_ns.store(START_NS, Ordering::Release);
    d.audio_word.store(AudioWord { sample_rate: FS, config_gen: 1 }.pack(), Ordering::Release);
    let channels = ChannelsWord { rx: 8, tx: 8, latency_samples: 192 };
    d.channels_word.store(channels.pack(), Ordering::Release);
    d.tx_guard_samples.store(GUARD, Ordering::Release);
    d.flags.store(DAEMON_ENGINE_RUNNING, Ordering::Release);
    view.clock().publish(&ClockRecord {
        snapshot: media_clock(),
        valid: true,
        state: ClockState::FreeRunning,
        step_gen: 0,
        grandmaster: 0,
        publish_ns: START_NS,
    });
    (r, view)
}

/// The engine, a daemon and a HAL that runs IO cycles on them.
struct Rig {
    platform: &'static StubPlatform,
    io: IoEngine,
    regions: Vec<Arc<SharedRegion>>,
    view: RegionRef<'static>,
    /// The latest zero time stamp: device sample time, host ns.
    zts: (f64, f64),
    /// Device time of the next cycle.
    t: i64,
    next_rx: Option<u64>,
    next_sent: Option<u64>,
    /// Channel 0 as ReadInput gave it, frame by frame.
    input: Vec<f32>,
    /// Channel 0 as the TX thread sent it, by media index.
    sent: Vec<f32>,
    /// Whether the gate was open, per cycle.
    gates: Vec<bool>,
}

impl Rig {
    fn new() -> Self {
        let platform = StubPlatform::new().leak();
        platform.set_timebase(1, 1);
        platform.set_now_ticks(START_NS);
        let cfg = DriverConfig::fallback();
        assert_eq!((cfg.input_safety_offset, cfg.output_safety_offset), (216, 55));
        let io = IoEngine::new(&cfg, platform);
        io.apply_config(&cfg);
        io.reset_timeline(FS);
        let (region, view) = region(1);
        let a = io.new_attachment(region.handle().map().unwrap()).unwrap();
        assert!(io.attach(Some(a)).is_none());
        io.start_io();
        let (s0, h0, _) = io.cached_zero_timestamp();
        Rig {
            platform,
            io,
            regions: vec![region],
            view,
            zts: (s0, h0 as f64),
            t: N,
            next_rx: None,
            next_sent: None,
            input: Vec::new(),
            sent: Vec::new(),
            gates: Vec::new(),
        }
    }

    fn media_now(&self) -> u64 {
        ns_to_samples(media_clock().media_ns_at(self.platform.now_ticks()), FS)
    }

    /// Switches to a new daemon region, as after a daemon restart.
    fn restart_daemon(&mut self, generation: u64) {
        let (region, view) = region(generation);
        let a = self.io.new_attachment(region.handle().map().unwrap()).unwrap();
        // No IO runs concurrently: the old attachment can go at once.
        drop(self.io.attach(Some(a)));
        self.regions.push(region);
        self.view = view;
        self.next_rx = None;
        self.next_sent = None;
    }

    /// One IO cycle: the daemon's RX up to now, GetZeroTimeStamp,
    /// ReadInput, WriteMix of `mix(device frame, channel)`, then the TX
    /// thread's sends up to now. Returns the ReadInput buffer.
    fn cycle(&mut self, mix: impl Fn(i64, usize) -> f32) -> Vec<f32> {
        let host = self.zts.1 + (self.t as f64 - self.zts.0) * 1e9 / f64::from(FS);
        let now = (host.ceil() as u64).max(self.platform.now_ticks());
        self.platform.set_now_ticks(now);
        self.view.daemon().heartbeat_ns.store(now, Ordering::Release);
        let media = self.media_now();
        let from = *self.next_rx.get_or_insert(media - 20_000);
        for m in from..=media {
            for c in 0..CHANNELS {
                self.view.rx(c).unwrap().write_one(m, from_f32(tone(m as i64)));
            }
        }
        self.next_rx = Some(media + 1);

        let (s, h, _) = self.io.zero_timestamp();
        self.zts = (s, h as f64);
        self.gates.push(self.io.snapshot().gate);
        let mut cycle = IOCycleInfo { mNominalIOBufferFrameSize: N as u32, ..Default::default() };
        let (t_in, t_out) = (self.t - N - S_IN, self.t + N + S_OUT);
        cycle.mInputTime.mSampleTime = t_in as f64;
        cycle.mOutputTime.mSampleTime = t_out as f64;

        let mut input = vec![f32::NAN; N as usize * CHANNELS];
        let read = kAudioServerPlugInIOOperationReadInput;
        assert_eq!(
            unsafe { self.io.do_io(3, read, N as u32, &cycle, input.as_mut_ptr().cast()) },
            0
        );
        self.input.extend(input.iter().step_by(CHANNELS));
        let mut output: Vec<f32> = (0..N as usize * CHANNELS)
            .map(|k| mix(t_out + (k / CHANNELS) as i64, k % CHANNELS))
            .collect();
        let write = kAudioServerPlugInIOOperationWriteMix;
        let main = output.as_mut_ptr().cast::<c_void>();
        assert_eq!(unsafe { self.io.do_io(4, write, N as u32, &cycle, main) }, 0);

        let due = media - GUARD;
        let from = *self.next_sent.get_or_insert(due);
        for p in from..=due {
            self.sent.push(to_f32(self.view.tx(0).unwrap().read_one(p).unwrap_or(0)));
        }
        self.next_sent = Some(due + 1);
        self.t += N;
        input
    }

    fn run(&mut self, cycles: usize) {
        for _ in 0..cycles {
            self.cycle(|t, _| tone(t));
        }
    }

    fn set_engine_running(&self, running: bool) {
        let flags = if running { DAEMON_ENGINE_RUNNING } else { 0 };
        self.view.daemon().flags.store(flags, Ordering::Release);
    }
}

/// The largest frame-to-frame change in `x`, and where.
fn max_step(x: &[f32]) -> (f32, usize) {
    x.windows(2)
        .enumerate()
        .map(|(i, w)| ((w[1] - w[0]).abs(), i))
        .fold((0.0, 0), |a, b| if b.0 > a.0 { b } else { a })
}

/// The longest run of exact silence in `x`.
fn longest_silence(x: &[f32]) -> usize {
    let (mut best, mut run) = (0, 0);
    for &v in x {
        run = if v == 0.0 { run + 1 } else { 0 };
        best = best.max(run);
    }
    best
}

#[test]
fn the_gate_fades_audio_in_and_out_both_ways() {
    let mut rig = Rig::new();
    // StartIO, then 1 s of audio; the daemon's engine stops for half a
    // second (as it does to apply a new latency) and starts again.
    rig.run(CYCLES_PER_SEC);
    rig.set_engine_running(false);
    rig.run(CYCLES_PER_SEC / 2);
    rig.set_engine_running(true);
    rig.run(CYCLES_PER_SEC);

    let opened = rig.gates.iter().filter(|&&g| g).count();
    let closed = &rig.gates[CYCLES_PER_SEC + 1..CYCLES_PER_SEC * 3 / 2];
    assert!(rig.gates[0] && closed.iter().all(|&g| !g), "{:?}", &rig.gates[..4]);
    assert!(opened > CYCLES_PER_SEC * 3 / 2 && *rig.gates.last().unwrap());

    let len = edge_ramp_frames(FS) as usize;
    for (what, x) in [("input", &rig.input), ("sent", &rig.sent)] {
        // No step anywhere, at StartIO or either edge ...
        let (step, at) = max_step(x);
        assert!(step < MAX_STEP, "{what}: a step of {step} at frame {at}");
        // ... though it went silent in between, and came back.
        let silence = longest_silence(x);
        assert!(
            (FS as usize * 4 / 10..FS as usize * 7 / 10).contains(&silence),
            "{what}: {silence}"
        );
        assert!(x[x.len() - 1000..].iter().all(|&v| v >= 0.1), "{what} at the end");
        // Once faded in, it is the tone at full level, bit-exactly (the
        // ring and the device time are a fixed offset apart).
        let late = &x[x.len() - 2000..];
        let exact = (0..960).any(|s| {
            late.iter().enumerate().all(|(i, &v)| to_f32(from_f32(tone(s + i as i64))) == v)
        });
        assert!(exact, "{what} is not the tone at unity gain");
        // The fades take the ramp's length: about 5 ms below the tone's
        // floor at each of the three edges.
        let quiet = x.iter().filter(|&&v| v > 0.0 && v < 0.1).count();
        assert!(quiet >= 3 * len / 9 && quiet <= 3 * len, "{what}: {quiet} quiet frames");
    }
    // The input started with the first frame of a fade-in.
    let first = rig.input.iter().position(|&v| v != 0.0).unwrap();
    assert!(rig.input[first] <= 0.9 / len as f32, "{}", rig.input[first]);
}

#[test]
fn a_fault_or_a_new_daemon_cuts_without_a_tail() {
    let mut rig = Rig::new();
    rig.run(CYCLES_PER_SEC / 4);
    // A new daemon: what it carries is another stream, which fades in from
    // silence, with no tail of the old one.
    rig.restart_daemon(2);
    let before = rig.input.len();
    let input = rig.cycle(|t, _| tone(t));
    assert!(rig.gates.last().copied().unwrap(), "the gate opens on the new region at once");
    assert!(input[0] > 0.0 && input[0] <= 0.9 / edge_ramp_frames(FS) as f32, "{}", input[0]);
    rig.run(CYCLES_PER_SEC / 4);
    assert!(rig.input[before..].iter().rev().take(1000).all(|&v| v >= 0.1));
    // A fault (a panic caught on the IO path): silent at once, both ways,
    // with nothing written after it.
    rig.io.set_faulted();
    let written: Vec<u64> = (0..2000u64).map(|i| rig.media_now() + i).collect();
    let tx_before: Vec<_> = written.iter().map(|&m| rig.view.tx(0).unwrap().read_one(m)).collect();
    let input = rig.cycle(|t, _| tone(t));
    assert!(input.iter().all(|&v| v == 0.0));
    rig.run(10);
    let tx_after: Vec<_> = written.iter().map(|&m| rig.view.tx(0).unwrap().read_one(m)).collect();
    assert_eq!(tx_before, tx_after, "a faulted engine wrote output");
    assert!(rig.io.snapshot().faulted);
}

#[test]
fn non_finite_output_is_sent_as_silence() {
    let mut rig = Rig::new();
    // Past the fade-in, so what is written is what the host gave.
    rig.run(CYCLES_PER_SEC / 10);
    let specials = [f32::INFINITY, f32::NEG_INFINITY, f32::NAN, -f32::NAN, 1.5, -1.5, 0.25];
    let t_out = rig.t + N + S_OUT;
    rig.cycle(|t, ch| match specials.get((t - t_out) as usize) {
        Some(&x) if ch == 0 => x,
        _ => tone(t),
    });
    let off = rig.io.snapshot().media_offset;
    let got: Vec<_> = (0..specials.len())
        .map(|i| rig.view.tx(0).unwrap().read_one((t_out + off) as u64 + i as u64))
        .collect();
    let expected = [0, 0, 0, 0, i32::MAX, i32::MIN, 1 << 29];
    assert_eq!(got, expected.map(Some));
    // Counted, for the status, as is what fades out of a closed gate.
    assert_eq!(rig.io.snapshot().non_finite_out, 4);
    assert_eq!(rig.view.plugin().non_finite_out.load(Ordering::Relaxed), 4);
    rig.set_engine_running(false);
    rig.cycle(|_, _| f32::NAN);
    assert_eq!(rig.io.snapshot().non_finite_out, 4 + 8 * N as u64);
    // Not what the gate drops.
    rig.run(CYCLES_PER_SEC / 10);
    rig.cycle(|_, _| f32::INFINITY);
    assert_eq!(rig.io.snapshot().non_finite_out, 4 + 8 * N as u64);
    // The other channels are untouched.
    let other = rig.view.tx(1).unwrap().read_one((t_out + off) as u64);
    assert_eq!(other, Some(from_f32(tone(t_out))));
}
