//! The daemon's region as the IO paths see it, and the slot that lets the
//! IPC queue replace it under running IO (design sections 6.2 and 11).
//!
//! Real-time readers enter the slot, which counts them, then load the
//! current attachment:
//!
//! ```text
//! READERS.fetch_add(1, SeqCst); a = CURRENT.load(SeqCst); ...; READERS.fetch_sub(1, Release)
//! ```
//!
//! The IPC queue swaps in a new attachment, neutralizes the old mapping at
//! once and frees it only once it has seen no readers after the swap. A
//! reader that still holds the old attachment incremented the count before
//! its load, and its load came before the swap, so a count of 0 seen after
//! the swap proves it has finished. [`AttachSlot::swap`] enforces this: the
//! old attachment comes back as a [`Retired`], which becomes an owned `Box`
//! only once the slot has been seen quiescent.

// The real-time paths enter here; io.rs denies the same lints for the whole
// engine.
#![deny(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects
)]

use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use ovsc_ipc::region::MappedRegion;
use ovsc_shm::layout::{LayoutError, MAX_CHANNELS, RING_BYTES, RX_OFFSET, RegionRef, TX_OFFSET};
use ovsc_shm::time::Timebase;

use crate::platform;

/// The furthest the daemon's heartbeat may be from this process's clock
/// when its region is attached. Further means the two processes do not
/// share a clock base (design section 6.2).
pub const CLOCK_BASE_TOLERANCE_NS: u64 = 10_000_000_000;

/// A mapped and validated daemon region.
///
/// `view` borrows the memory `mapped` owns: it must not be used after the
/// attachment is dropped, which the `'static` cannot express.
pub struct Attachment {
    pub mapped: MappedRegion,
    pub view: RegionRef<'static>,
    /// The daemon's generation, from the header: one per daemon process.
    pub generation: u64,
}

impl Attachment {
    /// Validates a freshly mapped region against this build's layout, and
    /// its clock base against the production host clock
    /// ([`platform::production`]).
    ///
    /// Only for code whose IO engine runs on that clock. Anything else must
    /// compare with the engine's own clock, or a test on a manual clock sees
    /// every heartbeat as either too far off here or stale in the gate. Use
    /// [`IoEngine::new_attachment`](crate::io::IoEngine::new_attachment)
    /// instead; the link uses its sink's `new_attachment`, which checks
    /// against `sink.now_ns()`.
    pub fn new(mapped: MappedRegion) -> Result<Box<Attachment>, LayoutError> {
        let p = platform::production();
        Self::new_at(mapped, p.timebase().ticks_to_ns(p.now_ticks()))
    }

    /// [`Attachment::new`], comparing the daemon's heartbeat with `now_ns`,
    /// this process's host time in nanoseconds.
    ///
    /// Rejects a region whose header disagrees with the compiled layout,
    /// whose generation is 0 (the header promises a nonzero one), or whose
    /// heartbeat lies more than [`CLOCK_BASE_TOLERANCE_NS`] from `now_ns`.
    /// A heartbeat of 0 (not written yet) passes: the IO gate stays closed
    /// until heartbeats arrive, and checks them itself. A daemon timebase
    /// that differs from ours is tolerated, because the region carries
    /// nanoseconds; see [`Attachment::daemon_timebase`].
    pub fn new_at(mapped: MappedRegion, now_ns: u64) -> Result<Box<Attachment>, LayoutError> {
        // SAFETY: `mapped` is valid for reads and writes of its length until
        // it is dropped, and it moves into the attachment with the view, so
        // the view never outlives it (see the type's documentation). The
        // daemon writes the header once, before it shares the region, and
        // everything else only through atomics.
        let view = unsafe { RegionRef::from_raw(mapped.as_ptr(), mapped.len()) }?;
        let generation = view.header().daemon_generation;
        if generation == 0 {
            return Err(LayoutError::BadGeometry);
        }
        let heartbeat_ns = view.daemon().heartbeat_ns.load(Ordering::Acquire);
        if heartbeat_ns != 0 && heartbeat_ns.abs_diff(now_ns) > CLOCK_BASE_TOLERANCE_NS {
            return Err(LayoutError::ClockBase { heartbeat_ns, now_ns });
        }
        Ok(Box::new(Attachment { mapped, view, generation }))
    }

    /// The daemon's Mach timebase, which may differ from ours (under
    /// Rosetta, say) without harm.
    pub fn daemon_timebase(&self) -> Timebase {
        self.view.header().timebase()
    }

    /// Commits the pages the IO paths will use: the control blocks and the
    /// rings of the active channels, with at least one `fetch_add(0)` per
    /// page, so the first real-time access takes no page fault.
    ///
    /// The control blocks are committed through words the plug-in owns: the
    /// plug-in status shares its page with the header and the daemon's
    /// blocks, which are never written, not even with 0. Every trace entry
    /// is touched, so the trace's pages are committed whatever the page
    /// size.
    pub fn touch(&self, input_channels: u32, output_channels: u32) {
        let rings =
            |channels: u32| (channels as usize).min(MAX_CHANNELS).saturating_mul(RING_BYTES);
        self.view.plugin().read_calls.fetch_add(0, Ordering::Relaxed);
        let (header, entries) = self.view.io_trace();
        header.session.fetch_add(0, Ordering::Relaxed);
        for e in entries {
            e.cycle_counter.fetch_add(0, Ordering::Relaxed);
        }
        self.mapped.touch(RX_OFFSET, rings(input_channels));
        self.mapped.touch(TX_OFFSET, rings(output_channels));
    }
}

/// Where the current attachment lives: an atomic pointer plus a count of the
/// real-time readers that may be using it.
pub struct AttachSlot {
    current: AtomicPtr<Attachment>,
    readers: AtomicU32,
}

impl Default for AttachSlot {
    fn default() -> Self {
        Self::new()
    }
}

impl AttachSlot {
    /// An empty slot.
    pub const fn new() -> Self {
        Self { current: AtomicPtr::new(ptr::null_mut()), readers: AtomicU32::new(0) }
    }

    /// Enters as a reader. The attachment the guard holds stays mapped until
    /// the guard drops. Real-time safe: two atomic operations.
    #[inline]
    pub fn enter(&self) -> AttachGuard<'_> {
        self.readers.fetch_add(1, Ordering::SeqCst);
        let current = self.current.load(Ordering::SeqCst);
        AttachGuard { slot: self, current }
    }

    /// Installs `new` and returns the previous attachment.
    ///
    /// Readers that entered before the swap may still be using the previous
    /// attachment, so it comes back as a [`Retired`]: neutralize its mapping
    /// at once through [`Retired::get`], and free it with
    /// [`Retired::into_box`] once this slot is quiescent.
    pub fn swap(&self, new: Option<Box<Attachment>>) -> Option<Retired> {
        let new = new.map_or(ptr::null_mut(), Box::into_raw);
        let old = self.current.swap(new, Ordering::SeqCst);
        // Every non-null pointer in the slot came from Box::into_raw and is
        // owned by the slot; the swap hands ownership to the Retired.
        NonNull::new(old).map(|attachment| Retired { attachment, slot: self.addr() })
    }

    /// Whether no reader is inside right now. Seen after a swap, it means
    /// none can still hold the previous attachment.
    pub fn quiescent(&self) -> bool {
        self.readers.load(Ordering::SeqCst) == 0
    }

    /// The slot's address, which identifies it to the attachments it
    /// retires.
    fn addr(&self) -> usize {
        ptr::from_ref(self).addr()
    }
}

impl Drop for AttachSlot {
    fn drop(&mut self) {
        // `&mut self`: no guard is alive, so the slot is quiescent and the
        // attachment comes back owned.
        if let Some(Ok(a)) = self.swap(None).map(|r| r.into_box(self)) {
            drop(a);
        }
    }
}

/// An attachment swapped out of an [`AttachSlot`] that readers which entered
/// before the swap may still be using.
///
/// It gives shared access only, which is enough to neutralize its mapping,
/// until the slot has been seen quiescent: then [`Retired::into_box`] hands
/// it over to be freed. Dropping a `Retired` leaks the attachment rather
/// than risk freeing it under a reader.
#[must_use = "dropping a Retired leaks the attachment; free it with into_box once quiescent"]
pub struct Retired {
    attachment: NonNull<Attachment>,
    /// The address of the slot it came from: only that slot's readers can
    /// hold it.
    slot: usize,
}

// SAFETY: a Retired owns its attachment, which is Send and Sync (checked
// below); readers share it only through `&Attachment`.
unsafe impl Send for Retired {}
// SAFETY: as above; `&Retired` gives only `&Attachment`.
unsafe impl Sync for Retired {}

const _: () = {
    const fn send_and_sync<T: Send + Sync>() {}
    send_and_sync::<Attachment>();
};

impl Retired {
    /// The attachment, shared with any reader still inside.
    pub fn get(&self) -> &Attachment {
        // SAFETY: the slot handed its ownership over in the swap, and nothing
        // frees the attachment before this Retired becomes a Box.
        unsafe { self.attachment.as_ref() }
    }

    /// The attachment, owned, if `slot` is the one it was swapped out of
    /// and no reader is inside it now. Every reader that could hold the
    /// attachment entered before the swap, which came before this call, so
    /// none is left. Otherwise gives `self` back, to try again later.
    pub fn into_box(self, slot: &AttachSlot) -> Result<Box<Attachment>, Retired> {
        if slot.addr() != self.slot || !slot.quiescent() {
            return Err(self);
        }
        // SAFETY: no reader can still use the attachment (above).
        Ok(unsafe { self.into_box_unchecked() })
    }

    /// The attachment, owned, without checking for readers.
    ///
    /// # Safety
    /// The box must not be dropped while a reader of the slot it came from
    /// may still be using the attachment: not before the slot has been seen
    /// quiescent after the swap. Until then, only read through it.
    pub unsafe fn into_box_unchecked(self) -> Box<Attachment> {
        // SAFETY: the pointer came from Box::into_raw, ownership passed to
        // this Retired in the swap, and the caller's guarantee covers the
        // readers.
        unsafe { Box::from_raw(self.attachment.as_ptr()) }
    }
}

/// A reader's hold on the current attachment; see [`AttachSlot::enter`].
pub struct AttachGuard<'a> {
    slot: &'a AttachSlot,
    current: *const Attachment,
}

impl AttachGuard<'_> {
    /// The attachment that was current when the guard was made, if any.
    #[inline]
    pub fn get(&self) -> Option<&Attachment> {
        // SAFETY: the pointer came from the slot while this guard was
        // counted as a reader, so the attachment is not freed before the
        // guard drops (the retire protocol above).
        unsafe { self.current.as_ref() }
    }
}

impl Drop for AttachGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        self.slot.readers.fetch_sub(1, Ordering::Release);
    }
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod tests {
    use std::sync::Arc;

    use ovsc_ipc::region::SharedRegion;
    use ovsc_shm::layout::{HOST_ARCH, HeaderInit, REGION_SIZE};

    use super::*;

    /// A region laid out by a daemon of generation `generation` with heartbeat
    /// `heartbeat_ns`, and a mapping of it.
    fn region(generation: u64, heartbeat_ns: u64) -> (Arc<SharedRegion>, MappedRegion) {
        let r = SharedRegion::create(REGION_SIZE).unwrap();
        let h = HeaderInit {
            daemon_generation: generation,
            daemon_pid: 1,
            arch: HOST_ARCH,
            timebase: Timebase { numer: 125, denom: 3 },
            created_host_ns: 1,
            daemon_version: HeaderInit::version_bytes("test"),
        };
        let view = unsafe { RegionRef::init(r.as_ptr(), r.len(), &h) }.unwrap();
        view.daemon().heartbeat_ns.store(heartbeat_ns, Ordering::Release);
        let m = r.handle().map().unwrap();
        (r, m)
    }

    #[test]
    fn attachments_validate_layout_generation_and_clock_base() {
        let now = 1_000_000_000_000;
        let (_r, m) = region(0x1f3a, now - 5_000_000_000);
        let a = Attachment::new_at(m, now).unwrap();
        assert_eq!(a.generation, 0x1f3a);
        assert_eq!(a.daemon_timebase(), Timebase { numer: 125, denom: 3 });
        a.touch(8, 8);
        a.touch(1000, 0);

        let (_r, m) = region(0, now);
        assert_eq!(Attachment::new_at(m, now).err(), Some(LayoutError::BadGeometry));
        let (_r, m) = region(7, now + CLOCK_BASE_TOLERANCE_NS + 1);
        assert_eq!(
            Attachment::new_at(m, now).err(),
            Some(LayoutError::ClockBase {
                heartbeat_ns: now + CLOCK_BASE_TOLERANCE_NS + 1,
                now_ns: now
            })
        );
        // No heartbeat yet: nothing to compare.
        let (_r, m) = region(7, 0);
        assert!(Attachment::new_at(m, now).is_ok());

        // Not a region at all.
        let r = SharedRegion::create(REGION_SIZE).unwrap();
        let m = r.handle().map().unwrap();
        assert_eq!(Attachment::new_at(m, now).err(), Some(LayoutError::BadMagic(0)));
    }

    #[test]
    fn attachments_compare_with_the_production_clock() {
        let p = platform::production();
        let now = p.timebase().ticks_to_ns(p.now_ticks());
        let (_r, m) = region(3, now.max(1));
        assert!(Attachment::new(m).is_ok());
        let (_r, m) = region(3, now + 2 * CLOCK_BASE_TOLERANCE_NS);
        assert!(matches!(Attachment::new(m), Err(LayoutError::ClockBase { .. })));
    }

    #[test]
    fn readers_hold_the_attachment_they_entered_with() {
        let slot = AttachSlot::new();
        assert!(slot.enter().get().is_none());
        assert!(slot.quiescent());
        let (_r1, m1) = region(1, 0);
        assert!(slot.swap(Some(Attachment::new_at(m1, 0).unwrap())).is_none());

        let g = slot.enter();
        assert!(!slot.quiescent());
        let (_r2, m2) = region(2, 0);
        let old = slot.swap(Some(Attachment::new_at(m2, 0).unwrap())).unwrap();
        assert_eq!(old.get().generation, 1);
        // The reader still sees the attachment it entered with; a new one
        // sees the new attachment.
        assert_eq!(g.get().map(|a| a.generation), Some(1));
        assert_eq!(slot.enter().get().map(|a| a.generation), Some(2));
        assert!(!slot.quiescent());
        // Not freed while the reader is inside, nor on another slot's word.
        let Err(old) = old.into_box(&slot) else { panic!("freed under a reader") };
        drop(g);
        assert!(slot.quiescent());
        let Err(old) = old.into_box(&AttachSlot::new()) else { panic!("freed for another slot") };
        let old = old.into_box(&slot).ok().unwrap();
        assert_eq!(old.generation, 1);
        drop(old);
        let last = slot.swap(None).unwrap().into_box(&slot).ok().unwrap();
        assert_eq!(last.generation, 2);
        assert!(slot.enter().get().is_none());
    }
}
