//! Lock-free per-channel sample rings indexed by media time.
//!
//! This is the slot format of `ovsc-core`'s `TimedRing`, which now
//! delegates to it, so rings in the shared region and rings on the heap are
//! bit-identical. Every sample lives at the slot of its media-clock
//! timestamp, packed with the low 32 bits of that timestamp into one
//! `AtomicU64`: `(low32(ts) << 32) | (sample as u32)`. A reader can tell a
//! fresh sample from a stale one without locks: if the tag doesn't match, the
//! slot holds old data (packet loss, nothing written yet, buffer overrun)
//! and reads as silence. Writers and readers never block each other, which
//! is what real-time audio callbacks need.
//!
//! The tag only tells timestamps apart within 2^32 samples (24.9 h at
//! 48 kHz, 6.2 h at 192 kHz): a sample that nobody overwrites, because its
//! writer stopped, matches again at `ts + 2^32`, and a reader there would
//! take a ring's worth of old audio for fresh. [`RingRef::sweep`], run now
//! and then off the real-time paths, forgets such samples long before that.

#![forbid(unsafe_code)]
#![deny(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects
)]

use core::fmt;
use core::mem::size_of;
use core::sync::atomic::{AtomicU64, Ordering};

/// Packs a sample and its timestamp into one slot value.
#[inline]
pub fn pack(ts: u64, sample: i32) -> u64 {
    ((ts as u32 as u64) << 32) | sample as u32 as u64
}

/// The sample in `slot` if its tag matches `ts`, `None` if the slot holds
/// another timestamp's sample.
#[inline]
pub fn unpack(slot: u64, ts: u64) -> Option<i32> {
    ((slot >> 32) as u32 == ts as u32).then_some(slot as u32 as i32)
}

/// How far, in samples, the nearest timestamp a slot's tag allows is from
/// media time `now`: the tag is the low 32 bits of the timestamp, so the
/// answer is at most 2^31.
#[inline]
fn tag_distance(slot: u64, now: u64) -> u32 {
    let tag = (slot >> 32) as u32;
    (tag.wrapping_sub(now as u32) as i32).unsigned_abs()
}

/// Slots [`RingRef::sweep`] loads before it reads the clock.
const SWEEP_CHUNK: usize = 64;

/// A view of a ring of samples addressed by absolute media-clock sample
/// index. The slots may live anywhere: on the heap, or in memory shared with
/// another process.
#[derive(Clone, Copy)]
pub struct RingRef<'a> {
    slots: &'a [AtomicU64],
    mask: usize,
}

impl<'a> RingRef<'a> {
    /// A ring over `slots`, whose length must be a power of two and at least
    /// 2; `None` otherwise.
    pub const fn new(slots: &'a [AtomicU64]) -> Option<Self> {
        let len = slots.len();
        if len < 2 || !len.is_power_of_two() {
            return None;
        }
        Some(Self { slots, mask: len.wrapping_sub(1) })
    }

    /// Number of samples the ring holds.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    #[inline]
    fn slot(&self, ts: u64) -> Option<&'a AtomicU64> {
        self.slots.get(ts as usize & self.mask)
    }

    /// Stores `samples` at timestamps `ts, ts + 1, …`.
    #[inline]
    pub fn write(&self, ts: u64, samples: &[i32]) {
        for (i, &s) in samples.iter().enumerate() {
            self.write_one(ts.wrapping_add(i as u64), s);
        }
    }

    /// Stores one sample.
    #[inline]
    pub fn write_one(&self, ts: u64, sample: i32) {
        if let Some(slot) = self.slot(ts) {
            slot.store(pack(ts, sample), Ordering::Relaxed);
        }
    }

    /// Reads samples at timestamps `ts, ts + 1, …` into `out`. Missing samples
    /// read as 0. Returns the number of samples that were present.
    #[inline]
    pub fn read(&self, ts: u64, out: &mut [i32]) -> usize {
        let mut present = 0usize;
        for (i, slot) in out.iter_mut().enumerate() {
            match self.read_one(ts.wrapping_add(i as u64)) {
                Some(s) => {
                    *slot = s;
                    present = present.wrapping_add(1);
                }
                None => *slot = 0,
            }
        }
        present
    }

    /// Reads one sample, or `None` if the slot holds no sample for `ts`.
    #[inline]
    pub fn read_one(&self, ts: u64) -> Option<i32> {
        unpack(self.slot(ts)?.load(Ordering::Relaxed), ts)
    }

    /// Forgets all stored samples.
    ///
    /// Slots go back to 0, the never-written value: it reads as silence for
    /// every timestamp (a matching tag of 0 also carries sample 0).
    pub fn clear(&self) {
        for slot in self.slots {
            slot.store(0, Ordering::Relaxed);
        }
    }

    /// How far from the current media time, in samples, a sample may be
    /// before [`RingRef::sweep`] forgets it: two ring lengths. `None` for a
    /// ring so large (over 2^29 samples) that two lengths reach half the
    /// tag's range, which the ring never sweeps.
    pub fn sweep_window(&self) -> Option<u32> {
        let window = self.capacity().checked_mul(2)?;
        u32::try_from(window).ok().filter(|&w| w <= 1 << 30)
    }

    /// Forgets every sample whose timestamp is more than
    /// [`RingRef::sweep_window`] samples from the media time `now` returns,
    /// and returns how many it forgot. Stops early if `now` returns `None`
    /// (no clock).
    ///
    /// Run it more often than every `2^32 - 2 · window` samples (hours at
    /// any rate) and a sample written at `ts` is gone before `ts + 2^32`,
    /// where its tag would match again: a ring whose writer stopped then
    /// reads as silence for ever, not just for 2^32 samples.
    ///
    /// Not real-time: it reads the whole ring, and the clock once per 64
    /// slots. It never forgets a sample a reader could still want:
    ///
    /// * readers and writers only touch timestamps within one ring length
    ///   of now (older slots have been overwritten by then, newer ones would
    ///   overwrite what is about to be read), and the window is twice that;
    /// * the clock is read after the slots it judges were loaded, so every
    ///   sample is judged by a time no older than its write, however long
    ///   the sweep was preempted;
    /// * a slot is cleared with a compare-exchange against the value judged,
    ///   so a sample written meanwhile is kept.
    ///
    /// A cleared slot holds 0, the never-written value.
    pub fn sweep(&self, mut now: impl FnMut() -> Option<u64>) -> usize {
        let Some(window) = self.sweep_window() else {
            return 0;
        };
        let mut forgotten = 0usize;
        for chunk in self.slots.chunks(SWEEP_CHUNK) {
            let mut seen = [0u64; SWEEP_CHUNK];
            for (v, slot) in seen.iter_mut().zip(chunk) {
                *v = slot.load(Ordering::Relaxed);
            }
            // A 0 never matches (or, in slot 0, matches as silence).
            if seen.iter().all(|&v| v == 0) {
                continue;
            }
            let Some(t) = now() else {
                return forgotten;
            };
            for (&v, slot) in seen.iter().zip(chunk) {
                if v != 0
                    && tag_distance(v, t) > window
                    && slot.compare_exchange(v, 0, Ordering::Relaxed, Ordering::Relaxed).is_ok()
                {
                    forgotten = forgotten.wrapping_add(1);
                }
            }
        }
        forgotten
    }

    /// Faults in the ring's memory with one `fetch_add(0)` per `page_bytes`
    /// (and on the last slot), so that the first real-time access doesn't
    /// take a page fault. The values are unchanged.
    pub fn touch_pages(&self, page_bytes: usize) {
        let stride = page_bytes.checked_div(size_of::<AtomicU64>()).unwrap_or(0).max(1);
        for slot in self.slots.iter().step_by(stride) {
            slot.fetch_add(0, Ordering::Relaxed);
        }
        if let Some(last) = self.slots.last() {
            last.fetch_add(0, Ordering::Relaxed);
        }
    }
}

impl fmt::Debug for RingRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RingRef").field("capacity", &self.capacity()).finish()
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects, clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use core::cell::Cell;
    use core::sync::atomic::AtomicBool;
    use std::vec::Vec;

    fn slots(n: usize) -> Vec<AtomicU64> {
        (0..n).map(|_| AtomicU64::new(0)).collect()
    }

    #[test]
    fn write_then_read() {
        let s = slots(1024);
        let ring = RingRef::new(&s).unwrap();
        assert_eq!(ring.capacity(), 1024);
        let ts = 1_700_000_000u64 * 48_000;
        ring.write(ts, &[1, 2, 3, 4]);
        let mut out = [9; 6];
        assert_eq!(ring.read(ts - 1, &mut out), 4);
        assert_eq!(out, [0, 1, 2, 3, 4, 0]);
    }

    #[test]
    fn stale_samples_read_as_silence() {
        let s = slots(16);
        let ring = RingRef::new(&s).unwrap();
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
        let s = slots(8);
        let ring = RingRef::new(&s).unwrap();
        ring.write(3, &[i32::MIN, -1, i32::MAX]);
        let mut out = [0; 3];
        ring.read(3, &mut out);
        assert_eq!(out, [i32::MIN, -1, i32::MAX]);
        ring.clear();
        assert_eq!(ring.read_one(4), None);
    }

    #[test]
    fn concurrent_writer_and_reader() {
        let s = slots(256);
        let ring = RingRef::new(&s).unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for ts in 0..200_000u64 {
                    ring.write_one(ts, (ts as i32).wrapping_mul(3));
                }
            });
            for ts in (0..200_000u64).step_by(7) {
                if let Some(s) = ring.read_one(ts) {
                    assert_eq!(s, (ts as i32).wrapping_mul(3));
                }
            }
        });
    }

    #[test]
    fn slot_format() {
        assert_eq!(pack(0x1_2345_6789, -2), 0x2345_6789_FFFF_FFFE);
        assert_eq!(unpack(0x2345_6789_FFFF_FFFE, 0x1_2345_6789), Some(-2));
        assert_eq!(unpack(0x2345_6789_FFFF_FFFE, 0x2345_6789), Some(-2));
        assert_eq!(unpack(0x2345_6789_FFFF_FFFE, 0x2345_678A), None);
        let s = slots(4);
        let ring = RingRef::new(&s).unwrap();
        ring.write_one(0x1_0000_0006, 42);
        assert_eq!(s[2].load(Ordering::Relaxed), pack(6, 42));
    }

    /// A media clock for sweeps that reads `now`.
    fn clock(now: &Cell<u64>) -> impl FnMut() -> Option<u64> + '_ {
        || Some(now.get())
    }

    #[test]
    fn sweep_window_is_two_ring_lengths() {
        assert_eq!(RingRef::new(&slots(16)).unwrap().sweep_window(), Some(32));
        assert_eq!(RingRef::new(&slots(32768)).unwrap().sweep_window(), Some(65536));
    }

    #[test]
    fn a_sample_nobody_overwrites_matches_again_2_pow_32_later() {
        // What sweeping is for.
        let s = slots(32768);
        let ring = RingRef::new(&s).unwrap();
        let ts = 1_700_000_000u64 * 48_000;
        ring.write(ts, &[0x7fff_ff00; 4]);
        assert_eq!(ring.read_one(ts + 32768), None);
        assert_eq!(ring.read_one(ts + (1 << 32)), Some(0x7fff_ff00));
        assert_eq!(ring.read_one(ts + 3 + (1 << 33)), Some(0x7fff_ff00));
    }

    #[test]
    fn swept_samples_read_as_silence_2_pow_32_later() {
        let s = slots(32768);
        let ring = RingRef::new(&s).unwrap();
        let ts = 1_700_000_000u64 * 48_000;
        // A writer that stops after a full lap of loud audio.
        let lap: Vec<i32> = (0..32768).map(|i| 0x7fff_ff00 - i * 256).collect();
        ring.write(ts - 32768, &lap);
        let now = Cell::new(ts);
        assert_eq!(ring.sweep(clock(&now)), 0, "a fresh lap is kept");
        // Media time runs on by 2^32 samples (24.9 h at 48 kHz, 6.2 h at
        // 192 kHz) and a bit, with a sweep every 2^28 samples or so.
        let mut forgotten = 0;
        while now.get() < ts + (1 << 32) + 1000 {
            now.set(now.get() + (1 << 28) + 12_345);
            forgotten += ring.sweep(clock(&now));
        }
        assert_eq!(forgotten, 32768);
        // Where the tags would match again, the ring is silent.
        let mut out = [1; 32768];
        assert_eq!(ring.read(ts - 32768 + (1 << 32), &mut out), 0);
        assert!(out.iter().all(|&x| x == 0));
        assert_eq!(ring.read_one(ts - 1 + (1 << 33)), None);
        assert!(s.iter().all(|slot| slot.load(Ordering::Relaxed) == 0));
    }

    #[test]
    fn sweep_keeps_samples_near_now_and_forgets_the_rest() {
        let s = slots(16);
        let ring = RingRef::new(&s).unwrap();
        // The low 32 bits of the timestamps wrap around `now`.
        let now = Cell::new((1u64 << 40) + 10);
        let t = now.get();
        // Within the window (32 samples) on either side: kept.
        for ts in [t - 32, t - 11, t - 1, t, t + 17, t + 32] {
            ring.clear();
            ring.write_one(ts, 5);
            assert_eq!(ring.sweep(clock(&now)), 0, "{ts:#x}");
            assert_eq!(ring.read_one(ts), Some(5), "{ts:#x}");
        }
        // Just outside it, or up to half a tag range away: forgotten.
        for ts in [t - 33, t + 33, t - 40_000, t + (1 << 31), t - (1 << 31) + 1] {
            ring.write_one(ts, 5);
            assert_eq!(ring.sweep(clock(&now)), 1, "{ts:#x}");
            assert_eq!(ring.read_one(ts), None, "{ts:#x}");
            assert_eq!(ring.read_one(ts + (1 << 32)), None, "{ts:#x}");
        }
        // Old silence goes too; then only never-written slots are left.
        ring.write_one(t - 1000, 0);
        assert_eq!(ring.sweep(clock(&now)), 1);
        assert!(s.iter().all(|slot| slot.load(Ordering::Relaxed) == 0));
    }

    #[test]
    fn sweep_without_a_clock_forgets_nothing() {
        let s = slots(256);
        let ring = RingRef::new(&s).unwrap();
        ring.write(10, &[3; 256]);
        assert_eq!(ring.sweep(|| None), 0);
        assert_eq!(ring.read_one(10), Some(3));
        // A clock that goes away mid-sweep stops it.
        let mut reads = 0;
        let forgotten = ring.sweep(|| {
            reads += 1;
            (reads == 1).then_some(1 << 20)
        });
        assert_eq!((forgotten, reads), (SWEEP_CHUNK, 2));
        assert_eq!(ring.read_one(10 + SWEEP_CHUNK as u64), Some(3));
    }

    #[test]
    fn a_preempted_sweep_judges_each_chunk_by_a_new_clock_reading() {
        // Media time moves on by far more than the window between the two
        // chunks, as it would if the sweep were preempted, and the writer
        // fills the second chunk for the new time: it is kept.
        let s = slots(128);
        let ring = RingRef::new(&s).unwrap();
        let t = 1u64 << 40;
        let later = t + 1_000_000;
        ring.write(t, &[9; 64]);
        ring.write(later, &[8; 64]);
        assert_eq!(later & 127, 64);
        let mut reads = 0;
        let forgotten = ring.sweep(|| {
            reads += 1;
            Some(if reads == 1 { t } else { later })
        });
        assert_eq!((forgotten, reads), (0, 2));
        assert_eq!(ring.read_one(t), Some(9));
        assert_eq!(ring.read_one(later + 63), Some(8));
    }

    #[test]
    fn sweep_keeps_a_sample_written_after_it_loaded_the_slot() {
        // The slots hold a stale lap. While the sweep reads the clock (after
        // loading them), a writer fills them for the current time: the
        // compare-exchange fails and the fresh samples stay.
        let s = slots(64);
        let ring = RingRef::new(&s).unwrap();
        let t = 1u64 << 40;
        ring.write(t - 1_000_000, &[1; 64]);
        let forgotten = ring.sweep(|| {
            ring.write(t, &[2; 64]);
            Some(t)
        });
        assert_eq!(forgotten, 0);
        let mut out = [0; 64];
        assert_eq!(ring.read(t, &mut out), 64);
        assert_eq!(out, [2; 64]);
    }

    #[test]
    fn sweep_never_clobbers_a_concurrent_writer() {
        // A writer keeps writing at the clock's time while sweeps run on the
        // same clock: no fresh sample is ever forgotten.
        let s = slots(1024);
        let ring = RingRef::new(&s).unwrap();
        let time = AtomicU64::new(1 << 33);
        let done = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..300 {
                    ring.sweep(|| Some(time.load(Ordering::Relaxed)));
                }
                done.store(true, Ordering::Relaxed);
            });
            while !done.load(Ordering::Relaxed) {
                let t = time.load(Ordering::Relaxed);
                ring.write(t, &[7; 64]);
                for i in 0..64 {
                    assert_eq!(ring.read_one(t + i), Some(7), "{t}+{i}");
                }
                time.store(t + 64, Ordering::Relaxed);
            }
        });
    }

    #[test]
    fn new_requires_power_of_two() {
        for n in [0, 1, 3, 6, 1000] {
            assert!(RingRef::new(&slots(n)).is_none(), "{n}");
        }
        for n in [2, 4, 32768] {
            assert_eq!(RingRef::new(&slots(n)).unwrap().capacity(), n);
        }
    }

    #[test]
    fn touch_pages_keeps_values() {
        let s = slots(4096);
        let ring = RingRef::new(&s).unwrap();
        ring.write(10, &[1, -2, 3]);
        ring.write_one(4095, 9);
        for page in [0, 1, 7, 8, 4096, 16384, usize::MAX] {
            ring.touch_pages(page);
        }
        assert_eq!(ring.read_one(11), Some(-2));
        assert_eq!(ring.read_one(4095), Some(9));
        assert_eq!(ring.read_one(0), Some(0));
    }
}
