//! Microphone capture and Opus-in-Ogg encoding for voice-note recording.
//!
//! Everything here is provider-neutral: recording produces raw mono f32 PCM,
//! [`encode_opus_ogg`] wraps it in a gapless Ogg Opus container, and
//! [`waveform_from_pcm`] derives the preview peaks both providers show. The
//! send paths in later phases only ever see the encoded bytes and the
//! waveform.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use opus_pure::{Application, MAX_PACKET_BYTES, OggOpusWriter, OpusEncoder, OpusHead, OpusTags};

/// Longest recording allowed, in seconds. The capture callback discards
/// anything past this so a stuck key cannot grow the buffer forever.
pub const MAX_CLIP_SECS: u32 = 120;

/// Preferred capture rate. Every Opus rate divides 48 kHz, so resampling by the
/// hardware at this rate keeps the encode step trivial and gapless.
pub const CAPTURE_RATE: u32 = 48_000;

/// The only input rates Opus can encode (RFC 6716 §2). Ordered most-preferred
/// first; the recorder picks the topmost rate a device supports.
const OPUS_RATES: [u32; 5] = [CAPTURE_RATE, 24_000, 16_000, 12_000, 8_000];

/// Errors from recording and encoding. These belong to the audio module, not
/// the messaging boundary: providers never see PCM or raw Opus errors.
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no audio input device available")]
    NoInputDevice,
    #[error("opening the input stream failed: {0}")]
    Open(#[from] cpal::BuildStreamError),
    #[error("starting the input stream failed: {0}")]
    Start(#[from] cpal::PlayStreamError),
    #[error("default input config failed: {0}")]
    DefaultConfig(#[from] cpal::DefaultStreamConfigError),
    #[error("sample rate {0} Hz is not supported by Opus (8/12/16/24/48 kHz)")]
    UnsupportedSampleRate(u32),
    #[error("Opus supports only 1 or 2 channels, got {0}")]
    UnsupportedChannels(u16),
    #[error("{0}")]
    Invalid(&'static str),
    #[error("nothing was recorded")]
    EmptyRecording,
    #[error("opus encode failed: {0}")]
    Opus(#[from] opus_pure::Error),
}

/// Live microphone capture.
///
/// Opens the host's default input device, picks a config at an Opus-supported
/// rate (preferring 48 kHz mono f32) and mixes every buffer down to mono f32 in
/// the capture callback. The stream runs until the `Recorder` is dropped;
/// [`finish`](Self::finish) returns everything captured so far and clears the
/// buffer.
///
/// Cannot be constructed or tested without a real audio device; the offline
/// tests cover the pure helpers instead.
pub struct Recorder {
    #[expect(dead_code, reason = "kept alive for the lifetime of the stream")]
    stream: cpal::Stream,
    shared: Arc<Shared>,
}

/// State shared between the capture callback thread and the UI thread.
struct Shared {
    /// Captured mono f32 frames, bounded by `max_frames`.
    frames: Mutex<Vec<f32>>,
    /// Most recent RMS level, as an `f32` bit pattern, for the recording
    /// indicator. `AtomicU32` because `f32` is not `Atomic` on all targets.
    level: AtomicU32,
    sample_rate: u32,
    channels: u16,
    max_frames: usize,
}

impl Recorder {
    pub fn start() -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or(AudioError::NoInputDevice)?;
        let (stream_config, sample_format) = pick_input_config(&device)?;

        let shared = Arc::new(Shared {
            frames: Mutex::new(Vec::new()),
            level: AtomicU32::new(0),
            sample_rate: stream_config.sample_rate.0,
            channels: stream_config.channels,
            max_frames: MAX_CLIP_SECS as usize * stream_config.sample_rate.0 as usize,
        });

        let capture = Arc::clone(&shared);
        let stream = device.build_input_stream_raw(
            &stream_config,
            sample_format,
            move |data, _info| push_capture(data, &capture),
            move |err| tracing::warn!(%err, "audio input stream error"),
            None,
        )?;
        stream.play()?;

        Ok(Recorder { stream, shared })
    }

    /// How long the recording is so far.
    pub fn duration(&self) -> Duration {
        let frames = self.shared.frames.lock().unwrap().len();
        Duration::from_secs_f64(frames as f64 / self.shared.sample_rate as f64)
    }

    /// Most recent captured RMS level, roughly 0.0..=1.0, for a live meter.
    pub fn rms(&self) -> f32 {
        f32::from_bits(self.shared.level.load(Ordering::Relaxed))
    }

    /// The sample rate the capture was opened at an Opus-supported rate.
    pub fn sample_rate(&self) -> u32 {
        self.shared.sample_rate
    }

    /// Stop the capture and return the mono f32 frames recorded so far.
    pub fn finish(&mut self) -> Vec<f32> {
        let mut frames = self.shared.frames.lock().unwrap();
        std::mem::take(&mut *frames)
    }
}

/// The capture surface the push-to-talk state machine drives. `Recorder` is
/// the hardware-backed impl; the state tests inject a fake instead of opening
/// a real mic, so the press/release transitions run fully offline.
pub trait MicCapture: Send {
    /// How long the recording is so far.
    fn duration(&self) -> Duration;
    /// Most recent captured RMS level, roughly 0.0..=1.0, for a live meter.
    fn rms(&self) -> f32;
    /// The sample rate the capture was opened at.
    fn sample_rate(&self) -> u32;
    /// Stop the capture and return the mono f32 frames recorded so far.
    fn finish(&mut self) -> Vec<f32>;
}

impl MicCapture for Recorder {
    fn duration(&self) -> Duration {
        Recorder::duration(self)
    }
    fn rms(&self) -> f32 {
        Recorder::rms(self)
    }
    fn sample_rate(&self) -> u32 {
        Recorder::sample_rate(self)
    }
    fn finish(&mut self) -> Vec<f32> {
        Recorder::finish(self)
    }
}

/// Push one capture buffer into the shared state: convert to f32, mix down,
/// append, enforce the length cap and refresh the live level.
fn push_capture(data: &cpal::Data, shared: &Shared) {
    let samples: Vec<f32> = match data.sample_format() {
        cpal::SampleFormat::F32 => collect_f32::<f32>(data),
        cpal::SampleFormat::I8 => collect_f32::<i8>(data),
        cpal::SampleFormat::I16 => collect_f32::<i16>(data),
        cpal::SampleFormat::I24 => collect_f32::<cpal::I24>(data),
        cpal::SampleFormat::I32 => collect_f32::<i32>(data),
        cpal::SampleFormat::I64 => collect_f32::<i64>(data),
        cpal::SampleFormat::U8 => collect_f32::<u8>(data),
        cpal::SampleFormat::U16 => collect_f32::<u16>(data),
        cpal::SampleFormat::U32 => collect_f32::<u32>(data),
        cpal::SampleFormat::U64 => collect_f32::<u64>(data),
        cpal::SampleFormat::F64 => collect_f32::<f64>(data),
        // `SampleFormat` is non-exhaustive (hosts may add formats); skip
        // buffers we cannot convert.
        _ => Vec::new(),
    };

    if samples.is_empty() {
        return;
    }

    let frames = mixdown_mono(&samples, shared.channels as usize);
    if frames.is_empty() {
        return;
    }

    let mut guard = shared.frames.lock().unwrap();
    guard.extend_from_slice(&frames);

    if guard.len() > shared.max_frames {
        guard.truncate(shared.max_frames);
    }

    let mean_sq = frames.iter().map(|s| s * s).sum::<f32>() / frames.len() as f32;
    shared
        .level
        .store(mean_sq.sqrt().to_bits(), Ordering::Relaxed);
}

/// Convert a capture buffer to f32 samples, using the sample type `T` the host
/// reported. dasp's `Sample` covers every format cpal exposes, so the callbacks
/// for integer PCM use the same normalization the reference players do.
fn collect_f32<T>(data: &cpal::Data) -> Vec<f32>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    data.as_slice::<T>()
        .map(|samples| samples.iter().map(|&s| s.to_sample::<f32>()).collect())
        .unwrap_or_default()
}

/// Pick the best input config for Opus encoding: the topmost supported rate
/// from [`OPUS_RATES`], preferring mono and f32. Falls back to the device's
/// default config when no enumerated range covers an Opus rate.
fn pick_input_config(
    device: &cpal::Device,
) -> Result<(cpal::StreamConfig, cpal::SampleFormat), AudioError> {
    let mut best: Option<((bool, bool, u16), cpal::StreamConfig, cpal::SampleFormat)> = None;
    if let Ok(configs) = device.supported_input_configs() {
        for range in configs {
            let Some(rate) = preferred_opus_rate(&range) else {
                continue;
            };
            let Some(cfg) = range.try_with_sample_rate(cpal::SampleRate(rate)) else {
                continue;
            };
            let score = (
                range.channels() != 1,
                range.sample_format() != cpal::SampleFormat::F32,
                range.channels(),
            );
            if best.as_ref().is_none_or(|(s, _, _)| score < *s) {
                best = Some((score, cfg.config(), range.sample_format()));
            }
        }
    }
    if let Some((_, config, format)) = best {
        return Ok((config, format));
    }

    let fallback = device.default_input_config()?;

    if !OPUS_RATES.contains(&fallback.sample_rate().0) {
        return Err(AudioError::UnsupportedSampleRate(fallback.sample_rate().0));
    }

    Ok((fallback.config(), fallback.sample_format()))
}

/// The highest-priority Opus rate a config range can deliver, or `None` if it
/// cannot deliver any (e.g. a device locked to 44.1 kHz).
fn preferred_opus_rate(range: &cpal::SupportedStreamConfigRange) -> Option<u32> {
    OPUS_RATES
        .iter()
        .copied()
        .find(|rate| range.min_sample_rate().0 <= *rate && *rate <= range.max_sample_rate().0)
}

/// Encode mono or stereo f32 PCM as a gapless Ogg Opus stream.
///
/// The end-trim arithmetic mirrors `opus-pure`'s `examples/encode.rs`: the
/// final granule position claims exactly the input audio, so a player stops
/// where the recording did rather than a frame late. Output is stored totally
/// in memory — voice notes are short and this avoids a temp file.
pub fn encode_opus_ogg(
    pcm: &[f32],
    sample_rate: u32,
    channels: u16,
) -> Result<Vec<u8>, AudioError> {
    let channels = channels as usize;
    if channels != 1 && channels != 2 {
        return Err(AudioError::UnsupportedChannels(channels as u16));
    }
    if !OPUS_RATES.contains(&sample_rate) {
        return Err(AudioError::UnsupportedSampleRate(sample_rate));
    }
    if pcm.is_empty() {
        return Err(AudioError::EmptyRecording);
    }
    if !pcm.len().is_multiple_of(channels) {
        return Err(AudioError::Invalid(
            "pcm length is not a whole number of sample frames",
        ));
    }

    let rate = sample_rate as i32;
    let frame = (rate / 50) as usize; // 20 ms frames
    let per_frame = frame * channels;

    let mut encoder = OpusEncoder::new(rate, channels, Application::Audio)?;
    encoder.bitrate_bps = 64_000;

    let mut tags = OpusTags::new();
    tags.push("ENCODER", env!("CARGO_PKG_NAME"))?;

    let head = OpusHead::for_encoder(&encoder, sample_rate);

    // ---- Ending the file exactly (RFC 7845 §4.2 and §4.4) ----
    // Every Opus rate divides 48 kHz, so one encoder-rate sample is this many
    // granule ticks and the conversions below are exact.
    let ticks = 48_000 / rate as usize;
    let total = pcm.len() / channels; // sample frames of real audio
    // 1. The encoder runs `pre_skip` samples behind its input; feeding that
    //    much extra silence flushes them. Round up to a whole frame.
    let lookahead = usize::from(head.pre_skip).div_ceil(ticks);
    let frames = (total + lookahead).div_ceil(frame);
    // 2. The padding decodes as real output, so the file has to say where the
    //    audio stopped: the pre-skip plus the audio, nothing else.
    let final_granule = u64::from(head.pre_skip) + (total * ticks) as u64;

    let mut writer = OggOpusWriter::with_tags(Vec::new(), head, tags)?;
    let mut packet = vec![0u8; MAX_PACKET_BYTES];
    let mut block = vec![0.0f32; per_frame];

    for i in 0..frames {
        // Whole frames only: past the end of the input the block is silence,
        // which is the padding that flushes the encoder's delay.
        let start = (i * per_frame).min(pcm.len());
        let end = (start + per_frame).min(pcm.len());
        block[..end - start].copy_from_slice(&pcm[start..end]);
        block[end - start..].fill(0.0);

        let n = encoder.encode(&block, frame, &mut packet)?;
        if i + 1 == frames {
            let duration = final_granule - u64::try_from(writer.granule()).expect("granule >= 0");
            writer.write_packet_with_duration(&packet[..n], duration as u32)?;
        } else {
            writer.write_packet(&packet[..n])?;
        }
    }

    Ok(writer.finish()?)
}

/// Average interleaved frames down to mono. A trailing partial frame is
/// dropped; buffers from cpal are always frame-aligned anyway.
fn mixdown_mono(samples: &[f32], channels: usize) -> Vec<f32> {
    let mut frames = Vec::with_capacity(samples.len() / channels);

    for frame in samples.chunks_exact(channels) {
        let sum: f32 = frame.iter().sum();
        frames.push(sum / channels as f32);
    }

    frames
}

/// Peak amplitude per bucket, scaled to the 0..=127 range both providers use
/// for voice-note waveforms. Always yields exactly `buckets` entries; a clip
/// shorter than the bucket count repeats its tail peaks.
pub fn waveform_from_pcm(pcm: &[f32], buckets: usize) -> Vec<u8> {
    if pcm.is_empty() || buckets == 0 {
        return Vec::new();
    }
    (0..buckets)
        .map(|b| {
            let start = b * pcm.len() / buckets;
            let end = (b + 1) * pcm.len() / buckets;
            let peak = pcm[start..end.max(start + 1)]
                .iter()
                .map(|s| s.abs())
                .fold(0.0f32, f32::max);
            (peak * 127.0).round().clamp(0.0, 127.0) as u8
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        AudioError, encode_opus_ogg, mixdown_mono, preferred_opus_rate, waveform_from_pcm,
    };
    use opus_pure::OggOpusReader;
    use std::io::Cursor;

    /// A sine tone: `channels`-wide interleaved f32 frames at `rate`.
    fn tone(rate: u32, seconds: f32, channels: usize) -> Vec<f32> {
        let total = (rate as f32 * seconds) as usize;
        (0..total * channels)
            .map(|i| {
                let f = i / channels;
                (f as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 0.5
            })
            .collect()
    }

    #[test]
    fn rejects_non_opus_sample_rate() {
        assert!(matches!(
            encode_opus_ogg(&tone(44_100, 0.1, 1), 44_100, 1),
            Err(AudioError::UnsupportedSampleRate(44_100))
        ));
    }

    #[test]
    fn rejects_empty_pcm() {
        assert!(matches!(
            encode_opus_ogg(&[], 48_000, 1),
            Err(AudioError::EmptyRecording)
        ));
    }

    #[test]
    fn rejects_more_than_two_channels() {
        assert!(matches!(
            encode_opus_ogg(&tone(48_000, 0.1, 3), 48_000, 3),
            Err(AudioError::UnsupportedChannels(3))
        ));
    }

    #[test]
    fn rejects_partial_sample_frame() {
        let pcm = tone(48_000, 0.1, 2);
        assert!(matches!(
            encode_opus_ogg(&pcm[..pcm.len() - 1], 48_000, 2),
            Err(AudioError::Invalid(_))
        ));
    }

    #[test]
    fn encodes_mono_audio_gaplessly() {
        let rate = 48_000;
        let seconds = 0.25f32;
        let pcm = tone(rate, seconds, 1);

        let ogg = encode_opus_ogg(&pcm, rate, 1).expect("encode");
        assert!(ogg.starts_with(b"OggS"), "missing Ogg capture pattern");

        let mut reader = OggOpusReader::new(Cursor::new(ogg.as_slice())).expect("parse");
        assert_eq!(reader.head().channel_count, 1);
        assert_eq!(reader.head().input_sample_rate, rate);
        assert!(reader.head().pre_skip > 0);
        assert!(
            reader.tags().get("ENCODER").is_some(),
            "missing ENCODER tag"
        );

        let mut end_granule = 0u64;
        let mut saw_eos = false;
        for packet in reader.packets() {
            let packet = packet.expect("packet");
            end_granule = end_granule.max(packet.page_granule.max(0) as u64);
            saw_eos |= packet.end_of_stream;
        }
        assert!(saw_eos, "stream must end with an EOS page");

        let played = end_granule - u64::from(reader.head().pre_skip);
        let expected = (rate as f32 * seconds) as u64;
        assert_eq!(played, expected, "end-trim must land exactly on the audio");
    }

    #[test]
    fn encodes_stereo_audio() {
        let rate = 48_000;
        let pcm = tone(rate, 0.1, 2);
        let ogg = encode_opus_ogg(&pcm, rate, 2).expect("encode");
        let reader = OggOpusReader::new(Cursor::new(ogg.as_slice())).expect("parse");
        assert_eq!(reader.head().channel_count, 2);
    }

    #[test]
    fn mixdown_averages_frames() {
        let stereo = [1.0, 3.0, 2.0, 4.0, 0.5, -0.5];
        assert_eq!(mixdown_mono(&stereo, 2), [2.0, 3.0, 0.0]);
    }

    #[test]
    fn mixdown_drops_partial_frame() {
        assert_eq!(mixdown_mono(&[1.0, 2.0, 3.0], 2), [1.5]);
    }

    #[test]
    fn mixdown_passes_mono_through() {
        let mono = [0.25, -0.25];
        assert_eq!(mixdown_mono(&mono, 1), mono);
    }

    #[test]
    fn waveform_downscales_to_bucket_count() {
        let pcm = tone(48_000, 1.0, 1);
        let waveform = waveform_from_pcm(&pcm, 128);
        assert_eq!(waveform.len(), 128);
        assert!(waveform.iter().all(|b| *b <= 127));
    }

    #[test]
    fn waveform_clamps_peak_at_127() {
        assert_eq!(waveform_from_pcm(&[1.0f32; 48], 4), [127; 4]);
    }

    #[test]
    fn waveform_preserves_silence() {
        assert_eq!(waveform_from_pcm(&[0.0f32; 48], 4), [0; 4]);
    }

    #[test]
    fn prefers_highest_opus_rate_in_range() {
        let range = cpal::SupportedStreamConfigRange::new(
            2,
            cpal::SampleRate(44_100),
            cpal::SampleRate(48_000),
            cpal::SupportedBufferSize::Unknown,
            cpal::SampleFormat::F32,
        );
        assert_eq!(preferred_opus_rate(&range), Some(48_000));
    }

    #[test]
    fn falls_back_to_lowest_opus_rate() {
        let range = cpal::SupportedStreamConfigRange::new(
            1,
            cpal::SampleRate(8_000),
            cpal::SampleRate(8_000),
            cpal::SupportedBufferSize::Unknown,
            cpal::SampleFormat::F32,
        );
        assert_eq!(preferred_opus_rate(&range), Some(8_000));
    }

    #[test]
    fn rejects_rate_no_opus_value_covers() {
        let range = cpal::SupportedStreamConfigRange::new(
            1,
            cpal::SampleRate(44_100),
            cpal::SampleRate(44_100),
            cpal::SupportedBufferSize::Unknown,
            cpal::SampleFormat::F32,
        );
        assert_eq!(preferred_opus_rate(&range), None);
    }
}
