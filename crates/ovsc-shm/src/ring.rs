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
