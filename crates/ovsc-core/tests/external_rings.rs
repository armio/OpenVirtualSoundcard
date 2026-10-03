//! A device whose rings live in memory it doesn't own, laid out like the
//! region the macOS daemon shares with its driver, exchanging audio with an
//! ordinary device over 127.0.0.1.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::net::{Ipv4Addr, SocketAddr};
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use ovsc_clock::{local_now_ns, system_clock};
use ovsc_core::buffer::TimedRing;
use ovsc_core::client;
use ovsc_core::directory::StaticDirectory;
use ovsc_core::{
    AudioIo, Channels, Device, DeviceConfig, Error, ExternalRings, Ports, StartOptions,
};
use ovsc_proto::arc;
use ovsc_shm::layout::{HOST_ARCH, HeaderInit, PAGE_BYTES, REGION_SIZE, RING_FRAMES, RegionRef};
use ovsc_shm::time::Timebase;

fn ports(base: u16) -> Ports {
    Ports { arc: base, cmc: base + 1, flow_control: base + 2, settings: base + 3 }
}

fn config(name: &str, base: u16, process_id: u16) -> DeviceConfig {
    DeviceConfig {
        name: name.into(),
        interface: "127.0.0.1".into(),
        tx_channels: Channels::Count(4),
        rx_channels: Channels::Count(4),
        latency_ms: 10.0,
        ports: ports(base),
        discovery: false,
        process_id,
        ..Default::default()
    }
}

/// A 16 KiB-aligned heap buffer laid out as a shared region, standing in for
/// the daemon's mapping.
struct Region {
    base: NonNull<u8>,
    layout: Layout,
}

// SAFETY: after `Region::new` the memory is only accessed through atomics
// (the rings) or read (the header), from any thread.
unsafe impl Send for Region {}
// SAFETY: as above.
unsafe impl Sync for Region {}

impl Region {
    fn new() -> Arc<Self> {
        let layout = Layout::from_size_align(REGION_SIZE, PAGE_BYTES).unwrap();
        // SAFETY: the layout has a nonzero size.
        let base = NonNull::new(unsafe { alloc_zeroed(layout) }).expect("region allocation");
        let init = HeaderInit {
            daemon_generation: 1,
            daemon_pid: std::process::id(),
            arch: HOST_ARCH,
            timebase: Timebase::NANOS,
            created_host_ns: local_now_ns(),
            daemon_version: HeaderInit::version_bytes("external-rings test"),
        };
        // SAFETY: REGION_SIZE bytes we own, not shared with anyone yet.
        unsafe { RegionRef::init(base.as_ptr(), REGION_SIZE, &init) }.unwrap();
        Arc::new(Self { base, layout })
    }

    fn view(&self) -> RegionRef<'_> {
        // SAFETY: the allocation lives as long as `self`, and is only
        // accessed through atomics.
        unsafe { RegionRef::from_raw(self.base.as_ptr(), self.layout.size()) }.unwrap()
    }

    /// The first `rx` receive and `tx` transmit rings of the region.
    fn rings(self: &Arc<Self>, rx: usize, tx: usize) -> ExternalRings {
        let view = self.view();
        let ring = |slots: Option<NonNull<AtomicU64>>| {
            let keep: Arc<dyn Send + Sync> = self.clone();
            // SAFETY: the region's rings are RING_FRAMES atomics each, inside
            // the allocation that `keep` keeps alive.
            Arc::new(unsafe { TimedRing::from_raw(slots.unwrap(), RING_FRAMES, keep) })
        };
        ExternalRings {
            rx: (0..rx).map(|ch| ring(view.rx_slots(ch))).collect(),
            tx: (0..tx).map(|ch| ring(view.tx_slots(ch))).collect(),
        }
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: allocated in `Region::new` with this layout.
        unsafe { dealloc(self.base.as_ptr(), self.layout) }
    }
}

/// Deterministic 24-bit test signal, distinct per channel.
fn signal(ts: u64, ch: usize) -> i32 {
    let v = (ts.wrapping_mul(2654435761).wrapping_add(ch as u64 * 7919) & 0xff_ffff) as i32;
    (v - 0x80_0000) << 8
}

/// Keeps the transmit rings filled ahead of time, like an audio backend.
fn start_feeder(io: AudioIo, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let lead = io.sample_rate as u64 / 10;
        let mut written = io.now().unwrap();
        let mut buf = vec![0i32; 8192];
        while !stop.load(Ordering::Relaxed) {
            let now = io.now().unwrap();
            let until = now + lead;
            if until > written {
                let n = ((until - written) as usize).min(buf.len());
                for ch in 0..io.tx.len() {
                    for (i, s) in buf[..n].iter_mut().enumerate() {
                        *s = signal(written + i as u64, ch);
                    }
                    io.write_tx(ch, written, &buf[..n]);
                }
                written += n as u64;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    })
}

/// Checks that `rx_ch` of `io` carries `tx_ch`'s signal for the last 100 ms.
fn verify(io: &AudioIo, rx_ch: usize, tx_ch: usize) -> Result<(), String> {
    let now = io.now().unwrap();
    let end = now - io.latency_samples;
    let start = end - io.sample_rate as u64 / 10;
    let mut buf = vec![0i32; (end - start) as usize];
    let present = io.read_rx(rx_ch, start, &mut buf);
    if present < buf.len() * 99 / 100 {
        return Err(format!("rx {rx_ch}: only {present}/{} samples arrived", buf.len()));
    }
    for (i, &s) in buf.iter().enumerate() {
        let ts = start + i as u64;
        if io.rx[rx_ch].read_one(ts).is_some() && s != signal(ts, tx_ch) {
            return Err(format!("rx {rx_ch}: sample at {ts} is {s:#x}, want tx {tx_ch}"));
        }
    }
    Ok(())
}

async fn eventually(what: &str, mut check: impl FnMut() -> Result<(), String>) {
    let mut last = String::new();
    for _ in 0..100 {
        match check() {
            Ok(()) => return,
            Err(e) => last = e,
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}: {last}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audio_flows_through_rings_in_a_region() {
    let clock = system_clock();
    let region = Region::new();
    let ext_directory = StaticDirectory::new();
    let heap_directory = StaticDirectory::new();
    let ext = Device::start_with_options(
        config("ext-dev", 24470, 4),
        clock.clone(),
        StartOptions {
            directory: Some(ext_directory.clone()),
            rings: Some(region.rings(4, 4)),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let heap = Device::start_with_directory(
        config("heap-dev", 24480, 5),
        clock.clone(),
        heap_directory.clone(),
    )
    .await
    .unwrap();
    for entry in heap.directory_entries() {
        ext_directory.insert(entry);
    }
    for entry in ext.directory_entries() {
        heap_directory.insert(entry);
    }
    assert!(ext.clock().is_ready());
    let names: Vec<String> = ["01", "02", "03", "04"].map(String::from).into();
    assert_eq!(ext.channel_names(), (names.clone(), names));

    let stop = Arc::new(AtomicBool::new(false));
    let feeders =
        [start_feeder(ext.audio(), stop.clone()), start_feeder(heap.audio(), stop.clone())];

    // Both directions: region-backed transmit rings to heap receive rings,
    // and heap transmit rings to region-backed receive rings.
    let mut changes = ext.changes();
    ext.subscribe(1, "02", "heap-dev").unwrap();
    assert!(changes.has_changed().unwrap());
    heap.subscribe(2, "03", "ext-dev").unwrap();
    let ext_io = ext.audio();
    let heap_io = heap.audio();
    eventually("audio in both directions", || {
        verify(&ext_io, 0, 1)?;
        verify(&heap_io, 1, 2)
    })
    .await;

    // The samples really are in the region: read them back through its own
    // view, as the driver would.
    let view = region.view();
    let (rx0, tx2) = (view.rx(0).unwrap(), view.tx(2).unwrap());
    let end = ext_io.now().unwrap() - ext_io.latency_samples;
    let mut present = 0;
    for ts in end - 4800..end {
        if let Some(s) = rx0.read_one(ts) {
            assert_eq!(s, signal(ts, 1), "region rx 0 at {ts}");
            present += 1;
        }
        assert_eq!(tx2.read_one(ts), Some(signal(ts, 2)), "region tx 2 at {ts}");
    }
    assert!(present > 4700, "only {present} samples in region rx 0");

    // The packet counters move while the flows run.
    let (ext_before, heap_before) = (ext.stats(), heap.stats());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (ext_after, heap_after) = (ext.stats(), heap.stats());
    for (what, before, after) in [("ext", ext_before, ext_after), ("heap", heap_before, heap_after)]
    {
        assert!(after.tx_packets > before.tx_packets, "{what}: {before:?} -> {after:?}");
        assert!(after.rx_packets > before.rx_packets, "{what}: {before:?} -> {after:?}");
        assert!(after.tx_underruns >= before.tx_underruns);
        assert!(after.rx_late_packets >= before.rx_late_packets);
    }

    // Renames over ARC show up in the current names and notify watchers.
    changes.mark_unchanged();
    let req = arc::encode_rename_rx_channels_request(client::next_seq(), &[(4, "Return")]);
    let arc_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 24470));
    client::transact_ok(Ipv4Addr::LOCALHOST, arc_addr, &req, Duration::from_millis(500), 3)
        .await
        .unwrap();
    assert!(changes.has_changed().unwrap());
    let (rx_names, tx_names) = ext.channel_names();
    assert_eq!(rx_names[3], "Return");
    assert_eq!(tx_names[3], "04");

    stop.store(true, Ordering::Relaxed);
    for f in feeders {
        f.join().unwrap();
    }
    heap.shutdown().await;
    ext.shutdown().await;
    // Shutting down waits for the device's tasks and receive threads, so
    // only the audio handles still hold the region's rings.
    drop((ext_io, heap_io));
    assert_eq!(Arc::strong_count(&region), 1, "references to the region after shutdown");
}

#[tokio::test]
async fn mismatched_rings_are_rejected() {
    let start = |rx: usize, tx: usize, frames: usize| {
        let ring = || Arc::new(TimedRing::new(frames));
        let rings = ExternalRings {
            rx: (0..rx).map(|_| ring()).collect(),
            tx: (0..tx).map(|_| ring()).collect(),
        };
        let options = StartOptions { rings: Some(rings), ..Default::default() };
        Device::start_with_options(config("bad-rings", 24490, 6), system_clock(), options)
    };
    for (rx, tx, frames) in [(3, 4, RING_FRAMES), (4, 5, RING_FRAMES), (4, 4, RING_FRAMES / 2)] {
        match start(rx, tx, frames).await {
            Err(Error::Config(_)) => {}
            Err(e) => panic!("{rx}/{tx} rings of {frames}: {e}"),
            Ok(_) => panic!("{rx}/{tx} rings of {frames} accepted"),
        }
    }
    // Matching rings start, and the failures above left no ports bound.
    start(4, 4, RING_FRAMES).await.unwrap().shutdown().await;
}
