//! The IO engine between a fake daemon and a simulated HAL (design sections
//! 7.4, 8.2, 8.3 and 17.1).
//!
//! The daemon runs on its own thread, in lockstep with the simulated host
//! clock: it publishes its clock in the region (first nothing, then a
//! Locking clock at a different phase, then a free-running clock at
//! +50 ppm), a 10 Hz heartbeat and ENGINE_RUNNING at 48 kHz, and writes RX
//! rings at media timestamps. RX channels 1-4 carry a pattern of the media
//! index; channels 5-8 carry what TX channels 5-8 sent, as a subscription to
//! the device itself would, copied once the TX thread would have sent it.
//!
//! The HAL wakes every N frames of the device time it extrapolates from the
//! zero time stamps (the Raw clock algorithm uses them as they are), and
//! calls GetZeroTimeStamp, ReadInput at T - N - S_in and WriteMix at
//! T + N + S_out. Over 30 simulated seconds, for N of 32, 512 and 4096:
//!
//! * input is the RX pattern bit-exactly, and the looped channels return
//!   the output at device-time delay 0;
//! * the TX rings hold the written floats at ring index t + off, before
//!   the daemon sends them;
//! * the margins never go negative;
//! * zero time stamps are consecutive, strictly increasing and never in the
//!   future, with one seed throughout;
//! * the gate zero-fills input and drops output before the clock is usable,
//!   while the heartbeat is stale and while the daemon runs at another rate.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;

use ovsc_hal::abi::*;
use ovsc_hal::io::IoEngine;
use ovsc_hal::model::DriverConfig;
use ovsc_hal::platform::Timebase;
use ovsc_hal::platform::stub::StubPlatform;
use ovsc_ipc::region::SharedRegion;
use ovsc_shm::clock::ClockRecord;
use ovsc_shm::layout::{HOST_ARCH, HeaderInit, REGION_SIZE, RegionRef};
use ovsc_shm::sample::{from_f32, to_f32};
use ovsc_shm::status::{AudioWord, ChannelsWord, DAEMON_ENGINE_RUNNING};
use ovsc_shm::time::{ClockSnapshot, ClockState, ns_to_samples};
use ovsc_shm::timeline::Regime;

const FS: u32 = 48_000;
const CHANNELS: usize = 8;
/// RX channels 0..LOOP carry the RX pattern; LOOP..CHANNELS loop TX back.
const LOOP: usize = 4;
const PERIOD: f64 = 16384.0;
/// Apple silicon's timebase.
const TB: Timebase = Timebase { numer: 125, denom: 3 };
const SEC: u64 = 1_000_000_000;
/// Host time when the run starts.
const START_NS: u64 = 5 * SEC;
/// The fake network's delay before an RX sample lands, frames.
const NET_DELAY: u64 = 48;
/// The daemon's receive latency L (4 ms) and TX guard (500 us).
const LATENCY: u32 = 192;
const GUARD: u64 = 24;
const RUN_NS: u64 = 30 * SEC;
/// The scenario, as offsets from the start.
const LOCKING_AT: u64 = SEC;
const LOCKED_AT: u64 = 2 * SEC;
const HEARTBEAT_STOP: u64 = 20 * SEC;
const HEARTBEAT_RESUME: u64 = 21_500_000_000;
const WRONG_RATE_FROM: u64 = 25 * SEC;
const WRONG_RATE_UNTIL: u64 = 25_500_000_000;

/// A 24-bit code, left-justified, from a hash of (channel, index).
fn code(seed: u64, c: usize, i: u64) -> i32 {
    let x = i.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (c as u64 + seed).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    (((x >> 40) as u32) << 8) as i32
}

/// What RX channel `c` carries at media index `m`.
fn rx_pattern(c: usize, m: u64) -> i32 {
    code(1, c, m)
}

/// What the HAL plays on channel `c` at device frame `t`.
fn out_pattern(c: usize, t: i64) -> f32 {
    to_f32(code(2, c, t as u64))
}

/// The daemon's free-running clock: +50 ppm against the host, from a
/// PTP-like epoch.
fn free_clock(local_ref_ns: u64) -> ClockSnapshot {
    ClockSnapshot { local_ref_ns, media_ref_ns: 1_700_000_000 * SEC, rate: 1.000_05 }
}

/// The fake daemon, run by its own thread at the host times the HAL loop
/// sends it.
struct Daemon {
    view: RegionRef<'static>,
    last_heartbeat: u64,
    clock_phase: u8,
    /// Next media index to write on the RX pattern channels.
    next_rx: Option<u64>,
    /// Next media index to copy from TX to the looped RX channels.
    next_loop: Option<u64>,
}

impl Daemon {
    fn new(view: RegionRef<'static>) -> Self {
        let d = view.daemon();
        let channels = ChannelsWord { rx: 8, tx: 8, latency_samples: LATENCY };
        d.channels_word.store(channels.pack(), Ordering::Release);
        d.tx_guard_samples.store(GUARD, Ordering::Release);
        d.flags.store(DAEMON_ENGINE_RUNNING, Ordering::Release);
        Daemon { view, last_heartbeat: 0, clock_phase: 0, next_rx: None, next_loop: None }
    }

    fn step(&mut self, now: u64) {
        let since = now - START_NS;
        let d = self.view.daemon();
        // The engine's rate, briefly wrong.
        let rate = if (WRONG_RATE_FROM..WRONG_RATE_UNTIL).contains(&since) { 96_000 } else { FS };
        d.audio_word
            .store(AudioWord { sample_rate: rate, config_gen: 1 }.pack(), Ordering::Release);
        // The heartbeat, at 10 Hz, with a pause.
        let paused = (HEARTBEAT_STOP..HEARTBEAT_RESUME).contains(&since);
        if !paused && now - self.last_heartbeat >= SEC / 10 {
            d.heartbeat_ns.store(now, Ordering::Release);
            self.last_heartbeat = now;
        }
        // The clock: nothing, then Locking at another phase, then free.
        let phase = match since {
            s if s < LOCKING_AT => 0,
            s if s < LOCKED_AT => 1,
            _ => 2,
        };
        if phase != self.clock_phase {
            self.clock_phase = phase;
            let mut snapshot = free_clock(START_NS);
            let state = if phase == 1 {
                snapshot.media_ref_ns += 10_000_000;
                ClockState::Locking
            } else {
                ClockState::FreeRunning
            };
            let record = ClockRecord {
                snapshot,
                valid: true,
                state,
                step_gen: u64::from(phase),
                grandmaster: 0,
                publish_ns: now,
            };
            self.view.clock().publish(&record);
        }
        if phase < 2 {
            return;
        }
        // RX, as the network delivers it.
        let media = ns_to_samples(free_clock(START_NS).media_ns_at(now), FS);
        let last = media - NET_DELAY;
        let from = *self.next_rx.get_or_insert(last - 20_000);
        for m in from..=last {
            for c in 0..LOOP {
                self.view.rx(c).unwrap().write_one(m, rx_pattern(c, m));
            }
        }
        self.next_rx = Some(last + 1);
        // TX sends frame p once media time reaches p + guard; a missing
        // sample goes out as silence. The looped channels receive it.
        let sent = media - GUARD;
        let from = *self.next_loop.get_or_insert(sent - 20_000);
        for p in from..=sent {
            for c in LOOP..CHANNELS {
                let s = self.view.tx(c).unwrap().read_one(p).unwrap_or(0);
                self.view.rx(c).unwrap().write_one(p, s);
            }
        }
        self.next_loop = Some(sent + 1);
    }
}

/// The HAL's view of the device clock: the two latest zero time stamps,
/// used as they are (the Raw clock algorithm).
struct HalClock {
    prev: Option<(f64, f64)>,
    last: (f64, f64),
    seed: u64,
}

impl HalClock {
    /// Device frames per ns.
    fn rate(&self) -> f64 {
        match self.prev {
            Some((s0, h0)) => (self.last.0 - s0) / (self.last.1 - h0),
            None => f64::from(FS) / 1e9,
        }
    }

    /// The host time, ns, at which the device reaches `sample`.
    fn host_ns_at(&self, sample: f64) -> f64 {
        self.last.1 + (sample - self.last.0) / self.rate()
    }

    /// Takes a zero time stamp from GetZeroTimeStamp, checking that it is
    /// the same as the last one or the next, and never later than now.
    fn take(&mut self, (sample, ticks, seed): (f64, u64, u64), now_ticks: u64) {
        assert!(ticks <= now_ticks, "time stamp {ticks} after now {now_ticks}");
        assert_eq!(seed, self.seed, "the seed changed");
        let host = TB.ticks_to_ns(ticks) as f64;
        if sample == self.last.0 {
            assert_eq!(host, self.last.1, "a reported time stamp changed");
            return;
        }
        assert_eq!(sample, self.last.0 + PERIOD, "time stamps not consecutive");
        assert!(host > self.last.1, "host time went back");
        self.prev = Some(self.last);
        self.last = (sample, host);
    }
}

/// Whether the gate must be open (Some(true)) or closed (Some(false)) at
/// `since` ns after the start, or None near a transition.
fn expected_gate(since: u64) -> Option<bool> {
    let ms = since / 1_000_000;
    match ms {
        0..2000 => Some(false),
        2000..2100 => None,
        2100..20000 => Some(true),
        // The last heartbeat came at 20.0 s at the latest, so it is stale
        // from 21.0 s.
        20000..21050 => None,
        21050..21500 => Some(false),
        21500..21600 => None,
        21600..25000 => Some(true),
        25000..25500 => Some(false),
        25500..25600 => None,
        _ => Some(true),
    }
}

fn region() -> (Arc<SharedRegion>, RegionRef<'static>) {
    let r = SharedRegion::create(REGION_SIZE).unwrap();
    let h = HeaderInit {
        daemon_generation: 0x1f3a_5eed,
        daemon_pid: 4242,
        arch: HOST_ARCH,
        timebase: TB,
        created_host_ns: START_NS,
        daemon_version: HeaderInit::version_bytes("io_harness"),
    };
    // SAFETY: a fresh region, laid out before anyone else sees it; the
    // Arc outlives every use of the view in a run.
    let view = unsafe { RegionRef::init(r.as_ptr(), r.len(), &h) }.unwrap();
    view.daemon().heartbeat_ns.store(START_NS, Ordering::Release);
    (r, view)
}

fn run(n: u32) {
    let platform = StubPlatform::new().leak();
    platform.set_timebase(TB.numer, TB.denom);
    platform.set_now_ticks(TB.ns_to_ticks_ceil(START_NS));
    let cfg = DriverConfig::fallback();
    let (s_in, s_out) = (cfg.input_safety_offset as i64, cfg.output_safety_offset as i64);
    assert_eq!((s_in, s_out, cfg.output_latency), (216, 55, LATENCY));

    // Initialize, then the link attaches the daemon's region.
    let io = IoEngine::new(&cfg, platform);
    io.apply_config(&cfg);
    io.reset_timeline(cfg.sample_rate);
    let (region, view) = region();
    let a = io.new_attachment(region.handle().map().unwrap()).unwrap();
    assert!(io.attach(Some(a)).is_none());

    let (to_daemon, daemon_rx) = mpsc::channel::<u64>();
    let (daemon_tx, from_daemon) = mpsc::channel::<()>();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let mut daemon = Daemon::new(view);
            while let Ok(now) = daemon_rx.recv() {
                daemon.step(now);
                daemon_tx.send(()).unwrap();
            }
        });
        let advance = |ns: u64| {
            let ticks = TB.ns_to_ticks_ceil(ns).max(platform_now(platform));
            platform.set_now_ticks(ticks);
            to_daemon.send(TB.ticks_to_ns(ticks)).unwrap();
            from_daemon.recv().unwrap();
            ticks
        };
        advance(START_NS);

        io.start_io();
        let first = io.cached_zero_timestamp();
        assert_eq!((first.0, first.2), (0.0, 1));
        let mut hal = HalClock { prev: None, last: (0.0, TB.ticks_to_ns(first.1) as f64), seed: 1 };

        let n64 = i64::from(n);
        let nf = n as usize;
        let mut input = vec![0f32; nf * CHANNELS];
        let mut output = vec![0f32; nf * CHANNELS];
        // Device frames written while the gate was open: [from, to).
        let mut written: Vec<(i64, i64)> = Vec::new();
        let (mut checked_rx, mut checked_loop, mut opened) = (0u64, 0u64, 0u64);
        let mut cycle = IOCycleInfo { mNominalIOBufferFrameSize: n, ..Default::default() };
        let mut c = 1i64;
        loop {
            let t = c * n64;
            let now_ticks = advance(hal.host_ns_at(t as f64).ceil() as u64);
            let since = TB.ticks_to_ns(now_ticks) - START_NS;
            if since >= RUN_NS {
                break;
            }
            hal.take(io.zero_timestamp(), now_ticks);
            let snap = io.snapshot();
            if let Some(open) = expected_gate(since) {
                assert_eq!(snap.gate, open, "gate at {since} ns: {snap:?}");
            }
            let off = snap.media_offset;
            cycle.mIOCycleCounter = c as u64;
            cycle.mCurrentTime.mSampleTime = t as f64;
            cycle.mCurrentTime.mHostTime = now_ticks;
            let t_in = t - n64 - s_in;
            let t_out = t + n64 + s_out;
            cycle.mInputTime.mSampleTime = t_in as f64;
            cycle.mOutputTime.mSampleTime = t_out as f64;

            // ReadInput.
            input.fill(f32::NAN);
            let read = kAudioServerPlugInIOOperationReadInput;
            let main = input.as_mut_ptr().cast::<c_void>();
            assert_eq!(unsafe { io.do_io(3, read, n, &cycle, main) }, 0);
            for (i, frame) in input.chunks_exact(CHANNELS).enumerate() {
                let td = t_in + i as i64;
                let m = (td + off) as u64;
                for (ch, &x) in frame.iter().enumerate() {
                    let expected = if !snap.gate {
                        0.0
                    } else if ch < LOOP {
                        checked_rx += 1;
                        to_f32(rx_pattern(ch, m))
                    } else if written.iter().any(|&(a, b)| (a..b).contains(&td)) {
                        checked_loop += 1;
                        out_pattern(ch, td)
                    } else {
                        0.0
                    };
                    assert_eq!(x.to_bits(), expected.to_bits(), "input {ch} at {td} ({since} ns)");
                }
            }

            // WriteMix.
            for (i, frame) in output.chunks_exact_mut(CHANNELS).enumerate() {
                for (ch, x) in frame.iter_mut().enumerate() {
                    *x = out_pattern(ch, t_out + i as i64);
                }
            }
            let mix = kAudioServerPlugInIOOperationWriteMix;
            let main = output.as_mut_ptr().cast::<c_void>();
            assert_eq!(unsafe { io.do_io(4, mix, n, &cycle, main) }, 0);
            for (i, frame) in output.chunks_exact(CHANNELS).enumerate() {
                let m = (t_out + i as i64 + off) as u64;
                for (ch, &x) in frame.iter().enumerate() {
                    let got = view.tx(ch).unwrap().read_one(m);
                    let expected = snap.gate.then(|| from_f32(x));
                    assert_eq!(got, expected, "output {ch} at {} ({since} ns)", t_out + i as i64);
                }
            }
            if snap.gate {
                opened += 1;
                match written.last_mut() {
                    Some(last) if last.1 == t_out => last.1 = t_out + n64,
                    _ => written.push((t_out, t_out + n64)),
                }
            }
            c += 1;
        }
        drop(to_daemon);

        let s = io.snapshot();
        assert_eq!(s.regime, Regime::Following);
        assert_eq!((s.seed, s.seed_bumps), (1, 0));
        // One slip, when the free clock was first followed.
        assert_eq!(s.absorbs, 1, "{s:?}");
        assert_eq!((s.late_output_cycles, s.early_input_cycles), (0, 0), "{s:?}");
        assert!(s.min_output_margin >= 0 && s.min_input_margin >= 0, "{s:?}");
        assert!(s.min_output_margin < i64::MAX && s.min_input_margin < i64::MAX, "{s:?}");
        assert_eq!(s.input_missing, 0, "{s:?}");
        assert_eq!(s.frames_capped, 0);
        assert!(s.silenced_cycles > 0);
        // The status lines the daemon logs say the same.
        let p = view.plugin();
        assert_eq!(p.min_output_margin.load(Ordering::Relaxed) as i64, s.min_output_margin);
        assert_eq!(p.min_input_margin.load(Ordering::Relaxed) as i64, s.min_input_margin);
        assert_eq!(p.silenced_cycles.load(Ordering::Relaxed), s.silenced_cycles);
        assert_eq!(p.regime.load(Ordering::Relaxed), Regime::Following.code());
        assert_eq!(p.gate.load(Ordering::Relaxed), 1);
        let (trace, entries) = view.io_trace();
        assert_eq!(trace.session.load(Ordering::Relaxed), 1);
        assert_eq!(trace.next.load(Ordering::Relaxed), 64);
        assert_eq!(entries[1].frames.load(Ordering::Relaxed), u64::from(n) | u64::from(n) << 32);
        // Most of the run had audio flowing, and it was checked.
        let cycles = c as u64;
        assert!(opened * 10 > cycles * 8, "open {opened} of {cycles}");
        assert!(checked_rx > 20 * u64::from(FS) && checked_loop > 20 * u64::from(FS));
    });
    io.stop_io();
    drop(io.attach(None));
    drop(region);
}

fn platform_now(p: &StubPlatform) -> u64 {
    use ovsc_hal::platform::Platform;
    p.now_ticks()
}

#[test]
fn io_at_32_frames() {
    run(32);
}

#[test]
fn io_at_512_frames() {
    run(512);
}

#[test]
fn io_at_4096_frames() {
    run(4096);
}
