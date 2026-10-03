//! Everything except `loopback`: listing, properties, waiting and the
//! conformance checks.

use std::time::{Duration, Instant};

use crate::ca::*;
use crate::cli::Args;

pub fn device(uid: &str) -> Result<AudioObjectID, String> {
    device_by_uid(uid).ok_or_else(|| {
        list();
        format!("no device with UID {uid:?}")
    })
}

pub fn list() {
    for d in devices() {
        let g = kAudioObjectPropertyScopeGlobal;
        let uid = get_string(d, kAudioDevicePropertyDeviceUID, g).unwrap_or_default();
        let name = get_string(d, kAudioObjectPropertyName, g).unwrap_or_default();
        let inputs: u32 =
            stream_channels(d, kAudioObjectPropertyScopeInput).unwrap_or_default().iter().sum();
        let outputs: u32 =
            stream_channels(d, kAudioObjectPropertyScopeOutput).unwrap_or_default().iter().sum();
        let rate: f64 = get(d, kAudioDevicePropertyNominalSampleRate, g).unwrap_or(0.0);
        println!("{d:4}  {inputs:3} in {outputs:3} out  {rate:7.0} Hz  {name:?}  uid {uid:?}");
    }
}

pub fn fmt_err<T: std::fmt::Debug>(r: Result<T, OSStatus>) -> String {
    match r {
        Ok(v) => format!("{v:?}"),
        Err(s) => format!("error {}", status_str(s)),
    }
}

fn fmt_fourcc(r: Result<u32, OSStatus>) -> String {
    match r {
        Ok(v) => format!("'{}'", fourcc_str(v)),
        Err(s) => format!("error {}", status_str(s)),
    }
}

pub fn info(d: AudioObjectID) {
    let g = kAudioObjectPropertyScopeGlobal;
    println!("device id: {d}");
    println!("name: {}", fmt_err(get_string(d, kAudioObjectPropertyName, g)));
    println!("manufacturer: {}", fmt_err(get_string(d, kAudioObjectPropertyManufacturer, g)));
    println!("uid: {}", fmt_err(get_string(d, kAudioDevicePropertyDeviceUID, g)));
    println!("model uid: {}", fmt_err(get_string(d, kAudioDevicePropertyModelUID, g)));
    println!("alive: {}", fmt_err(get::<u32>(d, kAudioDevicePropertyDeviceIsAlive, g)));
    println!(
        "running somewhere: {}",
        fmt_err(get::<u32>(d, kAudioDevicePropertyDeviceIsRunningSomewhere, g))
    );
    println!("hidden: {}", fmt_err(get::<u32>(d, kAudioDevicePropertyIsHidden, g)));
    println!("transport: {}", fmt_fourcc(get::<u32>(d, kAudioDevicePropertyTransportType, g)));
    println!("clock domain: {}", fmt_err(get::<u32>(d, kAudioDevicePropertyClockDomain, g)));
    println!(
        "clock algorithm: {}",
        fmt_fourcc(get::<u32>(d, kAudioDevicePropertyClockAlgorithm, g))
    );
    println!(
        "zero timestamp period: {}",
        fmt_err(get::<u32>(d, kAudioDevicePropertyZeroTimeStampPeriod, g))
    );
    println!("nominal rate: {}", fmt_err(get::<f64>(d, kAudioDevicePropertyNominalSampleRate, g)));
    println!("actual rate: {}", fmt_err(get::<f64>(d, kAudioDevicePropertyActualSampleRate, g)));
    let rates: Vec<String> =
        available_rates(d).iter().map(|r| format!("{}-{}", r.mMinimum, r.mMaximum)).collect();
    println!("available rates: {}", rates.join(", "));
    println!(
        "buffer frame size: {}",
        fmt_err(get::<u32>(d, kAudioDevicePropertyBufferFrameSize, g))
    );
    println!(
        "buffer frame size range: {}",
        fmt_err(get::<AudioValueRange>(d, kAudioDevicePropertyBufferFrameSizeRange, g))
    );
    for (label, scope) in
        [("input", kAudioObjectPropertyScopeInput), ("output", kAudioObjectPropertyScopeOutput)]
    {
        println!("{label} channels per buffer: {}", fmt_err(stream_channels(d, scope)));
        println!("{label} latency: {}", fmt_err(get::<u32>(d, kAudioDevicePropertyLatency, scope)));
        println!(
            "{label} safety offset: {}",
            fmt_err(get::<u32>(d, kAudioDevicePropertySafetyOffset, scope))
        );
        for s in streams(d, scope) {
            let f = get::<AudioStreamBasicDescription>(s, kAudioStreamPropertyVirtualFormat, g);
            println!(
                "{label} stream {s}: latency {}, format {}",
                fmt_err(get::<u32>(s, kAudioDevicePropertyLatency, g)),
                fmt_err(f)
            );
        }
    }
}

pub fn wait(uid: &str, timeout: Duration) -> i32 {
    let start = Instant::now();
    loop {
        if let Some(d) = device_by_uid(uid) {
            println!("device {uid:?} is id {d} after {:.1} s", start.elapsed().as_secs_f64());
            return 0;
        }
        if start.elapsed() > timeout {
            eprintln!("device {uid:?} did not appear within {} s", timeout.as_secs());
            list();
            return 1;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn scope_of(a: &Args) -> Result<u32, String> {
    match a.str("--scope").unwrap_or("glob") {
        "glob" => Ok(kAudioObjectPropertyScopeGlobal),
        "inpt" => Ok(kAudioObjectPropertyScopeInput),
        "outp" => Ok(kAudioObjectPropertyScopeOutput),
        s => Err(format!("bad --scope {s:?} (glob, inpt or outp)")),
    }
}

/// `prop <uid> <fourcc> [--scope s] [--element n] [--type t]`
pub fn prop(a: &Args) -> Result<i32, String> {
    a.only(&["--scope", "--element", "--type"])?;
    let (Some(uid), Some(sel)) = (a.positional.get(1), a.positional.get(2)) else {
        return Err("prop needs <uid> <fourcc>".into());
    };
    let d = device(uid)?;
    let selector = parse_fourcc(sel).ok_or_else(|| format!("bad fourcc {sel:?}"))?;
    let scope = scope_of(a)?;
    let element = a.get::<u32>("--element")?.unwrap_or(0);
    let value = match a.str("--type").unwrap_or("u32s") {
        "u32" => get_raw_el(d, selector, scope, element, 4).map(|b| {
            if b.len() >= 4 {
                u32::from_ne_bytes([b[0], b[1], b[2], b[3]]).to_string()
            } else {
                format!("{b:?}")
            }
        }),
        "f64" => get_raw_el(d, selector, scope, element, 8).map(|b| {
            if b.len() >= 8 {
                let mut w = [0u8; 8];
                w.copy_from_slice(&b[..8]);
                f64::from_ne_bytes(w).to_string()
            } else {
                format!("{b:?}")
            }
        }),
        "cfstring" => get_string_el(d, selector, scope, element),
        "u32s" => size_el(d, selector, scope, element)
            .and_then(|n| get_raw_el(d, selector, scope, element, n))
            .map(|b| {
                let v: Vec<u32> =
                    b.as_chunks::<4>().0.iter().map(|c| u32::from_ne_bytes(*c)).collect();
                format!("{v:?}")
            }),
        t => return Err(format!("bad --type {t:?} (u32, f64, cfstring or u32s)")),
    };
    match value {
        Ok(v) => {
            println!("{v}");
            Ok(0)
        }
        Err(s) => {
            eprintln!("'{}': error {}", fourcc_str(selector), status_str(s));
            Ok(1)
        }
    }
}

/// The OpenVirtualSoundcard status property ('ovst'), or an error string.
pub fn status_string(d: AudioObjectID) -> String {
    get_string_el(d, fourcc(b"ovst"), kAudioObjectPropertyScopeGlobal, 0)
        .unwrap_or_else(|s| format!("error {}", status_str(s)))
}

/// `wait-status <uid> <substring> [secs]`
pub fn wait_status(a: &Args) -> Result<i32, String> {
    a.only(&[])?;
    let (Some(uid), Some(want)) = (a.positional.get(1), a.positional.get(2)) else {
        return Err("wait-status needs <uid> <substring>".into());
    };
    let secs = a.pos::<u64>(3, "seconds")?.unwrap_or(30);
    let start = Instant::now();
    let mut last = String::new();
    loop {
        if let Some(d) = device_by_uid(uid) {
            last = status_string(d);
            if last.contains(want.as_str()) {
                println!(
                    "status has {want:?} after {:.1} s: {last}",
                    start.elapsed().as_secs_f64()
                );
                return Ok(0);
            }
        }
        if start.elapsed() > Duration::from_secs(secs) {
            eprintln!("status did not show {want:?} within {secs} s; last: {last}");
            return Ok(1);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// `wait-rate <uid> <hz> [secs]`
pub fn wait_rate(a: &Args) -> Result<i32, String> {
    a.only(&[])?;
    let uid = a.positional.get(1).ok_or("wait-rate needs <uid> <hz>")?;
    let hz = a.pos::<f64>(2, "rate")?.ok_or("wait-rate needs <uid> <hz>")?;
    let secs = a.pos::<u64>(3, "seconds")?.unwrap_or(30);
    let start = Instant::now();
    let mut last = 0.0;
    loop {
        if let Some(d) = device_by_uid(uid) {
            last = get::<f64>(
                d,
                kAudioDevicePropertyNominalSampleRate,
                kAudioObjectPropertyScopeGlobal,
            )
            .unwrap_or(0.0);
            if last == hz {
                println!("rate is {hz} after {:.1} s", start.elapsed().as_secs_f64());
                return Ok(0);
            }
        }
        if start.elapsed() > Duration::from_secs(secs) {
            eprintln!("rate did not become {hz} within {secs} s; last {last}");
            return Ok(1);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// `check <uid> [expectations]`: prints the properties a host relies on and
/// exits 1 if any differs from what was asked for.
pub fn check(a: &Args) -> Result<i32, String> {
    a.only(&[
        "--rate",
        "--inputs",
        "--outputs",
        "--zts-period",
        "--input-safety",
        "--output-safety",
        "--input-latency",
        "--output-latency",
        "--clock-algorithm",
        "--element-names",
    ])?;
    let uid = a.positional.get(1).ok_or("check needs <uid>")?;
    let d = device(uid)?;
    let g = kAudioObjectPropertyScopeGlobal;
    let (inp, outp) = (kAudioObjectPropertyScopeInput, kAudioObjectPropertyScopeOutput);
    let mut failures: Vec<String> = Vec::new();
    let mut expect = |what: &str, got: Result<String, OSStatus>, want: Option<String>| {
        let shown = match &got {
            Ok(v) => v.clone(),
            Err(s) => format!("error {}", status_str(*s)),
        };
        match want {
            Some(w) if got.as_ref().ok() != Some(&w) => {
                println!("{what}: {shown}  (MISMATCH: want {w})");
                failures.push(format!("{what}: got {shown}, want {w}"));
            }
            Some(_) => println!("{what}: {shown}  (ok)"),
            None => println!("{what}: {shown}"),
        }
    };
    let num = |r: Result<u32, OSStatus>| r.map(|v| v.to_string());
    let chans = |scope| stream_channels(d, scope).map(|v| v.iter().sum::<u32>().to_string());

    expect(
        "nominal rate",
        get::<f64>(d, kAudioDevicePropertyNominalSampleRate, g).map(|v| format!("{v}")),
        a.get::<f64>("--rate")?.map(|v| format!("{v}")),
    );
    expect(
        "actual rate",
        get::<f64>(d, kAudioDevicePropertyActualSampleRate, g).map(|v| format!("{v}")),
        None,
    );
    expect("alive", num(get(d, kAudioDevicePropertyDeviceIsAlive, g)), Some("1".into()));
    expect(
        "buffer frame size range",
        get::<AudioValueRange>(d, kAudioDevicePropertyBufferFrameSizeRange, g)
            .map(|r| format!("{}-{}", r.mMinimum, r.mMaximum)),
        None,
    );
    expect("input channels", chans(inp), a.get::<u32>("--inputs")?.map(|v| v.to_string()));
    expect("output channels", chans(outp), a.get::<u32>("--outputs")?.map(|v| v.to_string()));
    expect(
        "zero timestamp period",
        num(get(d, kAudioDevicePropertyZeroTimeStampPeriod, g)),
        a.get::<u32>("--zts-period")?.map(|v| v.to_string()),
    );
    expect(
        "input safety offset",
        num(get(d, kAudioDevicePropertySafetyOffset, inp)),
        a.get::<u32>("--input-safety")?.map(|v| v.to_string()),
    );
    expect(
        "output safety offset",
        num(get(d, kAudioDevicePropertySafetyOffset, outp)),
        a.get::<u32>("--output-safety")?.map(|v| v.to_string()),
    );
    expect(
        "input latency",
        num(get(d, kAudioDevicePropertyLatency, inp)),
        a.get::<u32>("--input-latency")?.map(|v| v.to_string()),
    );
    expect(
        "output latency",
        num(get(d, kAudioDevicePropertyLatency, outp)),
        a.get::<u32>("--output-latency")?.map(|v| v.to_string()),
    );
    expect(
        "clock algorithm",
        get::<u32>(d, kAudioDevicePropertyClockAlgorithm, g).map(fourcc_str),
        a.str("--clock-algorithm").map(String::from),
    );
    let names: Option<Vec<String>> =
        a.str("--element-names").map(|s| s.split(',').map(String::from).collect());
    for (label, scope) in [("input", inp), ("output", outp)] {
        let n: u32 = stream_channels(d, scope).unwrap_or_default().iter().sum();
        for e in 1..=n {
            let want = names.as_ref().map(|v| v.get(e as usize - 1).cloned().unwrap_or_default());
            expect(
                &format!("{label} element {e} name"),
                get_string_el(d, kAudioObjectPropertyElementName, scope, e),
                want,
            );
        }
    }
    if failures.is_empty() {
        println!("CHECK: PASS");
        Ok(0)
    } else {
        println!("CHECK: FAIL ({} mismatches)", failures.len());
        Ok(1)
    }
}

/// Selectors `walk` tries on every object, in every scope.
const WALK_SELECTORS: &[u32] = &[
    kAudioObjectPropertyBaseClass,
    kAudioObjectPropertyClass,
    kAudioObjectPropertyOwner,
    kAudioObjectPropertyName,
    kAudioObjectPropertyManufacturer,
    kAudioObjectPropertyOwnedObjects,
    kAudioObjectPropertyControlList,
    kAudioObjectPropertyCustomPropertyInfoList,
    kAudioDevicePropertyDeviceUID,
    kAudioDevicePropertyModelUID,
    kAudioDevicePropertyTransportType,
    kAudioDevicePropertyRelatedDevices,
    kAudioDevicePropertyClockDomain,
    kAudioDevicePropertyDeviceIsAlive,
    kAudioDevicePropertyDeviceIsRunning,
    kAudioDevicePropertyDeviceIsRunningSomewhere,
    kAudioDevicePropertyIsHidden,
    kAudioDevicePropertyNominalSampleRate,
    kAudioDevicePropertyAvailableNominalSampleRates,
    kAudioDevicePropertyActualSampleRate,
    kAudioDevicePropertyZeroTimeStampPeriod,
    kAudioDevicePropertyClockAlgorithm,
    kAudioDevicePropertyClockIsStable,
    kAudioDevicePropertyBufferFrameSize,
    kAudioDevicePropertyBufferFrameSizeRange,
    kAudioDevicePropertyLatency,
    kAudioDevicePropertySafetyOffset,
    kAudioDevicePropertyStreams,
    kAudioDevicePropertyStreamConfiguration,
    kAudioDevicePropertyPreferredChannelsForStereo,
    kAudioDevicePropertyPreferredChannelLayout,
    kAudioDevicePropertyDeviceCanBeDefaultDevice,
    kAudioDevicePropertyDeviceCanBeDefaultSystemDevice,
    kAudioStreamPropertyVirtualFormat,
    kAudioStreamPropertyPhysicalFormat,
    kAudioStreamPropertyAvailableVirtualFormats,
    kAudioStreamPropertyAvailablePhysicalFormats,
    kAudioStreamPropertyDirection,
    kAudioStreamPropertyTerminalType,
    kAudioStreamPropertyStartingChannel,
    kAudioStreamPropertyIsActive,
    // OpenVirtualSoundcard's status string.
    fourcc(b"ovst"),
];

/// Selectors whose value is a +1 CF object the caller must release.
const CF_SELECTORS: &[u32] = &[
    kAudioObjectPropertyName,
    kAudioObjectPropertyManufacturer,
    kAudioDevicePropertyDeviceUID,
    kAudioDevicePropertyModelUID,
    kAudioObjectPropertyElementName,
    fourcc(b"ovst"),
];

/// `walk <uid>`: for every object the device owns and every selector it
/// claims to have, the size and the data must be readable and agree.
pub fn walk(a: &Args) -> Result<i32, String> {
    a.only(&[])?;
    let uid = a.positional.get(1).ok_or("walk needs <uid>")?;
    let d = device(uid)?;
    let mut objects = vec![d];
    let mut i = 0;
    while i < objects.len() {
        for o in owned_objects(objects[i]) {
            if !objects.contains(&o) {
                objects.push(o);
            }
        }
        i += 1;
    }
    let scopes = [
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyScopeInput,
        kAudioObjectPropertyScopeOutput,
    ];
    let mut problems = 0;
    let mut checked = 0;
    for &o in &objects {
        let class = get::<u32>(o, kAudioObjectPropertyClass, kAudioObjectPropertyScopeGlobal)
            .map(fourcc_str)
            .unwrap_or_default();
        let mut present = Vec::new();
        for &scope in &scopes {
            let mut targets: Vec<(u32, u32)> = WALK_SELECTORS.iter().map(|&s| (s, 0)).collect();
            let n: u32 = stream_channels(o, scope).unwrap_or_default().iter().sum();
            if o == d && scope != kAudioObjectPropertyScopeGlobal {
                targets.extend((1..=n).map(|e| (kAudioObjectPropertyElementName, e)));
            }
            for (sel, element) in targets {
                if !has_el(o, sel, scope, element) {
                    continue;
                }
                checked += 1;
                let what = format!(
                    "object {o} ('{class}') '{}' scope '{}' element {element}",
                    fourcc_str(sel),
                    fourcc_str(scope)
                );
                let size = match size_el(o, sel, scope, element) {
                    Ok(s) => s,
                    Err(s) => {
                        println!(
                            "PROBLEM {what}: has the property but size fails: {}",
                            status_str(s)
                        );
                        problems += 1;
                        continue;
                    }
                };
                match get_raw_el(o, sel, scope, element, size) {
                    Ok(bytes) if bytes.len() as u32 > size => {
                        println!(
                            "PROBLEM {what}: wrote {} bytes, more than its size {size}",
                            bytes.len()
                        );
                        problems += 1;
                    }
                    Ok(bytes) => {
                        let cf = CF_SELECTORS.contains(&sel);
                        if cf && bytes.len() >= std::mem::size_of::<usize>() {
                            let mut p = [0u8; std::mem::size_of::<usize>()];
                            p.copy_from_slice(&bytes[..std::mem::size_of::<usize>()]);
                            let obj = usize::from_ne_bytes(p) as *const std::ffi::c_void;
                            if !obj.is_null() {
                                unsafe { CFRelease(obj) };
                            }
                        }
                        if scope == kAudioObjectPropertyScopeGlobal || element > 0 {
                            present.push(format!("'{}'", fourcc_str(sel)));
                        }
                    }
                    Err(s) => {
                        println!("PROBLEM {what}: size {size} but data fails: {}", status_str(s));
                        problems += 1;
                    }
                }
            }
        }
        println!("object {o} ('{class}'): {}", present.join(" "));
    }
    println!("walked {} objects, {checked} properties, {problems} problems", objects.len());
    Ok(if problems == 0 { 0 } else { 1 })
}
