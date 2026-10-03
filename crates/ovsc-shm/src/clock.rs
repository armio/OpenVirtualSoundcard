//! The clock block: the daemon's media clock, published through a seqlock.
//!
//! One writer (the daemon's clock mirror, or `ovsc-clock`'s
//! `ClockWriter` in-process) publishes [`ClockRecord`]s; any number of
//! readers, in any process that maps the block, read them without locks.
//!
//! Readers in another process must not trust the writer to finish: a daemon
//! can die in the middle of a write and leave the sequence odd forever. So
//! [`ClockBlock::read_bounded`] gives up after a fixed number of tries, and
//! [`ClockBlock::publish`] repairs an odd sequence on the next write.

#![forbid(unsafe_code)]

#[cfg(target_has_atomic = "64")]
use core::sync::atomic::{AtomicU64, Ordering, fence};

use crate::time::{ClockSnapshot, ClockState};

/// How many times a real-time reader tries before giving up.
pub const READ_TRIES: u32 = 4;

/// Bit 8 of the state word: the record carries a valid snapshot. Bits 0-7
/// hold the [`ClockState`] code.
pub const STATE_WORD_VALID: u64 = 1 << 8;

/// Everything one publish carries.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClockRecord {
    /// The local-to-media mapping. Meaningless unless `valid`.
    pub snapshot: ClockSnapshot,
    /// Whether the clock has a time source.
    pub valid: bool,
    pub state: ClockState,
    /// Incremented on every discontinuity of the media clock.
    pub step_gen: u64,
    /// `uuid48 << 16 | port_id` of the PTP grandmaster, 0 if none.
    pub grandmaster: u64,
    /// Host time of this publish, nanoseconds.
    pub publish_ns: u64,
}

impl ClockRecord {
    /// The state word for this record: state code plus the valid bit.
    pub const fn state_word(&self) -> u64 {
        let valid = if self.valid { STATE_WORD_VALID } else { 0 };
        self.state.to_u8() as u64 | valid
    }
}

/// The outcome of a bounded read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ClockRead {
    /// A consistent record.
    Record(ClockRecord),
    /// Nothing was ever published.
    NeverWritten,
    /// Every try overlapped a write (or a writer died mid-write).
    Contended,
}

/// The clock block of the shared region (128 bytes, one cache line on Apple
/// silicon).
///
/// The fields are public so that diagnostics and tests can inspect them;
/// writers must go through [`ClockBlock::publish`] and readers through the
/// read methods, which implement the seqlock.
#[cfg(target_has_atomic = "64")]
#[repr(C, align(128))]
#[derive(Debug)]
pub struct ClockBlock {
    /// Seqlock sequence: odd while a write is in progress, 0 = never written.
    pub seq: AtomicU64,
    /// `ClockSnapshot::local_ref_ns`: host nanoseconds.
    pub host_ref_ns: AtomicU64,
    /// `ClockSnapshot::media_ref_ns`: PTP media time, nanoseconds.
    pub media_ref_ns: AtomicU64,
    /// `ClockSnapshot::rate` as f64 bits: media ns per host ns.
    pub rate_bits: AtomicU64,
    /// Bits 0-7: [`ClockState`]; bit 8: [`STATE_WORD_VALID`].
    pub state_word: AtomicU64,
    pub step_gen: AtomicU64,
    pub grandmaster: AtomicU64,
    pub publish_ns: AtomicU64,
    pub(crate) _reserved: [AtomicU64; 8],
}

#[cfg(target_has_atomic = "64")]
impl ClockBlock {
    /// A block that was never written.
    pub const fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            host_ref_ns: AtomicU64::new(0),
            media_ref_ns: AtomicU64::new(0),
            rate_bits: AtomicU64::new(0),
            state_word: AtomicU64::new(0),
            step_gen: AtomicU64::new(0),
            grandmaster: AtomicU64::new(0),
            publish_ns: AtomicU64::new(0),
            _reserved: [const { AtomicU64::new(0) }; 8],
        }
    }

    /// Publishes a record. Readers see either the old or the new record,
    /// never a mix of both.
    ///
    /// There must be a single writer. If a previous writer died mid-write
    /// and left the sequence odd, this write starts from the next even value,
    /// so readers recover as soon as it completes.
    pub fn publish(&self, r: &ClockRecord) {
        let s = self.seq.load(Ordering::Relaxed);
        let mut base = s.wrapping_add(1) & !1;
        if base.wrapping_add(2) == 0 {
            // Never wrap back to 0, which means "never written".
            base = 0;
        }
        self.seq.store(base.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        self.host_ref_ns.store(r.snapshot.local_ref_ns, Ordering::Relaxed);
        self.media_ref_ns.store(r.snapshot.media_ref_ns, Ordering::Relaxed);
        self.rate_bits.store(r.snapshot.rate.to_bits(), Ordering::Relaxed);
        self.state_word.store(r.state_word(), Ordering::Relaxed);
        self.step_gen.store(r.step_gen, Ordering::Relaxed);
        self.grandmaster.store(r.grandmaster, Ordering::Relaxed);
        self.publish_ns.store(r.publish_ns, Ordering::Relaxed);
        self.seq.store(base.wrapping_add(2), Ordering::Release);
    }

    /// Reads the current record, trying at most `tries` times. Never blocks,
    /// so it is safe on real-time threads and against a writer that died.
    pub fn read_bounded(&self, tries: u32) -> ClockRead {
        for _ in 0..tries {
            match self.try_read() {
                Attempt::Done(r) => return r,
                Attempt::Busy => core::hint::spin_loop(),
                Attempt::Torn => {}
            }
        }
        ClockRead::Contended
    }

    /// Reads the current record, spinning until no write overlaps the read;
    /// `None` if nothing was ever published.
    ///
    /// Only for readers in the writer's own process, where the writer cannot
    /// die mid-write.
    pub fn read_spin(&self) -> Option<ClockRecord> {
        loop {
            match self.try_read() {
                Attempt::Done(ClockRead::Record(r)) => return Some(r),
                Attempt::Done(_) => return None,
                Attempt::Busy => core::hint::spin_loop(),
                Attempt::Torn => {}
            }
        }
    }

    #[inline]
    fn try_read(&self) -> Attempt {
        let s1 = self.seq.load(Ordering::Acquire);
        if s1 == 0 {
            return Attempt::Done(ClockRead::NeverWritten);
        }
        if s1 & 1 == 1 {
            return Attempt::Busy;
        }
        let local_ref_ns = self.host_ref_ns.load(Ordering::Relaxed);
        let media_ref_ns = self.media_ref_ns.load(Ordering::Relaxed);
        let rate = f64::from_bits(self.rate_bits.load(Ordering::Relaxed));
        let state_word = self.state_word.load(Ordering::Relaxed);
        let step_gen = self.step_gen.load(Ordering::Relaxed);
        let grandmaster = self.grandmaster.load(Ordering::Relaxed);
        let publish_ns = self.publish_ns.load(Ordering::Relaxed);
        fence(Ordering::Acquire);
        if self.seq.load(Ordering::Relaxed) != s1 {
            return Attempt::Torn;
        }
        Attempt::Done(ClockRead::Record(ClockRecord {
            snapshot: ClockSnapshot { local_ref_ns, media_ref_ns, rate },
            valid: state_word & STATE_WORD_VALID != 0,
            // An unknown code (a newer or broken writer) is treated as
            // having no time source.
            state: ClockState::from_u8(state_word as u8).unwrap_or(ClockState::Unlocked),
            step_gen,
            grandmaster,
            publish_ns,
        }))
    }
}

#[cfg(target_has_atomic = "64")]
impl Default for ClockBlock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_has_atomic = "64")]
enum Attempt {
    /// A final answer: a record, or never written.
    Done(ClockRead),
    /// A write is in progress.
    Busy,
    /// A write completed during the read.
    Torn,
}
