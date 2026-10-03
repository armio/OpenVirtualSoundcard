//! Media clock for OpenVirtualSoundcard.
//!
//! Every Dante device on a network shares one notion of time, distributed by
//! PTPv1 (IEEE 1588-2002). Audio packets are stamped with that time, expressed
//! in samples, and receivers play each sample at `timestamp + latency`.
//!
//! This crate turns the network time into something the rest of OpenVirtualSoundcard can
//! use cheaply and from real-time audio threads:
//!
//! * [`MediaClock`] is a cloneable, lock-free handle that answers "what is the
//!   network time right now?" in nanoseconds or in samples.
//! * [`ClockWriter`] is the single producer that publishes new
//!   [`ClockSnapshot`]s (a linear mapping from the local monotonic clock to
//!   network time).
//! * [`ptp`] contains the in-process PTPv1 follower that drives a
//!   [`ClockWriter`] from the network.
//! * A [`ClockMirror`] receives every change the writer makes, for copying
//!   the clock into memory shared with another process (the macOS driver).
//! * [`system_clock`] provides a clock derived from the host's wall clock,
//!   useful for tests and for running several OpenVirtualSoundcard instances on one host
//!   without any PTP master present.
//!
//! "Media time" in this crate is always the PTP master's time in nanoseconds,
//! i.e. `seconds * 1e9 + nanoseconds` of the PTP timestamps.
//!
//! The snapshot and state types and the sample conversions come from
//! `ovsc-shm`, and the published snapshot sits behind its seqlock, so
//! the daemon and the driver share one definition of both.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ovsc_shm::clock::{ClockBlock, ClockRecord};

pub use ovsc_shm::time::{ClockSnapshot, ClockState, NANOS_PER_SEC, ns_to_samples, samples_to_ns};

pub mod ptp;

/// Nanoseconds on the host's monotonic clock.
///
/// All local timestamps in OpenVirtualSoundcard (packet receive times, clock snapshots,
/// sleep deadlines) use this time base. It is system-wide, so timestamps can
/// be shared with other processes such as an audio driver:
///
/// * macOS: `CLOCK_UPTIME_RAW`, which is `mach_absolute_time` in
///   nanoseconds, the host time Core Audio uses.
/// * Linux and other Unix systems: `CLOCK_MONOTONIC`.
/// * Elsewhere: time since a process-wide epoch.
#[cfg(unix)]
pub fn local_now_ns() -> u64 {
    #[cfg(target_vendor = "apple")]
    const CLOCK: libc::clockid_t = libc::CLOCK_UPTIME_RAW;
    #[cfg(not(target_vendor = "apple"))]
    const CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC;
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid out-pointer, and both clocks exist on every
    // supported system, so this cannot fail.
    unsafe { libc::clock_gettime(CLOCK, &mut ts) };
    ts.tv_sec as u64 * NANOS_PER_SEC + ts.tv_nsec as u64
}

/// Nanoseconds on the host's monotonic clock (see the Unix version).
#[cfg(not(unix))]
pub fn local_now_ns() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let epoch = *EPOCH.get_or_init(Instant::now);
    Instant::now().saturating_duration_since(epoch).as_nanos() as u64
}

/// Identity of the PTP master the clock follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MasterInfo {
    /// PTPv1 source UUID (usually the master's MAC address).
    pub uuid: [u8; 6],
    /// PTPv1 source port id.
    pub port_id: u16,
    /// Address the master's Sync messages come from.
    pub addr: std::net::Ipv4Addr,
}

/// Diagnostic information about the clock.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ClockStatus {
    pub state: ClockState,
    pub master: Option<MasterInfo>,
    /// Last measured offset from the master (master minus us), nanoseconds.
    pub offset_ns: i64,
    /// Last measured mean path delay to the master, nanoseconds.
    pub mean_path_delay_ns: i64,
    /// Current frequency correction relative to the local clock, ppb.
    pub freq_offset_ppb: f64,
}

/// Receives every change of a [`MediaClock`], for example to copy it into
/// memory shared with another process.
///
/// [`ClockWriter`] calls the mirror after each of its own stores, from the
/// writer's thread, so a mirror always ends up with the clock's latest
/// state. Calls are serialised: a mirror never runs concurrently with
/// itself. It must not call [`MediaClock::set_mirror`] of the same clock.
pub trait ClockMirror: Send + Sync {
    /// The clock's current mapping (`None` without a time source) and its
    /// status.
    fn publish(&self, snapshot: Option<ClockSnapshot>, status: &ClockStatus);
}

/// [`MediaClock::set_mirror`] on a clock that has no writer to mirror
/// ([`system_clock`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("this clock reads the system clock on demand and cannot be mirrored")]
pub struct MirrorUnsupported;

#[derive(Debug)]
enum Source {
    Published,
    SystemTime,
}

struct Shared {
    source: Source,
    /// The published mapping, behind the same seqlock the macOS driver reads
    /// from shared memory.
    block: ClockBlock,
    status: Mutex<ClockStatus>,
    /// Only touched on writer paths and by [`MediaClock::set_mirror`].
    mirror: Mutex<Option<Arc<dyn ClockMirror>>>,
}

impl Shared {
    fn new(source: Source) -> Self {
        Self {
            source,
            block: ClockBlock::new(),
            status: Mutex::new(ClockStatus::default()),
            mirror: Mutex::new(None),
        }
    }

    fn snapshot(&self) -> Option<ClockSnapshot> {
        self.block.read_spin().filter(|r| r.valid).map(|r| r.snapshot)
    }

    fn status(&self) -> ClockStatus {
        *self.status.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Hands the current state to the mirror, if there is one.
    ///
    /// The state is read while the mirror lock is held, so a writer update
    /// racing with [`MediaClock::set_mirror`] can't leave the mirror with an
    /// older state than the clock's.
    fn update_mirror(&self) {
        let mirror = self.mirror.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(m) = mirror.as_ref() {
            m.publish(self.snapshot(), &self.status());
        }
    }
}

impl fmt::Debug for Shared {
    // The mirror is left out: a mirror that formats the clock would
    // otherwise deadlock on its own lock.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shared")
            .field("source", &self.source)
            .field("snapshot", &self.snapshot())
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

/// Cloneable, lock-free read handle to the media clock.
///
/// Reading never blocks and never allocates, so it is safe to call from
/// real-time audio threads.
#[derive(Clone, Debug)]
pub struct MediaClock {
    shared: Arc<Shared>,
}

/// The single writer of a [`MediaClock`].
#[derive(Debug)]
pub struct ClockWriter {
    shared: Arc<Shared>,
}

impl MediaClock {
    /// Creates a clock with no time yet, plus the writer that will feed it.
    pub fn new() -> (MediaClock, ClockWriter) {
        let shared = Arc::new(Shared::new(Source::Published));
        (MediaClock { shared: shared.clone() }, ClockWriter { shared })
    }

    /// The current local-to-media mapping, or `None` if the clock has no time
    /// source yet.
    pub fn snapshot(&self) -> Option<ClockSnapshot> {
        match self.shared.source {
            Source::SystemTime => Some(system_snapshot()),
            Source::Published => self.shared.snapshot(),
        }
    }

    /// Current media time in nanoseconds.
    #[inline]
    pub fn now_ns(&self) -> Option<u64> {
        self.snapshot().map(|s| s.media_ns_at(local_now_ns()))
    }

    /// Current media time as a sample index at `sample_rate`.
    #[inline]
    pub fn now_samples(&self, sample_rate: u32) -> Option<u64> {
        self.now_ns().map(|ns| ns_to_samples(ns, sample_rate))
    }

    /// How long (on the local clock) until media time reaches `media_ns`.
    /// Returns zero if that time has already passed, `None` without a clock.
    pub fn duration_until(&self, media_ns: u64) -> Option<Duration> {
        let snap = self.snapshot()?;
        let target = snap.local_ns_at(media_ns);
        Some(Duration::from_nanos(target.saturating_sub(local_now_ns())))
    }

    /// Whether the clock currently has a time source.
    pub fn is_ready(&self) -> bool {
        self.snapshot().is_some()
    }

    /// Diagnostic status.
    pub fn status(&self) -> ClockStatus {
        match self.shared.source {
            Source::SystemTime => {
                ClockStatus { state: ClockState::FreeRunning, ..Default::default() }
            }
            Source::Published => self.shared.status(),
        }
    }

    /// Installs `mirror` (or removes the current one with `None`). The new
    /// mirror immediately receives the current state, then every change the
    /// writer makes.
    ///
    /// Fails for [`system_clock`], whose time is computed on each read and
    /// never published.
    pub fn set_mirror(
        &self,
        mirror: Option<Arc<dyn ClockMirror>>,
    ) -> Result<(), MirrorUnsupported> {
        if let Source::SystemTime = self.shared.source {
            return Err(MirrorUnsupported);
        }
        *self.shared.mirror.lock().unwrap_or_else(|e| e.into_inner()) = mirror;
        self.shared.update_mirror();
        Ok(())
    }
}

impl ClockWriter {
    /// Publishes a new mapping. Readers see either the old or the new
    /// snapshot, never a mix of both.
    pub fn publish(&self, snap: ClockSnapshot) {
        self.store(snap, true);
        self.shared.update_mirror();
    }

    /// Marks the clock as having no valid time (e.g. the master disappeared
    /// and the holdover period expired).
    pub fn invalidate(&self) {
        self.store(ClockSnapshot { local_ref_ns: 0, media_ref_ns: 0, rate: 1.0 }, false);
        self.shared.update_mirror();
    }

    /// Updates the diagnostic status.
    pub fn set_status(&self, status: ClockStatus) {
        *self.shared.status.lock().unwrap_or_else(|e| e.into_inner()) = status;
        self.shared.update_mirror();
    }

    /// A read handle to the clock this writer feeds.
    pub fn clock(&self) -> MediaClock {
        MediaClock { shared: self.shared.clone() }
    }

    fn store(&self, snapshot: ClockSnapshot, valid: bool) {
        // In-process readers only look at the mapping; the state, step
        // generation and grandmaster are the mirror's business.
        self.shared.block.publish(&ClockRecord {
            snapshot,
            valid,
            state: ClockState::Unlocked,
            step_gen: 0,
            grandmaster: 0,
            publish_ns: 0,
        });
    }
}

fn system_snapshot() -> ClockSnapshot {
    let local_ref_ns = local_now_ns();
    let media_ref_ns =
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
    ClockSnapshot { local_ref_ns, media_ref_ns, rate: 1.0 }
}

/// A media clock that follows the host's wall clock (`SystemTime`).
///
/// It is not synchronised with any Dante network, but every process on the
/// same host (or on hosts whose wall clocks are tightly synchronised) agrees
/// on it. Use it for tests, demos and loopback setups.
///
/// Its time is read on demand rather than published, so it can't be
/// mirrored (see [`MediaClock::set_mirror`]).
pub fn system_clock() -> MediaClock {
    MediaClock { shared: Arc::new(Shared::new(Source::SystemTime)) }
}

/// A media clock that reads the host's wall clock once, at creation, and
/// then advances with the local monotonic clock.
///
/// Unlike [`system_clock`] it is immune to NTP adjusting the wall clock,
/// which on some hosts (freshly booted VMs, for example) slews it by more
/// than PTP's ±500 ppm range. Use it as the time source of a PTP master.
pub fn free_running_clock() -> MediaClock {
    free_running_clock_with_rate(1.0)
}

/// Like [`free_running_clock`], but media time advances `rate` media
/// nanoseconds per local nanosecond, for testing how followers cope with a
/// master whose frequency differs from theirs.
///
/// # Panics
///
/// If `rate` is not a positive, finite number.
pub fn free_running_clock_with_rate(rate: f64) -> MediaClock {
    assert!(rate.is_finite() && rate > 0.0, "invalid free-running clock rate {rate}");
    let (clock, writer) = MediaClock::new();
    writer.publish(ClockSnapshot { rate, ..system_snapshot() });
    writer.set_status(ClockStatus {
        state: ClockState::FreeRunning,
        freq_offset_ppb: (rate - 1.0) * 1e9,
        ..Default::default()
    });
    clock
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn unpublished_clock_has_no_time() {
        let (clock, _writer) = MediaClock::new();
        assert!(clock.snapshot().is_none());
        assert!(clock.now_ns().is_none());
        assert!(!clock.is_ready());
    }

    #[test]
    fn published_snapshot_is_visible() {
        let (clock, writer) = MediaClock::new();
        let snap = ClockSnapshot { local_ref_ns: 10, media_ref_ns: 1_000, rate: 1.0 };
        writer.publish(snap);
        assert_eq!(clock.snapshot(), Some(snap));
        writer.invalidate();
        assert!(clock.snapshot().is_none());
    }

    #[test]
    fn snapshot_mapping_round_trips() {
        let snap = ClockSnapshot {
            local_ref_ns: 5_000_000_000,
            media_ref_ns: 1_700_000_000_000_000_000,
            rate: 1.0 + 25e-6,
        };
        let local = 7_500_000_000;
        let media = snap.media_ns_at(local);
        assert_eq!(media, 1_700_000_000_000_000_000 + 2_500_062_500);
        let back = snap.local_ns_at(media);
        assert!((back as i64 - local as i64).abs() <= 1);
        assert!((snap.freq_offset_ppb() - 25_000.0).abs() < 1e-3);
    }

    #[test]
    fn sample_conversions() {
        assert_eq!(ns_to_samples(1_000_000_000, 48_000), 48_000);
        assert_eq!(ns_to_samples(1_500_000_000, 44_100), 66_150);
        assert_eq!(samples_to_ns(48_000, 48_000), 1_000_000_000);
        // Large media times (decades since the epoch) must not overflow.
        let ns = 2_000_000_000u64 * 1_000_000_000;
        assert_eq!(ns_to_samples(ns, 96_000), 2_000_000_000u64 * 96_000);
    }

    #[test]
    fn system_clock_advances() {
        let clock = system_clock();
        let a = clock.now_ns().unwrap();
        std::thread::sleep(Duration::from_millis(2));
        let b = clock.now_ns().unwrap();
        assert!(b > a);
        assert_eq!(clock.status().state, ClockState::FreeRunning);
    }

    #[test]
    fn free_running_clock_starts_at_wall_time_and_runs_at_rate_one() {
        let clock = free_running_clock();
        let wall = system_clock().now_ns().unwrap();
        let ours = clock.now_ns().unwrap();
        assert!((ours as i64 - wall as i64).abs() < 50_000_000);
        assert_eq!(clock.snapshot().unwrap().rate, 1.0);
        assert_eq!(clock.status().state, ClockState::FreeRunning);
    }

    #[test]
    fn free_running_clock_with_rate_runs_at_that_rate() {
        let clock = free_running_clock_with_rate(1.00005);
        assert_eq!(clock.snapshot().unwrap().rate, 1.00005);
        let status = clock.status();
        assert_eq!(status.state, ClockState::FreeRunning);
        assert!((status.freq_offset_ppb - 50_000.0).abs() < 1e-3);
        let wall = system_clock().now_ns().unwrap();
        assert!((clock.now_ns().unwrap() as i64 - wall as i64).abs() < 50_000_000);
    }

    #[test]
    #[should_panic(expected = "invalid free-running clock rate")]
    fn free_running_clock_rejects_bad_rates() {
        free_running_clock_with_rate(f64::NAN);
    }

    /// Records every call, as (snapshot, state).
    #[derive(Default)]
    struct Recorder(Mutex<Vec<(Option<ClockSnapshot>, ClockState)>>);

    impl ClockMirror for Recorder {
        fn publish(&self, snapshot: Option<ClockSnapshot>, status: &ClockStatus) {
            self.0.lock().unwrap().push((snapshot, status.state));
        }
    }

    impl Recorder {
        fn take(&self) -> Vec<(Option<ClockSnapshot>, ClockState)> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }
    }

    #[test]
    fn mirror_sees_every_change_in_order() {
        let (clock, writer) = MediaClock::new();
        let a = ClockSnapshot { local_ref_ns: 10, media_ref_ns: 1_000, rate: 1.0 };
        writer.publish(a);
        writer.set_status(ClockStatus { state: ClockState::Locking, ..Default::default() });

        // The current state arrives as soon as the mirror is installed.
        let recorder = Arc::new(Recorder::default());
        clock.set_mirror(Some(recorder.clone())).unwrap();
        assert_eq!(recorder.take(), [(Some(a), ClockState::Locking)]);

        let b = ClockSnapshot { local_ref_ns: 20, media_ref_ns: 2_000, rate: 1.0 + 1e-6 };
        writer.publish(b);
        writer.set_status(ClockStatus { state: ClockState::Locked, ..Default::default() });
        writer.invalidate();
        writer.set_status(ClockStatus::default());
        assert_eq!(
            recorder.take(),
            [
                (Some(b), ClockState::Locking),
                (Some(b), ClockState::Locked),
                (None, ClockState::Locked),
                (None, ClockState::Unlocked),
            ]
        );
        // Each call happens after the store, so the mirror never runs ahead
        // of in-process readers.
        assert!(clock.snapshot().is_none());

        // Removed mirrors hear nothing more, and removing calls nothing.
        clock.set_mirror(None).unwrap();
        writer.publish(a);
        assert!(recorder.take().is_empty());
    }

    #[test]
    fn mirror_replays_a_free_running_clock() {
        let clock = free_running_clock_with_rate(0.9999);
        let recorder = Arc::new(Recorder::default());
        clock.set_mirror(Some(recorder.clone())).unwrap();
        let calls = recorder.take();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0.unwrap().rate, 0.9999);
        assert_eq!(calls[0].1, ClockState::FreeRunning);
    }

    #[test]
    fn system_clock_cannot_be_mirrored() {
        let recorder = Arc::new(Recorder::default());
        assert_eq!(system_clock().set_mirror(Some(recorder.clone())), Err(MirrorUnsupported));
        assert_eq!(system_clock().set_mirror(None), Err(MirrorUnsupported));
        assert!(recorder.take().is_empty());
    }

    #[test]
    fn concurrent_readers_never_see_torn_snapshots() {
        let (clock, writer) = MediaClock::new();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let clock = clock.clone();
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        if let Some(s) = clock.snapshot() {
                            // The writer always keeps media_ref == local_ref * 2.
                            assert_eq!(s.media_ref_ns, s.local_ref_ns * 2);
                        }
                    }
                })
            })
            .collect();
        for i in 0..200_000u64 {
            writer.publish(ClockSnapshot { local_ref_ns: i, media_ref_ns: i * 2, rate: 1.0 });
        }
        stop.store(true, Ordering::Relaxed);
        for r in readers {
            r.join().unwrap();
        }
    }
}
