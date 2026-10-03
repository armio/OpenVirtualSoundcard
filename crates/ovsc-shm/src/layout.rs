//! The shared region: constants, header and a validated view.
//!
//! The daemon creates one region per process. It holds a header, the clock
//! block, the status blocks and the IO trace in its first 64 KiB, then one
//! ring per channel and direction:
//!
//! ```text
//! 0x0000_0000  Header          256 B   written once by the daemon
//! 0x0000_0400  ClockBlock      128 B   daemon clock mirror -> plug-in
//! 0x0000_0800  DaemonStatus    256 B   daemon -> plug-in
//! 0x0000_0C00  PluginStatus    512 B   plug-in -> daemon
//! 0x0000_1000  IoTraceHeader   128 B   plug-in -> daemon
//! 0x0000_1080  IoTraceEntry    64 x 64 B
//! 0x0001_0000  RX rings        128 x 256 KiB   daemon writes, plug-in reads
//! 0x0201_0000  TX rings        128 x 256 KiB   plug-in writes, daemon reads
//! 0x0401_0000  end
//! ```
//!
//! Every field is a fixed-size integer at a fixed offset, so the layout is
//! the same on arm64 and x86_64. A mapper never trusts offsets or sizes read
//! from the region: [`RegionRef::from_raw`] compares every header field with
//! the compiled constants, and the accessors only use those constants.
//! [`LAYOUT_HASH`] covers every constant, size and field offset, so two
//! builds agree on it only if they agree on the whole layout.

#![allow(unsafe_code)]

use core::fmt;
use core::marker::PhantomData;
use core::mem::{offset_of, size_of};
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicU64, Ordering, fence};

use crate::clock::ClockBlock;
use crate::ring::RingRef;
use crate::status::{DaemonStatus, IoTraceEntry, IoTraceHeader, PluginStatus};
use crate::time::Timebase;

/// `"OVSCSHM1"`, read as a little-endian u64.
pub const MAGIC: u64 = u64::from_le_bytes(*b"OVSCSHM1");
pub const LAYOUT_VERSION: u32 = 1;
/// Rings per direction.
pub const MAX_CHANNELS: usize = 128;
/// Samples per ring: 682 ms at 48 kHz, 170 ms at 192 kHz. The same as
/// `ovsc-core`'s default ring capacity.
pub const RING_FRAMES: usize = 32768;
pub const RING_BYTES: usize = RING_FRAMES * size_of::<AtomicU64>();
pub const HEADER_OFFSET: usize = 0x0000;
pub const CLOCK_OFFSET: usize = 0x0400;
pub const DAEMON_STATUS_OFFSET: usize = 0x0800;
pub const PLUGIN_STATUS_OFFSET: usize = 0x0C00;
pub const IO_TRACE_OFFSET: usize = 0x1000;
/// The first IO trace entry, right after the trace header.
pub const IO_TRACE_ENTRIES_OFFSET: usize = IO_TRACE_OFFSET + size_of::<IoTraceHeader>();
/// RX ring `c` starts at `RX_OFFSET + c * RING_BYTES`.
pub const RX_OFFSET: usize = 0x0001_0000;
/// TX ring `c` starts at `TX_OFFSET + c * RING_BYTES`.
pub const TX_OFFSET: usize = 0x0201_0000;
/// 64 MiB + 64 KiB of address space. Only the pages of active channels are
/// ever touched, so only those are committed.
pub const REGION_SIZE: usize = 0x0401_0000;
pub const IO_TRACE_ENTRIES: usize = 64;
/// Frames per IO operation the plug-in handles; more are zero-filled or
/// dropped, and counted.
pub const IO_FRAMES_CAP: usize = 16384;

/// The page size of Apple silicon, the largest page size of any supported
/// host. REGION_SIZE and every block boundary are multiples of it.
pub const PAGE_BYTES: usize = 0x4000;
/// Required alignment of the region's base address: 4 KiB, the smallest page
/// size of any supported host, so that every mapping qualifies.
pub const REGION_ALIGN: usize = 0x1000;

/// `Header::arch` of an arm64 daemon.
pub const ARCH_ARM64: u32 = 1;
/// `Header::arch` of an x86_64 daemon.
pub const ARCH_X86_64: u32 = 2;
/// `Header::arch` for this build (0 on other architectures).
pub const HOST_ARCH: u32 = if cfg!(target_arch = "aarch64") {
    ARCH_ARM64
} else if cfg!(target_arch = "x86_64") {
    ARCH_X86_64
} else {
    0
};

/// The region header (256 bytes). The daemon writes it once, before the
/// region is shared; it is read-only afterwards.
#[repr(C, align(128))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// [`MAGIC`].
    pub magic: u64,
    /// [`LAYOUT_VERSION`].
    pub layout_version: u32,
    /// `size_of::<Header>()`.
    pub header_size: u32,
    /// [`REGION_SIZE`].
    pub region_size: u64,
    /// [`LAYOUT_HASH`].
    pub layout_hash: u64,
    /// [`MAX_CHANNELS`].
    pub max_channels: u32,
    /// [`RING_FRAMES`].
    pub ring_frames: u32,
    /// [`RX_OFFSET`].
    pub rx_offset: u64,
    /// [`TX_OFFSET`].
    pub tx_offset: u64,
    /// [`CLOCK_OFFSET`].
    pub clock_offset: u32,
    /// [`DAEMON_STATUS_OFFSET`].
    pub daemon_status_offset: u32,
    /// [`PLUGIN_STATUS_OFFSET`].
    pub plugin_status_offset: u32,
    /// [`IO_TRACE_OFFSET`].
    pub io_trace_offset: u32,
    /// Random and nonzero; identifies the daemon process that created the
    /// region.
    pub daemon_generation: u64,
    pub daemon_pid: u32,
    /// [`ARCH_ARM64`] or [`ARCH_X86_64`].
    pub arch: u32,
    /// The daemon's Mach timebase. Informational: shared times are in ns.
    pub timebase_numer: u32,
    pub timebase_denom: u32,
    /// Host nanoseconds when the region was created.
    pub created_host_ns: u64,
    /// UTF-8, NUL-padded.
    pub daemon_version: [u8; 32],
    pub(crate) _reserved: [u64; 15],
}

impl Header {
    /// The header the creator writes.
    pub const fn new(h: &HeaderInit) -> Self {
        Self {
            magic: MAGIC,
            layout_version: LAYOUT_VERSION,
            header_size: size_of::<Header>() as u32,
            region_size: REGION_SIZE as u64,
            layout_hash: LAYOUT_HASH,
            max_channels: MAX_CHANNELS as u32,
            ring_frames: RING_FRAMES as u32,
            rx_offset: RX_OFFSET as u64,
            tx_offset: TX_OFFSET as u64,
            clock_offset: CLOCK_OFFSET as u32,
            daemon_status_offset: DAEMON_STATUS_OFFSET as u32,
            plugin_status_offset: PLUGIN_STATUS_OFFSET as u32,
            io_trace_offset: IO_TRACE_OFFSET as u32,
            daemon_generation: h.daemon_generation,
            daemon_pid: h.daemon_pid,
            arch: h.arch,
            timebase_numer: h.timebase.numer,
            timebase_denom: h.timebase.denom,
            created_host_ns: h.created_host_ns,
            daemon_version: h.daemon_version,
            _reserved: [0; 15],
        }
    }

    /// Checks every layout field against the compiled constants.
    pub fn validate(&self) -> Result<(), LayoutError> {
        if self.magic != MAGIC {
            return Err(LayoutError::BadMagic(self.magic));
        }
        if self.layout_version != LAYOUT_VERSION {
            return Err(LayoutError::BadVersion(self.layout_version));
        }
        if self.layout_hash != LAYOUT_HASH {
            return Err(LayoutError::BadHash(self.layout_hash));
        }
        let geometry_ok = self.header_size as usize == size_of::<Header>()
            && self.region_size == REGION_SIZE as u64
            && self.max_channels as usize == MAX_CHANNELS
            && self.ring_frames as usize == RING_FRAMES
            && self.rx_offset == RX_OFFSET as u64
            && self.tx_offset == TX_OFFSET as u64
            && self.clock_offset as usize == CLOCK_OFFSET
            && self.daemon_status_offset as usize == DAEMON_STATUS_OFFSET
            && self.plugin_status_offset as usize == PLUGIN_STATUS_OFFSET
            && self.io_trace_offset as usize == IO_TRACE_OFFSET;
        if !geometry_ok {
            return Err(LayoutError::BadGeometry);
        }
        Ok(())
    }

    /// The daemon's Mach timebase.
    pub fn timebase(&self) -> Timebase {
        Timebase { numer: self.timebase_numer, denom: self.timebase_denom }
    }

    /// The daemon's version string, up to the first NUL or invalid UTF-8.
    pub fn daemon_version(&self) -> &str {
        let v = &self.daemon_version;
        let end = v.iter().position(|&b| b == 0).unwrap_or(v.len());
        let v = &v[..end];
        match core::str::from_utf8(v) {
            Ok(s) => s,
            // The prefix up to the error is valid UTF-8 by definition.
            Err(e) => core::str::from_utf8(&v[..e.valid_up_to()]).unwrap_or(""),
        }
    }
}

/// What the creator puts in the header besides the layout constants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderInit {
    /// Random and nonzero.
    pub daemon_generation: u64,
    pub daemon_pid: u32,
    /// [`HOST_ARCH`] of the daemon.
    pub arch: u32,
    pub timebase: Timebase,
    pub created_host_ns: u64,
    /// See [`HeaderInit::version_bytes`].
    pub daemon_version: [u8; 32],
}

impl HeaderInit {
    /// `version` as a NUL-padded field, truncated to 32 bytes (at a character
    /// boundary).
    pub fn version_bytes(version: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        let mut end = version.len().min(out.len());
        while !version.is_char_boundary(end) {
            end -= 1;
        }
        out[..end].copy_from_slice(&version.as_bytes()[..end]);
        out
    }
}

/// Why a memory range is not a usable region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutError {
    /// The range is shorter than [`REGION_SIZE`].
    TooSmall { need: usize, got: usize },
    /// The base address is null or not aligned to [`REGION_ALIGN`].
    Misaligned,
    /// The header doesn't start with [`MAGIC`]: not a region.
    BadMagic(u64),
    /// A region of another layout version.
    BadVersion(u32),
    /// A region of the same version whose layout differs from this build's.
    BadHash(u64),
    /// A header field disagrees with this build's constants.
    BadGeometry,
    /// The daemon's heartbeat is too far from the mapper's own host clock:
    /// the two processes do not share a clock base. Found by the driver when
    /// it attaches, not by [`RegionRef::from_raw`].
    ClockBase { heartbeat_ns: u64, now_ns: u64 },
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            LayoutError::TooSmall { need, got } => {
                write!(f, "shared region too small: {got} bytes, need {need}")
            }
            LayoutError::Misaligned => {
                write!(f, "shared region not aligned to {REGION_ALIGN} bytes")
            }
            LayoutError::BadMagic(m) => {
                write!(f, "not an OpenVirtualSoundcard shared region (magic {m:#x})")
            }
            LayoutError::BadVersion(v) => {
                write!(f, "shared region layout version {v}, expected {LAYOUT_VERSION}")
            }
            LayoutError::BadHash(h) => {
                write!(f, "shared region layout hash {h:#x}, expected {LAYOUT_HASH:#x}")
            }
            LayoutError::BadGeometry => write!(f, "shared region header disagrees with the layout"),
            LayoutError::ClockBase { heartbeat_ns, now_ns } => write!(
                f,
                "shared region clock base differs: daemon heartbeat at {heartbeat_ns} ns, \
                 local clock at {now_ns} ns"
            ),
        }
    }
}

impl core::error::Error for LayoutError {}

/// A validated view of a region.
///
/// It is a pointer plus a length: copying it is free, and it can be shared
/// between threads, because everything it hands out is either the read-only
/// header or made of atomics.
#[derive(Clone, Copy)]
pub struct RegionRef<'a> {
    base: NonNull<u8>,
    len: usize,
    _memory: PhantomData<&'a [AtomicU64]>,
}

// SAFETY: a RegionRef only gives out shared references to atomics and to the
// header, which nobody writes while views exist (the contract of `init` and
// `from_raw`). Both are safe to use from any thread.
unsafe impl Send for RegionRef<'_> {}
// SAFETY: as above.
unsafe impl Sync for RegionRef<'_> {}

impl<'a> RegionRef<'a> {
    /// Lays out a new region at `base`: zeroes the control blocks (the first
    /// 64 KiB, up to [`RX_OFFSET`]), writes the header, then issues a Release
    /// fence. The rings are left untouched, so their pages stay uncommitted;
    /// in freshly mapped memory they read as silence.
    ///
    /// # Safety
    ///
    /// `base` must be valid for reads and writes of `len` bytes for `'a`, and
    /// no other thread or process may access the memory until `init` returns
    /// (the region is shared afterwards). After that, the same rules as for
    /// [`RegionRef::from_raw`] apply.
    pub unsafe fn init(base: *mut u8, len: usize, h: &HeaderInit) -> Result<Self, LayoutError> {
        check_range(base, len)?;
        // SAFETY: `check_range` showed that the range covers REGION_SIZE
        // bytes at a suitably aligned base; the caller guarantees exclusive
        // access, so plain writes are fine.
        unsafe {
            ptr::write_bytes(base, 0, RX_OFFSET);
            ptr::write(base.add(HEADER_OFFSET).cast::<Header>(), Header::new(h));
        }
        fence(Ordering::Release);
        // SAFETY: the caller's guarantees cover `from_raw`'s.
        unsafe { Self::from_raw(base, len) }
    }

    /// Validates the region at `base` and returns a view of it.
    ///
    /// Checks that `len` covers [`REGION_SIZE`], that `base` is aligned to
    /// [`REGION_ALIGN`], and that the magic, version, layout hash and every
    /// geometry field of the header match this build. The view never uses
    /// offsets read from the region.
    ///
    /// # Safety
    ///
    /// `base` must be valid for reads and writes of `len` bytes for `'a`
    /// (the mapping must outlive every view). Other threads and processes
    /// may access the region concurrently, but only through atomics, and
    /// nobody may write the header while views exist.
    pub unsafe fn from_raw(base: *mut u8, len: usize) -> Result<Self, LayoutError> {
        let base = check_range(base, len)?;
        // SAFETY: the range covers a whole header at an aligned address. A
        // volatile copy validates a stable snapshot even if the writer
        // misbehaves.
        let header = unsafe { ptr::read_volatile(base.as_ptr().cast::<Header>()) };
        header.validate()?;
        Ok(Self { base, len, _memory: PhantomData })
    }

    /// A reference to the `T` at `offset`.
    ///
    /// Callers pass compiled offsets: `offset + size_of::<T>() <= RX_OFFSET`,
    /// aligned for `T` (checked at compile time below).
    #[inline]
    fn at<T>(&self, offset: usize) -> &'a T {
        debug_assert!(offset + size_of::<T>() <= self.len);
        debug_assert!(offset % align_of::<T>() == 0);
        // SAFETY: the region was validated to cover REGION_SIZE bytes at an
        // address aligned to REGION_ALIGN, the offsets are in bounds and
        // aligned, and every T used here is either made of atomics or the
        // read-only header.
        unsafe { &*self.base.as_ptr().add(offset).cast::<T>() }
    }

    /// The header.
    #[inline]
    pub fn header(&self) -> &'a Header {
        self.at(HEADER_OFFSET)
    }

    /// The clock block.
    #[inline]
    pub fn clock(&self) -> &'a ClockBlock {
        self.at(CLOCK_OFFSET)
    }

    /// The daemon's status block.
    #[inline]
    pub fn daemon(&self) -> &'a DaemonStatus {
        self.at(DAEMON_STATUS_OFFSET)
    }

    /// The plug-in's status block.
    #[inline]
    pub fn plugin(&self) -> &'a PluginStatus {
        self.at(PLUGIN_STATUS_OFFSET)
    }

    /// The IO trace header and its entries.
    #[inline]
    pub fn io_trace(&self) -> (&'a IoTraceHeader, &'a [IoTraceEntry; IO_TRACE_ENTRIES]) {
        (self.at(IO_TRACE_OFFSET), self.at(IO_TRACE_ENTRIES_OFFSET))
    }

    /// The RX ring of channel `ch` (daemon writes, plug-in reads), or `None`
    /// if `ch >= MAX_CHANNELS`.
    #[inline]
    pub fn rx(&self, ch: usize) -> Option<RingRef<'a>> {
        self.ring(RX_OFFSET, ch)
    }

    /// The TX ring of channel `ch` (plug-in writes, daemon reads), or `None`
    /// if `ch >= MAX_CHANNELS`.
    #[inline]
    pub fn tx(&self, ch: usize) -> Option<RingRef<'a>> {
        self.ring(TX_OFFSET, ch)
    }

    /// The first of the [`RING_FRAMES`] slots of RX ring `ch`, or `None` if
    /// `ch >= MAX_CHANNELS`.
    #[inline]
    pub fn rx_slots(&self, ch: usize) -> Option<NonNull<AtomicU64>> {
        self.slots(RX_OFFSET, ch)
    }

    /// The first of the [`RING_FRAMES`] slots of TX ring `ch`, or `None` if
    /// `ch >= MAX_CHANNELS`.
    #[inline]
    pub fn tx_slots(&self, ch: usize) -> Option<NonNull<AtomicU64>> {
        self.slots(TX_OFFSET, ch)
    }

    /// The base address of the region.
    #[inline]
    pub fn base(&self) -> *mut u8 {
        self.base.as_ptr()
    }

    /// The length of the mapping (at least [`REGION_SIZE`]).
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Always false: a region is never empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        false
    }

    #[inline]
    fn slots(&self, area: usize, ch: usize) -> Option<NonNull<AtomicU64>> {
        if ch >= MAX_CHANNELS {
            return None;
        }
        // SAFETY: area + (MAX_CHANNELS - 1) * RING_BYTES + RING_BYTES is at
        // most REGION_SIZE (checked at compile time), which the region
        // covers; the result is in bounds and therefore not null.
        Some(unsafe { self.base.add(area + ch * RING_BYTES).cast::<AtomicU64>() })
    }

    #[inline]
    fn ring(&self, area: usize, ch: usize) -> Option<RingRef<'a>> {
        let slots = self.slots(area, ch)?;
        // SAFETY: the ring's RING_FRAMES slots lie inside the region (see
        // `slots`) at an 8-byte aligned address, and are only ever accessed
        // as atomics.
        let slots = unsafe { core::slice::from_raw_parts(slots.as_ptr(), RING_FRAMES) };
        RingRef::new(slots)
    }
}

impl fmt::Debug for RegionRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegionRef").field("base", &self.base).field("len", &self.len).finish()
    }
}

/// Checks the size and alignment of a candidate region.
fn check_range(base: *mut u8, len: usize) -> Result<NonNull<u8>, LayoutError> {
    if len < REGION_SIZE {
        return Err(LayoutError::TooSmall { need: REGION_SIZE, got: len });
    }
    let base = NonNull::new(base).ok_or(LayoutError::Misaligned)?;
    if base.as_ptr() as usize % REGION_ALIGN != 0 {
        return Err(LayoutError::Misaligned);
    }
    Ok(base)
}

/// FNV-1a 64 over the little-endian bytes of `words`.
const fn fnv1a64(words: &[u64]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < words.len() {
        let bytes = words[i].to_le_bytes();
        let mut j = 0;
        while j < bytes.len() {
            hash ^= bytes[j] as u64;
            hash = hash.wrapping_mul(0x0100_0000_01b3);
            j += 1;
        }
        i += 1;
    }
    hash
}

/// Everything the layout depends on, in a fixed order.
const LAYOUT_WORDS: &[u64] = &[
    LAYOUT_VERSION as u64,
    MAX_CHANNELS as u64,
    RING_FRAMES as u64,
    HEADER_OFFSET as u64,
    CLOCK_OFFSET as u64,
    DAEMON_STATUS_OFFSET as u64,
    PLUGIN_STATUS_OFFSET as u64,
    IO_TRACE_OFFSET as u64,
    IO_TRACE_ENTRIES_OFFSET as u64,
    RX_OFFSET as u64,
    TX_OFFSET as u64,
    REGION_SIZE as u64,
    IO_TRACE_ENTRIES as u64,
    // Header
    size_of::<Header>() as u64,
    offset_of!(Header, magic) as u64,
    offset_of!(Header, layout_version) as u64,
    offset_of!(Header, header_size) as u64,
    offset_of!(Header, region_size) as u64,
    offset_of!(Header, layout_hash) as u64,
    offset_of!(Header, max_channels) as u64,
    offset_of!(Header, ring_frames) as u64,
    offset_of!(Header, rx_offset) as u64,
    offset_of!(Header, tx_offset) as u64,
    offset_of!(Header, clock_offset) as u64,
    offset_of!(Header, daemon_status_offset) as u64,
    offset_of!(Header, plugin_status_offset) as u64,
    offset_of!(Header, io_trace_offset) as u64,
    offset_of!(Header, daemon_generation) as u64,
    offset_of!(Header, daemon_pid) as u64,
    offset_of!(Header, arch) as u64,
    offset_of!(Header, timebase_numer) as u64,
    offset_of!(Header, timebase_denom) as u64,
    offset_of!(Header, created_host_ns) as u64,
    offset_of!(Header, daemon_version) as u64,
    offset_of!(Header, _reserved) as u64,
    // ClockBlock
    size_of::<ClockBlock>() as u64,
    offset_of!(ClockBlock, seq) as u64,
    offset_of!(ClockBlock, host_ref_ns) as u64,
    offset_of!(ClockBlock, media_ref_ns) as u64,
    offset_of!(ClockBlock, rate_bits) as u64,
    offset_of!(ClockBlock, state_word) as u64,
    offset_of!(ClockBlock, step_gen) as u64,
    offset_of!(ClockBlock, grandmaster) as u64,
    offset_of!(ClockBlock, publish_ns) as u64,
    offset_of!(ClockBlock, _reserved) as u64,
    // DaemonStatus
    size_of::<DaemonStatus>() as u64,
    offset_of!(DaemonStatus, heartbeat_ns) as u64,
    offset_of!(DaemonStatus, audio_word) as u64,
    offset_of!(DaemonStatus, channels_word) as u64,
    offset_of!(DaemonStatus, tx_guard_samples) as u64,
    offset_of!(DaemonStatus, flags) as u64,
    offset_of!(DaemonStatus, tx_packets) as u64,
    offset_of!(DaemonStatus, tx_underruns) as u64,
    offset_of!(DaemonStatus, rx_packets) as u64,
    offset_of!(DaemonStatus, rx_late_packets) as u64,
    offset_of!(DaemonStatus, peers) as u64,
    offset_of!(DaemonStatus, _reserved) as u64,
    // PluginStatus
    size_of::<PluginStatus>() as u64,
    offset_of!(PluginStatus, read_calls) as u64,
    offset_of!(PluginStatus, write_calls) as u64,
    offset_of!(PluginStatus, input_frames) as u64,
    offset_of!(PluginStatus, input_missing) as u64,
    offset_of!(PluginStatus, output_frames) as u64,
    offset_of!(PluginStatus, silenced_cycles) as u64,
    offset_of!(PluginStatus, late_output_cycles) as u64,
    offset_of!(PluginStatus, early_input_cycles) as u64,
    offset_of!(PluginStatus, min_output_margin) as u64,
    offset_of!(PluginStatus, min_input_margin) as u64,
    offset_of!(PluginStatus, max_frames) as u64,
    offset_of!(PluginStatus, last_frames) as u64,
    offset_of!(PluginStatus, io_heartbeat_ns) as u64,
    offset_of!(PluginStatus, faulted) as u64,
    offset_of!(PluginStatus, frames_capped) as u64,
    offset_of!(PluginStatus, _reserved0) as u64,
    offset_of!(PluginStatus, zts_calls) as u64,
    offset_of!(PluginStatus, seed) as u64,
    offset_of!(PluginStatus, media_offset) as u64,
    offset_of!(PluginStatus, regime) as u64,
    offset_of!(PluginStatus, absorbs) as u64,
    offset_of!(PluginStatus, seed_bumps) as u64,
    offset_of!(PluginStatus, last_zts_sample) as u64,
    offset_of!(PluginStatus, last_zts_host_ticks) as u64,
    offset_of!(PluginStatus, device_rate_ppm_milli) as u64,
    offset_of!(PluginStatus, phase_error_ns) as u64,
    offset_of!(PluginStatus, gate) as u64,
    offset_of!(PluginStatus, clock_read_failures) as u64,
    offset_of!(PluginStatus, _reserved1) as u64,
    offset_of!(PluginStatus, plugin_instance) as u64,
    offset_of!(PluginStatus, plugin_pid) as u64,
    offset_of!(PluginStatus, applied_word) as u64,
    offset_of!(PluginStatus, io_clients) as u64,
    offset_of!(PluginStatus, attach_count) as u64,
    offset_of!(PluginStatus, attached_generation) as u64,
    offset_of!(PluginStatus, _reserved2) as u64,
    // IoTraceHeader
    size_of::<IoTraceHeader>() as u64,
    offset_of!(IoTraceHeader, session) as u64,
    offset_of!(IoTraceHeader, next) as u64,
    offset_of!(IoTraceHeader, _reserved) as u64,
    // IoTraceEntry
    size_of::<IoTraceEntry>() as u64,
    offset_of!(IoTraceEntry, cycle_counter) as u64,
    offset_of!(IoTraceEntry, op_stream) as u64,
    offset_of!(IoTraceEntry, frames) as u64,
    offset_of!(IoTraceEntry, current_sample) as u64,
    offset_of!(IoTraceEntry, current_host_ticks) as u64,
    offset_of!(IoTraceEntry, input_sample) as u64,
    offset_of!(IoTraceEntry, output_sample) as u64,
    offset_of!(IoTraceEntry, done_host_ticks) as u64,
];

/// FNV-1a 64 over the layout constants, the size of every block and the
/// offset of every field. Exchanged in the IPC handshake and stored in the
/// header; the golden value is pinned in `tests/layout_golden.rs`.
pub const LAYOUT_HASH: u64 = fnv1a64(LAYOUT_WORDS);

// The layout is consistent: blocks have their sizes, don't overlap, are
// aligned, and the rings tile the region exactly.
const _: () = {
    assert!(size_of::<Header>() == 0x100);
    assert!(size_of::<ClockBlock>() == 0x80);
    assert!(size_of::<DaemonStatus>() == 0x100);
    assert!(size_of::<PluginStatus>() == 0x200);
    assert!(size_of::<IoTraceHeader>() == 0x80);
    assert!(size_of::<IoTraceEntry>() == 0x40);
    assert!(align_of::<Header>() <= REGION_ALIGN && HEADER_OFFSET % align_of::<Header>() == 0);
    assert!(CLOCK_OFFSET % align_of::<ClockBlock>() == 0);
    assert!(DAEMON_STATUS_OFFSET % align_of::<DaemonStatus>() == 0);
    assert!(PLUGIN_STATUS_OFFSET % align_of::<PluginStatus>() == 0);
    assert!(IO_TRACE_OFFSET % align_of::<IoTraceHeader>() == 0);
    assert!(IO_TRACE_ENTRIES_OFFSET % align_of::<IoTraceEntry>() == 0);
    assert!(HEADER_OFFSET + size_of::<Header>() <= CLOCK_OFFSET);
    assert!(CLOCK_OFFSET + size_of::<ClockBlock>() <= DAEMON_STATUS_OFFSET);
    assert!(DAEMON_STATUS_OFFSET + size_of::<DaemonStatus>() <= PLUGIN_STATUS_OFFSET);
    assert!(PLUGIN_STATUS_OFFSET + size_of::<PluginStatus>() <= IO_TRACE_OFFSET);
    assert!(IO_TRACE_ENTRIES_OFFSET + IO_TRACE_ENTRIES * size_of::<IoTraceEntry>() <= RX_OFFSET);
    assert!(RING_FRAMES.is_power_of_two() && RING_FRAMES >= 2);
    // The header stores these as u32.
    assert!(RING_FRAMES <= u32::MAX as usize);
    assert!(MAX_CHANNELS <= u32::MAX as usize);
    assert!(RX_OFFSET % PAGE_BYTES == 0 && RING_BYTES % PAGE_BYTES == 0);
    assert!(RX_OFFSET + MAX_CHANNELS * RING_BYTES == TX_OFFSET);
    assert!(TX_OFFSET + MAX_CHANNELS * RING_BYTES == REGION_SIZE);
    assert!(REGION_SIZE % PAGE_BYTES == 0);
    assert!(PAGE_BYTES % REGION_ALIGN == 0);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_bytes_pad_and_truncate() {
        let v = HeaderInit::version_bytes("0.1.0");
        assert_eq!(&v[..6], b"0.1.0\0");
        assert!(v[5..].iter().all(|&b| b == 0));
        // 31 ASCII bytes and a 2-byte character: the character is dropped.
        let long = "abcdefghijklmnopqrstuvwxyz01234é";
        let v = HeaderInit::version_bytes(long);
        assert_eq!(&v[..31], &long.as_bytes()[..31]);
        assert_eq!(v[31], 0);
    }

    #[test]
    fn header_version_string() {
        let mut h = Header::new(&HeaderInit {
            daemon_generation: 1,
            daemon_pid: 2,
            arch: HOST_ARCH,
            timebase: Timebase::NANOS,
            created_host_ns: 3,
            daemon_version: HeaderInit::version_bytes("ovsc 0.1.0"),
        });
        assert_eq!(h.daemon_version(), "ovsc 0.1.0");
        assert_eq!(h.validate(), Ok(()));
        h.daemon_version = [b'x'; 32];
        assert_eq!(h.daemon_version().len(), 32);
        h.daemon_version[3] = 0xFF;
        assert_eq!(h.daemon_version(), "xxx");
    }

    #[test]
    fn errors_display() {
        use std::string::ToString;
        let e = LayoutError::TooSmall { need: REGION_SIZE, got: 4096 };
        assert_eq!(e.to_string(), "shared region too small: 4096 bytes, need 67174400");
        assert!(LayoutError::BadHash(1).to_string().contains("layout hash 0x1"));
    }
}
