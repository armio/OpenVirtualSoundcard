//! Sound-card bridge for OpenVirtualSoundcard.
//!
//! Connects the audio of an OpenVirtualSoundcard device ([`AudioIo`]) to local audio
//! devices through [cpal]: CoreAudio on macOS, WASAPI on Windows, ALSA on
//! Linux. Two independent directions:
//!
//! * **playback**: network receive channels → a device's outputs;
//! * **capture**: a device's inputs → network transmit channels.
//!
//! The device can be real hardware or a virtual loopback device, which is how
//! audio applications get at the network: the bridge plays the network's
//! receive channels into the loopback, the application records them from it,
//! and the application's output comes back through the loopback into the
//! transmit channels.
//!
//! ```no_run
//! use ovsc_soundcard::{Bridge, BridgeConfig, list_devices};
//!
//! # fn run(device: &ovsc_core::Device) -> Result<(), ovsc_soundcard::Error> {
//! for d in list_devices()? {
//!     println!("{} [{}]: {} in / {} out", d.name, d.id, d.max_input_channels, d.max_output_channels);
//! }
//! let bridge = Bridge::start(
//!     device.audio(),
//!     &BridgeConfig {
//!         output_device: Some("BlackHole 16ch".into()),
//!         input_device: Some("BlackHole 16ch".into()),
//!         // Receive channels 1-8 on BlackHole channels 1-8, ...
//!         output_map: Some((0..8).map(Some).collect()),
//!         // ... and BlackHole channels 9-16 into transmit channels 1-8.
//!         input_map: Some([None; 8].into_iter().chain((0..8).map(Some)).collect()),
//!         ..Default::default()
//!     },
//! )?;
//! loop {
//!     std::thread::sleep(std::time::Duration::from_secs(10));
//!     println!("{:?}", bridge.stats());
//! }
//! # }
//! ```
//!
//! Device names match exactly or by unique substring, ignoring case; use the
//! identifier from [`list_devices`] to tell same-named devices apart. Devices
//! open at the network's sample rate (an error explains what the device
//! supports otherwise) with 32-bit float or integer samples where available.
//!
//! # Virtual devices
//!
//! **macOS, [BlackHole](https://github.com/ExistentialAudio/BlackHole).**
//! Install `BlackHole 16ch` (`brew install blackhole-16ch`) and set its rate
//! in *Audio MIDI Setup* to the network's (usually 48 kHz). BlackHole loops
//! each output channel back to the same input channel and mixes all clients,
//! so give the two directions separate channels, as in the example above:
//! the DAW records BlackHole inputs 1-8 and sends to BlackHole outputs 9-16.
//! (Or use two BlackHole devices, one per direction.)
//!
//! **Windows, [VB-Cable](https://vb-audio.com/Cable/).** A cable is one-way:
//! what is played to `CABLE Input` comes out of `CABLE Output`. Use
//! `output_device: "CABLE Input"` and record `CABLE Output` in the
//! application; for the return path, a second cable (e.g. VB-Cable A+B:
//! the application plays to `CABLE-A Input`, the bridge captures
//! `CABLE-A Output`). In shared mode WASAPI only offers each device's
//! configured format, so set the cables (*Sound settings → Advanced*, and
//! the VB-Cable control panel) to the network's sample rate.
//!
//! **Linux, `snd-aloop`.** `sudo modprobe snd-aloop` creates the `Loopback`
//! card: what is played to device 0, subdevice *n* can be captured from
//! device 1, subdevice *n*, and the other way round. Bridge to
//! `plughw:CARD=Loopback,DEV=0` in both directions and point the application
//! at `plughw:CARD=Loopback,DEV=1` (or the matching `hw:` names). The first
//! side to open a subdevice fixes its rate, so start the bridge first.
//! Real interfaces work the same way under their ALSA names.
//!
//! # Clocks and drift
//!
//! The network's rings are indexed by PTP media time, but a sound card
//! consumes and produces samples at the rate of its own crystal (virtual
//! devices: the host's clock). The two differ by tens of ppm, so each stream
//! keeps a fractional position on the media timeline, advances it by a
//! slowly steered ratio per device frame, and resamples accordingly:
//!
//! * [`controller`]: a gear-shifted PI phase-locked loop compares the
//!   stream's position with where it should be (measured against the media
//!   clock at every callback, jitter filtered out) and steers the ratio,
//!   within ±1000 ppm. It locks in a few seconds and then holds the ratio
//!   within a ppm or two of the true clock ratio, an inaudible pitch
//!   modulation. Start-up, stalls and clock steps realign the stream after a
//!   short mute instead of slewing for minutes.
//! * The resampler is a 64-tap Kaiser-windowed sinc interpolator with 1024
//!   tabulated phases, accurate to better than −100 dB up to 20 kHz at
//!   48 kHz; it reads the rings (or a short capture history) at arbitrary
//!   fractional positions, so the ratio can change at every sample.
//!
//! Audio callbacks don't allocate, lock or log; statistics are atomics read
//! by [`Bridge::stats`].
//!
//! # Latency
//!
//! Playback reads the rings `latency + safety` behind the media clock, where
//! `safety` is one device buffer, the interpolator's 32-sample look-ahead and
//! the margin (1 ms by default, [`BridgeConfig::margin_ms`]); the device's
//! own output latency comes on top. Capture stamps the newest captured frame
//! `safety` ahead of the media clock. Small device buffers keep both short.
//!
//! # Limitations
//!
//! * Absolute timing is not compensated for the devices' own converter and
//!   driver latencies, so playback is not sample-aligned with other Dante
//!   receivers.
//! * A device disconnect ends its stream ([`DirectionStats::device_lost`]);
//!   start a new bridge to reconnect.

mod bridge;
pub mod controller;
mod device;
mod kernel;
mod pos;
mod sample;
mod stats;
mod stream;

pub use bridge::{Bridge, BridgeConfig, BridgeStats, DEFAULT_MARGIN_MS};
pub use device::{DeviceDescription, Direction, list_devices};
pub use stats::DirectionStats;

#[doc(no_inline)]
pub use ovsc_core::AudioIo;

/// Errors of the sound-card bridge.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("audio system unavailable: {0}")]
    NoHost(String),
    #[error("no {direction} device matches {query:?}; available {direction} devices: {}", list(.available))]
    DeviceNotFound { direction: Direction, query: String, available: Vec<String> },
    #[error(
        "{query:?} matches several {direction} devices ({}); use the full name or the device id",
        .matches.join(", ")
    )]
    AmbiguousDevice { direction: Direction, query: String, matches: Vec<String> },
    #[error("{device} {message}")]
    Unsupported { device: String, message: String },
    #[error("{0}")]
    Config(String),
    #[error("{context}: {source}")]
    Audio {
        context: String,
        #[source]
        source: cpal::Error,
    },
}

impl Error {
    pub(crate) fn audio(context: impl Into<String>, source: cpal::Error) -> Self {
        Error::Audio { context: context.into(), source }
    }
}

fn list(items: &[String]) -> String {
    if items.is_empty() { "none".into() } else { items.join(", ") }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_list_the_alternatives() {
        let e = Error::DeviceNotFound {
            direction: Direction::Output,
            query: "BlackHole".into(),
            available: vec!["Speakers".into(), "CABLE Input".into()],
        };
        assert_eq!(
            e.to_string(),
            "no output device matches \"BlackHole\"; available output devices: Speakers, CABLE Input"
        );
        let e = Error::DeviceNotFound {
            direction: Direction::Input,
            query: "x".into(),
            available: vec![],
        };
        assert!(e.to_string().ends_with("available input devices: none"));
    }

    #[test]
    fn bridge_can_move_between_threads() {
        fn assert_send<T: Send>() {}
        assert_send::<Bridge>();
    }

    #[test]
    fn starting_without_devices_is_a_config_error() {
        let (clock, _writer) = ovsc_clock::MediaClock::new();
        let io = AudioIo {
            clock,
            sample_rate: 48_000,
            format: ovsc_proto::audio::SampleFormat::S24,
            latency_samples: 48,
            rx: vec![],
            tx: vec![],
            rx_names: vec![],
            tx_names: vec![],
        };
        assert!(matches!(
            Bridge::start(io.clone(), &BridgeConfig::default()),
            Err(Error::Config(_))
        ));
        let cfg = BridgeConfig {
            output_device: Some("x".into()),
            margin_ms: Some(0.0),
            ..Default::default()
        };
        assert!(matches!(Bridge::start(io, &cfg), Err(Error::Config(_))));
    }

    /// Plays a test tone on the default output device for five seconds.
    /// Needs sound hardware: `cargo test -p ovsc-soundcard -- --ignored`.
    #[test]
    #[ignore]
    fn plays_on_default_device() {
        use std::sync::Arc;
        let clock = ovsc_clock::system_clock();
        let ring = Arc::new(ovsc_core::buffer::TimedRing::new(1 << 15));
        let io = AudioIo {
            clock: clock.clone(),
            sample_rate: 48_000,
            format: ovsc_proto::audio::SampleFormat::S24,
            latency_samples: 96,
            rx: vec![ring.clone()],
            tx: vec![],
            rx_names: vec!["tone".into()],
            tx_names: vec![],
        };
        let cfg = BridgeConfig { output_device: Some("default".into()), ..Default::default() };
        let bridge = Bridge::start(io, &cfg).expect("start");
        let mut next = clock.now_samples(48_000).unwrap();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < end {
            let now = clock.now_samples(48_000).unwrap();
            while next < now {
                let phase = (next % 48_000) as f64 / 48_000.0 * 440.0 * std::f64::consts::TAU;
                ring.write_one(next, (phase.sin() * 0.2 * i32::MAX as f64) as i32);
                next += 1;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let stats = bridge.stats().output.unwrap();
        println!("{stats:?}");
        assert!(stats.running && stats.callbacks > 0);
    }
}
