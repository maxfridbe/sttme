// Rust guideline compliant 2026-02-21
//! Microphone audio capture, 16 kHz resampling, WAV codecs, speech detection, and LCARS audio cues.
//!
//! Captures audio via `cpal` on a dedicated worker thread (falling back to `PipeWire`'s
//! `pw-record` if ALSA device initialization ever fails), converts arbitrary hardware channel
//! counts and sample rates to 16,000 Hz mono `f32` required by Whisper, and plays embedded
//! Star Trek LCARS audio cues (`computerbeep_50` on listen start, `computerbeep_48` when done).

use std::f32::consts::PI;
use std::io::{Cursor, Read};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use hound::{SampleFormat as WavSampleFormat, WavReader, WavSpec, WavWriter};
use tracing::{Level, event};

/// Target sample rate in Hz expected by `OpenAI` Whisper and `whisper.cpp`.
///
/// Whisper's log-Mel spectrogram front-end is hardcoded to 16,000 Hz; feeding unresampled
/// 48,000 Hz audio distorts pitch by 3x and breaks speech recognition.
pub const WHISPER_SAMPLE_RATE: u32 = 16_000;

/// Minimum recording duration in samples (`400ms` at 16 kHz = `6,400` samples).
///
/// Rejects accidental double-taps of `F12` while still capturing brisk one-word commands.
pub const MIN_SPEECH_SAMPLES: usize = 6_400;

/// Segment window in milliseconds used by [`has_speech`] RMS energy detection.
///
/// Matches `talktype.py`'s 50ms (`800` samples at 16 kHz) chunk size so short spoken
/// phrases surrounded by silence are not diluted by full-clip averaging.
const SPEECH_SEGMENT_MS: usize = 50;

/// Star Trek LCARS `computerbeep_50` cue played when starting to listen (`48 kHz` 16-bit mono WAV).
///
/// Sourced from `https://www.trekcore.com/audio/computer/computerbeep_50.mp3` and embedded as
/// RIFF PCM WAV so the single `sttme` executable plays it instantaneously with zero runtime codecs.
const BEEP_LISTENING_WAV: &[u8] = include_bytes!("../assets/computerbeep_50.wav");

/// Star Trek LCARS `computerbeep_48` cue played when STT processing completes (`48 kHz` 16-bit mono WAV).
///
/// Sourced from `https://www.trekcore.com/audio/computer/computerbeep_48.mp3` and embedded as
/// RIFF PCM WAV so the single `sttme` executable plays it instantaneously with zero runtime codecs.
const BEEP_PROCESSED_WAV: &[u8] = include_bytes!("../assets/computerbeep_48.wav");

/// Sample rate (`48,000 Hz`) used when synthesizing the fallback low-pitch error tone.
///
/// Matches standard `PipeWire` / ALSA desktop output rates to avoid unnecessary resampling.
const SYNTH_ERROR_SAMPLE_RATE: u32 = 48_000;

/// Frequency (`220 Hz`) of the subtle low tone played on cancelled or empty recordings.
///
/// Matches `talktype.py`'s error pitch so failure feedback is distinct from LCARS chirps.
const ERROR_BEEP_HZ: f32 = 220.0;

/// Duration (`180 ms`) of the subtle low tone played on cancelled or empty recordings.
///
/// Long enough to register audibly without delaying subsequent hotkey presses.
const ERROR_BEEP_SECONDS: f32 = 0.18;

/// Maximum amplitude (`0.12`) of the synthesized fallback error tone.
///
/// Matches `talktype.py`'s `volume = 0.12` so error feedback stays gentle on headphones.
const BEEP_VOLUME: f32 = 0.12;

/// Fade-in / fade-out ramp duration (`5ms`) applied to synthesized error tones.
///
/// Eliminates audible speaker/headphone pop caused by discontinuous step transitions at `t = 0`.
const BEEP_FADE_SECONDS: f32 = 0.005;

/// Extra drain margin (`50ms`) kept alive after the last sample is queued to `cpal`.
///
/// Prevents `Drop` of the output stream from truncating the tail of the LCARS cue inside
/// the ALSA / `PipeWire` hardware ring buffer.
const OUTPUT_DRAIN_MARGIN: Duration = Duration::from_millis(50);

/// Audio feedback cues played during recording and transcription lifecycle events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeepKind {
    /// Listening started (`computerbeep_50` LCARS cue).
    Start,
    /// Speech processed and delivered (`computerbeep_48` LCARS cue).
    Success,
    /// No speech, cancelled, or error (`220 Hz`, `180 ms` subtle low tone).
    Error,
}

/// Lock-free real-time microphone RMS meter shared with the Wayland waveform overlay.
#[derive(Debug, Default)]
pub struct WaveformMeter {
    peak_rms_bits: AtomicU32,
}

impl WaveformMeter {
    /// Creates a zeroed real-time audio level meter.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            peak_rms_bits: AtomicU32::new(0),
        }
    }

    /// Publishes an RMS reading from the real-time audio callback without locking.
    pub fn record_rms(&self, rms: f32) {
        if !rms.is_finite() || rms <= 0.0 {
            return;
        }
        let _ =
            self.peak_rms_bits
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |prev_bits| {
                    let prev = f32::from_bits(prev_bits);
                    Some(prev.max(rms).to_bits())
                });
    }

    /// Atomically reads and resets the peak RMS accumulated since the previous UI frame.
    #[must_use]
    pub fn take_rms(&self) -> f32 {
        f32::from_bits(self.peak_rms_bits.swap(0, Ordering::Relaxed))
    }
}

/// Thread-safe handle to an in-flight microphone recording session.
#[derive(Debug)]
pub struct ActiveCapture {
    backend: CaptureBackend,
    meter: Arc<WaveformMeter>,
}

#[derive(Debug)]
enum CaptureBackend {
    PwStream {
        child: Child,
        reader_handle: JoinHandle<Vec<f32>>,
    },
    Cpal {
        stop_tx: Sender<()>,
        join_handle: JoinHandle<Vec<f32>>,
    },
}

impl ActiveCapture {
    /// Starts recording audio from the system default microphone immediately.
    ///
    /// # Errors
    ///
    /// Returns an error if neither `pw-record` (`PipeWire`) nor `cpal` (ALSA) can open an input stream.
    pub fn start() -> Result<Self> {
        let meter = Arc::new(WaveformMeter::new());
        if let Ok(backend) = start_pw_stream_capture(&meter) {
            return Ok(Self { backend, meter });
        }
        let backend = start_cpal_capture(&meter)?;
        Ok(Self { backend, meter })
    }

    /// Returns a lock-free real-time RMS meter for driving the Wayland live waveform HUD.
    #[must_use]
    pub fn meter(&self) -> Arc<WaveformMeter> {
        Arc::clone(&self.meter)
    }

    /// Stops recording and returns mono `16,000 Hz` `f32` PCM samples in `[-1.0, 1.0]`.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying capture worker panicked.
    pub fn stop(self) -> Result<Vec<f32>> {
        match self.backend {
            CaptureBackend::PwStream {
                mut child,
                reader_handle,
            } => {
                drop(child.kill());
                drop(child.wait());
                reader_handle
                    .join()
                    .map_err(|_panic| anyhow::anyhow!("PipeWire reader thread panicked"))
            }
            CaptureBackend::Cpal {
                stop_tx,
                join_handle,
            } => {
                let _ = stop_tx.send(());
                join_handle
                    .join()
                    .map_err(|_panic| anyhow::anyhow!("audio capture worker thread panicked"))
            }
        }
    }
}

fn start_cpal_capture(meter: &Arc<WaveformMeter>) -> Result<CaptureBackend> {
    let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let meter_clone = Arc::clone(meter);

    let join_handle = thread::Builder::new()
        .name("sttme-audio-in".to_owned())
        .spawn(move || run_cpal_input_worker(&ready_tx, &stop_rx, &meter_clone))
        .context("failed to spawn audio capture thread")?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(CaptureBackend::Cpal {
            stop_tx,
            join_handle,
        }),
        Ok(Err(err)) => {
            let _ = join_handle.join();
            Err(err)
        }
        Err(_) => {
            let _ = join_handle.join();
            bail!("audio capture thread exited unexpectedly during initialization")
        }
    }
}

fn run_cpal_input_worker(
    ready_tx: &Sender<Result<()>>,
    stop_rx: &mpsc::Receiver<()>,
    meter: &Arc<WaveformMeter>,
) -> Vec<f32> {
    let host = cpal::default_host();
    let Some(device) = host.default_input_device() else {
        let _ = ready_tx.send(Err(anyhow::anyhow!("no default microphone detected")));
        return Vec::new();
    };

    let supported = match device.default_input_config() {
        Ok(cfg) => cfg,
        Err(err) => {
            let _ = ready_tx.send(Err(anyhow::Error::new(err)));
            return Vec::new();
        }
    };

    let sample_format = supported.sample_format();
    let config: StreamConfig = supported.into();
    let in_rate = config.sample_rate;
    let channels = usize::from(config.channels.max(1));
    let raw_mono: Arc<Mutex<Vec<f32>>> =
        Arc::new(Mutex::new(Vec::with_capacity((in_rate as usize) * 10)));

    let stream =
        match build_cpal_input_stream(&device, sample_format, config, channels, &raw_mono, meter) {
            Ok(s) => s,
            Err(err) => {
                let _ = ready_tx.send(Err(err));
                return Vec::new();
            }
        };

    if let Err(err) = stream.play() {
        let _ = ready_tx.send(Err(anyhow::Error::new(err)));
        return Vec::new();
    }

    let _ = ready_tx.send(Ok(()));
    let _ = stop_rx.recv();
    drop(stream);

    let captured = raw_mono
        .lock()
        .map(|mut guard| std::mem::take(&mut *guard))
        .unwrap_or_default();
    resample_linear(&captured, in_rate, WHISPER_SAMPLE_RATE)
}

fn build_cpal_input_stream(
    device: &cpal::Device,
    sample_format: SampleFormat,
    config: StreamConfig,
    channels: usize,
    raw_mono: &Arc<Mutex<Vec<f32>>>,
    meter: &Arc<WaveformMeter>,
) -> Result<cpal::Stream> {
    let raw_clone = Arc::clone(raw_mono);
    let meter_clone = Arc::clone(meter);
    let err_fn = |err| {
        event!(
            name: "audio.stream.error",
            Level::WARN,
            error.message = %err,
            "audio input stream error: {{error.message}}",
        );
    };

    let stream_res = match sample_format {
        SampleFormat::F32 => device.build_input_stream(
            config,
            move |data: &[f32], _| push_mono_f32(&raw_clone, &meter_clone, data, channels),
            err_fn,
            None,
        ),
        SampleFormat::I16 => device.build_input_stream(
            config,
            move |data: &[i16], _| {
                push_mono_mapped(&raw_clone, &meter_clone, data, channels, |s| {
                    f32::from(s) / 32_768.0
                });
            },
            err_fn,
            None,
        ),
        SampleFormat::I32 => device.build_input_stream(
            config,
            move |data: &[i32], _| {
                push_mono_mapped(&raw_clone, &meter_clone, data, channels, |s| {
                    (s as f32) / 2_147_483_648.0
                });
            },
            err_fn,
            None,
        ),
        SampleFormat::U16 => device.build_input_stream(
            config,
            move |data: &[u16], _| {
                push_mono_mapped(&raw_clone, &meter_clone, data, channels, |s| {
                    (f32::from(s) - 32_768.0) / 32_768.0
                });
            },
            err_fn,
            None,
        ),
        other => bail!("unsupported microphone sample format: {other:?}"),
    };

    stream_res.map_err(anyhow::Error::new)
}

fn start_pw_stream_capture(meter: &Arc<WaveformMeter>) -> Result<CaptureBackend> {
    let mut child = Command::new("pw-record")
        .args([
            "--raw",
            "--rate",
            "16000",
            "--channels",
            "1",
            "--format",
            "s16",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn `pw-record`")?;

    let mut stdout = child
        .stdout
        .take()
        .context("missing stdout pipe on `pw-record`")?;
    let meter_clone = Arc::clone(meter);

    let reader_handle = thread::Builder::new()
        .name("sttme-pw-in".to_owned())
        .spawn(move || {
            let mut samples = Vec::with_capacity((WHISPER_SAMPLE_RATE as usize) * 10);
            // 320 bytes = 160 i16 samples = 10ms frame at 16,000 Hz for responsive 50 FPS metering.
            let mut raw_buf = [0_u8; 640];
            let mut carry_byte: Option<u8> = None;

            while let Ok(read_n) = stdout.read(&mut raw_buf) {
                if read_n == 0 {
                    break;
                }
                let start_len = samples.len();
                let mut slice = &raw_buf[..read_n];
                if let Some(prev) = carry_byte.take()
                    && let Some((&first, rest)) = slice.split_first()
                {
                    let sample = i16::from_le_bytes([prev, first]);
                    samples.push(f32::from(sample) / 32_768.0);
                    slice = rest;
                }
                let (pairs, rem) = slice.as_chunks::<2>();
                for &pair in pairs {
                    let sample = i16::from_le_bytes(pair);
                    samples.push(f32::from(sample) / 32_768.0);
                }
                if let Some(&leftover) = rem.first() {
                    carry_byte = Some(leftover);
                }
                update_meter_from_slice(&meter_clone, &samples[start_len..]);
            }
            samples
        })
        .context("failed to spawn PipeWire audio reader thread")?;

    Ok(CaptureBackend::PwStream {
        child,
        reader_handle,
    })
}

fn push_mono_f32(buf: &Mutex<Vec<f32>>, meter: &WaveformMeter, data: &[f32], channels: usize) {
    let Ok(mut guard) = buf.lock() else {
        return;
    };
    let start_len = guard.len();
    if channels == 1 {
        guard.extend_from_slice(data);
    } else {
        let inv = 1.0 / (channels as f32);
        for frame in data.chunks_exact(channels) {
            let sum: f32 = frame.iter().copied().sum();
            guard.push(sum * inv);
        }
    }
    update_meter_from_slice(meter, &guard[start_len..]);
}

fn push_mono_mapped<T: Copy>(
    buf: &Mutex<Vec<f32>>,
    meter: &WaveformMeter,
    data: &[T],
    channels: usize,
    to_f32: impl Fn(T) -> f32,
) {
    let Ok(mut guard) = buf.lock() else {
        return;
    };
    let start_len = guard.len();
    let inv = 1.0 / (channels as f32);
    for frame in data.chunks_exact(channels) {
        let mut sum = 0.0_f32;
        for &sample in frame {
            sum += to_f32(sample);
        }
        guard.push(sum * inv);
    }
    update_meter_from_slice(meter, &guard[start_len..]);
}

fn update_meter_from_slice(meter: &WaveformMeter, mono_slice: &[f32]) {
    if mono_slice.is_empty() {
        return;
    }
    let mut sum_sq = 0.0_f32;
    let mut peak = 0.0_f32;
    for &sample in mono_slice {
        sum_sq += sample * sample;
        peak = peak.max(sample.abs());
    }
    let rms = (sum_sq / (mono_slice.len() as f32)).sqrt();
    // Blend RMS energy with transient peak amplitude so speech syllables visibly pop.
    meter.record_rms(0.5 * rms + 0.5 * peak);
}

/// Resamples mono `f32` audio from `in_rate` Hz to `out_rate` Hz with anti-aliased box/linear filtering.
#[must_use]
pub fn resample_linear(samples: &[f32], in_rate: u32, out_rate: u32) -> Vec<f32> {
    if samples.is_empty() || in_rate == 0 || out_rate == 0 {
        return Vec::new();
    }
    if in_rate == out_rate {
        return samples.to_vec();
    }
    // Fast path for exact integer decimation (e.g., 48,000 Hz -> 16,000 Hz is 3:1).
    if in_rate.is_multiple_of(out_rate) {
        let factor = (in_rate / out_rate) as usize;
        let inv = 1.0 / (factor as f32);
        return samples
            .chunks_exact(factor)
            .map(|chunk| chunk.iter().copied().sum::<f32>() * inv)
            .collect();
    }

    let ratio = f64::from(in_rate) / f64::from(out_rate);
    let out_len = ((samples.len() as f64) / ratio).floor() as usize;
    let mut output = Vec::with_capacity(out_len);
    let max_idx = samples.len().saturating_sub(1);

    for i in 0..out_len {
        let src_pos = (i as f64) * ratio;
        let idx0 = (src_pos.floor() as usize).min(max_idx);
        let idx1 = (idx0 + 1).min(max_idx);
        let frac = (src_pos - (idx0 as f64)) as f32;
        let interpolated = samples[idx0] + (samples[idx1] - samples[idx0]) * frac;
        output.push(interpolated);
    }
    output
}

/// Checks whether 16 kHz mono audio contains speech energy above `threshold`.
///
/// Evaluates 50ms (`800`-sample) windows independently so short spoken phrases
/// surrounded by leading or trailing silence are accurately detected.
#[must_use]
pub fn has_speech(samples_16k: &[f32], threshold: f32) -> bool {
    let segment_samples = (WHISPER_SAMPLE_RATE as usize * SPEECH_SEGMENT_MS) / 1000;
    let min_tail = segment_samples / 2;

    for chunk in samples_16k.chunks(segment_samples) {
        if chunk.len() < min_tail {
            continue;
        }
        let mean_sq = chunk.iter().map(|&s| s * s).sum::<f32>() / (chunk.len() as f32);
        if mean_sq.sqrt() > threshold {
            return true;
        }
    }
    false
}

/// Encodes mono `f32` PCM samples at `sample_rate` Hz into a 16-bit PCM RIFF WAV byte buffer.
///
/// # Errors
///
/// Returns an error if in-memory WAV serialization fails.
pub fn encode_wav_mono(samples: &[f32], sample_rate: u32) -> Result<Vec<u8>> {
    let spec = WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: WavSampleFormat::Int,
    };
    let mut cursor = Cursor::new(Vec::with_capacity(44 + samples.len() * 2));
    let mut writer = WavWriter::new(&mut cursor, spec).context("failed to init WAV writer")?;
    for &sample in samples {
        let clamped = sample.clamp(-1.0, 1.0);
        let quantized = (clamped * 32_767.0) as i16;
        writer
            .write_sample(quantized)
            .context("failed to write WAV sample")?;
    }
    writer.finalize().context("failed to finalize WAV buffer")?;
    Ok(cursor.into_inner())
}

/// Encodes mono `16,000 Hz` `f32` PCM samples into a 16-bit PCM RIFF WAV byte buffer.
///
/// # Errors
///
/// Returns an error if in-memory WAV serialization fails.
pub fn encode_wav_16k(samples_16k: &[f32]) -> Result<Vec<u8>> {
    encode_wav_mono(samples_16k, WHISPER_SAMPLE_RATE)
}

/// Decodes arbitrary WAV bytes into `(mono_f32_samples, native_sample_rate)`.
///
/// # Errors
///
/// Returns an error if `wav_bytes` is not a valid PCM/IEEE-float RIFF WAV stream.
pub fn decode_wav_mono_f32(wav_bytes: &[u8]) -> Result<(Vec<f32>, u32)> {
    let reader = WavReader::new(Cursor::new(wav_bytes)).context("invalid WAV header")?;
    let spec = reader.spec();
    let channels = usize::from(spec.channels.max(1));
    let in_rate = spec.sample_rate;

    let interleaved: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
        (WavSampleFormat::Float, _) => reader
            .into_samples::<f32>()
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("failed to read float WAV samples")?,
        (WavSampleFormat::Int, 8) => reader
            .into_samples::<i8>()
            .map(|r| r.map(|v| f32::from(v) / 128.0))
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("failed to read 8-bit WAV samples")?,
        (WavSampleFormat::Int, 16) => reader
            .into_samples::<i16>()
            .map(|r| r.map(|v| f32::from(v) / 32_768.0))
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("failed to read 16-bit WAV samples")?,
        (WavSampleFormat::Int, bits) => {
            let scale = ((1_i64 << (bits.saturating_sub(1))) as f32).max(1.0);
            reader
                .into_samples::<i32>()
                .map(|r| r.map(|v| (v as f32) / scale))
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("failed to read integer WAV samples")?
        }
    };

    let mono: Vec<f32> = if channels == 1 {
        interleaved
    } else {
        let inv = 1.0 / (channels as f32);
        interleaved
            .chunks_exact(channels)
            .map(|frame| frame.iter().copied().sum::<f32>() * inv)
            .collect()
    };

    Ok((mono, in_rate))
}

/// Decodes arbitrary WAV bytes into mono `16,000 Hz` `f32` PCM samples.
///
/// # Errors
///
/// Returns an error if `wav_bytes` is not a valid PCM/IEEE-float RIFF WAV stream.
pub fn decode_wav_to_16k_f32(wav_bytes: &[u8]) -> Result<Vec<f32>> {
    let (mono, in_rate) = decode_wav_mono_f32(wav_bytes)?;
    Ok(resample_linear(&mono, in_rate, WHISPER_SAMPLE_RATE))
}

/// Plays a non-blocking audio feedback cue on the default output device.
pub fn play_beep(kind: BeepKind) {
    drop(
        thread::Builder::new()
            .name("sttme-beep".to_owned())
            .spawn(move || {
                if play_cue_pipewire(kind) {
                    return;
                }
                let (samples, src_rate) = load_cue_samples(kind);
                if !samples.is_empty() && play_samples_cpal(&samples, src_rate).is_err() {
                    play_samples_pw_fallback(&samples, src_rate);
                }
            }),
    );
}

fn play_cue_pipewire(kind: BeepKind) -> bool {
    let wav_cow: std::borrow::Cow<'static, [u8]> = match kind {
        BeepKind::Start => std::borrow::Cow::Borrowed(BEEP_LISTENING_WAV),
        BeepKind::Success => std::borrow::Cow::Borrowed(BEEP_PROCESSED_WAV),
        BeepKind::Error => {
            let Ok(encoded) = encode_wav_mono(&synthesize_error_tone(), SYNTH_ERROR_SAMPLE_RATE)
            else {
                return false;
            };
            std::borrow::Cow::Owned(encoded)
        }
    };

    let base_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map_or_else(std::env::temp_dir, std::path::PathBuf::from);
    let wav_path = base_dir.join(format!(
        "sttme-cue-{}-{:?}.wav",
        std::process::id(),
        thread::current().id()
    ));
    if std::fs::write(&wav_path, wav_cow.as_ref()).is_err() {
        return false;
    }
    let ok = Command::new("pw-play")
        .arg(&wav_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    drop(std::fs::remove_file(&wav_path));
    ok
}

fn load_cue_samples(kind: BeepKind) -> (Vec<f32>, u32) {
    match kind {
        BeepKind::Start => decode_wav_mono_f32(BEEP_LISTENING_WAV).unwrap_or_default(),
        BeepKind::Success => decode_wav_mono_f32(BEEP_PROCESSED_WAV).unwrap_or_default(),
        BeepKind::Error => (synthesize_error_tone(), SYNTH_ERROR_SAMPLE_RATE),
    }
}

fn synthesize_error_tone() -> Vec<f32> {
    let rate = SYNTH_ERROR_SAMPLE_RATE as f32;
    let total_frames = (rate * ERROR_BEEP_SECONDS) as usize;
    let fade_frames = ((rate * BEEP_FADE_SECONDS) as usize).max(1);
    let mut out = Vec::with_capacity(total_frames);

    for idx in 0..total_frames {
        let envelope = if idx < fade_frames {
            0.5 * (1.0 - (PI * (idx as f32) / (fade_frames as f32)).cos())
        } else if idx + fade_frames > total_frames {
            let rem = total_frames.saturating_sub(idx);
            0.5 * (1.0 - (PI * (rem as f32) / (fade_frames as f32)).cos())
        } else {
            1.0
        };
        let t = (idx as f32) / rate;
        out.push(BEEP_VOLUME * envelope * (2.0 * PI * ERROR_BEEP_HZ * t).sin());
    }
    out
}

fn play_samples_cpal(samples: &[f32], src_rate: u32) -> Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("no default audio output device")?;
    let supported = device
        .default_output_config()
        .context("no default audio output config")?;

    let sample_format = supported.sample_format();
    let config: StreamConfig = supported.into();
    let out_rate = config.sample_rate;
    let channels = usize::from(config.channels.max(1));
    let resampled = resample_linear(samples, src_rate, out_rate);
    if resampled.is_empty() {
        return Ok(());
    }

    let duration_secs = (resampled.len() as f32) / (out_rate as f32);
    let stream = build_cpal_output_stream(&device, sample_format, config, channels, resampled)?;
    stream.play().context("failed to play cpal output stream")?;
    thread::sleep(Duration::from_secs_f32(duration_secs) + OUTPUT_DRAIN_MARGIN);
    Ok(())
}

fn build_cpal_output_stream(
    device: &cpal::Device,
    sample_format: SampleFormat,
    config: StreamConfig,
    channels: usize,
    resampled: Vec<f32>,
) -> Result<cpal::Stream> {
    let mut cursor_frame = 0_usize;
    let mut next_sample = move || -> f32 {
        let val = resampled.get(cursor_frame).copied().unwrap_or(0.0);
        cursor_frame = cursor_frame.saturating_add(1);
        val.clamp(-1.0, 1.0)
    };
    let err_fn = |_| {};

    let stream_res = match sample_format {
        SampleFormat::F32 => device.build_output_stream(
            config,
            move |out: &mut [f32], _| {
                for frame in out.chunks_mut(channels) {
                    frame.fill(next_sample());
                }
            },
            err_fn,
            None,
        ),
        SampleFormat::I16 => device.build_output_stream(
            config,
            move |out: &mut [i16], _| {
                for frame in out.chunks_mut(channels) {
                    frame.fill((next_sample() * 32_767.0) as i16);
                }
            },
            err_fn,
            None,
        ),
        SampleFormat::I32 => device.build_output_stream(
            config,
            move |out: &mut [i32], _| {
                for frame in out.chunks_mut(channels) {
                    frame.fill((f64::from(next_sample()) * 2_147_483_647.0) as i32);
                }
            },
            err_fn,
            None,
        ),
        SampleFormat::U16 => device.build_output_stream(
            config,
            move |out: &mut [u16], _| {
                for frame in out.chunks_mut(channels) {
                    frame.fill(((next_sample() + 1.0) * 32_767.5) as u16);
                }
            },
            err_fn,
            None,
        ),
        other => bail!("unsupported output sample format: {other:?}"),
    };

    stream_res.map_err(anyhow::Error::new)
}

fn play_samples_pw_fallback(samples: &[f32], src_rate: u32) {
    let Ok(wav_bytes) = encode_wav_mono(samples, src_rate) else {
        return;
    };
    let wav_path = std::env::temp_dir().join(format!(
        "sttme-cue-{}-{:?}.wav",
        std::process::id(),
        thread::current().id()
    ));
    if std::fs::write(&wav_path, wav_bytes).is_ok() {
        let played = Command::new("pw-play")
            .arg(&wav_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !played {
            drop(
                Command::new("paplay")
                    .arg(&wav_path)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status(),
            );
        }
        drop(std::fs::remove_file(&wav_path));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_decimates_48k_to_16k_accurately() {
        let input_48k = vec![0.5_f32; 48_000];
        let out_16k = resample_linear(&input_48k, 48_000, 16_000);
        assert_eq!(out_16k.len(), 16_000);
        assert!((out_16k[0] - 0.5).abs() < 1e-5);
    }

    #[test]
    fn wav_roundtrip_preserves_speech_energy() {
        let samples: Vec<f32> = (0..16_000)
            .map(|i| 0.25 * (2.0 * PI * 440.0 * (i as f32) / 16_000.0).sin())
            .collect();
        assert!(has_speech(&samples, 0.01));

        let wav = encode_wav_16k(&samples).expect("encode wav");
        let decoded = decode_wav_to_16k_f32(&wav).expect("decode wav");
        assert_eq!(decoded.len(), samples.len());
        assert!(has_speech(&decoded, 0.01));

        let silence = vec![0.0001_f32; 16_000];
        assert!(!has_speech(&silence, 0.01));
    }

    #[test]
    fn embedded_trekcore_cues_decode_cleanly() {
        let (listen_pcm, listen_rate) =
            decode_wav_mono_f32(BEEP_LISTENING_WAV).expect("decode computerbeep_50.wav");
        assert_eq!(listen_rate, 48_000);
        assert!(listen_pcm.len() > 30_000);
        assert!(listen_pcm.iter().any(|&s| s.abs() > 0.05));

        let (done_pcm, done_rate) =
            decode_wav_mono_f32(BEEP_PROCESSED_WAV).expect("decode computerbeep_48.wav");
        assert_eq!(done_rate, 48_000);
        assert!(done_pcm.len() > 30_000);
        assert!(done_pcm.iter().any(|&s| s.abs() > 0.05));
    }
}
