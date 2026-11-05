//! Lock-free per-channel sample storage indexed by media time.
//!
//! The network side and the audio side of OpenVirtualSoundcard never talk to each other
//! directly. They meet in a [`TimedRing`] per channel, where every sample
//! lives at the slot of its media-clock timestamp:
//!
//! * receive flows write samples at the timestamp carried by the packet, and
//!   audio backends read them back at `now - latency`;
//! * audio backends write transmit samples at `now + lead`, and transmit flows
//!   read them at the timestamp they are about to send.
//!
//! Each slot packs the low 32 bits of the timestamp together with the sample
//! into one `AtomicU64`, so a reader can tell a fresh sample from a stale one
//! without locks: if the tag doesn't match, the slot holds old data (packet
//! loss, nothing written yet, buffer overrun) and reads as silence. Writers
//! and readers never block each other, which is what real-time audio
//! callbacks need. The tag tells timestamps apart within 2^32 samples only;
//! a [`Sweeper`] forgets samples whose writer stopped long before their tag
//! could match again.
//!
//! The slot format is [`ovsc_shm::ring`]'s, and the slots can live in
//! memory the device doesn't own (see [`TimedRing::from_raw`]), such as a
//! region shared with the macOS driver.

use std::fmt;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc::{RecvTimeoutError, TryRecvError};
use std::time::Duration;

use ovsc_proto::audio::Sample;
use ovsc_shm::ring::RingRef;

/// Default capacity: 32768 samples (≈ 680 ms at 48 kHz, 170 ms at 192 kHz).
pub const DEFAULT_CAPACITY: usize = 1 << 15;

/// A ring of samples addressed by absolute media-clock sample index.
pub struct TimedRing {
    storage: Storage,
    mask: usize,
}

/// Where a ring's slots live.
enum Storage {
    /// On the heap, owned by the ring.
    Owned(Box<[AtomicU64]>),
    /// Somewhere else, kept alive by `_keep` (see [`TimedRing::from_raw`]).
    External { ptr: NonNull<AtomicU64>, len: usize, _keep: Arc<dyn Send + Sync> },
}

// SAFETY: the ring only hands out shared references to `AtomicU64`s, which
// are `Sync`. External slots stay valid while `_keep` lives (the contract of
// `TimedRing::from_raw`), and `_keep` itself is `Send + Sync`.
unsafe impl Send for TimedRing {}
// SAFETY: see `Send`.
unsafe impl Sync for TimedRing {}

impl TimedRing {
    /// Creates a ring holding `capacity` samples (rounded up to a power of 2).
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(2).next_power_of_two();
        // Slot 0 packs (ts=0, sample=0): reading never-written slots yields
        // silence either way.
        let slots = (0..capacity).map(|_| AtomicU64::new(0)).collect();
        Self { storage: Storage::Owned(slots), mask: capacity - 1 }
    }

    /// A ring over `frames` slots at `ptr` that the ring doesn't own, such
    /// as a channel of a shared-memory region. The ring holds `keepalive`
    /// until it is dropped. The slots keep their contents: call
    /// [`TimedRing::clear`] for a fresh ring.
    ///
    /// # Panics
    ///
    /// If `frames` is not a power of two of at least 2.
    ///
    /// # Safety
    ///
    /// * `ptr` must point to `frames` consecutive, initialised `AtomicU64`s
    ///   (8-byte aligned), which stay mapped and valid for as long as
    ///   `keepalive` is alive;
    /// * that memory may only ever be accessed atomically, in this process
    ///   and any other that maps it.
    pub unsafe fn from_raw(
        ptr: NonNull<AtomicU64>,
        frames: usize,
        keepalive: Arc<dyn Send + Sync>,
    ) -> TimedRing {
        assert!(
            frames >= 2 && frames.is_power_of_two(),
            "ring of {frames} frames: not a power of two of at least 2"
        );
        Self { storage: Storage::External { ptr, len: frames, _keep: keepalive }, mask: frames - 1 }
    }

    pub fn capacity(&self) -> usize {
        self.mask + 1
    }

    #[inline]
    fn slots(&self) -> &[AtomicU64] {
        match &self.storage {
            Storage::Owned(slots) => slots,
            // SAFETY: `from_raw`'s contract: `len` initialised atomics that
            // stay valid while `_keep`, which we hold, lives.
            Storage::External { ptr, len, .. } => unsafe {
                std::slice::from_raw_parts(ptr.as_ptr(), *len)
            },
        }
    }

    /// The ring's slots. Both constructors guarantee a power-of-two length
    /// of at least 2, so this is always `Some`.
    #[inline]
    fn ring(&self) -> Option<RingRef<'_>> {
        RingRef::new(self.slots())
    }

    /// Stores `samples` at timestamps `ts, ts + 1, …`.
    #[inline]
    pub fn write(&self, ts: u64, samples: &[Sample]) {
        if let Some(r) = self.ring() {
            r.write(ts, samples);
        }
    }

    /// Stores one sample.
    #[inline]
    pub fn write_one(&self, ts: u64, sample: Sample) {
        if let Some(r) = self.ring() {
            r.write_one(ts, sample);
        }
    }

    /// Reads samples at timestamps `ts, ts + 1, …` into `out`. Missing samples
    /// read as 0. Returns the number of samples that were present.
    #[inline]
    pub fn read(&self, ts: u64, out: &mut [Sample]) -> usize {
        match self.ring() {
            Some(r) => r.read(ts, out),
            None => {
                out.fill(0);
                0
            }
        }
    }

    /// Reads one sample, or `None` if the slot holds no sample for `ts`.
    #[inline]
    pub fn read_one(&self, ts: u64) -> Option<Sample> {
        self.ring()?.read_one(ts)
    }

    /// Forgets all stored samples.
    pub fn clear(&self) {
        if let Some(r) = self.ring() {
            r.clear();
        }
    }

    /// Forgets the samples whose timestamps are more than two ring lengths
    /// from the media time `now` returns; see [`RingRef::sweep`] and
    /// [`Sweeper`]. Not real-time. Returns how many it forgot.
    pub fn sweep(&self, now: impl FnMut() -> Option<u64>) -> usize {
        self.ring().map_or(0, |r| r.sweep(now))
    }
}

/// How often a [`Sweeper`] sweeps its rings.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(5);

/// A thread that sweeps a device's rings (see [`TimedRing::sweep`]) when it
/// starts and every [`SWEEP_INTERVAL`] after, until stopped or dropped.
///
/// A slot keeps only the low 32 bits of its sample's timestamp, so the last
/// lap of a ring whose writer stopped (a receive flow that died or was
/// unsubscribed, a host that quit while the console stays subscribed to our
/// transmit channels) would match again 2^32 samples later: 24.9 h at
/// 48 kHz, 6.2 h at 192 kHz. Up to a ring's worth of old audio (680 ms at
/// 48 kHz) would then reach the host or the console as if it were live.
/// Sweeping every few seconds forgets such samples hours before that.
///
/// A sweep of one ring loads its 32768 slots and takes tens of
/// microseconds; it runs at normal priority, never on a real-time path.
pub struct Sweeper {
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Sweeper {
    /// Starts sweeping `rings` against `now`, the media time in samples
    /// (`None` while there is no clock, which skips the sweep).
    pub fn spawn(
        rings: Vec<Arc<TimedRing>>,
        interval: Duration,
        mut now: impl FnMut() -> Option<u64> + Send + 'static,
    ) -> std::io::Result<Self> {
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::Builder::new().name("ovsc-sweep".into()).spawn(move || {
            loop {
                for ring in &rings {
                    ring.sweep(&mut now);
                    // Stopping only drops the sender: leave between rings.
                    if !matches!(stopped.try_recv(), Err(TryRecvError::Empty)) {
                        return;
                    }
                }
                if !matches!(stopped.recv_timeout(interval), Err(RecvTimeoutError::Timeout)) {
                    return;
                }
            }
        })?;
        Ok(Self { stop: Some(stop), thread: Some(thread) })
    }

    /// Stops the thread and waits for it, which takes at most one ring's
    /// sweep. It then holds none of the rings.
    pub fn stop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Sweeper {
    fn drop(&mut self) {
        self.stop();
    }
}

impl fmt::Debug for Sweeper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sweeper").field("running", &self.thread.is_some()).finish()
    }
}

impl fmt::Debug for TimedRing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TimedRing")
            .field("capacity", &self.capacity())
            .field("external", &matches!(self.storage, Storage::External { .. }))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn write_then_read() {
        let ring = TimedRing::new(1000);
        assert_eq!(ring.capacity(), 1024);
        let ts = 1_700_000_000u64 * 48_000;
        ring.write(ts, &[1, 2, 3, 4]);
        let mut out = [9; 6];
        assert_eq!(ring.read(ts - 1, &mut out), 4);
        assert_eq!(out, [0, 1, 2, 3, 4, 0]);
    }

    #[test]
    fn stale_samples_read_as_silence() {
        let ring = TimedRing::new(16);
        ring.write(100, &[7; 4]);
        // Same slots, one lap later: tags don't match.
        let mut out = [1; 4];
        assert_eq!(ring.read(116, &mut out), 0);
        assert_eq!(out, [0; 4]);
        assert_eq!(ring.read_one(100), Some(7));
        // Overwriting a lap later evicts the old sample.
        ring.write_one(116, 5);
        assert_eq!(ring.read_one(100), None);
        assert_eq!(ring.read_one(116), Some(5));
    }

    #[test]
    fn negative_samples_survive_packing() {
        let ring = TimedRing::new(8);
        ring.write(3, &[i32::MIN, -1, i32::MAX]);
        let mut out = [0; 3];
        ring.read(3, &mut out);
        assert_eq!(out, [i32::MIN, -1, i32::MAX]);
        ring.clear();
        assert_eq!(ring.read_one(4), None);
    }

    #[test]
    fn concurrent_writer_and_reader() {
        let ring = Arc::new(TimedRing::new(256));
        let writer = {
            let ring = ring.clone();
            std::thread::spawn(move || {
                for ts in 0..200_000u64 {
                    ring.write_one(ts, (ts as i32).wrapping_mul(3));
                }
            })
        };
        for ts in (0..200_000u64).step_by(7) {
            if let Some(s) = ring.read_one(ts) {
                assert_eq!(s, (ts as i32).wrapping_mul(3));
            }
        }
        writer.join().unwrap();
    }

    /// Slots owned by someone else, as a shared-memory region would be.
    struct Slots(Box<[AtomicU64]>);

    fn external(frames: usize) -> (Arc<Slots>, TimedRing) {
        let slots = Arc::new(Slots((0..frames).map(|_| AtomicU64::new(0)).collect()));
        let ptr = NonNull::from(&slots.0[0]);
        // SAFETY: `frames` atomics, kept alive by the ring's clone of `slots`.
        let ring = unsafe { TimedRing::from_raw(ptr, frames, slots.clone()) };
        (slots, ring)
    }

    #[test]
    fn external_ring_uses_the_given_slots() {
        let (slots, ring) = external(16);
        assert_eq!(ring.capacity(), 16);
        ring.write(100, &[7, -8]);
        // The slot format is ovsc-shm's.
        assert_eq!(slots.0[100 % 16].load(Ordering::Relaxed), ovsc_shm::ring::pack(100, 7));
        let shared = RingRef::new(&slots.0).unwrap();
        assert_eq!(shared.read_one(101), Some(-8));
        shared.write_one(102, 9);
        let mut out = [1; 4];
        assert_eq!(ring.read(99, &mut out), 3);
        assert_eq!(out, [0, 7, -8, 9]);
        ring.clear();
        assert_eq!(ring.read_one(100), None);
        assert!(format!("{ring:?}").contains("external: true"));
    }

    #[test]
    fn external_ring_keeps_its_memory_alive() {
        let (slots, ring) = external(8);
        assert_eq!(Arc::strong_count(&slots), 2);
        let ring = Arc::new(ring);
        let reader = {
            let ring = ring.clone();
            std::thread::spawn(move || ring.read_one(3))
        };
        ring.write_one(3, 5);
        reader.join().unwrap();
        drop(ring);
        assert_eq!(Arc::strong_count(&slots), 1);
    }

    #[test]
    #[should_panic(expected = "not a power of two")]
    fn external_ring_needs_a_power_of_two() {
        external(12);
    }

    #[test]
    fn a_swept_ring_is_silent_2_pow_32_samples_later() {
        let ring = TimedRing::new(DEFAULT_CAPACITY);
        let ts = 1_700_000_000u64 * 48_000;
        ring.write(ts, &[0x7fff_ff00; 32]);
        // Unswept, the tags match again 2^32 samples on.
        assert_eq!(ring.read_one(ts + (1 << 32)), Some(0x7fff_ff00));
        assert_eq!(ring.sweep(|| Some(ts + 16)), 0);
        // A sweep any time in between forgets them.
        assert_eq!(ring.sweep(|| Some(ts + 48_000 * 3600)), 32);
        assert_eq!(ring.read_one(ts + (1 << 32)), None);
        assert_eq!(ring.read_one(ts), None);
        let (_slots, external) = external(16);
        external.write(100, &[1, 2]);
        assert_eq!(external.sweep(|| Some(100 + 33)), 1);
        assert_eq!((external.read_one(100), external.read_one(101)), (None, Some(2)));
    }

    /// Polls `f` for up to two seconds.
    fn eventually(mut f: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        f()
    }

    #[test]
    fn the_sweeper_sweeps_every_ring_at_once_and_then_periodically() {
        let rings: Vec<_> = (0..3).map(|_| Arc::new(TimedRing::new(1024))).collect();
        let now = Arc::new(AtomicU64::new(1 << 40));
        let t0 = now.load(Ordering::Relaxed);
        // Stale samples (a stopped writer, a day ago) and fresh ones.
        for (i, ring) in rings.iter().enumerate() {
            ring.write(t0 - 48_000 * 86_400 + i as u64, &[5; 8]);
            ring.write(t0 - 100, &[6; 8]);
        }
        let clock = now.clone();
        let mut sweeper = Sweeper::spawn(rings.clone(), Duration::from_millis(10), move || {
            Some(clock.load(Ordering::Relaxed))
        })
        .unwrap();
        // The first sweep comes at once and keeps what is fresh.
        let stale = |r: &TimedRing, i: usize| r.read_one(t0 - 48_000 * 86_400 + i as u64);
        assert!(eventually(|| rings.iter().enumerate().all(|(i, r)| stale(r, i).is_none())));
        assert!(rings.iter().all(|r| r.read_one(t0 - 100) == Some(6)));
        // Later sweeps follow the clock: the writer stops, time moves on.
        now.store(t0 + 10_000, Ordering::Relaxed);
        assert!(eventually(|| rings.iter().all(|r| r.read_one(t0 - 100).is_none())));
        // Stopped, it holds none of the rings.
        sweeper.stop();
        assert!(format!("{sweeper:?}").contains("running: false"));
        assert!(rings.iter().all(|r| Arc::strong_count(r) == 1));
    }

    #[test]
    fn a_sweeper_stops_at_once_whatever_its_interval() {
        let ring = Arc::new(TimedRing::new(64));
        let started = std::time::Instant::now();
        let sweeper = Sweeper::spawn(vec![ring.clone()], Duration::from_secs(3600), || None);
        drop(sweeper.unwrap());
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(Arc::strong_count(&ring), 1);
    }
}
