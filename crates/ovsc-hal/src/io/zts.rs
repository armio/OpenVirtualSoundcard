//! Lock-free cells the zero time stamp path shares with the other real-time
//! paths (design section 13):
//!
//! * [`TryLock`]: the device timeline belongs to whichever GetZeroTimeStamp
//!   caller takes it; the others never wait;
//! * [`ZtsCache`]: the last zero time stamp handed out, which those other
//!   callers, and a caller that panicked, return instead;
//! * [`ModelCell`]: a copy of the device model, from which DoIOOperation
//!   extrapolates the device time for its margins.
//!
//! None of them allocates or blocks; readers give up after a bounded number
//! of tries.

use std::cell::UnsafeCell;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering, fence};

use ovsc_shm::timeline::DeviceModel;

/// A zero time stamp as the HAL receives it: (sample time, host ticks,
/// seed).
pub type ZtsTriple = (f64, u64, u64);

/// How many times a real-time reader tries a cell before giving up.
const READ_TRIES: u32 = 8;

/// A lock that is only ever tried: real-time callers that find it taken do
/// something else instead of waiting.
pub(crate) struct TryLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: the value is only reached through a guard, and at most one guard
// exists at a time (`locked`), so sharing the lock shares no `&mut T`.
unsafe impl<T: Send> Sync for TryLock<T> {}

impl<T> TryLock<T> {
    pub(crate) const fn new(value: T) -> Self {
        Self { locked: AtomicBool::new(false), value: UnsafeCell::new(value) }
    }

    /// The value, if nobody else holds it. Never waits.
    pub(crate) fn try_lock(&self) -> Option<TryLockGuard<'_, T>> {
        self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| TryLockGuard { lock: self })
    }

    /// The value, yielding the thread until it is free. Not for real-time
    /// threads: StartIO and configuration changes use it, and the holders
    /// they wait for only run for one zero time stamp.
    pub(crate) fn lock_spin(&self) -> TryLockGuard<'_, T> {
        loop {
            if let Some(g) = self.try_lock() {
                return g;
            }
            std::thread::yield_now();
        }
    }
}

/// Exclusive access to a [`TryLock`]'s value; dropping it (also while
/// unwinding from a panic) frees the lock.
pub(crate) struct TryLockGuard<'a, T> {
    lock: &'a TryLock<T>,
}

impl<T> Deref for TryLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: this guard is the only one (see `TryLock`), so no `&mut`
        // to the value exists elsewhere.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for TryLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as for `deref`; `&mut self` makes this borrow unique.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for TryLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

/// One slot of the [`ZtsCache`].
struct Slot {
    /// The cache position whose triple the slot holds, or [`WRITING`].
    tag: AtomicU64,
    sample_bits: AtomicU64,
    host_ticks: AtomicU64,
    seed: AtomicU64,
}

impl Slot {
    const fn new(tag: u64) -> Self {
        Self {
            tag: AtomicU64::new(tag),
            sample_bits: AtomicU64::new(0),
            host_ticks: AtomicU64::new(0),
            seed: AtomicU64::new(1),
        }
    }
}

/// Slots in the cache; a power of two.
const SLOTS: usize = 8;
const SLOT_MASK: usize = 7;
const _: () = assert!(SLOTS.is_power_of_two() && SLOT_MASK == SLOTS - 1);
/// The tag of a slot being written.
const WRITING: u64 = u64::MAX;

/// The last zero time stamp handed out (design section 13).
///
/// A seqlock over a ring of slots: the writer fills the slot after the
/// current one, then moves the position to it. A reader of the current slot
/// can only be disturbed by a writer that has gone all the way round the
/// ring, that is, by `SLOTS - 1` new time stamps during one read; time
/// stamps change at most once per call and normally once every 16384
/// frames. So a writer that is preempted mid-write never holds readers up,
/// and every read sees a whole triple.
///
/// There must be one writer at a time: the engine writes only while it holds
/// the timeline lock.
pub(crate) struct ZtsCache {
    /// The position of the newest slot (its tag), counted since creation.
    pos: AtomicU64,
    slots: [Slot; SLOTS],
}

impl ZtsCache {
    /// Sample time 0 at host time 0, seed 1.
    pub(crate) const fn new() -> Self {
        Self {
            pos: AtomicU64::new(0),
            slots: [
                Slot::new(0),
                Slot::new(WRITING),
                Slot::new(WRITING),
                Slot::new(WRITING),
                Slot::new(WRITING),
                Slot::new(WRITING),
                Slot::new(WRITING),
                Slot::new(WRITING),
            ],
        }
    }

    fn slot(&self, pos: u64) -> Option<&Slot> {
        self.slots.get(pos as usize & SLOT_MASK)
    }

    /// Makes `zts` the newest triple. Single writer.
    pub(crate) fn store(&self, (sample, host_ticks, seed): ZtsTriple) {
        let pos = self.pos.load(Ordering::Relaxed).wrapping_add(1);
        let Some(slot) = self.slot(pos) else {
            return;
        };
        slot.tag.store(WRITING, Ordering::Relaxed);
        fence(Ordering::Release);
        slot.sample_bits.store(sample.to_bits(), Ordering::Relaxed);
        slot.host_ticks.store(host_ticks, Ordering::Relaxed);
        slot.seed.store(seed, Ordering::Relaxed);
        slot.tag.store(pos, Ordering::Release);
        self.pos.store(pos, Ordering::Release);
    }

    /// The newest triple. Real-time safe: a bounded number of tries.
    pub(crate) fn load(&self) -> ZtsTriple {
        let mut read = (0.0, 0, 1);
        for _ in 0..READ_TRIES {
            match self.try_load() {
                Ok(zts) => return zts,
                Err(zts) => read = zts,
            }
            std::hint::spin_loop();
        }
        // Only reached if the writer wrapped the ring during every try.
        read
    }

    /// One read: `Ok` if no write overlapped it, else `Err` with what was
    /// read.
    fn try_load(&self) -> Result<ZtsTriple, ZtsTriple> {
        let pos = self.pos.load(Ordering::Acquire);
        let Some(slot) = self.slot(pos) else {
            return Err((0.0, 0, 1));
        };
        let tag = slot.tag.load(Ordering::Acquire);
        let read = (
            f64::from_bits(slot.sample_bits.load(Ordering::Relaxed)),
            slot.host_ticks.load(Ordering::Relaxed),
            slot.seed.load(Ordering::Relaxed),
        );
        fence(Ordering::Acquire);
        if tag == pos && slot.tag.load(Ordering::Relaxed) == pos { Ok(read) } else { Err(read) }
    }
}

/// A seqlock copy of the device model (design section 8.3), published by
/// whoever holds the timeline and read by the IO thread for its margins.
/// Single writer: the timeline lock holder.
pub(crate) struct ModelCell {
    /// Odd while a write is in progress; 0 until the first write.
    seq: AtomicU64,
    h_a: AtomicU64,
    t_whole: AtomicU64,
    t_frac: AtomicU64,
    rho: AtomicU64,
}

impl ModelCell {
    pub(crate) const fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            h_a: AtomicU64::new(0),
            t_whole: AtomicU64::new(0),
            t_frac: AtomicU64::new(0),
            rho: AtomicU64::new(0),
        }
    }

    pub(crate) fn store(&self, m: &DeviceModel) {
        let s = self.seq.load(Ordering::Relaxed);
        // An even base, so readers never mistake a write for a stable value.
        let base = s.wrapping_add(1) & !1;
        self.seq.store(base.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        self.h_a.store(m.h_a, Ordering::Relaxed);
        self.t_whole.store(m.t_whole as u64, Ordering::Relaxed);
        self.t_frac.store(m.t_frac.to_bits(), Ordering::Relaxed);
        self.rho.store(m.rho.to_bits(), Ordering::Relaxed);
        self.seq.store(base.wrapping_add(2), Ordering::Release);
    }

    /// The model, or `None` if it was never written or every try overlapped
    /// a write.
    pub(crate) fn load(&self) -> Option<DeviceModel> {
        for _ in 0..READ_TRIES {
            let s = self.seq.load(Ordering::Acquire);
            if s == 0 {
                return None;
            }
            if s & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let m = DeviceModel {
                h_a: self.h_a.load(Ordering::Relaxed),
                t_whole: self.t_whole.load(Ordering::Relaxed) as i64,
                t_frac: f64::from_bits(self.t_frac.load(Ordering::Relaxed)),
                rho: f64::from_bits(self.rho.load(Ordering::Relaxed)),
            };
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == s {
                return Some(m);
            }
        }
        None
    }
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::panic
)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    #[test]
    fn try_lock_is_exclusive_and_freed_on_drop() {
        let lock = TryLock::new(5);
        let mut g = lock.try_lock().unwrap();
        assert!(lock.try_lock().is_none());
        *g += 1;
        drop(g);
        assert_eq!(*lock.lock_spin(), 6);
        // A panic while holding the lock frees it too.
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = lock.try_lock().unwrap();
            panic!("held");
        }));
        assert!(r.is_err());
        assert!(lock.try_lock().is_some());
    }

    #[test]
    fn zts_cache_starts_at_seed_1() {
        let cache = ZtsCache::new();
        assert_eq!(cache.load(), (0.0, 0, 1));
        cache.store((16384.0, 99, 1));
        assert_eq!(cache.load(), (16384.0, 99, 1));
        // Round the ring several times.
        for k in 2..30u64 {
            cache.store((k as f64, k, k));
            assert_eq!(cache.load(), (k as f64, k, k));
        }
    }

    #[test]
    fn zts_cache_never_mixes_time_stamps() {
        // Every stored triple has host == 3 * sample and seed == sample, so a
        // mixed read breaks the relation. The writer stores as fast as it
        // can, far faster than the engine ever does.
        fn whole((sample, host, seed): ZtsTriple) -> bool {
            host == 3 * sample as u64 && seed == sample as u64
        }
        let cache = ZtsCache::new();
        cache.store((1.0, 3, 1));
        let writing = AtomicU32::new(1);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for n in 2..300_000u64 {
                    cache.store((n as f64, 3 * n, n));
                }
                writing.store(0, Ordering::Release);
            });
            for _ in 0..3 {
                scope.spawn(|| {
                    let mut last = 0.0;
                    while writing.load(Ordering::Acquire) > 0 {
                        if let Ok(zts) = cache.try_load() {
                            assert!(whole(zts), "mixed read {zts:?}");
                            assert!(zts.0 >= last, "went back from {last} to {}", zts.0);
                            last = zts.0;
                        }
                    }
                });
            }
        });
        assert_eq!(cache.load(), (299_999.0, 899_997, 299_999));
    }

    #[test]
    fn model_cell_round_trips() {
        let cell = ModelCell::new();
        assert_eq!(cell.load(), None);
        let m = DeviceModel { h_a: 7, t_whole: -3, t_frac: 0.25, rho: 4.8e-5 };
        cell.store(&m);
        assert_eq!(cell.load(), Some(m));
        let n = DeviceModel { h_a: 9, t_whole: 1 << 40, t_frac: 0.5, rho: 9.6e-5 };
        cell.store(&n);
        assert_eq!(cell.load(), Some(n));
    }

    #[test]
    fn model_cell_reads_are_whole() {
        let cell = ModelCell::new();
        let done = AtomicU32::new(0);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for i in 1..200_000u64 {
                    let m = DeviceModel {
                        h_a: i,
                        t_whole: i as i64,
                        t_frac: 1.0 / i as f64,
                        rho: i as f64,
                    };
                    cell.store(&m);
                }
                done.store(1, Ordering::Release);
            });
            while done.load(Ordering::Acquire) == 0 {
                if let Some(m) = cell.load() {
                    assert_eq!(m.t_whole as u64, m.h_a);
                    assert_eq!(m.rho, m.h_a as f64);
                    assert_eq!(m.t_frac, 1.0 / m.h_a as f64);
                }
            }
        });
    }
}
