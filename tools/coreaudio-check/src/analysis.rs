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
    /// Each delay the channel locked to, in order: the first lock, then one
    /// per delay change.
    pub locks: Vec<Lock>,
    /// Runs of bad frames after lock; bad frames less than `RUN_MERGE_GAP`
    /// apart count as one run.
    pub bad_runs: Vec<BadRun>,
    /// Some matched frames, as (received index, sent index): the first after
    /// each lock or slip, then every `MATCH_SAMPLE_EVERY`th.
    pub samples: Vec<(usize, u64)>,
}

/// The frames matched at one delay, from the lock to that delay until the
/// next change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Lock {
    /// Received index of the first frame matched at this delay.
    pub start: usize,
    /// The sent index of that frame.
    pub sent: u64,
    /// Received index of the last frame matched at this delay.
    pub end: usize,
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
                Some(n) => {
                    report.delay = Some(i as i64 - n as i64);
                    report.locks.push(Lock { start: i, sent: n, end: i });
                }
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
            if let Some(lock) = report.locks.last_mut() {
                lock.end = i;
            }
        } else {
            if let Some(m) = locate(i) {
                let d = i as i64 - m as i64;
                if d != delay {
                    report.delay_changes += 1;
                    report.delay = Some(d);
                    report.locks.push(Lock { start: i, sent: m, end: i });
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

    /// Whether device time `t` lies in a gap of this timeline: after its
    /// first cycle began and before its last ended, yet in no cycle, because
    /// the host skipped IO there.
    pub fn in_gap(&self, t: f64) -> bool {
        let (Some(&(_, _, first)), Some(&(_, frames, last))) =
            (self.cycles.first(), self.cycles.last())
        else {
            return false;
        };
        first <= t
            && t < last + f64::from(frames)
            && !self.cycles.iter().any(|&(_, n, s)| s <= t && t < s + f64::from(n))
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

/// Where a frame came back, in device time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// At the expected device delay.
    InPlace,
    /// Whole zero time stamp periods late, and played where the output
    /// timeline has a gap: the host skipped IO there and wrote no output, so
    /// the device played what the host's IO buffer held from an earlier lap.
    /// That is the host's loss, not a misplaced frame.
    Stale,
    /// Anywhere else, or where the timelines cannot tell.
    Misplaced,
}

/// A channel's delay changes, by cause (`DeviceTime::split_changes`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    /// Changes that moved audio in device time: the device's slips.
    pub slips: u64,
    /// Changes that skipped host IO explains.
    pub host: u64,
}

/// Judges where in device time the frames of one run came back, from the
/// input and output timelines of its IO cycles.
pub struct DeviceTime<'a> {
    pub input: &'a CycleMap,
    pub output: &'a CycleMap,
    /// The device delay every frame should come back at.
    pub want: i64,
    /// The zero time stamp period, the length of the host's IO buffer: what
    /// the buffer holds from its previous lap is this many frames old. 0 if
    /// unknown.
    pub period: i64,
}

impl DeviceTime<'_> {
    /// Input sample time minus output sample time of the frame received at
    /// index `received` that was sent at index `sent`.
    pub fn delay(&self, received: usize, sent: u64) -> Option<i64> {
        let (ti, to) = (self.input.time_of(received as u64)?, self.output.time_of(sent)?);
        Some((ti - to).round() as i64)
    }

    /// Whether the frame received at `received` was played at a device time
    /// the host wrote no output for.
    fn played_in_gap(&self, received: usize) -> bool {
        self.input
            .time_of(received as u64)
            .is_some_and(|t| self.output.in_gap(t - self.want as f64))
    }

    pub fn place(&self, received: usize, sent: u64) -> Placement {
        match self.delay(received, sent) {
            Some(d) if d == self.want => Placement::InPlace,
            Some(d)
                if self.period > 0
                    && (d - self.want) % self.period == 0
                    && self.played_in_gap(received) =>
            {
                Placement::Stale
            }
            _ => Placement::Misplaced,
        }
    }

    /// How many of `samples`, matched frames as (received index, sent index),
    /// came back stale and how many misplaced.
    pub fn split_samples(&self, samples: &[(usize, u64)]) -> (u64, u64) {
        let (mut stale, mut misplaced) = (0, 0);
        for &(i, n) in samples {
            match self.place(i, n) {
                Placement::InPlace => {}
                Placement::Stale => stale += 1,
                Placement::Misplaced => misplaced += 1,
            }
        }
        (stale, misplaced)
    }

    /// Sorts a channel's delay changes, between consecutive `locks`, into
    /// the device's slips and the host's.
    ///
    /// The delay in captured frames changes when the host skips IO although
    /// the device placed every frame right. A skip moves the input timeline
    /// at one point of the captured stream and the output timeline at
    /// another, so the frames played just before it come back at one
    /// captured delay and the frames after it at another, at the same device
    /// delay. Where the host wrote no output, the device plays what the
    /// host's buffer held a lap earlier, whole zero time stamp periods late.
    /// One skip can so make three changes: across the gap, into the earlier
    /// lap and out of it. None of them moved a frame in device time, so none
    /// is a slip. A change is a slip when the device delay changed between
    /// two locks and one of them was anywhere but in place or stale. Judging
    /// each change this way, rather than allowing so many slips per skip,
    /// still fails a device that slips next to a skip.
    pub fn split_changes(&self, locks: &[Lock]) -> Changes {
        // Lap-old data starts and ends inside a gap.
        let judge = |l: &Lock| {
            let place = match self.place(l.start, l.sent) {
                Placement::Stale if !self.played_in_gap(l.end) => Placement::Misplaced,
                p => p,
            };
            (self.delay(l.start, l.sent), place)
        };
        let mut changes = Changes::default();
        for w in locks.windows(2) {
            let ((a, pa), (b, pb)) = (judge(&w[0]), judge(&w[1]));
            let same_device_delay = a.is_some() && a == b;
            if same_device_delay || (pa != Placement::Misplaced && pb != Placement::Misplaced) {
                changes.host += 1;
            } else {
                changes.slips += 1;
            }
        }
        changes
    }
}

/// The number `key` has in a status line of `key=value` words, such as
/// OpenVirtualSoundcard's (`ovst`).
pub fn status_counter(status: &str, key: &str) -> Option<u64> {
    status.split_whitespace().find_map(|w| w.strip_prefix(key)?.strip_prefix('=')?.parse().ok())
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

    /// The zero time stamp period of the OpenVirtualSoundcard device.
    const PERIOD: i64 = 16_384;

    /// One channel of a loopback, recorded as on the CI runners: IO cycles of
    /// `frames`, the output timeline `lead` frames ahead of the input, and
    /// the device recording each frame at device delay 0 unless `delays`
    /// says otherwise. After each cycle in `skips` the host skips IO for the
    /// frames given, on both timelines at once; where it wrote no output, the
    /// device plays what the host's buffer held a zero time stamp period
    /// earlier, or silence before the buffer's first lap.
    struct Loop {
        frames: u32,
        lead: i64,
        cycles: u64,
        /// (cycle, frames skipped after it)
        skips: Vec<(u64, i64)>,
        /// (input time, device delay from then on)
        delays: Vec<(i64, i64)>,
    }

    struct Recorded {
        input: CycleMap,
        output: CycleMap,
        received: Vec<f32>,
        sent: u64,
    }

    impl Loop {
        fn new(frames: u32, lead: i64, cycles: u64) -> Loop {
            Loop { frames, lead, cycles, skips: Vec::new(), delays: Vec::new() }
        }

        /// The input time cycle `k` starts at.
        fn input_time(&self, k: u64) -> i64 {
            let skipped: i64 = self.skips.iter().filter(|s| s.0 < k).map(|s| s.1).sum();
            100_000 + (k * u64::from(self.frames)) as i64 + skipped
        }

        fn record(&self) -> Recorded {
            let n = u64::from(self.frames);
            let (mut input, mut output) = (CycleMap::default(), CycleMap::default());
            let mut played: HashMap<i64, f32> = HashMap::new();
            for k in 0..self.cycles {
                let t = self.input_time(k);
                input.push(k * n, self.frames, t as f64);
                output.push(k * n, self.frames, (t + self.lead) as f64);
                for j in 0..n {
                    played.insert(t + self.lead + j as i64, sample(0, k * n + j));
                }
            }
            let play = |t: i64| {
                (0..4).find_map(|lap| played.get(&(t - lap * PERIOD)).copied()).unwrap_or(0.0)
            };
            let delay = |t: i64| self.delays.iter().rev().find(|d| t >= d.0).map_or(0, |d| d.1);
            let mut received = Vec::new();
            for k in 0..self.cycles {
                for j in 0..self.frames as i64 {
                    let t = self.input_time(k) + j;
                    received.push(play(t - delay(t)));
                }
            }
            Recorded { input, output, received, sent: self.cycles * n }
        }

        /// The analysis, the delay changes by cause, and the sampled frames
        /// that came back stale and misplaced.
        fn judge(&self) -> (ChannelReport, Changes, (u64, u64)) {
            let rec = self.record();
            let r = analyze(0, 0, rec.sent, &rec.received);
            let time =
                DeviceTime { input: &rec.input, output: &rec.output, want: 0, period: PERIOD };
            let changes = time.split_changes(&r.locks);
            let split = time.split_samples(&r.samples);
            (r, changes, split)
        }
    }

    #[test]
    fn a_skip_makes_three_delay_changes_and_no_slip() {
        // S10 on a macos-latest runner: 512-frame cycles, output 1295 frames
        // ahead of input, Core Audio skipping 756 frames once.
        let mut l = Loop::new(512, 1295, 400);
        l.skips.push((200, 756));
        let (r, changes, (stale, misplaced)) = l.judge();
        // Across the gap, into the earlier lap, out of it: more than the 2
        // per jump the checker used to allow.
        assert_eq!(r.delay_changes, 3);
        assert_eq!(r.locks.len(), 4);
        assert_eq!(changes, Changes { slips: 0, host: 3 });
        assert!(stale > 0);
        assert_eq!(misplaced, 0);
        assert!(r.bad_runs.is_empty(), "{:?}", r.bad_runs);
    }

    #[test]
    fn a_skip_longer_than_the_lead_makes_two() {
        let mut l = Loop::new(512, 1295, 400);
        l.skips.push((200, 2_000));
        let (r, changes, (stale, misplaced)) = l.judge();
        assert_eq!(r.delay_changes, 2);
        assert_eq!(changes, Changes { slips: 0, host: 2 });
        assert!(stale > 0);
        assert_eq!(misplaced, 0);
    }

    #[test]
    fn many_skips_at_32_frames_make_no_slip() {
        // S5 at 32 frames on a runner: tens of skips in 5 s, some back to
        // back, of 44 to 600 frames.
        let mut l = Loop::new(32, 335, 7_500);
        let mut k = 900;
        for s in 0..60 {
            l.skips.push((k, 44 + (s * 97) % 557));
            k += if s % 5 == 0 { 1 } else { 90 + (s as u64 * 31) % 40 };
        }
        let (r, changes, (stale, misplaced)) = l.judge();
        assert!(r.delay_changes > 2 * 60, "{}", r.delay_changes);
        assert_eq!(changes.slips, 0);
        assert_eq!(changes.host, r.delay_changes);
        assert!(stale > 0);
        assert_eq!(misplaced, 0);
    }

    #[test]
    fn a_device_slip_is_a_slip() {
        // The device drops 3 frames: everything after comes back 3 early.
        let mut l = Loop::new(512, 1295, 400);
        l.delays.push((l.input_time(150) + 100, -3));
        let (r, changes, (_, misplaced)) = l.judge();
        assert_eq!(r.delay_changes, 1);
        assert_eq!(changes, Changes { slips: 1, host: 0 });
        assert!(misplaced > 0);
    }

    #[test]
    fn a_device_slip_next_to_a_skip_is_a_slip() {
        // The skip of the first test, and the device then slipping by 7
        // frames where the frames after the skip come back: it repeats the
        // last 7 frames of the earlier lap, then the rest comes 7 late.
        let mut l = Loop::new(512, 1295, 400);
        l.skips.push((200, 756));
        let after_gap = l.input_time(201) + 1295;
        l.delays.push((after_gap, 7));
        let (r, changes, (_, misplaced)) = l.judge();
        assert_eq!(r.delay_changes, 4);
        assert_eq!(changes, Changes { slips: 2, host: 2 });
        assert!(misplaced > 0);
        // 100 frames later, among the frames that came back in place.
        l.delays[0].0 = after_gap + 100;
        let (r, changes, _) = l.judge();
        assert_eq!(r.delay_changes, 4);
        assert_eq!(changes, Changes { slips: 1, host: 3 });
        // A slip that comes back is two.
        l.delays.push((l.input_time(260), 0));
        let (_, changes, _) = l.judge();
        assert_eq!(changes, Changes { slips: 2, host: 3 });
    }

    #[test]
    fn an_earlier_lap_where_the_host_wrote_output_is_misplaced() {
        // A device that replays its ring a lap late for a while, with no skip.
        let mut l = Loop::new(512, 1295, 400);
        l.delays.push((l.input_time(150), PERIOD));
        l.delays.push((l.input_time(170), 0));
        let (r, changes, (stale, misplaced)) = l.judge();
        assert_eq!(r.delay_changes, 2);
        assert_eq!(changes, Changes { slips: 2, host: 0 });
        assert_eq!(stale, 0);
        assert!(misplaced > 0);
    }

    #[test]
    fn an_earlier_lap_that_outlasts_the_gap_is_misplaced() {
        // The device goes on replaying the earlier lap after the host's gap.
        let mut l = Loop::new(512, 1295, 400);
        l.skips.push((200, 756));
        let gap_start = l.input_time(200) + 512 + 1295;
        l.delays.push((gap_start, PERIOD));
        l.delays.push((gap_start + 756 + 2_000, 0));
        let (r, changes, (_, misplaced)) = l.judge();
        assert_eq!(r.delay_changes, 3);
        assert_eq!(changes, Changes { slips: 2, host: 1 });
        assert!(misplaced > 0);
    }

    #[test]
    fn gaps_in_a_timeline() {
        let mut m = CycleMap::default();
        m.push(0, 512, 1_000.0);
        m.push(512, 512, 2_000.0);
        assert!(m.in_gap(1_512.0));
        assert!(m.in_gap(1_999.0));
        assert!(!m.in_gap(1_511.0));
        assert!(!m.in_gap(2_000.0));
        // Outside the run is no gap.
        assert!(!m.in_gap(999.0));
        assert!(!m.in_gap(2_512.0));
        assert!(!CycleMap::default().in_gap(0.0));
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
    fn counters_from_a_status_line() {
        let s = "daemon=attached clock=following missing=3 in_missing=6488 tx_underruns=x";
        assert_eq!(status_counter(s, "in_missing"), Some(6488));
        assert_eq!(status_counter(s, "missing"), Some(3));
        assert_eq!(status_counter(s, "tx_underruns"), None);
        assert_eq!(status_counter(s, "late_out"), None);
        assert_eq!(status_counter("error 'who?'", "in_missing"), None);
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
