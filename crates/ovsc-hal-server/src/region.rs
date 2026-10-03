//! The daemon's shared region (design sections 6 and 14.3): created once per
//! daemon process, laid out and stamped with a random generation, then
//! handed to every driver instance in its welcome. The device engine's
//! rings live in it.

use std::hash::{BuildHasher, Hasher};
use std::io;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use tracing::warn;

use ovsc_clock::local_now_ns;
use ovsc_core::ExternalRings;
use ovsc_core::buffer::TimedRing;
use ovsc_ipc::region::{RegionHandle, SharedRegion};
use ovsc_shm::layout::{
    HOST_ARCH, HeaderInit, MAX_CHANNELS, PAGE_BYTES, REGION_SIZE, RING_BYTES, RING_FRAMES,
    RX_OFFSET, RegionRef, TX_OFFSET,
};
use ovsc_shm::time::Timebase;

use crate::mirror::MirrorWriter;

/// The daemon's region: the memory, a validated view of it, and its
/// generation.
pub struct HalRegion {
    region: Arc<SharedRegion>,
    /// A view of `region`, which this struct keeps mapped; never handed out
    /// with a lifetime longer than a borrow of `self`.
    view: RegionRef<'static>,
    generation: u64,
    /// The clock block's writer state, shared by every mirror of the region.
    mirror: MirrorWriter,
}

impl HalRegion {
    /// Creates and lays out a region of `REGION_SIZE` bytes: zeroed control
    /// blocks, a header with a random nonzero generation, this process's
    /// ID, architecture and timebase, the creation time and `version`.
    /// The header page is locked into memory (best effort) and committed.
    pub fn create(version: &str) -> io::Result<Arc<HalRegion>> {
        let region = SharedRegion::create(REGION_SIZE)?;
        // Nobody else can see the region yet, so the page can be touched
        // before the header is written.
        if let Err(e) = region.lock_range(0, PAGE_BYTES) {
            warn!("hal: cannot lock the region header in memory: {e}");
        }
        region.prefault_range(0, PAGE_BYTES);
        let generation = random_generation();
        let init = HeaderInit {
            daemon_generation: generation,
            daemon_pid: std::process::id(),
            arch: HOST_ARCH,
            timebase: host_timebase(),
            created_host_ns: local_now_ns(),
            daemon_version: HeaderInit::version_bytes(version),
        };
        // SAFETY: the mapping covers `region.len()` bytes and stays mapped as
        // long as `region`, which the HalRegion keeps next to the view. Its
        // handle has not been handed out, so nobody else accesses it yet.
        let view = unsafe { RegionRef::init(region.as_ptr(), region.len(), &init) }
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Arc::new(HalRegion { region, view, generation, mirror: MirrorWriter::default() }))
    }

    /// A view of the region.
    pub fn view(&self) -> RegionRef<'_> {
        self.view
    }

    /// What a welcome carries to let the driver map the region.
    pub fn handle(&self) -> RegionHandle {
        self.region.handle()
    }

    /// The random, nonzero generation in the header.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Rings for a device with `rx` receive and `tx` transmit channels: the
    /// region's first `rx` RX rings and first `tx` TX rings, each keeping
    /// the region alive. Their pages are locked into memory (best effort)
    /// and committed, so the real-time paths of both processes never fault
    /// on them. Fails if either count exceeds `MAX_CHANNELS`.
    pub fn external_rings(self: &Arc<Self>, rx: usize, tx: usize) -> io::Result<ExternalRings> {
        if rx > MAX_CHANNELS || tx > MAX_CHANNELS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{rx} rx and {tx} tx channels: the region holds {MAX_CHANNELS} of each"),
            ));
        }
        for (what, area, n) in [("rx", RX_OFFSET, rx), ("tx", TX_OFFSET, tx)] {
            let len = n * RING_BYTES;
            if let Err(e) = self.region.lock_range(area, len) {
                warn!("hal: cannot lock the {n} {what} rings in memory: {e}");
            }
            self.region.prefault_range(area, len);
        }
        let view = self.view();
        let ring = |slots: Option<NonNull<AtomicU64>>| -> io::Result<Arc<TimedRing>> {
            let slots = slots.ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
            let keep: Arc<dyn Send + Sync> = self.clone();
            // SAFETY: a ring of the region is RING_FRAMES initialised,
            // 8-byte aligned atomics inside the mapping, which `keep` keeps
            // alive; every process only accesses it atomically.
            Ok(Arc::new(unsafe { TimedRing::from_raw(slots, RING_FRAMES, keep) }))
        };
        Ok(ExternalRings {
            rx: (0..rx).map(|ch| ring(view.rx_slots(ch))).collect::<io::Result<_>>()?,
            tx: (0..tx).map(|ch| ring(view.tx_slots(ch))).collect::<io::Result<_>>()?,
        })
    }

    pub(crate) fn mirror_state(&self) -> &MirrorWriter {
        &self.mirror
    }
}

impl std::fmt::Debug for HalRegion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HalRegion")
            .field("base", &self.view.base())
            .field("len", &self.view.len())
            .field("generation", &format_args!("{:016x}", self.generation))
            .finish()
    }
}

/// A random nonzero u64. The standard library's hasher keys are seeded from
/// the OS's random source; time and process ID are mixed in as well.
fn random_generation() -> u64 {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(local_now_ns());
    h.write_u32(std::process::id());
    h.finish().max(1)
}

/// This host's Mach timebase.
#[cfg(target_os = "macos")]
fn host_timebase() -> Timebase {
    #[repr(C)]
    struct MachTimebaseInfo {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> std::ffi::c_int;
    }
    let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
    // SAFETY: `info` is a valid out-pointer.
    let kr = unsafe { mach_timebase_info(&mut info) };
    if kr == 0 && info.numer != 0 && info.denom != 0 {
        Timebase { numer: info.numer, denom: info.denom }
    } else {
        Timebase::NANOS
    }
}

/// Hosts other than macOS have no Mach ticks; times are nanoseconds.
#[cfg(not(target_os = "macos"))]
fn host_timebase() -> Timebase {
    Timebase::NANOS
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use ovsc_shm::layout::{LAYOUT_HASH, MAGIC};

    use super::*;

    #[test]
    fn regions_are_laid_out_and_distinct() {
        let a = HalRegion::create("ovsc 0.1.0").unwrap();
        let b = HalRegion::create("ovsc 0.1.0").unwrap();
        assert_ne!(a.generation(), 0);
        assert_ne!(a.generation(), b.generation());
        let h = a.view().header();
        assert_eq!((h.magic, h.layout_hash), (MAGIC, LAYOUT_HASH));
        assert_eq!(h.daemon_generation, a.generation());
        assert_eq!(h.daemon_pid, std::process::id());
        assert_eq!(h.arch, HOST_ARCH);
        assert_eq!(h.daemon_version(), "ovsc 0.1.0");
        assert!(h.created_host_ns > 0 && h.created_host_ns <= local_now_ns());
        assert_eq!(a.view().len(), REGION_SIZE);
        // The driver's mapping validates.
        let m = a.handle().map().unwrap();
        // SAFETY: the mapping lives until the end of the test.
        let v = unsafe { RegionRef::from_raw(m.as_ptr(), m.len()) }.unwrap();
        assert_eq!(v.header().daemon_generation, a.generation());
    }

    #[test]
    fn external_rings_are_region_rings() {
        let r = HalRegion::create("test").unwrap();
        let rings = r.external_rings(2, 3).unwrap();
        assert_eq!((rings.rx.len(), rings.tx.len()), (2, 3));
        assert_eq!(rings.rx[0].capacity(), RING_FRAMES);
        rings.rx[1].write_one(1000, 77);
        assert_eq!(r.view().rx(1).unwrap().read_one(1000), Some(77));
        r.view().tx(2).unwrap().write_one(5, -9);
        assert_eq!(rings.tx[2].read_one(5), Some(-9));
        // Each ring keeps the region alive.
        assert_eq!(Arc::strong_count(&r), 6);
        drop(rings);
        assert_eq!(Arc::strong_count(&r), 1);
        // At most 128 per direction.
        for (rx, tx) in [(MAX_CHANNELS + 1, 1), (1, MAX_CHANNELS + 1)] {
            let err = r.external_rings(rx, tx).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        }
        // The rings start at their compiled offsets.
        let rings = r.external_rings(1, 1).unwrap();
        rings.tx[0].write_one(3, 1);
        // SAFETY: 8-aligned offset inside the live region.
        let slot = unsafe { &*r.view().base().add(TX_OFFSET + 3 * 8).cast::<AtomicU64>() };
        assert_ne!(slot.load(Ordering::SeqCst), 0);
    }
}
