//! `loopback`: plays the test signal on every output channel, records every
//! input channel, and checks what came back.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::analysis::{self, ChannelReport, CycleMap};
use crate::ca::*;
use crate::cli::Args;
use crate::commands::{device, fmt_err, info, status_string};

pub const OPTIONS: &[&str] = &[
    "--seconds",
    "--rate",
    "--buffer",
    "--channels",
    "--max-bad",
    "--max-slips",
    "--max-jumps",
    "--expect-device-delay",
    "--expect-rate-ppm",
    "--rate-tol-ppm",
    "--allow-outage",
    "--expect-silent-input",
    "--snapshot-prop",
];

struct Options {
    seconds: f64,
    rate: Option<f64>,
    buffer: Option<u32>,
    channels: Option<usize>,
    max_bad: u64,
    max_slips: u64,
    max_jumps: u64,
    expect_device_delay: Option<i64>,
    expect_rate_ppm: Option<f64>,
    rate_tol_ppm: f64,
    allow_outage: Option<f64>,
    expect_silent_input: bool,
    snapshot_prop: Option<u32>,
}

impl Options {
    fn from(a: &Args) -> Result<Options, String> {
        a.only(OPTIONS)?;
        let snapshot_prop = match a.str("--snapshot-prop") {
            None => None,
            Some(s) => Some(parse_fourcc(s).ok_or_else(|| format!("bad fourcc {s:?}"))?),
        };
        Ok(Options {
            seconds: a.get("--seconds")?.unwrap_or(10.0),
            rate: a.get("--rate")?,
            buffer: a.get("--buffer")?,
            channels: a.get("--channels")?,
            max_bad: a.get("--max-bad")?.unwrap_or(0),
            max_slips: a.get("--max-slips")?.unwrap_or(0),
            max_jumps: a.get("--max-jumps")?.unwrap_or(3),
            expect_device_delay: a.get("--expect-device-delay")?,
            expect_rate_ppm: a.get("--expect-rate-ppm")?,
            rate_tol_ppm: a.get("--rate-tol-ppm")?.unwrap_or(5.0),
            allow_outage: a.get("--allow-outage")?,
            expect_silent_input: a.switch("--expect-silent-input"),
            snapshot_prop,
        })
    }
}

#[derive(Clone, Copy, Default)]
struct Cycle {
    now_sample: f64,
    now_host: u64,
    in_sample: f64,
    in_host: u64,
    in_flags: u32,
    out_sample: f64,
    out_host: u64,
    /// Capture index and send index of the first frame of this cycle.
    in_start: u64,
    out_start: u64,
    in_frames: u32,
    out_frames: u32,
}

/// Everything the IO proc touches; allocated before IO starts.
struct Session {
    out_frame: u64,
    send_frames: u64,
    in_channels: usize,
    capture: Vec<f32>,
    capacity: usize,
    captured: AtomicUsize,
    in_total: u64,
    cycles: Vec<Cycle>,
    n_cycles: AtomicUsize,
}

unsafe extern "C" fn io_proc(
    _device: AudioObjectID,
    now: *const AudioTimeStamp,
    input: *const AudioBufferList,
    input_time: *const AudioTimeStamp,
    output: *mut AudioBufferList,
    output_time: *const AudioTimeStamp,
    client: *mut c_void,
) -> OSStatus {
    // SAFETY: `client` is the Session, used only on this thread while IO
    // runs; the main thread reads it after AudioDeviceStop.
    let s = unsafe { &mut *(client as *mut Session) };

    let out_start = s.out_frame;
    let mut frames_out = 0usize;
    let mut base = 0usize;
    for b in unsafe { AudioBufferList::buffers(output) } {
        let nch = b.mNumberChannels as usize;
        if nch == 0 || b.mData.is_null() {
            continue;
        }
        let frames = b.mDataByteSize as usize / (4 * nch);
        frames_out = frames;
        let data = unsafe { std::slice::from_raw_parts_mut(b.mData as *mut f32, frames * nch) };
        for f in 0..frames {
            let n = s.out_frame + f as u64;
            for c in 0..nch {
                data[f * nch + c] =
                    if n < s.send_frames { analysis::sample(base + c, n) } else { 0.0 };
            }
        }
        base += nch;
    }
    s.out_frame += frames_out as u64;

    let at = s.captured.load(Ordering::Relaxed);
    let in_start = s.in_total;
    let mut frames_in = 0usize;
    let mut base = 0usize;
    for b in unsafe { AudioBufferList::buffers(input) } {
        let nch = b.mNumberChannels as usize;
        if nch == 0 || b.mData.is_null() {
            continue;
        }
        let frames = b.mDataByteSize as usize / (4 * nch);
        frames_in = frames;
        let data = unsafe { std::slice::from_raw_parts(b.mData as *const f32, frames * nch) };
        for f in 0..frames.min(s.capacity.saturating_sub(at)) {
            for c in 0..nch.min(s.in_channels.saturating_sub(base)) {
                s.capture[(at + f) * s.in_channels + base + c] = data[f * nch + c];
            }
        }
        base += nch;
    }
    s.in_total += frames_in as u64;
    s.captured.store((at + frames_in).min(s.capacity), Ordering::Release);

    let k = s.n_cycles.load(Ordering::Relaxed);
    if k < s.cycles.len() && !now.is_null() && !input_time.is_null() && !output_time.is_null() {
        let (nt, it, ot) = unsafe { (&*now, &*input_time, &*output_time) };
        s.cycles[k] = Cycle {
            now_sample: nt.mSampleTime,
            now_host: nt.mHostTime,
            in_sample: it.mSampleTime,
            in_host: it.mHostTime,
            in_flags: it.mFlags,
            out_sample: ot.mSampleTime,
            out_host: ot.mHostTime,
            in_start,
            out_start,
            in_frames: frames_in as u32,
            out_frames: frames_out as u32,
        };
        s.n_cycles.store(k + 1, Ordering::Release);
    }
    0
}

fn check_format(d: AudioObjectID, scope: u32, label: &str) -> bool {
    let mut ok = true;
    for s in streams(d, scope) {
        match get::<AudioStreamBasicDescription>(
            s,
            kAudioStreamPropertyVirtualFormat,
            kAudioObjectPropertyScopeGlobal,
        ) {
            Ok(f)
                if f.mFormatID == kAudioFormatLinearPCM
                    && f.mFormatFlags & kAudioFormatFlagIsFloat != 0
                    && f.mBitsPerChannel == 32 => {}
            other => {
                eprintln!("{label} stream {s} is not 32-bit float: {}", fmt_err(other));
                ok = false;
            }
        }
    }
    ok
}

fn snapshot(d: AudioObjectID, prop: u32, label: &str) {
    let v = if prop == fourcc(b"ovst") {
        status_string(d)
    } else {
        get_string_el(d, prop, kAudioObjectPropertyScopeGlobal, 0)
            .unwrap_or_else(|s| format!("error {}", status_str(s)))
    };
    println!("snapshot {label} '{}': {v}", fourcc_str(prop));
}

pub fn run(a: &Args) -> Result<i32, String> {
    let o = Options::from(a)?;
    let uid = a.positional.get(1).ok_or("loopback needs <uid>")?;
    let d = device(uid)?;
    let g = kAudioObjectPropertyScopeGlobal;
    if let Some(rate) = o.rate {
        let current: f64 = get(d, kAudioDevicePropertyNominalSampleRate, g).unwrap_or(0.0);
        if current != rate {
            set(d, kAudioDevicePropertyNominalSampleRate, g, rate)
                .map_err(|s| format!("cannot set the sample rate to {rate}: {}", status_str(s)))?;
            let start = Instant::now();
            while get::<f64>(d, kAudioDevicePropertyNominalSampleRate, g).ok() != Some(rate) {
                if start.elapsed() > Duration::from_secs(5) {
                    return Err(format!("the sample rate did not change to {rate}"));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    if let Some(frames) = o.buffer
        && let Err(s) = set(d, kAudioDevicePropertyBufferFrameSize, g, frames)
    {
        eprintln!("cannot set the buffer size to {frames}: {}", status_str(s));
    }
    info(d);
    let output_ok = check_format(d, kAudioObjectPropertyScopeOutput, "output");
    let input_ok = check_format(d, kAudioObjectPropertyScopeInput, "input");
    if !(output_ok && input_ok) {
        return Ok(1);
    }

    let rate: f64 = get(d, kAudioDevicePropertyNominalSampleRate, g).unwrap_or(48_000.0);
    let zts_period: u32 = get(d, kAudioDevicePropertyZeroTimeStampPeriod, g).unwrap_or(0);
    let outs: u32 =
        stream_channels(d, kAudioObjectPropertyScopeOutput).unwrap_or_default().iter().sum();
    let ins: u32 =
        stream_channels(d, kAudioObjectPropertyScopeInput).unwrap_or_default().iter().sum();
    let channels = (outs.min(ins) as usize).min(o.channels.unwrap_or(usize::MAX));
    if channels == 0 {
        return Err(format!("the device needs input and output channels ({ins} in, {outs} out)"));
    }
    let send_frames = (o.seconds * rate) as u64;
    let capacity = ((o.seconds + 2.0) * rate) as usize;
    let mut session = Box::new(Session {
        out_frame: 0,
        send_frames,
        in_channels: ins as usize,
        capture: vec![0.0; capacity * ins as usize],
        capacity,
        captured: AtomicUsize::new(0),
        in_total: 0,
        // Enough for 16-frame buffers.
        cycles: vec![Cycle::default(); capacity / 16 + 64],
        n_cycles: AtomicUsize::new(0),
    });

    let mut proc_id: AudioDeviceIOProcID = None;
    let client = &mut *session as *mut Session as *mut c_void;
    let s = unsafe { AudioDeviceCreateIOProcID(d, io_proc, client, &mut proc_id) };
    if s != 0 {
        return Err(format!("AudioDeviceCreateIOProcID: {}", status_str(s)));
    }
    let started = Instant::now();
    let s = unsafe { AudioDeviceStart(d, proc_id) };
    if s != 0 {
        unsafe { AudioDeviceDestroyIOProcID(d, proc_id) };
        return Err(format!("AudioDeviceStart: {}", status_str(s)));
    }
    println!("running {channels} channels for {} s at {rate} Hz", o.seconds);
    let deadline = Duration::from_secs_f64(o.seconds + 10.0);
    let snapshots = [(0.25, "at 25%"), (0.75, "at 75%")];
    let mut next_snapshot = 0;
    while session.captured.load(Ordering::Acquire) < capacity && started.elapsed() < deadline {
        if let Some(prop) = o.snapshot_prop
            && let Some(&(at, label)) = snapshots.get(next_snapshot)
            && started.elapsed().as_secs_f64() >= at * o.seconds
        {
            snapshot(d, prop, label);
            next_snapshot += 1;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    unsafe {
        AudioDeviceStop(d, proc_id);
        AudioDeviceDestroyIOProcID(d, proc_id);
    }
    if let Some(prop) = o.snapshot_prop {
        snapshot(d, prop, "at end");
    }
    let wall = started.elapsed().as_secs_f64();
    let captured = session.captured.load(Ordering::Acquire);
    let n_cycles = session.n_cycles.load(Ordering::Acquire);
    println!("captured {captured} frames in {wall:.2} s over {n_cycles} IO cycles");

    let mut failures = Vec::new();
    let mut result: BTreeMap<&str, String> = BTreeMap::new();
    if captured < capacity {
        failures.push(format!("IO delivered only {captured} of {capacity} frames"));
    }

    let cycles = &session.cycles[..n_cycles];
    for (k, c) in cycles.iter().take(8).enumerate() {
        println!(
            "cycle {k}: now {} @ {} | input {} @ {} | output {} @ {} | frames in {} out {}",
            c.now_sample,
            c.now_host,
            c.in_sample,
            c.in_host,
            c.out_sample,
            c.out_host,
            c.in_frames,
            c.out_frames
        );
    }
    if let Some(first) = cycles.first() {
        println!(
            "output leads input by {} frames; now - input = {} frames, output - now = {} frames",
            first.out_sample - first.in_sample,
            first.now_sample - first.in_sample,
            first.out_sample - first.now_sample
        );
    }

    // Discontinuities in input sample time are overloads or resyncs.
    let mut jumps = 0u64;
    for w in cycles.windows(2) {
        if w[1].in_sample != w[0].in_sample + w[0].in_frames as f64 {
            jumps += 1;
            if jumps <= 10 {
                println!(
                    "timeline jump: input sample time {} -> {} after {} frames",
                    w[0].in_sample, w[1].in_sample, w[0].in_frames
                );
            }
        }
    }
    println!("timeline jumps: {jumps}");
    result.insert("jumps", jumps.to_string());
    if jumps > o.max_jumps {
        failures.push(format!("{jumps} timeline jumps (max {})", o.max_jumps));
    }

    let points: Vec<(f64, u64)> = cycles
        .iter()
        .filter(|c| {
            c.in_flags & kAudioTimeStampSampleTimeValid != 0
                && c.in_flags & kAudioTimeStampHostTimeValid != 0
        })
        .map(|c| (c.in_sample, ticks_to_ns(c.in_host)))
        .collect();
    match analysis::fit_rate(&points) {
        Some(r) => {
            let ppm = (r / rate - 1.0) * 1e6;
            println!("measured rate: {r:.3} Hz ({ppm:+.2} ppm against the host clock)");
            result.insert("rate_hz", format!("{r:.3}"));
            result.insert("rate_ppm", format!("{ppm:.2}"));
            if let Some(want) = o.expect_rate_ppm
                && (ppm - want).abs() > o.rate_tol_ppm
            {
                failures
                    .push(format!("rate {ppm:+.2} ppm, want {want:+.2} +- {} ppm", o.rate_tol_ppm));
            }
        }
        None => {
            println!("measured rate: unknown");
            if o.expect_rate_ppm.is_some() {
                failures.push("no valid timestamps to measure the rate".into());
            }
        }
    }

    let received = |c: usize| -> Vec<f32> {
        (0..captured).map(|f| session.capture[f * ins as usize + c]).collect()
    };
    result.insert("channels", channels.to_string());

    if o.expect_silent_input {
        let mut loud = Vec::new();
        for c in 0..ins as usize {
            if !analysis::is_silent(&received(c)) {
                loud.push(c);
            }
        }
        println!("silent input: {}", if loud.is_empty() { "yes" } else { "no" });
        if !loud.is_empty() {
            failures.push(format!("input channels {loud:?} were not silent"));
        }
    } else {
        if (0..ins as usize).all(|c| analysis::is_silent(&received(c))) {
            println!(
                "every input channel was silent. On your own Mac, check that your terminal app \
                 may use the microphone (System Settings > Privacy & Security > Microphone): \
                 without that permission macOS records silence"
            );
        }
        let (mut in_map, mut out_map) = (CycleMap::default(), CycleMap::default());
        for c in cycles {
            in_map.push(c.in_start, c.in_frames, c.in_sample);
            out_map.push(c.out_start, c.out_frames, c.out_sample);
        }
        // Each jump the host reports may lose what was being played and
        // recorded then; allow for it. Anything beyond is the device's fault.
        let cycle_frames = cycles.iter().map(|c| c.in_frames as u64).max().unwrap_or(0);
        let allowed_bad = o.max_bad + jumps * 4 * cycle_frames;
        let outage_frames = o.allow_outage.map(|s| (s * rate) as u64);
        let allowed_slips = o.max_slips + 2 * jumps + if outage_frames.is_some() { 2 } else { 0 };
        if jumps > 0 {
            println!(
                "allowing for the jumps: up to {allowed_bad} bad frames and {allowed_slips} slips per channel"
            );
        }
        let mut reports: Vec<ChannelReport> = Vec::new();
        let mut device_hist: BTreeMap<i64, u64> = BTreeMap::new();
        for c in 0..channels {
            let r = analysis::analyze(c, c, send_frames, &received(c));
            let hist = analysis::device_delays(&r.samples, &in_map, &out_map);
            for (k, v) in &hist {
                *device_hist.entry(*k).or_insert(0) += v;
            }
            let longest = r.bad_runs.iter().copied().max_by_key(|run| run.len).unwrap_or_default();
            println!(
                "input {c} <- output {}: lead-in {} frames, delay {} (device time {:?}), matched {}, bad {} ({} zero) in {} runs (longest {}), slips {}",
                r.output,
                r.lead_in,
                r.delay.map_or("none".into(), |d| d.to_string()),
                hist,
                r.matched,
                r.mismatched,
                r.zeros,
                r.bad_runs.len(),
                longest.len,
                r.delay_changes
            );
            if !r.bad_runs.is_empty() {
                let runs: Vec<String> = r
                    .bad_runs
                    .iter()
                    .take(8)
                    .map(|run| format!("{:.3} s ({})", run.start as f64 / rate, run.len))
                    .collect();
                let more = r.bad_runs.len().saturating_sub(runs.len());
                let more = if more > 0 { format!(" and {more} more") } else { String::new() };
                println!("  input {c} bad runs at {}{more}", runs.join(", "));
            }
            if let (0, Some(want)) = (c, o.expect_device_delay) {
                let off = r.samples.iter().filter_map(|&(i, n)| {
                    let (ti, to) = (in_map.time_of(i as u64)?, out_map.time_of(n)?);
                    ((ti - to).round() as i64 != want).then_some((i, n, ti, to))
                });
                for (i, n, ti, to) in off.take(5) {
                    println!(
                        "  input 0 frame {i} (input time {ti}, cycle {:?}) was sent as frame {n} \
                         (output time {to}, cycle {:?})",
                        in_map.cycle_of(i as u64),
                        out_map.cycle_of(n)
                    );
                }
            }
            if r.delay.is_none() {
                failures.push(format!("input {c}: never received output {c}"));
            } else {
                let mut bad = r.mismatched;
                if let Some(limit) = outage_frames {
                    if longest.len > limit {
                        failures.push(format!(
                            "input {c}: an outage of {} frames (max {limit})",
                            longest.len
                        ));
                    } else {
                        // The one allowed outage: its bad frames, since a run
                        // is measured from its first bad frame to its last
                        // and can hold frames that matched.
                        bad -= longest.bad;
                    }
                }
                if bad > allowed_bad {
                    failures.push(format!("input {c}: {bad} bad frames (max {allowed_bad})"));
                }
                if r.delay_changes > allowed_slips {
                    failures.push(format!(
                        "input {c}: {} slips (max {allowed_slips})",
                        r.delay_changes
                    ));
                }
                if r.analyzed() < send_frames / 2 {
                    failures.push(format!("input {c}: only {} frames compared", r.analyzed()));
                }
            }
            reports.push(r);
        }
        let delays: Vec<Option<i64>> = reports.iter().map(|r| r.delay).collect();
        if outage_frames.is_none() && delays.windows(2).any(|w| w[0] != w[1]) {
            failures.push(format!("channels came back with different delays: {delays:?}"));
        }
        println!("device-time delay over all channels: {device_hist:?}");
        if let Some(want) = o.expect_device_delay {
            // When the host skips IO it can hand over its IO buffer where
            // nobody wrote this lap: data exactly a zero time stamp period
            // (the buffer's length) old. That is loss on the host's side,
            // covered by the jump allowance, not a misplaced ring.
            let (stale_frames, wrong) =
                analysis::split_delays(&device_hist, want, i64::from(zts_period), jumps > 0);
            if stale_frames > 0 {
                println!(
                    "{stale_frames} sampled frames came back whole zero time stamp periods late: \
                     host buffer data left from an earlier lap while the host skipped IO"
                );
            }
            if device_hist.is_empty() {
                failures.push("no matched frames to measure the device-time delay".into());
            } else if wrong > 0 {
                failures.push(format!(
                    "device-time delay {device_hist:?}, want {want} for every frame"
                ));
            }
        }
        if let Some((&k, _)) = device_hist.iter().max_by_key(|(_, v)| **v) {
            result.insert("device_delay", k.to_string());
        }
        result.insert("capture_delay", format!("{:?}", delays.first().copied().flatten()));
        result.insert("matched", reports.iter().map(|r| r.matched).sum::<u64>().to_string());
        result.insert("bad", reports.iter().map(|r| r.mismatched).sum::<u64>().to_string());
        result.insert("slips", reports.iter().map(|r| r.delay_changes).sum::<u64>().to_string());
        result.insert("runs", reports.iter().map(|r| r.bad_runs.len()).sum::<usize>().to_string());
    }

    let pass = failures.is_empty();
    for f in &failures {
        println!("FAIL: {f}");
    }
    println!("RESULT: {}", if pass { "PASS" } else { "FAIL" });
    let kv: Vec<String> = result.iter().map(|(k, v)| format!("{k}={v}")).collect();
    println!("CA-RESULT result={} {}", if pass { "PASS" } else { "FAIL" }, kv.join(" "));
    Ok(if pass { 0 } else { 1 })
}
