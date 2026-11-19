//! ffmpeg-backed streaming audio engine for video tracks.
//!
//! The video container is decoded by `ffmpeg` into raw `f32le` PCM on stdout
//! and consumed by a rodio [`Source`]. This deliberately does not use
//! Symphonia/Rodio's container decoding for video audio: the pinned
//! `symphonia-codec-aac 0.5.5` desynchronises on some AAC streams and reports a
//! silent EOF after about a second. FFmpeg is already required for the video
//! frames, and its AAC decoder is the one that matches the picture pipeline.
//!
//! Playback is streaming: no full decoded WAV is buffered. `pause`, `seek`,
//! `stop`, `load` and `Drop` kill the decoder child and drop the sink, so at
//! most one ffmpeg audio process exists per engine. Seeking respawns the child
//! at the requested position rather than trying to seek a pipe.

use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use rodio::{OutputStream, OutputStreamBuilder, Sink, Source};
use tempfile::NamedTempFile;
use tracing::{debug, warn};

use super::ffmpeg::temp_file;
use super::rodio::Timing;
use crate::tui::player::{MediaEngine, PlayState};

/// Output PCM format from ffmpeg. A fixed stereo/48 kHz stream keeps the
/// rodio source independent of the container's native layout and rate.
const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const SAMPLE_BYTES: usize = 4;
const READ_CHUNK_BYTES: usize = 16 * 1024;

fn decode_args(path: &Path, position: f64) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["-hide_banner", "-loglevel", "error", "-nostdin"]
        .iter()
        .map(OsString::from)
        .collect();

    // Input seeking is accurate-seek by default and keeps large offsets cheap;
    // audio is respawned for every seek, so there is no pipe seek to support.
    if position > 0.0 {
        args.push("-ss".into());
        args.push(format!("{position:.3}").into());
    }

    args.push("-i".into());
    args.push(path.as_os_str().to_os_string());
    args.extend(
        [
            "-map", "0:a:0", "-vn", "-sn", "-dn", "-ac", "2", "-ar", "48000", "-f", "f32le", "-",
        ]
        .iter()
        .map(OsString::from),
    );
    args
}

/// Probe the first audio stream.
///
/// `Ok(None)` means the container has no audio track at all: a valid silent
/// video, not an error. `Ok(Some(duration))` returns `0.0` when the stream has
/// no declared duration; playback still works and is bounded by the decoder's
/// EOF.
fn probe_audio(path: &Path) -> Result<Option<f64>, String> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=index,duration",
            "-show_entries",
            "format=duration",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .map_err(|err| format!("cannot run ffprobe: {err}"))?;

    if !output.status.success() {
        return Err("ffprobe could not read the video audio".to_string());
    }

    parse_audio_probe_json(&String::from_utf8_lossy(&output.stdout))
}

fn parse_audio_probe_json(json: &str) -> Result<Option<f64>, String> {
    let probe: serde_json::Value = serde_json::from_str(json)
        .map_err(|err| format!("cannot read the ffprobe audio output: {err}"))?;

    let streams = probe
        .get("streams")
        .and_then(serde_json::Value::as_array)
        .ok_or("the ffprobe audio output has no stream list")?;
    let Some(stream) = streams.first() else {
        return Ok(None);
    };

    let duration = stream["duration"]
        .as_str()
        .or_else(|| probe["format"]["duration"].as_str())
        .and_then(|raw| raw.trim().parse::<f64>().ok())
        .filter(|secs| *secs > 0.0)
        .unwrap_or(0.0);

    Ok(Some(duration))
}

/// A blocking PCM reader exposed to rodio as a streaming [`Source`].
///
/// `ended` is set when stdout closes normally; `failed` is set when the pipe
/// breaks or a partial sample arrives. The engine still inspects the child's
/// exit status before treating EOF as success.
struct PcmStream<R> {
    reader: R,
    samples: Vec<f32>,
    /// Bytes of a sample split across two pipe reads; a pipe may return any
    /// byte count, not necessarily a multiple of the sample width.
    carry: Vec<u8>,
    cursor: usize,
    ended: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    duration: f64,
}

impl<R: Read> PcmStream<R> {
    fn new(reader: R, ended: Arc<AtomicBool>, failed: Arc<AtomicBool>, duration: f64) -> Self {
        Self {
            reader,
            samples: Vec::new(),
            carry: Vec::new(),
            cursor: 0,
            ended,
            failed,
            duration,
        }
    }

    fn fill(&mut self) -> bool {
        let mut bytes = [0_u8; READ_CHUNK_BYTES];

        loop {
            let read = match self.reader.read(&mut bytes) {
                Ok(0) => {
                    // A trailing partial sample is a truncated PCM stream,
                    // not a clean EOF.
                    if self.carry.is_empty() {
                        self.ended.store(true, Ordering::Relaxed);
                    } else {
                        self.failed.store(true, Ordering::Relaxed);
                    }
                    return false;
                }
                Ok(read) => read,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.failed.store(true, Ordering::Relaxed);
                    return false;
                }
            };

            let mut pending = std::mem::take(&mut self.carry);
            pending.extend_from_slice(&bytes[..read]);

            let complete = pending.len() / SAMPLE_BYTES * SAMPLE_BYTES;
            self.samples.clear();
            self.samples.extend(
                pending[..complete]
                    .as_chunks::<SAMPLE_BYTES>()
                    .0
                    .iter()
                    .map(|sample| f32::from_le_bytes(*sample)),
            );
            self.carry = pending.split_off(complete);
            self.cursor = 0;

            if !self.samples.is_empty() {
                return true;
            }
        }
    }
}

impl<R: Read> Iterator for PcmStream<R> {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor >= self.samples.len() && !self.fill() {
            return None;
        }

        let sample = self.samples[self.cursor];
        self.cursor += 1;
        Some(sample)
    }
}

impl<R: Read + Send> Source for PcmStream<R> {
    fn current_span_len(&self) -> Option<usize> {
        Some(self.samples.len().saturating_sub(self.cursor))
    }

    fn channels(&self) -> u16 {
        CHANNELS
    }

    fn sample_rate(&self) -> u32 {
        SAMPLE_RATE
    }

    fn total_duration(&self) -> Option<Duration> {
        (self.duration > 0.0).then(|| Duration::from_secs_f64(self.duration))
    }
}

/// The concrete [`MediaEngine`] for a video's audio track.
pub struct FfmpegAudioEngine {
    /// The container spilled from the provider bytes, kept for replay after a
    /// clean EOF and unlinked by an explicit `stop`.
    temp: Option<NamedTempFile>,
    /// Total duration of the audio stream; `0.0` when unknown or absent.
    duration: f64,
    /// Whether the loaded container actually has an audio stream. A silent
    /// video keeps its bytes for the frame engine but never spawns an audio
    /// decoder.
    has_audio: bool,
    child: Option<Child>,
    stderr_reader: Option<JoinHandle<()>>,
    ended: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    timing: Timing,
    status: PlayState,
    stream: Option<OutputStream>,
    sink: Option<Sink>,
    #[cfg(test)]
    headless: bool,
    #[cfg(test)]
    virtual_output: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
}

impl Default for FfmpegAudioEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl FfmpegAudioEngine {
    pub fn new() -> Self {
        Self {
            temp: None,
            duration: 0.0,
            has_audio: false,
            child: None,
            stderr_reader: None,
            ended: Arc::new(AtomicBool::new(false)),
            failed: Arc::new(AtomicBool::new(false)),
            timing: Timing::default(),
            status: PlayState::Stopped,
            stream: None,
            sink: None,
            #[cfg(test)]
            headless: false,
            #[cfg(test)]
            virtual_output: None,
        }
    }

    #[cfg(test)]
    fn headless() -> Self {
        let mut engine = Self::new();
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

    fn drop_sink(&mut self) {
        self.sink.take();
        #[cfg(test)]
        self.stop_virtual_output();
    }

    fn ensure_sink(&mut self) -> Result<(), String> {
        if self.sink.is_some() {
            return Ok(());
        }

        #[cfg(test)]
        if self.headless {
            self.stop_virtual_output();
            let (sink, output) = Sink::new();
            self.virtual_output = Some(super::rodio::drain_in_real_time(output));
            self.sink = Some(sink);
            return Ok(());
        }

        if self.stream.is_none() {
            let mut stream = OutputStreamBuilder::open_default_stream()
                .map_err(|err| format!("cannot open the audio output device: {err}"))?;
            stream.log_on_drop(false);
            self.stream = Some(stream);
        }

        let Some(stream) = &self.stream else {
            return Err("the audio output stream is unavailable".to_string());
        };
        self.sink = Some(Sink::connect_new(stream.mixer()));
        Ok(())
    }

    fn kill_decode(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }

        if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
    }

    fn spawn_decode(&mut self, path: &Path, position: f64) -> Result<(), String> {
        // Open the device before spawning ffmpeg: a failed output must not
        // leave a decoder child blocked on a pipe nobody drains.
        self.ensure_sink()?;

        let mut child = Command::new("ffmpeg")
            .args(decode_args(path, position))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| format!("cannot run ffmpeg for video audio: {err}"))?;

        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            return Err("ffmpeg was spawned without an audio pipe".to_string());
        };
        let stderr = child.stderr.take();

        // Fresh per-session flags: a child killed by the engine has already
        // been joined, so it cannot write to these after a respawn.
        self.ended = Arc::new(AtomicBool::new(false));
        self.failed = Arc::new(AtomicBool::new(false));

        let source = PcmStream::new(
            stdout,
            Arc::clone(&self.ended),
            Arc::clone(&self.failed),
            self.duration,
        );

        let Some(sink) = &self.sink else {
            let _ = child.kill();
            return Err("the audio output sink is unavailable".to_string());
        };
        sink.append(source);
        sink.play();

        if let Some(stderr) = stderr {
            self.stderr_reader = Some(std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let line = line.trim_end();
                    if !line.is_empty() {
                        warn!(diagnostic = line, "ffmpeg video-audio reported a problem");
                    }
                }
            }));
        }

        self.child = Some(child);
        Ok(())
    }
}

impl MediaEngine for FfmpegAudioEngine {
    fn load(&mut self, bytes: &[u8]) -> Result<f64, String> {
        self.kill_decode();
        self.drop_sink();
        self.temp = None;
        self.has_audio = false;
        self.duration = 0.0;
        self.timing.stop();
        self.status = PlayState::Stopped;

        let mut temp = match temp_file() {
            Ok(temp) => temp,
            Err(err) => {
                self.status = PlayState::Error;
                return Err(format!("cannot create video-audio scratch file: {err}"));
            }
        };
        if let Err(err) = temp.write_all(bytes) {
            self.status = PlayState::Error;
            return Err(format!("cannot write video-audio scratch file: {err}"));
        }

        let audio = probe_audio(temp.path());
        self.temp = Some(temp);

        match audio {
            Ok(Some(duration)) => {
                self.has_audio = true;
                self.duration = duration;
                self.status = PlayState::Paused;
                debug!(duration, "video audio loaded, paused at the start");
                Ok(duration)
            }
            Ok(None) => {
                // A silent video is valid media: keep the bytes for the frame
                // engine, but never spawn a decoder that would fail on a
                // stream that does not exist.
                self.status = PlayState::Unavailable;
                debug!("video has no audio track, playing frames only");
                Ok(0.0)
            }
            Err(err) => {
                self.status = PlayState::Error;
                warn!(error = %err, "video audio could not be probed");
                Err(err)
            }
        }
    }

    fn play(&mut self) {
        if !self.has_audio {
            return;
        }

        let Some(path) = self.temp.as_ref().map(|temp| temp.path().to_path_buf()) else {
            self.status = PlayState::Stopped;
            return;
        };

        // A session that reached EOF keeps its file; the space bar means
        // "play it again" from the beginning.
        if self.status == PlayState::Stopped {
            self.timing.seek(0.0, false);
        }

        let position = self.timing.current(self.duration);

        self.kill_decode();
        self.drop_sink();

        match self.spawn_decode(&path, position) {
            Ok(()) => {
                self.timing.play();
                self.status = PlayState::Playing;
                debug!(position_secs = position, "video audio playing");
            }
            Err(err) => {
                self.drop_sink();
                self.timing.pause(self.duration);
                self.status = PlayState::Error;
                warn!(error = %err, "video audio playback could not start");
            }
        }
    }

    fn pause(&mut self) {
        if self.status != PlayState::Playing {
            return;
        }

        self.kill_decode();
        self.drop_sink();
        self.timing.pause(self.duration);
        self.status = PlayState::Paused;
    }

    fn seek(&mut self, position_secs: f64) {
        if !self.has_audio {
            return;
        }

        let Some(path) = self.temp.as_ref().map(|temp| temp.path().to_path_buf()) else {
            return;
        };

        let target = if self.duration > 0.0 {
            position_secs.clamp(0.0, self.duration)
        } else {
            position_secs.max(0.0)
        };
        let playing = self.status == PlayState::Playing;

        self.kill_decode();
        self.drop_sink();
        self.timing.seek(target, playing);

        if !playing {
            // No frame to publish while paused: `play` respawns at this
            // position using the timing clock.
            return;
        }

        match self.spawn_decode(&path, target) {
            Ok(()) => self.status = PlayState::Playing,
            Err(err) => {
                self.drop_sink();
                self.timing.pause(self.duration);
                self.status = PlayState::Error;
                warn!(error = %err, "video audio seek could not restart the decode");
            }
        }
    }

    fn stop(&mut self) {
        self.kill_decode();
        self.drop_sink();
        self.stream.take();
        // Dropping the `NamedTempFile` unlinks the scratch copy.
        self.temp = None;
        self.duration = 0.0;
        self.has_audio = false;
        self.timing.stop();
        self.status = PlayState::Stopped;
    }

    fn is_busy(&self) -> bool {
        self.temp.is_some() || self.status == PlayState::Playing
    }

    fn play_cue(&mut self, _bytes: &[u8]) -> Result<(), String> {
        // Notification cues belong to the ordinary audio player; the video
        // audio session must never take over that output.
        debug!("notification cue ignored by the video audio engine");
        Ok(())
    }

    fn position(&self) -> f64 {
        self.timing.current(self.duration)
    }

    fn duration(&self) -> f64 {
        self.duration
    }

    fn status(&mut self) -> PlayState {
        if self.status != PlayState::Playing {
            return self.status;
        }

        if self.failed.load(Ordering::Relaxed) {
            self.kill_decode();
            self.drop_sink();
            self.timing.pause(self.duration);
            self.status = PlayState::Error;
            return self.status;
        }

        if !self.ended.load(Ordering::Relaxed) {
            return self.status;
        }

        // stdout closed, but only the child's exit status separates a clean
        // end from a decoder that gave up after emitting some samples.
        let exit = self
            .child
            .as_mut()
            .and_then(|child| child.try_wait().ok().flatten());
        let Some(exit) = exit else {
            return self.status;
        };

        self.child = None;
        if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
        self.drop_sink();
        self.timing.pause(self.duration);

        self.status = if exit.success() {
            debug!("video audio reached end of stream");
            PlayState::Stopped
        } else {
            warn!(
                code = exit.code(),
                "video audio decode exited with a failure status"
            );
            PlayState::Error
        };
        self.status
    }
}

impl Drop for FfmpegAudioEngine {
    fn drop(&mut self) {
        self.kill_decode();
        self.drop_sink();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::process::Command;
    use std::time::Instant;

    fn args_of(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn media_tools_available() -> bool {
        ["ffmpeg", "ffprobe"].iter().all(|tool| {
            Command::new(tool)
                .arg("-version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
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

    #[test]
    fn decode_args_map_only_the_audio_stream_to_f32_pcm() {
        let args = args_of(&decode_args(Path::new("/tmp/clip.bin"), 1.5));
        assert_eq!(
            args,
            vec![
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-ss",
                "1.500",
                "-i",
                "/tmp/clip.bin",
                "-map",
                "0:a:0",
                "-vn",
                "-sn",
                "-dn",
                "-ac",
                "2",
                "-ar",
                "48000",
                "-f",
                "f32le",
                "-",
            ]
        );
    }

    #[test]
    fn pcm_stream_decodes_little_endian_samples_and_signals_eof() {
        let mut bytes = Vec::new();
        for sample in [0.25_f32, -0.5, 1.0] {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        let ended = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let mut stream = PcmStream::new(
            Cursor::new(bytes),
            Arc::clone(&ended),
            Arc::clone(&failed),
            2.0,
        );

        assert_eq!(stream.next(), Some(0.25));
        assert_eq!(stream.next(), Some(-0.5));
        assert_eq!(stream.next(), Some(1.0));
        assert_eq!(stream.next(), None);
        assert!(ended.load(Ordering::Relaxed));
        assert!(!failed.load(Ordering::Relaxed));
        assert_eq!(stream.total_duration(), Some(Duration::from_secs(2)));
    }

    struct ChunkedReader {
        bytes: Vec<u8>,
        max: usize,
        pos: usize,
    }

    impl Read for ChunkedReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let remaining = self.bytes.len().saturating_sub(self.pos);
            let count = remaining.min(self.max).min(buf.len());
            buf[..count].copy_from_slice(&self.bytes[self.pos..self.pos + count]);
            self.pos += count;
            Ok(count)
        }
    }

    #[test]
    fn pcm_stream_reassembles_samples_split_across_pipe_reads() {
        let mut bytes = Vec::new();
        for sample in [0.25_f32, -0.5] {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        let ended = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let reader = ChunkedReader {
            bytes,
            max: 3,
            pos: 0,
        };
        let mut stream = PcmStream::new(reader, Arc::clone(&ended), Arc::clone(&failed), 1.0);

        assert_eq!(stream.next(), Some(0.25));
        assert_eq!(stream.next(), Some(-0.5));
        assert_eq!(stream.next(), None);
        assert!(ended.load(Ordering::Relaxed));
        assert!(!failed.load(Ordering::Relaxed));
    }

    #[test]
    fn pcm_stream_rejects_a_partial_sample() {
        let ended = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let mut stream = PcmStream::new(
            Cursor::new(vec![0_u8; 3]),
            Arc::clone(&ended),
            Arc::clone(&failed),
            0.0,
        );

        assert_eq!(stream.next(), None);
        assert!(!ended.load(Ordering::Relaxed));
        assert!(failed.load(Ordering::Relaxed));
    }

    #[test]
    fn audio_probe_distinguishes_a_missing_track_from_an_unknown_duration() {
        assert_eq!(
            parse_audio_probe_json(r#"{"streams":[],"format":{"duration":"9.5"}}"#),
            Ok(None),
            "an empty selected-stream list means the video is silent"
        );
        assert_eq!(
            parse_audio_probe_json(
                r#"{"streams":[{"index":1,"duration":"3.25"}],"format":{"duration":"9.5"}}"#
            ),
            Ok(Some(3.25)),
            "the audio stream's own duration wins"
        );
        assert_eq!(
            parse_audio_probe_json(r#"{"streams":[{"index":1}],"format":{"duration":"9.5"}}"#),
            Ok(Some(9.5)),
            "the container duration is the fallback"
        );
        assert_eq!(
            parse_audio_probe_json(r#"{"streams":[{"index":1}]}"#),
            Ok(Some(0.0)),
            "a stream with no duration is still present"
        );
        assert!(parse_audio_probe_json("not json").is_err());
    }

    #[test]
    fn a_video_without_an_audio_track_stays_viewable_and_silent() {
        if !media_tools_available() {
            eprintln!("skipping: ffmpeg/ffprobe are not installed");
            return;
        }

        let holder = temp_file().expect("a scratch clip holder");
        let clip = holder.path().to_path_buf();
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x64:rate=10:duration=1",
                "-c:v",
                "mpeg4",
                "-pix_fmt",
                "yuv420p",
                "-y",
                "-f",
                "mp4",
            ])
            .arg(&clip)
            .status()
            .expect("ffmpeg generates the silent test clip");
        assert!(status.success());

        let bytes = std::fs::read(&clip).unwrap();
        let mut engine = FfmpegAudioEngine::headless();
        assert_eq!(engine.load(&bytes), Ok(0.0));
        assert_eq!(engine.status(), PlayState::Unavailable);
        assert_eq!(engine.duration(), 0.0);
        assert!(
            engine.child.is_none(),
            "a silent video must not spawn an audio decoder at load"
        );

        // Play, pause and seek must all stay no-ops: there is no stream to
        // decode, and a silent video must never become an error.
        engine.play();
        assert_eq!(engine.status(), PlayState::Unavailable);
        assert!(
            engine.child.is_none(),
            "a silent video must not spawn an audio decoder when played"
        );
        engine.pause();
        assert_eq!(engine.status(), PlayState::Unavailable);
        engine.seek(5.0);
        assert_eq!(engine.status(), PlayState::Unavailable);
        assert_eq!(engine.position(), 0.0);

        engine.stop();
        assert_eq!(engine.status(), PlayState::Stopped);
        assert!(!engine.is_busy());
    }

    #[test]
    fn a_video_with_audio_streams_plays_and_seeks() {
        if !media_tools_available() {
            eprintln!("skipping: ffmpeg/ffprobe are not installed");
            return;
        }

        let holder = temp_file().expect("a scratch clip holder");
        let clip = holder.path().to_path_buf();
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x64:rate=10:duration=3",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=3",
                "-c:v",
                "mpeg4",
                "-c:a",
                "aac",
                "-pix_fmt",
                "yuv420p",
                "-shortest",
                "-y",
                "-f",
                "mp4",
            ])
            .arg(&clip)
            .status()
            .expect("ffmpeg generates the test clip");
        assert!(status.success());

        let bytes = std::fs::read(&clip).unwrap();
        let mut engine = FfmpegAudioEngine::headless();
        let duration = engine.load(&bytes).expect("the audio track loads");
        assert!((duration - 3.0).abs() < 0.2, "duration was {duration}");

        // Play past the point where the old Symphonia pipeline used to report
        // a false EOF, then seek and keep streaming from the new position.
        engine.play();
        wait_until(Duration::from_secs(5), || engine.position() > 1.2);
        assert_eq!(engine.status(), PlayState::Playing);

        engine.seek(2.0);
        wait_until(Duration::from_secs(5), || engine.position() > 2.2);
        assert_eq!(engine.status(), PlayState::Playing);

        engine.pause();
        assert_eq!(engine.status(), PlayState::Paused);
        assert!(
            engine.child.is_none(),
            "pause must reap the audio decoder instead of leaving it running"
        );
        assert!(
            engine.stderr_reader.is_none(),
            "pause must join the diagnostics drainer"
        );
        let paused_at = engine.position();
        std::thread::sleep(Duration::from_millis(100));
        assert!((engine.position() - paused_at).abs() < 1e-6);

        engine.stop();
        assert_eq!(engine.status(), PlayState::Stopped);
        assert!(
            engine.child.is_none(),
            "stop must leave no audio decoder child behind"
        );
        assert!(
            engine.stderr_reader.is_none(),
            "stop must join the diagnostics drainer"
        );
        assert!(!engine.is_busy());
    }

    #[test]
    fn dropping_a_playing_engine_kills_and_reaps_its_audio_decoder() {
        if !media_tools_available() {
            eprintln!("skipping: ffmpeg/ffprobe are not installed");
            return;
        }

        let holder = temp_file().expect("a scratch clip holder");
        let clip = holder.path().to_path_buf();
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x64:rate=10:duration=2",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-c:v",
                "mpeg4",
                "-c:a",
                "aac",
                "-pix_fmt",
                "yuv420p",
                "-shortest",
                "-y",
                "-f",
                "mp4",
            ])
            .arg(&clip)
            .status()
            .expect("ffmpeg generates the test clip");
        assert!(status.success());

        let bytes = std::fs::read(&clip).unwrap();
        let mut engine = FfmpegAudioEngine::headless();
        engine.load(&bytes).expect("the audio track loads");
        engine.play();
        wait_until(Duration::from_secs(5), || engine.position() > 0.1);

        assert!(
            engine.child.is_some(),
            "the test needs a live audio decoder to drop"
        );
        #[cfg(target_os = "linux")]
        let pid = engine.child.as_ref().unwrap().id();
        let scratch = engine.temp.as_ref().unwrap().path().to_path_buf();

        drop(engine);

        assert!(!scratch.exists(), "drop must unlink the scratch copy");
        #[cfg(target_os = "linux")]
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "drop must kill and reap the audio decoder child"
        );
    }
}
