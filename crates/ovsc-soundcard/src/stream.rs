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
//!
//! Whenever a stream jumps or mutes (see the [controller](crate::controller)),
//! the audio before the break ramps down over up to 2 ms and the audio after
//! it ramps up over 5 ms, so a discontinuity is a short dip, never a click.

use std::sync::Arc;

use ovsc_core::buffer::TimedRing;

use crate::controller::{DriftController, Limits, MAX_PPM, Side, Steer};
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
/// Fade-in when a stream starts or after a discontinuity, seconds.
const FADE_IN_SECONDS: f64 = 0.005;
/// Fade-out before a discontinuity, seconds. Playback can only ramp down
/// within the callback that breaks off, so short callbacks get less.
const FADE_OUT_SECONDS: f64 = 0.002;
/// Largest distance (seconds) the drift loop tries to pull in rather than
/// realign.
const JUMP_SECONDS: f64 = 0.05;
/// How long the safety distance remembers a callback size, seconds of
/// device frames: it allows for the largest callback of the last 10 to
/// 20 s, so that one oversized callback (a WASAPI hiccup) doesn't add its
/// size to the latency for the rest of the show.
const SIZE_MEMORY_SECONDS: f64 = 10.0;
/// The most frames per packet the transmitter sends (ovsc-core's
/// `FPP_MAX`, the maximum advertised for transmit flows).
const TX_FPP_MAX: u64 = 32;
/// Device input beyond this magnitude (+6 dBFS) is clipped. Hosts can hand
/// over floats above full scale; the network clips at full scale anyway.
const INPUT_LIMIT: f32 = 2.0;
/// Device input below this magnitude (≈ −400 dBFS) is flushed to zero, so
/// that subnormal floats (a decaying tail nobody flushed) can't slow the
/// interpolator down: x86 computes with them many times slower.
const INPUT_FLUSH: f32 = 1e-20;

/// Media samples the capture stream must stay ahead of the transmitter's
/// deadline besides its margin: a packet leaves `guard_us` after its first
/// sample's timestamp and carries up to [`TX_FPP_MAX`] samples, so its last
/// sample must be in the ring `TX_FPP_MAX - 1 - guard` samples before its
/// own time (as for the macOS driver's output safety offset,
/// docs/MACOS.md, "Latency and safety offsets").
pub fn transmit_lead(rate: u32, guard_us: u32) -> u64 {
    let guard = guard_us as u64 * rate as u64 / 1_000_000;
    (TX_FPP_MAX - 1).saturating_sub(guard)
}

/// Timing state shared by both directions.
struct Timing {
    controller: DriftController,
    rate: u32,
    playback: bool,
    jump: f64,
    /// Extra safety margin, media samples.
    margin: u64,
    /// Lead on top of the margin, media samples (capture: see
    /// [`transmit_lead`]).
    lead: u64,
    /// The callback size the safety distance allows for: the largest of the
    /// current and the previous memory window.
    max_frames: usize,
    /// Largest callback of the current and of the previous memory window.
    recent: usize,
    older: usize,
    /// Device frames counted in the current memory window, and its length.
    window: u64,
    window_len: u64,
    /// Output gain, ramping towards `gain_target` by `gain_step` per frame.
    gain: f32,
    gain_target: f32,
    gain_step: f32,
    /// Frames to keep silent before the gain resumes its ramp.
    hold: usize,
    fade_in_frames: u32,
    fade_out_frames: usize,
    stats: Arc<StatsCell>,
}

impl Timing {
    fn new(
        rate: u32,
        margin: u64,
        lead: u64,
        capacity: usize,
        playback: bool,
        stats: Arc<StatsCell>,
    ) -> Self {
        let jump = (JUMP_SECONDS * rate as f64).min(capacity as f64 / 4.0);
        let mut timing = Timing {
            // Replaced below, once the limits can be computed.
            controller: DriftController::new(
                rate as f64,
                Limits { jump, ahead: jump, behind: jump, lossy: Side::Ahead },
            ),
            rate,
            playback,
            jump,
            margin,
            lead,
            max_frames: 0,
            recent: 0,
            older: 0,
            window: 0,
            window_len: (SIZE_MEMORY_SECONDS * rate as f64) as u64,
            gain: 0.0,
            gain_target: 0.0,
            gain_step: 0.0,
            hold: 0,
            fade_in_frames: ((FADE_IN_SECONDS * rate as f64) as u32).max(1),
            fade_out_frames: ((FADE_OUT_SECONDS * rate as f64) as usize).max(1),
            stats,
        };
        timing.controller = DriftController::new(rate as f64, timing.limits());
        timing.stats.set_buffer(0, timing.safety(), rate);
        timing
    }

    /// When the stream stops slewing: playback loses audio ahead of its
    /// target, capture behind it. On the other side, more than a callback
    /// plus the margin off target is a backlog (a stall, a clock step)
    /// worth dropping rather than slewing back at 1 ms per second.
    fn limits(&self) -> Limits {
        let margin = self.margin as f64;
        let slack = (self.max_frames as f64 + margin).min(self.jump);
        if self.playback {
            Limits { jump: self.jump, ahead: margin, behind: slack, lossy: Side::Ahead }
        } else {
            Limits { jump: self.jump, ahead: slack, behind: margin, lossy: Side::Behind }
        }
    }

    /// Starts a callback of `frames` frames: counts it and tracks the
    /// callback size the safety distance allows for.
    fn begin(&mut self, frames: usize) {
        self.stats.callback();
        self.hold = 0;
        self.recent = self.recent.max(frames);
        self.window += frames as u64;
        if self.window >= self.window_len {
            self.older = self.recent;
            self.recent = 0;
            self.window = 0;
        }
        let size = self.older.max(self.recent);
        if size != self.max_frames {
            let grew = size > self.max_frames;
            self.max_frames = size;
            self.controller.set_limits(self.limits());
            self.stats.set_buffer(size, self.safety(), self.rate);
            // A larger callback moves the target away from the network's
            // edge at once: if that puts the stream beyond its margin, jump
            // now rather than lose audio while a second callback confirms
            // it. A smaller one moves the target back, which the controller
            // slews or (beyond a callback plus the margin) jumps.
            if grew && self.controller.running() {
                self.controller.expect_step();
            }
        }
    }

    /// Distance between the stream position and the network's edge, media
    /// samples: one callback, the interpolator's look-ahead, the margin and
    /// the lead.
    fn safety(&self) -> u64 {
        self.max_frames as u64 + HALF as u64 + self.margin + self.lead
    }

    /// The gain for the next frame.
    #[inline]
    fn gain(&mut self) -> f32 {
        if self.hold > 0 {
            self.hold -= 1;
            return 0.0;
        }
        if self.gain < self.gain_target {
            self.gain = (self.gain + self.gain_step).min(self.gain_target);
        } else if self.gain > self.gain_target {
            self.gain = (self.gain - self.gain_step).max(self.gain_target);
        }
        self.gain
    }

    /// Whether the stream is audible, or about to fade in.
    fn audible(&self) -> bool {
        self.gain > 0.0 || self.gain_target > 0.0
    }

    /// Ramps the gain down to silence over the next `frames` frames.
    fn fade_out(&mut self, frames: usize) {
        self.gain_target = 0.0;
        self.gain_step = self.gain / frames.max(1) as f32;
    }

    /// Silent from now on.
    fn silence(&mut self) {
        self.gain = 0.0;
        self.gain_target = 0.0;
    }

    /// Ramps up to full level, unless already there or on the way.
    fn unmute(&mut self) {
        if self.gain_target < 1.0 {
            self.gain_target = 1.0;
            self.gain_step = 1.0 / self.fade_in_frames as f32;
        }
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

/// A device input sample as the interpolator may use it. NaN and ±Inf (a
/// plugin blowing up) become silence: through the 64-tap interpolator a
/// single infinity would turn into 64 samples of full-scale noise.
#[inline]
fn sanitize(v: f32) -> f32 {
    if v.is_finite() && v.abs() >= INPUT_FLUSH { v.clamp(-INPUT_LIMIT, INPUT_LIMIT) } else { 0.0 }
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
            timing: Timing::new(rate, margin, 0, capacity, true, stats),
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
        self.timing.begin(frames);
        let now = now_ns.map(|ns| MediaPos::from_ns(ns, self.rate));
        let steer = match now {
            Some(now) => {
                let target = now.offset(-((self.latency + self.timing.safety()) as f64));
                self.timing.controller.update(target.minus(self.pos), frames)
            }
            None => {
                self.timing.controller.reset();
                Steer { shift: 0.0, ratio: self.timing.controller.ratio(), mute: true }
            }
        };
        let mut done = 0;
        if steer.is_discontinuous() {
            // Ramp what was playing down before moving, instead of cutting
            // it off: the old position's audio has arrived (or reads as
            // silence), so the first frames can still play it.
            if self.timing.audible() {
                done = self.timing.fade_out_frames.min(frames);
                self.timing.fade_out(done);
                self.render_frames(&mut out[..done * channels], steer.ratio);
            }
            self.pos = self.pos.offset(steer.shift);
            self.timing.silence();
        }
        let rest = frames - done;
        if steer.mute {
            out[done * channels..].fill(S::from_f32(0.0));
            self.pos.advance(steer.ratio * rest as f64);
        } else {
            self.timing.unmute();
            if let (Some(now), true) = (now, rest > 0) {
                self.check_edges(now, steer.ratio, rest);
            }
            self.render_frames(&mut out[done * channels..], steer.ratio);
        }
        self.timing.stats.publish(&self.timing.controller, now.is_some());
    }

    fn check_edges(&self, now: MediaPos, ratio: f64, frames: usize) {
        // The newest sample this callback reads must have arrived...
        let span = (self.pos.frac + ratio * (frames - 1) as f64) as u64;
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

    fn render_frames<S: DeviceSample>(&mut self, out: &mut [S], ratio: f64) {
        let channels = self.sources.len();
        for chunk in out.chunks_mut(CHUNK * channels) {
            self.render_chunk(chunk, ratio);
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
    /// `bits` the network sample width, `margin` the extra safety margin
    /// and `lead` the [`transmit_lead`], both in media samples.
    pub fn new(
        rate: u32,
        bits: u32,
        sinks: Vec<Option<Arc<TimedRing>>>,
        margin: u64,
        lead: u64,
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
            timing: Timing::new(rate, margin, lead, capacity, false, stats),
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
        self.timing.begin(frames);
        if self.timing.stats.take_xrun() {
            // The device reported losing input: the next error is a step,
            // not a late callback, so jump at once. (Playback doesn't do
            // this: after an output xrun the backend refills its buffer
            // back to back, and the first callbacks are no measure.)
            self.timing.controller.expect_step();
        }
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
        if steer.is_discontinuous() {
            let before = self.w;
            self.shift(steer.shift, steer.ratio);
            if let Some(now) = now {
                // The transmitter hasn't sent the newest samples yet: ramp
                // them down instead of ending in a cut. Moving back, the
                // samples from the new position on get overwritten anyway.
                let end = if steer.mute { before } else { before.min(self.w) };
                self.fade_out_written(end, now.whole + self.timing.lead);
            }
            self.timing.silence();
        }
        self.ratio = steer.ratio;
        let write = !steer.mute;
        if write {
            self.timing.unmute();
        }
        let first = self.w;
        if let (Some(now), true) = (now, write) {
            // The transmitter may already have sent everything up to
            // `now + lead` (see `transmit_lead`): what this callback writes
            // there is lost. Start the rest from silence, fading in, rather
            // than with a cut-off fragment.
            let sent = now.whole + self.timing.lead;
            if first <= sent {
                self.timing.stats.underrun();
                self.timing.silence();
                self.timing.hold = (sent + 1 - first) as usize;
                self.timing.unmute();
            }
        }
        for chunk in input.chunks(CHUNK * channels) {
            self.append(chunk);
            self.produce(write);
        }
        if let (Some(now), true) = (now, write)
            && self.w > now.whole + self.capacity.saturating_sub(self.timing.margin)
        {
            self.timing.stats.overrun();
        }
        self.timing.stats.publish(&self.timing.controller, now.is_some());
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

    /// Ramps the samples written just before timestamp `end` down to
    /// silence, as far as the transmitter can't have sent them yet (it may
    /// have sent everything up to `sent`).
    fn fade_out_written(&mut self, end: u64, sent: u64) {
        let n = (self.timing.fade_out_frames as u64).min(end.saturating_sub(sent + 1));
        for ring in self.sinks.iter().flatten() {
            for k in 0..n {
                // k = 0 is the last sample before `end`.
                let ts = end - 1 - k;
                if let Some(s) = ring.read_one(ts) {
                    let gain = k as f32 / n as f32;
                    ring.write_one(ts, self.to_network.convert(from_network(s) * gain));
                }
            }
        }
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
                *d = sanitize(frame[c].to_f32());
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
    /// The capture lead with the default transmit guard (500 µs).
    const LEAD: u64 = 7;

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
        let mut capture =
            Capture::new(RATE, 24, vec![None, Some(ring.clone())], 48, LEAD, stats.clone());

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
        let mut capture = Capture::new(RATE, 24, vec![Some(sink.clone())], 48, LEAD, stats.clone());
        let mut t = EPOCH_NS;
        for &n in [1usize, 7, 1000, 4096, 333, 2].iter().cycle().take(60) {
            let mut out = vec![0f32; n];
            playout.render(Some(t as u64), &mut out);
            capture.process(Some(t as u64), &vec![0.25f32; n]);
            t += n as f64 / RATE as f64 * NS;
        }
        assert_eq!(stats.snapshot("", 1, "").buffer_frames, 4096);
    }

    /// Steps of a smooth 997 Hz sine at amplitude 0.5 stay below 0.066; a
    /// click (a cut, a jump without a fade) is a larger step.
    const CLICK: f64 = 0.1;

    fn clicks(y: &[f64]) -> usize {
        y.windows(2).filter(|w| (w[1] - w[0]).abs() > CLICK).count()
    }

    /// Milliseconds of `y` (in 1 ms windows) below half the level of a
    /// 0.5-amplitude sine.
    fn quiet_ms(y: &[f64]) -> usize {
        y.chunks_exact(RATE as usize / 1000).filter(|w| rms(w) < 0.5 * 0.5 / 2f64.sqrt()).count()
    }

    /// A capture scenario: a 997 Hz sine at 0.5 captured in callbacks of
    /// `period` frames, each delivered once its last frame has been
    /// captured (plus up to `jitter` seconds), and read by a transmitter
    /// sending `fpp`-sample packets `guard` samples after their timestamp.
    #[derive(Clone, Copy)]
    struct CaptureSpec {
        period: usize,
        seconds: f64,
        jitter: f64,
        margin: u64,
        fpp: u64,
        guard: u64,
        /// (time, frames): the device loses this many frames once.
        gap: Option<(f64, usize)>,
        /// Whether the backend reports that loss as an xrun.
        report: bool,
        /// (time, frames): one callback of this size instead of `period`.
        big: Option<(f64, usize)>,
        /// Plays an infinity, then a NaN, from this time on (once each).
        poison: Option<f64>,
    }

    impl CaptureSpec {
        fn new(period: usize, seconds: f64) -> Self {
            CaptureSpec {
                period,
                seconds,
                jitter: 0.3e-3,
                margin: 48,
                fpp: 16,
                guard: 24,
                gap: None,
                report: false,
                big: None,
                poison: None,
            }
        }
    }

    struct CaptureRun {
        /// What the transmitter sent from the time the stream started
        /// running, and the media time of `sent[0]`.
        sent: Vec<f64>,
        sent_from: u64,
        /// Samples the transmitter found missing once the stream ran.
        missing: usize,
        stats: crate::DirectionStats,
    }

    impl CaptureRun {
        /// The samples sent from media time `t` (seconds after the start).
        fn from(&self, t: f64) -> &[f64] {
            let start = (sample_at(EPOCH_NS) + t * RATE as f64) as u64;
            &self.sent[(start.saturating_sub(self.sent_from) as usize).min(self.sent.len())..]
        }
    }

    fn run_capture(spec: CaptureSpec) -> CaptureRun {
        let (tone, amplitude, drift_ppm) = (997.0, 0.5, 30.0);
        let ring = Arc::new(TimedRing::new(1 << 15));
        let stats = Arc::new(StatsCell::new());
        let mut capture =
            Capture::new(RATE, 24, vec![Some(ring.clone())], spec.margin, LEAD, stats.clone());
        let device_rate = RATE as f64 * (1.0 + drift_ppm * 1e-6);
        let omega = 2.0 * PI * tone / device_rate;
        let mut rng = Rng(spec.period as u64);
        let (mut delivered, mut lost) = (0usize, 0usize);
        let (mut gap, mut big, mut poison) = (spec.gap, spec.big, spec.poison);
        let mut next_packet: Option<u64> = None;
        let mut run =
            CaptureRun { sent: Vec::new(), sent_from: 0, missing: 0, stats: Default::default() };
        let mut audible_from = None;
        let mut input = Vec::new();
        loop {
            let start = (delivered + lost) as f64 / device_rate;
            if start > spec.seconds {
                break;
            }
            if let Some((at, frames)) = gap
                && start >= at
            {
                gap = None;
                lost += frames;
                if spec.report {
                    stats.device_error(&cpal::Error::new(cpal::ErrorKind::Xrun));
                }
            }
            let frames = match big {
                Some((at, frames)) if start >= at => {
                    big = None;
                    frames
                }
                _ => spec.period,
            };
            let t = (delivered + lost + frames) as f64 / device_rate + rng.next_f64() * spec.jitter;
            let now_ns = (EPOCH_NS + t * NS) as u64;
            let now = sample_at(now_ns as f64) as u64;
            // The transmitter catches up to `now` before this callback.
            let next = next_packet.get_or_insert((now / spec.fpp) * spec.fpp);
            while *next + spec.guard <= now {
                for ts in *next..*next + spec.fpp {
                    let Some(from) = audible_from else { continue };
                    if ts < from {
                        continue;
                    }
                    if run.sent.is_empty() {
                        run.sent_from = ts;
                    }
                    match ring.read_one(ts) {
                        Some(s) => run.sent.push(s as f64 / 2_147_483_648.0),
                        None => {
                            run.sent.push(0.0);
                            run.missing += 1;
                        }
                    }
                }
                *next += spec.fpp;
            }
            input.clear();
            input.extend(
                (0..frames)
                    .map(|k| (amplitude * (omega * (delivered + lost + k) as f64).sin()) as f32),
            );
            if let Some(at) = poison
                && start >= at
            {
                poison = None;
                input[frames / 3] = f32::INFINITY;
                input[2 * frames / 3] = f32::NAN;
            }
            capture.process(Some(now_ns), &input);
            if audible_from.is_none() && capture.timing.controller.running() {
                audible_from = Some(capture.w);
            }
            delivered += frames;
        }
        run.stats = stats.snapshot("test", 1, "f32");
        run
    }

    #[test]
    fn capture_xrun_costs_only_the_lost_audio() {
        // The device loses one 128-frame buffer and says so (ALSA, WASAPI):
        // the stream jumps at the next callback. The transmitter misses
        // the 128 lost samples (plus that callback's lateness, up to
        // 0.3 ms) and nothing else; there is no 50 ms of chopped audio and
        // no 100 ms mute.
        for period in [64, 128, 256] {
            let spec = CaptureSpec {
                gap: Some((10.0, 128)),
                report: true,
                ..CaptureSpec::new(period, 20.0)
            };
            let run = run_capture(spec);
            assert!(run.missing <= 128 + 15, "{period}: {} samples missing", run.missing);
            assert_eq!(run.stats.realigns, 1, "{period}: {:?}", run.stats);
            assert!(run.stats.locked, "{:?}", run.stats);
            // One cut where the lost audio begins; the audio after the hole
            // fades in.
            assert!(clicks(run.from(1.0)) <= 1, "{period}: {} clicks", clicks(run.from(1.0)));
            assert!(quiet_ms(run.from(9.0)) <= 8, "{period}: {} ms quiet", quiet_ms(run.from(9.0)));
        }
    }

    #[test]
    fn capture_gap_recovers_within_a_few_callbacks() {
        // The same gap, unreported (a CoreAudio overload, say): three
        // callbacks confirm it. Before, this cost ≈ 130 ms.
        for period in [64, 128, 256] {
            let spec = CaptureSpec { gap: Some((10.0, 128)), ..CaptureSpec::new(period, 20.0) };
            let run = run_capture(spec);
            let bound = 128 + 2 * (128 - 48);
            assert!(run.missing <= bound, "{period}: {} samples missing", run.missing);
            assert_eq!(run.stats.realigns, 1, "{period}: {:?}", run.stats);
            // A cut where each late callback's audio was already sent; what
            // arrives in time fades in rather than starting with a cut.
            assert!(clicks(run.from(1.0)) <= 3, "{period}: {} clicks", clicks(run.from(1.0)));
            assert!(
                quiet_ms(run.from(9.0)) <= 14,
                "{period}: {} ms quiet",
                quiet_ms(run.from(9.0))
            );
        }
    }

    #[test]
    fn capture_safety_covers_the_transmit_packet_span() {
        // 32-sample packets sent 24 samples after their timestamp need
        // each sample 7 samples early. With the margin trimmed to 5
        // samples, nothing may go missing unnoticed.
        let spec =
            CaptureSpec { margin: 5, fpp: 32, jitter: 0.05e-3, ..CaptureSpec::new(128, 20.0) };
        let run = run_capture(spec);
        assert!(run.missing == 0 || run.stats.underruns > 0, "{} missing unnoticed", run.missing);
        assert_eq!(run.missing, 0, "{:?}", run.stats);
        assert_eq!(transmit_lead(48_000, 500), LEAD);
        assert_eq!(transmit_lead(48_000, 0), 31);
        assert_eq!(transmit_lead(96_000, 500), 0);
    }

    #[test]
    fn capture_drops_non_finite_input() {
        // A plugin blows up: one infinity, one NaN. Each becomes a single
        // silent sample, not 64 samples of full-scale noise.
        let spec = CaptureSpec { poison: Some(10.0), ..CaptureSpec::new(128, 12.0) };
        let run = run_capture(spec);
        let worst = run.sent.iter().fold(0f64, |w, v| w.max(v.abs()));
        assert!(worst < 0.6, "peak {worst}");
        assert_eq!(run.missing, 0);
        assert_eq!(sanitize(f32::INFINITY), 0.0);
        assert_eq!(sanitize(f32::NEG_INFINITY), 0.0);
        assert_eq!(sanitize(f32::NAN), 0.0);
        assert_eq!(sanitize(1e-39), 0.0, "subnormal");
        assert_eq!(sanitize(-1e-25), 0.0);
        assert_eq!(sanitize(1e-10), 1e-10);
        assert_eq!(sanitize(-0.5), -0.5);
        assert_eq!(sanitize(5.0), INPUT_LIMIT);
    }

    #[test]
    fn capture_callback_size_hiccup_is_forgotten() {
        // One 1024-frame callback among 64-frame ones. The safety grows to
        // match at once (no settling mute), then shrinks back once the big
        // callback has been forgotten, instead of adding 20 ms for good.
        let spec = CaptureSpec { big: Some((5.0, 1024)), ..CaptureSpec::new(64, 21.0) };
        let run = run_capture(spec);
        assert_eq!(run.stats.buffer_frames, 64, "{:?}", run.stats);
        let base = (64 + HALF as u64 + 48 + LEAD) as f64 / 48.0;
        assert!((run.stats.latency_ms - base).abs() < 1e-9, "{:?}", run.stats);
        assert_eq!(run.stats.realigns, 2, "{:?}", run.stats);
        assert!(clicks(run.from(1.0)) <= 1, "{} clicks", clicks(run.from(1.0)));
    }

    /// A playback scenario: a 997 Hz sine at 0.5 received in 48-sample
    /// packets (each arriving 0.3 ms after its last sample), played in
    /// callbacks of `period` frames.
    #[derive(Clone, Copy)]
    struct PlayoutSpec {
        period: usize,
        seconds: f64,
        jitter: f64,
        /// (time, samples): the media clock steps once.
        clock_step: Option<(f64, f64)>,
        /// (time, frames): the device stalls once; every later callback
        /// comes that much later.
        stall: Option<(f64, usize)>,
        /// (time, frames): one callback of this size instead of `period`.
        big: Option<(f64, usize)>,
    }

    impl PlayoutSpec {
        fn new(period: usize, seconds: f64) -> Self {
            PlayoutSpec {
                period,
                seconds,
                jitter: 0.3e-3,
                clock_step: None,
                stall: None,
                big: None,
            }
        }
    }

    struct PlayoutRun {
        /// What the device played (from 1 s on), and the device time of
        /// `played[0]`.
        played: Vec<f64>,
        stats: crate::DirectionStats,
    }

    impl PlayoutRun {
        /// The device frames from `t` seconds on (approximately).
        fn from(&self, t: f64) -> &[f64] {
            let i = ((t - 1.0).max(0.0) * RATE as f64) as usize;
            &self.played[i.min(self.played.len())..]
        }
    }

    fn run_playout(spec: PlayoutSpec) -> PlayoutRun {
        let (tone, amplitude, drift_ppm, latency, fpp) = (997.0, 0.5, -40.0, 192, 48u64);
        let ring = Arc::new(TimedRing::new(1 << 15));
        let stats = Arc::new(StatsCell::new());
        let mut playout = Playout::new(RATE, latency, vec![Some(ring.clone())], 48, stats.clone());
        let device_rate = RATE as f64 * (1.0 + drift_ppm * 1e-6);
        let mut rng = Rng(spec.period as u64 ^ 0x5eed);
        let mut written = (sample_at(EPOCH_NS) as u64 / fpp) * fpp - 4 * fpp;
        let origin = written;
        let mut packet = vec![0i32; fpp as usize];
        let (mut stall, mut big) = (spec.stall, spec.big);
        let (mut played_frames, mut stalled) = (0usize, 0usize);
        let mut run = PlayoutRun { played: Vec::new(), stats: Default::default() };
        let mut out = Vec::new();
        loop {
            let start = (played_frames + stalled) as f64 / device_rate;
            if start > spec.seconds {
                break;
            }
            if let Some((at, frames)) = stall
                && start >= at
            {
                stall = None;
                stalled += frames;
            }
            let frames = match big {
                Some((at, frames)) if start >= at => {
                    big = None;
                    frames
                }
                _ => spec.period,
            };
            let t = (played_frames + stalled) as f64 / device_rate + rng.next_f64() * spec.jitter;
            let mut now_ns = EPOCH_NS + t * NS;
            if let Some((at, samples)) = spec.clock_step
                && t >= at
            {
                now_ns += samples / RATE as f64 * NS;
            }
            // The network delivers by real time, whatever the clock says.
            let real_now = sample_at(EPOCH_NS + t * NS);
            while (written + fpp) as f64 + 15.0 <= real_now {
                for (k, s) in packet.iter_mut().enumerate() {
                    let phase =
                        2.0 * PI * tone * (written + k as u64 - origin) as f64 / RATE as f64;
                    *s = (amplitude * phase.sin() * 2_147_483_648.0) as i32 & !0xff;
                }
                ring.write(written, &packet);
                written += fpp;
            }
            out.clear();
            out.resize(frames, 0f32);
            playout.render(Some(now_ns as u64), &mut out);
            if start >= 1.0 {
                run.played.extend(out.iter().map(|&v| v as f64));
            }
            played_frames += frames;
        }
        run.stats = stats.snapshot("test", 1, "f32");
        run
    }

    #[test]
    fn playout_without_disturbance_is_clean() {
        let run = run_playout(PlayoutSpec::new(128, 20.0));
        assert_eq!(clicks(&run.played), 0);
        assert_eq!(quiet_ms(&run.played), 0);
        assert_eq!(run.stats.realigns, 0);
    }

    #[test]
    fn playout_early_callbacks_recover_quickly() {
        // The media clock steps back by a callback (a new PTP leader): the
        // stream reads 128 samples closer to the network's edge than its
        // margin allows. It jumps back after three callbacks, with a dip
        // of a few ms instead of 100 ms of silence.
        for period in [64, 128, 256] {
            let spec =
                PlayoutSpec { clock_step: Some((10.0, -128.0)), ..PlayoutSpec::new(period, 20.0) };
            let run = run_playout(spec);
            assert_eq!(run.stats.realigns, 1, "{period}: {:?}", run.stats);
            assert_eq!(clicks(&run.played), 0, "{period}");
            assert!(quiet_ms(run.from(9.0)) <= 8, "{period}: {} ms quiet", quiet_ms(run.from(9.0)));
            assert!(run.stats.locked);
        }
    }

    #[test]
    fn playout_stall_drops_the_backlog() {
        // The device stalls for 20 ms: playback falls 20 ms behind. Slewed
        // back at 1000 ppm that would take 20 s; it jumps instead.
        let spec = PlayoutSpec { stall: Some((10.0, 960)), ..PlayoutSpec::new(128, 13.0) };
        let run = run_playout(spec);
        assert_eq!(run.stats.realigns, 1, "{:?}", run.stats);
        assert!(run.stats.error_samples.abs() < 5.0, "{:?}", run.stats);
        assert_eq!(clicks(&run.played), 0);
        assert!(quiet_ms(run.from(9.0)) <= 8, "{} ms quiet", quiet_ms(run.from(9.0)));
    }

    #[test]
    fn playout_first_big_callback_is_forgotten() {
        // WASAPI's first output callback fills the whole buffer (here 1056
        // frames), the rest are a period (480). The safety follows the
        // period once the first callback has been forgotten.
        let spec = PlayoutSpec { big: Some((0.0, 1056)), ..PlayoutSpec::new(480, 21.0) };
        let run = run_playout(spec);
        assert_eq!(run.stats.buffer_frames, 480, "{:?}", run.stats);
        let base = (480 + HALF + 48) as f64 / 48.0;
        assert!((run.stats.latency_ms - base).abs() < 1e-9, "{:?}", run.stats);
        assert!(run.stats.realigns <= 1, "{:?}", run.stats);
        assert_eq!(clicks(&run.played), 0);
        assert!(run.stats.locked);
    }

    #[test]
    fn playout_callback_size_hiccup_is_forgotten() {
        let spec = PlayoutSpec { big: Some((5.0, 1024)), ..PlayoutSpec::new(64, 21.0) };
        let run = run_playout(spec);
        assert_eq!(run.stats.buffer_frames, 64, "{:?}", run.stats);
        assert_eq!(run.stats.realigns, 2, "{:?}", run.stats);
        assert_eq!(clicks(&run.played), 0);
        assert!(run.stats.locked);
    }

    #[test]
    fn playout_fades_out_when_the_clock_goes() {
        let ring = Arc::new(TimedRing::new(1 << 15));
        let stats = Arc::new(StatsCell::new());
        let mut playout = Playout::new(RATE, 96, vec![Some(ring.clone())], 48, stats.clone());
        let start = sample_at(EPOCH_NS) as u64;
        ring.write(start - 2000, &vec![0x4000_0000; 30_000]);
        let mut out = vec![0f32; 64];
        let mut levels = Vec::new();
        for i in 0..200u64 {
            let ns = EPOCH_NS + (i * 64) as f64 / RATE as f64 * NS;
            playout.render(Some(ns as u64), &mut out);
        }
        assert!((out[63] - 0.5).abs() < 1e-6, "{}", out[63]);
        // The clock disappears: the level ramps down within the callback.
        for _ in 0..3 {
            playout.render(None, &mut out);
            levels.extend(out.iter().map(|&v| v as f64));
        }
        assert!(levels[0] > 0.45 && levels[0] < 0.5, "{}", levels[0]);
        assert!(levels.windows(2).all(|w| w[1] <= w[0]), "not a ramp down");
        assert_eq!(levels[63], 0.0);
    }
}
