//! The whole macOS stack except Apple's code, in `cargo test` (design
//! sections 11, 12, 16 and 17.1).
//!
//! The daemon side is real: a device engine on 127.0.0.1 whose receive
//! channels subscribe to its own transmit channels (rx n <- tx 0n), its
//! rings in the shared region, its free-running clock (+50 ppm) mirrored
//! there, and the driver service over the in-memory transport. The driver is
//! the `ovsc-hal` rlib, driven only through its vtable by a simulated
//! HAL IO thread (`support/hal_sim.rs`) that plays PRBS24 at 512 frames per
//! cycle and checks every input frame against what it played at the same
//! device time. The device returns its own output at device-time delay 0,
//! whatever the latencies (design section 8.4), so every frame must match.
//!
//! The engine runs with the longest receive latency, 40 ms, and the daemon
//! adds 60 ms to the input and 100 ms to the output safety offset instead
//! of 0.5 ms and 1 ms: the HAL reads a frame 100 ms after it was played at
//! the earliest, and writes it 100 ms before it is sent. Delay 0 holds all
//! the same, and the whole machine may stall for up to about 100 ms
//! without a frame going missing. Virtual machines do: one was seen
//! stopping every thread of every process at once for up to 60 ms, a few
//! times a minute, with nothing running. Where threads may not run
//! real-time (Linux without CAP_SYS_NICE) nothing shields them from
//! shorter stalls either. The driver starts with this configuration in its
//! host storage, as a previous run would have left it, so attaching
//! changes nothing. The margins would also hide a deadline the driver and
//! the daemon's transmit thread disagree on, so (a) ends with the output
//! offset of the default configuration and checks every frame that goes
//! missing against when the HAL thread wrote it. The margins of the
//! default configuration are measured on real hardware by the end-to-end
//! tests.
//!
//! * (a) A steady loop: after 1 s of settling, 10 s of all 8 channels
//!   bit-exact, no transmit underrun, no late output or early input cycle,
//!   and the device rate the HAL sees at +50 ppm. Then 3 s with the output
//!   offset of the default configuration, where a frame may go missing
//!   only if the HAL thread wrote it after its deadline.
//! * (b) The service restarts: the daemon dies and a new one, with a new
//!   region, is up within 1 s. The driver swaps the new region in and
//!   retires the old one; the loop is bit-exact again within 3 s, the zero
//!   time stamps stay consecutive under one seed, and the old region is
//!   unmapped only after the retire grace. That the driver also waits for
//!   its IO paths to leave the old region cannot be seen from the vtable;
//!   ovsc-hal's `attach_stress` test covers it.
//! * (c) The engine restarts with 4 channels instead of 8: the driver asks
//!   for a configuration change, the HAL performs it, the device reports 4
//!   channels and the loop runs bit-exact on them.
//! * (d) The daemon goes away for good: input is silent, IO keeps running
//!   and the zero time stamps hold over at the last rate.
//!
//! Everything runs in real time on the host clock, so the scenarios run one
//! at a time, and only on Linux and macOS, where they are known to keep
//! their margins on CI runners.

#![cfg(any(target_os = "linux", target_os = "macos"))]

#[path = "support/hal_sim.rs"]
mod hal_sim;
#[path = "support/prbs.rs"]
mod prbs;

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use ovsc_clock::{MediaClock, free_running_clock_with_rate, local_now_ns};
use ovsc_core::{Channels, Device, DeviceConfig, InitialSubscription, Ports, StartOptions};
use ovsc_hal::ClientTransport;
use ovsc_hal::link::RETIRE_GRACE_NS;
use ovsc_hal::model::DriverConfig;
use ovsc_hal_server::{
    EngineInfo, HalOptions, HalRegion, HalServer, ShmClockMirror, driver_config,
};
use ovsc_ipc::mem::MemRegistry;
use ovsc_ipc::protocol::{SERVICE_NAME, STORAGE_KEY};
use ovsc_proto::arc::SubscriptionStatus;
use ovsc_shm::status::REGIME_FOLLOWING;

use hal_sim::{Cycle, Driver, HalThread, Layout, Record};

const FS: u32 = 48_000;
/// The daemon's free-running clock against the host's.
const RATE: f64 = 1.000_05;
const RATE_PPM: f64 = 50.0;
/// The HAL's IO buffer size.
const FRAMES: u32 = 512;
/// The engine's receive latency, and the margins the daemon adds to the
/// driver's safety offsets; see the module documentation.
const LATENCY_MS: f64 = 40.0;
const INPUT_MARGIN_US: u32 = 60_000;
const OUTPUT_MARGIN_US: u32 = 100_000;
const CHANNELS: u16 = 8;
const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;
const VERSION: &str = "ovsc fullstack test";
/// The effective user ID of Core Audio's driver helper.
const HELPER_EUID: u32 = 202;
const HELPER_PID: i32 = 4242;
/// After the gate opens: the first round trips are silence.
const SETTLE: Duration = Duration::from_secs(1);
/// How far the HAL thread's estimate of when it finished writing a frame
/// may be off, in device time, frames: 1 ms.
const WRITE_TIME_TOLERANCE: f64 = 48.0;

/// The scenarios run in real time against the driver's margins: one at a
/// time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn options() -> HalOptions {
    HalOptions {
        input_margin_us: INPUT_MARGIN_US,
        output_margin_us: OUTPUT_MARGIN_US,
        prevent_idle_sleep: false,
        status_log_interval: Duration::ZERO,
        ..Default::default()
    }
}

/// A device of `channels` x `channels` whose receive channel n subscribes
/// to its own transmit channel 0n.
fn device_config(name: &str, channels: u16, base: u16, process_id: u16) -> DeviceConfig {
    DeviceConfig {
        name: name.into(),
        interface: "127.0.0.1".into(),
        sample_rate: FS,
        latency_ms: LATENCY_MS,
        tx_channels: Channels::Count(channels),
        rx_channels: Channels::Count(channels),
        ports: Ports { arc: base, cmc: base + 1, flow_control: base + 2, settings: base + 3 },
        discovery: false,
        process_id,
        subscriptions: (1..=channels)
            .map(|n| InitialSubscription {
                rx_channel: n,
                tx_channel: format!("{n:02}"),
                tx_device: name.into(),
            })
            .collect(),
        ..Default::default()
    }
}

/// Starts an engine on `region`'s rings. The ports of an engine that just
/// shut down free up as its tasks wind down, so a busy port is tried again
/// for a while, as the daemon's start-up loop would.
async fn start_device(region: &Arc<HalRegion>, clock: &MediaClock, cfg: &DeviceConfig) -> Device {
    let (rx, tx) = (cfg.rx_channels.names().len(), cfg.tx_channels.names().len());
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let rings = region.external_rings(rx, tx).expect("the region's rings");
        let options = StartOptions { rings: Some(rings), ..Default::default() };
        match Device::start_with_options(cfg.clone(), clock.clone(), options).await {
            Ok(device) => return device,
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(e) => panic!("the engine does not start: {e}"),
        }
    }
}

/// One daemon process: its region, clock, driver service and engine.
struct Daemon {
    region: Arc<HalRegion>,
    clock: MediaClock,
    server: Option<HalServer>,
    device: Option<Device>,
}

impl Daemon {
    /// Starts as `ovsc run` does with the coreaudio backend (design
    /// section 14.4): the region, the clock and its mirror, the service,
    /// then the engine on the region's rings.
    async fn start(registry: &Arc<MemRegistry>, cfg: &DeviceConfig) -> Daemon {
        let region = HalRegion::create(VERSION).expect("the region");
        let clock = free_running_clock_with_rate(RATE);
        clock.set_mirror(Some(ShmClockMirror::new(region.clone()))).expect("a mirror");
        let server = HalServer::start(
            options(),
            region.clone(),
            Box::new(registry.bind(SERVICE_NAME)),
            EngineInfo::from_config(cfg).expect("an engine the driver can take"),
        )
        .expect("the service");
        let device = start_device(&region, &clock, cfg).await;
        server.engine_started(&device);
        Daemon { region, clock, server: Some(server), device: Some(device) }
    }

    fn device(&self) -> &Device {
        self.device.as_ref().expect("a running engine")
    }

    fn server(&self) -> &HalServer {
        self.server.as_ref().expect("a running service")
    }

    /// The driver's margin counters in this daemon's region: (late output
    /// cycles, early input cycles, smallest output margin, smallest input
    /// margin since IO started).
    fn margins(&self) -> (u64, u64, i64, i64) {
        let p = self.region.view().plugin();
        let get = |w: &std::sync::atomic::AtomicU64| w.load(Ordering::Relaxed);
        (
            get(&p.late_output_cycles),
            get(&p.early_input_cycles),
            get(&p.min_output_margin) as i64,
            get(&p.min_input_margin) as i64,
        )
    }

    /// Replaces the engine with one started from `cfg`, in the same process
    /// and region.
    async fn restart_engine(&mut self, cfg: &DeviceConfig) {
        self.server().engine_stopped();
        if let Some(d) = self.device.take() {
            d.shutdown().await;
        }
        let device = start_device(&self.region, &self.clock, cfg).await;
        self.server().engine_started(&device);
        self.device = Some(device);
    }

    /// The daemon dies: its connections drop and its heartbeat stops at
    /// once, then the engine, the clock and the region go. Returns the
    /// region, alive for as long as someone still holds it.
    async fn kill(mut self) -> Weak<HalRegion> {
        drop(self.server.take());
        if let Some(d) = self.device.take() {
            d.shutdown().await;
        }
        Arc::downgrade(&self.region)
    }

    /// Stops politely: byes to the drivers, then the engine.
    async fn stop(mut self) {
        if let Some(s) = self.server.take() {
            s.shutdown().await;
        }
        if let Some(d) = self.device.take() {
            d.shutdown().await;
        }
    }
}

/// Whether every receive channel of `device` gets audio from its
/// subscription.
fn receiving(device: &Device) -> Result<(), String> {
    let channels = device.rx_channels();
    let flowing = |s: SubscriptionStatus| {
        matches!(s, SubscriptionStatus::ReceivingUnicast | SubscriptionStatus::ReceivingMulticast)
    };
    if channels.iter().all(|c| flowing(c.status)) {
        Ok(())
    } else {
        Err(format!("{:?}", channels.iter().map(|c| c.status).collect::<Vec<_>>()))
    }
}

/// Polls `check` every 5 ms until it succeeds, for at most `timeout`.
async fn wait_for<T>(
    what: &str,
    timeout: Duration,
    mut check: impl FnMut() -> Result<T, String>,
) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        match check() {
            Ok(v) => return v,
            Err(e) if Instant::now() >= deadline => panic!("timed out waiting for {what}: {e}"),
            Err(_) => tokio::time::sleep(Duration::from_millis(5)).await,
        }
    }
}

/// Sleeps until the host clock reads `ns`.
async fn sleep_until_ns(ns: u64) {
    tokio::time::sleep(Duration::from_nanos(ns.saturating_sub(local_now_ns()))).await;
}

/// Device frames per ns of host time.
fn frames_per_ns() -> f64 {
    f64::from(FS) * RATE * 1e-9
}

/// About how many cycles fall into `span_ns` of host time.
fn expected_cycles(span_ns: u64) -> f64 {
    span_ns as f64 * frames_per_ns() / f64::from(FRAMES)
}

/// The device time at which `c` had written its output: its wake, plus how
/// late it woke and how long it took, as the HAL measured them. The writes
/// end before the cycle does, so this is never early.
fn written_by(c: &Cycle) -> f64 {
    c.time as f64 + (c.late_ns + c.busy_ns) as f64 * frames_per_ns()
}

/// A cycle, for failure messages; a wrong frame is looked up in the signal
/// to tell the delay it came back at.
fn describe(c: &Cycle) -> String {
    let total = u64::from(c.frames) * c.inputs as u64;
    let mut s = format!(
        "cycle at device time {} (session {}, {} us late): {} of {total} channel-frames \
         matched, {} silent, {} wrong",
        c.time,
        c.session,
        c.late_ns / 1000,
        c.matched,
        c.silent,
        c.wrong
    );
    if let Some((ch, t, x)) = c.first_wrong {
        let _ = write!(s, "; input {ch} at {t} is {x}");
        if let Some(n) = prbs::locate(ch, x, t, 50_000) {
            let _ = write!(s, ", played at {n} (device-time delay {})", t - n);
        }
    }
    s
}

/// The device rate the HAL sees over `[from_ns, to_ns)`, in ppm from
/// nominal: from the first and the last new zero time stamp in it.
fn rate_ppm(rec: &Record, from_ns: u64, to_ns: u64) -> f64 {
    let knots: Vec<_> = rec.knots_between(from_ns, to_ns).collect();
    assert!(knots.len() >= 3, "{} zero time stamps in the window", knots.len());
    let (a, b) = (knots[0], knots[knots.len() - 1]);
    assert_eq!(a.session, b.session, "an IO restart in the window");
    let rate = (b.sample - a.sample) / ((b.host_ns - a.host_ns) as f64 * 1e-9);
    (rate / f64::from(FS) - 1.0) * 1e6
}

/// A daemon, a driver attached to it and the HAL thread running IO.
struct Stack {
    registry: Arc<MemRegistry>,
    cfg: DeviceConfig,
    daemon: Option<Daemon>,
    driver: Arc<Driver>,
    hal: Option<HalThread>,
}

impl Stack {
    /// Starts a daemon with an 8 x 8 self-subscribed engine named `name` on
    /// ports from `base`, then the driver; once it is attached, IO; once
    /// audio flows, waits `SETTLE`.
    async fn start(name: &str, base: u16, process_id: u16) -> Stack {
        let registry = MemRegistry::new();
        let cfg = device_config(name, CHANNELS, base, process_id);
        let daemon = Daemon::start(&registry, &cfg).await;
        wait_for("the engine's self-subscriptions", Duration::from_secs(5), || {
            receiving(daemon.device())
        })
        .await;

        // The configuration the daemon offers, as the driver stored it when
        // it last ran.
        let engine = EngineInfo::from_config(&cfg).expect("an engine the driver can take");
        let stored = driver_config(&engine, 1, &options()).expect("a driver configuration");
        let r = registry.clone();
        let link = Box::new(move || {
            Arc::new(r.client(SERVICE_NAME, HELPER_EUID, HELPER_PID)) as Arc<dyn ClientTransport>
        });
        let driver = Driver::new(link, Some(&stored));
        let attached = format!("daemon=attached gen={:x} ", daemon.region.generation());
        wait_for("the driver to attach", Duration::from_secs(5), || {
            let ovst = driver.ovst();
            if ovst.starts_with(&attached) { Ok(()) } else { Err(ovst) }
        })
        .await;
        let layout = driver.layout();
        // 40 + 60 ms of input safety; 7 frames + 100 ms of output safety.
        let expected = Layout {
            sample_rate: FS,
            inputs: 8,
            outputs: 8,
            input_safety: 4800,
            output_safety: 4807,
        };
        assert_eq!(layout, expected, "the device's layout");

        let hal = HalThread::start(driver.clone(), FRAMES);
        wait_for("audio to flow", Duration::from_secs(5), || {
            let ovst = driver.ovst();
            if ovst.contains(" clock=following ") && ovst.contains(" gate=1 ") {
                Ok(())
            } else {
                Err(ovst)
            }
        })
        .await;
        tokio::time::sleep(SETTLE).await;
        Stack { registry, cfg, daemon: Some(daemon), driver, hal: Some(hal) }
    }

    fn daemon(&self) -> &Daemon {
        self.daemon.as_ref().expect("a daemon")
    }

    fn hal(&self) -> &HalThread {
        self.hal.as_ref().expect("the HAL thread")
    }

    /// Waits until the HAL thread has run past `ns`.
    async fn wait_past(&self, ns: u64) {
        sleep_until_ns(ns).await;
        wait_for("the HAL thread", Duration::from_secs(2), || {
            let last = self.hal().record().last_due_ns();
            if last >= ns { Ok(()) } else { Err(format!("last cycle due at {last} ns")) }
        })
        .await;
    }

    /// Checks that every cycle due in `[from_ns, to_ns)` was bit-exact at
    /// device-time delay 0 on `inputs` channels, once the HAL thread has
    /// run past it. Returns how many cycles were checked.
    async fn assert_exact(&self, what: &str, from_ns: u64, to_ns: u64, inputs: usize) -> usize {
        self.wait_past(to_ns).await;
        let rec = self.hal().record();
        let cycles: Vec<&Cycle> = rec.cycles_between(from_ns, to_ns).collect();
        let expected = expected_cycles(to_ns - from_ns);
        assert!(
            cycles.len() as f64 >= expected - 3.0,
            "{what}: {} cycles in {} ms, expected about {expected:.0}",
            cycles.len(),
            (to_ns - from_ns) / MS
        );
        // A HAL thread that finishes too late loses output; say so.
        let late_ms = cycles.iter().map(|c| c.late_ns + c.busy_ns).max().unwrap_or(0) as f64 / 1e6;
        for c in &cycles {
            assert_eq!(c.inputs, inputs, "{what}: {}", describe(c));
            assert!(
                c.exact(),
                "{what}: {}; the HAL thread finished cycles up to {late_ms:.2} ms after they \
                 were due\n{}",
                describe(c),
                self.driver.diagnostics()
            );
        }
        cycles.len()
    }

    /// Checks that the driver saw no late output and no early input cycle.
    /// Otherwise reports when the HAL thread ran late.
    fn assert_margins(&self) {
        let (late, early, min_out, min_in) = self.daemon().margins();
        if (late, early) == (0, 0) {
            return;
        }
        let rec = self.hal().record();
        let start = rec.sessions.first().map_or(0, |s| s.started_ns);
        let mut slow: Vec<&Cycle> = rec.cycles.iter().collect();
        slow.sort_by_key(|c| std::cmp::Reverse(c.late_ns + c.busy_ns));
        let mut report = String::new();
        for c in slow.iter().take(5) {
            let _ = writeln!(
                report,
                "  session {} at {} ms: woke {} us late, busy {} us",
                c.session,
                c.due_ns.saturating_sub(start) / MS,
                c.late_ns / 1000,
                c.busy_ns / 1000
            );
        }
        for (i, session) in rec.sessions.iter().enumerate() {
            let at = session.started_ns.saturating_sub(start) / MS;
            let _ = writeln!(report, "  session {} started at {at} ms", i + 1);
        }
        panic!(
            "{late} late output and {early} early input cycles (smallest margins: output \
             {min_out}, input {min_in} frames); the slowest cycles:\n{report}{}",
            self.driver.diagnostics()
        );
    }

    /// Checks the input of the cycles due in `[from_ns, to_ns)`, once the
    /// HAL thread has run past it, for frames that went missing although
    /// they were written in time. The transmit thread sends frame `t` at
    /// device time `t - lead` at the earliest (a packet of `FPP_MAX` frames
    /// that ends with `t`, sent `guard` after its time stamp). A frame that
    /// did not come back must have been written after that, or the driver
    /// and the daemon disagree on the deadline. Returns how many cycles were
    /// checked and how many had frames missing.
    async fn assert_lost_only_when_late(
        &self,
        from_ns: u64,
        to_ns: u64,
        lead: i64,
    ) -> (usize, usize) {
        self.wait_past(to_ns).await;
        let rec = self.hal().record();
        let cycles: Vec<&Cycle> = rec.cycles_between(from_ns, to_ns).collect();
        let expected = expected_cycles(to_ns - from_ns);
        assert!(
            cycles.len() as f64 >= expected - 3.0,
            "{} cycles in {} ms, expected about {expected:.0}",
            cycles.len(),
            (to_ns - from_ns) / MS
        );
        // How many frames before its earliest send frame `t` was written,
        // by the first of the cycles that wrote it (two may, where the
        // output offset changed).
        let slack = |t: i64| {
            rec.cycles
                .iter()
                .filter(|w| (w.output_time..w.output_time + i64::from(w.frames)).contains(&t))
                .map(|w| (t - lead) as f64 - written_by(w))
                .reduce(f64::max)
        };
        let mut lost = 0;
        for c in &cycles {
            let Some((first, last)) = c.unmatched else { continue };
            lost += 1;
            for t in [first, last] {
                match slack(t) {
                    None => panic!("frame {t} was never written; {}", describe(c)),
                    Some(slack) if slack >= WRITE_TIME_TOLERANCE => panic!(
                        "frame {t} went missing although it was written {:.2} ms before the \
                         transmit thread could send it; {}\n{}",
                        slack / frames_per_ns() / 1e6,
                        describe(c),
                        self.driver.diagnostics()
                    ),
                    Some(_) => {}
                }
            }
        }
        (cycles.len(), lost)
    }

    /// Waits for `n` bit-exact cycles in a row on `inputs` channels, due
    /// after `after_ns`, for at most `timeout`. Returns when the first of
    /// them was due.
    async fn wait_exact_run(
        &self,
        after_ns: u64,
        inputs: usize,
        n: usize,
        timeout: Duration,
    ) -> u64 {
        wait_for("bit-exact audio", timeout, || {
            let rec = self.hal().record();
            let mut run: Option<(u64, usize)> = None;
            let mut last = None;
            for c in rec.cycles.iter().filter(|c| c.due_ns >= after_ns) {
                last = Some(c);
                if c.inputs == inputs && c.exact() {
                    let (start, len) = run.get_or_insert((c.due_ns, 0));
                    *len += 1;
                    if *len >= n {
                        return Ok(*start);
                    }
                } else {
                    run = None;
                }
            }
            Err(last.map_or_else(|| "no cycle yet".to_owned(), describe))
        })
        .await
    }

    /// Stops IO, checks what must hold in every scenario, and stops the
    /// daemon if there is one. Returns what the HAL thread saw.
    async fn finish(mut self) -> Record {
        let rec = self.hal.take().expect("the HAL thread").stop();
        let diagnostics = self.driver.diagnostics();
        assert!(
            rec.errors.is_empty(),
            "the HAL saw {} errors: {:#?}\n{diagnostics}",
            rec.errors.len() as u64 + rec.more_errors,
            rec.errors
        );
        let seeds: BTreeSet<u64> = rec.knots.iter().map(|k| k.seed).collect();
        assert_eq!(seeds.len(), 1, "zero time stamp seeds {seeds:?}\n{diagnostics}");
        assert!(!self.driver.faulted(), "{diagnostics}");
        assert!(diagnostics.contains(" faulted=0 "), "{diagnostics}");
        if let Some(d) = self.daemon.take() {
            d.stop().await;
        }
        rec
    }
}

/// The memory of `region`, which stays mapped for as long as it has a
/// strong reference. In-process, the driver's mapping of the region is one;
/// on macOS the driver maps the region itself, so there is nothing to watch
/// from here.
#[cfg(not(target_os = "macos"))]
fn region_memory(region: &HalRegion) -> Weak<ovsc_ipc::region::SharedRegion> {
    let ovsc_ipc::region::RegionHandle::Local(shared) = region.handle();
    Arc::downgrade(&shared)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_steady_loop_is_bit_exact_at_device_time_delay_0() {
    let _serial = SERIAL.lock().await;
    let s = Stack::start("fullstack-a", 24900, 91).await;
    let from = local_now_ns();
    let underruns = s.daemon().device().stats().tx_underruns;
    let to = from + 10 * SEC;
    let checked = s.assert_exact("the steady loop", from, to, 8).await;
    assert!(checked > 900, "{checked} cycles");

    let stats = s.daemon().device().stats();
    assert_eq!(stats.tx_underruns - underruns, 0, "transmit underruns in the steady loop");
    assert!(stats.tx_packets > 0 && stats.rx_packets > 0, "{stats:?}");
    s.assert_margins();
    let rate = rate_ppm(&s.hal().record(), from, to);
    assert!((rate - RATE_PPM).abs() < 3.0, "the HAL sees the device at {rate:+.2} ppm");

    // The daemon sees the driver attached and streaming.
    let status = s.daemon().server().status();
    assert_eq!((status.peers, status.engine_running, status.config_gen), (1, true, 1));
    assert!(status.gate && status.io_clients == 1, "{status}");
    assert_eq!(status.regime, REGIME_FOLLOWING, "{status}");
    assert_eq!((status.seed, status.faulted), (1, false), "{status}");
    let ovst = s.driver.ovst();
    assert!(ovst.contains(" late_out=0 early_in=0 "), "{ovst}");
    assert!(ovst.contains(" attach=1 "), "{ovst}");

    // The output at the offset of the default configuration: written N +
    // 1 ms before the transmit thread may send it instead of N + 100 ms.
    // Frames written from now on come back about 50 ms later.
    let engine = EngineInfo::from_device(s.daemon().device());
    let defaults = driver_config(&engine, 1, &HalOptions::default()).expect("the defaults");
    let lead = i64::from(engine.fpp_max) - 1 - i64::from(engine.tx_guard_samples);
    s.hal().set_output_safety(Some(i64::from(defaults.output_safety_offset)));
    let from = local_now_ns() + 200 * MS;
    let (checked, lost) = s.assert_lost_only_when_late(from, from + 3 * SEC, lead).await;
    assert!(lost * 4 < checked, "{lost} of {checked} cycles had frames missing");

    let rec = s.finish().await;
    assert_eq!(rec.sessions.len(), 1);
    assert!(rec.performs.is_empty(), "{:?}", rec.performs);
    // A frame written too late is sent as silence, never as other audio.
    assert!(rec.cycles.iter().all(|c| c.wrong == 0), "audio from the wrong time or channel");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_service_is_swapped_in_without_a_seed_change() {
    let _serial = SERIAL.lock().await;
    let mut s = Stack::start("fullstack-b", 24910, 92).await;
    let from = local_now_ns();
    s.assert_exact("before the restart", from, from + SEC, 8).await;

    // The daemon dies and a new one, with a new region, starts within 1 s.
    let old = s.daemon.take().expect("a daemon");
    let old_gen = old.region.generation();
    let old_region = Arc::downgrade(&old.region);
    #[cfg(not(target_os = "macos"))]
    let old_memory = region_memory(&old.region);
    let restart_at = local_now_ns();
    let started = Instant::now();
    let restart = async {
        old.kill().await;
        let daemon = Daemon::start(&s.registry, &s.cfg).await;
        (daemon, started.elapsed())
    };

    // Meanwhile, the driver maps the new region and retires the old one:
    // the old mapping goes only once the grace has passed, with IO running
    // all along. Until then the driver holds it, at the end alone. The
    // host times bracket the log lines: the swap was logged after
    // `before_swap`, the last time it was not there yet, and the retire
    // before `freed_at`.
    let freed = format!("freed the region of generation {old_gen:016x}");
    #[cfg(not(target_os = "macos"))]
    let mut held_by_the_driver_alone = false;
    let retire = async {
        let deadline = Instant::now() + Duration::from_secs(7);
        let mut before_swap = local_now_ns();
        let mut swapped_at = None;
        loop {
            #[cfg(not(target_os = "macos"))]
            let (memory, region) = (old_memory.strong_count(), old_region.strong_count());
            let now = local_now_ns();
            let logs = s.driver.platform;
            if swapped_at.is_none() {
                if logs.logged(", attachment 2)") {
                    swapped_at = Some(now);
                } else {
                    before_swap = now;
                }
            }
            // The retire logs before it unmaps, so the counts read above
            // were read before any unmapping.
            if logs.logged(&freed) {
                let swapped_at = swapped_at.expect("the swap before the retire");
                return (before_swap, swapped_at, local_now_ns());
            }
            #[cfg(not(target_os = "macos"))]
            {
                assert!(memory > 0, "the old region was unmapped before it was retired");
                held_by_the_driver_alone |= swapped_at.is_some() && region == 0;
            }
            assert!(
                Instant::now() < deadline,
                "the old region was never retired\n{}",
                s.driver.diagnostics()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    let ((daemon, restart), (before_swap, swapped_at, freed_at)) = tokio::join!(restart, retire);
    assert!(restart < Duration::from_secs(1), "the restart took {restart:?}");
    let new_gen = daemon.region.generation();
    assert_ne!(new_gen, old_gen);
    s.daemon = Some(daemon);
    let attached = format!("(generation {new_gen:016x}, attachment 2)");
    assert!(s.driver.platform.logged(&attached), "{}", s.driver.diagnostics());
    let at_most = freed_at - before_swap;
    assert!(
        at_most >= RETIRE_GRACE_NS,
        "the old region was freed at most {} ms after the swap, within the grace of {} ms",
        at_most / MS,
        RETIRE_GRACE_NS / MS
    );
    let held = freed_at - swapped_at;
    assert!(held < 3 * SEC, "the old region was freed {} ms after the swap", held / MS);
    #[cfg(not(target_os = "macos"))]
    {
        assert!(held_by_the_driver_alone, "the old daemon's region outlived the swap");
        wait_for("the old region's memory to go", Duration::from_secs(1), || {
            match old_memory.strong_count() {
                0 => Ok(()),
                n => Err(format!("{n} references")),
            }
        })
        .await;
    }
    wait_for("the old daemon to go", Duration::from_secs(2), || match old_region.strong_count() {
        0 => Ok(()),
        n => Err(format!("{n} references")),
    })
    .await;

    // Bit-exact again within 3 s of the restart, on the new region.
    let back = restart_at + 3 * SEC;
    s.assert_exact("after the restart", back, back + 2 * SEC, 8).await;
    let ovst = s.driver.ovst();
    assert!(ovst.starts_with(&format!("daemon=attached gen={new_gen:x} ")), "{ovst}");
    assert!(ovst.contains(" seed=1 ") && ovst.contains(" attach=2 "), "{ovst}");
    s.assert_margins();
    assert_eq!(s.daemon().server().status().peers, 1);

    // One IO session throughout: consecutive zero time stamps, one seed
    // (checked by finish), and never audio from the wrong time.
    let rec = s.finish().await;
    assert_eq!(rec.sessions.len(), 1);
    assert!(rec.performs.is_empty(), "{:?}", rec.performs);
    let wrong: Vec<String> = rec.cycles.iter().filter(|c| c.wrong > 0).map(describe).collect();
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_channel_change_is_performed_and_the_loop_runs_on_4_channels() {
    let _serial = SERIAL.lock().await;
    let mut s = Stack::start("fullstack-c", 24920, 93).await;
    let from = local_now_ns();
    s.assert_exact("on 8 channels", from, from + SEC, 8).await;
    // Configuration changes are requested no earlier than 2 s after
    // Initialize.
    sleep_until_ns(s.driver.initialized_ns() + 2 * SEC + 500 * MS).await;

    // The engine restarts with 4 x 4 in the same daemon, which pushes the
    // new configuration to the driver.
    let cfg = device_config("fullstack-c", 4, 24920, 93);
    s.daemon.as_mut().expect("a daemon").restart_engine(&cfg).await;
    let perform = wait_for("the configuration change", Duration::from_secs(5), || {
        s.hal().record().performs.first().copied().ok_or_else(|| s.driver.ovst())
    })
    .await;
    assert_eq!((perform.action, perform.status), (1, 0), "Perform");
    assert_eq!((perform.before.inputs, perform.before.outputs), (8, 8));
    let four = Layout { inputs: 4, outputs: 4, ..perform.before };
    assert_eq!(perform.after, four, "the layout after Perform");
    let ovst = s.driver.ovst();
    assert!(ovst.contains(" rate=48000 in=4 out=4 "), "{ovst}");
    // The link stores what Perform applied from its own queue, after it.
    wait_for("the configuration to be stored", Duration::from_secs(2), || {
        let stored = s.driver.host.storage(STORAGE_KEY).ok_or("nothing stored")?;
        let c = DriverConfig::from_storage_string(&stored).map_err(|e| e.to_string())?;
        match (c.config_gen, c.input_channels, c.output_channels) {
            (2, 4, 4) => Ok(()),
            (g, i, o) => Err(format!("configuration {g} with {i} in, {o} out")),
        }
    })
    .await;
    let status = s.daemon().server().status();
    assert_eq!((status.config_gen, status.engine_running), (2, true), "{status}");

    // The loop runs on 4 channels, bit-exact.
    let exact = s.wait_exact_run(perform.at_ns, 4, 20, Duration::from_secs(5)).await;
    assert!(
        exact - perform.at_ns < 3 * SEC,
        "bit-exact {} ms after Perform",
        (exact - perform.at_ns) / MS
    );
    s.assert_exact("on 4 channels", exact, exact + 1500 * MS, 4).await;
    s.assert_margins();

    // Audio from another time only right after IO restarted, where the
    // HAL's sample times start again from 0 over what the old session
    // wrote: up to 2 N + S_out ahead of the restart, which the new session
    // reads S_in later. Then 100 ms more, for Perform and the restart.
    let rec = s.finish().await;
    assert_eq!(rec.performs.len(), 1, "{:?}", rec.performs);
    assert_eq!(rec.sessions.len(), 2);
    assert_eq!(rec.sessions[1].layout, four);
    assert_eq!(rec.knots.iter().find(|k| k.session == 2).map(|k| k.sample), Some(0.0));
    let stale = 2 * i64::from(FRAMES) + perform.before.output_safety + four.input_safety;
    let stale_until = rec.sessions[1].started_ns + (stale as f64 / frames_per_ns()) as u64;
    let wrong: Vec<String> = rec
        .cycles
        .iter()
        .filter(|c| c.wrong > 0 && !(c.session == 2 && c.due_ns < stale_until + 100 * MS))
        .map(describe)
        .collect();
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_the_daemon_input_is_silent_and_io_holds_over() {
    let _serial = SERIAL.lock().await;
    let mut s = Stack::start("fullstack-d", 24930, 94).await;
    let from = local_now_ns();
    s.assert_exact("with the daemon", from, from + SEC, 8).await;

    let gone = s.daemon.take().expect("a daemon").kill().await;
    let absent_at = local_now_ns();
    let to = absent_at + 4 * SEC;
    s.wait_past(to).await;
    {
        let rec = s.hal().record();
        // Silence once the last received audio is read, but IO goes on at
        // the device's pace.
        let quiet = absent_at + 200 * MS;
        let cycles: Vec<&Cycle> = rec.cycles_between(quiet, to).collect();
        let expected = expected_cycles(to - quiet);
        assert!(
            (cycles.len() as f64 - expected).abs() <= 3.0,
            "{} cycles in {} ms, expected {expected:.0}",
            cycles.len(),
            (to - quiet) / MS
        );
        for c in &cycles {
            assert_eq!(c.nonzero, 0, "input without the daemon: {}", describe(c));
        }
        // The zero time stamps hold over at the rate the device had, once
        // the heartbeat went stale.
        let rate = rate_ppm(&rec, absent_at + 1500 * MS, to);
        assert!((rate - RATE_PPM).abs() < 5.0, "the HAL sees the device at {rate:+.2} ppm");
    }
    let ovst = s.driver.ovst();
    assert!(ovst.starts_with("daemon=connecting "), "{ovst}");
    assert!(ovst.contains(" clock=holdover ") && ovst.contains(" gate=0 "), "{ovst}");
    assert!(ovst.contains(" seed=1 "), "{ovst}");
    // The daemon is gone; the driver keeps its own mapping of the region
    // while it waits for a new one.
    wait_for("the old daemon to go", Duration::from_secs(2), || match gone.strong_count() {
        0 => Ok(()),
        n => Err(format!("{n} references")),
    })
    .await;

    let rec = s.finish().await;
    assert_eq!(rec.sessions.len(), 1);
    let wrong: Vec<String> = rec.cycles.iter().filter(|c| c.wrong > 0).map(describe).collect();
    assert!(wrong.is_empty(), "{wrong:#?}");
}
