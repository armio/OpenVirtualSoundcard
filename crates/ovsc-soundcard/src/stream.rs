//! The real-time halves of the bridge, independent of any audio API.
//!
//! [`Playout`] turns network receive rings into device output buffers;
//! [`Capture`] turns device input buffers into network transmit rings. Both
//! are driven by one call per device callback that takes the media time at
//! the start of the callback, so the tests below run them against simulated
//! clocks and rings with no audio device involved.
//!
//! Neither allocates, locks or logs once constructed: buffers are sized up
//! front and processing happens in chunks of at most [`CHUNK`] frames, so
//! any callback size works.

use std::sync::Arc;

use ovsc_core::buffer::TimedRing;

use crate::controller::{DriftController, Limits, MAX_PPM, Steer};
use crate::kernel::{HALF, Kernel, TAPS, dot};
use crate::pos::MediaPos;
use crate::sample::{DeviceSample, ToNetwork, from_network};
use crate::stats::StatsCell;

/// Frames processed per inner chunk.
const CHUNK: usize = 256;
/// Extra media samples a chunk can span at the largest ratio, 1 + 1000 ppm.
const STRETCH: usize = CHUNK.div_ceil(1000);
/// Media samples one playback chunk can span.
const SPAN: usize = CHUNK + STRETCH + TAPS + 4;
/// Device frames the capture history holds.
const HISTORY: usize = CHUNK + TAPS + 4;
/// Media samples one capture chunk can produce.
const PRODUCE: usize = CHUNK + STRETCH + 4;
/// Fade-in after a realignment, seconds.
const FADE_SECONDS: f64 = 0.005;
/// Largest distance (seconds) the drift loop tries to pull in rather than
/// realign.
const JUMP_SECONDS: f64 = 0.05;

/// Timing state shared by both directions.
struct Timing {
    controller: DriftController,
    /// Extra safety margin, media samples.
    margin: u64,
    /// Largest callback seen, frames.
    max_frames: usize,
    fade: u32,
    fade_len: u32,
    stats: Arc<StatsCell>,
}

impl Timing {
    fn new(rate: u32, margin: u64, capacity: usize, playback: bool, stats: Arc<StatsCell>) -> Self {
        let jump = (JUMP_SECONDS * rate as f64).min(capacity as f64 / 4.0);
        let margin_f = margin as f64;
        // Playback loses audio when it runs ahead of its target, capture
        // when it falls behind.
        let limits = if playback {
            Limits { jump, ahead: margin_f, behind: jump }
        } else {
            Limits { jump, ahead: jump, behind: margin_f }
        };
        Timing {
            controller: DriftController::new(rate as f64, limits),
            margin,
            max_frames: 0,
            fade: 0,
            fade_len: ((FADE_SECONDS * rate as f64) as u32).max(1),
            stats,
        }
    }

    /// Tracks the callback size. The safety distance includes one callback,
    /// so a callback larger than any before moves the target; if that eats
    /// a significant part of the margin, start over.
    fn note_frames(&mut self, frames: usize) {
        if frames > self.max_frames {
            let grew = (frames - self.max_frames) as u64;
            self.max_frames = frames;
            self.stats.set_buffer_frames(frames);
            if self.controller.running() && grew > self.margin / 2 {
                self.controller.reset();
            }
        }
    }

    /// Distance between the stream position and the network's edge, media
    /// samples: one callback, the interpolator's look-ahead and the margin.
    fn safety(&self) -> u64 {
        self.max_frames as u64 + HALF as u64 + self.margin
    }

    #[inline]
    fn gain(&mut self) -> f32 {
        if self.fade >= self.fade_len {
            return 1.0;
        }
        self.fade += 1;
        self.fade as f32 / self.fade_len as f32
    }
}

fn min_capacity(rings: &[Option<Arc<TimedRing>>]) -> usize {
    rings
        .iter()
        .flatten()
        .map(|r| r.capacity())
        .min()
        .unwrap_or(ovsc_core::buffer::DEFAULT_CAPACITY)
}

/// Network receive channels → device output.
///
/// The read position trails the media clock by `latency + safety`, where
/// `safety` is one device callback, the interpolator's look-ahead and the
/// margin: every sample the callback touches has arrived (the device then
/// adds its own output latency on top).
pub struct Playout {
    kernel: &'static Kernel,
    rate: u32,
    latency: u64,
    capacity: u64,
    /// Ring feeding each device channel.
    sources: Vec<Option<Arc<TimedRing>>>,
    timing: Timing,
    /// Media position of the next device frame.
    pos: MediaPos,
    raw: Vec<i32>,
    /// Ring contents for the current chunk, `SPAN` samples per channel.
    planar: Vec<f32>,
    coeffs: [f32; TAPS],
}

impl Playout {
    /// `sources[i]` is the ring played on device channel `i` (`None`:
    /// silence); `latency` the network receive latency and `margin` the
    /// extra safety margin, both in media samples.
    pub fn new(
        rate: u32,
        latency: u64,
        sources: Vec<Option<Arc<TimedRing>>>,
        margin: u64,
        stats: Arc<StatsCell>,
    ) -> Self {
        let capacity = min_capacity(&sources);
        let channels = sources.len();
        Playout {
            kernel: Kernel::get(),
            rate,
            latency,
            capacity: capacity as u64,
            sources,
            timing: Timing::new(rate, margin, capacity, true, stats),
            pos: MediaPos::default(),
            raw: vec![0; SPAN],
            planar: vec![0.0; channels * SPAN],
            coeffs: [0.0; TAPS],
        }
    }

    /// Fills one interleaved device buffer. `now_ns` is the media time at
    /// the start of the callback (`None` if the clock isn't available).
    pub fn render<S: DeviceSample>(&mut self, now_ns: Option<u64>, out: &mut [S]) {
        let channels = self.sources.len();
        let frames = out.len() / channels.max(1);
        if frames == 0 {
            return;
        }
        self.timing.stats.callback();
        self.timing.note_frames(frames);
        let Some(now) = now_ns.map(|ns| MediaPos::from_ns(ns, self.rate)) else {
            out.fill(S::from_f32(0.0));
            self.timing.controller.reset();
            self.timing.stats.publish(&self.timing.controller, false);
            return;
        };

        let target = now.offset(-((self.latency + self.timing.safety()) as f64));
        let steer = self.timing.controller.update(target.minus(self.pos), frames);
        self.pos = self.pos.offset(steer.shift);
        if steer.mute {
            out.fill(S::from_f32(0.0));
            self.timing.fade = 0;
            self.pos.advance(steer.ratio * frames as f64);
        } else {
            self.check_edges(now, &steer, frames);
            for chunk in out.chunks_mut(CHUNK * channels) {
                self.render_chunk(chunk, steer.ratio);
            }
        }
        self.timing.stats.publish(&self.timing.controller, true);
    }

    fn check_edges(&self, now: MediaPos, steer: &Steer, frames: usize) {
        // The newest sample this callback reads must have arrived...
        let span = (self.pos.frac + steer.ratio * (frames - 1) as f64) as u64;
        let newest = self.pos.whole + span + HALF as u64;
        if newest > now.whole.saturating_sub(self.latency) {
            self.timing.stats.underrun();
        }
        // ...and the oldest must not have been overwritten yet.
        let oldest = self.pos.whole.saturating_sub(HALF as u64 - 1);
        if oldest + self.capacity < now.whole + self.timing.margin {
            self.timing.stats.overrun();
        }
    }

    fn render_chunk<S: DeviceSample>(&mut self, out: &mut [S], ratio: f64) {
        let channels = self.sources.len();
        let frames = out.len() / channels;
        // Ring timestamp of planar index 0.
        let start = self.pos.whole.saturating_sub(HALF as u64 - 1);
        let reach = (self.pos.frac + ratio * frames as f64) as usize;
        let span = (reach + TAPS + 1).min(SPAN);
        for (c, source) in self.sources.iter().enumerate() {
            if let Some(ring) = source {
                ring.read(start, &mut self.raw[..span]);
                let dst = &mut self.planar[c * SPAN..c * SPAN + span];
                for (d, &s) in dst.iter_mut().zip(&self.raw[..span]) {
                    *d = from_network(s);
                }
            }
        }
        for frame in out.chunks_exact_mut(channels) {
            self.kernel.coefficients(self.pos.frac, &mut self.coeffs);
            let first = self.pos.whole.saturating_sub(HALF as u64 - 1);
            let off = ((first - start) as usize).min(span - TAPS);
            let gain = self.timing.gain();
            for (c, (o, source)) in frame.iter_mut().zip(&self.sources).enumerate() {
                let v = match source {
                    Some(_) => dot(&self.coeffs, &self.planar[c * SPAN + off..]) * gain,
                    None => 0.0,
                };
                *o = S::from_f32(v);
            }
            self.pos.advance(ratio);
        }
    }
}

/// Device input → network transmit channels.
///
/// Captured frames go into a short history; the stream converts them to
/// media-rate samples at the fractional history position `q` and writes
/// them at consecutive media timestamps from `w` on. The media time of the
/// newest captured frame is kept `safety` ahead of the media clock, so that
/// the transmitter, which sends sample `t` at about media time `t`, always
/// finds it in the ring, including just before the next callback.
pub struct Capture {
    kernel: &'static Kernel,
    rate: u32,
    capacity: u64,
    /// Ring fed by each device channel.
    sinks: Vec<Option<Arc<TimedRing>>>,
    to_network: ToNetwork,
    timing: Timing,
    ratio: f64,
    /// Captured audio, `HISTORY` frames per channel.
    history: Vec<f32>,
    /// Valid frames in the history.
    len: usize,
    /// History position of the next media sample.
    q: f64,
    /// Media timestamp of the next media sample.
    w: u64,
    /// Converted samples, `PRODUCE` per channel.
    produced: Vec<i32>,
    coeffs: [f32; TAPS],
}

impl Capture {
    /// `sinks[i]` is the ring device channel `i` feeds (`None`: ignored),
    /// `bits` the network sample width and `margin` the extra safety margin
    /// in media samples.
    pub fn new(
        rate: u32,
        bits: u32,
        sinks: Vec<Option<Arc<TimedRing>>>,
        margin: u64,
        stats: Arc<StatsCell>,
    ) -> Self {
        let capacity = min_capacity(&sinks);
        let channels = sinks.len();
        Capture {
            kernel: Kernel::get(),
            rate,
            capacity: capacity as u64,
            sinks,
            to_network: ToNetwork::new(bits),
            timing: Timing::new(rate, margin, capacity, false, stats),
            ratio: 1.0,
            history: vec![0.0; channels * HISTORY],
            // Start with a window of silence so interpolation can begin
            // with the first captured frame.
            len: TAPS,
            q: (HALF - 1) as f64,
            w: 0,
            produced: vec![0; channels * PRODUCE],
            coeffs: [0.0; TAPS],
        }
    }

    /// Consumes one interleaved device buffer. `now_ns` is the media time at
    /// the start of the callback (`None` if the clock isn't available).
    pub fn process<S: DeviceSample>(&mut self, now_ns: Option<u64>, input: &[S]) {
        let channels = self.sinks.len();
        let frames = input.len() / channels.max(1);
        if frames == 0 {
            return;
        }
        self.timing.stats.callback();
        self.timing.note_frames(frames);
        let now = now_ns.map(|ns| MediaPos::from_ns(ns, self.rate));
        let steer = match now {
            Some(now) => {
                // Media time of the end of the captured data, once this
                // callback's frames are in.
                let pending = (self.len + frames) as f64 - self.q;
                let end = MediaPos::new(self.w, 0.0).offset(pending * self.ratio);
                let target = now.offset(self.timing.safety() as f64);
                self.timing.controller.update(target.minus(end), frames)
            }
            None => {
                self.timing.controller.reset();
                // Keep the history flowing, write nothing.
                Steer { shift: 0.0, ratio: self.ratio, mute: true }
            }
        };
        self.shift(steer.shift, steer.ratio);
        self.ratio = steer.ratio;
        let write = !steer.mute;
        if steer.mute {
            self.timing.fade = 0;
        }
        let first = self.w;
        for chunk in input.chunks(CHUNK * channels) {
            self.append(chunk);
            self.produce(write);
        }
        let stats = &self.timing.stats;
        if let (Some(now), true) = (now, write) {
            // Samples at or before `now` may already have been sent.
            if first <= now.whole {
                stats.underrun();
            }
            if self.w > now.whole + self.capacity.saturating_sub(self.timing.margin) {
                stats.overrun();
            }
        }
        stats.publish(&self.timing.controller, now.is_some());
    }

    /// Moves the stream's media position by `shift` samples: the next
    /// sample lands on a whole timestamp, the fraction is skipped in the
    /// history.
    fn shift(&mut self, shift: f64, ratio: f64) {
        if shift == 0.0 {
            return;
        }
        let moved = MediaPos::new(self.w, 0.0).offset(shift);
        let (w, skip) =
            if moved.frac > 0.0 { (moved.whole + 1, 1.0 - moved.frac) } else { (moved.whole, 0.0) };
        self.w = w;
        self.q += skip / ratio;
    }

    fn append<S: DeviceSample>(&mut self, chunk: &[S]) {
        let channels = self.sinks.len();
        let frames = chunk.len() / channels;
        if self.len + frames > HISTORY {
            // Cannot happen with the sizes above; stay safe regardless.
            self.discard(self.len + frames - HISTORY);
        }
        for c in 0..channels {
            let dst = &mut self.history[c * HISTORY + self.len..c * HISTORY + self.len + frames];
            for (d, frame) in dst.iter_mut().zip(chunk.chunks_exact(channels)) {
                *d = frame[c].to_f32();
            }
        }
        self.len += frames;
    }

    /// Converts all history that has enough look-ahead into media samples,
    /// writes them out if `write`, and drops history no longer needed.
    fn produce(&mut self, write: bool) {
        let step = 1.0 / self.ratio;
        let mut count = 0;
        while (self.q as usize) + HALF < self.len && count < PRODUCE {
            if write {
                let i = self.q as usize;
                self.kernel.coefficients(self.q - i as f64, &mut self.coeffs);
                let base = (i + 1).saturating_sub(HALF);
                let gain = self.timing.gain();
                for (c, sink) in self.sinks.iter().enumerate() {
                    if sink.is_some() {
                        let v = dot(&self.coeffs, &self.history[c * HISTORY + base..]) * gain;
                        self.produced[c * PRODUCE + count] = self.to_network.convert(v);
                    }
                }
            }
            self.q += step;
            count += 1;
        }
        if write {
            for (c, sink) in self.sinks.iter().enumerate() {
                if let Some(ring) = sink {
                    ring.write(self.w, &self.produced[c * PRODUCE..c * PRODUCE + count]);
                }
            }
        }
        self.w += count as u64;
        self.discard(((self.q as usize) + 1).saturating_sub(HALF));
    }

    /// Drops the oldest `n` frames of history.
    fn discard(&mut self, n: usize) {
        let n = n.min(self.len);
        if n == 0 {
            return;
        }
        for c in 0..self.sinks.len() {
            let ch = &mut self.history[c * HISTORY..(c + 1) * HISTORY];
            ch.copy_within(n..self.len, 0);
        }
        self.len -= n;
        self.q = (self.q - n as f64).max((HALF - 1) as f64);
    }
}

// `STRETCH` covers ratios up to 1 + 1000 ppm.
const _: () = assert!(MAX_PPM <= 1000.0);

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    const RATE: u32 = 48_000;
    const NS: f64 = 1e9;
    /// Media time at the start of the simulations (somewhere in 2023).
    const EPOCH_NS: f64 = 1.7e18;

    struct Rng(u64);
    impl Rng {
        fn next_f64(&mut self) -> f64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^= z >> 31;
            (z >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// A device whose clock runs `drift_ppm` fast, calling back every
    /// `period` frames, measured up to `jitter` seconds late. Yields
    /// (callback index, media time in ns).
    fn callbacks(
        drift_ppm: f64,
        period: usize,
        jitter: f64,
        seconds: f64,
    ) -> impl Iterator<Item = (usize, u64)> {
        let device_rate = RATE as f64 * (1.0 + drift_ppm * 1e-6);
        let count = (seconds * device_rate / period as f64) as usize;
        let mut rng = Rng(period as u64 ^ drift_ppm.to_bits());
        (0..count).map(move |i| {
            let t = (i * period) as f64 / device_rate + rng.next_f64() * jitter;
            (i, (EPOCH_NS + t * NS) as u64)
        })
    }

    fn sample_at(ns: f64) -> f64 {
        ns * RATE as f64 / NS
    }

    /// Largest deviation from the sine recurrence
    /// `y[k+1] + y[k-1] = 2 cos(ω) y[k]`, which every sampled sinusoid of
    /// angular frequency ω (per sample) obeys whatever its phase and
    /// amplitude. A gap, a repeated or dropped sample, or a click shows up
    /// as a deviation comparable to the amplitude.
    fn max_recurrence_error(y: &[f64], omega: f64) -> f64 {
        let c = 2.0 * omega.cos();
        y.windows(3).map(|w| (w[2] + w[0] - c * w[1]).abs()).fold(0.0, f64::max)
    }

    /// Frequency (cycles per sample) from a least-squares fit of the
    /// interpolated upward zero crossings.
    fn measure_frequency(y: &[f64]) -> f64 {
        let crossings: Vec<f64> = y
            .windows(2)
            .enumerate()
            .filter(|(_, w)| w[0] < 0.0 && w[1] >= 0.0)
            .map(|(i, w)| i as f64 + w[0] / (w[0] - w[1]))
            .collect();
        let n = crossings.len() as f64;
        let mean_k = (n - 1.0) / 2.0;
        let mean_t = crossings.iter().sum::<f64>() / n;
        let (mut num, mut den) = (0.0, 0.0);
        for (k, t) in crossings.iter().enumerate() {
            num += (k as f64 - mean_k) * (t - mean_t);
            den += (k as f64 - mean_k).powi(2);
        }
        // Samples per cycle -> cycles per sample.
        den / num
    }

    fn rms(y: &[f64]) -> f64 {
        (y.iter().map(|v| v * v).sum::<f64>() / y.len() as f64).sqrt()
    }

    /// Plays a 997 Hz sine received at media rate on a device whose clock
    /// is off by `drift_ppm`, and checks what the device gets.
    fn check_playout(drift_ppm: f64, period: usize, jitter: f64) {
        let tone = 997.0;
        let amplitude = 0.5;
        let latency = 192; // 4 ms
        let fpp = 48;
        let ring = Arc::new(TimedRing::new(1 << 15));
        let stats = Arc::new(StatsCell::new());
        // Device channel 0 plays the tone, channel 1 is unmapped.
        let mut playout =
            Playout::new(RATE, latency, vec![Some(ring.clone()), None], 48, stats.clone());

        let seconds = 50.0;
        let mut written = (sample_at(EPOCH_NS) as u64 / fpp) * fpp - 4 * fpp;
        // Phase reference (absolute media times are too large for an f64
        // phase to stay accurate).
        let origin = written;
        let mut out = vec![0f32; period * 2];
        let mut played = Vec::new();
        let mut packet = vec![0i32; fpp as usize];
        let mut running_from = None;
        for (i, now_ns) in callbacks(drift_ppm, period, jitter, seconds) {
            // The network delivers every packet whose last sample is at
            // least 0.3 ms old.
            let now = sample_at(now_ns as f64);
            while (written + fpp) as f64 + 15.0 <= now {
                for (k, s) in packet.iter_mut().enumerate() {
                    let t = (written + k as u64 - origin) as f64 / RATE as f64;
                    let v = amplitude * (2.0 * PI * tone * t).sin();
                    *s = (v * 2_147_483_648.0) as i32 & !0xff;
                }
                ring.write(written, &packet);
                written += fpp;
            }
            playout.render(Some(now_ns), &mut out);
            assert!(out.iter().skip(1).step_by(2).all(|&v| v == 0.0), "unmapped channel");
            if running_from.is_none() && playout.timing.controller.running() {
                running_from = Some((i, stats.snapshot("", 2, "").underruns));
            }
            played.extend(out.iter().step_by(2).map(|&v| v as f64));
        }

        let s = stats.snapshot("test", 2, "f32");
        let (start, underruns_at_start) = running_from.expect("never started");
        assert!(start * period < RATE as usize / 4, "muted for {} frames", start * period);
        assert!(s.locked, "{s:?}");
        assert_eq!(s.realigns, 0, "{s:?}");
        assert_eq!(s.underruns, underruns_at_start, "{s:?}");
        assert_eq!(s.overruns, 0);
        assert_eq!(s.buffer_frames as usize, period);
        // Media samples per device frame.
        assert!(((1.0 + s.ratio_ppm * 1e-6) * (1.0 + drift_ppm * 1e-6) - 1.0).abs() < 5e-6);

        // Analyse the last 10 s.
        let device_rate = RATE as f64 * (1.0 + drift_ppm * 1e-6);
        let tail = &played[played.len() - 10 * RATE as usize..];
        let omega = 2.0 * PI * tone / device_rate;
        let worst = max_recurrence_error(tail, omega);
        assert!(worst < 1e-4, "discontinuity: recurrence error {worst:e}");
        // The tone keeps its pitch: the device clock error is compensated
        // (to within the loop's residual wander, see the controller tests).
        let freq = measure_frequency(tail) * device_rate;
        assert!((freq / tone - 1.0).abs() < 5e-6, "played {freq} Hz instead of {tone} Hz");
        let level = rms(tail) * 2f64.sqrt() / amplitude;
        assert!((level - 1.0).abs() < 1e-4, "level off by {:.6}", level - 1.0);
    }

    #[test]
    fn playout_resamples_for_fast_device() {
        check_playout(80.0, 256, 0.5e-3);
    }

    #[test]
    fn playout_resamples_for_slow_device() {
        check_playout(-50.0, 480, 1e-3);
    }

    /// Captures a 997 Hz sine on a device whose clock is off by
    /// `drift_ppm` and checks what a transmitter reading the ring at media
    /// rate sends.
    fn check_capture(drift_ppm: f64, period: usize, jitter: f64) {
        let tone = 997.0;
        let amplitude = 0.5;
        // The transmitter sends packets of `fpp` samples stamped `t` once
        // media time reaches `t + guard`.
        let fpp = 48;
        let guard = 24;
        let ring = Arc::new(TimedRing::new(1 << 15));
        let stats = Arc::new(StatsCell::new());
        // Device channel 1 feeds the ring, channel 0 is ignored.
        let mut capture = Capture::new(RATE, 24, vec![None, Some(ring.clone())], 48, stats.clone());

        let device_rate = RATE as f64 * (1.0 + drift_ppm * 1e-6);
        let omega_device = 2.0 * PI * tone / device_rate;
        let mut input = vec![0f32; period * 2];
        let mut next_packet = None;
        let mut sent = Vec::new();
        // Everything from this timestamp on must reach the transmitter.
        let mut audible_from = None;
        let mut missing = 0;
        for (i, now_ns) in callbacks(drift_ppm, period, jitter, 50.0) {
            let now = sample_at(now_ns as f64) as u64;
            // The transmitter catches up to `now` before this callback.
            let next = next_packet.get_or_insert((now / fpp) * fpp);
            while *next + guard <= now {
                for t in *next..*next + fpp {
                    match ring.read_one(t) {
                        Some(s) => sent.push(s as f64 / 2_147_483_648.0),
                        None => {
                            sent.push(0.0);
                            missing += audible_from.is_some_and(|from| t >= from) as usize;
                        }
                    }
                }
                *next += fpp;
            }
            for (k, frame) in input.chunks_exact_mut(2).enumerate() {
                let phase = omega_device * (i * period + k) as f64;
                frame[0] = 0.9;
                frame[1] = (amplitude * phase.sin()) as f32;
            }
            capture.process(Some(now_ns), &input);
            if audible_from.is_none() && capture.timing.controller.running() {
                audible_from = Some(capture.w);
            }
        }

        let s = stats.snapshot("test", 2, "f32");
        assert!(s.locked, "{s:?}");
        assert_eq!(s.realigns, 0, "{s:?}");
        assert_eq!(s.underruns, 0, "{s:?}");
        assert_eq!(missing, 0, "transmitter found holes");
        // Media samples per device frame.
        assert!(((1.0 + s.ratio_ppm * 1e-6) * (1.0 + drift_ppm * 1e-6) - 1.0).abs() < 5e-6);

        let tail = &sent[sent.len() - 10 * RATE as usize..];
        let omega = 2.0 * PI * tone / RATE as f64;
        let worst = max_recurrence_error(tail, omega);
        assert!(worst < 1e-4, "discontinuity: recurrence error {worst:e}");
        let freq = measure_frequency(tail) * RATE as f64;
        assert!((freq / tone - 1.0).abs() < 5e-6, "sent {freq} Hz instead of {tone} Hz");
        let level = rms(tail) * 2f64.sqrt() / amplitude;
        assert!((level - 1.0).abs() < 1e-4, "level off by {:.6}", level - 1.0);
    }

    #[test]
    fn capture_resamples_for_fast_device() {
        check_capture(80.0, 256, 0.5e-3);
    }

    #[test]
    fn capture_resamples_for_slow_device() {
        check_capture(-50.0, 480, 1e-3);
    }

    #[test]
    fn playout_mutes_without_clock_and_fades_in() {
        let ring = Arc::new(TimedRing::new(1 << 12));
        let stats = Arc::new(StatsCell::new());
        let mut playout = Playout::new(RATE, 96, vec![Some(ring.clone())], 48, stats.clone());
        let mut out = vec![1i16; 64];
        playout.render(None, &mut out);
        assert!(out.iter().all(|&v| v == 0));
        assert!(!stats.snapshot("", 1, "").clock_ok);

        // A constant signal: after settling, the output ramps up to it.
        let start = sample_at(EPOCH_NS) as u64;
        ring.write(start - 2000, &vec![0x4000_0000; 12_000]);
        let mut levels = Vec::new();
        for i in 0..120u64 {
            let ns = EPOCH_NS + (i * 64) as f64 / RATE as f64 * NS;
            playout.render(Some(ns as u64), &mut out);
            levels.extend(out.iter().copied());
        }
        assert!(stats.snapshot("", 1, "").clock_ok);
        let first = levels.iter().position(|&v| v != 0).expect("never unmuted");
        assert!(first >= 8 * 64, "audible during settling");
        assert!(levels[first] < 1000, "no fade-in: {}", levels[first]);
        assert_eq!(*levels.last().unwrap(), 0x4000);
    }

    #[test]
    fn odd_callback_sizes_are_handled() {
        // Callbacks larger than the internal chunk and of varying size.
        let ring = Arc::new(TimedRing::new(1 << 15));
        let stats = Arc::new(StatsCell::new());
        let mut playout = Playout::new(RATE, 96, vec![Some(ring.clone())], 48, stats.clone());
        let sink = Arc::new(TimedRing::new(1 << 15));
        let mut capture = Capture::new(RATE, 24, vec![Some(sink.clone())], 48, stats.clone());
        let mut t = EPOCH_NS;
        for &n in [1usize, 7, 1000, 4096, 333, 2].iter().cycle().take(60) {
            let mut out = vec![0f32; n];
            playout.render(Some(t as u64), &mut out);
            capture.process(Some(t as u64), &vec![0.25f32; n]);
            t += n as f64 / RATE as f64 * NS;
        }
        assert_eq!(stats.snapshot("", 1, "").buffer_frames, 4096);
    }
}
