//! The clock block's seqlock: readers never see a torn record, and bounded
//! readers never hang on a dead writer.

use std::sync::atomic::{AtomicBool, Ordering};

use ovsc_shm::clock::{ClockBlock, ClockRead, ClockRecord, READ_TRIES, STATE_WORD_VALID};
use ovsc_shm::time::{ClockSnapshot, ClockState};

/// A record whose every field is derived from `i`, so a mix of two records
/// is detectable.
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

const PUBLISHES: u64 = 200_000;

/// What one reader saw.
#[derive(Default)]
struct Seen {
    records: u64,
    contended: u64,
    last: u64,
}

impl Seen {
    fn record(&mut self, r: ClockRecord) {
        let i = r.snapshot.local_ref_ns;
        assert_eq!(r, record(i), "torn record");
        assert!(i >= self.last, "went back from {} to {i}", self.last);
        self.last = i;
        self.records += 1;
    }
}

#[test]
fn three_readers_never_see_a_torn_record() {
    let block = Box::new(ClockBlock::new());
    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        let readers: Vec<_> = (0..3)
            .map(|n| {
                let (block, done) = (&block, &done);
                s.spawn(move || {
                    let mut seen = Seen::default();
                    while !done.load(Ordering::Acquire) {
                        // Reader 0 spins, the others are bounded like the
                        // driver's real-time thread.
                        if n == 0 {
                            if let Some(r) = block.read_spin() {
                                seen.record(r);
                            }
                        } else {
                            match block.read_bounded(READ_TRIES) {
                                ClockRead::Record(r) => seen.record(r),
                                ClockRead::NeverWritten => assert_eq!(seen.records, 0),
                                ClockRead::Contended => seen.contended += 1,
                            }
                        }
                    }
                    // Once the writer has finished, every reader sees the last
                    // record.
                    assert_eq!(
                        block.read_bounded(READ_TRIES),
                        ClockRead::Record(record(PUBLISHES))
                    );
                    (seen.records, seen.contended)
                })
            })
            .collect();
        for i in 1..=PUBLISHES {
            block.publish(&record(i));
        }
        done.store(true, Ordering::Release);
        for r in readers {
            let (records, contended) = r.join().unwrap();
            println!("{records} records, {contended} contended reads");
        }
    });
    assert_eq!(block.seq.load(Ordering::Relaxed), 2 * PUBLISHES);
}

#[test]
fn never_written() {
    let block = ClockBlock::new();
    assert_eq!(block.read_bounded(READ_TRIES), ClockRead::NeverWritten);
    assert_eq!(block.read_spin(), None);
    assert_eq!(ClockBlock::default().read_bounded(1), ClockRead::NeverWritten);
}

#[test]
fn stuck_odd_sequence_is_contended_until_the_next_publish() {
    let block = ClockBlock::new();
    block.publish(&record(1));
    // A writer that died mid-write leaves the sequence odd.
    block.seq.store(3, Ordering::Relaxed);
    assert_eq!(block.read_bounded(READ_TRIES), ClockRead::Contended);
    assert_eq!(block.read_bounded(1), ClockRead::Contended);
    assert_eq!(block.read_bounded(0), ClockRead::Contended);
    // The next writer starts from the next even value.
    block.publish(&record(2));
    assert_eq!(block.seq.load(Ordering::Relaxed), 6);
    assert_eq!(block.read_bounded(READ_TRIES), ClockRead::Record(record(2)));
    assert_eq!(block.read_spin(), Some(record(2)));
    // Even a stuck first write (sequence 1) is repaired.
    let fresh = ClockBlock::new();
    fresh.seq.store(1, Ordering::Relaxed);
    assert_eq!(fresh.read_bounded(READ_TRIES), ClockRead::Contended);
    fresh.publish(&record(9));
    assert_eq!(fresh.seq.load(Ordering::Relaxed), 4);
    assert_eq!(fresh.read_bounded(READ_TRIES), ClockRead::Record(record(9)));
}

#[test]
fn sequence_never_wraps_to_never_written() {
    let block = ClockBlock::new();
    for start in [u64::MAX - 1, u64::MAX, u64::MAX - 2] {
        block.seq.store(start, Ordering::Relaxed);
        block.publish(&record(5));
        assert_eq!(block.seq.load(Ordering::Relaxed), 2, "from {start:#x}");
        assert_eq!(block.read_bounded(READ_TRIES), ClockRead::Record(record(5)));
    }
}

#[test]
fn fields_and_state_word() {
    let block = ClockBlock::new();
    let r = ClockRecord {
        snapshot: ClockSnapshot { local_ref_ns: 11, media_ref_ns: 22, rate: 1.000_05 },
        valid: true,
        state: ClockState::FreeRunning,
        step_gen: 33,
        grandmaster: 0x0011_2233_4455_0001,
        publish_ns: 44,
    };
    block.publish(&r);
    assert_eq!(block.host_ref_ns.load(Ordering::Relaxed), 11);
    assert_eq!(block.media_ref_ns.load(Ordering::Relaxed), 22);
    assert_eq!(block.rate_bits.load(Ordering::Relaxed), 1.000_05f64.to_bits());
    assert_eq!(block.state_word.load(Ordering::Relaxed), 3 | STATE_WORD_VALID);
    assert_eq!(block.step_gen.load(Ordering::Relaxed), 33);
    assert_eq!(block.grandmaster.load(Ordering::Relaxed), 0x0011_2233_4455_0001);
    assert_eq!(block.publish_ns.load(Ordering::Relaxed), 44);
    assert_eq!(block.read_bounded(READ_TRIES), ClockRead::Record(r));

    let invalid = ClockRecord { valid: false, state: ClockState::Locking, ..r };
    block.publish(&invalid);
    assert_eq!(block.state_word.load(Ordering::Relaxed), 1);
    assert_eq!(block.read_spin(), Some(invalid));

    // An unknown state code reads as Unlocked; the valid bit is kept.
    block.state_word.store(0x7F | STATE_WORD_VALID, Ordering::Relaxed);
    let ClockRead::Record(got) = block.read_bounded(READ_TRIES) else { panic!() };
    assert_eq!((got.state, got.valid), (ClockState::Unlocked, true));
}
