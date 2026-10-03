//! The shared region across processes: a forked child plays the daemon,
//! publishing the clock block and writing rings a million times, while the
//! parent reads them like the driver. The parent must never see a torn
//! clock record or a ring slot whose sample does not belong to its tag.
//!
//! Runs without the test harness (`harness = false`), so the process has a
//! single thread when it forks.

#[cfg(unix)]
fn main() {
    imp::run();
}

#[cfg(not(unix))]
fn main() {
    println!("cross_process: Unix only, skipped");
}

#[cfg(unix)]
mod imp {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::Ordering;

    use ovsc_ipc::region::SharedRegion;
    use ovsc_shm::clock::{ClockRead, ClockRecord, READ_TRIES};
    use ovsc_shm::layout::{HOST_ARCH, HeaderInit, REGION_SIZE, RING_FRAMES, RegionRef};
    use ovsc_shm::time::{ClockSnapshot, ClockState, Timebase};

    const WRITES: u64 = 1_000_000;
    const CHANNELS: usize = 4;

    /// A record whose every field derives from `i`, so a mix of two
    /// records is detectable.
    fn record(i: u64) -> ClockRecord {
        ClockRecord {
            snapshot: ClockSnapshot {
                local_ref_ns: i,
                media_ref_ns: i.wrapping_mul(0x9E37_79B9_7F4A_7C15),
                rate: 1.0 + (i % 1000) as f64 * 1e-9,
            },
            valid: i % 3 != 0,
            state: ClockState::from_u8((i % 4) as u8).unwrap(),
            step_gen: i / 7,
            grandmaster: !i,
            publish_ns: i ^ 0x5555_5555_5555_5555,
        }
    }

    /// The sample channel `c` carries at time `ts`.
    fn sample(c: usize, ts: u64) -> i32 {
        (ts as u32).wrapping_mul(2_654_435_761).wrapping_add(c as u32 * 0x0100_0193) as i32
    }

    /// The daemon: never returns.
    fn child(view: RegionRef<'_>) -> ! {
        let ok = catch_unwind(AssertUnwindSafe(|| {
            let rings: Vec<_> = (0..CHANNELS).map(|c| view.rx(c).unwrap()).collect();
            for i in 1..=WRITES {
                view.clock().publish(&record(i));
                for (c, ring) in rings.iter().enumerate() {
                    ring.write_one(i, sample(c, i));
                }
                view.daemon().heartbeat_ns.store(i, Ordering::Release);
            }
        }))
        .is_ok();
        // SAFETY: ends the child without running the parent's exit code.
        unsafe { libc::_exit(if ok { 0 } else { 1 }) }
    }

    #[derive(Default)]
    struct Seen {
        records: u64,
        contended: u64,
        last: u64,
        ring_hits: u64,
        ring_misses: u64,
    }

    impl Seen {
        fn check(&mut self, view: RegionRef<'_>) {
            match view.clock().read_bounded(READ_TRIES) {
                ClockRead::Record(r) => {
                    let i = r.snapshot.local_ref_ns;
                    assert_eq!(r, record(i), "torn clock record");
                    assert!(i >= self.last, "clock went back from {} to {i}", self.last);
                    self.last = i;
                    self.records += 1;
                }
                ClockRead::Contended => self.contended += 1,
                ClockRead::NeverWritten => assert_eq!(self.last, 0, "clock block went blank"),
            }
            let progress = view.daemon().heartbeat_ns.load(Ordering::Acquire);
            for back in [0, 1, 64, RING_FRAMES as u64 / 2, RING_FRAMES as u64 - 1] {
                let ts = progress.saturating_sub(back).max(1);
                for c in 0..CHANNELS {
                    match view.rx(c).unwrap().read_one(ts) {
                        Some(v) => {
                            assert_eq!(v, sample(c, ts), "wrong sample for tag {ts} on {c}");
                            self.ring_hits += 1;
                        }
                        None => self.ring_misses += 1,
                    }
                }
            }
        }
    }

    pub fn run() {
        let region = SharedRegion::create(REGION_SIZE).expect("create the region");
        let init = HeaderInit {
            daemon_generation: 0x0D17_0001,
            daemon_pid: std::process::id(),
            arch: HOST_ARCH,
            timebase: Timebase::NANOS,
            created_host_ns: 1,
            daemon_version: HeaderInit::version_bytes("cross-process test"),
        };
        // SAFETY: the region is fresh and not yet shared; it outlives every
        // view (the child exits, the parent drops it last).
        let view = unsafe { RegionRef::init(region.as_ptr(), region.len(), &init) }.unwrap();

        // SAFETY: the process is single-threaded (no test harness).
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork: {}", std::io::Error::last_os_error());
        if pid == 0 {
            child(view);
        }

        // The driver: read until the child is done.
        // SAFETY: as for init; this process's mapping is shared with the
        // child's.
        let mapped = unsafe { RegionRef::from_raw(region.as_ptr(), region.len()) }.unwrap();
        let mut seen = Seen::default();
        let mut status = 0;
        let mut loops = 0u64;
        loop {
            seen.check(mapped);
            loops += 1;
            if loops % 256 == 0 {
                // SAFETY: waits for our own child without blocking.
                let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
                assert!(r >= 0, "waitpid: {}", std::io::Error::last_os_error());
                if r == pid {
                    break;
                }
            }
        }
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "child failed: {status:#x}"
        );

        // Everything the child wrote last is there, and nothing older.
        seen.check(mapped);
        assert_eq!(seen.last, WRITES);
        match mapped.clock().read_bounded(READ_TRIES) {
            ClockRead::Record(r) => assert_eq!(r, record(WRITES)),
            other => panic!("final record: {other:?}"),
        }
        for c in 0..CHANNELS {
            let ring = mapped.rx(c).unwrap();
            for ts in WRITES + 1 - RING_FRAMES as u64..=WRITES {
                assert_eq!(ring.read_one(ts), Some(sample(c, ts)), "channel {c} at {ts}");
            }
            assert_eq!(ring.read_one(WRITES - RING_FRAMES as u64), None, "overwritten slot");
            assert_eq!(mapped.rx(CHANNELS).unwrap().read_one(WRITES), None, "unused channel");
        }
        assert!(seen.records > 1, "no concurrent reads: {} records", seen.records);
        println!(
            "cross_process: ok ({} loops, {} records, {} contended, {} ring hits, {} misses)",
            loops, seen.records, seen.contended, seen.ring_hits, seen.ring_misses
        );
    }
}
