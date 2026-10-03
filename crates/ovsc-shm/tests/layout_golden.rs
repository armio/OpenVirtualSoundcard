//! The shared-memory layout is a contract between separately built binaries
//! (the daemon and the driver), so every size, offset and the layout hash are
//! pinned here. A change to any of them must bump LAYOUT_VERSION or at least
//! update the golden hash deliberately.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::mem::{align_of, offset_of, size_of};
use std::ptr;
use std::sync::atomic::Ordering;

use ovsc_shm::clock::ClockBlock;
use ovsc_shm::layout::*;
use ovsc_shm::status::{DaemonStatus, IoTraceEntry, IoTraceHeader, PluginStatus};
use ovsc_shm::time::Timebase;

const GOLDEN_LAYOUT_HASH: u64 = 0x5f5f_7004_974a_51da;

/// A zeroed, 16 KiB-aligned heap buffer standing in for a mapping.
struct Buffer {
    ptr: *mut u8,
    layout: Layout,
}

impl Buffer {
    fn new(len: usize) -> Self {
        let layout = Layout::from_size_align(len, 0x4000).unwrap();
        // SAFETY: the layout has a nonzero size.
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null());
        Buffer { ptr, layout }
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this layout.
        unsafe { dealloc(self.ptr, self.layout) };
    }
}

fn init_args() -> HeaderInit {
    HeaderInit {
        daemon_generation: 0x1234_5678_9ABC_DEF0,
        daemon_pid: 4242,
        arch: HOST_ARCH,
        timebase: Timebase { numer: 125, denom: 3 },
        created_host_ns: 987_654_321,
        daemon_version: HeaderInit::version_bytes("ovsc 0.1.0"),
    }
}

fn addr<T>(r: &T) -> usize {
    r as *const T as usize
}

#[test]
fn constants() {
    assert_eq!(MAGIC.to_le_bytes(), *b"OVSCSHM1");
    assert_eq!(LAYOUT_VERSION, 1);
    assert_eq!(MAX_CHANNELS, 128);
    assert_eq!(RING_FRAMES, 32768);
    assert_eq!(RING_BYTES, 0x40000);
    assert_eq!(HEADER_OFFSET, 0x0);
    assert_eq!(CLOCK_OFFSET, 0x400);
    assert_eq!(DAEMON_STATUS_OFFSET, 0x800);
    assert_eq!(PLUGIN_STATUS_OFFSET, 0xC00);
    assert_eq!(IO_TRACE_OFFSET, 0x1000);
    assert_eq!(IO_TRACE_ENTRIES_OFFSET, 0x1080);
    assert_eq!(RX_OFFSET, 0x0001_0000);
    assert_eq!(TX_OFFSET, 0x0201_0000);
    assert_eq!(REGION_SIZE, 0x0401_0000);
    assert_eq!(REGION_SIZE, 67_174_400);
    assert_eq!(IO_TRACE_ENTRIES, 64);
    assert_eq!(IO_FRAMES_CAP, 16384);
}

#[test]
fn layout_hash_is_pinned() {
    assert_eq!(
        LAYOUT_HASH, GOLDEN_LAYOUT_HASH,
        "the shared-memory layout changed: bump LAYOUT_VERSION if needed and update the golden hash"
    );
}

#[test]
fn block_sizes() {
    assert_eq!((size_of::<Header>(), align_of::<Header>()), (256, 128));
    assert_eq!((size_of::<ClockBlock>(), align_of::<ClockBlock>()), (128, 128));
    assert_eq!((size_of::<DaemonStatus>(), align_of::<DaemonStatus>()), (256, 128));
    assert_eq!((size_of::<PluginStatus>(), align_of::<PluginStatus>()), (512, 128));
    assert_eq!((size_of::<IoTraceHeader>(), align_of::<IoTraceHeader>()), (128, 128));
    assert_eq!((size_of::<IoTraceEntry>(), align_of::<IoTraceEntry>()), (64, 64));
}

#[test]
fn header_offsets() {
    assert_eq!(offset_of!(Header, magic), 0x00);
    assert_eq!(offset_of!(Header, layout_version), 0x08);
    assert_eq!(offset_of!(Header, header_size), 0x0C);
    assert_eq!(offset_of!(Header, region_size), 0x10);
    assert_eq!(offset_of!(Header, layout_hash), 0x18);
    assert_eq!(offset_of!(Header, max_channels), 0x20);
    assert_eq!(offset_of!(Header, ring_frames), 0x24);
    assert_eq!(offset_of!(Header, rx_offset), 0x28);
    assert_eq!(offset_of!(Header, tx_offset), 0x30);
    assert_eq!(offset_of!(Header, clock_offset), 0x38);
    assert_eq!(offset_of!(Header, daemon_status_offset), 0x3C);
    assert_eq!(offset_of!(Header, plugin_status_offset), 0x40);
    assert_eq!(offset_of!(Header, io_trace_offset), 0x44);
    assert_eq!(offset_of!(Header, daemon_generation), 0x48);
    assert_eq!(offset_of!(Header, daemon_pid), 0x50);
    assert_eq!(offset_of!(Header, arch), 0x54);
    assert_eq!(offset_of!(Header, timebase_numer), 0x58);
    assert_eq!(offset_of!(Header, timebase_denom), 0x5C);
    assert_eq!(offset_of!(Header, created_host_ns), 0x60);
    assert_eq!(offset_of!(Header, daemon_version), 0x68);
}

#[test]
fn clock_block_offsets() {
    assert_eq!(offset_of!(ClockBlock, seq), 0x00);
    assert_eq!(offset_of!(ClockBlock, host_ref_ns), 0x08);
    assert_eq!(offset_of!(ClockBlock, media_ref_ns), 0x10);
    assert_eq!(offset_of!(ClockBlock, rate_bits), 0x18);
    assert_eq!(offset_of!(ClockBlock, state_word), 0x20);
    assert_eq!(offset_of!(ClockBlock, step_gen), 0x28);
    assert_eq!(offset_of!(ClockBlock, grandmaster), 0x30);
    assert_eq!(offset_of!(ClockBlock, publish_ns), 0x38);
}

#[test]
fn daemon_status_offsets() {
    assert_eq!(offset_of!(DaemonStatus, heartbeat_ns), 0x00);
    assert_eq!(offset_of!(DaemonStatus, audio_word), 0x08);
    assert_eq!(offset_of!(DaemonStatus, channels_word), 0x10);
    assert_eq!(offset_of!(DaemonStatus, tx_guard_samples), 0x18);
    assert_eq!(offset_of!(DaemonStatus, flags), 0x20);
    assert_eq!(offset_of!(DaemonStatus, tx_packets), 0x28);
    assert_eq!(offset_of!(DaemonStatus, tx_underruns), 0x30);
    assert_eq!(offset_of!(DaemonStatus, rx_packets), 0x38);
    assert_eq!(offset_of!(DaemonStatus, rx_late_packets), 0x40);
    assert_eq!(offset_of!(DaemonStatus, peers), 0x48);
}

#[test]
fn plugin_status_offsets() {
    // Line 0: the IO thread.
    assert_eq!(offset_of!(PluginStatus, read_calls), 0x00);
    assert_eq!(offset_of!(PluginStatus, write_calls), 0x08);
    assert_eq!(offset_of!(PluginStatus, input_frames), 0x10);
    assert_eq!(offset_of!(PluginStatus, input_missing), 0x18);
    assert_eq!(offset_of!(PluginStatus, output_frames), 0x20);
    assert_eq!(offset_of!(PluginStatus, silenced_cycles), 0x28);
    assert_eq!(offset_of!(PluginStatus, late_output_cycles), 0x30);
    assert_eq!(offset_of!(PluginStatus, early_input_cycles), 0x38);
    assert_eq!(offset_of!(PluginStatus, min_output_margin), 0x40);
    assert_eq!(offset_of!(PluginStatus, min_input_margin), 0x48);
    assert_eq!(offset_of!(PluginStatus, max_frames), 0x50);
    assert_eq!(offset_of!(PluginStatus, last_frames), 0x58);
    assert_eq!(offset_of!(PluginStatus, io_heartbeat_ns), 0x60);
    assert_eq!(offset_of!(PluginStatus, faulted), 0x68);
    assert_eq!(offset_of!(PluginStatus, frames_capped), 0x70);
    // Line 1: GetZeroTimeStamp.
    assert_eq!(offset_of!(PluginStatus, zts_calls), 0x80);
    assert_eq!(offset_of!(PluginStatus, seed), 0x88);
    assert_eq!(offset_of!(PluginStatus, media_offset), 0x90);
    assert_eq!(offset_of!(PluginStatus, regime), 0x98);
    assert_eq!(offset_of!(PluginStatus, absorbs), 0xA0);
    assert_eq!(offset_of!(PluginStatus, seed_bumps), 0xA8);
    assert_eq!(offset_of!(PluginStatus, last_zts_sample), 0xB0);
    assert_eq!(offset_of!(PluginStatus, last_zts_host_ticks), 0xB8);
    assert_eq!(offset_of!(PluginStatus, device_rate_ppm_milli), 0xC0);
    assert_eq!(offset_of!(PluginStatus, phase_error_ns), 0xC8);
    assert_eq!(offset_of!(PluginStatus, gate), 0xD0);
    assert_eq!(offset_of!(PluginStatus, clock_read_failures), 0xD8);
    // Line 2: the IPC queue.
    assert_eq!(offset_of!(PluginStatus, plugin_instance), 0x100);
    assert_eq!(offset_of!(PluginStatus, plugin_pid), 0x108);
    assert_eq!(offset_of!(PluginStatus, applied_word), 0x110);
    assert_eq!(offset_of!(PluginStatus, io_clients), 0x118);
    assert_eq!(offset_of!(PluginStatus, attach_count), 0x120);
    assert_eq!(offset_of!(PluginStatus, attached_generation), 0x128);
}

#[test]
fn io_trace_offsets() {
    assert_eq!(offset_of!(IoTraceHeader, session), 0x00);
    assert_eq!(offset_of!(IoTraceHeader, next), 0x08);
    assert_eq!(offset_of!(IoTraceEntry, cycle_counter), 0x00);
    assert_eq!(offset_of!(IoTraceEntry, op_stream), 0x08);
    assert_eq!(offset_of!(IoTraceEntry, frames), 0x10);
    assert_eq!(offset_of!(IoTraceEntry, current_sample), 0x18);
    assert_eq!(offset_of!(IoTraceEntry, current_host_ticks), 0x20);
    assert_eq!(offset_of!(IoTraceEntry, input_sample), 0x28);
    assert_eq!(offset_of!(IoTraceEntry, output_sample), 0x30);
    assert_eq!(offset_of!(IoTraceEntry, done_host_ticks), 0x38);
    assert_eq!(IO_TRACE_ENTRIES_OFFSET + IO_TRACE_ENTRIES * size_of::<IoTraceEntry>(), 0x2080);
}

#[test]
fn init_then_from_raw() {
    let buf = Buffer::new(REGION_SIZE);
    let base = buf.ptr;
    // Garbage in the control area must not survive init; ring memory is left
    // alone.
    // SAFETY: both ranges lie inside the buffer.
    unsafe {
        ptr::write_bytes(base, 0xAA, RX_OFFSET);
        // A slot holding (ts = 0x1111_1111, sample 0x1111_1111).
        ptr::write_bytes(base.add(RX_OFFSET + (0x1111_1111 % RING_FRAMES) * 8), 0x11, 8);
    }
    // SAFETY: the buffer is ours, exclusively, and outlives the views.
    let created = unsafe { RegionRef::init(base, REGION_SIZE, &init_args()) }.unwrap();
    // SAFETY: as above.
    let region = unsafe { RegionRef::from_raw(base, REGION_SIZE) }.unwrap();
    assert_eq!(created.base(), base);
    assert_eq!(region.base(), base);
    assert_eq!(region.len(), REGION_SIZE);

    let h = region.header();
    assert_eq!(h.magic, MAGIC);
    assert_eq!(h.layout_version, 1);
    assert_eq!(h.header_size, 256);
    assert_eq!(h.region_size, REGION_SIZE as u64);
    assert_eq!(h.layout_hash, GOLDEN_LAYOUT_HASH);
    assert_eq!((h.max_channels, h.ring_frames), (128, 32768));
    assert_eq!((h.rx_offset, h.tx_offset), (0x10000, 0x2010000));
    assert_eq!(
        (h.clock_offset, h.daemon_status_offset, h.plugin_status_offset, h.io_trace_offset),
        (0x400, 0x800, 0xC00, 0x1000)
    );
    assert_eq!(h.daemon_generation, 0x1234_5678_9ABC_DEF0);
    assert_eq!(h.daemon_pid, 4242);
    assert_eq!(h.arch, HOST_ARCH);
    assert_eq!(h.timebase(), Timebase { numer: 125, denom: 3 });
    assert_eq!(h.created_host_ns, 987_654_321);
    assert_eq!(h.daemon_version(), "ovsc 0.1.0");

    // The accessors point at the compiled offsets.
    let b = base as usize;
    assert_eq!(addr(h), b);
    assert_eq!(addr(region.clock()), b + 0x400);
    assert_eq!(addr(region.daemon()), b + 0x800);
    assert_eq!(addr(region.plugin()), b + 0xC00);
    let (trace, entries) = region.io_trace();
    assert_eq!(addr(trace), b + 0x1000);
    assert_eq!(addr(&entries[0]), b + 0x1080);
    assert_eq!(addr(&entries[63]), b + 0x1080 + 63 * 64);
    assert_eq!(region.rx_slots(0).unwrap().as_ptr() as usize, b + 0x10000);
    assert_eq!(region.rx_slots(127).unwrap().as_ptr() as usize, b + 0x10000 + 127 * 0x40000);
    assert_eq!(region.tx_slots(0).unwrap().as_ptr() as usize, b + 0x2010000);
    assert_eq!(region.tx_slots(127).unwrap().as_ptr() as usize, b + 0x4010000 - 0x40000);
    assert!(region.rx_slots(128).is_none() && region.tx_slots(128).is_none());
    assert!(region.rx(128).is_none() && region.tx(usize::MAX).is_none());

    // The control area was zeroed: the clock block was never written.
    assert_eq!(region.clock().seq.load(Ordering::Relaxed), 0);
    assert_eq!(region.daemon().flags.load(Ordering::Relaxed), 0);
    assert_eq!(entries[63].done_host_ticks.load(Ordering::Relaxed), 0);
    // Ring memory was not touched by init.
    let rx0 = region.rx(0).unwrap();
    assert_eq!(rx0.capacity(), RING_FRAMES);
    assert_eq!(rx0.read_one(0x1111_1111), Some(0x1111_1111));

    // Rings and blocks are the same memory through every view.
    let tx3 = region.tx(3).unwrap();
    tx3.write(1_000_000, &[1, -2, 3]);
    assert_eq!(created.tx(3).unwrap().read_one(1_000_001), Some(-2));
    // SAFETY: slot 1_000_001 mod RING_FRAMES of TX ring 3 is inside the buffer.
    let raw = unsafe {
        ptr::read(
            base.add(TX_OFFSET + 3 * RING_BYTES + (1_000_001 % RING_FRAMES) * 8).cast::<u64>(),
        )
    };
    assert_eq!(raw, ovsc_shm::ring::pack(1_000_001, -2));
    region.daemon().heartbeat_ns.store(77, Ordering::Relaxed);
    assert_eq!(created.daemon().heartbeat_ns.load(Ordering::Relaxed), 77);
}

#[test]
fn views_are_send_sync_copy() {
    fn check<T: Send + Sync + Copy>() {}
    check::<RegionRef<'static>>();
    check::<ovsc_shm::ring::RingRef<'static>>();
}

#[test]
fn from_raw_rejects_bad_ranges() {
    let buf = Buffer::new(REGION_SIZE + 0x4000);
    let base = buf.ptr;
    // Zeroed memory is not a region.
    // SAFETY (all calls below): every range lies inside the buffer, which
    // outlives the views.
    assert_eq!(
        unsafe { RegionRef::from_raw(base, REGION_SIZE) }.unwrap_err(),
        LayoutError::BadMagic(0)
    );

    unsafe { RegionRef::init(base, REGION_SIZE, &init_args()) }.unwrap();
    assert_eq!(
        unsafe { RegionRef::from_raw(base, REGION_SIZE - 1) }.unwrap_err(),
        LayoutError::TooSmall { need: REGION_SIZE, got: REGION_SIZE - 1 }
    );
    assert_eq!(
        unsafe { RegionRef::from_raw(base, 0) }.unwrap_err(),
        LayoutError::TooSmall { need: REGION_SIZE, got: 0 }
    );
    assert_eq!(
        unsafe { RegionRef::from_raw(ptr::null_mut(), REGION_SIZE) }.unwrap_err(),
        LayoutError::Misaligned
    );
    for off in [8, 0x80, 0x800] {
        assert_eq!(
            unsafe { RegionRef::from_raw(base.add(off), REGION_SIZE) }.unwrap_err(),
            LayoutError::Misaligned,
            "offset {off:#x}"
        );
        assert_eq!(
            unsafe { RegionRef::init(base.add(off), REGION_SIZE, &init_args()) }.unwrap_err(),
            LayoutError::Misaligned
        );
    }
    assert_eq!(
        unsafe { RegionRef::init(base, REGION_SIZE - 1, &init_args()) }.unwrap_err(),
        LayoutError::TooSmall { need: REGION_SIZE, got: REGION_SIZE - 1 }
    );
    // A longer mapping is fine, and so is any page-aligned base (4 KiB pages
    // on Intel Macs and Linux).
    let r = unsafe { RegionRef::from_raw(base, REGION_SIZE + 0x4000) }.unwrap();
    assert_eq!(r.len(), REGION_SIZE + 0x4000);
    unsafe { RegionRef::init(base.add(0x1000), REGION_SIZE, &init_args()) }.unwrap();
}

#[test]
fn from_raw_rejects_each_corrupted_header_field() {
    let buf = Buffer::new(REGION_SIZE);
    let base = buf.ptr;
    // SAFETY: the buffer is ours and outlives the views.
    unsafe { RegionRef::init(base, REGION_SIZE, &init_args()) }.unwrap();
    // SAFETY: the buffer starts with an initialized header.
    let good = unsafe { ptr::read(base.cast::<Header>()) };

    let check = |corrupt: &dyn Fn(&mut Header), expected: LayoutError| {
        let mut h = good;
        corrupt(&mut h);
        // SAFETY: no view is alive while the header is rewritten, and every
        // range lies inside the buffer.
        unsafe { ptr::write(base.cast::<Header>(), h) };
        let got = unsafe { RegionRef::from_raw(base, REGION_SIZE) }.map(|_| ());
        unsafe { ptr::write(base.cast::<Header>(), good) };
        assert_eq!(got, Err(expected), "{h:?}");
    };

    let magic2 = u64::from_le_bytes(*b"OVSCSHM2");
    check(&|h| h.magic = magic2, LayoutError::BadMagic(magic2));
    check(&|h| h.magic = 0, LayoutError::BadMagic(0));
    check(&|h| h.layout_version = 2, LayoutError::BadVersion(2));
    check(&|h| h.layout_version = 0, LayoutError::BadVersion(0));
    check(&|h| h.layout_hash ^= 1, LayoutError::BadHash(GOLDEN_LAYOUT_HASH ^ 1));
    check(&|h| h.header_size = 255, LayoutError::BadGeometry);
    check(&|h| h.region_size = 0x0201_0000, LayoutError::BadGeometry);
    check(&|h| h.max_channels = 64, LayoutError::BadGeometry);
    check(&|h| h.ring_frames = 16384, LayoutError::BadGeometry);
    check(&|h| h.rx_offset = 0x2_0000, LayoutError::BadGeometry);
    check(&|h| h.tx_offset = 0x0101_0000, LayoutError::BadGeometry);
    check(&|h| h.clock_offset = 0x480, LayoutError::BadGeometry);
    check(&|h| h.daemon_status_offset = 0x880, LayoutError::BadGeometry);
    check(&|h| h.plugin_status_offset = 0xD00, LayoutError::BadGeometry);
    check(&|h| h.io_trace_offset = 0x2000, LayoutError::BadGeometry);

    // Fields that describe the daemon, not the layout, are not validated
    // here (the driver checks the clock base separately).
    let mut h = good;
    h.daemon_generation = 0;
    h.daemon_pid = 1;
    h.arch = 99;
    h.timebase_numer = 1;
    h.timebase_denom = 1;
    h.daemon_version = [0xFF; 32];
    // SAFETY: as in `check`.
    unsafe { ptr::write(base.cast::<Header>(), h) };
    let r = unsafe { RegionRef::from_raw(base, REGION_SIZE) }.unwrap();
    assert_eq!(r.header().daemon_version(), "");
    assert_eq!(r.header().validate(), Ok(()));
}
