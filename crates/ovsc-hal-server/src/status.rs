//! The daemon's side of the region's status blocks (design sections 6.4 to
//! 6.6 and 14.3): a task that writes the heartbeat, the device's packet
//! counters and the peer count ten times a second, logs a status line every
//! `status_log_interval`, and prints the driver's IO trace once per IO
//! session.

use std::fmt;
use std::sync::Weak;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tokio::time::MissedTickBehavior;
use tracing::info;

use ovsc_shm::layout::{IO_TRACE_ENTRIES, RegionRef};
use ovsc_shm::status::{
    IoTraceEntry, PluginStatus, REGIME_FOLLOWING, REGIME_HOLDOVER, REGIME_SYNTHETIC,
};

use crate::server::Inner;

/// How often the heartbeat is written.
pub const HEARTBEAT_PERIOD: Duration = Duration::from_millis(100);

/// A partial IO trace is printed once it stopped growing for this many
/// heartbeats (IO stopped before filling it).
const TRACE_QUIET_TICKS: u32 = 10;

/// The driver link at a glance, for the daemon's status line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HalStatus {
    /// Driver connections that got a welcome.
    pub peers: usize,
    /// Whether the engine runs and the driver may stream.
    pub engine_running: bool,
    /// The configuration generation offered to the driver.
    pub config_gen: u64,
    /// Core Audio clients doing IO on the device.
    pub io_clients: u64,
    /// Whether the driver's IO gate is open.
    pub gate: bool,
    /// One of the `REGIME_*` codes of `ovsc_shm::status`.
    pub regime: u64,
    /// Discontinuities the driver absorbed into its ring offset.
    pub absorbs: u64,
    /// The driver's zero-timestamp seed.
    pub seed: u64,
    pub late_output_cycles: u64,
    pub early_input_cycles: u64,
    /// IO operations the gate silenced.
    pub silenced_cycles: u64,
    /// Input channel-frames that had no sample.
    pub input_missing: u64,
    /// Packets the device sent with a channel the driver had not filled.
    pub tx_underruns: u64,
    /// Smallest output margin since IO started, in frames.
    pub min_output_margin: i64,
    /// Smallest input margin since IO started, in frames.
    pub min_input_margin: i64,
    /// Whether the driver caught a panic and went silent.
    pub faulted: bool,
}

impl HalStatus {
    /// Reads the driver's counters from `view`; the rest comes from the
    /// server.
    pub(crate) fn read(
        view: RegionRef<'_>,
        peers: usize,
        engine_running: bool,
        config_gen: u64,
    ) -> Self {
        let p: &PluginStatus = view.plugin();
        let get = |w: &std::sync::atomic::AtomicU64| w.load(Ordering::Relaxed);
        Self {
            peers,
            engine_running,
            config_gen,
            io_clients: get(&p.io_clients),
            gate: get(&p.gate) != 0,
            regime: get(&p.regime),
            absorbs: get(&p.absorbs),
            seed: get(&p.seed),
            late_output_cycles: get(&p.late_output_cycles),
            early_input_cycles: get(&p.early_input_cycles),
            silenced_cycles: get(&p.silenced_cycles),
            input_missing: get(&p.input_missing),
            tx_underruns: get(&view.daemon().tx_underruns),
            min_output_margin: get(&p.min_output_margin) as i64,
            min_input_margin: get(&p.min_input_margin) as i64,
            faulted: get(&p.faulted) != 0,
        }
    }
}

impl fmt::Display for HalStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "peers={} io={} gate={} regime={} absorbs={} seed={} late_out={} early_in={} \
             silenced={} in_missing={} tx_underruns={} min_out_margin={} min_in_margin={}",
            self.peers,
            self.io_clients,
            u8::from(self.gate),
            regime_name(self.regime),
            self.absorbs,
            self.seed,
            self.late_output_cycles,
            self.early_input_cycles,
            self.silenced_cycles,
            self.input_missing,
            self.tx_underruns,
            self.min_output_margin,
            self.min_input_margin,
        )?;
        if !self.engine_running {
            f.write_str(" engine=stopped")?;
        }
        if self.faulted {
            f.write_str(" faulted=1")?;
        }
        Ok(())
    }
}

fn regime_name(code: u64) -> &'static str {
    match code {
        REGIME_SYNTHETIC => "synthetic",
        REGIME_FOLLOWING => "following",
        REGIME_HOLDOVER => "holdover",
        _ => "unknown",
    }
}

/// The status task: runs until the server is gone or the task is aborted.
pub(crate) async fn run(server: Weak<Inner>) {
    let mut tick = tokio::time::interval(HEARTBEAT_PERIOD);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut trace = TraceDump::default();
    let mut last_log = Instant::now();
    loop {
        tick.tick().await;
        let Some(server) = server.upgrade() else { return };
        server.beat();
        trace.poll(server.region.view());
        let interval = server.opts.status_log_interval;
        if !interval.is_zero() && last_log.elapsed() >= interval {
            last_log = Instant::now();
            info!("hal: {}", server.status());
        }
    }
}

/// Prints each IO session's trace once.
#[derive(Debug, Default)]
struct TraceDump {
    /// The last session printed.
    printed: u64,
    /// The session and entry count at the last poll.
    seen: (u64, u64),
    /// Polls since `seen` last changed.
    quiet: u32,
}

impl TraceDump {
    fn poll(&mut self, view: RegionRef<'_>) {
        let (header, entries) = view.io_trace();
        let session = header.session.load(Ordering::Acquire);
        let count = header.next.load(Ordering::Acquire).min(IO_TRACE_ENTRIES as u64);
        if (session, count) == self.seen {
            self.quiet = self.quiet.saturating_add(1);
        } else {
            self.seen = (session, count);
            self.quiet = 0;
        }
        if session == 0 || session == self.printed || count == 0 {
            return;
        }
        if count < IO_TRACE_ENTRIES as u64 && self.quiet < TRACE_QUIET_TICKS {
            return;
        }
        let lines: Vec<String> = entries.iter().take(count as usize).map(format_entry).collect();
        // A new session may have started meanwhile and overwritten entries;
        // its own trace gets printed instead.
        if header.session.load(Ordering::Acquire) != session {
            return;
        }
        self.printed = session;
        info!("hal: io trace of session {session}, {count} operations:");
        for (i, line) in lines.iter().enumerate() {
            info!("hal: io_trace {session} {i:2}: {line}");
        }
    }
}

fn format_entry(e: &IoTraceEntry) -> String {
    let get = |w: &std::sync::atomic::AtomicU64| w.load(Ordering::Relaxed);
    let op_stream = get(&e.op_stream);
    let frames = get(&e.frames);
    format!(
        "cycle={} op={} stream={} frames={}/{} current={}@{} input={} output={} done={}",
        get(&e.cycle_counter),
        fourcc(op_stream as u32),
        op_stream >> 32,
        frames as u32,
        frames >> 32,
        f64::from_bits(get(&e.current_sample)),
        get(&e.current_host_ticks),
        f64::from_bits(get(&e.input_sample)),
        f64::from_bits(get(&e.output_sample)),
        get(&e.done_host_ticks),
    )
}

/// A four-character code as text (`'read'`), or in hex if not printable.
fn fourcc(code: u32) -> String {
    let bytes = code.to_be_bytes();
    if bytes.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        bytes.iter().map(|&b| b as char).collect()
    } else {
        format!("{code:#010x}")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use super::*;
    use crate::region::HalRegion;

    #[test]
    fn status_reads_the_driver_counters() {
        let r = HalRegion::create("test").unwrap();
        let p = r.view().plugin();
        p.io_clients.store(2, Ordering::Relaxed);
        p.gate.store(1, Ordering::Relaxed);
        p.regime.store(REGIME_FOLLOWING, Ordering::Relaxed);
        p.absorbs.store(1, Ordering::Relaxed);
        p.seed.store(3, Ordering::Relaxed);
        p.min_output_margin.store((-5i64) as u64, Ordering::Relaxed);
        r.view().daemon().tx_underruns.store(4, Ordering::Relaxed);
        let s = HalStatus::read(r.view(), 1, true, 9);
        assert_eq!(s.min_output_margin, -5);
        assert_eq!(
            s.to_string(),
            "peers=1 io=2 gate=1 regime=following absorbs=1 seed=3 late_out=0 early_in=0 \
             silenced=0 in_missing=0 tx_underruns=4 min_out_margin=-5 min_in_margin=0"
        );
        p.faulted.store(1, Ordering::Relaxed);
        let s = HalStatus::read(r.view(), 0, false, 9);
        assert!(s.to_string().ends_with(" engine=stopped faulted=1"));
    }

    #[test]
    fn fourccs_print_as_text() {
        assert_eq!(fourcc(u32::from_be_bytes(*b"read")), "read");
        assert_eq!(fourcc(u32::from_be_bytes(*b"rite")), "rite");
        assert_eq!(fourcc(7), "0x00000007");
    }

    #[test]
    fn traces_are_printed_once_per_session() {
        let r = HalRegion::create("test").unwrap();
        let (header, entries) = r.view().io_trace();
        let set = |w: &AtomicU64, v: u64| w.store(v, Ordering::Release);
        let mut dump = TraceDump::default();
        // Nothing traced yet.
        dump.poll(r.view());
        assert_eq!(dump.printed, 0);
        // A session filling up: printed once full.
        set(&header.session, 1);
        set(&header.next, 10);
        dump.poll(r.view());
        assert_eq!(dump.printed, 0);
        set(&entries[0].op_stream, u32::from_be_bytes(*b"read") as u64 | 3 << 32);
        set(&entries[0].frames, 512 | 512 << 32);
        set(&header.next, IO_TRACE_ENTRIES as u64 + 5);
        dump.poll(r.view());
        assert_eq!(dump.printed, 1);
        assert!(format_entry(&entries[0]).contains("op=read stream=3 frames=512/512"));
        // A short session: printed once it stops growing.
        set(&header.session, 2);
        set(&header.next, 3);
        for _ in 0..TRACE_QUIET_TICKS {
            dump.poll(r.view());
            assert_eq!(dump.printed, 1);
        }
        dump.poll(r.view());
        assert_eq!(dump.printed, 2);
    }
}
