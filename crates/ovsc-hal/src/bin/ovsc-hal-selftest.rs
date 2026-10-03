//! ovsc-hal-selftest: checks the daemon's side of the macOS driver
//! without Core Audio (design section 17.4, scenario S3).
//!
//! It runs the driver's own link to the daemon's Mach service, as the
//! plug-in does inside the Core Audio driver helper, but with a sink that
//! records what the link hands over instead of feeding a device. Then it
//! checks what the daemon shares:
//!
//! 1. a welcome arrives;
//! 2. the region is `REGION_SIZE` bytes and its header valid;
//! 3. the daemon's heartbeat advances within 1 s and lies within 10 ms of
//!    this process's `CLOCK_UPTIME_RAW`;
//! 4. the clock block is valid and Locked or FreeRunning within
//!    `--clock-timeout` seconds;
//! 5. the engine runs, at the configuration's sample rate;
//! 6. a 100 ms pattern written to TX ring `--channel` at media time now +
//!    20 ms comes back in RX ring `--channel` at the same indices within
//!    1 s, which needs the daemon's receive channel subscribed to its own
//!    transmit channel (skipped with `--no-loop`).
//!
//! Each check prints a PASS or FAIL line and the last line is a
//! `SELFTEST-SUMMARY`; the exit status is 0 only if every check passed. The
//! daemon must accept this process's user (CI runs it as root, with uid 0
//! allowed).

#[cfg(target_os = "macos")]
mod selftest {
    use std::sync::{Arc, Mutex, PoisonError};
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    use ovsc_hal::abi::OSStatus;
    use ovsc_hal::io::{AttachSlot, Attachment};
    use ovsc_hal::link::{ConfigPlan, Link, LinkSink, LinkStatus};
    use ovsc_hal::model::DriverConfig;
    use ovsc_hal::platform::stub::host_now_ns;
    use ovsc_hal::platform::{self, Timebase};
    use ovsc_ipc::protocol::SERVICE_NAME;
    use ovsc_ipc::xpc::XpcClient;
    use ovsc_shm::clock::{ClockRead, ClockRecord, READ_TRIES};
    use ovsc_shm::layout::{REGION_SIZE, RegionRef};
    use ovsc_shm::status::{AudioWord, DAEMON_ENGINE_RUNNING};
    use ovsc_shm::time::{ClockState, ns_to_samples};

    const USAGE: &str = "usage: ovsc-hal-selftest [--service NAME] [--clock-timeout SECS] \
                         [--channel N] [--no-loop]";

    /// How long to wait for the welcome.
    const WELCOME_TIMEOUT: Duration = Duration::from_secs(10);
    /// The heartbeat's limits (check 3).
    const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(1);
    const HEARTBEAT_TOLERANCE_NS: u64 = 10_000_000;
    /// The pattern's place and length (check 6), and how long it may take.
    const PATTERN_LEAD_MS: u64 = 20;
    const PATTERN_MS: u64 = 100;
    const LOOP_TIMEOUT: Duration = Duration::from_secs(1);

    struct Options {
        service: String,
        clock_timeout: Duration,
        channel: usize,
        run_loop: bool,
    }

    impl Options {
        /// The options in `args`; `None` for `--help`.
        fn parse(mut args: impl Iterator<Item = String>) -> Result<Option<Options>, String> {
            let mut o = Options {
                service: SERVICE_NAME.to_owned(),
                clock_timeout: Duration::from_secs(60),
                channel: 8,
                run_loop: true,
            };
            while let Some(arg) = args.next() {
                let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
                match arg.as_str() {
                    "--service" => o.service = value("--service")?,
                    "--clock-timeout" => {
                        let v = value("--clock-timeout")?;
                        let secs: f64 =
                            v.parse().map_err(|_| format!("bad --clock-timeout {v}"))?;
                        o.clock_timeout = Duration::try_from_secs_f64(secs)
                            .map_err(|_| format!("bad --clock-timeout {v}"))?;
                    }
                    "--channel" => {
                        let v = value("--channel")?;
                        o.channel = v
                            .parse()
                            .ok()
                            .filter(|c| *c >= 1)
                            .ok_or(format!("bad --channel {v} (channels count from 1)"))?;
                    }
                    "--no-loop" => o.run_loop = false,
                    "-h" | "--help" => return Ok(None),
                    other => return Err(format!("unknown argument {other}")),
                }
            }
            Ok(Some(o))
        }
    }

    /// The link's sink: records the attachment and configuration the link
    /// hands over. Every configuration counts as published as it is.
    struct Recorder {
        slot: AttachSlot,
        config: Mutex<Option<DriverConfig>>,
    }

    impl Recorder {
        fn config(&self) -> Option<DriverConfig> {
            self.config.lock().unwrap_or_else(PoisonError::into_inner).clone()
        }
    }

    impl LinkSink for Recorder {
        fn attach(&self, a: Option<Box<Attachment>>) -> Option<Box<Attachment>> {
            let old = self.slot.swap(a)?;
            Some(match old.into_box(&self.slot) {
                Ok(old) => old,
                // SAFETY: the link frees what it gets back only once
                // `quiescent` has returned true after this swap.
                Err(old) => unsafe { old.into_box_unchecked() },
            })
        }

        fn quiescent(&self) -> bool {
            self.slot.quiescent()
        }

        fn current_generation(&self) -> Option<u64> {
            self.slot.enter().get().map(|a| a.generation)
        }

        fn published_config(&self) -> DriverConfig {
            self.config().unwrap_or_else(DriverConfig::fallback)
        }

        fn stage(&self, cfg: DriverConfig) -> ConfigPlan {
            *self.config.lock().unwrap_or_else(PoisonError::into_inner) = Some(cfg);
            ConfigPlan::Same
        }

        // Never asked: every configuration is the same as the published one.
        fn request_config_change(&self) -> OSStatus {
            0
        }

        fn names_changed(&self) {}

        fn store(&self, _cfg: &DriverConfig) {}

        fn set_status(&self, s: LinkStatus) {
            println!("link: {s:?}");
        }

        fn now_ns(&self) -> u64 {
            host_now_ns()
        }

        fn timebase(&self) -> Timebase {
            platform::production().timebase()
        }

        fn log(&self, _level: u8, msg: &str) {
            println!("link: {msg}");
        }
    }

    /// The checks' results.
    #[derive(Default)]
    struct Report {
        passed: Vec<&'static str>,
        failed: Vec<&'static str>,
        skipped: Vec<&'static str>,
    }

    impl Report {
        fn check(&mut self, name: &'static str, r: Result<String, String>) -> bool {
            match r {
                Ok(detail) => {
                    println!("PASS {name}: {detail}");
                    self.passed.push(name);
                    true
                }
                Err(detail) => {
                    println!("FAIL {name}: {detail}");
                    self.failed.push(name);
                    false
                }
            }
        }

        fn skip(&mut self, name: &'static str, why: &str) {
            println!("SKIP {name}: {why}");
            self.skipped.push(name);
        }

        /// Prints the summary line; returns the exit status.
        fn finish(self) -> i32 {
            let total = self.passed.len() + self.failed.len();
            if self.failed.is_empty() {
                println!(
                    "SELFTEST-SUMMARY PASS {} ({total} of {total}{})",
                    self.passed.join(" "),
                    if self.skipped.is_empty() {
                        String::new()
                    } else {
                        format!(", skipped {}", self.skipped.join(" "))
                    }
                );
                0
            } else {
                println!(
                    "SELFTEST-SUMMARY FAIL {} ({} of {total} passed)",
                    self.failed.join(","),
                    self.passed.len()
                );
                1
            }
        }
    }

    /// The daemon's clock record, if it has a consistent one.
    fn clock_record(view: RegionRef<'_>) -> Option<ClockRecord> {
        match view.clock().read_bounded(READ_TRIES) {
            ClockRead::Record(r) => Some(r),
            ClockRead::NeverWritten | ClockRead::Contended => None,
        }
    }

    /// Waits for the welcome, and for the link to stage its configuration.
    fn welcome(link: &Link, recorder: &Recorder) -> Result<u64, String> {
        let end = Instant::now() + WELCOME_TIMEOUT;
        loop {
            match link.status() {
                LinkStatus::Attached { generation } if recorder.config().is_some() => {
                    return Ok(generation);
                }
                LinkStatus::Incompatible(reason) => {
                    return Err(format!("the daemon is incompatible ({reason})"));
                }
                s if Instant::now() >= end => {
                    return Err(format!(
                        "no welcome within {} s (link {s:?})",
                        WELCOME_TIMEOUT.as_secs()
                    ));
                }
                _ => sleep(Duration::from_millis(10)),
            }
        }
    }

    fn region(a: &Attachment, generation: u64) -> Result<String, String> {
        let h = a.view.header();
        let detail = format!(
            "region_size {}, mapped {} bytes, daemon {} pid {}, generation {:016x}",
            h.region_size,
            a.mapped.len(),
            h.daemon_version(),
            h.daemon_pid,
            h.daemon_generation
        );
        let valid = h.validate();
        if h.region_size != REGION_SIZE as u64 || a.mapped.len() != REGION_SIZE {
            Err(format!("{detail}; want {REGION_SIZE}"))
        } else if let Err(e) = valid {
            Err(format!("{detail}; header invalid: {e}"))
        } else if h.daemon_generation != generation {
            Err(format!("{detail}; the welcome said {generation:016x}"))
        } else {
            Ok(format!("{detail}; header valid"))
        }
    }

    fn heartbeat(view: RegionRef<'_>) -> Result<String, String> {
        let d = view.daemon();
        let first = d.heartbeat_ns.load(std::sync::atomic::Ordering::Acquire);
        let end = Instant::now() + HEARTBEAT_TIMEOUT;
        loop {
            let beat = d.heartbeat_ns.load(std::sync::atomic::Ordering::Acquire);
            let now = host_now_ns();
            if beat != first {
                let behind = i128::from(now) - i128::from(beat);
                let detail = format!(
                    "advanced by {:.1} ms; {:.3} ms behind CLOCK_UPTIME_RAW",
                    beat.wrapping_sub(first) as f64 / 1e6,
                    behind as f64 / 1e6
                );
                return if behind.unsigned_abs() <= u128::from(HEARTBEAT_TOLERANCE_NS) {
                    Ok(detail)
                } else {
                    Err(format!("{detail}, more than 10 ms off"))
                };
            }
            if Instant::now() >= end {
                return Err(format!("stuck at {first} for {} s", HEARTBEAT_TIMEOUT.as_secs()));
            }
            sleep(Duration::from_millis(1));
        }
    }

    fn clock(view: RegionRef<'_>, timeout: Duration) -> Result<String, String> {
        let start = Instant::now();
        let mut last = None;
        loop {
            if let Some(r) = clock_record(view) {
                if r.valid && matches!(r.state, ClockState::Locked | ClockState::FreeRunning) {
                    return Ok(format!(
                        "{:?} after {:.1} s, {:+.3} ppm, grandmaster {:016x}, step_gen {}",
                        r.state,
                        start.elapsed().as_secs_f64(),
                        r.snapshot.freq_offset_ppb() / 1000.0,
                        r.grandmaster,
                        r.step_gen
                    ));
                }
                last = Some(r);
            }
            if start.elapsed() >= timeout {
                return Err(match last {
                    Some(r) => format!(
                        "after {:.0} s: {:?}, valid {}",
                        timeout.as_secs_f64(),
                        r.state,
                        r.valid
                    ),
                    None => format!("never written in {:.0} s", timeout.as_secs_f64()),
                });
            }
            sleep(Duration::from_millis(100));
        }
    }

    fn engine(
        view: RegionRef<'_>,
        cfg: &DriverConfig,
        timeout: Duration,
    ) -> Result<String, String> {
        let d = view.daemon();
        let start = Instant::now();
        loop {
            let flags = d.flags.load(std::sync::atomic::Ordering::Acquire);
            let audio = AudioWord::unpack(d.audio_word.load(std::sync::atomic::Ordering::Acquire));
            let running = flags & DAEMON_ENGINE_RUNNING != 0;
            if running && audio.sample_rate == cfg.sample_rate {
                return Ok(format!(
                    "running at {} Hz, configuration {}",
                    audio.sample_rate, audio.config_gen
                ));
            }
            if start.elapsed() >= timeout {
                return Err(format!(
                    "flags {flags:#x}, rate {} Hz; the configuration says {} Hz",
                    audio.sample_rate, cfg.sample_rate
                ));
            }
            sleep(Duration::from_millis(100));
        }
    }

    /// The pattern's samples: distinct 16-bit codes, left-justified, so they
    /// survive any Dante encoding bit-exactly; never 0, which is silence.
    fn pattern(len: usize) -> Vec<i32> {
        let mut x: u32 = 0x2545_f491;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let code = (x & 0xffff).max(1);
                (code << 16) as i32
            })
            .collect()
    }

    fn network_loop(
        view: RegionRef<'_>,
        cfg: &DriverConfig,
        channel: usize,
    ) -> Result<String, String> {
        let rate = cfg.sample_rate;
        let available = cfg.input_channels.min(cfg.output_channels) as usize;
        if channel > available {
            return Err(format!("channel {channel}, but the device has {available}"));
        }
        let (Some(tx), Some(rx)) = (view.tx(channel - 1), view.rx(channel - 1)) else {
            return Err(format!("no ring for channel {channel}"));
        };
        let Some(r) = clock_record(view).filter(|r| r.valid) else {
            return Err("the daemon's clock is not valid".to_owned());
        };
        let samples = pattern((u64::from(rate) * PATTERN_MS / 1000) as usize);
        let media_now = ns_to_samples(r.snapshot.media_ns_at(host_now_ns()), rate);
        let at = media_now + u64::from(rate) * PATTERN_LEAD_MS / 1000;
        tx.write(at, &samples);
        let written = Instant::now();

        let mut seen = vec![false; samples.len()];
        let (mut found, mut wrong) = (0usize, 0usize);
        let mut first_wrong = None;
        loop {
            for (i, want) in samples.iter().enumerate() {
                if seen[i] {
                    continue;
                }
                match rx.read_one(at + i as u64) {
                    Some(v) if v == *want => found += 1,
                    Some(v) => {
                        wrong += 1;
                        first_wrong.get_or_insert((i, v, *want));
                    }
                    None => continue,
                }
                seen[i] = true;
            }
            if found + wrong == samples.len() || written.elapsed() >= LOOP_TIMEOUT {
                break;
            }
            sleep(Duration::from_millis(5));
        }
        let detail = format!(
            "{found} of {} frames written to TX channel {channel} at media sample {at} came back \
             on RX channel {channel} at the same indices",
            samples.len()
        );
        match first_wrong {
            None if found == samples.len() => {
                Ok(format!("{detail} within {:.0} ms", written.elapsed().as_secs_f64() * 1e3))
            }
            None => Err(format!("{detail} within {} s", LOOP_TIMEOUT.as_secs())),
            Some((i, got, want)) => Err(format!(
                "{detail}; {wrong} wrong, the first at frame {i}: {got:#010x}, want {want:#010x}"
            )),
        }
    }

    pub fn main() -> i32 {
        let o = match Options::parse(std::env::args().skip(1)) {
            Ok(Some(o)) => o,
            Ok(None) => {
                println!("{USAGE}");
                return 0;
            }
            Err(msg) => {
                eprintln!("ovsc-hal-selftest: {msg}\n{USAGE}");
                return 2;
            }
        };
        let recorder = Arc::new(Recorder { slot: AttachSlot::new(), config: Mutex::new(None) });
        let transport = Arc::new(XpcClient::new(&o.service));
        let instance = platform::production().random_u64();
        let pid = std::process::id() as i32;
        println!("connecting to {} as instance {instance:016x}, pid {pid}", o.service);
        let link = Link::new(transport, recorder.clone(), instance, pid);
        link.start();

        let mut report = Report::default();
        let generation = match welcome(&link, &recorder) {
            Ok(g) => g,
            Err(e) => {
                report.check("welcome", Err(e));
                return report.finish();
            }
        };
        // Hold the attachment for the rest of the run, so that the link
        // keeps it mapped even if the daemon restarts meanwhile.
        let guard = recorder.slot.enter();
        let (Some(a), Some(cfg)) = (guard.get(), recorder.config()) else {
            report.check("welcome", Err("attached, but nothing was handed over".to_owned()));
            return report.finish();
        };
        report.check(
            "welcome",
            Ok(format!(
                "generation {generation:016x}, configuration {}: {} Hz, {} in, {} out",
                cfg.config_gen, cfg.sample_rate, cfg.input_channels, cfg.output_channels
            )),
        );
        let view = a.view;
        report.check("region", region(a, generation));
        report.check("heartbeat", heartbeat(view));
        let clock_ok = report.check("clock", clock(view, o.clock_timeout));
        report.check("engine", engine(view, &cfg, o.clock_timeout));
        if !o.run_loop {
            report.skip("loop", "--no-loop");
        } else if clock_ok {
            report.check("loop", network_loop(view, &cfg, o.channel));
        } else {
            report.check("loop", Err("no usable clock".to_owned()));
        }
        drop(guard);
        report.finish()
    }
}

fn main() {
    #[cfg(target_os = "macos")]
    std::process::exit(selftest::main());
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("ovsc-hal-selftest: macOS only");
        std::process::exit(1);
    }
}
