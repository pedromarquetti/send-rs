//! rodio-backed [`MediaEngine`] implementation.
//!
//! Audio is decoded fully in memory: Opus-in-Ogg (the container both WhatsApp
//! and Telegram use for voice notes) through a bespoke rodio `Source` over
//! `opus-pure`, everything else (wav/mp3/m4a/flac/vorbis) through rodio's
//! symphonia decoders. Playback goes through a `Sink` on the default output
//! device, which is opened lazily on the first `play` and recreated per
//! session; stopping a session drops the sink, which halts audio without
//! blocking on the end of the cue. The engine keeps its own wall-clock
//! position, so seeking and progress never depend on the device round-trip.

use std::io::Cursor;
use std::time::{Duration, Instant};

use opus_pure::ogg::GRANULE_RATE;
use opus_pure::{MAX_PACKET_SAMPLES, OggOpusReader, OggPacket, OpusDecoder};
use rodio::source::SeekError;
use rodio::{Decoder, OutputStream, OutputStreamBuilder, Sink, Source};
use tracing::{debug, warn};

use crate::tui::player::{MediaEngine, PlayState};

/// A fully decoded piece of audio, ready to be played.
struct Decoded {
    source: Box<dyn Source + Send>,
    sample_rate: u32,
    channels: u16,
    duration: Option<Duration>,
}

/// A rodio `Source` decoding Opus audio from an Ogg container held in memory.
///
/// WhatsApp and Telegram deliver voice notes as whole OGG/Opus blobs. The
/// notes are short, so the container is demuxed once up front (packets are
/// copied out) and decoding is sample-accurate in both directions. Playable
/// audio is the raw decoder stream between the Opus `pre_skip` padding and the
/// final page granule; both bounds are positions on that one timeline, so
/// seeking back to the front also prunes the end.
struct OpusSource {
    packets: Vec<OggPacket>,
    next_packet: usize,
    decoder: OpusDecoder,
    channels: usize,
    /// The Opus `pre_skip` padding (raw stream samples); the file's playable
    /// length is `end_granule` minus this.
    pre_skip: u64,
    /// Raw stream offset of the first sample that may be emitted: `pre_skip`
    /// initially, and `pre_skip + seek_offset` after a seek.
    emit_lo: u64,
    /// Raw stream offset one past the last sample that may be emitted, taken
    /// from the final page granule (the encoder may append padding after it).
    emit_hi: u64,
    /// Raw stream offset of the next packet; advances one packet per decode.
    cursor: u64,
    scratch: Vec<f32>,
    block: Vec<f32>,
    block_pos: usize,
}

impl OpusSource {
    fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let mut reader = OggOpusReader::new(Cursor::new(bytes)).map_err(|err| err.to_string())?;
        let head = reader.head().clone();

        let mut packets = Vec::new();
        let mut end_granule = 0u64;

        for packet in reader.packets() {
            let packet = packet.map_err(|err| err.to_string())?;
            end_granule = end_granule.max(packet.page_granule.max(0) as u64);
            packets.push(packet);
        }

        let channels = head.channel_count as usize;
        let decoder = head
            .decoder(GRANULE_RATE as i32)
            .map_err(|err| err.to_string())?;
        let pre_skip = u64::from(head.pre_skip);

        Ok(Self {
            packets,
            next_packet: 0,
            decoder,
            channels,
            pre_skip,
            emit_lo: pre_skip,
            emit_hi: end_granule,
            cursor: 0,
            scratch: vec![0.0; MAX_PACKET_SAMPLES * channels],
            block: Vec::new(),
            block_pos: 0,
        })
    }

    /// Total playable samples: the file length from the granule without the
    /// `pre_skip` padding. Constant across seeks.
    fn playable(&self) -> u64 {
        self.emit_hi.saturating_sub(self.pre_skip)
    }

    fn seek_to(&mut self, position: Duration) {
        let target = (position.as_secs_f64() * f64::from(GRANULE_RATE)) as u64;

        self.emit_lo = self.pre_skip + target.min(self.playable());
        self.cursor = 0;
        self.next_packet = 0;
        self.block.clear();
        self.block_pos = 0;

        if let Err(err) = self.decoder.reset_state() {
            warn!(error = %err, "failed to reset the opus decoder on seek");
        }
    }
}

impl Iterator for OpusSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        loop {
            if self.block_pos < self.block.len() {
                let sample = self.block[self.block_pos];
                self.block_pos += 1;
                return Some(sample);
            }

            if self.cursor >= self.emit_hi || self.next_packet >= self.packets.len() {
                return None;
            }

            let packet = self.packets[self.next_packet].clone();
            self.next_packet += 1;

            let start = self.cursor;
            let frames =
                match self
                    .decoder
                    .decode(&packet.data, MAX_PACKET_SAMPLES, &mut self.scratch)
                {
                    Ok(frames) => frames,
                    Err(err) => {
                        warn!(error = %err, "dropping an undecodable opus packet");
                        self.cursor = start;
                        continue;
                    }
                };
            self.cursor = start + frames as u64;

            // Emit only the slice of this packet inside the playable window.
            let lo = start.max(self.emit_lo);
            let hi = (start + frames as u64).min(self.emit_hi);

            if lo >= hi {
                continue;
            }

            let off = (lo - start) as usize;
            let take = (hi - lo) as usize;

            self.block.clear();
            self.block.extend_from_slice(
                &self.scratch[off * self.channels..(off + take) * self.channels],
            );
            self.block_pos = 0;
        }
    }
}

impl Source for OpusSource {
    fn current_span_len(&self) -> Option<usize> {
        let remaining = self.block.len() - self.block_pos;
        (remaining > 0).then_some(remaining)
    }

    fn channels(&self) -> rodio::ChannelCount {
        self.channels as rodio::ChannelCount
    }

    fn sample_rate(&self) -> rodio::SampleRate {
        GRANULE_RATE as rodio::SampleRate
    }

    fn total_duration(&self) -> Option<Duration> {
        let playable = self.playable();
        (playable > 0).then(|| Duration::from_secs_f64(playable as f64 / f64::from(GRANULE_RATE)))
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        self.seek_to(pos);
        Ok(())
    }
}

/// Decode `bytes` into a playable source. Ogg containers are tried as Opus
/// first (voice notes), falling back to rodio's symphonia decoders.
fn decode(bytes: &[u8]) -> Result<Decoded, String> {
    if bytes.starts_with(b"OggS") {
        if let Ok(source) = OpusSource::from_bytes(bytes) {
            let sample_rate = source.sample_rate();
            let channels = source.channels();
            let duration = source.total_duration();

            return Ok(Decoded {
                source: Box::new(source),
                sample_rate,
                channels,
                duration,
            });
        }
        debug!("ogg container did not contain opus; trying symphonia decoders");
    }

    let decoder = Decoder::new(Cursor::new(bytes.to_vec()))
        .map_err(|err| format!("unsupported or corrupt audio format: {err}"))?;

    Ok(Decoded {
        sample_rate: decoder.sample_rate(),
        channels: decoder.channels(),
        duration: decoder.total_duration(),
        source: Box::new(decoder),
    })
}

/// A loaded-but-not-necessarily-attached audio session.
struct Loaded {
    /// The decoded source, present until it has been attached to a sink.
    source: Option<Box<dyn Source + Send>>,
    duration: Option<Duration>,
}

/// Engine-authoritative playback clock. While `playing_since` is set the
/// position is extrapolated on demand, so `position()` never needs a sink
/// round-trip.
#[derive(Debug, Default)]
struct Timing {
    position: f64,
    playing_since: Option<Instant>,
}

impl Timing {
    fn current(&self, duration: f64) -> f64 {
        let mut position = self.position;
        if let Some(since) = self.playing_since {
            position += since.elapsed().as_secs_f64();
            if duration > 0.0 {
                position = position.min(duration);
            }
        }
        position
    }

    fn play(&mut self) {
        self.playing_since = Some(Instant::now());
    }

    fn pause(&mut self, duration: f64) {
        self.position = self.current(duration);
        self.playing_since = None;
    }

    fn seek(&mut self, position: f64, playing: bool) {
        self.position = position;
        self.playing_since = playing.then(Instant::now);
    }

    fn stop(&mut self) {
        self.position = 0.0;
        self.playing_since = None;
    }
}

/// The concrete [`MediaEngine`] for audio.
///
/// The default output device is opened lazily on the first `play` and a fresh
/// `Sink` is created for every session. Ending a session (EOF, `stop`, or a
/// new `load` over an old session) drops the sink, which stops audio without
/// blocking on the end of the cue.
pub struct RodioEngine {
    stream: Option<OutputStream>,
    sink: Option<Sink>,
    current: Option<Loaded>,
    timing: Timing,
    status: PlayState,
}

impl Default for RodioEngine {
    fn default() -> Self {
        Self {
            stream: None,
            sink: None,
            current: None,
            timing: Timing::default(),
            status: PlayState::Stopped,
        }
    }
}

impl RodioEngine {
    /// Bind the engine to an already-built sink, skipping device discovery.
    /// Test-only: the audio pipeline runs against a driver thread instead of
    /// an output device.
    #[cfg(test)]
    fn with_sink(sink: Sink) -> Self {
        let mut engine = Self::default();
        engine.sink = Some(sink);
        engine
    }

    fn open_output(&mut self) -> Result<(), String> {
        let stream = OutputStreamBuilder::open_default_stream()
            .map_err(|err| format!("cannot open the audio output device: {err}"))?;
        let sink = Sink::connect_new(stream.mixer());
        self.stream = Some(stream);
        self.sink = Some(sink);
        Ok(())
    }
}

impl MediaEngine for RodioEngine {
    fn load(&mut self, bytes: &[u8]) -> Result<f64, String> {
        let decoded = match decode(bytes) {
            Ok(decoded) => decoded,
            Err(err) => {
                self.sink.take();
                self.current = None;
                self.timing.stop();
                self.status = PlayState::Error;
                return Err(err);
            }
        };

        self.sink.take();
        self.current = Some(Loaded {
            source: Some(decoded.source),
            duration: decoded.duration,
        });
        self.timing.stop();
        self.status = PlayState::Paused;

        Ok(decoded.duration.map(|d| d.as_secs_f64()).unwrap_or(0.0))
    }

    fn play(&mut self) {
        let pending = match self.current.as_mut() {
            None => {
                self.status = PlayState::Stopped;
                return;
            }
            Some(current) => current.source.take(),
        };

        if let Some(mut source) = pending {
            let position = self.timing.position;

            if position > 0.0
                && let Err(err) = source.try_seek(Duration::from_secs_f64(position))
            {
                debug!(error = %err, "cannot seek the pending source to the pre-play position");
            }

            if self.sink.is_none()
                && let Err(err) = self.open_output()
            {
                // Device open failed; keep the pending source so `play` can be
                // retried once a device appears.
                if let Some(current) = &mut self.current {
                    current.source = Some(source);
                }
                warn!(error = %err, "audio playback could not start");
                self.status = PlayState::Error;
                return;
            }

            if let Some(sink) = &self.sink {
                sink.append(source);
            }
        }

        if let Some(sink) = &self.sink {
            sink.play();
        }

        self.timing.play();
        self.status = PlayState::Playing;
    }

    fn pause(&mut self) {
        if self.status == PlayState::Playing {
            if let Some(sink) = &self.sink {
                sink.pause();
            }

            self.timing.pause(self.duration());
            self.status = PlayState::Paused;
        }
    }

    fn seek(&mut self, position_secs: f64) {
        let Some(current) = &self.current else {
            return;
        };

        let max = current.duration.map(|d| d.as_secs_f64()).unwrap_or(0.0);

        let target = if max > 0.0 {
            position_secs.clamp(0.0, max)
        } else {
            position_secs.max(0.0)
        };

        // A pending (not yet attached) source is seeked in `play`; an empty
        // sink is already finished, so no sink seek is issued.
        if let Some(sink) = &self.sink
            && let Err(err) = sink.try_seek(Duration::from_secs_f64(target))
        {
            debug!(error = %err, "audio seek is not supported by the engine");
        }

        self.timing.seek(target, self.status == PlayState::Playing);
    }

    fn stop(&mut self) {
        self.sink.take();
        self.stream.take();
        self.current = None;
        self.timing.stop();
        self.status = PlayState::Stopped;
    }

    fn position(&self) -> f64 {
        self.timing.current(self.duration())
    }

    fn duration(&self) -> f64 {
        self.current
            .as_ref()
            .and_then(|current| current.duration)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    }

    fn status(&mut self) -> PlayState {
        if self.status == PlayState::Playing
            && let Some(sink) = &self.sink
            && sink.empty()
        {
            self.timing.pause(self.duration());
            self.sink.take();
            self.current = None;
            self.status = PlayState::Stopped;
        }
        self.status
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rodio::queue::SourcesQueueOutput;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;

    const TONE_WAV: &[u8] = include_bytes!("fixtures/tone.wav");
    const VOICE_OGG: &[u8] = include_bytes!("fixtures/voice.ogg");

    /// Consume the sink output in a loop until the test signals completion.
    /// `Sink::try_seek` waits for feedback that is only produced while the
    /// queue is being consumed, so this thread is required for engine seek
    /// tests (and for decoding to progress at all).
    fn spawn_drain(output: SourcesQueueOutput, done: Arc<AtomicBool>) -> JoinHandle<()> {
        let mut output = output;
        std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                let _ = output.next();
            }
        })
    }

    fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if condition() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("condition not met within {timeout:?}");
    }

    fn engine_harness() -> (RodioEngine, Arc<AtomicBool>, JoinHandle<()>) {
        let (sink, output) = Sink::new();
        let done = Arc::new(AtomicBool::new(false));
        let drain = spawn_drain(output, done.clone());
        (RodioEngine::with_sink(sink), done, drain)
    }

    #[test]
    fn decodes_wav_fixture() {
        let decoded = decode(TONE_WAV).unwrap();
        assert_eq!(decoded.channels, 1);
        assert_eq!(decoded.sample_rate, 8000);
        let duration = decoded.duration.unwrap();
        assert!(
            (duration.as_secs_f64() - 1.0).abs() < 0.05,
            "unexpected wav duration {duration:?}"
        );

        let samples = decoded.source.count();
        assert!(
            (samples as i64 - 8000).abs() < 100,
            "unexpected sample count {samples}"
        );
    }

    #[test]
    fn decodes_opus_fixture() {
        let decoded = decode(VOICE_OGG).unwrap();
        assert_eq!(decoded.channels, 1);
        assert_eq!(decoded.sample_rate, GRANULE_RATE);
        let duration = decoded.duration.unwrap();
        assert!(
            (duration.as_secs_f64() - 2.0).abs() < 0.1,
            "unexpected opus duration {duration:?}"
        );

        let samples = decoded.source.count();
        let expected = (2.0 * f64::from(GRANULE_RATE)) as i64;
        assert!(
            (samples as i64 - expected).abs() < 600,
            "expected ~{expected} samples, got {samples}"
        );
    }

    #[test]
    fn opus_seek_resumes_at_position() {
        let mut source = decode(VOICE_OGG).unwrap().source;
        source.try_seek(Duration::from_millis(750)).unwrap();

        let samples = source.count();
        let expected = (1.25 * f64::from(GRANULE_RATE)) as i64;
        assert!(
            (samples as i64 - expected).abs() < 600,
            "expected ~{expected} samples after seeking, got {samples}"
        );
    }

    #[test]
    fn seek_window_is_trimmed_at_both_ends() {
        let unseeked = OpusSource::from_bytes(VOICE_OGG).unwrap();
        assert_eq!(unseeked.playable(), 96000);
        let mut seeked = OpusSource::from_bytes(VOICE_OGG).unwrap();
        seeked.try_seek(Duration::from_millis(750)).unwrap();
        // From 0.75s to the (pre-skip trimmed) end; the seek window keeps the
        // front pre-skip out, so allow one interleaved frame of slack.
        let count = seeked.count() as i64;
        let expected = (1.25 * f64::from(GRANULE_RATE)) as i64;
        assert!(
            (count - expected).abs() < 1000,
            "expected ~{expected} samples after seeking, got {count}"
        );
    }

    #[test]
    fn malformed_bytes_report_error_status() {
        let mut engine = RodioEngine::with_sink({
            let (sink, output) = Sink::new();
            drop(output);
            sink
        });
        assert!(engine.load(b"definitely not audio").is_err());
        assert_eq!(engine.status(), PlayState::Error);
    }

    #[test]
    fn engine_load_play_pause_seek_stop() {
        let (mut engine, done, drain) = engine_harness();

        let duration = engine.load(VOICE_OGG).unwrap();
        assert!((duration - 2.0).abs() < 0.1);
        assert_eq!(engine.status(), PlayState::Paused);
        assert_eq!(engine.position(), 0.0);

        engine.play();
        assert_eq!(engine.status(), PlayState::Playing);
        wait_until(Duration::from_secs(3), || engine.position() > 0.05);

        engine.pause();
        let paused = engine.position();
        assert_eq!(engine.status(), PlayState::Paused);
        std::thread::sleep(Duration::from_millis(60));
        let drifted = engine.position();
        assert!(
            (paused - drifted).abs() < 0.05,
            "paused position must not advance ({paused} vs {drifted})"
        );

        engine.seek(1.5);
        assert!(
            (engine.position() - 1.5).abs() < 0.05,
            "seek did not land at 1.5s"
        );
        engine.seek(999.0);
        assert!(
            (engine.position() - 2.0).abs() < 0.05,
            "seek past the end did not clamp"
        );
        engine.seek(-5.0);
        assert!(
            engine.position().abs() < 0.001,
            "seek before the start did not clamp to zero"
        );

        engine.play();
        engine.stop();
        assert_eq!(engine.status(), PlayState::Stopped);
        assert_eq!(engine.position(), 0.0);

        done.store(true, Ordering::Relaxed);
        drain.join().unwrap();
    }

    #[test]
    fn engine_reports_eof_as_stopped() {
        let (mut engine, done, drain) = engine_harness();

        engine.load(VOICE_OGG).unwrap();
        engine.play();
        assert_eq!(engine.status(), PlayState::Playing);

        wait_until(Duration::from_secs(5), || {
            engine.status() == PlayState::Stopped
        });
        let position = engine.position();
        assert!(
            (position - 2.0).abs() < 0.1,
            "expected the end position, got {position}"
        );

        done.store(true, Ordering::Relaxed);
        drain.join().unwrap();
    }

    #[test]
    fn seek_before_first_play_starts_at_position() {
        let (mut engine, done, drain) = engine_harness();

        engine.load(VOICE_OGG).unwrap();
        engine.seek(1.0);
        engine.play();

        wait_until(Duration::from_secs(3), || engine.position() > 1.05);

        done.store(true, Ordering::Relaxed);
        drain.join().unwrap();
    }
}
