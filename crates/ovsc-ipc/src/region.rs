//! The shared region's memory: created by the daemon, handed to the driver,
//! mapped there and retired again (design sections 6, 10.1 and 11).
//!
//! * [`SharedRegion`] is the daemon's region: `mmap(MAP_ANON|MAP_SHARED)` on
//!   Unix, boxed with `xpc_shmem_create` right away on macOS (before any other
//!   VM operation can fragment it); 16 KiB-aligned heap memory elsewhere.
//! * [`RegionHandle`] is what travels in a welcome: the retained `xpc_shmem`
//!   on macOS, or the region itself in-process.
//! * [`MappedRegion`] is the driver's view. A mapping of its own (from
//!   `xpc_shmem_map`) can be neutralized in place and is unmapped on drop; a
//!   view of an in-process region just keeps the region alive.
//!
//! The memory is only ever accessed through atomics, by every process and
//! thread that maps it, so handing out raw pointers to it is sound as long
//! as the mapping lives.

use std::fmt;
use std::io;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

#[cfg(target_os = "macos")]
use std::ffi::c_void;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn xpc_shmem_create(region: *mut c_void, length: usize) -> *mut c_void;
    fn xpc_shmem_map(xshmem: *mut c_void, region: *mut *mut c_void) -> usize;
    fn xpc_retain(object: *mut c_void) -> *mut c_void;
    fn xpc_release(object: *mut c_void);
}

/// The alignment of heap-backed regions: the largest page size of any
/// supported host.
#[cfg(not(unix))]
const HEAP_ALIGN: usize = 16 * 1024;

/// The host's page size, cached so that [`MappedRegion::touch`] makes no
/// system call. Filled in when a region is created or mapped.
static PAGE_SIZE: AtomicUsize = AtomicUsize::new(0);

fn page_size() -> usize {
    let cached = PAGE_SIZE.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    #[cfg(unix)]
    let page = {
        // SAFETY: sysconf has no preconditions.
        let v = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        usize::try_from(v).ok().filter(|p| *p >= 4096 && p.is_power_of_two()).unwrap_or(4096)
    };
    #[cfg(not(unix))]
    let page = 4096;
    PAGE_SIZE.store(page, Ordering::Relaxed);
    page
}

/// `fetch_add(0)` on the first word of every page of `[off, off + len)`
/// inside the `region_len` bytes at `base`, which commits those pages
/// writable. `base` must be page-aligned and valid for `region_len` bytes.
fn touch_pages(base: NonNull<u8>, region_len: usize, off: usize, len: usize) {
    let page = match PAGE_SIZE.load(Ordering::Relaxed) {
        0 => 4096,
        p => p,
    };
    let end = off.saturating_add(len).min(region_len);
    let mut at = off - off % page;
    while at < end && at + size_of::<AtomicU64>() <= region_len {
        // SAFETY: `at` is a multiple of the page size inside the mapping, so
        // the word is in bounds and 8-byte aligned; the region is only ever
        // accessed through atomics.
        let word = unsafe { &*base.as_ptr().add(at).cast::<AtomicU64>() };
        word.fetch_add(0, Ordering::Relaxed);
        at += page;
    }
}

fn check_range(len: usize, off: usize, n: usize) -> io::Result<()> {
    match off.checked_add(n) {
        Some(end) if end <= len => Ok(()),
        _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "range outside the region")),
    }
}

/// A retained `xpc_shmem` object (macOS).
#[cfg(target_os = "macos")]
pub struct XpcShmem(NonNull<c_void>);

#[cfg(target_os = "macos")]
impl XpcShmem {
    /// Takes over one reference to `object`.
    ///
    /// # Safety
    /// `object` must be a non-null `xpc_shmem` object the caller owns a
    /// reference to.
    unsafe fn from_owned(object: NonNull<c_void>) -> Self {
        XpcShmem(object)
    }

    /// Retains `object`, which the caller only borrows.
    ///
    /// # Safety
    /// `object` must be a live `xpc_shmem` object.
    pub(crate) unsafe fn retain(object: NonNull<c_void>) -> Self {
        // SAFETY: the caller guarantees a live XPC object.
        unsafe { xpc_retain(object.as_ptr()) };
        XpcShmem(object)
    }

    /// The object, still owned by `self`.
    pub(crate) fn as_raw(&self) -> *mut c_void {
        self.0.as_ptr()
    }
}

#[cfg(target_os = "macos")]
impl Clone for XpcShmem {
    fn clone(&self) -> Self {
        // SAFETY: `self` holds a reference, so the object is live.
        unsafe { XpcShmem::retain(self.0) }
    }
}

#[cfg(target_os = "macos")]
impl Drop for XpcShmem {
    fn drop(&mut self) {
        // SAFETY: releases the reference `self` owns.
        unsafe { xpc_release(self.0.as_ptr()) };
    }
}

#[cfg(target_os = "macos")]
impl fmt::Debug for XpcShmem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "XpcShmem({:p})", self.0)
    }
}

// SAFETY: XPC objects are reference-counted thread-safely, and an xpc_shmem
// is immutable.
#[cfg(target_os = "macos")]
unsafe impl Send for XpcShmem {}
// SAFETY: as above.
#[cfg(target_os = "macos")]
unsafe impl Sync for XpcShmem {}

/// The daemon's shared region.
pub struct SharedRegion {
    ptr: NonNull<u8>,
    len: usize,
    #[cfg(target_os = "macos")]
    shmem: XpcShmem,
}

// SAFETY: the region is plain memory accessed only through atomics; the
// pointer stays valid until drop.
unsafe impl Send for SharedRegion {}
// SAFETY: as above.
unsafe impl Sync for SharedRegion {}

impl SharedRegion {
    /// Creates a zeroed region of `len` bytes. Pages are committed only when
    /// touched.
    pub fn create(len: usize) -> io::Result<Arc<SharedRegion>> {
        if len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty region"));
        }
        page_size();
        // SAFETY: an anonymous mapping has no preconditions; failure is
        // checked.
        #[cfg(unix)]
        let ptr = unsafe {
            let p = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANON | libc::MAP_SHARED,
                -1,
                0,
            );
            if p == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            NonNull::new_unchecked(p.cast::<u8>())
        };
        #[cfg(not(unix))]
        let ptr = {
            let layout = Self::heap_layout(len)?;
            // SAFETY: the layout has a nonzero size.
            let p = unsafe { std::alloc::alloc_zeroed(layout) };
            NonNull::new(p).ok_or_else(|| io::Error::from(io::ErrorKind::OutOfMemory))?
        };
        #[cfg(target_os = "macos")]
        {
            // Boxed immediately, before any other VM operation (xpc_shmem_create).
            // SAFETY: `ptr` is a fresh MAP_SHARED mapping of `len` bytes.
            let shmem = unsafe { xpc_shmem_create(ptr.as_ptr().cast(), len) };
            let Some(shmem) = NonNull::new(shmem) else {
                // SAFETY: unmaps the mapping made above.
                unsafe { libc::munmap(ptr.as_ptr().cast(), len) };
                return Err(io::Error::other("xpc_shmem_create failed"));
            };
            // SAFETY: xpc_shmem_create returns a new reference.
            let shmem = unsafe { XpcShmem::from_owned(shmem) };
            Ok(Arc::new(SharedRegion { ptr, len, shmem }))
        }
        #[cfg(not(target_os = "macos"))]
        Ok(Arc::new(SharedRegion { ptr, len }))
    }

    #[cfg(not(unix))]
    fn heap_layout(len: usize) -> io::Result<std::alloc::Layout> {
        let size = len.checked_next_multiple_of(HEAP_ALIGN).unwrap_or(usize::MAX);
        std::alloc::Layout::from_size_align(size, HEAP_ALIGN)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "region too large"))
    }

    /// The region's base address.
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    /// The region's length in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Always false: a region is never empty.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// What a welcome carries to let a peer map this region.
    pub fn handle(self: &Arc<Self>) -> RegionHandle {
        #[cfg(target_os = "macos")]
        return RegionHandle::Xpc(self.shmem.clone());
        #[cfg(not(target_os = "macos"))]
        return RegionHandle::Local(self.clone());
    }

    /// A handle for a peer in the same process on every OS: mapping it
    /// shares this memory and holds a strong reference, instead of making a
    /// separate xpc mapping as [`SharedRegion::handle`] does on macOS. Tests
    /// use it to watch when a peer lets a region go.
    pub fn local_handle(self: &Arc<Self>) -> RegionHandle {
        RegionHandle::Local(self.clone())
    }

    /// Locks `[off, off + len)` into memory (`mlock`), so the real-time
    /// paths never page-fault on it. Best effort: callers log a failure and
    /// carry on. Unsupported on heap-backed regions.
    pub fn lock_range(&self, off: usize, len: usize) -> io::Result<()> {
        check_range(self.len, off, len)?;
        if len == 0 {
            return Ok(());
        }
        #[cfg(unix)]
        {
            let page = page_size();
            let start = off - off % page;
            // SAFETY: the range lies inside this mapping; mlock does not
            // change its contents.
            let r = unsafe { libc::mlock(self.ptr.as_ptr().add(start).cast(), off + len - start) };
            if r != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(not(unix))]
        Err(io::Error::new(io::ErrorKind::Unsupported, "mlock on a heap region"))
    }

    /// Commits the pages of `[off, off + len)` (clamped to the region) by
    /// touching one word per page.
    pub fn prefault_range(&self, off: usize, len: usize) {
        touch_pages(self.ptr, self.len, off, len);
    }
}

impl Drop for SharedRegion {
    fn drop(&mut self) {
        // SAFETY: the mapping was made in `create` and nothing can use it
        // after the last reference is gone (every view holds an Arc or a
        // mapping of its own).
        #[cfg(unix)]
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.len);
        }
        #[cfg(not(unix))]
        if let Ok(layout) = Self::heap_layout(self.len) {
            // SAFETY: allocated in `create` with this layout.
            unsafe { std::alloc::dealloc(self.ptr.as_ptr(), layout) };
        }
    }
}

impl fmt::Debug for SharedRegion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedRegion").field("ptr", &self.ptr).field("len", &self.len).finish()
    }
}

/// A region as it travels between processes (or within one).
///
/// Two handles are equal when they refer to the same object.
#[derive(Clone, Debug)]
pub enum RegionHandle {
    /// A retained `xpc_shmem`, mapped with `xpc_shmem_map`.
    #[cfg(target_os = "macos")]
    Xpc(XpcShmem),
    /// The region itself, for the in-process transport.
    Local(Arc<SharedRegion>),
}

impl PartialEq for RegionHandle {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            #[cfg(target_os = "macos")]
            (RegionHandle::Xpc(a), RegionHandle::Xpc(b)) => a.0 == b.0,
            (RegionHandle::Local(a), RegionHandle::Local(b)) => Arc::ptr_eq(a, b),
            #[cfg(target_os = "macos")]
            _ => false,
        }
    }
}

impl Eq for RegionHandle {}

impl RegionHandle {
    /// Maps the region into this process.
    pub fn map(&self) -> io::Result<MappedRegion> {
        page_size();
        match self {
            #[cfg(target_os = "macos")]
            RegionHandle::Xpc(shmem) => {
                let mut addr: *mut c_void = std::ptr::null_mut();
                // SAFETY: `shmem` is a live xpc_shmem; `addr` is a valid
                // out-pointer.
                let len = unsafe { xpc_shmem_map(shmem.as_raw(), &mut addr) };
                match NonNull::new(addr.cast::<u8>()) {
                    Some(ptr) if len > 0 => Ok(MappedRegion { ptr, len, owner: Owner::Munmap }),
                    _ => Err(io::Error::other("xpc_shmem_map failed")),
                }
            }
            RegionHandle::Local(region) => Ok(MappedRegion {
                ptr: region.ptr,
                len: region.len,
                owner: Owner::Keep { _region: region.clone() },
            }),
        }
    }
}

enum Owner {
    /// A mapping of this process's own, unmapped on drop.
    #[cfg(target_os = "macos")]
    Munmap,
    /// A view of an in-process region, kept alive.
    Keep { _region: Arc<SharedRegion> },
}

/// The driver's view of a region.
pub struct MappedRegion {
    ptr: NonNull<u8>,
    len: usize,
    owner: Owner,
}

// SAFETY: the mapping is plain memory accessed only through atomics, valid
// until drop.
unsafe impl Send for MappedRegion {}
// SAFETY: as above.
unsafe impl Sync for MappedRegion {}

impl MappedRegion {
    /// The mapping's base address.
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    /// The mapping's length in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Always false: a mapping is never empty.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Commits the pages of `[off, off + len)` (clamped to the mapping) by a
    /// `fetch_add(0)` on one word per page. Makes no system call.
    pub fn touch(&self, off: usize, len: usize) {
        touch_pages(self.ptr, self.len, off, len);
    }

    /// Detaches this process from the region without unmapping the range:
    /// an anonymous private mapping replaces it in place
    /// (`mmap(MAP_FIXED|MAP_ANON|MAP_PRIVATE)`, atomic in XNU). Readers that
    /// still hold the address then read zeros, which are stale ring tags and
    /// so silence, and their writes go to private pages. The peer's memory is
    /// untouched.
    ///
    /// A no-op for a view of an in-process region. On failure the old
    /// mapping stays.
    pub fn neutralize(&self) -> io::Result<()> {
        match self.owner {
            #[cfg(target_os = "macos")]
            Owner::Munmap => {
                // SAFETY: replaces this process's own mapping of exactly this
                // range; the range stays mapped (now private and zeroed), so
                // concurrent atomic accesses stay valid.
                let p = unsafe {
                    libc::mmap(
                        self.ptr.as_ptr().cast(),
                        self.len,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_FIXED | libc::MAP_ANON | libc::MAP_PRIVATE,
                        -1,
                        0,
                    )
                };
                if p == libc::MAP_FAILED {
                    return Err(io::Error::last_os_error());
                }
                if p != self.ptr.as_ptr().cast() {
                    return Err(io::Error::other("MAP_FIXED placed the mapping elsewhere"));
                }
                Ok(())
            }
            Owner::Keep { .. } => Ok(()),
        }
    }
}

impl Drop for MappedRegion {
    fn drop(&mut self) {
        match self.owner {
            #[cfg(target_os = "macos")]
            Owner::Munmap => {
                // SAFETY: unmaps this process's own mapping, which nothing
                // uses after the MappedRegion is gone.
                unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
            }
            Owner::Keep { .. } => {}
        }
    }
}

impl fmt::Debug for MappedRegion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MappedRegion").field("ptr", &self.ptr).field("len", &self.len).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(base: *mut u8, off: usize) -> &'static AtomicU64 {
        // SAFETY: the tests use 8-aligned offsets inside live regions.
        unsafe { &*base.add(off).cast::<AtomicU64>() }
    }

    #[test]
    fn create_rejects_an_empty_region() {
        assert_eq!(SharedRegion::create(0).unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn handles_compare_by_identity() {
        let a = SharedRegion::create(64 * 1024).unwrap();
        let b = SharedRegion::create(64 * 1024).unwrap();
        assert_eq!(a.handle(), a.handle());
        assert_eq!(a.handle(), a.handle().clone());
        assert_ne!(a.handle(), b.handle());
    }

    #[test]
    fn mapping_sees_the_region_and_touch_keeps_contents() {
        let r = SharedRegion::create(256 * 1024).unwrap();
        assert_eq!(r.as_ptr() as usize % 4096, 0);
        word(r.as_ptr(), 0).store(7, Ordering::SeqCst);
        word(r.as_ptr(), 0x8000).store(9, Ordering::SeqCst);
        let m = r.handle().map().unwrap();
        assert!(m.len() >= r.len());
        assert_eq!(word(m.as_ptr(), 0).load(Ordering::SeqCst), 7);
        m.touch(0, m.len());
        r.prefault_range(0, r.len());
        // Clamped, never out of bounds.
        m.touch(m.len() - 1, 1 << 30);
        m.touch(usize::MAX - 8, 100);
        r.prefault_range(r.len() + 10, 10);
        assert_eq!(word(m.as_ptr(), 0x8000).load(Ordering::SeqCst), 9);
        word(m.as_ptr(), 16).store(5, Ordering::SeqCst);
        assert_eq!(word(r.as_ptr(), 16).load(Ordering::SeqCst), 5);
        // The region outlives its owner while mapped.
        let p = m.as_ptr();
        drop(r);
        assert_eq!(word(p, 0).load(Ordering::SeqCst), 7);
    }

    #[test]
    fn small_regions_touch_safely() {
        let r = SharedRegion::create(100).unwrap();
        r.prefault_range(0, 100);
        r.handle().map().unwrap().touch(0, 100);
        let r = SharedRegion::create(4).unwrap();
        // Too small for a word: nothing is touched.
        r.prefault_range(0, 4);
    }

    #[test]
    fn lock_range_checks_its_range() {
        let r = SharedRegion::create(64 * 1024).unwrap();
        let err = r.lock_range(r.len() - 1, 2).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(r.lock_range(usize::MAX, 2).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        r.lock_range(r.len(), 0).unwrap();
        // Best effort: a memlock limit may refuse it, but never as a range
        // error.
        if let Err(e) = r.lock_range(100, 5000) {
            assert_ne!(e.kind(), io::ErrorKind::InvalidInput, "{e}");
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn neutralize_is_a_no_op_on_local_views() {
        let r = SharedRegion::create(64 * 1024).unwrap();
        word(r.as_ptr(), 8).store(3, Ordering::SeqCst);
        let m = r.handle().map().unwrap();
        m.neutralize().unwrap();
        assert_eq!(word(m.as_ptr(), 8).load(Ordering::SeqCst), 3);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn neutralize_detaches_an_own_mapping() {
        let r = SharedRegion::create(64 * 1024).unwrap();
        word(r.as_ptr(), 8).store(3, Ordering::SeqCst);
        let m = r.handle().map().unwrap();
        assert_eq!(word(m.as_ptr(), 8).load(Ordering::SeqCst), 3);
        m.neutralize().unwrap();
        assert_eq!(word(m.as_ptr(), 8).load(Ordering::SeqCst), 0);
        word(m.as_ptr(), 8).store(4, Ordering::SeqCst);
        assert_eq!(word(r.as_ptr(), 8).load(Ordering::SeqCst), 3);
    }
}
