//! Retiring attachments the IO paths may still be reading (design section
//! 11).
//!
//! The link swaps a new attachment in (or none), neutralizes the old one's
//! mapping at once, and hands it to a [`Retirement`]. Every
//! [`RETIRE_POLL`] the link then asks the IO engine whether it is quiescent
//! and polls with the answer. An attachment is freed, which unmaps it, once
//! a poll after its swap has seen the engine quiescent *and* at least
//! [`RETIRE_GRACE_NS`] has passed since the swap.
//!
//! The quiescence alone is the proof: a reader that still holds the old
//! attachment counted itself in before loading it, and loaded it before the
//! swap, so a reader count of 0 seen after the swap means it has finished.
//! The neutralized mapping and the grace period are defence in depth.

use std::time::Duration;

/// How often the link polls for quiescence while attachments wait.
pub const RETIRE_POLL: Duration = Duration::from_millis(10);

/// The least time between a swap and freeing the attachment swapped out.
pub const RETIRE_GRACE_NS: u64 = 1_000_000_000;

struct Retiring<T> {
    item: T,
    /// Host time of the swap.
    swapped_ns: u64,
    /// A poll after the swap found the IO engine quiescent.
    quiet: bool,
}

/// Attachments swapped out and not yet freed, oldest first.
pub struct Retirement<T> {
    items: Vec<Retiring<T>>,
    /// A poll is scheduled.
    polling: bool,
}

impl<T> Default for Retirement<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Retirement<T> {
    pub const fn new() -> Self {
        Self { items: Vec::new(), polling: false }
    }

    /// Keeps `item`, swapped out at `now_ns`, until it may be freed. Returns
    /// whether the caller must schedule a poll (none is pending).
    pub fn push(&mut self, item: T, now_ns: u64) -> bool {
        self.items.push(Retiring { item, swapped_ns: now_ns, quiet: false });
        !std::mem::replace(&mut self.polling, true)
    }

    /// One poll at `now_ns`. `quiescent` must have been read after every
    /// swap of the items held, which is true of any poll the link runs.
    /// Returns the items that may be freed now, and whether to poll again.
    pub fn poll(&mut self, quiescent: bool, now_ns: u64) -> (Vec<T>, bool) {
        let mut done = Vec::new();
        let mut kept = Vec::with_capacity(self.items.len());
        for mut r in self.items.drain(..) {
            r.quiet |= quiescent;
            if r.quiet && now_ns.saturating_sub(r.swapped_ns) >= RETIRE_GRACE_NS {
                done.push(r.item);
            } else {
                kept.push(r);
            }
        }
        self.items = kept;
        self.polling = !self.items.is_empty();
        (done, self.polling)
    }

    /// How many items wait.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    #[test]
    fn an_item_waits_for_quiescence_and_the_grace() {
        let mut r = Retirement::new();
        assert!(r.push("a", 0));
        // Quiescent, but within the grace.
        assert_eq!(r.poll(true, 999 * MS), (vec![], true));
        // The grace has passed, and quiescence was seen before.
        assert_eq!(r.poll(false, 1000 * MS), (vec!["a"], false));
        assert!(r.is_empty());
    }

    #[test]
    fn a_busy_engine_holds_items_past_the_grace() {
        let mut r = Retirement::new();
        r.push("a", 0);
        for t in [10, 500, 1000, 5000] {
            assert_eq!(r.poll(false, t * MS), (vec![], true));
        }
        assert_eq!(r.poll(true, 5010 * MS), (vec!["a"], false));
    }

    #[test]
    fn items_retire_on_their_own_clocks() {
        let mut r = Retirement::new();
        assert!(r.push("a", 0));
        // A poll is already pending.
        assert!(!r.push("b", 600 * MS));
        assert_eq!(r.len(), 2);
        assert_eq!(r.poll(true, 1000 * MS), (vec!["a"], true));
        assert_eq!(r.poll(false, 1599 * MS), (vec![], true));
        assert_eq!(r.poll(false, 1600 * MS), (vec!["b"], false));
        // Polling stopped, so the next push starts it again.
        assert!(r.push("c", 2000 * MS));
    }

    #[test]
    fn quiescence_seen_before_a_swap_does_not_count() {
        let mut r: Retirement<&str> = Retirement::new();
        r.push("a", 0);
        r.poll(true, 10 * MS);
        r.push("b", 20 * MS);
        // "a" saw quiescence at 10 ms, "b" has not seen it yet.
        assert_eq!(r.poll(false, 1500 * MS), (vec!["a"], true));
        assert_eq!(r.poll(false, 3000 * MS), (vec![], true));
        assert_eq!(r.poll(true, 3010 * MS), (vec!["b"], false));
    }
}
