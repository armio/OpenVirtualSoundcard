//! Statistics shared lock-free between audio callbacks and the bridge.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed};

use ovsc_clock::local_now_ns;

use crate::controller::DriftController;

/// A stream that hasn't called back for this many buffer periods...
const STALL_PERIODS: u64 = 4;
/// ...and at least this long (ns) counts as stalled.
const STALL_MIN_NS: u64 = 100_000_000;
/// Before its first callback, a stream gets this long (ns) to start.
const START_GRACE_NS: u64 = 2_000_000_000;

/// Health of one direction of a [`Bridge`](crate::Bridge).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DirectionStats {
    /// Name of the device.
    pub device: String,
    /// Device channels the stream has open.
    pub channels: u16,
    /// Device sample format (`"f32"`, `"i32"`, `"i24"` or `"i16"`).
    pub sample_format: String,
    /// The callback size the stream allows for: the largest number of
    /// frames the device delivered in one callback over the last 10 to
    /// 20 seconds.
    pub buffer_frames: u32,
    /// Latency the bridge adds in this direction, in milliseconds: one
    /// callback (`buffer_frames`), the interpolator's 32-sample look-ahead
    /// and the margin (capture: plus the transmit packet lead). Playback
    /// adds it to the network receive latency, capture to the transmit
    /// path; the device's own latency comes on top of either.
    pub latency_ms: f64,
    /// Whether the media clock was available at the last callback.
    pub clock_ok: bool,
    /// Whether audio is flowing (the stream is past settling and calling
    /// back).
    pub running: bool,
    /// Whether the drift-tracking loop has converged (and the stream is
    /// calling back).
    pub locked: bool,
    /// The device stopped calling back without reporting an error: no
    /// callback for four buffer periods (at least 100 ms; 2 s before the
    /// first one). The stream is silent; reopening the bridge may help.
    pub stalled: bool,
    /// Time since the last callback (or since the stream started), ms.
    pub since_callback_ms: f64,
    /// Resampling correction in ppm: media samples per device frame, minus
    /// one. Positive when the device clock runs slow relative to the
    /// network.
    pub ratio_ppm: f64,
    /// Filtered timing error (target minus actual stream position), in
    /// media samples. Close to zero once locked.
    pub error_samples: f64,
    /// Device callbacks so far.
    pub callbacks: u64,
    /// Callbacks that touched audio too close to the network's edge:
    /// playback read samples that may not have arrived yet; capture wrote
    /// samples that may already have been sent.
    pub underruns: u64,
    /// Callbacks that touched audio so far from the network's edge that the
    /// ring may already have reused its slots.
    pub overruns: u64,
    /// Times the stream jumped back onto its target (a device gap or xrun,
    /// a stall, a media-clock step, a larger callback) or realigned: each
    /// is a dip of a few milliseconds.
    pub realigns: u64,
    /// Errors reported by the audio backend (xruns, disconnects, ...).
    pub device_errors: u64,
    /// The backend reported the device as gone; the stream is dead.
    pub device_lost: bool,
    /// The backend moved the stream to another device (a stream opened on
    /// the system default follows it): `device` may no longer be the one
    /// in use.
    pub device_changed: bool,
    /// Whether the callback thread runs with real-time priority: `Some(true)`
    /// once raised, `Some(false)` if the system refused (on Linux, grant
    /// `CAP_SYS_NICE` or run as root), `None` if the bridge didn't try (the
    /// platform's audio thread is real-time already, or the device is
    /// served by a sound server) or hasn't yet.
    pub realtime: Option<bool>,
}

/// Values of [`StatsCell::realtime`].
const RT_UNKNOWN: u8 = 0;
const RT_YES: u8 = 1;
const RT_REFUSED: u8 = 2;

/// The atomics behind a [`DirectionStats`].
#[derive(Debug)]
pub struct StatsCell {
    buffer_frames: AtomicU32,
    latency_bits: AtomicU64,
    stall_ns: AtomicU64,
    last_callback_ns: AtomicU64,
    clock_ok: AtomicBool,
    running: AtomicBool,
    locked: AtomicBool,
    ratio_bits: AtomicU64,
    error_bits: AtomicU64,
    callbacks: AtomicU64,
    underruns: AtomicU64,
    overruns: AtomicU64,
    realigns: AtomicU64,
    device_errors: AtomicU64,
    device_lost: AtomicBool,
    device_changed: AtomicBool,
    /// The backend reported an xrun the data callback hasn't seen yet.
    xrun: AtomicBool,
    realtime: AtomicU8,
}

impl Default for StatsCell {
    fn default() -> Self {
        Self::new()
    }
}

impl StatsCell {
    pub fn new() -> Self {
        StatsCell {
            buffer_frames: AtomicU32::new(0),
            latency_bits: AtomicU64::new(0f64.to_bits()),
            stall_ns: AtomicU64::new(STALL_MIN_NS),
            // Measured from creation until the first callback.
            last_callback_ns: AtomicU64::new(local_now_ns()),
            clock_ok: AtomicBool::new(false),
            running: AtomicBool::new(false),
            locked: AtomicBool::new(false),
            ratio_bits: AtomicU64::new(1f64.to_bits()),
            error_bits: AtomicU64::new(0f64.to_bits()),
            callbacks: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            overruns: AtomicU64::new(0),
            realigns: AtomicU64::new(0),
            device_errors: AtomicU64::new(0),
            device_lost: AtomicBool::new(false),
            device_changed: AtomicBool::new(false),
            xrun: AtomicBool::new(false),
            realtime: AtomicU8::new(RT_UNKNOWN),
        }
    }

    /// Counts a callback and notes when it came. Reading the monotonic
    /// clock is a vDSO call (no system call) on the platforms cpal serves.
    #[inline]
    pub fn callback(&self) {
        self.callbacks.fetch_add(1, Relaxed);
        self.last_callback_ns.store(local_now_ns(), Relaxed);
    }

    #[inline]
    pub fn underrun(&self) {
        self.underruns.fetch_add(1, Relaxed);
    }

    #[inline]
    pub fn overrun(&self) {
        self.overruns.fetch_add(1, Relaxed);
    }

    /// Records the callback size the stream allows for, and the resulting
    /// safety distance (media samples at `rate`).
    pub fn set_buffer(&self, frames: usize, safety: u64, rate: u32) {
        let rate = rate.max(1) as u64;
        self.buffer_frames.store(frames.min(u32::MAX as usize) as u32, Relaxed);
        let latency_ms = safety as f64 * 1000.0 / rate as f64;
        self.latency_bits.store(latency_ms.to_bits(), Relaxed);
        let period_ns = frames as u64 * 1_000_000_000 / rate;
        self.stall_ns.store((STALL_PERIODS * period_ns).max(STALL_MIN_NS), Relaxed);
    }

    /// Publishes the controller state after a callback.
    #[inline]
    pub fn publish(&self, controller: &DriftController, clock_ok: bool) {
        self.clock_ok.store(clock_ok, Relaxed);
        self.running.store(controller.running(), Relaxed);
        self.locked.store(controller.locked(), Relaxed);
        self.ratio_bits.store(controller.ratio().to_bits(), Relaxed);
        self.error_bits.store(controller.error().to_bits(), Relaxed);
        self.realigns.store(controller.realigns(), Relaxed);
    }

    /// Whether the backend reported an xrun since the last call. Called by
    /// the data callback.
    #[inline]
    pub fn take_xrun(&self) -> bool {
        self.xrun.load(Relaxed) && self.xrun.swap(false, Relaxed)
    }

    /// Records whether raising the callback thread's priority worked.
    pub fn set_realtime(&self, ok: bool) {
        self.realtime.store(if ok { RT_YES } else { RT_REFUSED }, Relaxed);
    }

    /// Records an error reported by the audio backend. Called from the
    /// backend's error callback, which can run on the audio thread: atomics
    /// only.
    pub fn device_error(&self, error: &cpal::Error) {
        use cpal::ErrorKind;
        match error.kind() {
            // Not an audio error: the backend couldn't make its thread
            // real-time (cpal's `realtime` feature, e.g. WASAPI's MMCSS).
            ErrorKind::RealtimeDenied => {
                self.realtime.store(RT_REFUSED, Relaxed);
                return;
            }
            ErrorKind::Xrun => self.xrun.store(true, Relaxed),
            ErrorKind::DeviceChanged => self.device_changed.store(true, Relaxed),
            // WASAPI reports the audio service going away as an unavailable
            // host, and ends the stream.
            ErrorKind::DeviceNotAvailable
            | ErrorKind::StreamInvalidated
            | ErrorKind::HostUnavailable => self.device_lost.store(true, Relaxed),
            _ => {}
        }
        self.device_errors.fetch_add(1, Relaxed);
    }

    pub fn snapshot(&self, device: &str, channels: u16, sample_format: &str) -> DirectionStats {
        let callbacks = self.callbacks.load(Relaxed);
        let idle_ns = local_now_ns().saturating_sub(self.last_callback_ns.load(Relaxed));
        let stalled = is_stalled(idle_ns, callbacks, self.stall_ns.load(Relaxed));
        DirectionStats {
            device: device.to_owned(),
            channels,
            sample_format: sample_format.to_owned(),
            buffer_frames: self.buffer_frames.load(Relaxed),
            latency_ms: f64::from_bits(self.latency_bits.load(Relaxed)),
            clock_ok: self.clock_ok.load(Relaxed),
            // What the last callback published is stale once callbacks stop.
            running: self.running.load(Relaxed) && !stalled,
            locked: self.locked.load(Relaxed) && !stalled,
            stalled,
            since_callback_ms: idle_ns as f64 / 1e6,
            ratio_ppm: (f64::from_bits(self.ratio_bits.load(Relaxed)) - 1.0) * 1e6,
            error_samples: f64::from_bits(self.error_bits.load(Relaxed)),
            callbacks,
            underruns: self.underruns.load(Relaxed),
            overruns: self.overruns.load(Relaxed),
            realigns: self.realigns.load(Relaxed),
            device_errors: self.device_errors.load(Relaxed),
            device_lost: self.device_lost.load(Relaxed),
            device_changed: self.device_changed.load(Relaxed),
            realtime: match self.realtime.load(Relaxed) {
                RT_YES => Some(true),
                RT_REFUSED => Some(false),
                _ => None,
            },
        }
    }
}

/// Whether a stream that last called back `idle_ns` ago (or started then,
/// if `callbacks` is 0) has stalled, given its stall threshold.
fn is_stalled(idle_ns: u64, callbacks: u64, stall_ns: u64) -> bool {
    let limit = if callbacks == 0 { START_GRACE_NS } else { stall_ns };
    idle_ns > limit
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(kind: cpal::ErrorKind) -> cpal::Error {
        cpal::Error::with_message(kind, "test")
    }

    #[test]
    fn device_errors_are_classified() {
        let cell = StatsCell::new();
        cell.device_error(&error(cpal::ErrorKind::Xrun));
        let s = cell.snapshot("d", 2, "f32");
        assert_eq!((s.device_errors, s.device_lost, s.device_changed), (1, false, false));
        assert!(cell.take_xrun());
        assert!(!cell.take_xrun(), "an xrun is reported once");

        cell.device_error(&error(cpal::ErrorKind::RealtimeDenied));
        let s = cell.snapshot("d", 2, "f32");
        assert_eq!(s.device_errors, 1, "a refused priority is not an audio error");
        assert_eq!(s.realtime, Some(false));

        cell.device_error(&error(cpal::ErrorKind::DeviceChanged));
        assert!(cell.snapshot("d", 2, "f32").device_changed);
        assert!(!cell.snapshot("d", 2, "f32").device_lost);
        for kind in [cpal::ErrorKind::DeviceNotAvailable, cpal::ErrorKind::HostUnavailable] {
            let cell = StatsCell::new();
            cell.device_error(&error(kind));
            assert!(cell.snapshot("d", 2, "f32").device_lost, "{kind:?}");
        }
    }

    #[test]
    fn stall_threshold_follows_the_buffer() {
        let cell = StatsCell::new();
        // 4096 frames at 48 kHz: four periods are 341 ms.
        cell.set_buffer(4096, 4096 + 32 + 48, 48_000);
        assert_eq!(cell.stall_ns.load(Relaxed), 341_333_332);
        let s = cell.snapshot("d", 2, "f32");
        assert_eq!(s.buffer_frames, 4096);
        assert!((s.latency_ms - 4176.0 / 48.0).abs() < 1e-9);
        // Small buffers: at least 100 ms.
        cell.set_buffer(64, 144, 48_000);
        assert_eq!(cell.stall_ns.load(Relaxed), STALL_MIN_NS);

        assert!(!is_stalled(1_900_000_000, 0, STALL_MIN_NS), "starting");
        assert!(is_stalled(2_100_000_000, 0, STALL_MIN_NS));
        assert!(!is_stalled(90_000_000, 5, STALL_MIN_NS));
        assert!(is_stalled(110_000_000, 5, STALL_MIN_NS));
    }

    #[test]
    fn a_stalled_stream_is_not_reported_locked() {
        let cell = StatsCell::new();
        cell.running.store(true, Relaxed);
        cell.locked.store(true, Relaxed);
        cell.callback();
        let s = cell.snapshot("d", 2, "f32");
        assert!(s.running && s.locked && !s.stalled);
        // The last callback was long ago.
        cell.last_callback_ns.store(local_now_ns() - 500_000_000, Relaxed);
        let s = cell.snapshot("d", 2, "f32");
        assert!(s.stalled && !s.running && !s.locked, "{s:?}");
        assert!(s.since_callback_ms >= 500.0);
    }
}
