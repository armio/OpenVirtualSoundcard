//! Concurrent GetZeroTimeStamp callers (design section 13, risk R4): four
//! threads move the host clock on and ask for zero time stamps as fast as
//! they can. Whoever gets the timeline computes; the others get the last
//! time stamp handed out.
//!
//! Each thread must see time stamps in order: the same one again, unchanged,
//! or a later one at a later host time, never later than now. Together the
//! threads must have seen every time stamp of the timeline, one period
//! apart, each with the same host time whichever thread saw it. Only a new
//! seed may start a new sequence, and seeds only grow.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use ovsc_hal::io::IoEngine;
use ovsc_hal::model::DriverConfig;
use ovsc_hal::platform::stub::StubPlatform;
use ovsc_hal::platform::{Platform, Timebase};
use ovsc_ipc::region::SharedRegion;
use ovsc_shm::clock::ClockRecord;
use ovsc_shm::layout::{HOST_ARCH, HeaderInit, REGION_SIZE, RegionRef};
use ovsc_shm::status::{AudioWord, DAEMON_ENGINE_RUNNING};
use ovsc_shm::time::{ClockSnapshot, ClockState};

const THREADS: usize = 4;
const CALLS: usize = 50_000;
const PERIOD: f64 = 16384.0;
const SEC: u64 = 1_000_000_000;
const STEPS_PER_PERIOD: usize = 64;

#[test]
fn concurrent_callers_see_consecutive_time_stamps() {
    let platform = StubPlatform::new().leak();
    let tb = Timebase { numer: 125, denom: 3 };
    platform.set_timebase(tb.numer, tb.denom);
    let start_ns = 3 * SEC;
    platform.set_now_ticks(tb.ns_to_ticks_ceil(start_ns));

    let cfg = DriverConfig::fallback();
    let io = IoEngine::new(&cfg, platform);
    io.apply_config(&cfg);
    io.reset_timeline(cfg.sample_rate);

    // A daemon on a free clock at -80 ppm, so the timeline follows it.
    let region = SharedRegion::create(REGION_SIZE).unwrap();
    let h = HeaderInit {
        daemon_generation: 5,
        daemon_pid: 1,
        arch: HOST_ARCH,
        timebase: tb,
        created_host_ns: start_ns,
        daemon_version: HeaderInit::version_bytes("zts_concurrency"),
    };
    // SAFETY: a fresh region that outlives the test's use of the view.
    let view = unsafe { RegionRef::init(region.as_ptr(), region.len(), &h) }.unwrap();
    view.clock().publish(&ClockRecord {
        snapshot: ClockSnapshot { local_ref_ns: start_ns, media_ref_ns: 1 << 50, rate: 0.999_92 },
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
    let a = io.new_attachment(region.handle().map().unwrap()).unwrap();
    drop(io.attach(Some(a)));
    io.start_io();

    // A 64th of a period per call and thread.
    let step_ns = 16384 * SEC / 48_000 / STEPS_PER_PERIOD as u64;
    let seen: Vec<Vec<(u64, f64, u64)>> = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..THREADS)
            .map(|_| {
                scope.spawn(|| {
                    let first = io.zero_timestamp();
                    let mut last = first;
                    // The distinct time stamps this thread saw: (seed,
                    // sample time, host ticks).
                    let mut seen = vec![(first.2, first.0, first.1)];
                    for _ in 0..CALLS {
                        platform.advance_ns(step_ns);
                        // The daemon stays alive.
                        let now_ns = tb.ticks_to_ns(platform.now_ticks());
                        d.heartbeat_ns.fetch_max(now_ns, Ordering::AcqRel);
                        let zts = io.zero_timestamp();
                        let now = platform.now_ticks();
                        let (sample, host, seed) = zts;
                        assert!(host <= now, "{zts:?} after now {now}");
                        assert_eq!(sample % PERIOD, 0.0, "{zts:?}");
                        let (last_sample, last_host, last_seed) = last;
                        if seed == last_seed && sample == last_sample {
                            // A reported time stamp never changes.
                            assert_eq!(host, last_host, "{last:?} then {zts:?}");
                            continue;
                        }
                        if seed == last_seed {
                            // Later time stamps of the timeline, at later
                            // host times; other threads may have taken some
                            // in between.
                            assert!(sample > last_sample, "{last:?} then {zts:?}");
                            assert!(host > last_host, "{last:?} then {zts:?}");
                        } else {
                            assert!(seed > last_seed, "{last:?} then {zts:?}");
                        }
                        seen.push((seed, sample, host));
                        last = zts;
                    }
                    seen
                })
            })
            .collect();
        threads.into_iter().map(|t| t.join().unwrap()).collect()
    });

    // Together, the threads saw every time stamp of each timeline in turn,
    // each with one host time, whichever thread saw it.
    let mut all: BTreeMap<(u64, u64), u64> = BTreeMap::new();
    for &(seed, sample, host) in seen.iter().flatten() {
        let prev = all.insert((seed, sample as u64), host);
        assert!(prev.is_none_or(|h| h == host), "seed {seed} sample {sample}: {prev:?} and {host}");
    }
    let mut knots = 0;
    for (&(seed, sample), &host) in &all {
        if let Some((&(next_seed, next_sample), &next_host)) =
            all.range((seed, sample + 1)..).next()
        {
            if next_seed == seed {
                assert_eq!(next_sample, sample + PERIOD as u64, "seed {seed}: gap after {sample}");
                assert!(next_host > host);
                knots += 1;
            }
        }
    }
    let s = io.snapshot();
    assert_eq!(s.zts_calls, (THREADS * (CALLS + 1)) as u64);
    // The clock moved on by this many periods. A caller that holds the
    // timeline while the others move the clock more than 4 periods on makes
    // the timeline jump to the present with a new seed, as an IO stall
    // would. On an idle machine nearly every period gets its time stamp; a
    // loaded one preempts callers for long stretches, so only check that
    // the timeline made progress at all.
    let periods = THREADS * CALLS / STEPS_PER_PERIOD;
    assert!(knots > periods / 50, "{knots} of {periods} time stamps, {s:?}");
    io.stop_io();
}
