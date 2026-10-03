//! Finding audio devices and choosing how to open them.

use std::fmt;

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{BufferSize, SampleFormat, SupportedBufferSize, SupportedStreamConfigRange};

use crate::{Error, Result};

/// Sample rates Dante devices run at.
const DANTE_RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

/// A direction of audio flow, from the device's point of view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// The device plays audio (network receive channels).
    Output,
    /// The device captures audio (network transmit channels).
    Input,
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Direction::Output => "output",
            Direction::Input => "input",
        })
    }
}

/// An audio device, as reported by [`list_devices`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceDescription {
    /// Human-readable name, e.g. `"BlackHole 16ch"`.
    pub name: String,
    /// The audio backend's identifier for the device (ALSA PCM name such as
    /// `hw:CARD=Loopback,DEV=0`, CoreAudio UID, WASAPI endpoint ID). Several
    /// ALSA devices can share a name; the identifier is unique.
    pub id: String,
    pub max_input_channels: u16,
    pub max_output_channels: u16,
    /// The device's preferred sample rate, if it reports one.
    pub default_sample_rate: Option<u32>,
    /// The Dante sample rates (44.1 to 192 kHz) the device supports.
    pub sample_rates: Vec<u32>,
    pub is_default_input: bool,
    pub is_default_output: bool,
}

/// Lists the audio devices of the platform's default audio host (CoreAudio,
/// WASAPI or ALSA), with at least one usable input or output channel.
///
/// Never panics: on a machine without sound hardware this returns an empty
/// list (or an error if the audio system itself is unavailable).
pub fn list_devices() -> Result<Vec<DeviceDescription>> {
    std::panic::catch_unwind(list_devices_inner)
        .unwrap_or_else(|_| Err(Error::NoHost("the audio backend panicked".into())))
}

fn list_devices_inner() -> Result<Vec<DeviceDescription>> {
    let host = open_host()?;
    let default_out = host.default_output_device().and_then(|d| device_id(&d));
    let default_in = host.default_input_device().and_then(|d| device_id(&d));
    let devices = host.devices().map_err(|e| Error::audio("cannot enumerate audio devices", e))?;
    let mut out = Vec::new();
    for device in devices {
        let Some(name) = device_name(&device) else { continue };
        let id = device_id(&device).unwrap_or_else(|| name.clone());
        let outputs: Vec<_> = if device.supports_output() {
            device.supported_output_configs().map(Iterator::collect).unwrap_or_default()
        } else {
            Vec::new()
        };
        let inputs: Vec<_> = if device.supports_input() {
            device.supported_input_configs().map(Iterator::collect).unwrap_or_default()
        } else {
            Vec::new()
        };
        if outputs.is_empty() && inputs.is_empty() {
            continue;
        }
        let max_channels = |ranges: &[SupportedStreamConfigRange]| {
            ranges.iter().map(|r| r.channels()).max().unwrap_or(0)
        };
        let default_sample_rate = device
            .default_output_config()
            .or_else(|_| device.default_input_config())
            .ok()
            .map(|c| c.sample_rate());
        let sample_rates = DANTE_RATES
            .into_iter()
            .filter(|&rate| outputs.iter().chain(&inputs).any(|r| r.contains_rate(rate)))
            .collect();
        out.push(DeviceDescription {
            is_default_input: default_in.as_deref() == Some(id.as_str()) && !inputs.is_empty(),
            is_default_output: default_out.as_deref() == Some(id.as_str()) && !outputs.is_empty(),
            name,
            id,
            max_input_channels: max_channels(&inputs),
            max_output_channels: max_channels(&outputs),
            default_sample_rate,
            sample_rates,
        });
    }
    Ok(out)
}

/// The platform's default audio host. Avoids `cpal::default_host()`, which
/// panics when the host cannot be initialised.
pub(crate) fn open_host() -> Result<cpal::Host> {
    let mut last_error = None;
    for id in cpal::available_hosts() {
        match cpal::host_from_id(id) {
            Ok(host) => return Ok(host),
            Err(e) => last_error = Some(format!("{}: {e}", id.name())),
        }
    }
    Err(Error::NoHost(last_error.unwrap_or_else(|| "no audio host on this platform".into())))
}

fn device_name(device: &cpal::Device) -> Option<String> {
    device.description().ok().map(|d| d.name().to_owned())
}

fn device_id(device: &cpal::Device) -> Option<String> {
    device.id().ok().map(|id| id.id().to_owned())
}

/// Finds the device `query` names for `direction`: an exact (case-
/// insensitive) match on the identifier or name, else a unique substring
/// match. `"default"` also selects the host's default device.
pub(crate) fn find_device(
    host: &cpal::Host,
    query: &str,
    direction: Direction,
) -> Result<(cpal::Device, String)> {
    let devices = host
        .devices()
        .map_err(|e| Error::audio("cannot enumerate audio devices", e))?
        .filter(|d| match direction {
            Direction::Output => d.supports_output(),
            Direction::Input => d.supports_input(),
        })
        .filter_map(|d| Some((device_name(&d)?, d)))
        .collect::<Vec<_>>();
    let candidates: Vec<Candidate> = devices
        .iter()
        .map(|(name, d)| Candidate { name: name.clone(), id: device_id(d).unwrap_or_default() })
        .collect();
    match pick(query, &candidates) {
        Ok(i) => {
            let (name, device) = devices.into_iter().nth(i).expect("index from pick");
            Ok((device, name))
        }
        Err(Pick::NotFound) if query.eq_ignore_ascii_case("default") => {
            let device = match direction {
                Direction::Output => host.default_output_device(),
                Direction::Input => host.default_input_device(),
            };
            device.and_then(|d| device_name(&d).map(|name| (d, name))).ok_or_else(|| {
                Error::DeviceNotFound {
                    direction,
                    query: query.to_owned(),
                    available: describe(&candidates),
                }
            })
        }
        Err(Pick::NotFound) => Err(Error::DeviceNotFound {
            direction,
            query: query.to_owned(),
            available: describe(&candidates),
        }),
        Err(Pick::Ambiguous(matches)) => {
            Err(Error::AmbiguousDevice { direction, query: query.to_owned(), matches })
        }
    }
}

fn describe(candidates: &[Candidate]) -> Vec<String> {
    candidates
        .iter()
        .map(|c| {
            if c.id.is_empty() || c.id == c.name {
                c.name.clone()
            } else {
                format!("{} [{}]", c.name, c.id)
            }
        })
        .collect()
}

#[derive(Debug)]
struct Candidate {
    name: String,
    id: String,
}

#[derive(Debug, PartialEq)]
enum Pick {
    NotFound,
    Ambiguous(Vec<String>),
}

fn pick(query: &str, candidates: &[Candidate]) -> std::result::Result<usize, Pick> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Err(Pick::NotFound);
    }
    let lower = |s: &str| s.to_lowercase();
    if let Some(i) = candidates.iter().position(|c| !c.id.is_empty() && lower(&c.id) == q) {
        return Ok(i);
    }
    // Several ALSA PCMs of one card share a name: the first one wins.
    if let Some(i) = candidates.iter().position(|c| lower(&c.name) == q) {
        return Ok(i);
    }
    let matches: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| lower(&c.name).contains(&q) || lower(&c.id).contains(&q))
        .map(|(i, _)| i)
        .collect();
    let Some(&first) = matches.first() else { return Err(Pick::NotFound) };
    let mut names: Vec<String> = Vec::new();
    for &i in &matches {
        if !names.contains(&candidates[i].name) {
            names.push(candidates[i].name.clone());
        }
    }
    if names.len() == 1 { Ok(first) } else { Err(Pick::Ambiguous(names)) }
}

/// How to open a device stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StreamChoice {
    pub channels: u16,
    pub format: SampleFormat,
    pub buffer: BufferSize,
}

/// Preference among the sample formats the bridge converts to and from.
fn format_rank(format: SampleFormat) -> Option<u8> {
    match format {
        SampleFormat::F32 => Some(0),
        SampleFormat::I32 => Some(1),
        SampleFormat::I24 => Some(2),
        SampleFormat::I16 => Some(3),
        _ => None,
    }
}

/// Picks a stream configuration at `rate` among `ranges`: preferably
/// `wanted` channels (else the next larger count, else the largest), never
/// fewer than `required`; the best sample format for that channel count;
/// and `buffer_frames` clamped to what the device supports.
pub(crate) fn choose_config(
    ranges: &[SupportedStreamConfigRange],
    rate: u32,
    wanted: usize,
    required: usize,
    buffer_frames: Option<u32>,
) -> std::result::Result<StreamChoice, String> {
    let at_rate: Vec<_> = ranges.iter().filter(|r| r.contains_rate(rate)).collect();
    if at_rate.is_empty() {
        let mut rates: Vec<String> = ranges
            .iter()
            .map(|r| match (r.min_sample_rate(), r.max_sample_rate()) {
                (a, b) if a == b => format!("{a}"),
                (a, b) => format!("{a}-{b}"),
            })
            .collect();
        rates.sort();
        rates.dedup();
        return Err(format!(
            "does not support {rate} Hz (supported: {} Hz); set the device to {rate} Hz or run \
             the network at a rate the device supports",
            rates.join(", ")
        ));
    }
    let usable: Vec<_> =
        at_rate.iter().copied().filter(|r| format_rank(r.sample_format()).is_some()).collect();
    if usable.is_empty() {
        let mut formats: Vec<String> =
            at_rate.iter().map(|r| r.sample_format().to_string()).collect();
        formats.dedup();
        return Err(format!(
            "offers only {} at {rate} Hz; the bridge needs f32, i32, i24 or i16",
            formats.join(", ")
        ));
    }
    let mut counts: Vec<u16> = usable.iter().map(|r| r.channels()).collect();
    counts.sort_unstable();
    counts.dedup();
    let max = *counts.last().expect("usable is not empty");
    if (max as usize) < required {
        return Err(format!("has {max} channels at {rate} Hz, the channel map needs {required}"));
    }
    let channels = counts
        .iter()
        .copied()
        .find(|&c| c as usize == wanted)
        .or_else(|| counts.iter().copied().find(|&c| c as usize > wanted))
        .unwrap_or(max);
    let best = usable
        .iter()
        .filter(|r| r.channels() == channels)
        .min_by_key(|r| format_rank(r.sample_format()))
        .expect("channel count comes from usable");
    let buffer = match (buffer_frames, best.buffer_size()) {
        (None, _) => BufferSize::Default,
        (Some(n), SupportedBufferSize::Range { min, max }) => {
            BufferSize::Fixed(n.clamp(*min, *max))
        }
        (Some(n), SupportedBufferSize::Unknown) => BufferSize::Fixed(n),
    };
    Ok(StreamChoice { channels, format: best.sample_format(), buffer })
}

/// Resolves a channel map into "network channel for each device channel".
/// `map[i]` is the network channel (0-based) of device channel `i`, `None`
/// leaves it unused; without a map, device channel `i` gets network channel
/// `i` while both exist. With `unique`, no network channel may appear twice
/// (capture: two device channels cannot feed one transmit channel).
pub(crate) fn resolve_map(
    map: Option<&[Option<usize>]>,
    device_channels: usize,
    network_channels: usize,
    unique: bool,
    name: &str,
) -> std::result::Result<Vec<Option<usize>>, String> {
    let resolved: Vec<Option<usize>> = match map {
        Some(map) => {
            if map.len() > device_channels {
                return Err(format!(
                    "{name} has {} entries but the device has {device_channels} channels",
                    map.len()
                ));
            }
            for (i, ch) in map.iter().enumerate() {
                let Some(ch) = *ch else { continue };
                if ch >= network_channels {
                    return Err(format!(
                        "{name}[{i}] = {ch}, but there are only {network_channels} network \
                         channels (numbered from 0)"
                    ));
                }
                if unique && map[..i].contains(&Some(ch)) {
                    return Err(format!("{name} uses network channel {ch} twice"));
                }
            }
            (0..device_channels).map(|i| map.get(i).copied().flatten()).collect()
        }
        None => (0..device_channels).map(|i| (i < network_channels).then_some(i)).collect(),
    };
    if resolved.iter().all(Option::is_none) {
        return Err(format!("{name}: no channel to bridge"));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates(list: &[(&str, &str)]) -> Vec<Candidate> {
        list.iter().map(|(n, i)| Candidate { name: n.to_string(), id: i.to_string() }).collect()
    }

    #[test]
    fn device_names_match_exactly_then_by_unique_substring() {
        let c = candidates(&[
            ("MacBook Pro Speakers", "BuiltInSpeakerDevice"),
            ("BlackHole 2ch", "BlackHole2ch_UID"),
            ("BlackHole 16ch", "BlackHole16ch_UID"),
            ("Loopback, Loopback PCM", "hw:CARD=Loopback,DEV=0"),
            ("Loopback, Loopback PCM", "plughw:CARD=Loopback,DEV=0"),
        ]);
        assert_eq!(pick("blackhole 16CH", &c), Ok(2));
        assert_eq!(pick("16ch", &c), Ok(2));
        assert_eq!(pick("speakers", &c), Ok(0));
        assert_eq!(pick("blackhole2ch_uid", &c), Ok(1));
        assert_eq!(pick("plughw:CARD=Loopback,DEV=0", &c), Ok(4));
        // Same-named ALSA PCMs are not ambiguous: the first is taken.
        assert_eq!(pick("loopback", &c), Ok(3));
        assert_eq!(
            pick("blackhole", &c),
            Err(Pick::Ambiguous(vec!["BlackHole 2ch".into(), "BlackHole 16ch".into()]))
        );
        assert_eq!(pick("VB-Cable", &c), Err(Pick::NotFound));
        assert_eq!(pick("  ", &c), Err(Pick::NotFound));
        assert_eq!(
            describe(&c[..2]),
            ["MacBook Pro Speakers [BuiltInSpeakerDevice]", "BlackHole 2ch [BlackHole2ch_UID]"]
        );
    }

    fn range(
        channels: u16,
        min: u32,
        max: u32,
        format: SampleFormat,
    ) -> SupportedStreamConfigRange {
        let buffer = SupportedBufferSize::Range { min: 64, max: 4096 };
        SupportedStreamConfigRange::new(channels, min, max, buffer, format)
    }

    #[test]
    fn config_prefers_wanted_channels_and_float() {
        let ranges = [
            range(2, 44_100, 48_000, SampleFormat::I16),
            range(2, 44_100, 48_000, SampleFormat::F32),
            range(8, 44_100, 96_000, SampleFormat::I24),
            range(8, 44_100, 96_000, SampleFormat::I32),
            range(16, 44_100, 96_000, SampleFormat::F32),
        ];
        let c = choose_config(&ranges, 48_000, 2, 1, None).unwrap();
        assert_eq!(
            c,
            StreamChoice { channels: 2, format: SampleFormat::F32, buffer: BufferSize::Default }
        );
        // No 4-channel config: next larger.
        let c = choose_config(&ranges, 48_000, 4, 1, Some(128)).unwrap();
        assert_eq!(c.channels, 8);
        assert_eq!(c.format, SampleFormat::I32);
        assert_eq!(c.buffer, BufferSize::Fixed(128));
        // More than the device has: the largest.
        let c = choose_config(&ranges, 96_000, 64, 1, Some(10)).unwrap();
        assert_eq!((c.channels, c.buffer), (16, BufferSize::Fixed(64)));
    }

    #[test]
    fn config_errors_explain_the_problem() {
        let ranges = [
            range(2, 44_100, 48_000, SampleFormat::F32),
            range(2, 96_000, 96_000, SampleFormat::U8),
        ];
        let e = choose_config(&ranges, 192_000, 2, 1, None).unwrap_err();
        assert!(e.contains("192000 Hz") && e.contains("44100-48000") && e.contains("96000"), "{e}");
        let e = choose_config(&ranges, 96_000, 2, 1, None).unwrap_err();
        assert!(e.contains("u8"), "{e}");
        let e = choose_config(&ranges, 48_000, 2, 4, None).unwrap_err();
        assert!(e.contains("needs 4"), "{e}");
    }

    #[test]
    fn channel_maps() {
        assert_eq!(resolve_map(None, 4, 2, false, "m"), Ok(vec![Some(0), Some(1), None, None]));
        assert_eq!(resolve_map(None, 2, 8, false, "m"), Ok(vec![Some(0), Some(1)]));
        let dup = [Some(3), Some(3)];
        assert_eq!(
            resolve_map(Some(&dup), 4, 4, false, "m"),
            Ok(vec![Some(3), Some(3), None, None])
        );
        assert!(resolve_map(Some(&dup), 4, 4, true, "m").unwrap_err().contains("twice"));
        // Skipping device channels: capture inputs 3-4 into transmit 1-2.
        let skip = [None, None, Some(0), Some(1)];
        assert_eq!(resolve_map(Some(&skip), 4, 2, true, "m"), Ok(skip.to_vec()));
        let bad = [Some(0), Some(4)];
        assert!(resolve_map(Some(&bad), 4, 4, false, "m").unwrap_err().contains("m[1] = 4"));
        assert!(resolve_map(Some(&[Some(0), Some(1), Some(2)]), 2, 4, false, "m").is_err());
        assert!(resolve_map(Some(&[None, None]), 2, 4, false, "m").is_err());
        assert!(resolve_map(None, 2, 0, false, "m").is_err());
    }

    #[test]
    fn listing_devices_never_panics() {
        // Works with or without sound hardware; on a machine without any it
        // is typically an empty list.
        match list_devices() {
            Ok(devices) => {
                for d in devices {
                    assert!(!d.name.is_empty());
                    assert!(d.max_input_channels > 0 || d.max_output_channels > 0);
                }
            }
            Err(e) => assert!(!e.to_string().is_empty()),
        }
    }
}
