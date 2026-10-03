//! Statistics shared lock-free between audio callbacks and the bridge.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};

use crate::controller::DriftController;

/// Health of one direction of a [`Bridge`](crate::Bridge).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DirectionStats {
    /// Name of the device.
    pub device: String,
    /// Device channels the stream has open.
    pub channels: u16,
    /// Device sample format (`"f32"`, `"i32"`, `"i24"` or `"i16"`).
    pub sample_format: String,
    /// Largest number of frames the device has delivered in one callback.
    pub buffer_frames: u32,
    /// Whether the media clock was available at the last callback.
    pub clock_ok: bool,
    /// Whether audio is flowing (the stream is past settling).
    pub running: bool,
    /// Whether the drift-tracking loop has converged.
    pub locked: bool,
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
    /// Times the stream lost track (stall, clock step) and realigned.
    pub realigns: u64,
    /// Errors reported by the audio backend (xruns, disconnects, ...).
    pub device_errors: u64,
    /// The backend reported the device as gone; the stream is dead.
    pub device_lost: bool,
}

/// The atomics behind a [`DirectionStats`].
#[derive(Debug, Default)]
pub struct StatsCell {
    buffer_frames: AtomicU32,
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
}

impl StatsCell {
    pub fn new() -> Self {
        let cell = StatsCell::default();
        cell.ratio_bits.store(1f64.to_bits(), Relaxed);
        cell
    }

    #[inline]
    pub fn callback(&self) {
        self.callbacks.fetch_add(1, Relaxed);
    }

    #[inline]
    pub fn underrun(&self) {
        self.underruns.fetch_add(1, Relaxed);
    }

    #[inline]
    pub fn overrun(&self) {
        self.overruns.fetch_add(1, Relaxed);
    }

    #[inline]
    pub fn set_buffer_frames(&self, frames: usize) {
        self.buffer_frames.store(frames.min(u32::MAX as usize) as u32, Relaxed);
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

    /// Records an error reported by the audio backend. Called from the
    /// backend's error callback.
    pub fn device_error(&self, error: &cpal::Error) {
        self.device_errors.fetch_add(1, Relaxed);
        if matches!(
            error.kind(),
            cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::StreamInvalidated
        ) {
            self.device_lost.store(true, Relaxed);
        }
    }

    pub fn snapshot(&self, device: &str, channels: u16, sample_format: &str) -> DirectionStats {
        DirectionStats {
            device: device.to_owned(),
            channels,
            sample_format: sample_format.to_owned(),
            buffer_frames: self.buffer_frames.load(Relaxed),
            clock_ok: self.clock_ok.load(Relaxed),
            running: self.running.load(Relaxed),
            locked: self.locked.load(Relaxed),
            ratio_ppm: (f64::from_bits(self.ratio_bits.load(Relaxed)) - 1.0) * 1e6,
            error_samples: f64::from_bits(self.error_bits.load(Relaxed)),
            callbacks: self.callbacks.load(Relaxed),
            underruns: self.underruns.load(Relaxed),
            overruns: self.overruns.load(Relaxed),
            realigns: self.realigns.load(Relaxed),
            device_errors: self.device_errors.load(Relaxed),
            device_lost: self.device_lost.load(Relaxed),
        }
    }
}
