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
//!
//! When the leader just goes silent (it rebooted, its link went down), the
//! network elects another one, which is usually not better than the old
//! leader's last data set. Waiting for the old leader's timeout would leave
//! the clock unsupervised for seconds, so once the current master has been
//! silent for [`TAKEOVER_AFTER_SILENCE`] (or for four of the Sync intervals
//! it advertises, if that is longer), the best of the masters still sending
//! takes over after [`SWITCH_AFTER_SYNCS`] Syncs. If the old leader
//! comes back and is better, it takes over again the same way; a leader
//! change keeps the clock continuous (see the servo), so the cost of that is
//! small.

use std::cmp::Ordering;
use std::net::Ipv4Addr;
use std::time::Duration;

use super::wire::{Header, PortIdentity, SyncBody};

/// How many consecutive Syncs a strictly better master must send before we
/// switch to it.
pub const SWITCH_AFTER_SYNCS: u32 = 2;

/// How long the current master must have been silent before another one
/// may take over without being better: four Sync intervals of a master
/// sending every 250 ms (as the development master does), so that a lost
/// Sync or two never moves the selection. A master that advertises a longer
/// Sync interval gets four of its own intervals, so that its normal gaps are
/// never mistaken for silence (docs/PROTOCOL.md §9 lists Dante's interval
/// as unconfirmed). It is never longer than the master timeout.
pub const TAKEOVER_AFTER_SILENCE: Duration = Duration::from_secs(1);

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
    /// log2 of the Sync interval the master advertises, in seconds. Not
    /// compared: it only sets how long the master may be silent before
    /// another one takes over (see [`TAKEOVER_AFTER_SILENCE`]).
    pub sync_interval: i8,
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
            sync_interval: body.sync_interval,
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
        let takeover_ns = self.takeover_after(&current.dataset);
        let current_silent = now_ns.saturating_sub(current.last_sync_ns) > takeover_ns;
        let takes_over =
            m_is_best_live(&self.masters, idx, current.dataset.source, now_ns, takeover_ns);
        let m = &mut self.masters[idx];
        if m.dataset.is_better_than(&current.dataset) || (current_silent && takes_over) {
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

    /// How long `master` must have been silent before a master that is not
    /// better may take over (see [`TAKEOVER_AFTER_SILENCE`]).
    fn takeover_after(&self, master: &MasterDataset) -> u64 {
        // The field is log2 seconds; anything outside ±7 is not a real
        // interval, and the clamp keeps the arithmetic finite.
        let interval_s = 2f64.powi(i32::from(master.sync_interval.clamp(-7, 7)));
        let four_intervals = (4.0 * interval_s * 1e9) as u64;
        four_intervals.max(TAKEOVER_AFTER_SILENCE.as_nanos() as u64).min(self.timeout_ns)
    }

    fn switch_to(&mut self, new: Option<PortIdentity>) -> MasterChange {
        for m in &mut self.masters {
            m.better_syncs = 0;
        }
        let previous = std::mem::replace(&mut self.current, new);
        MasterChange { previous, current: new }
    }
}

/// Whether `masters[idx]` is the best of the masters other than `current`
/// heard within `live_ns` of `now_ns`.
fn m_is_best_live(
    masters: &[ForeignMaster],
    idx: usize,
    current: PortIdentity,
    now_ns: u64,
    live_ns: u64,
) -> bool {
    let candidate = &masters[idx].dataset;
    !masters.iter().any(|m| {
        m.dataset.source != current
            && m.dataset.source != candidate.source
            && now_ns.saturating_sub(m.last_sync_ns) <= live_ns
            && m.dataset.is_better_than(candidate)
    })
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
            sync_interval: -2,
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
        sel.on_sync(ds(5), ADDR, 3 * SEC);
        assert_eq!(sel.on_sync(degraded, ADDR, 3 * SEC), None);
        sel.on_sync(ds(5), ADDR, 4 * SEC);
        assert_eq!(sel.on_sync(ds(1), ADDR, 4 * SEC), None);
        assert!(sel.on_sync(ds(1), ADDR, 5 * SEC).is_some());
    }

    #[test]
    fn falls_back_when_current_master_times_out() {
        let mut sel = MasterSelector::new(Duration::from_secs(5));
        sel.on_sync(ds(1), ADDR, SEC);
        sel.on_sync(ds(9), ADDR, SEC);
        assert_eq!(sel.current().unwrap().dataset.source, ds(1).source);
        // ds(1) goes quiet, and so does everyone else.
        assert_eq!(sel.expire(6 * SEC), None);
        let change = sel.expire(7 * SEC).unwrap();
        assert_eq!(change, MasterChange { previous: Some(ds(1).source), current: None });
        assert!(sel.current().is_none());
        assert!(sel.masters().is_empty());
    }

    #[test]
    fn a_live_master_takes_over_from_a_silent_one() {
        let ms = SEC / 1000;
        let mut sel = MasterSelector::new(Duration::from_secs(5));
        sel.on_sync(ds(1), ADDR, SEC);
        // ds(9) is worse, so while ds(1) sends it never takes over, and a
        // gap of up to a second in ds(1)'s Syncs changes nothing.
        let mut t = SEC;
        for _ in 0..8 {
            t += 250 * ms;
            assert_eq!(sel.on_sync(ds(1), ADDR, t), None);
            assert_eq!(sel.on_sync(ds(9), ADDR, t + ms), None);
        }
        assert_eq!(sel.on_sync(ds(9), ADDR, t + 600 * ms), None);
        assert_eq!(sel.on_sync(ds(9), ADDR, t + 900 * ms), None);
        // Silent for more than a second: two Syncs of ds(9) take over,
        // long before the 5 s timeout.
        assert_eq!(sel.on_sync(ds(9), ADDR, t + 1_100 * ms), None);
        let change = sel.on_sync(ds(9), ADDR, t + 1_350 * ms).unwrap();
        assert_eq!(
            change,
            MasterChange { previous: Some(ds(1).source), current: Some(ds(9).source) }
        );
        // ds(1) back, and better: it takes over again after two Syncs.
        assert_eq!(sel.on_sync(ds(1), ADDR, t + 1_500 * ms), None);
        assert_eq!(sel.on_sync(ds(1), ADDR, t + 1_750 * ms).unwrap().current, Some(ds(1).source));
    }

    #[test]
    fn only_the_best_live_master_takes_over() {
        let ms = SEC / 1000;
        let mut sel = MasterSelector::new(Duration::from_secs(5));
        sel.on_sync(ds(1), ADDR, SEC);
        // ds(1) falls silent; ds(9) and the better ds(5) both send.
        let mut changes = Vec::new();
        for i in 1..=12u64 {
            let t = SEC + i * 250 * ms;
            changes.extend(sel.on_sync(ds(9), ADDR, t));
            changes.extend(sel.on_sync(ds(5), ADDR, t + ms));
        }
        assert_eq!(
            changes,
            [MasterChange { previous: Some(ds(1).source), current: Some(ds(5).source) }]
        );
    }

    #[test]
    fn a_master_with_slow_syncs_is_not_taken_over_between_them() {
        let ms = SEC / 1000;
        // Takes `slow` (sending every 2^interval s) silent at `t`, with the
        // worse ds(9) sending every 250 ms; returns how long after `t` ds(9)
        // took over.
        let takeover = |sel: &mut MasterSelector, t: u64| {
            let mut at = t;
            loop {
                at += 250 * ms;
                if let Some(change) = sel.on_sync(ds(9), ADDR, at) {
                    assert_eq!(change.current, Some(ds(9).source));
                    return at - t;
                }
            }
        };

        // ds(1) advertises a Sync every 2 s (IEEE 1588-2002's default); the
        // gaps between its Syncs are not silence.
        let slow = MasterDataset { sync_interval: 1, ..ds(1) };
        let mut sel = MasterSelector::new(Duration::from_secs(5));
        sel.on_sync(slow, ADDR, SEC);
        let mut t = SEC;
        for _ in 0..10 {
            for k in 1..=7 {
                assert_eq!(sel.on_sync(ds(9), ADDR, t + k * 250 * ms), None);
            }
            t += 2 * SEC;
            assert_eq!(sel.on_sync(slow, ADDR, t), None);
        }
        // Four of its intervals are longer than the timeout, so ds(9) only
        // takes over after that, as it always did.
        assert_eq!(takeover(&mut sel, t), 5_500 * ms);

        // One Sync a second: ds(9) takes over after four seconds of silence.
        let slow = MasterDataset { sync_interval: 0, ..ds(1) };
        let mut sel = MasterSelector::new(Duration::from_secs(5));
        sel.on_sync(slow, ADDR, SEC);
        assert_eq!(takeover(&mut sel, SEC), 4_500 * ms);
    }
}
