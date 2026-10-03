//! Simplified PTPv1 best master clock (BMC) selection for a slave-only clock.
//!
//! A full IEEE 1588-2002 BMC also decides whether *we* should become master.
//! OpenVirtualSoundcard never does, so all that is left is: remember which masters are
//! currently sending Sync messages, and pick the best of them using the
//! PTPv1 data set comparison order.
//!
//! To avoid flapping between masters while a Dante network re-elects its
//! master, the selection only moves away from a live master when another one
//! has been strictly better for [`SWITCH_AFTER_SYNCS`] consecutive Syncs.

use std::cmp::Ordering;
use std::net::Ipv4Addr;
use std::time::Duration;

use super::wire::{Header, PortIdentity, SyncBody};

/// How many consecutive Syncs a strictly better master must send before we
/// switch to it.
pub const SWITCH_AFTER_SYNCS: u32 = 2;

/// Rank of a PTPv1 clock identifier; lower is better.
///
/// IEEE 1588-2002 orders primary references before free-running clocks:
/// `ATOM < GPS < NTP < HAND < INIT < DFLT`; unknown identifiers rank last.
pub fn identifier_rank(identifier: &[u8; 4]) -> u8 {
    // Identifiers shorter than four characters are NUL- (or space-) padded.
    let len = identifier.iter().rposition(|&b| b != 0 && b != b' ').map_or(0, |i| i + 1);
    match &identifier[..len] {
        b"ATOM" => 0,
        b"GPS" => 1,
        b"NTP" => 2,
        b"HAND" => 3,
        b"INIT" => 4,
        b"DFLT" => 5,
        _ => 6,
    }
}

/// The parts of a Sync message that master selection compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MasterDataset {
    /// The port sending the Sync messages.
    pub source: PortIdentity,
    pub grandmaster_uuid: [u8; 6],
    pub preferred: bool,
    pub stratum: u8,
    pub identifier: [u8; 4],
    pub variance: i16,
    pub steps_removed: u16,
}

impl MasterDataset {
    /// Extracts the data set advertised by a Sync message.
    pub fn from_sync(header: &Header, body: &SyncBody) -> MasterDataset {
        MasterDataset {
            source: header.source,
            grandmaster_uuid: body.grandmaster_clock_uuid,
            preferred: body.grandmaster_preferred,
            stratum: body.grandmaster_clock_stratum,
            identifier: body.grandmaster_clock_identifier,
            variance: body.grandmaster_clock_variance,
            steps_removed: body.local_steps_removed,
        }
    }

    /// Compares two data sets; `Ordering::Less` means `self` is the better
    /// master.
    ///
    /// Order: preferred first, then lower stratum, then better identifier,
    /// then lower variance, then lower grandmaster UUID. The last criteria
    /// (fewer steps removed, then lower sender identity) only separate two
    /// paths to the same grandmaster and make the order total.
    pub fn compare(&self, other: &MasterDataset) -> Ordering {
        self.key().cmp(&other.key())
    }

    /// Whether `self` is strictly better than `other`.
    pub fn is_better_than(&self, other: &MasterDataset) -> bool {
        self.compare(other) == Ordering::Less
    }

    fn key(&self) -> (bool, u8, u8, i16, [u8; 6], u16, PortIdentity) {
        (
            !self.preferred,
            self.stratum,
            identifier_rank(&self.identifier),
            self.variance,
            self.grandmaster_uuid,
            self.steps_removed,
            self.source,
        )
    }
}

/// A master that has recently sent Sync messages.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ForeignMaster {
    pub dataset: MasterDataset,
    /// Source address of its Sync messages.
    pub addr: Ipv4Addr,
    /// Local time ([`crate::local_now_ns`]) of its last Sync.
    pub last_sync_ns: u64,
    /// Consecutive Syncs in which it was strictly better than the current
    /// master.
    better_syncs: u32,
}

/// A change of the selected master.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MasterChange {
    pub previous: Option<PortIdentity>,
    pub current: Option<PortIdentity>,
}

/// Tracks foreign masters and selects the one to follow.
///
/// This is pure bookkeeping: time is passed in, so it can be tested without
/// a network.
#[derive(Clone, Debug)]
pub struct MasterSelector {
    timeout_ns: u64,
    masters: Vec<ForeignMaster>,
    current: Option<PortIdentity>,
}

impl MasterSelector {
    /// A selector that forgets masters after `timeout` without a Sync.
    pub fn new(timeout: Duration) -> MasterSelector {
        MasterSelector {
            timeout_ns: timeout.as_nanos().min(u64::MAX as u128) as u64,
            masters: Vec::new(),
            current: None,
        }
    }

    /// The master currently followed.
    pub fn current(&self) -> Option<&ForeignMaster> {
        let id = self.current?;
        self.masters.iter().find(|m| m.dataset.source == id)
    }

    /// All masters heard from within the timeout.
    pub fn masters(&self) -> &[ForeignMaster] {
        &self.masters
    }

    /// Records a Sync message received at local time `now_ns`, and returns
    /// the resulting change of master, if any.
    pub fn on_sync(
        &mut self,
        dataset: MasterDataset,
        addr: Ipv4Addr,
        now_ns: u64,
    ) -> Option<MasterChange> {
        let id = dataset.source;
        let idx = match self.masters.iter().position(|m| m.dataset.source == id) {
            Some(i) => {
                let m = &mut self.masters[i];
                m.dataset = dataset;
                m.addr = addr;
                m.last_sync_ns = now_ns;
                i
            }
            None => {
                self.masters.push(ForeignMaster {
                    dataset,
                    addr,
                    last_sync_ns: now_ns,
                    better_syncs: 0,
                });
                self.masters.len() - 1
            }
        };

        let Some(current) = self.current().copied() else {
            return Some(self.switch_to(Some(id)));
        };
        if current.dataset.source == id {
            return None;
        }
        let m = &mut self.masters[idx];
        if m.dataset.is_better_than(&current.dataset) {
            m.better_syncs += 1;
            if m.better_syncs >= SWITCH_AFTER_SYNCS {
                return Some(self.switch_to(Some(id)));
            }
        } else {
            m.better_syncs = 0;
        }
        None
    }

    /// Forgets masters that have been silent for longer than the timeout. If
    /// the current master is among them, selects the best remaining one (or
    /// none) and returns the change.
    pub fn expire(&mut self, now_ns: u64) -> Option<MasterChange> {
        let timeout = self.timeout_ns;
        self.masters.retain(|m| now_ns.saturating_sub(m.last_sync_ns) <= timeout);
        let current = self.current?;
        if self.masters.iter().any(|m| m.dataset.source == current) {
            return None;
        }
        let best = self
            .masters
            .iter()
            .min_by(|a, b| a.dataset.compare(&b.dataset))
            .map(|m| m.dataset.source);
        Some(self.switch_to(best))
    }

    /// Forgets all masters.
    pub fn clear(&mut self) {
        self.masters.clear();
        self.current = None;
    }

    fn switch_to(&mut self, new: Option<PortIdentity>) -> MasterChange {
        for m in &mut self.masters {
            m.better_syncs = 0;
        }
        let previous = std::mem::replace(&mut self.current, new);
        MasterChange { previous, current: new }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ds(uuid_last: u8) -> MasterDataset {
        MasterDataset {
            source: PortIdentity { uuid: [0, 0x1d, 0xc1, 0, 0, uuid_last], port_id: 1 },
            grandmaster_uuid: [0, 0x1d, 0xc1, 0, 0, uuid_last],
            preferred: false,
            stratum: 4,
            identifier: *b"DFLT",
            variance: 0,
            steps_removed: 0,
        }
    }

    const ADDR: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 10);
    const SEC: u64 = 1_000_000_000;

    #[test]
    fn identifier_ranking() {
        let order = [*b"ATOM", *b"GPS\0", *b"NTP\0", *b"HAND", *b"INIT", *b"DFLT", *b"XYZW"];
        for pair in order.windows(2) {
            assert!(identifier_rank(&pair[0]) < identifier_rank(&pair[1]), "{pair:?}");
        }
        assert_eq!(identifier_rank(b"GPS "), identifier_rank(b"GPS\0"));
        assert_eq!(identifier_rank(b"\0\0\0\0"), 6);
    }

    #[test]
    fn dataset_comparison_order() {
        let base = ds(5);

        // Preferred beats everything else.
        let preferred = MasterDataset { preferred: true, stratum: 255, ..ds(9) };
        assert!(preferred.is_better_than(&base));

        // Then lower stratum.
        let better_stratum = MasterDataset { stratum: 3, identifier: *b"XXXX", ..ds(9) };
        assert!(better_stratum.is_better_than(&base));

        // Then identifier.
        let gps = MasterDataset { identifier: *b"GPS\0", variance: 100, ..ds(9) };
        assert!(gps.is_better_than(&base));
        let atom = MasterDataset { identifier: *b"ATOM", ..ds(9) };
        assert!(atom.is_better_than(&gps));

        // Then lower variance.
        let low_var = MasterDataset { variance: -100, ..ds(9) };
        assert!(low_var.is_better_than(&base));

        // Then lower UUID.
        assert!(ds(4).is_better_than(&base));
        assert!(!ds(6).is_better_than(&base));

        // Equal data sets are not better than each other.
        assert_eq!(base.compare(&base), Ordering::Equal);
        assert!(!base.is_better_than(&base));
    }

    #[test]
    fn first_master_is_selected_immediately() {
        let mut sel = MasterSelector::new(Duration::from_secs(5));
        let change = sel.on_sync(ds(5), ADDR, SEC).unwrap();
        assert_eq!(change, MasterChange { previous: None, current: Some(ds(5).source) });
        assert_eq!(sel.current().unwrap().dataset, ds(5));
        assert_eq!(sel.on_sync(ds(5), ADDR, 2 * SEC), None);
    }

    #[test]
    fn switches_to_better_master_after_two_syncs() {
        let mut sel = MasterSelector::new(Duration::from_secs(5));
        sel.on_sync(ds(5), ADDR, SEC);
        // A worse master never takes over.
        for i in 0..5 {
            assert_eq!(sel.on_sync(ds(9), ADDR, SEC + i), None);
        }
        // A better one needs two consecutive Syncs.
        assert_eq!(sel.on_sync(ds(1), ADDR, 2 * SEC), None);
        let change = sel.on_sync(ds(1), ADDR, 3 * SEC).unwrap();
        assert_eq!(change.previous, Some(ds(5).source));
        assert_eq!(change.current, Some(ds(1).source));
    }

    #[test]
    fn better_streak_resets_when_candidate_degrades() {
        let mut sel = MasterSelector::new(Duration::from_secs(5));
        sel.on_sync(ds(5), ADDR, SEC);
        assert_eq!(sel.on_sync(ds(1), ADDR, 2 * SEC), None);
        let degraded = MasterDataset { stratum: 200, ..ds(1) };
        assert_eq!(sel.on_sync(degraded, ADDR, 3 * SEC), None);
        assert_eq!(sel.on_sync(ds(1), ADDR, 4 * SEC), None);
        assert!(sel.on_sync(ds(1), ADDR, 5 * SEC).is_some());
    }

    #[test]
    fn falls_back_when_current_master_times_out() {
        let mut sel = MasterSelector::new(Duration::from_secs(5));
        sel.on_sync(ds(1), ADDR, SEC);
        sel.on_sync(ds(9), ADDR, SEC);
        assert_eq!(sel.current().unwrap().dataset.source, ds(1).source);
        // ds(1) goes quiet; ds(9) keeps sending.
        for t in 2..=7 {
            sel.on_sync(ds(9), ADDR, t * SEC);
        }
        assert_eq!(sel.expire(6 * SEC), None);
        let change = sel.expire(7 * SEC).unwrap();
        assert_eq!(change.current, Some(ds(9).source));
        // Then ds(9) goes quiet too.
        let change = sel.expire(13 * SEC).unwrap();
        assert_eq!(change, MasterChange { previous: Some(ds(9).source), current: None });
        assert!(sel.current().is_none());
        assert!(sel.masters().is_empty());
    }
}
