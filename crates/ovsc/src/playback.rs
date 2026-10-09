//! Streaming virtual-soundcheck playback. Files stay on disk; the network
//! and native input path consume timestamped rings with exclusive ownership.
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use ovsc_control::{Marker, PlaybackSettings, PlaybackStatus, PlaybackTarget, Request};
use ovsc_core::{AudioIo, Device, ReceiveOverride, TransmitOverride};

pub struct Wave {
    file: File,
    data_offset: u64,
    pub frames: u64,
    pub rate: u32,
    pub channels: u16,
    bits: u16,
    encoding: u16,
    pub markers: Vec<Marker>,
}
fn u16le(b: &[u8]) -> u16 {
    u16::from_le_bytes(b[..2].try_into().unwrap())
}
fn u32le(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}
impl Wave {
    pub fn open(mut file: File) -> anyhow::Result<Self> {
        let length = file.metadata()?.len();
        let mut header = [0; 12];
        file.read_exact(&mut header).context("reading WAV header")?;
        ensure!(&header[..4] == b"RIFF" && &header[8..] == b"WAVE", "choose a RIFF WAV file");
        let end = 8 + u64::from(u32le(&header[4..]));
        ensure!(end <= length && end >= 12, "WAV is truncated");
        let mut fmt = None;
        let mut data = None;
        let mut markers = Vec::new();
        let mut labels = BTreeMap::new();
        let mut offset = 12;
        let mut chunks = 0;
        let mut metadata_bytes = 0;
        while offset + 8 <= end {
            chunks += 1;
            ensure!(chunks <= 4096, "too many WAV chunks");
            file.seek(SeekFrom::Start(offset))?;
            let mut h = [0; 8];
            file.read_exact(&mut h)?;
            let size = u64::from(u32le(&h[4..]));
            let start = offset + 8;
            ensure!(start + size <= end, "WAV chunk is truncated");
            if &h[..4] != b"data" && matches!(&h[..4], b"fmt " | b"cue " | b"LIST") {
                metadata_bytes += size;
                ensure!(metadata_bytes <= 4 * 1024 * 1024, "WAV metadata is too large");
            }
            match &h[..4] {
                b"fmt " => {
                    ensure!(
                        (16..=4096).contains(&size) && fmt.is_none(),
                        "invalid WAV format chunk"
                    );
                    let mut b = vec![0; size as usize];
                    file.read_exact(&mut b)?;
                    let mut encoding = u16le(&b);
                    let channels = u16le(&b[2..]);
                    let rate = u32le(&b[4..]);
                    let bits = u16le(&b[14..]);
                    if encoding == 0xfffe {
                        ensure!(
                            size >= 40 && u16le(&b[16..]) >= 22,
                            "invalid extensible WAV format"
                        );
                        ensure!(
                            b[26..40]
                                == [0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71],
                            "unsupported WAV subtype"
                        );
                        encoding = u16le(&b[24..]);
                        ensure!(
                            u16le(&b[18..]) > 0 && u16le(&b[18..]) <= bits,
                            "invalid valid-bits count"
                        );
                    }
                    ensure!(
                        channels > 0 && channels <= 256 && rate > 0,
                        "invalid WAV channel count or sample rate"
                    );
                    ensure!(
                        (encoding == 1 && [16, 24, 32].contains(&bits))
                            || (encoding == 3 && bits == 32),
                        "playback supports PCM 16/24/32-bit or Float32 WAV"
                    );
                    ensure!(u16le(&b[12..]) == channels * (bits / 8), "invalid WAV frame size");
                    fmt = Some((encoding, channels, rate, bits));
                }
                b"data" => {
                    ensure!(data.is_none(), "multiple WAV data chunks are not supported");
                    data = Some((start, size));
                }
                b"cue " => {
                    ensure!(markers.is_empty(), "multiple cue chunks are not supported");
                    ensure!((4..=4 + 1024 * 24).contains(&size), "too many WAV markers");
                    let mut b = vec![0; size as usize];
                    file.read_exact(&mut b)?;
                    let count = u32le(&b) as usize;
                    ensure!(count <= 1024 && size as usize == 4 + count * 24, "invalid cue chunk");
                    for entry in b[4..].chunks_exact(24) {
                        ensure!(
                            &entry[8..12] == b"data"
                                && u32le(&entry[12..]) == 0
                                && u32le(&entry[16..]) == 0,
                            "unsupported WAV cue layout"
                        );
                        markers.push(Marker {
                            id: u32le(entry),
                            frame: u64::from(u32le(&entry[20..])),
                            label: String::new(),
                        });
                    }
                }
                b"LIST" if size <= 1024 * 1024 => {
                    let mut b = vec![0; size as usize];
                    file.read_exact(&mut b)?;
                    if b.starts_with(b"adtl") {
                        let mut p = 4;
                        while p + 8 <= b.len() {
                            let n = u32le(&b[p + 4..]) as usize;
                            ensure!(p + 8 + n <= b.len(), "invalid WAV label chunk");
                            if &b[p..p + 4] == b"labl" && n >= 5 {
                                let id = u32le(&b[p + 8..]);
                                let label = &b[p + 12..p + 8 + n];
                                let label = label.split(|&c| c == 0).next().unwrap_or_default();
                                ensure!(
                                    labels.len() < 1024 || labels.contains_key(&id),
                                    "too many WAV labels"
                                );
                                labels.insert(
                                    id,
                                    String::from_utf8_lossy(label)
                                        .chars()
                                        .take(128)
                                        .collect::<String>(),
                                );
                            }
                            p += 8 + n + n % 2;
                        }
                    }
                }
                _ => {}
            }
            offset = start + size + size % 2;
        }
        let (encoding, channels, rate, bits) = fmt.context("WAV has no format chunk")?;
        let (data_offset, size) = data.context("WAV has no audio data")?;
        let align = u64::from(channels) * u64::from(bits / 8);
        ensure!(size % align == 0 && size > 0, "WAV contains no complete audio frames");
        let frames = size / align;
        markers.retain(|m| m.frame <= frames);
        for marker in &mut markers {
            marker.label =
                labels.remove(&marker.id).unwrap_or_else(|| format!("Marker {}", marker.id));
        }
        markers.sort_by_key(|m| m.frame);
        Ok(Self { file, data_offset, frames, rate, channels, bits, encoding, markers })
    }
    fn read(&mut self, frame: u64, count: usize, bytes: &mut Vec<u8>) -> anyhow::Result<()> {
        let align = usize::from(self.channels) * usize::from(self.bits / 8);
        ensure!(frame + count as u64 <= self.frames, "playback position exceeds the file");
        self.file.seek(SeekFrom::Start(self.data_offset + frame * align as u64))?;
        bytes.resize(count * align, 0);
        self.file.read_exact(bytes).context("reading playback audio")?;
        Ok(())
    }
    fn sample(&self, bytes: &[u8], frame: usize, ch: usize) -> i32 {
        let width = usize::from(self.bits / 8);
        let p = (frame * usize::from(self.channels) + ch) * width;
        let b = &bytes[p..p + width];
        match (self.encoding, self.bits) {
            (3, 32) => {
                let f = f32::from_le_bytes(b.try_into().unwrap());
                if f.is_finite() { (f.clamp(-1.0, 1.0) as f64 * i32::MAX as f64) as i32 } else { 0 }
            }
            (_, 16) => (i16::from_le_bytes(b.try_into().unwrap()) as i32) << 16,
            (_, 24) => i32::from_le_bytes([0, b[0], b[1], b[2]]),
            (_, 32) => i32::from_le_bytes(b.try_into().unwrap()),
            _ => unreachable!(),
        }
    }
}

#[allow(dead_code)] // Held for their Drop implementations, after the thread exits.
enum SourceLease {
    Receive(ReceiveOverride),
    Transmit(TransmitOverride),
}
struct State {
    status: PlaybackStatus,
    revision: u64,
}
pub struct Player {
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    _source: SourceLease,
}
impl Player {
    pub fn open(
        device: &Device,
        file: File,
        path: &str,
        target: PlaybackTarget,
    ) -> anyhow::Result<Self> {
        let wave = Wave::open(file)?;
        ensure!(
            wave.rate == device.info().sample_rate,
            "WAV is {} Hz; set the device to that sample rate before opening it",
            wave.rate
        );
        let (io, source) = match target {
            PlaybackTarget::Receive => {
                let (io, lease) = device.override_receive()?;
                (io, SourceLease::Receive(lease))
            }
            PlaybackTarget::Transmit => {
                let (io, lease) = device.override_transmit()?;
                (io, SourceLease::Transmit(lease))
            }
        };
        ensure!(!io.tx.is_empty(), "the device has no channels for playback");
        let destinations = (0..usize::from(wave.channels))
            .map(|i| if i < io.tx.len() { i as u16 + 1 } else { 0 })
            .collect();
        let state = Arc::new(Mutex::new(State {
            status: PlaybackStatus {
                path: Some(path.into()),
                target,
                frames: wave.frames,
                sample_rate: wave.rate,
                channels: wave.channels,
                markers: wave.markers.clone(),
                settings: PlaybackSettings { destinations, ..Default::default() },
                ..Default::default()
            },
            revision: 0,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let s = state.clone();
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("ovsc-playback".into())
            .spawn(move || {
                if let Err(error) = play(wave, io, &s, &flag) {
                    let mut state = s.lock().unwrap_or_else(|e| e.into_inner());
                    state.status.playing = false;
                    state.status.error = Some(format!("Playback stopped: {error:#}"));
                }
            })
            .context("starting playback")?;
        Ok(Self { state, stop, thread: Some(thread), _source: source })
    }
    pub fn status(&self) -> PlaybackStatus {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).status.clone()
    }
    pub fn command(&self, request: &Request, outputs: usize) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let status = &mut state.status;
        ensure!(status.error.is_none(), "reopen the recording after a playback error");
        match request {
            Request::Play => {
                if status.position >= status.frames {
                    status.position = status.settings.loop_range.map_or(0, |r| r.0);
                }
                status.playing = true;
            }
            Request::Pause => status.playing = false,
            Request::StopPlayback => {
                status.playing = false;
                status.position = 0;
            }
            Request::SeekPlayback { frame } => {
                ensure!(*frame <= status.frames, "seek position exceeds recording duration");
                status.position = *frame;
            }
            Request::ConfigurePlayback { settings } => {
                ensure!(
                    settings.destinations.len() == usize::from(status.channels),
                    "map every recording channel"
                );
                ensure!(
                    settings.level_db.is_finite() && (-60.0..=0.0).contains(&settings.level_db),
                    "playback level must be between -60 and 0 dB"
                );
                let mut used = Vec::new();
                for &ch in &settings.destinations {
                    ensure!(ch as usize <= outputs, "playback destination exceeds device channels");
                    if ch != 0 {
                        ensure!(!used.contains(&ch), "map each destination only once");
                        used.push(ch);
                    }
                }
                if let Some((a, b)) = settings.loop_range {
                    ensure!(
                        a < b && b <= status.frames,
                        "choose a loop start before its end within the recording"
                    );
                }
                status.settings = settings.clone();
            }
            _ => bail!("unsupported playback command"),
        }
        state.revision += 1;
        Ok(())
    }
}
impl Drop for Player {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
fn play(
    mut wave: Wave,
    io: AudioIo,
    state: &Arc<Mutex<State>>,
    stop: &AtomicBool,
) -> anyhow::Result<()> {
    let block = (io.sample_rate as usize / 100).max(1);
    let capacity = io.tx.iter().map(|r| r.capacity()).min().unwrap_or(2) as u64;
    let lead = (io.sample_rate as u64 / 20).min(capacity / 4).max(1);
    let mut planes = vec![vec![0; block]; io.tx.len()];
    let mut bytes = Vec::new();
    let mut revision = u64::MAX;
    let mut cursor = 0;
    let mut next_ts = None;
    let mut anchor = None;
    while !stop.load(Ordering::Relaxed) {
        let (status, current) = {
            let s = state.lock().unwrap_or_else(|e| e.into_inner());
            (
                PlaybackStatus {
                    target: s.status.target,
                    playing: s.status.playing,
                    position: s.status.position,
                    settings: s.status.settings.clone(),
                    ..Default::default()
                },
                s.revision,
            )
        };
        if current != revision {
            for ring in &io.tx {
                ring.clear();
            }
            cursor = status.position;
            if let Some((a, b)) = status.settings.loop_range {
                if cursor < a || cursor >= b {
                    cursor = a;
                }
            }
            next_ts = None;
            anchor = None;
            revision = current;
        }
        if !status.playing {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        let Some(now) = io.now() else {
            state.lock().unwrap_or_else(|e| e.into_inner()).status.waiting_for_clock = true;
            next_ts = None;
            anchor = None;
            cursor = status.position;
            for ring in &io.tx {
                ring.clear();
            }
            std::thread::sleep(Duration::from_millis(10));
            continue;
        };
        state.lock().unwrap_or_else(|e| e.into_inner()).status.waiting_for_clock = false;
        let mut ts = next_ts.unwrap_or(now + lead);
        if ts.abs_diff(now + lead) > u64::from(io.sample_rate) / 2 {
            ts = now + lead;
            cursor = status.position;
            anchor = None;
            for ring in &io.tx {
                ring.clear();
            }
        }
        let (anchor_ts, anchor_frame) = *anchor.get_or_insert((ts, cursor));
        let until = now + lead;
        while ts < until && !stop.load(Ordering::Relaxed) {
            let end = status.settings.loop_range.map_or(wave.frames, |r| r.1);
            if cursor >= end {
                if let Some((a, _)) = status.settings.loop_range {
                    cursor = a;
                } else {
                    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                    let presented = if status.target == PlaybackTarget::Receive {
                        now.saturating_sub(io.latency_samples)
                    } else {
                        now
                    };
                    if s.revision == revision && ts <= presented {
                        s.status.playing = false;
                        s.status.position = wave.frames;
                    }
                    break;
                }
            }
            if let Some((a, _)) = status.settings.loop_range {
                if cursor < a {
                    cursor = a;
                }
            }
            let n = (until - ts).min(block as u64).min(end - cursor) as usize;
            wave.read(cursor, n, &mut bytes)?;
            for plane in &mut planes {
                plane[..n].fill(0);
            }
            let gain = if status.settings.muted {
                0.0
            } else {
                10f64.powf(status.settings.level_db / 20.0)
            };
            for (ch, &dest) in status.settings.destinations.iter().enumerate() {
                if dest == 0 {
                    continue;
                }
                for (i, sample) in planes[dest as usize - 1][..n].iter_mut().enumerate() {
                    *sample = (wave.sample(&bytes, i, ch) as f64 * gain) as i32;
                }
            }
            for (ch, plane) in planes.iter().enumerate() {
                io.tx[ch].write(ts, &plane[..n]);
            }
            ts += n as u64;
            cursor += n as u64;
            let s = state.lock().unwrap_or_else(|e| e.into_inner());
            if s.revision != revision {
                break;
            }
        }
        {
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            if s.revision == revision && s.status.playing {
                let presented = if status.target == PlaybackTarget::Receive {
                    now.saturating_sub(io.latency_samples)
                } else {
                    now
                };
                let elapsed = presented.saturating_sub(anchor_ts);
                s.status.position = match status.settings.loop_range {
                    Some((a, b)) => a + ((anchor_frame.saturating_sub(a) + elapsed) % (b - a)),
                    None => anchor_frame.saturating_add(elapsed).min(wave.frames),
                };
            }
        }
        next_ts = Some(ts);
        std::thread::sleep(Duration::from_millis(2));
    }
    for ring in &io.tx {
        ring.clear();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ovsc_core::{Channels, DeviceConfig, Ports};

    fn file(test: &str, frames: usize) -> (std::path::PathBuf, File) {
        let path =
            std::env::temp_dir().join(format!("ovsc-playback-{}-{test}.wav", std::process::id()));
        let mut wav = crate::backend::WavWriter::create(&path, 2, 48000, 24).unwrap();
        let mut bytes = Vec::new();
        for _ in 0..frames {
            bytes.extend_from_slice(&[0x56, 0x34, 0x12, 0x21, 0x43, 0x65]);
        }
        wav.write_frames(&bytes).unwrap();
        wav.finalize().unwrap();
        drop(wav);
        let file = File::open(&path).unwrap();
        (path, file)
    }
    fn wait(mut check: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !check() {
            assert!(std::time::Instant::now() < deadline, "playback did not reach expected state");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    async fn device(base: u16) -> Device {
        Device::start(
            DeviceConfig {
                name: format!("playback-{base}"),
                interface: "127.0.0.1".into(),
                tx_channels: Channels::Count(2),
                rx_channels: Channels::Count(2),
                ports: Ports {
                    arc: base,
                    cmc: base + 1,
                    flow_control: base + 2,
                    settings: base + 3,
                },
                discovery: false,
                ..Default::default()
            },
            ovsc_clock::system_clock(),
        )
        .await
        .unwrap()
    }
    fn feeder(io: AudioIo, stop: Arc<AtomicBool>) -> JoinHandle<()> {
        std::thread::spawn(move || {
            let mut pos = io.now().unwrap();
            while !stop.load(Ordering::Relaxed) {
                let until = io.now().unwrap() + 4800;
                while pos < until {
                    let n = (until - pos).min(480) as usize;
                    io.tx[0].write(pos, &vec![0x11111100; n]);
                    io.tx[1].write(pos, &vec![0x22222200; n]);
                    pos += n as u64;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    }
    fn recent(io: &AudioIo, ch: usize) -> i32 {
        io.rx[ch].read_one(io.now().unwrap().saturating_sub(2400)).unwrap_or(0)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn input_soundcheck_replaces_live_audio_and_restores_it_on_unload() {
        let device = device(24940).await;
        let io = device.audio();
        let stop = Arc::new(AtomicBool::new(false));
        let feeder = feeder(io.clone(), stop.clone());
        device.subscribe(1, "01", &device.name()).unwrap();
        wait(|| recent(&io, 0) == 0x11111100);
        let (path, file) = file("inputs", 48000);
        let player =
            Player::open(&device, file, path.to_str().unwrap(), PlaybackTarget::Receive).unwrap();
        assert!(device.override_receive().is_err());
        assert_eq!(recent(&io, 0), 0);
        let settings = PlaybackSettings {
            destinations: vec![2, 1],
            level_db: 0.0,
            loop_range: Some((100, 200)),
            ..Default::default()
        };
        player.command(&Request::ConfigurePlayback { settings: settings.clone() }, 2).unwrap();
        player.command(&Request::Play, 2).unwrap();
        wait(|| recent(&io, 0) == 0x65432100 && recent(&io, 1) == 0x12345600);
        std::thread::sleep(Duration::from_millis(100));
        assert!(player.status().playing);
        assert!((100..=200).contains(&player.status().position));
        player
            .command(
                &Request::ConfigurePlayback {
                    settings: PlaybackSettings { muted: true, ..settings },
                },
                2,
            )
            .unwrap();
        wait(|| recent(&io, 0) == 0 && recent(&io, 1) == 0);
        player.command(&Request::Pause, 2).unwrap();
        assert!(!player.status().playing);
        player.command(&Request::SeekPlayback { frame: 1000 }, 2).unwrap();
        assert_eq!(player.status().position, 1000);
        player.command(&Request::StopPlayback, 2).unwrap();
        assert_eq!(player.status().position, 0);
        assert!(player.command(&Request::SeekPlayback { frame: 48001 }, 2).is_err());
        assert!(
            player
                .command(
                    &Request::ConfigurePlayback {
                        settings: PlaybackSettings {
                            destinations: vec![1, 1],
                            ..Default::default()
                        }
                    },
                    2
                )
                .is_err()
        );
        drop(player);
        wait(|| recent(&io, 0) == 0x11111100);
        stop.store(true, Ordering::Relaxed);
        feeder.join().unwrap();
        device.shutdown().await;
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dante_soundcheck_is_not_overwritten_by_application_output() {
        let device = device(24950).await;
        let io = device.audio();
        let stop = Arc::new(AtomicBool::new(false));
        let feeder = feeder(io.clone(), stop.clone());
        device.subscribe(1, "01", &device.name()).unwrap();
        wait(|| recent(&io, 0) == 0x11111100);
        let (path, file) = file("network", 12000);
        let player =
            Player::open(&device, file, path.to_str().unwrap(), PlaybackTarget::Transmit).unwrap();
        assert!(device.override_transmit().is_err());
        player
            .command(
                &Request::ConfigurePlayback {
                    settings: PlaybackSettings {
                        destinations: vec![1, 2],
                        level_db: 0.0,
                        ..Default::default()
                    },
                },
                2,
            )
            .unwrap();
        player.command(&Request::Play, 2).unwrap();
        wait(|| recent(&io, 0) == 0x12345600);
        wait(|| !player.status().playing);
        wait(|| recent(&io, 0) == 0);
        assert_eq!(player.status().position, 12000);
        drop(player);
        wait(|| recent(&io, 0) == 0x11111100);
        stop.store(true, Ordering::Relaxed);
        feeder.join().unwrap();
        device.shutdown().await;
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn supported_wave_encodings_and_extensible_format_decode_correctly() {
        for (name, encoding, bits, data, expected, extensible) in [
            ("s16", 1u16, 16u16, vec![0x34, 0x12], 0x12340000, false),
            ("s24", 1, 24, vec![0x56, 0x34, 0x12], 0x12345600, false),
            ("s32", 1, 32, vec![0x78, 0x56, 0x34, 0x12], 0x12345678, false),
            ("float", 3, 32, 0.5f32.to_le_bytes().to_vec(), 1073741823, false),
            ("nan", 3, 32, f32::NAN.to_le_bytes().to_vec(), 0, false),
            ("extensible", 1, 24, vec![0x56, 0x34, 0x12], 0x12345600, true),
        ] {
            let path =
                std::env::temp_dir().join(format!("ovsc-format-{}-{name}.wav", std::process::id()));
            let mut bytes = b"RIFF".to_vec();
            let extra = if extensible { 24 } else { 0 };
            bytes.extend_from_slice(
                &(36 + extra + data.len() as u32 + data.len() as u32 % 2).to_le_bytes(),
            );
            bytes.extend_from_slice(b"WAVEfmt ");
            bytes.extend_from_slice(&(16u32 + extra).to_le_bytes());
            bytes.extend_from_slice(&(if extensible { 0xfffeu16 } else { encoding }).to_le_bytes());
            bytes.extend_from_slice(&1u16.to_le_bytes());
            bytes.extend_from_slice(&48000u32.to_le_bytes());
            bytes.extend_from_slice(&(48000u32 * u32::from(bits / 8)).to_le_bytes());
            bytes.extend_from_slice(&(bits / 8).to_le_bytes());
            bytes.extend_from_slice(&bits.to_le_bytes());
            if extensible {
                bytes.extend_from_slice(&22u16.to_le_bytes());
                bytes.extend_from_slice(&bits.to_le_bytes());
                bytes.extend_from_slice(&0u32.to_le_bytes());
                bytes.extend_from_slice(&encoding.to_le_bytes());
                bytes.extend_from_slice(&[
                    0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71,
                ]);
            }
            bytes.extend_from_slice(b"data");
            bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&data);
            if data.len() % 2 != 0 {
                bytes.push(0);
            }
            std::fs::write(&path, bytes).unwrap();
            let mut wave = Wave::open(File::open(&path).unwrap()).unwrap();
            let mut decoded = Vec::new();
            wave.read(0, 1, &mut decoded).unwrap();
            assert_eq!(wave.sample(&decoded, 0, 0), expected, "{name}");
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn wave_reader_is_bit_exact_and_rejects_truncated_files() {
        let (path, file) = file("reader", 3);
        let mut wave = Wave::open(file).unwrap();
        let mut bytes = Vec::new();
        wave.read(0, 3, &mut bytes).unwrap();
        assert_eq!(wave.sample(&bytes, 2, 0), 0x12345600);
        assert_eq!(wave.sample(&bytes, 2, 1), 0x65432100);
        assert!(wave.read(3, 1, &mut bytes).is_err());
        File::options().write(true).open(&path).unwrap().set_len(45).unwrap();
        assert!(Wave::open(File::open(&path).unwrap()).is_err());
        std::fs::remove_file(path).unwrap();
    }
}
