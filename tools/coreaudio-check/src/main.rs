//! coreaudio-check: tests a Core Audio device the way an application uses
//! it. Used by the macOS CI to prove the OpenVirtualSoundcard driver works end to end.
//!
//! ```text
//! coreaudio-check list
//! coreaudio-check info <uid>
//! coreaudio-check wait <uid> [seconds]
//! coreaudio-check prop <uid> <fourcc> [--scope glob|inpt|outp] [--element N]
//!                 [--type u32|f64|cfstring|u32s]
//! coreaudio-check wait-status <uid> <substring> [seconds]
//! coreaudio-check wait-rate <uid> <hz> [seconds]
//! coreaudio-check check <uid> [--rate HZ] [--inputs N] [--outputs N] [--zts-period N]
//!                 [--input-safety N] [--output-safety N] [--input-latency N]
//!                 [--output-latency N] [--clock-algorithm FOURCC] [--element-names a,b,..]
//! coreaudio-check walk <uid>
//! coreaudio-check loopback <uid> [--seconds N] [--rate HZ] [--buffer FRAMES]
//!                 [--channels N] [--max-bad N] [--max-slips N] [--max-jumps N]
//!                 [--expect-device-delay D] [--expect-rate-ppm X] [--rate-tol-ppm T]
//!                 [--allow-outage SECS] [--expect-silent-input] [--snapshot-prop FOURCC]
//! ```
//!
//! `loopback` plays a distinct pseudo-random signal on each output channel
//! and records the input channels, expecting output channel `c` to come back
//! on input channel `c` (for OpenVirtualSoundcard: routed through the network and back).
//! It reports the round-trip delay, both in captured frames and in device
//! time, every frame that did not come back bit for bit, and the device's
//! sample rate measured against the host clock. Frames lost to Core Audio's
//! own IO overloads (timeline jumps) are allowed for, up to `--max-jumps` of
//! them. The last line is `CA-RESULT result=PASS|FAIL key=value ...`.

// Used only by the macOS code outside tests.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod analysis;
#[cfg(target_os = "macos")]
mod ca;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod cli;
#[cfg(target_os = "macos")]
mod commands;
#[cfg(target_os = "macos")]
mod loopback;

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("coreaudio-check only runs on macOS");
    std::process::exit(2);
}

#[cfg(target_os = "macos")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match cli::Args::parse(&args).and_then(|a| dispatch(&a)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("coreaudio-check: {e}");
            2
        }
    };
    std::process::exit(code);
}

#[cfg(target_os = "macos")]
fn dispatch(a: &cli::Args) -> Result<i32, String> {
    use std::time::Duration;
    match a.positional.first().map(String::as_str) {
        Some("list") => {
            a.only(&[])?;
            commands::list();
            Ok(0)
        }
        Some("info") => {
            a.only(&[])?;
            let uid = a.positional.get(1).ok_or("info needs <uid>")?;
            commands::info(commands::device(uid)?);
            Ok(0)
        }
        Some("wait") => {
            a.only(&[])?;
            let uid = a.positional.get(1).ok_or("wait needs <uid>")?;
            let secs = a.pos::<u64>(2, "seconds")?.unwrap_or(30);
            Ok(commands::wait(uid, Duration::from_secs(secs)))
        }
        Some("prop") => commands::prop(a),
        Some("wait-status") => commands::wait_status(a),
        Some("wait-rate") => commands::wait_rate(a),
        Some("check") => commands::check(a),
        Some("walk") => commands::walk(a),
        Some("loopback") => loopback::run(a),
        _ => Err("usage: coreaudio-check list | info | wait | prop | wait-status | wait-rate | \
                  check | walk | loopback (see the source header for options)"
            .into()),
    }
}
