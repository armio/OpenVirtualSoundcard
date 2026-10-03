//! Copies the daemon's media clock into the region's clock block, where the
//! driver reads it (design section 7.2).
//!
//! [`ShmClockMirror`] is an [`ovsc_clock::ClockMirror`]: the clock's
//! writer calls it synchronously after each change, so the block never lags
//! the in-process clock. On top of the mapping it maintains `step_gen`, which
//! counts discontinuities, so the driver knows when to realign rather than
//! slew:
//!
//! * the media time jumps by more than 1 us against the previous mapping;
//! * the clock becomes valid;
//! * the grandmaster changes;
//! * the state changes into Locked or FreeRunning.

use std::sync::{Arc, Mutex, PoisonError};

use ovsc_clock::{ClockMirror, ClockSnapshot, ClockState, ClockStatus, MasterInfo};
use ovsc_shm::clock::{ClockBlock, ClockRecord};

use crate::region::HalRegion;

/// A media-time jump larger than this between consecutive mappings is a
/// step.
pub const STEP_THRESHOLD_NS: u64 = 1_000;

/// Mirrors a media clock into a region (see the module documentation).
///
/// Every mirror of one region writes through the region's own writer state,
/// so mirrors made for successive clocks (an engine restarted with a new PTP
/// follower) keep the block single-writer and `step_gen` increasing.
pub struct ShmClockMirror {
    region: Arc<HalRegion>,
}

impl ShmClockMirror {
    pub fn new(region: Arc<HalRegion>) -> Arc<Self> {
        Arc::new(Self { region })
    }
}

impl ClockMirror for ShmClockMirror {
    fn publish(&self, snapshot: Option<ClockSnapshot>, status: &ClockStatus) {
        self.region.mirror_state().publish(self.region.view().clock(), snapshot, status);
    }
}

/// The clock block's writer state: what was published last, and the step
/// count.
#[derive(Debug, Default)]
pub(crate) struct MirrorWriter {
    state: Mutex<Previous>,
}

#[derive(Debug, Default)]
struct Previous {
    /// The last valid mapping.
    snapshot: Option<ClockSnapshot>,
    valid: bool,
    state: ClockState,
    grandmaster: u64,
    /// The last master the clock had, kept while it has none.
    last_master: u64,
    step_gen: u64,
}

impl MirrorWriter {
    /// Publishes the clock's state into `block`, bumping `step_gen` on a
    /// discontinuity. The lock is held across the write, which keeps the
    /// block's seqlock single-writer.
    pub(crate) fn publish(
        &self,
        block: &ClockBlock,
        snapshot: Option<ClockSnapshot>,
        status: &ClockStatus,
    ) {
        let mut prev = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let grandmaster = grandmaster_id(status.master.as_ref());
        let valid = snapshot.is_some();
        let jumped = match (snapshot, prev.snapshot) {
            (Some(new), Some(old)) if prev.valid => {
                new.media_ref_ns.abs_diff(old.media_ns_at(new.local_ref_ns)) > STEP_THRESHOLD_NS
            }
            _ => false,
        };
        let became_valid = valid && !prev.valid;
        // Losing the master is no discontinuity: the clock holds over on its
        // own mapping, and the plug-in keeps playing on it. A master other
        // than the last one is.
        let new_master = grandmaster != 0 && grandmaster != prev.last_master;
        let locked = matches!(status.state, ClockState::Locked | ClockState::FreeRunning)
            && status.state != prev.state;
        if jumped || became_valid || new_master || locked {
            prev.step_gen += 1;
        }
        prev.valid = valid;
        prev.state = status.state;
        prev.grandmaster = grandmaster;
        if grandmaster != 0 {
            prev.last_master = grandmaster;
        }
        if snapshot.is_some() {
            prev.snapshot = snapshot;
        }
        block.publish(&ClockRecord {
            // Meaningless while invalid; the last mapping helps diagnostics.
            snapshot: prev.snapshot.unwrap_or(ClockSnapshot {
                local_ref_ns: 0,
                media_ref_ns: 0,
                rate: 1.0,
            }),
            valid,
            state: status.state,
            step_gen: prev.step_gen,
            grandmaster,
            publish_ns: ovsc_clock::local_now_ns(),
        });
    }
}

/// `uuid48 << 16 | port_id` of the master, 0 if none.
fn grandmaster_id(master: Option<&MasterInfo>) -> u64 {
    master.map_or(0, |m| {
        let mut b = [0u8; 8];
        b[2..].copy_from_slice(&m.uuid);
        u64::from_be_bytes(b) << 16 | m.port_id as u64
    })
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use ovsc_shm::clock::{ClockRead, READ_TRIES};

    use super::*;

    fn read(block: &ClockBlock) -> ClockRecord {
        match block.read_bounded(READ_TRIES) {
            ClockRead::Record(r) => r,
            other => panic!("no record: {other:?}"),
        }
    }

    fn status(state: ClockState, master: Option<MasterInfo>) -> ClockStatus {
        ClockStatus { state, master, ..Default::default() }
    }

    #[test]
    fn steps_follow_the_rules() {
        let w = MirrorWriter::default();
        let block = ClockBlock::new();
        let m1 = MasterInfo { uuid: [1, 2, 3, 4, 5, 6], port_id: 7, addr: Ipv4Addr::LOCALHOST };
        let m2 = MasterInfo { uuid: [1, 2, 3, 4, 5, 9], ..m1 };
        let snap = ClockSnapshot { local_ref_ns: 1_000_000, media_ref_ns: 5_000_000, rate: 1.0 };
        // Later points of the same line.
        let at = |local: u64, extra: u64| ClockSnapshot {
            local_ref_ns: local,
            media_ref_ns: snap.media_ns_at(local) + extra,
            rate: 1.0,
        };

        // Invalid and unlocked: nothing to count.
        w.publish(&block, None, &status(ClockState::Unlocked, None));
        let r = read(&block);
        assert_eq!(
            (r.valid, r.state, r.step_gen, r.grandmaster),
            (false, ClockState::Unlocked, 0, 0)
        );

        // A master appears: new grandmaster.
        w.publish(&block, None, &status(ClockState::Locking, Some(m1)));
        let r = read(&block);
        assert_eq!(r.step_gen, 1);
        assert_eq!(r.grandmaster, 0x0102_0304_0506 << 16 | 7);

        // First valid mapping.
        w.publish(&block, Some(snap), &status(ClockState::Locking, Some(m1)));
        assert_eq!(read(&block).step_gen, 2);
        assert_eq!(read(&block).snapshot, snap);

        // Continuous updates, within 1 us: no step.
        w.publish(&block, Some(at(2_000_000, 0)), &status(ClockState::Locking, Some(m1)));
        w.publish(&block, Some(at(3_000_000, 1_000)), &status(ClockState::Locking, Some(m1)));
        assert_eq!(read(&block).step_gen, 2);

        // Locking -> Locked.
        w.publish(&block, Some(at(3_000_000, 1_000)), &status(ClockState::Locked, Some(m1)));
        assert_eq!(read(&block).step_gen, 3);
        assert_eq!(read(&block).state, ClockState::Locked);

        // A step of 1 us + 1 ns against the previous mapping.
        let before = at(3_000_000, 1_000);
        let stepped = ClockSnapshot {
            local_ref_ns: 4_000_000,
            media_ref_ns: before.media_ns_at(4_000_000) + 1_001,
            rate: 1.0,
        };
        w.publish(&block, Some(stepped), &status(ClockState::Locked, Some(m1)));
        assert_eq!(read(&block).step_gen, 4);

        // Master change while locked.
        w.publish(&block, Some(stepped), &status(ClockState::Locked, Some(m2)));
        assert_eq!(read(&block).step_gen, 5);

        // Master lost: the clock holds over on the same mapping, which is no
        // step. The same master back is none either until it locks again.
        w.publish(&block, Some(stepped), &status(ClockState::Unlocked, None));
        let r = read(&block);
        assert_eq!((r.valid, r.step_gen, r.grandmaster), (true, 5, 0));
        w.publish(&block, Some(stepped), &status(ClockState::Locking, Some(m2)));
        assert_eq!(read(&block).step_gen, 5);
        w.publish(&block, Some(stepped), &status(ClockState::Locked, Some(m2)));
        assert_eq!(read(&block).step_gen, 6);

        // Lost, holdover expired: the block keeps the last mapping. Valid
        // again counts once; so does a master other than the last.
        w.publish(&block, Some(stepped), &status(ClockState::Unlocked, None));
        w.publish(&block, None, &status(ClockState::Unlocked, None));
        let r = read(&block);
        assert_eq!((r.valid, r.step_gen), (false, 6));
        assert_eq!(r.snapshot, stepped);
        w.publish(&block, Some(stepped), &status(ClockState::Unlocked, None));
        assert_eq!(read(&block).step_gen, 7);
        w.publish(&block, Some(stepped), &status(ClockState::Locking, Some(m1)));
        assert_eq!(read(&block).step_gen, 8);

        // Locking -> FreeRunning.
        w.publish(&block, Some(stepped), &status(ClockState::FreeRunning, None));
        let r = read(&block);
        assert_eq!((r.valid, r.state, r.step_gen), (true, ClockState::FreeRunning, 9));
        assert!(r.publish_ns > 0);
    }
}
