//! Built-in audio backends that need no audio hardware.
//!
//! Each runs on its own thread, paced by the media clock, and talks to the
//! device only through [`AudioIo`].

use std::f64::consts::TAU;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context;
use tracing::info;

use ovsc_core::AudioIo;

/// A running backend thread.
pub struct Running {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    recording: Option<Arc<Mutex<ovsc_control::RecordingStatus>>>,
}

impl Running {
    fn spawn(name: &str, f: impl FnOnce(Arc<AtomicBool>) + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let thread =
            std::thread::Builder::new().name(name.into()).spawn(move || f(s)).expect("spawn");
        Self { stop, thread: Some(thread), recording: None }
    }

    pub fn recording_status(&self) -> ovsc_control::RecordingStatus {
        let mut status = self
            .recording
            .as_ref()
            .expect("recording worker")
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if self.thread.as_ref().is_none_or(|t| t.is_finished()) {
            status.recording = false;
            status.waiting_for_clock = false;
        }
        status
    }

    pub fn add_marker(&self, label: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.thread.as_ref().is_some_and(|t| !t.is_finished()),
            "recording is not running"
        );
        anyhow::ensure!(
            label.len() <= 128 && !label.contains('\0'),
            "marker labels must be at most 128 bytes and contain no NUL"
        );
        let mut status = self
            .recording
            .as_ref()
            .context("not a recorder")?
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        anyhow::ensure!(
            !status.waiting_for_clock || status.captured_frames > 0,
            "wait for recording to start before adding a marker"
        );
        anyhow::ensure!(status.markers.len() < 1024, "this take already has 1024 markers");
        let id = status.markers.len() as u32 + 1;
        let frame = status.captured_frames;
        let label =
            if label.trim().is_empty() { format!("Marker {id}") } else { label.trim().to_owned() };
        status.markers.push(ovsc_control::Marker { id, frame, label });
        Ok(())
    }

    pub fn finish_recording(mut self) -> ovsc_control::RecordingStatus {
        self.stop_inner();
        self.recording_status()
    }

    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

const TICK: Duration = Duration::from_millis(5);

/// Waits for the media clock, returning `None` if asked to stop first.
fn wait_for_clock(io: &AudioIo, stop: &AtomicBool) -> Option<u64> {
    let mut warned = false;
    loop {
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        if let Some(now) = io.now() {
            return Some(now);
        }
        if !warned {
            info!("waiting for the media clock (PTP master)…");
            warned = true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Sine tones on every transmit channel: channel `n` (1-based) plays
/// `n * base_hz`. The phase is derived from the media timestamp, so every
/// device generating the same tone is phase-aligned.
pub fn tone(io: AudioIo, base_hz: u32, level_db: f64) -> Running {
    Running::spawn("ovsc-tone", move |stop| {
        let Some(mut written) = wait_for_clock(&io, &stop) else { return };
        let amplitude = 10f64.powf(level_db / 20.0) * i32::MAX as f64;
        let lead = io.sample_rate as u64 / 20; // 50 ms ahead
        let rate = io.sample_rate as u64;
        let mut buf = vec![0i32; io.sample_rate as usize];
        info!("tone: {} Hz × channel number at {level_db} dBFS", base_hz);
        while !stop.load(Ordering::Relaxed) {
            if let Some(now) = io.now() {
                if written + lead < now || written > now + 10 * lead {
                    written = now; // clock jumped
                }
                let until = now + lead;
                while written < until {
                    let n = ((until - written) as usize).min(buf.len());
                    for ch in 0..io.tx.len() {
                        let hz = base_hz as u64 * (ch as u64 + 1);
                        for (i, s) in buf[..n].iter_mut().enumerate() {
                            // Exact phase: (ts * hz mod rate) / rate cycles.
                            let cycles = ((written + i as u64) % rate * hz % rate) as f64;
                            *s = (amplitude * (TAU * cycles / rate as f64).sin()) as i32;
                        }
                        io.write_tx(ch, written, &buf[..n]);
                    }
                    written += n as u64;
                }
            }
            std::thread::sleep(TICK);
        }
    })
}

/// Sends receive channel `n` back out on transmit channel `n`, delayed by the
/// receive latency plus a small lead.
pub fn loopback(io: AudioIo) -> Running {
    Running::spawn("ovsc-loopback", move |stop| {
        let Some(now) = wait_for_clock(&io, &stop) else { return };
        let latency = io.latency_samples;
        let lead = io.sample_rate as u64 / 100; // 10 ms
        let mut pos = now.saturating_sub(latency);
        let channels = io.rx.len().min(io.tx.len());
        let mut buf = vec![0i32; io.sample_rate as usize];
        info!("loopback: rx 1..{channels} -> tx 1..{channels}");
        while !stop.load(Ordering::Relaxed) {
            if let Some(now) = io.now() {
                let ready = now.saturating_sub(latency);
                if pos + io.sample_rate as u64 <= ready || pos > ready + lead {
                    pos = ready;
                }
                while pos < ready {
                    let n = ((ready - pos) as usize).min(buf.len());
                    for ch in 0..channels {
                        io.read_rx(ch, pos, &mut buf[..n]);
                        io.write_tx(ch, pos + latency + lead, &buf[..n]);
                    }
                    pos += n as u64;
                }
            }
            std::thread::sleep(TICK);
        }
    })
}

/// Records every receive channel into a WAV file (24-bit PCM).
pub fn record(io: AudioIo, path: &Path) -> anyhow::Result<Running> {
    anyhow::ensure!(!io.rx.is_empty(), "the record backend needs at least one receive channel");
    let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    record_file(io, path, file)
}

/// Records into an already opened file, so the control socket can enforce
/// ownership and avoid overwriting an existing destination.
pub fn record_file(io: AudioIo, path: &Path, file: File) -> anyhow::Result<Running> {
    anyhow::ensure!(!io.rx.is_empty(), "the record backend needs at least one receive channel");
    let mut wav = WavWriter::from_file(file, io.rx.len() as u16, io.sample_rate, 24)?;
    // Reserve enough RIFF space for 1024 labeled cue points.
    wav.reserve_bytes = 256 * 1024;
    let status = Arc::new(Mutex::new(ovsc_control::RecordingStatus {
        recording: true,
        waiting_for_clock: !io.clock.is_ready(),
        path: Some(path.to_string_lossy().into_owned()),
        sample_rate: io.sample_rate,
        channels: io.rx.len() as u16,
        ..Default::default()
    }));
    // Capture and disk I/O are independent. Bound the queue to two seconds
    // and 64 MiB, even at the highest supported rate and channel count.
    let capacity = io.rx.iter().map(|r| r.capacity()).min().unwrap_or(2);
    let block_frames = (io.sample_rate as usize / 100).max(1).min((capacity / 4).max(1));
    let block_bytes = block_frames * io.rx.len() * 3;
    let queue_blocks = (64 * 1024 * 1024 / block_bytes).clamp(2, 200);
    let (send, receive) = mpsc::sync_channel(queue_blocks);
    let progress = status.clone();
    let rate = io.sample_rate;
    let channels = io.rx.len();
    // Always put a valid empty header on disk, even before PTP is ready.
    wav.finalize()?;
    wav.sync()?;
    let disk = std::thread::Builder::new()
        .name("ovsc-record-disk".into())
        .spawn(move || {
            write_recording(&mut wav, receive, rate, channels, &progress);
        })
        .context("starting the recording disk writer")?;
    let progress = status.clone();
    let mut running = Running::spawn("ovsc-record", move |stop| {
        capture_recording(&io, &stop, send, block_frames, &progress);
        if disk.join().is_err() {
            progress.lock().unwrap_or_else(|e| e.into_inner()).error =
                Some("The recording disk writer stopped unexpectedly.".into());
        }
    });
    running.recording = Some(status);
    Ok(running)
}

type RecordingProgress = Arc<Mutex<ovsc_control::RecordingStatus>>;

/// Offsets are relative to the start of the file. A missing block therefore
/// leaves a measurable gap, rather than shifting all later audio earlier.
enum RecordingBlock {
    Audio { offset: u64, bytes: Vec<u8> },
    Finish { frames: u64 },
}

fn recording_delay(io: &AudioIo) -> u64 {
    let capacity = io.rx.iter().map(|r| r.capacity()).min().unwrap_or(2) as u64;
    // A recorder can wait longer than live monitoring for late packets.
    // Leave at least half the ring for scheduler jitter and capture backlog.
    io.latency_samples.max(io.sample_rate as u64 / 20).min(capacity / 2)
}

fn capture_recording(
    io: &AudioIo,
    stop: &AtomicBool,
    send: mpsc::SyncSender<RecordingBlock>,
    block_frames: usize,
    progress: &RecordingProgress,
) {
    let Some(now) = wait_for_clock(io, stop) else { return };
    let delay = recording_delay(io);
    let capacity = io.rx.iter().map(|r| r.capacity()).min().unwrap_or(2) as u64;
    let mut pos = now.saturating_sub(delay);
    let mut frames = 0u64;
    let mut last_clock = (now, ovsc_clock::local_now_ns());
    let mut planes = vec![vec![0; block_frames]; io.rx.len()];
    'capture: while !stop.load(Ordering::Relaxed) {
        let local = ovsc_clock::local_now_ns();
        let predicted = last_clock.0.saturating_add(ovsc_clock::ns_to_samples(
            local.saturating_sub(last_clock.1),
            io.sample_rate,
        ));
        let clock = io.now();
        let now = clock.unwrap_or(predicted);
        let ready = now.saturating_sub(delay);
        {
            let mut status = progress.lock().unwrap_or_else(|e| e.into_inner());
            status.waiting_for_clock = clock.is_none();
            // Clock jumps are a new timestamp epoch, not hours of silence.
            if ready < pos || now.abs_diff(predicted) > 2 * u64::from(io.sample_rate) {
                pos = ready;
                status.clock_resets += 1;
            }
        }
        last_clock = (now, local);
        // Old samples have already been overwritten. Preserve their duration
        // as an explicit gap in the disk stream and start at retained audio.
        let oldest = now.saturating_sub(capacity.saturating_sub(block_frames as u64));
        if pos < oldest {
            let lost = oldest - pos;
            frames += lost;
            pos = oldest;
            let mut status = progress.lock().unwrap_or_else(|e| e.into_inner());
            status.capture_lost_frames += lost;
            status.captured_frames = frames;
        }
        while pos < ready && !stop.load(Ordering::Relaxed) {
            let n = (ready - pos).min(block_frames as u64) as usize;
            let mut missing = 0u64;
            for (ch, plane) in planes.iter_mut().enumerate() {
                missing += (n - io.rx[ch].read(pos, &mut plane[..n])) as u64;
            }
            let mut bytes = Vec::with_capacity(n * io.rx.len() * 3);
            for i in 0..n {
                for plane in &planes {
                    bytes.extend_from_slice(&plane[i].to_le_bytes()[1..4]);
                }
            }
            progress.lock().unwrap_or_else(|e| e.into_inner()).missing_samples += missing;
            let block = RecordingBlock::Audio { offset: frames, bytes };
            frames += n as u64;
            pos += n as u64;
            progress.lock().unwrap_or_else(|e| e.into_inner()).captured_frames = frames;
            match send.try_send(block) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(_)) => {
                    progress.lock().unwrap_or_else(|e| e.into_inner()).disk_lost_frames += n as u64;
                }
                Err(mpsc::TrySendError::Disconnected(_)) => break 'capture,
            }
        }
        std::thread::sleep(TICK);
    }
    // The terminal offset also preserves any discarded trailing blocks.
    let _ = send.send(RecordingBlock::Finish { frames });
}

fn write_recording(
    wav: &mut WavWriter,
    receive: mpsc::Receiver<RecordingBlock>,
    rate: u32,
    channels: usize,
    progress: &RecordingProgress,
) {
    let align = channels * 3;
    let silence = vec![0; (rate as usize / 100).max(1) * align];
    let mut checkpoint = std::time::Instant::now();
    let result = (|| -> std::io::Result<()> {
        loop {
            let block = match receive.recv_timeout(Duration::from_millis(100)) {
                Ok(block) => Some(block),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            if let Some(block) = block {
                let offset = match &block {
                    RecordingBlock::Audio { offset, .. } => *offset,
                    RecordingBlock::Finish { frames } => *frames,
                };
                while wav.data_bytes / (align as u64) < offset {
                    let gap = offset - wav.data_bytes / (align as u64);
                    let n = gap.min((silence.len() / align) as u64) as usize;
                    wav.write_frames(&silence[..n * align])?;
                }
                if let RecordingBlock::Audio { bytes, .. } = &block {
                    wav.write_frames(bytes)?;
                }
                {
                    let mut status = progress.lock().unwrap_or_else(|e| e.into_inner());
                    status.frames = wav.data_bytes / align as u64;
                    status.bytes = wav.data_bytes;
                }
                if matches!(block, RecordingBlock::Finish { .. }) {
                    break;
                }
            }
            // Use local elapsed time: an unlocked or stepped PTP clock must
            // not prevent the header and data from being checkpointed.
            if checkpoint.elapsed() >= Duration::from_secs(1) {
                let markers = progress.lock().unwrap_or_else(|e| e.into_inner()).markers.clone();
                wav.finalize_markers(&markers)?;
                wav.sync()?;
                checkpoint = std::time::Instant::now();
            }
        }
        Ok(())
    })();
    let markers = progress.lock().unwrap_or_else(|e| e.into_inner()).markers.clone();
    let finalize = wav.finalize_markers(&markers).and_then(|()| wav.sync());
    let mut status = progress.lock().unwrap_or_else(|e| e.into_inner());
    status.frames = wav.data_bytes / align as u64;
    status.bytes = wav.data_bytes;
    status.error = match (result, finalize) {
        (Err(e), Err(f)) => Some(format!("Recording stopped: {e}. Finalizing also failed: {f}")),
        (Err(e), Ok(())) => Some(format!("Recording stopped: {e}")),
        (Ok(()), Err(e)) => Some(format!("Could not finalize the recording: {e}")),
        (Ok(()), Ok(())) => None,
    };
}

/// Count accepted bytes even when the storage device fails halfway through
/// a block. Retrying interruptions must never replay bytes already written.
fn write_audio_bytes(writer: &mut impl Write, bytes: &[u8]) -> (usize, std::io::Result<()>) {
    let mut written = 0;
    let result = loop {
        if written == bytes.len() {
            break Ok(());
        }
        match writer.write(&bytes[written..]) {
            Ok(0) => {
                break Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "disk accepted no audio",
                ));
            }
            Ok(n) => written += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => break Err(e),
        }
    };
    (written, result)
}

/// Minimal streaming WAV (RIFF, PCM) writer.
pub struct WavWriter {
    file: File,
    data_bytes: u64,
    block_align: usize,
    metadata_written: bool,
    reserve_bytes: u64,
}

impl WavWriter {
    #[cfg(test)]
    pub fn create(path: &Path, channels: u16, rate: u32, bits: u16) -> std::io::Result<Self> {
        Self::from_file(File::create(path)?, channels, rate, bits)
    }

    fn from_file(file: File, channels: u16, rate: u32, bits: u16) -> std::io::Result<Self> {
        let mut file = file;
        if channels == 0 || bits != 24 || rate == 0 {
            return Err(std::io::Error::other("invalid WAV format"));
        }
        let block_align = channels * bits / 8;
        let mut h = Vec::with_capacity(44);
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&36u32.to_le_bytes());
        h.extend_from_slice(b"WAVEfmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes()); // PCM
        h.extend_from_slice(&channels.to_le_bytes());
        h.extend_from_slice(&rate.to_le_bytes());
        h.extend_from_slice(&(rate * block_align as u32).to_le_bytes());
        h.extend_from_slice(&block_align.to_le_bytes());
        h.extend_from_slice(&bits.to_le_bytes());
        h.extend_from_slice(b"data");
        h.extend_from_slice(&0u32.to_le_bytes());
        file.write_all(&h)?;
        Ok(Self {
            file,
            data_bytes: 0,
            block_align: block_align as usize,
            metadata_written: false,
            reserve_bytes: 0,
        })
    }

    pub fn write_frames(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if bytes.len() % self.block_align != 0 {
            return Err(std::io::Error::other("incomplete WAV frame"));
        }
        if self.data_bytes + bytes.len() as u64 > u32::MAX as u64 - 36 - self.reserve_bytes {
            return Err(std::io::Error::other("WAV file size limit (4 GiB) reached"));
        }
        if self.metadata_written {
            self.file.set_len(44 + self.data_bytes)?;
            self.file.seek(SeekFrom::Start(44 + self.data_bytes))?;
            self.metadata_written = false;
        }
        let (written, result) = write_audio_bytes(&mut self.file, bytes);
        self.data_bytes += (written / self.block_align * self.block_align) as u64;
        if result.is_err() {
            // A full disk can leave half a PCM frame. Keep complete frames
            // only, so finalization can recover a playable partial take.
            self.file.set_len(44 + self.data_bytes)?;
            self.file.seek(SeekFrom::End(0))?;
        }
        result
    }

    fn sync(&self) -> std::io::Result<()> {
        self.file.sync_data()
    }

    /// Writes the sizes into the header; writing may continue afterwards.
    pub fn finalize(&mut self) -> std::io::Result<()> {
        self.finalize_markers(&[])
    }

    fn finalize_markers(&mut self, markers: &[ovsc_control::Marker]) -> std::io::Result<()> {
        let audio_end = 44 + self.data_bytes;
        self.file.set_len(audio_end)?;
        self.file.seek(SeekFrom::Start(audio_end))?;
        let markers: Vec<_> = markers
            .iter()
            .filter(|m| m.frame <= self.data_bytes / self.block_align as u64)
            .collect();
        if !markers.is_empty() {
            if self.data_bytes % 2 != 0 {
                self.file.write_all(&[0])?;
            }
            self.file.write_all(b"cue ")?;
            self.file.write_all(&(4 + markers.len() as u32 * 24).to_le_bytes())?;
            self.file.write_all(&(markers.len() as u32).to_le_bytes())?;
            for marker in &markers {
                self.file.write_all(&marker.id.to_le_bytes())?;
                self.file.write_all(&(marker.frame as u32).to_le_bytes())?;
                self.file.write_all(b"data")?;
                self.file.write_all(&[0; 8])?;
                self.file.write_all(&(marker.frame as u32).to_le_bytes())?;
            }
            let mut labels = b"adtl".to_vec();
            for marker in &markers {
                labels.extend_from_slice(b"labl");
                let len = 4 + marker.label.len() + 1;
                labels.extend_from_slice(&(len as u32).to_le_bytes());
                labels.extend_from_slice(&marker.id.to_le_bytes());
                labels.extend_from_slice(marker.label.as_bytes());
                labels.push(0);
                if len % 2 != 0 {
                    labels.push(0);
                }
            }
            self.file.write_all(b"LIST")?;
            self.file.write_all(&(labels.len() as u32).to_le_bytes())?;
            self.file.write_all(&labels)?;
        }
        let end = self.file.stream_position()?;
        if end - 8 > u32::MAX as u64 {
            return Err(std::io::Error::other("WAV markers exceed RIFF size limit"));
        }
        self.file.seek(SeekFrom::Start(4))?;
        self.file.write_all(&((end - 8) as u32).to_le_bytes())?;
        self.file.seek(SeekFrom::Start(40))?;
        self.file.write_all(&(self.data_bytes as u32).to_le_bytes())?;
        self.file.seek(SeekFrom::Start(audio_end))?;
        self.metadata_written = end > audio_end;
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recording_io(clock: ovsc_clock::MediaClock, channels: usize) -> AudioIo {
        AudioIo {
            clock,
            sample_rate: 48_000,
            format: ovsc_proto::audio::SampleFormat::S24,
            latency_samples: 192,
            rx: (0..channels).map(|_| Arc::new(ovsc_core::buffer::TimedRing::new(32768))).collect(),
            tx: Vec::new(),
            rx_names: vec!["01".into(); channels],
            tx_names: Vec::new(),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn new_subscriptions_join_the_same_recording_without_interrupting_other_channels() {
        use ovsc_core::{Channels, Device, DeviceConfig, Ports};
        let path = std::env::temp_dir().join(format!("ovsc-live-route-{}.wav", std::process::id()));
        let device = Device::start(
            DeviceConfig {
                name: "recorder-live".into(),
                interface: "127.0.0.1".into(),
                tx_channels: Channels::Count(2),
                rx_channels: Channels::Count(2),
                ports: Ports { arc: 24920, cmc: 24921, flow_control: 24922, settings: 24923 },
                discovery: false,
                ..Default::default()
            },
            ovsc_clock::system_clock(),
        )
        .await
        .unwrap();
        let io = device.audio();
        let feeder_io = io.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let feeder = std::thread::spawn(move || {
            let mut pos = feeder_io.now().unwrap();
            while !flag.load(Ordering::Relaxed) {
                let until = feeder_io.now().unwrap() + 4800;
                while pos < until {
                    let n = (until - pos).min(480) as usize;
                    feeder_io.write_tx(0, pos, &vec![0x12345600; n]);
                    feeder_io.write_tx(1, pos, &vec![0x65432100; n]);
                    pos += n as u64;
                }
                std::thread::sleep(TICK);
            }
        });
        let recording = record(io.clone(), &path).unwrap();
        let wait_recorded = |target| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while recording.recording_status().frames < target {
                assert!(std::time::Instant::now() < deadline, "recorder stalled during routing");
                std::thread::sleep(TICK);
            }
        };
        wait_recorded(4800);
        device.subscribe(1, "01", "recorder-live").unwrap();
        let wait_audio = |channel: usize, expected: i32| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let ts = io.now().unwrap().saturating_sub(2400);
                if io.rx[channel].read_one(ts) == Some(expected) {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "new subscription did not receive audio"
                );
                std::thread::sleep(TICK);
            }
        };
        wait_audio(0, 0x12345600);
        wait_recorded(recording.recording_status().frames + 4800);
        device.subscribe(2, "02", "recorder-live").unwrap();
        wait_audio(1, 0x65432100);
        wait_recorded(recording.recording_status().frames + 4800);
        device.unsubscribe(2).unwrap();
        wait_recorded(recording.recording_status().frames + 9600);
        let finished = recording.finish_recording();
        stop.store(true, Ordering::Relaxed);
        feeder.join().unwrap();
        device.shutdown().await;
        assert!(finished.error.is_none());
        assert_eq!(finished.channels, 2);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len() as u64, 44 + finished.bytes);
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 2);
        let frames: Vec<_> = bytes[44..].chunks_exact(6).collect();
        let silent = [0; 6];
        let first_only = [0x56, 0x34, 0x12, 0, 0, 0];
        let both = [0x56, 0x34, 0x12, 0x21, 0x43, 0x65];
        assert!(frames[..4800].iter().all(|f| **f == silent));
        let first = frames.iter().position(|f| **f == first_only).unwrap();
        let joined = frames.iter().position(|f| **f == both).unwrap();
        assert!(first < joined);
        // Channel 1 stays bit-exact after channel 2 joins and leaves.
        assert!(frames[joined..].iter().all(|f| f[..3] == first_only[..3]));
        assert!(frames[frames.len() - 480..].iter().all(|f| **f == first_only));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn recording_rejects_zero_channels_before_creating_file() {
        let path = std::env::temp_dir().join(format!("ovsc-zero-{}.wav", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert!(record(recording_io(ovsc_clock::system_clock(), 0), &path).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn stopping_while_waiting_for_clock_finalizes_an_empty_wav() {
        let path = std::env::temp_dir().join(format!("ovsc-wait-{}.wav", std::process::id()));
        let (clock, _writer) = ovsc_clock::MediaClock::new();
        let running = record(recording_io(clock, 1), &path).unwrap();
        assert!(running.recording_status().waiting_for_clock);
        let finished = running.finish_recording();
        assert!(!finished.recording);
        assert!(!finished.waiting_for_clock);
        assert!(finished.error.is_none());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 44);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn recording_resumes_after_backward_clock_step() {
        let path = std::env::temp_dir().join(format!("ovsc-step-{}.wav", std::process::id()));
        let (clock, writer) = ovsc_clock::MediaClock::new();
        let publish = |seconds: u64| {
            writer.publish(ovsc_clock::ClockSnapshot {
                local_ref_ns: ovsc_clock::local_now_ns(),
                media_ref_ns: seconds * 1_000_000_000,
                rate: 0.0,
            });
        };
        let data_len = || {
            let bytes = std::fs::read(&path).unwrap();
            if bytes.len() < 44 {
                return 0;
            }
            u32::from_le_bytes(bytes[40..44].try_into().unwrap())
        };
        let wait_for_data = |previous| {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while data_len() <= previous {
                assert!(std::time::Instant::now() < deadline, "recorder did not resume");
                std::thread::sleep(TICK);
            }
        };
        publish(100);
        let running = record(recording_io(clock, 1), &path).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        publish(102);
        wait_for_data(0);
        let before_step = data_len();
        publish(10);
        std::thread::sleep(Duration::from_millis(100));
        publish(12);
        wait_for_data(before_step);
        let active = running.recording_status();
        assert!(active.recording);
        assert!(!active.waiting_for_clock);
        assert!(active.frames > 0);
        assert_eq!(active.bytes, active.frames * 3);
        let finished = running.finish_recording();
        assert!(!finished.recording);
        assert!(finished.error.is_none());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn wav_size_limit_preserves_existing_audio_when_finalized() {
        let path = std::env::temp_dir().join(format!("ovsc-limit-{}.wav", std::process::id()));
        let mut wav = WavWriter::create(&path, 1, 48_000, 24).unwrap();
        wav.file.flush().unwrap();
        // A sparse file exercises the real RIFF limit without writing 4 GiB.
        wav.data_bytes = (u32::MAX as u64 - 36) / 3 * 3;
        wav.file.set_len(44 + wav.data_bytes).unwrap();
        wav.file.seek(SeekFrom::End(0)).unwrap();
        assert!(wav.write_frames(&[0; 3]).is_err());
        wav.finalize().unwrap();
        let expected = wav.data_bytes as u32;
        drop(wav);
        use std::io::Read;
        let mut header = [0; 44];
        File::open(&path).unwrap().read_exact(&mut header).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(u32::from_le_bytes(header[40..44].try_into().unwrap()), expected);
        assert_eq!(u32::from_le_bytes(header[4..8].try_into().unwrap()), expected + 36);
    }

    #[test]
    fn stalled_disk_queue_preserves_trailing_audio_duration_as_silence() {
        let path = std::env::temp_dir().join(format!("ovsc-queue-{}.wav", std::process::id()));
        let (clock, writer) = ovsc_clock::MediaClock::new();
        let publish = |ns| {
            writer.publish(ovsc_clock::ClockSnapshot {
                local_ref_ns: ovsc_clock::local_now_ns(),
                media_ref_ns: ns,
                rate: 0.0,
            })
        };
        publish(100_000_000_000);
        let io = recording_io(clock, 1);
        let start = 100 * 48_000 - recording_delay(&io);
        io.rx[0].write(start, &vec![0x12345600; 9600]);
        let progress = Arc::new(Mutex::new(ovsc_control::RecordingStatus {
            waiting_for_clock: true,
            ..Default::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (send, receive) = mpsc::sync_channel(1);
        let state = progress.clone();
        let flag = stop.clone();
        let capture = std::thread::spawn(move || capture_recording(&io, &flag, send, 480, &state));
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while progress.lock().unwrap().waiting_for_clock {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(TICK);
        }
        publish(100_200_000_000);
        while progress.lock().unwrap().disk_lost_frames < 9120 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(TICK);
        }
        stop.store(true, Ordering::Relaxed);
        // Drain only after the capture queue has overflowed.
        let blocks: Vec<_> = receive.iter().collect();
        capture.join().unwrap();
        let (send, receive) = mpsc::sync_channel(blocks.len());
        for block in blocks {
            send.send(block).unwrap();
        }
        drop(send);
        let mut wav = WavWriter::create(&path, 1, 48000, 24).unwrap();
        write_recording(&mut wav, receive, 48000, 1, &progress);
        let status = progress.lock().unwrap();
        assert!(status.error.is_none());
        assert_eq!(status.frames, 9600);
        assert_eq!(status.disk_lost_frames, 9120);
        assert_eq!(status.missing_samples, 0);
        drop(status);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[44..47], &[0x56, 0x34, 0x12]);
        assert!(bytes[44 + 480 * 3..].iter().all(|&b| b == 0));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn late_packets_are_recorded_and_missing_samples_are_reported() {
        let path = std::env::temp_dir().join(format!("ovsc-late-{}.wav", std::process::id()));
        let (clock, writer) = ovsc_clock::MediaClock::new();
        writer.publish(ovsc_clock::ClockSnapshot {
            local_ref_ns: ovsc_clock::local_now_ns(),
            media_ref_ns: 100_000_000_000,
            rate: 1.0,
        });
        let io = recording_io(clock.clone(), 1);
        let ring = io.rx[0].clone();
        let running = record(io.clone(), &path).unwrap();
        let timestamp = clock.now_samples(48000).unwrap();
        // Packet is later than live receive latency, but within recorder's
        // extra 50 ms grace. It must still appear verbatim in the WAV.
        std::thread::sleep(Duration::from_millis(15));
        ring.write(timestamp, &[0x12345600; 480]);
        std::thread::sleep(Duration::from_millis(100));
        let status = running.finish_recording();
        assert!(status.frames > 0 && status.missing_samples > 0);
        assert!(status.error.is_none());
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes[44..].chunks_exact(3).any(|s| s == [0x56, 0x34, 0x12]));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn clock_loss_keeps_recording_and_reacquisition_resumes() {
        let path = std::env::temp_dir().join(format!("ovsc-clock-loss-{}.wav", std::process::id()));
        let (clock, writer) = ovsc_clock::MediaClock::new();
        let snapshot = ovsc_clock::ClockSnapshot {
            local_ref_ns: ovsc_clock::local_now_ns(),
            media_ref_ns: 100_000_000_000,
            rate: 1.0,
        };
        writer.publish(snapshot);
        let running = record(recording_io(clock, 1), &path).unwrap();
        let wait = |condition: &dyn Fn(&ovsc_control::RecordingStatus) -> bool| {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            loop {
                let status = running.recording_status();
                if condition(&status) {
                    return status;
                }
                assert!(std::time::Instant::now() < deadline, "recording recovery timed out");
                std::thread::sleep(TICK);
            }
        };
        wait(&|s| s.frames > 0);
        writer.invalidate();
        let lost = wait(&|s| s.waiting_for_clock);
        wait(&|s| s.frames > lost.frames + 480);
        writer.publish(snapshot);
        let resumed = wait(&|s| !s.waiting_for_clock);
        assert!(resumed.recording);
        assert_eq!(resumed.clock_resets, 0);
        let finished = running.finish_recording();
        assert!(finished.error.is_none());
        assert!(finished.frames > lost.frames);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn large_forward_clock_step_does_not_create_hours_of_silence() {
        let path = std::env::temp_dir().join(format!("ovsc-forward-{}.wav", std::process::id()));
        let (clock, writer) = ovsc_clock::MediaClock::new();
        let publish = |ns| {
            writer.publish(ovsc_clock::ClockSnapshot {
                local_ref_ns: ovsc_clock::local_now_ns(),
                media_ref_ns: ns,
                rate: 0.0,
            })
        };
        publish(100_000_000_000);
        let running = record(recording_io(clock, 1), &path).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        publish(3_600_000_000_000);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while running.recording_status().clock_resets == 0 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(TICK);
        }
        publish(3_600_200_000_000);
        while running.recording_status().frames == 0 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(TICK);
        }
        let finished = running.finish_recording();
        assert_eq!(finished.clock_resets, 1);
        assert!(finished.frames <= 9600);
        assert!(finished.error.is_none());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn storage_interruptions_and_partial_failures_do_not_duplicate_audio() {
        struct Storage {
            bytes: Vec<u8>,
            calls: usize,
        }
        impl Write for Storage {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.calls += 1;
                match self.calls {
                    1 => Err(std::io::ErrorKind::Interrupted.into()),
                    2 => {
                        self.bytes.extend_from_slice(&bytes[..3]);
                        Ok(3)
                    }
                    _ => Err(std::io::Error::other("disk full")),
                }
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut storage = Storage { bytes: Vec::new(), calls: 0 };
        let (accepted, result) = write_audio_bytes(&mut storage, &[1, 2, 3, 4, 5, 6]);
        assert_eq!(accepted, 3);
        assert_eq!(storage.bytes, [1, 2, 3]);
        assert_eq!(result.unwrap_err().to_string(), "disk full");
        struct Stalled;
        impl Write for Stalled {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Ok(0)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (accepted, result) = write_audio_bytes(&mut Stalled, &[1]);
        assert_eq!(accepted, 0);
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::WriteZero);
    }

    #[test]
    fn wav_markers_survive_checkpoints_without_corrupting_audio() {
        let path = std::env::temp_dir().join(format!("ovsc-markers-{}.wav", std::process::id()));
        let mut wav = WavWriter::create(&path, 1, 48000, 24).unwrap();
        wav.write_frames(&[1, 2, 3, 4, 5, 6, 7, 8, 9]).unwrap();
        let marker = ovsc_control::Marker { id: 1, frame: 2, label: "Verse".into() };
        wav.finalize_markers(std::slice::from_ref(&marker)).unwrap();
        wav.write_frames(&[10, 11, 12]).unwrap();
        wav.finalize_markers(std::slice::from_ref(&marker)).unwrap();
        drop(wav);
        let wave = crate::playback::Wave::open(File::open(&path).unwrap()).unwrap();
        assert_eq!(wave.frames, 4);
        assert_eq!(wave.markers, [marker]);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[44..56], &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize, bytes.len() - 8);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn wav_header_is_valid() {
        let path = std::env::temp_dir().join(format!("ovsc-{}.wav", std::process::id()));
        let mut w = WavWriter::create(&path, 2, 48_000, 24).unwrap();
        w.write_frames(&[1, 2, 3, 4, 5, 6]).unwrap();
        w.finalize().unwrap();
        w.write_frames(&[7, 8, 9, 10, 11, 12]).unwrap();
        w.finalize().unwrap();
        drop(w);
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(bytes.len(), 44 + 12);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 36 + 12);
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), 48_000);
        assert_eq!(u16::from_le_bytes(bytes[32..34].try_into().unwrap()), 6);
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 12);
    }
}
