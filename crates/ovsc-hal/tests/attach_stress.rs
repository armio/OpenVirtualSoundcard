//! The retire protocol of design section 11 under load: 100,000 attachment
//! swaps while an IO thread runs GetZeroTimeStamp, ReadInput and WriteMix
//! back to back. Each retired attachment is neutralized at once and
//! unmapped as soon as no reader is inside; an IO thread that could still
//! touch it would fault and end the test.
//!
//! Every region carries a live daemon clock, so the IO thread opens the gate
//! on the attachments it sees and reads and writes their rings, not only
//! their status blocks.
//!
//! The nightly job runs this test under ThreadSanitizer as well.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ovsc_hal::abi::*;
use ovsc_hal::io::IoEngine;
use ovsc_hal::model::DriverConfig;
use ovsc_hal::platform::stub::{StubPlatform, host_now_ns};
use ovsc_ipc::region::{MappedRegion, SharedRegion};
use ovsc_shm::clock::ClockRecord;
use ovsc_shm::layout::{HOST_ARCH, HeaderInit, REGION_SIZE, RegionRef};
use ovsc_shm::status::{AudioWord, DAEMON_ENGINE_RUNNING};
use ovsc_shm::time::{ClockSnapshot, ClockState, Timebase};

const SWAPS: u64 = 100_000;
const CHANNELS: u32 = 2;
const FRAMES: u32 = 64;

/// A region of daemon generation `generation` whose clock is the host clock
/// (media time = host time), alive and running at 48 kHz, mapped as the
/// driver would map it. The creator's reference is dropped, so the mapping
/// is the region's last owner.
fn region(generation: u64) -> MappedRegion {
    let r = SharedRegion::create(REGION_SIZE).unwrap();
    let now = host_now_ns();
    let h = HeaderInit {
        daemon_generation: generation,
        daemon_pid: 1,
        arch: HOST_ARCH,
        timebase: Timebase::NANOS,
        created_host_ns: now,
        daemon_version: HeaderInit::version_bytes("attach_stress"),
    };
    // SAFETY: a fresh region, laid out before anyone else sees it; the view
    // is not used after this function.
    let view = unsafe { RegionRef::init(r.as_ptr(), r.len(), &h) }.unwrap();
    view.clock().publish(&ClockRecord {
        snapshot: ClockSnapshot { local_ref_ns: 0, media_ref_ns: 0, rate: 1.0 },
        valid: true,
        state: ClockState::FreeRunning,
        step_gen: 0,
        grandmaster: 0,
        publish_ns: now,
    });
    let d = view.daemon();
    // Fresh for far longer than the region lives.
    d.heartbeat_ns.store(now, Ordering::Release);
    d.audio_word.store(AudioWord { sample_rate: 48_000, config_gen: 1 }.pack(), Ordering::Release);
    d.flags.store(DAEMON_ENGINE_RUNNING, Ordering::Release);
    r.handle().map().unwrap()
}

#[test]
fn swaps_under_running_io() {
    let platform = StubPlatform::with_host_clock().leak();
    let names: Vec<String> = (1..=CHANNELS).map(|i| format!("{i:02}")).collect();
    let cfg = DriverConfig {
        input_channels: CHANNELS,
        output_channels: CHANNELS,
        input_names: names.clone(),
        output_names: names,
        ..DriverConfig::fallback()
    };
    let io = IoEngine::new(&cfg, platform);
    io.apply_config(&cfg);
    io.reset_timeline(cfg.sample_rate);
    drop(io.attach(Some(io.new_attachment(region(1)).unwrap())));
    io.start_io();

    let stop = AtomicBool::new(false);
    let cycles = AtomicU64::new(0);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let n = (CHANNELS * FRAMES) as usize;
            let mut input = vec![0f32; n];
            let mut output = vec![0.25f32; n];
            let mut cycle = IOCycleInfo::default();
            let mut k = 0u64;
            while !stop.load(Ordering::Relaxed) {
                k += 1;
                let (sample, ..) = io.zero_timestamp();
                cycle.mIOCycleCounter = k;
                cycle.mInputTime.mSampleTime = sample;
                cycle.mOutputTime.mSampleTime = sample + 1000.0;
                let read = kAudioServerPlugInIOOperationReadInput;
                let main = input.as_mut_ptr().cast::<c_void>();
                assert_eq!(unsafe { io.do_io(3, read, FRAMES, &cycle, main) }, 0);
                let mix = kAudioServerPlugInIOOperationWriteMix;
                let main = output.as_mut_ptr().cast::<c_void>();
                assert_eq!(unsafe { io.do_io(4, mix, FRAMES, &cycle, main) }, 0);
                cycles.fetch_add(1, Ordering::Relaxed);
            }
        });

        let mut waits = 0u64;
        for generation in 2..SWAPS + 2 {
            let new = io.new_attachment(region(generation)).unwrap();
            let Some(old) = io.attach(Some(new)) else {
                panic!("no attachment to retire at generation {generation}");
            };
            // Retire: neutralize now, unmap once no reader is inside.
            old.mapped.neutralize().unwrap();
            while !io.quiescent() {
                waits += 1;
                std::hint::spin_loop();
            }
            drop(old);
        }
        stop.store(true, Ordering::Relaxed);
        eprintln!("{SWAPS} swaps, {} IO cycles, {waits} quiescence polls", {
            cycles.load(Ordering::Relaxed)
        });
    });

    let s = io.snapshot();
    assert!(!s.faulted);
    assert_eq!(s.attached_generation, Some(SWAPS + 1));
    let ops = 2 * cycles.load(Ordering::Relaxed);
    assert_eq!(s.read_calls + s.write_calls, ops);
    assert!(ops > 1000, "the IO thread hardly ran: {s:?}");
    // The gate opened on attachments the IO thread saw, so it used rings.
    assert!(s.silenced_cycles < ops, "{s:?}");
    io.stop_io();
    drop(io.attach(None));
}
