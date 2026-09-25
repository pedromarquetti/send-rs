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
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use opus_pure::ogg::GRANULE_RATE;
use opus_pure::{MAX_PACKET_SAMPLES, OggOpusReader, OggPacket, OpusDecoder};
#[cfg(test)]
use rodio::queue::SourcesQueueOutput;
use rodio::source::SeekError;
use rodio::{Decoder, OutputStream, OutputStreamBuilder, Sink, Source};
use tracing::{debug, warn};

use crate::tui::player::{MediaEngine, PlayState};

/// A fully decoded piece of audio, ready to be played.
struct Decoded {
    source: Box<dyn Source + Send>,
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
            return Ok(Decoded {
                duration: source.total_duration(),
                source: Box::new(source),
            });
        }
        debug!("ogg container did not contain opus; trying symphonia decoders");
    }

    let decoder = Decoder::new(Cursor::new(bytes.to_vec()))
        .map_err(|err| format!("unsupported or corrupt audio format: {err}"))?;

    Ok(Decoded {
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
/// blocking on the end of the cue. The raw session bytes are kept while the
/// session is loaded so a finished session can be restarted from the beginning
/// on a fresh `play` (what the user expects the space bar to do at EOF); an
/// explicit `stop` (Esc) forgets the session instead.
pub struct RodioEngine {
    stream: Option<OutputStream>,
    sink: Option<Sink>,
    current: Option<Loaded>,
    bytes: Option<Vec<u8>>,
    timing: Timing,
    status: PlayState,
    /// Tests run where no audio device exists, so `open_output` hands playback
    /// to a device-free sink drained in real time instead of cpal.
    #[cfg(test)]
    headless: bool,
    #[cfg(test)]
    virtual_output: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
}

impl Default for RodioEngine {
    fn default() -> Self {
        Self {
            stream: None,
            sink: None,
            current: None,
            bytes: None,
            timing: Timing::default(),
            status: PlayState::Stopped,
            #[cfg(test)]
            headless: false,
            #[cfg(test)]
            virtual_output: None,
        }
    }
}

impl RodioEngine {
    /// Engine bound to a device-free sink. The audio pipeline runs against a
    /// drain thread instead of an output device, so playback can be exercised
    /// on machines (and CI containers) without a sound card.
    #[cfg(test)]
    fn headless() -> Self {
        let mut engine = Self::default();
        engine.headless = true;
        engine
    }

    #[cfg(test)]
    fn stop_virtual_output(&mut self) {
        if let Some((done, drain)) = self.virtual_output.take() {
            done.store(true, Ordering::Relaxed);
            let _ = drain.join();
        }
    }

    fn open_output(&mut self) -> Result<(), String> {
        #[cfg(test)]
        if self.headless {
            self.stop_virtual_output();
            let (sink, output) = Sink::new();
            self.virtual_output = Some(drain_in_real_time(output));
            self.sink = Some(sink);
            return Ok(());
        }

        let mut stream = OutputStreamBuilder::open_default_stream()
            .map_err(|err| format!("cannot open the audio output device: {err}"))?;
        // rodio logs a "Dropping OutputStream" notice, but only as noise; the
        // engine controls the whole lifecycle. Suppress it.
        stream.log_on_drop(false);
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
                self.bytes = None;
                self.timing.stop();
                self.status = PlayState::Error;
                return Err(err);
            }
        };

        debug!(
            duration = decoded.duration.map(|d| d.as_secs_f64()).unwrap_or(0.0),
            "audio loaded"
        );
        self.sink.take();
        self.bytes = Some(bytes.to_vec());
        self.current = Some(Loaded {
            source: Some(decoded.source),
            duration: decoded.duration,
        });
        self.timing.stop();
        self.status = PlayState::Paused;

        Ok(decoded.duration.map(|d| d.as_secs_f64()).unwrap_or(0.0))
    }

    fn play(&mut self) {
        // A session that reached EOF dropped its `current` source. The space
        // bar after the end is a request to play the session again from the
        // start, so re-decode the retained bytes and continue below.
        if self.current.is_none()
            && let Some(bytes) = self.bytes.clone()
        {
            debug!("restarting finished session from the beginning");
            let _ = self.load(&bytes);
        }

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
        debug!("audio playing");
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
        if self.status == PlayState::Playing || self.sink.is_some() {
            debug!("stopping audio session");
        }

        self.sink.take();
        self.stream.take();
        self.current = None;
        self.bytes = None;
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
        // EOF: the sink consumed its last sample. The session is finished but
        // its bytes are retained so a later `play` can restart it.
        if self.status == PlayState::Playing
            && let Some(sink) = &self.sink
            && sink.empty()
        {
            debug!(
                position = self.position(),
                duration = self.duration(),
                "audio reached end of stream"
            );
            self.timing.pause(self.duration());
            self.sink.take();
            self.current = None;
            self.status = PlayState::Stopped;
        }
        self.status
    }
}

/// Consume a queue output at real time, the way an output device would. EOF is
/// detected through `Sink::empty()` while the engine extrapolates its own
/// wall-clock position, so draining faster than real time would end the cue
/// before the engine believes it played.
#[cfg(test)]
fn drain_in_real_time(mut output: SourcesQueueOutput) -> (Arc<AtomicBool>, JoinHandle<()>) {
    let done = Arc::new(AtomicBool::new(false));
    let finished = done.clone();

    let drain = std::thread::spawn(move || {
        let start = Instant::now();
        let mut consumed = 0.0_f64;

        while !finished.load(Ordering::Relaxed) {
            for _ in 0..1024 {
                if output.next().is_none() {
                    break;
                }
                consumed += 1.0;
            }

            let rate = f64::from(output.sample_rate()) * f64::from(output.channels());
            if rate > 0.0 {
                let ahead = consumed / rate - start.elapsed().as_secs_f64();
                if ahead > 0.0 {
                    std::thread::sleep(Duration::from_secs_f64(ahead.min(0.05)));
                }
            }
        }
    });

    (done, drain)
}

#[cfg(test)]
impl Drop for RodioEngine {
    fn drop(&mut self) {
        self.stop_virtual_output();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TONE_WAV: &[u8] = include_bytes!("fixtures/tone.wav");
    const VOICE_OGG: &[u8] = include_bytes!("fixtures/voice.ogg");

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

    #[test]
    fn decodes_wav_fixture() {
        let decoded = decode(TONE_WAV).unwrap();
        assert_eq!(decoded.source.channels(), 1);
        assert_eq!(decoded.source.sample_rate(), 8000);
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
        assert_eq!(decoded.source.channels(), 1);
        assert_eq!(decoded.source.sample_rate(), GRANULE_RATE);
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
        let mut engine = RodioEngine::headless();
        assert!(engine.load(b"definitely not audio").is_err());
        assert_eq!(engine.status(), PlayState::Error);
    }

    #[test]
    fn engine_load_play_pause_seek_stop() {
        let mut engine = RodioEngine::headless();

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
    }

    #[test]
    fn engine_reports_eof_as_stopped() {
        let mut engine = RodioEngine::headless();

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
    }

    #[test]
    fn seek_before_first_play_starts_at_position() {
        let mut engine = RodioEngine::headless();

        engine.load(VOICE_OGG).unwrap();
        engine.seek(1.0);
        engine.play();

        wait_until(Duration::from_secs(3), || engine.position() > 1.05);
    }

    #[test]
    fn play_after_eof_restarts_from_the_beginning() {
        let mut engine = RodioEngine::headless();

        engine.load(VOICE_OGG).unwrap();
        engine.play();
        wait_until(Duration::from_secs(5), || {
            engine.status() == PlayState::Stopped
        });
        let end = engine.position();
        assert!(
            (end - 2.0).abs() < 0.1,
            "expected the end position, got {end}"
        );

        // Space after the end restarts the finished session at zero.
        engine.play();
        assert_eq!(engine.status(), PlayState::Playing);
        wait_until(Duration::from_secs(3), || {
            let position = engine.position();
            position > 0.0 && position < 0.5
        });
    }

    #[test]
    fn stop_forgets_session_so_play_does_not_restart() {
        let mut engine = RodioEngine::headless();

        engine.load(VOICE_OGG).unwrap();
        engine.stop();
        assert_eq!(engine.status(), PlayState::Stopped);

        engine.play();
        assert_eq!(
            engine.status(),
            PlayState::Stopped,
            "an explicitly stopped session must not come back on play"
        );
    }

    #[test]
    fn play_without_loaded_media_stays_stopped() {
        let mut engine = RodioEngine::default();
        engine.play();
        assert_eq!(engine.status(), PlayState::Stopped);
        assert_eq!(engine.position(), 0.0);
    }
}
