//! The test signal and the checks run on what comes back. Plain Rust with no
//! Core Audio, so it is tested on every OS.

use std::collections::HashMap;

const SCALE: f32 = (1u32 << 23) as f32;

/// The integer sent on `channel` at frame `n`: pseudo-random in
/// [-2^22, 2^22), different on every channel.
pub fn code(channel: usize, n: u64) -> i32 {
    let h = splitmix64(n ^ (channel as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    (h >> 41) as i32 - (1 << 22)
}

/// The sample sent on `channel` at frame `n`: `code / 2^23`, at most -6 dBFS.
///
/// These values are exact in `f32`, and in the 24-bit integers on the wire, so
/// a correct path through the driver and the network returns them bit for
/// bit.
pub fn sample(channel: usize, n: u64) -> f32 {
    code(channel, n) as f32 / SCALE
}

/// The code a received sample carries, if it is one of ours.
pub fn to_code(x: f32) -> Option<i32> {
    let y = x * SCALE;
    (y.fract() == 0.0 && y.abs() <= SCALE).then_some(y as i32)
}

fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// What came back on one input channel.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ChannelReport {
    pub input: usize,
    /// The output channel it is compared with.
    pub output: usize,
    /// Frames received before the first recognised sample (round-trip
    /// latency plus start-up).
    pub lead_in: u64,
    /// Received index minus sent index, once locked.
    pub delay: Option<i64>,
    /// Frames that equal the sent sample at the locked delay.
    pub matched: u64,
    /// Frames after lock that do not (dropouts, glitches, wrong data).
    pub mismatched: u64,
    /// Of the mismatched frames, how many were exactly zero.
    pub zeros: u64,
    /// How often the delay changed after the first lock.
    pub delay_changes: u64,
    /// Runs of bad frames after lock; bad frames less than `RUN_MERGE_GAP`
    /// apart count as one run.
    pub bad_runs: Vec<BadRun>,
    /// Some matched frames, as (received index, sent index): the first after
    /// each lock or slip, then every `MATCH_SAMPLE_EVERY`th.
    pub samples: Vec<(usize, u64)>,
}

/// A run of bad frames after lock, with whatever matched in between.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BadRun {
    /// Received index of the first bad frame.
    pub start: usize,
    /// Frames from the first bad frame to the last, both included.
    pub len: u64,
    /// How many of those were bad. A frame in the middle of an outage can
    /// still match: one whose sent sample was exactly zero.
    pub bad: u64,
}

/// Bad frames closer than this belong to the same run.
pub const RUN_MERGE_GAP: usize = 1024;
/// How sparsely matched frames are kept in `ChannelReport::samples`.
pub const MATCH_SAMPLE_EVERY: u64 = 64;

impl ChannelReport {
    pub fn analyzed(&self) -> u64 {
        self.matched + self.mismatched
    }
}

/// Compares `received` (one input channel, one value per frame) with what
/// was sent on `output` during frames `0..sent_frames`.
pub fn analyze(input: usize, output: usize, sent_frames: u64, received: &[f32]) -> ChannelReport {
    // Index every pair of consecutive sent codes; pairs of 23-bit values are
    // unique in a test of a few million frames with overwhelming
    // probability.
    let mut index: HashMap<(i32, i32), u64> = HashMap::with_capacity(sent_frames as usize);
    for n in 0..sent_frames.saturating_sub(1) {
        index.insert((code(output, n), code(output, n + 1)), n);
    }
    let locate = |i: usize| -> Option<u64> {
        let a = to_code(*received.get(i)?)?;
        let b = to_code(*received.get(i + 1)?)?;
        index.get(&(a, b)).copied()
    };

    let mut report = ChannelReport { input, output, ..Default::default() };
    let mut fresh_lock = true;
    let mut i = 0usize;
    while i < received.len() {
        let Some(delay) = report.delay else {
            match locate(i) {
                Some(n) => report.delay = Some(i as i64 - n as i64),
                None => {
                    report.lead_in += 1;
                    i += 1;
                }
            }
            continue;
        };
        let n = i as i64 - delay;
        if n < 0 || n as u64 >= sent_frames {
            // Past the end of what was sent: nothing more to compare.
            break;
        }
        let x = received[i];
        if to_code(x) == Some(code(output, n as u64)) {
            if report.matched.is_multiple_of(MATCH_SAMPLE_EVERY) || fresh_lock {
                report.samples.push((i, n as u64));
            }
            fresh_lock = false;
            report.matched += 1;
        } else {
            if let Some(m) = locate(i) {
                let d = i as i64 - m as i64;
                if d != delay {
                    report.delay_changes += 1;
                    report.delay = Some(d);
                    fresh_lock = true;
                    // Re-check this frame at the new delay.
                    continue;
                }
            }
            report.mismatched += 1;
            if x == 0.0 {
                report.zeros += 1;
            }
            match report.bad_runs.last_mut() {
                Some(run) if i <= run.start + run.len as usize + RUN_MERGE_GAP => {
                    run.len = (i - run.start + 1) as u64;
                    run.bad += 1;
                }
                _ => report.bad_runs.push(BadRun { start: i, len: 1, bad: 1 }),
            }
        }
        i += 1;
    }
    report
}

/// Maps a running frame index (frames written or captured by the IO proc)
/// to the device sample time of that frame, from the per-cycle log.
#[derive(Debug, Default, Clone)]
pub struct CycleMap {
    /// (first frame index of the cycle, frames in the cycle, device sample
    /// time of that first frame), in cycle order.
    cycles: Vec<(u64, u32, f64)>,
}

impl CycleMap {
    pub fn push(&mut self, start: u64, frames: u32, sample_time: f64) {
        if frames > 0 {
            self.cycles.push((start, frames, sample_time));
        }
    }

    /// The cycle that covered frame `index`, as (first frame index, frames,
    /// device sample time of its first frame).
    pub fn cycle_of(&self, index: u64) -> Option<(u64, u32, f64)> {
        let k = self.cycles.partition_point(|&(start, _, _)| start <= index).checked_sub(1)?;
        let c = self.cycles[k];
        (index < c.0 + c.1 as u64).then_some(c)
    }

    /// The device sample time of frame `index`, if a cycle covered it.
    pub fn time_of(&self, index: u64) -> Option<f64> {
        let k = self.cycles.partition_point(|&(start, _, _)| start <= index).checked_sub(1)?;
        let (start, frames, t) = self.cycles[k];
        (index < start + frames as u64).then(|| t + (index - start) as f64)
    }
}

/// Delay in device time between when each sampled frame was played and when
/// it was recorded: input sample time minus output sample time. Returns a
/// histogram of whole-frame delays.
pub fn device_delays(
    samples: &[(usize, u64)],
    input: &CycleMap,
    output: &CycleMap,
) -> std::collections::BTreeMap<i64, u64> {
    let mut hist = std::collections::BTreeMap::new();
    for &(received, sent) in samples {
        if let (Some(ti), Some(to)) = (input.time_of(received as u64), output.time_of(sent)) {
            *hist.entry((ti - to).round() as i64).or_insert(0) += 1;
        }
    }
    hist
}

/// Splits the sampled frames of a device-delay histogram that are not at
/// `want` into (stale, wrong). Stale frames are whole multiples of `period`
/// (the zero time stamp period, the length of the host's IO buffer) away,
/// and only while the host skipped IO: it then hands over buffer data left
/// from an earlier lap. Every other delay is wrong.
pub fn split_delays(
    hist: &std::collections::BTreeMap<i64, u64>,
    want: i64,
    period: i64,
    host_skipped: bool,
) -> (u64, u64) {
    let (mut stale, mut wrong) = (0, 0);
    for (&k, &n) in hist {
        if k == want {
            continue;
        }
        if host_skipped && period > 0 && (k - want) % period == 0 {
            stale += n;
        } else {
            wrong += n;
        }
    }
    (stale, wrong)
}

/// True when every captured sample is exactly zero.
pub fn is_silent(received: &[f32]) -> bool {
    received.iter().all(|&x| x == 0.0)
}

/// Least-squares rate of sample time against host time, in samples per
/// second. Points are (sample time, host time in ns).
pub fn fit_rate(points: &[(f64, u64)]) -> Option<f64> {
    if points.len() < 2 {
        return None;
    }
    let (s0, h0) = points[0];
    let n = points.len() as f64;
    let (mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0);
    for &(s, h) in points {
        let x = (h - h0) as f64 * 1e-9;
        let y = s - s0;
        sx += x;
        sy += y;
        sxx += x * x;
        sxy += x * y;
    }
    let den = n * sxx - sx * sx;
    (den > 0.0).then(|| (n * sxy - sx * sy) / den)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent(output: usize, frames: u64) -> Vec<f32> {
        (0..frames).map(|n| sample(output, n)).collect()
    }

    #[test]
    fn samples_are_exact_in_f32_and_24_bit() {
        for n in 0..100_000 {
            let x = sample(3, n);
            assert!(x.abs() <= 0.5);
            assert_eq!(to_code(x), Some(code(3, n)));
            // f32 -> i32 left-justified -> 24-bit -> back, as the driver does.
            let wire = ((x as f64 * 2_147_483_648.0) as i32) >> 8;
            let back = ((wire << 8) as f64 / 2_147_483_648.0) as f32;
            assert_eq!(back.to_bits(), x.to_bits());
        }
        assert_ne!(code(0, 7), code(1, 7));
    }

    #[test]
    fn clean_loop_locks_at_the_delay() {
        let mut rx = vec![0.0; 300];
        rx.extend(sent(1, 10_000));
        let r = analyze(0, 1, 10_000, &rx);
        assert_eq!(r.lead_in, 300);
        assert_eq!(r.delay, Some(300));
        assert_eq!(r.matched, 10_000);
        assert_eq!(r.mismatched, 0);
        assert_eq!(r.delay_changes, 0);
    }

    #[test]
    fn dropouts_and_slips_are_counted() {
        let tx = sent(0, 20_000);
        let mut rx = vec![0.0; 100];
        rx.extend(&tx[..5_000]);
        // 64 frames of silence replacing audio (a dropout, same timeline).
        rx.extend(std::iter::repeat_n(0.0, 64));
        rx.extend(&tx[5_064..10_000]);
        // 32 frames lost (the timeline slips).
        rx.extend(&tx[10_032..20_000]);
        let r = analyze(0, 0, 20_000, &rx);
        assert_eq!(r.delay, Some(100 - 32));
        assert_eq!(r.zeros, 64);
        assert_eq!(r.mismatched, 64);
        assert_eq!(r.delay_changes, 1);
        assert_eq!(r.matched, 20_000 - 64 - 32);
    }

    #[test]
    fn wrong_channel_never_locks() {
        let rx = sent(2, 5_000);
        let r = analyze(0, 1, 5_000, &rx);
        assert_eq!(r.delay, None);
        assert_eq!(r.matched, 0);
        assert_eq!(r.lead_in, 5_000);
    }

    #[test]
    fn bad_runs_merge_and_split() {
        let tx = sent(0, 60_000);
        let mut rx = vec![0.0; 50];
        rx.extend(&tx[..10_000]);
        // One outage of 2000 frames (zeros), the same timeline after it.
        rx.extend(std::iter::repeat_n(0.0, 2_000));
        rx.extend(&tx[12_000..30_000]);
        let one = analyze(0, 0, 30_000, &rx);
        assert_eq!(one.bad_runs, vec![BadRun { start: 10_050, len: 2_000, bad: 2_000 }]);
        // A second outage far from the first is a second run.
        rx.extend(std::iter::repeat_n(0.0, 500));
        rx.extend(&tx[30_500..60_000]);
        let two = analyze(0, 0, 60_000, &rx);
        assert_eq!(two.bad_runs.len(), 2);
        assert_eq!(two.bad_runs[1].len, 500);
        assert_eq!(two.delay_changes, 0);
    }

    #[test]
    fn outage_with_a_slip_is_one_run() {
        let tx = sent(0, 40_000);
        let mut rx = vec![0.0; 10];
        rx.extend(&tx[..10_000]);
        rx.extend(std::iter::repeat_n(0.0, 3_000));
        // Comes back 700 frames later in the signal (the delay changed).
        rx.extend(&tx[13_700..40_000]);
        let r = analyze(0, 0, 40_000, &rx);
        assert_eq!(r.delay_changes, 1);
        assert_eq!(r.bad_runs.len(), 1);
        assert_eq!(r.bad_runs[0].start, 10_010);
    }

    #[test]
    fn a_frame_that_matches_inside_an_outage_is_not_bad() {
        let tx = sent(0, 30_000);
        let mut rx = tx[..10_000].to_vec();
        rx.extend(std::iter::repeat_n(0.0, 2_000));
        rx.extend(&tx[12_000..30_000]);
        // As if the sample sent there had been zero: it reads back right.
        rx[11_000] = tx[11_000];
        let r = analyze(0, 0, 30_000, &rx);
        assert_eq!(r.bad_runs, vec![BadRun { start: 10_000, len: 2_000, bad: 1_999 }]);
        assert_eq!(r.mismatched, 1_999);
    }

    #[test]
    fn stale_host_buffer_data_is_whole_periods_late() {
        let hist: std::collections::BTreeMap<i64, u64> =
            [(0, 500), (16_384, 7), (32_768, 2), (-16_384, 1), (3, 4)].into();
        assert_eq!(split_delays(&hist, 0, 16_384, true), (10, 4));
        assert_eq!(split_delays(&hist, 0, 16_384, false), (0, 14));
        assert_eq!(split_delays(&hist, 0, 0, true), (0, 14));
        assert_eq!(split_delays(&hist, 3, 16_384, true), (0, 510));
    }

    #[test]
    fn device_delay_survives_timeline_jumps() {
        // 512-frame cycles. The input timeline jumps by 1000 frames after
        // cycle 20 and the output timeline jumps with it, as when the host
        // skips IO; the device-time delay stays 256 throughout, although the
        // delay in captured frames changes.
        let (mut input, mut output) = (CycleMap::default(), CycleMap::default());
        let mut t = 10_000.0;
        for k in 0..60u64 {
            if k == 20 {
                t += 1_000.0;
            }
            output.push(k * 512, 512, t + 256.0);
            input.push(k * 512, 512, t);
            t += 512.0;
        }
        // Frame n is played at output time out(n) and recorded at input
        // time out(n) + 256 = in(i); find i for every sampled n.
        let mut samples = Vec::new();
        for n in (0..25_000u64).step_by(97) {
            let target = output.time_of(n).unwrap() + 256.0;
            let i = (0..30_720u64).find(|&i| input.time_of(i) == Some(target));
            if let Some(i) = i {
                samples.push((i as usize, n));
            }
        }
        assert!(samples.len() > 200);
        let hist = device_delays(&samples, &input, &output);
        assert_eq!(hist.keys().copied().collect::<Vec<_>>(), vec![256]);
        assert_eq!(input.time_of(60 * 512), None);
        // The cycle around a frame, across the jump.
        assert_eq!(input.cycle_of(19 * 512 + 3), Some((19 * 512, 512, 10_000.0 + 19.0 * 512.0)));
        assert_eq!(input.cycle_of(20 * 512), Some((20 * 512, 512, 11_000.0 + 20.0 * 512.0)));
        assert_eq!(input.cycle_of(60 * 512), None);
    }

    #[test]
    fn silent_input() {
        assert!(is_silent(&[0.0; 100]));
        assert!(!is_silent(&[0.0, 0.0, sample(0, 1)]));
    }

    #[test]
    fn rate_fit_with_offset() {
        let ppm = 50.0;
        let rate = 48_000.0 * (1.0 + ppm * 1e-6);
        let pts: Vec<(f64, u64)> = (0..2_000)
            .map(|i| (i as f64 * 512.0, (i as f64 * 512.0 / rate * 1e9) as u64))
            .collect();
        let r = fit_rate(&pts).unwrap();
        assert!(((r / 48_000.0 - 1.0) * 1e6 - ppm).abs() < 0.05, "{r}");
    }

    #[test]
    fn rate_fit() {
        let pts: Vec<(f64, u64)> = (0..100)
            .map(|i| (i as f64 * 512.0, (i as f64 * 512.0 / 48_010.0 * 1e9) as u64))
            .collect();
        let r = fit_rate(&pts).unwrap();
        assert!((r - 48_010.0).abs() < 0.01, "{r}");
    }
}
