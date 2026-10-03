//! The real-time paths never allocate (design section 13): a counting global
//! allocator sees no allocation and no free across 10,000 cycles of
//! GetZeroTimeStamp, ReadInput and WriteMix at 64 channels and 512 frames,
//! with the daemon attached and audio flowing.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};

use ovsc_hal::abi::*;
use ovsc_hal::io::IoEngine;
use ovsc_hal::model::DriverConfig;
use ovsc_hal::platform::Timebase;
use ovsc_hal::platform::stub::StubPlatform;
use ovsc_ipc::region::SharedRegion;
use ovsc_shm::clock::ClockRecord;
use ovsc_shm::layout::{HOST_ARCH, HeaderInit, REGION_SIZE, RING_FRAMES, RegionRef};
use ovsc_shm::status::{AudioWord, DAEMON_ENGINE_RUNNING};
use ovsc_shm::time::{ClockSnapshot, ClockState, ns_to_samples};

/// Counts allocations and frees made by threads that turned counting on.
struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static FREES: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

fn count(counter: &AtomicU64) {
    if COUNTING.try_with(Cell::get).unwrap_or(false) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: every call goes straight to the system allocator.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(&ALLOCS);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(&ALLOCS);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count(&ALLOCS);
        count(&FREES);
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        count(&FREES);
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const CHANNELS: u32 = 64;
const FRAMES: u32 = 512;
const CYCLES: u64 = 10_000;
const SEC: u64 = 1_000_000_000;

#[test]
fn real_time_paths_do_not_allocate() {
    let platform = StubPlatform::new().leak();
    platform.set_timebase(125, 3);
    let tb = Timebase { numer: 125, denom: 3 };
    let start_ns = 5 * SEC;
    platform.set_now_ticks(tb.ns_to_ticks_ceil(start_ns));

    let names: Vec<String> = (1..=CHANNELS).map(|i| format!("{i:02}")).collect();
    let cfg = DriverConfig {
        input_channels: CHANNELS,
        output_channels: CHANNELS,
        input_names: names.clone(),
        output_names: names,
        ..DriverConfig::fallback()
    };
    cfg.validate().unwrap();
    let io = IoEngine::new(&cfg, platform);
    io.apply_config(&cfg);
    io.reset_timeline(cfg.sample_rate);

    // The daemon: a free clock at +20 ppm, running at 48 kHz, and one ring's
    // worth of RX samples around the start.
    let region = SharedRegion::create(REGION_SIZE).unwrap();
    let h = HeaderInit {
        daemon_generation: 77,
        daemon_pid: 1,
        arch: HOST_ARCH,
        timebase: tb,
        created_host_ns: start_ns,
        daemon_version: HeaderInit::version_bytes("io_noalloc"),
    };
    // SAFETY: a fresh region that outlives the test's use of the view.
    let view = unsafe { RegionRef::init(region.as_ptr(), region.len(), &h) }.unwrap();
    let snapshot = ClockSnapshot { local_ref_ns: start_ns, media_ref_ns: 1 << 60, rate: 1.000_02 };
    view.clock().publish(&ClockRecord {
        snapshot,
        valid: true,
        state: ClockState::FreeRunning,
        step_gen: 0,
        grandmaster: 0,
        publish_ns: start_ns,
    });
    let d = view.daemon();
    d.heartbeat_ns.store(start_ns, Ordering::Release);
    d.audio_word.store(AudioWord { sample_rate: 48_000, config_gen: 1 }.pack(), Ordering::Release);
    d.flags.store(DAEMON_ENGINE_RUNNING, Ordering::Release);
    let media = ns_to_samples(snapshot.media_ns_at(start_ns), 48_000);
    for c in 0..CHANNELS as usize {
        let ring = view.rx(c).unwrap();
        for m in media - RING_FRAMES as u64 / 2..media + RING_FRAMES as u64 / 2 {
            ring.write_one(m, (m as i32) << 8);
        }
    }
    let a = io.new_attachment(region.handle().map().unwrap()).unwrap();
    drop(io.attach(Some(a)));
    io.start_io();

    let n = FRAMES as usize * CHANNELS as usize;
    let mut input = vec![0f32; n];
    let mut output: Vec<f32> = (0..n).map(|i| (i % 1000) as f32 / 1024.0).collect();
    let mut cycle = IOCycleInfo { mNominalIOBufferFrameSize: FRAMES, ..Default::default() };
    let period_ns = u64::from(FRAMES) * SEC / 48_000;
    let read = kAudioServerPlugInIOOperationReadInput;
    let mix = kAudioServerPlugInIOOperationWriteMix;

    COUNTING.with(|c| c.set(true));
    for k in 1..=CYCLES {
        let now_ns = start_ns + k * period_ns;
        platform.set_now_ticks(tb.ns_to_ticks_ceil(now_ns));
        d.heartbeat_ns.store(now_ns, Ordering::Release);
        let (_, host, _) = io.zero_timestamp();
        let t = (k * u64::from(FRAMES)) as f64;
        cycle.mIOCycleCounter = k;
        cycle.mCurrentTime.mSampleTime = t;
        cycle.mCurrentTime.mHostTime = host;
        cycle.mInputTime.mSampleTime = t - f64::from(FRAMES) - 216.0;
        cycle.mOutputTime.mSampleTime = t + f64::from(FRAMES) + 55.0;
        let main = input.as_mut_ptr().cast::<c_void>();
        assert_eq!(unsafe { io.do_io(3, read, FRAMES, &cycle, main) }, 0);
        let main = output.as_mut_ptr().cast::<c_void>();
        assert_eq!(unsafe { io.do_io(4, mix, FRAMES, &cycle, main) }, 0);
    }
    COUNTING.with(|c| c.set(false));

    let (allocs, frees) = (ALLOCS.load(Ordering::Relaxed), FREES.load(Ordering::Relaxed));
    assert_eq!((allocs, frees), (0, 0), "allocations and frees on the real-time paths");
    // Audio flowed the whole time, with samples present at first.
    let s = io.snapshot();
    assert!(s.gate, "{s:?}");
    assert_eq!((s.read_calls, s.write_calls, s.silenced_cycles), (CYCLES, CYCLES, 0), "{s:?}");
    assert!(s.input_missing < CYCLES * n as u64, "{s:?}");
    io.stop_io();
}
