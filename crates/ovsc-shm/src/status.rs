//! Status blocks of the shared region: the daemon's status, the plug-in's
//! counters and the IO trace.
//!
//! Every field is an `AtomicU64` with a single writer, so plain loads and
//! stores are enough; there is no seqlock. Signed values are stored as their
//! i64 bits, floats as their f64 bits. Packed words go through the
//! [`AudioWord`], [`ChannelsWord`] and [`AppliedWord`] helpers.

#![forbid(unsafe_code)]

use core::sync::atomic::AtomicU64;

/// `DaemonStatus::flags` bit: the device engine is running.
pub const DAEMON_ENGINE_RUNNING: u64 = 1;
/// `DaemonStatus::flags` bit: the daemon is shutting down.
pub const DAEMON_SHUTTING_DOWN: u64 = 2;

/// `PluginStatus::regime`: the plug-in runs on its own synthetic clock.
pub const REGIME_SYNTHETIC: u64 = 0;
/// `PluginStatus::regime`: the plug-in follows the daemon's clock.
pub const REGIME_FOLLOWING: u64 = 1;
/// `PluginStatus::regime`: the plug-in extrapolates the last clock it
/// followed.
pub const REGIME_HOLDOVER: u64 = 2;

/// `DaemonStatus::audio_word`: `sample_rate | config_gen << 32`. A word of 0
/// means the engine is not running.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioWord {
    pub sample_rate: u32,
    pub config_gen: u32,
}

impl AudioWord {
    pub const fn pack(self) -> u64 {
        self.sample_rate as u64 | (self.config_gen as u64) << 32
    }

    pub const fn unpack(w: u64) -> Self {
        Self { sample_rate: w as u32, config_gen: (w >> 32) as u32 }
    }
}

/// `DaemonStatus::channels_word`: `rx | tx << 16 | latency_samples << 32`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChannelsWord {
    pub rx: u16,
    pub tx: u16,
    pub latency_samples: u32,
}

impl ChannelsWord {
    pub const fn pack(self) -> u64 {
        self.rx as u64 | (self.tx as u64) << 16 | (self.latency_samples as u64) << 32
    }

    pub const fn unpack(w: u64) -> Self {
        Self { rx: w as u16, tx: (w >> 16) as u16, latency_samples: (w >> 32) as u32 }
    }
}

/// `PluginStatus::applied_word`: the configuration the plug-in last applied,
/// `sample_rate | config_gen << 32`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AppliedWord {
    pub sample_rate: u32,
    pub config_gen: u32,
}

impl AppliedWord {
    pub const fn pack(self) -> u64 {
        self.sample_rate as u64 | (self.config_gen as u64) << 32
    }

    pub const fn unpack(w: u64) -> Self {
        Self { sample_rate: w as u32, config_gen: (w >> 32) as u32 }
    }
}

const fn zero() -> AtomicU64 {
    AtomicU64::new(0)
}

/// The daemon's status (256 bytes). Written by the daemon, read by the
/// plug-in.
#[repr(C, align(128))]
#[derive(Debug)]
pub struct DaemonStatus {
    /// Host nanoseconds, written at 10 Hz while the daemon is alive.
    pub heartbeat_ns: AtomicU64,
    /// An [`AudioWord`]; 0 while the engine is not running.
    pub audio_word: AtomicU64,
    /// A [`ChannelsWord`].
    pub channels_word: AtomicU64,
    pub tx_guard_samples: AtomicU64,
    /// [`DAEMON_ENGINE_RUNNING`] | [`DAEMON_SHUTTING_DOWN`].
    pub flags: AtomicU64,
    pub tx_packets: AtomicU64,
    pub tx_underruns: AtomicU64,
    pub rx_packets: AtomicU64,
    pub rx_late_packets: AtomicU64,
    /// Attached plug-in connections.
    pub peers: AtomicU64,
    pub(crate) _reserved: [AtomicU64; 22],
}

impl DaemonStatus {
    pub const fn new() -> Self {
        Self {
            heartbeat_ns: zero(),
            audio_word: zero(),
            channels_word: zero(),
            tx_guard_samples: zero(),
            flags: zero(),
            tx_packets: zero(),
            tx_underruns: zero(),
            rx_packets: zero(),
            rx_late_packets: zero(),
            peers: zero(),
            _reserved: [const { zero() }; 22],
        }
    }
}

impl Default for DaemonStatus {
    fn default() -> Self {
        Self::new()
    }
}

/// The plug-in's counters (512 bytes). Written by the plug-in, read by the
/// daemon's status logger.
///
/// Each writer context owns whole cache lines: line 0 (0x00) the IO thread,
/// line 1 (0x80) GetZeroTimeStamp, line 2 (0x100, to the end) the IPC queue.
#[repr(C, align(128))]
#[derive(Debug)]
pub struct PluginStatus {
    // Line 0: the IO thread.
    pub read_calls: AtomicU64,
    pub write_calls: AtomicU64,
    pub input_frames: AtomicU64,
    /// Channel-frames read as silence because the ring had no sample.
    pub input_missing: AtomicU64,
    pub output_frames: AtomicU64,
    /// IO operations that were zero-filled or discarded by the gate.
    pub silenced_cycles: AtomicU64,
    pub late_output_cycles: AtomicU64,
    pub early_input_cycles: AtomicU64,
    /// Smallest output margin since StartIO, i64 bits.
    pub min_output_margin: AtomicU64,
    /// Smallest input margin since StartIO, i64 bits.
    pub min_input_margin: AtomicU64,
    pub max_frames: AtomicU64,
    pub last_frames: AtomicU64,
    pub io_heartbeat_ns: AtomicU64,
    pub faulted: AtomicU64,
    /// IO operations longer than `IO_FRAMES_CAP`.
    pub frames_capped: AtomicU64,
    pub(crate) _reserved0: AtomicU64,
    // Line 1: GetZeroTimeStamp.
    pub zts_calls: AtomicU64,
    pub seed: AtomicU64,
    /// Ring index minus device sample time, i64 bits.
    pub media_offset: AtomicU64,
    /// One of the `REGIME_*` codes.
    pub regime: AtomicU64,
    pub absorbs: AtomicU64,
    pub seed_bumps: AtomicU64,
    pub last_zts_sample: AtomicU64,
    pub last_zts_host_ticks: AtomicU64,
    /// Device rate relative to nominal, in thousandths of a ppm, i64 bits.
    pub device_rate_ppm_milli: AtomicU64,
    /// i64 bits.
    pub phase_error_ns: AtomicU64,
    pub gate: AtomicU64,
    pub clock_read_failures: AtomicU64,
    pub(crate) _reserved1: [AtomicU64; 4],
    // Line 2: the IPC queue.
    pub plugin_instance: AtomicU64,
    pub plugin_pid: AtomicU64,
    /// An [`AppliedWord`].
    pub applied_word: AtomicU64,
    pub io_clients: AtomicU64,
    pub attach_count: AtomicU64,
    pub attached_generation: AtomicU64,
    pub(crate) _reserved2: [AtomicU64; 26],
}

impl PluginStatus {
    pub const fn new() -> Self {
        Self {
            read_calls: zero(),
            write_calls: zero(),
            input_frames: zero(),
            input_missing: zero(),
            output_frames: zero(),
            silenced_cycles: zero(),
            late_output_cycles: zero(),
            early_input_cycles: zero(),
            min_output_margin: zero(),
            min_input_margin: zero(),
            max_frames: zero(),
            last_frames: zero(),
            io_heartbeat_ns: zero(),
            faulted: zero(),
            frames_capped: zero(),
            _reserved0: zero(),
            zts_calls: zero(),
            seed: zero(),
            media_offset: zero(),
            regime: zero(),
            absorbs: zero(),
            seed_bumps: zero(),
            last_zts_sample: zero(),
            last_zts_host_ticks: zero(),
            device_rate_ppm_milli: zero(),
            phase_error_ns: zero(),
            gate: zero(),
            clock_read_failures: zero(),
            _reserved1: [const { zero() }; 4],
            plugin_instance: zero(),
            plugin_pid: zero(),
            applied_word: zero(),
            io_clients: zero(),
            attach_count: zero(),
            attached_generation: zero(),
            _reserved2: [const { zero() }; 26],
        }
    }
}

impl Default for PluginStatus {
    fn default() -> Self {
        Self::new()
    }
}

/// The IO trace header (128 bytes), followed in the region by
/// `IO_TRACE_ENTRIES` [`IoTraceEntry`]s. Written by the plug-in.
#[repr(C, align(128))]
#[derive(Debug)]
pub struct IoTraceHeader {
    /// Incremented when IO starts (StartIO 0 to 1).
    pub session: AtomicU64,
    /// Entries written in this session.
    pub next: AtomicU64,
    pub(crate) _reserved: [AtomicU64; 14],
}

impl IoTraceHeader {
    pub const fn new() -> Self {
        Self { session: zero(), next: zero(), _reserved: [const { zero() }; 14] }
    }
}

impl Default for IoTraceHeader {
    fn default() -> Self {
        Self::new()
    }
}

/// One traced IO operation (64 bytes).
#[repr(C, align(64))]
#[derive(Debug)]
pub struct IoTraceEntry {
    pub cycle_counter: AtomicU64,
    /// Operation fourcc `| stream id << 32`.
    pub op_stream: AtomicU64,
    /// Frames `| nominal frames << 32`.
    pub frames: AtomicU64,
    /// `mCurrentTime.mSampleTime`, f64 bits.
    pub current_sample: AtomicU64,
    /// `mCurrentTime.mHostTime`, Mach ticks.
    pub current_host_ticks: AtomicU64,
    /// `mInputTime.mSampleTime`, f64 bits.
    pub input_sample: AtomicU64,
    /// `mOutputTime.mSampleTime`, f64 bits.
    pub output_sample: AtomicU64,
    /// Mach ticks at the end of the operation.
    pub done_host_ticks: AtomicU64,
}

impl IoTraceEntry {
    pub const fn new() -> Self {
        Self {
            cycle_counter: zero(),
            op_stream: zero(),
            frames: zero(),
            current_sample: zero(),
            current_host_ticks: zero(),
            input_sample: zero(),
            output_sample: zero(),
            done_host_ticks: zero(),
        }
    }
}

impl Default for IoTraceEntry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_round_trip() {
        let a = AudioWord { sample_rate: 192_000, config_gen: 0xDEAD_BEEF };
        assert_eq!(a.pack(), 192_000 | 0xDEAD_BEEF << 32);
        assert_eq!(AudioWord::unpack(a.pack()), a);
        assert_eq!(AudioWord::unpack(0), AudioWord::default());

        let c = ChannelsWord { rx: 128, tx: 0xFFFF, latency_samples: 768 };
        assert_eq!(c.pack(), 128 | 0xFFFF << 16 | 768 << 32);
        assert_eq!(ChannelsWord::unpack(c.pack()), c);

        let p = AppliedWord { sample_rate: 44_100, config_gen: 7 };
        assert_eq!(p.pack(), 44_100 | 7 << 32);
        assert_eq!(AppliedWord::unpack(p.pack()), p);
    }
}
