//! Built-in audio backends that need no audio hardware.
//!
//! Each runs on its own thread, paced by the media clock, and talks to the
//! device only through [`AudioIo`].

use std::f64::consts::TAU;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context;
use tracing::{info, warn};

use ovsc_core::AudioIo;

/// A running backend thread.
pub struct Running {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Running {
    fn spawn(name: &str, f: impl FnOnce(Arc<AtomicBool>) + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let thread =
            std::thread::Builder::new().name(name.into()).spawn(move || f(s)).expect("spawn");
        Self { stop, thread: Some(thread) }
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
    let mut wav = WavWriter::create(path, io.rx.len() as u16, io.sample_rate, 24)
        .with_context(|| format!("creating {}", path.display()))?;
    let path = path.to_owned();
    Ok(Running::spawn("ovsc-record", move |stop| {
        let Some(now) = wait_for_clock(&io, &stop) else { return };
        let latency = io.latency_samples;
        let mut pos = now.saturating_sub(latency);
        let channels = io.rx.len();
        let mut planes = vec![vec![0i32; io.sample_rate as usize]; channels];
        let mut frame_bytes = Vec::with_capacity(io.sample_rate as usize * channels * 3);
        let mut since_flush = 0u64;
        info!("recording {channels} channels to {}", path.display());
        while !stop.load(Ordering::Relaxed) {
            if let Some(now) = io.now() {
                let ready = now.saturating_sub(latency);
                if ready > pos + 10 * io.sample_rate as u64 {
                    warn!("recorder fell behind, skipping {} samples", ready - pos);
                    pos = ready;
                }
                while pos < ready {
                    let n = ((ready - pos) as usize).min(planes[0].len().max(1));
                    for (ch, plane) in planes.iter_mut().enumerate() {
                        io.read_rx(ch, pos, &mut plane[..n]);
                    }
                    frame_bytes.clear();
                    for i in 0..n {
                        for plane in &planes {
                            frame_bytes.extend_from_slice(&plane[i].to_le_bytes()[1..4]);
                        }
                    }
                    if let Err(e) = wav.write_frames(&frame_bytes) {
                        warn!("recording stopped: {e}");
                        return;
                    }
                    pos += n as u64;
                    since_flush += n as u64;
                }
                if since_flush >= io.sample_rate as u64 {
                    // Keep the header valid in case we're killed.
                    let _ = wav.finalize();
                    since_flush = 0;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if let Err(e) = wav.finalize() {
            warn!("could not finalise {}: {e}", path.display());
        } else {
            info!("recording saved to {}", path.display());
        }
    }))
}

/// Minimal streaming WAV (RIFF, PCM) writer.
pub struct WavWriter {
    file: BufWriter<File>,
    data_bytes: u64,
}

impl WavWriter {
    pub fn create(path: &Path, channels: u16, rate: u32, bits: u16) -> std::io::Result<Self> {
        let mut file = BufWriter::new(File::create(path)?);
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
        Ok(Self { file, data_bytes: 0 })
    }

    pub fn write_frames(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if self.data_bytes + bytes.len() as u64 > u32::MAX as u64 - 36 {
            return Err(std::io::Error::other("WAV file size limit (4 GiB) reached"));
        }
        self.file.write_all(bytes)?;
        self.data_bytes += bytes.len() as u64;
        Ok(())
    }

    /// Writes the sizes into the header; writing may continue afterwards.
    pub fn finalize(&mut self) -> std::io::Result<()> {
        self.file.flush()?;
        let f = self.file.get_mut();
        let end = f.stream_position()?;
        f.seek(SeekFrom::Start(4))?;
        f.write_all(&(36 + self.data_bytes as u32).to_le_bytes())?;
        f.seek(SeekFrom::Start(40))?;
        f.write_all(&(self.data_bytes as u32).to_le_bytes())?;
        f.seek(SeekFrom::Start(end))?;
        f.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
