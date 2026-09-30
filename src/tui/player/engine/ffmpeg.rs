//! ffmpeg-backed [`MediaEngine`] implementation.
//!
//! Video is decoded by spawning `ffmpeg` and reading a raw `rgb24` frame
//! stream off its stdout; there is no linked decoder, so an ffmpeg build the
//! user already has is the only requirement. Each decoded frame is published
//! into a shared "latest frame wins" slot and the UI is woken to draw it.
//!
//! Two properties of that pipeline drive the design:
//!
//! - `-ss` must go **before** `-i` for a fast keyframe seek, and a pipe is not
//!   seekable, so the container is spilled to a [`tempfile::NamedTempFile`]
//!   instead of being piped in. That file is owned by the engine: `stop` and
//!   `Drop` unlink it, EOF deliberately keeps it so the space bar can replay.
//! - `-fps_mode cfr -r <fps>` normalises variable-frame-rate sources, which is
//!   what makes `frame_index / fps` an exact presentation time and lets the
//!   decode thread pace itself against the wall clock instead of trusting the
//!   child to keep up.

use std::ffi::OsString;
use std::io::{self, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use image::{DynamicImage, RgbImage};
use tempfile::{Builder, NamedTempFile};
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};

use super::rodio::Timing;
use crate::tui::UiEvent;
use crate::tui::player::{MediaEngine, PlayState};

/// Terminal frame rates are far below 12 fps, so decoding a 60 fps source at
/// 60 fps only burns CPU on frames nobody sees. Also the fallback when a
/// container does not declare a frame rate.
const MAX_FPS: f64 = 12.0;

/// Bound on the decode width. The widget fits every frame to the popup area
/// afterwards, so decoding a 4K source at full size would only cost time.
const MAX_WIDTH: u32 = 1280;

/// The `ffprobe` command that reports the first video stream's geometry.
///
/// `-show_entries` is repeatable and ffprobe merges the flags, so the stream
/// and format durations come from one invocation.
fn probe_args(path: &Path) -> Vec<OsString> {
    [
        "-v",
        "quiet",
        "-print_format",
        "json",
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream=width,height,avg_frame_rate,duration,nb_frames",
        "-show_entries",
        "format=duration",
    ]
    .iter()
    .map(OsString::from)
    .chain(std::iter::once(path.as_os_str().to_os_string()))
    .collect()
}

/// The `ffmpeg` command that decodes from `position` into raw `rgb24` frames
/// on stdout.
///
/// The scale is explicit rather than an expression like `scale=-2` because the
/// reader has to know the exact byte count of a frame before reading it.
fn decode_args(path: &Path, position: f64, width: u32, height: u32, fps: f64) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["-hide_banner", "-loglevel", "error"]
        .iter()
        .map(OsString::from)
        .collect();

    // `-nostdin` is mandatory: the child shares the TUI's stdin and would
    // otherwise swallow keystrokes.
    args.push("-nostdin".into());

    if position > 0.0 {
        args.push("-ss".into());
        args.push(format!("{position:.3}").into());
    }

    args.push("-i".into());
    args.push(path.as_os_str().to_os_string());
    args.push("-an".into());
    args.push("-sn".into());
    args.push("-vf".into());
    args.push(format!("scale={width}:{height}").into());
    args.push("-fps_mode".into());
    args.push("cfr".into());
    args.push("-r".into());
    args.push(format!("{fps}").into());
    args.push("-f".into());
    args.push("rawvideo".into());
    args.push("-pix_fmt".into());
    args.push("rgb24".into());
    args.push("-".into());
    args
}

/// `avg_frame_rate` is a rational. `"0/0"` — and a zero numerator or
/// denominator — means the container does not declare a frame rate.
fn parse_frame_rate(raw: &str) -> Option<f64> {
    let (numerator, denominator) = raw.trim().split_once('/')?;
    let numerator = numerator.trim().parse::<f64>().ok()?;
    let denominator = denominator.trim().parse::<f64>().ok()?;

    (numerator > 0.0 && denominator > 0.0).then(|| numerator / denominator)
}

/// Fit the source inside `max_width` without upscaling, preserving the aspect
/// ratio. Dimensions are rounded to even numbers because a `yuv420p` source
/// scaled to an odd width trips chroma subsampling on some builds.
fn scaled_dimensions(src_width: u32, src_height: u32, max_width: u32) -> (u32, u32) {
    if src_width == 0 || src_height == 0 {
        return (0, 0);
    }

    let width = src_width.min(max_width);
    let scaled = u64::from(src_height) * u64::from(width) / u64::from(src_width);

    (
        width - (width % 2),
        ((scaled as u32) - ((scaled as u32) % 2)).max(2),
    )
}

/// Read geometry, frame rate and duration out of `ffprobe`'s JSON.
///
/// Only a video stream with usable dimensions is playable; a missing or
/// unparsable frame rate or duration falls back rather than failing the load,
/// because neither is needed to start decoding.
fn parse_probe_json(json: &str) -> Result<(u32, u32, f64, f64), String> {
    let probe: serde_json::Value = serde_json::from_str(json)
        .map_err(|err| format!("cannot read the ffprobe output: {err}"))?;

    let stream = probe["streams"]
        .as_array()
        .and_then(|streams| streams.first())
        .ok_or("the video has no decodable video stream")?;

    let width = stream["width"].as_u64().unwrap_or_default() as u32;
    let height = stream["height"].as_u64().unwrap_or_default() as u32;

    if width == 0 || height == 0 {
        return Err("the video stream reports no usable dimensions".into());
    }

    let fps = stream["avg_frame_rate"]
        .as_str()
        .and_then(parse_frame_rate)
        .map_or(MAX_FPS, |rate| rate.min(MAX_FPS));

    // A stream's own duration is more accurate than the container's, but many
    // mp4s only fill one of the two.
    let duration = stream["duration"]
        .as_str()
        .or_else(|| probe["format"]["duration"].as_str())
        .and_then(|raw| raw.trim().parse::<f64>().ok())
        .filter(|secs| *secs > 0.0)
        .unwrap_or(0.0);

    Ok((width, height, fps, duration))
}

/// Fill `buf` from `reader`, reporting how many bytes arrived before the pipe
/// closed. `read_exact` cannot be used here because a decode ending exactly on
/// a frame boundary must be told apart from a truncated final frame.
fn read_full(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;

    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }

    Ok(filled)
}

/// The scratch file holding the container for the current session.
///
/// Created owner-only; `NamedTempFile` unlinks it when dropped, which is what
/// makes `stop` the "forget the session" path. The extension is cosmetic —
/// ffmpeg probes the container from content.
fn temp_file() -> io::Result<NamedTempFile> {
    Builder::new()
        .prefix("senders-video-")
        .suffix(".bin")
        .tempfile()
}

/// The concrete [`MediaEngine`] for video.
///
/// `load` spills the container to a temp file and probes it, leaving the
/// session paused; `play` spawns `ffmpeg` at the current position and a decode
/// thread publishes frames into the shared slot. Every transition that ends a
/// running decode (`pause`, `seek`, `stop`, `load`, `Drop`) kills the child and
/// joins its thread, so at most one `ffmpeg` exists per engine. Ending at EOF
/// reports `Stopped` and keeps the temp file, so the space bar replays from the
/// beginning; an explicit `stop` forgets the session and deletes the file.
pub struct FfmpegEngine {
    /// The loaded container, or `None` once the session is forgotten.
    temp: Option<NamedTempFile>,
    /// Scaled decode dimensions; one frame is exactly `width * height * 3`
    /// bytes, which is what lets the reader size its buffer up front.
    width: u32,
    height: u32,
    fps: f64,
    duration: f64,
    /// The running decode, if any.
    child: Option<Child>,
    reader: Option<JoinHandle<()>>,
    /// Set by the decode thread when the child ended (clean EOF) or died.
    /// The engine only reads these while it still considers itself playing, so
    /// a decode killed by `pause`/`stop` cannot be mistaken for a failure.
    ended: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    timing: Timing,
    status: PlayState,
    frame_slot: Arc<Mutex<Option<DynamicImage>>>,
    wake: UnboundedSender<UiEvent>,
}

impl FfmpegEngine {
    /// Build an engine publishing decoded frames into `frame_slot` and asking
    /// the UI to redraw through `wake`. Both are handed over rather than
    /// wired in later because the player builds its engine on a worker thread.
    pub fn new(
        frame_slot: Arc<Mutex<Option<DynamicImage>>>,
        wake: UnboundedSender<UiEvent>,
    ) -> Self {
        Self {
            temp: None,
            width: 0,
            height: 0,
            fps: MAX_FPS,
            duration: 0.0,
            child: None,
            reader: None,
            ended: Arc::new(AtomicBool::new(false)),
            failed: Arc::new(AtomicBool::new(false)),
            timing: Timing::default(),
            status: PlayState::Stopped,
            frame_slot,
            wake,
        }
    }

    /// Kill the running decode and wait for its reader thread, if any.
    fn kill_decode(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }

        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }

    /// Spawn `ffmpeg` at `position` plus the thread that paces its frames into
    /// the slot.
    fn spawn_decode(&mut self, path: &Path, position: f64) -> Result<(), String> {
        let mut child = Command::new("ffmpeg")
            .args(decode_args(
                path,
                position,
                self.width,
                self.height,
                self.fps,
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // The process stderr is redirected to the log, so ffmpeg's own
            // diagnostics are captured instead of corrupting the TUI.
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|err| format!("cannot run ffmpeg: {err}"))?;

        let Some(mut stdout) = child.stdout.take() else {
            let _ = child.kill();
            return Err("ffmpeg was spawned without a frame pipe".into());
        };

        // Fresh per-session flags: a decode the engine itself killed has
        // already been joined, so it cannot write to these.
        self.ended = Arc::new(AtomicBool::new(false));
        self.failed = Arc::new(AtomicBool::new(false));

        let (width, height, fps) = (self.width, self.height, self.fps);
        let slot = Arc::clone(&self.frame_slot);
        let wake = self.wake.clone();
        let (ended, failed) = (Arc::clone(&self.ended), Arc::clone(&self.failed));

        self.reader = Some(std::thread::spawn(move || {
            let frame_bytes = width as usize * height as usize * 3;
            let mut index = 0_u64;
            let started = Instant::now();

            loop {
                // `RgbImage::from_raw` takes ownership of the pixels, so the
                // buffer is per frame rather than reused.
                let mut frame = vec![0u8; frame_bytes];

                match read_full(&mut stdout, &mut frame) {
                    // The pipe closed on a frame boundary: a clean end of
                    // stream, which is not a failure.
                    Ok(0) => break,
                    Ok(filled) if filled == frame_bytes => {}
                    Ok(_) => {
                        warn!("video decode ended mid-frame");
                        failed.store(true, Ordering::Relaxed);
                        return;
                    }
                    Err(err) => {
                        debug!(error = %err, "video frame pipe closed early");
                        failed.store(true, Ordering::Relaxed);
                        return;
                    }
                }

                if let Some(image) = RgbImage::from_raw(width, height, frame) {
                    // A busy UI thread means the previous frame has not been
                    // encoded yet; dropping this one keeps the decoder from
                    // stalling behind the renderer.
                    if let Ok(mut current) = slot.try_lock() {
                        *current = Some(DynamicImage::ImageRgb8(image));
                        let _ = wake.send(UiEvent::Redraw);
                    }
                }

                index += 1;
                let due = started + Duration::from_secs_f64(index as f64 / fps);
                let now = Instant::now();
                if due > now {
                    std::thread::sleep(due - now);
                }
            }

            debug!(
                position_secs = index as f64 / fps,
                "video reached end of stream"
            );
            ended.store(true, Ordering::Relaxed);
        }));

        self.child = Some(child);
        Ok(())
    }
}

impl MediaEngine for FfmpegEngine {
    fn load(&mut self, bytes: &[u8]) -> Result<f64, String> {
        self.kill_decode();

        let loaded = (|| -> Result<(NamedTempFile, u32, u32, f64, f64), String> {
            let temp = temp_file()
                .map_err(|err| format!("cannot create a temporary file for the video: {err}"))?;
            std::fs::write(temp.path(), bytes)
                .map_err(|err| format!("cannot write the video to disk: {err}"))?;

            let output = Command::new("ffprobe")
                .args(probe_args(temp.path()))
                .stdin(Stdio::null())
                .output()
                .map_err(|err| format!("cannot run ffprobe: {err}"))?;

            if !output.status.success() {
                let reason = String::from_utf8_lossy(&output.stderr);
                let reason = reason.lines().last().unwrap_or("unknown failure").trim();
                return Err(format!("ffprobe could not read the video: {reason}"));
            }

            let (src_width, src_height, fps, duration) =
                parse_probe_json(&String::from_utf8_lossy(&output.stdout))?;
            let (width, height) = scaled_dimensions(src_width, src_height, MAX_WIDTH);

            Ok((temp, width, height, fps, duration))
        })();

        let (temp, width, height, fps, duration) = match loaded {
            Ok(loaded) => loaded,
            Err(err) => {
                // `temp` never escaped the closure, so the file (if it was
                // created) is already unlinked by the time we get here.
                self.temp = None;
                self.width = 0;
                self.height = 0;
                self.duration = 0.0;
                self.timing.stop();
                self.status = PlayState::Error;
                warn!(error = %err, "video load failed");
                return Err(err);
            }
        };

        debug!(
            width,
            height, fps, duration, "video loaded, paused at the start"
        );

        // The popup shows the previous session's last frame until the new
        // decode publishes one, so clear it with the new media.
        if let Ok(mut slot) = self.frame_slot.try_lock() {
            *slot = None;
        }
        self.temp = Some(temp);
        self.width = width;
        self.height = height;
        self.fps = fps;
        self.duration = duration;
        self.timing.stop();
        self.status = PlayState::Paused;

        Ok(duration)
    }

    fn play(&mut self) {
        let Some(temp) = &self.temp else {
            self.status = PlayState::Stopped;
            return;
        };

        // A session that reached EOF reported `Stopped` but kept its file and
        // its end position, so the space bar means "play it again".
        if self.status == PlayState::Stopped {
            self.timing.seek(0.0, false);
        }

        let path = temp.path().to_path_buf();
        let position = self.timing.current(self.duration);

        self.kill_decode();

        match self.spawn_decode(&path, position) {
            Ok(()) => {
                self.timing.play();
                self.status = PlayState::Playing;
                debug!(position_secs = position, "video playing");
            }
            Err(err) => {
                warn!(error = %err, "video playback could not start");
                self.status = PlayState::Error;
            }
        }
    }

    fn pause(&mut self) {
        if self.status != PlayState::Playing {
            return;
        }

        self.kill_decode();
        self.timing.pause(self.duration);
        self.status = PlayState::Paused;
    }

    fn seek(&mut self, position_secs: f64) {
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
        self.timing.seek(target, playing);

        // A paused session rebases its clock and shows the frame the next
        // `play` decodes; a playing one has to be respawned at the new offset,
        // since the child cannot be told to jump in its output stream.
        if playing && let Err(err) = self.spawn_decode(&path, target) {
            warn!(error = %err, "video seek could not restart the decode");
            self.status = PlayState::Error;
        }
    }

    fn stop(&mut self) {
        if self.status == PlayState::Playing || self.temp.is_some() {
            debug!("stopping video session");
        }

        self.kill_decode();
        // Dropping the `NamedTempFile` unlinks the scratch copy.
        self.temp = None;
        self.width = 0;
        self.height = 0;
        self.duration = 0.0;
        self.timing.stop();
        self.status = PlayState::Stopped;

        // Forget the last picture too, so a stale frame cannot outlive the
        // session it belongs to.
        if let Ok(mut slot) = self.frame_slot.try_lock() {
            *slot = None;
        }
    }

    fn is_busy(&self) -> bool {
        self.temp.is_some() || self.status == PlayState::Playing
    }

    fn play_cue(&mut self, _bytes: &[u8]) -> Result<(), String> {
        // Cues are an audio concern and the video player never owns the audio
        // output device, so a cue cannot reach it. The audio engine is the one
        // that plays them.
        debug!("notification cue ignored by the video engine");
        Ok(())
    }

    fn position(&self) -> f64 {
        self.timing.current(self.duration)
    }

    fn duration(&self) -> f64 {
        self.duration
    }

    fn status(&mut self) -> PlayState {
        if self.status == PlayState::Playing && self.failed.load(Ordering::Relaxed) {
            self.kill_decode();
            self.timing.pause(self.duration);
            self.status = PlayState::Error;
        } else if self.status == PlayState::Playing && self.ended.load(Ordering::Relaxed) {
            // EOF: the session is finished but its file is kept, so a later
            // `play` restarts it from the beginning.
            self.kill_decode();
            self.timing.pause(self.duration);
            self.status = PlayState::Stopped;
        }

        self.status
    }
}

impl Drop for FfmpegEngine {
    fn drop(&mut self) {
        self.kill_decode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn args_of(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    /// CI installs no ffmpeg, so every test that actually decodes has to opt
    /// out rather than fail.
    fn ffmpeg_available() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[test]
    fn temp_file_is_owner_only_and_named_for_senders() {
        let temp = temp_file().expect("a scratch file can be created");

        let name = temp.path().file_name().unwrap().to_string_lossy();
        assert!(
            name.starts_with("senders-video-") && name.ends_with(".bin"),
            "unexpected scratch file name {name}"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = temp.path().metadata().unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "a message's video must not be world-readable"
            );
        }

        let path = temp.path().to_path_buf();
        drop(temp);
        assert!(!path.exists(), "the scratch file must be unlinked on drop");
    }

    #[test]
    fn probe_args_match_the_documented_command() {
        let args = args_of(&probe_args(Path::new("/tmp/clip.bin")));

        assert_eq!(
            args,
            vec![
                "-v",
                "quiet",
                "-print_format",
                "json",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=width,height,avg_frame_rate,duration,nb_frames",
                "-show_entries",
                "format=duration",
                "/tmp/clip.bin",
            ]
        );
    }

    #[test]
    fn decode_args_carry_the_scale_rate_and_paced_output() {
        let args = args_of(&decode_args(
            Path::new("/tmp/clip.bin"),
            1.5,
            320,
            240,
            12.0,
        ));

        assert_eq!(
            args,
            vec![
                "-hide_banner",
                "-loglevel",
                "error",
                // The child shares the TUI's stdin, so it must never read it.
                "-nostdin",
                "-ss",
                "1.500",
                "-i",
                "/tmp/clip.bin",
                "-an",
                "-sn",
                "-vf",
                "scale=320:240",
                "-fps_mode",
                "cfr",
                "-r",
                "12",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgb24",
                "-",
            ]
        );
    }

    #[test]
    fn decode_args_omit_a_seek_at_the_start() {
        let args = args_of(&decode_args(Path::new("/tmp/clip.bin"), 0.0, 64, 48, 12.0));

        assert!(!args.contains(&"-ss".to_string()));
    }

    #[test]
    fn frame_rate_parser_handles_rationals_and_the_unset_case() {
        assert_eq!(parse_frame_rate("25/1"), Some(25.0));
        assert_eq!(parse_frame_rate("30000/1001"), Some(30000.0 / 1001.0));
        // What ffprobe prints for a container with no declared rate.
        assert_eq!(parse_frame_rate("0/0"), None);
        assert_eq!(parse_frame_rate("25/0"), None);
        assert_eq!(parse_frame_rate("25"), None);
        assert_eq!(parse_frame_rate(""), None);
    }

    #[test]
    fn scaled_dimensions_keep_the_aspect_ratio_and_stay_even() {
        assert_eq!(scaled_dimensions(320, 240, 1280), (320, 240));
        // A 1080p source is bounded by width, not by an arbitrary cap on both.
        assert_eq!(scaled_dimensions(1920, 1080, 1280), (1280, 720));
        // Never upscale a small source.
        assert_eq!(scaled_dimensions(160, 120, 1280), (160, 120));
        // Odd dimensions round down to an even width/height pair.
        assert_eq!(scaled_dimensions(641, 481, 1280), (640, 480));
        assert_eq!(scaled_dimensions(1921, 1081, 1280), (1280, 720));
        // A degenerate source has no playable dimensions.
        assert_eq!(scaled_dimensions(0, 240, 1280), (0, 0));
        assert_eq!(scaled_dimensions(320, 0, 1280), (0, 0));
    }

    #[test]
    fn probe_json_yields_geometry_rate_and_duration() {
        // The exact shape `ffprobe -print_format json` produces for the
        // documented probe command.
        let json = r#"{
  "streams": [
    {
      "width": 320,
      "height": 240,
      "avg_frame_rate": "25/1",
      "duration": "2.000000",
      "nb_frames": "50"
    }
  ],
  "format": { "duration": "2.000000" }
}"#;

        let (width, height, fps, duration) = parse_probe_json(json).unwrap();
        assert_eq!((width, height), (320, 240));
        assert_eq!(fps, 12.0, "decode is capped at terminal frame rates");
        assert_eq!(duration, 2.0);
    }

    #[test]
    fn probe_json_falls_back_for_rate_and_duration() {
        // A container with no stream duration and no declared frame rate: the
        // rate falls back to the cap and the duration to "unknown" (0.0).
        let json = r#"{
  "streams": [ { "width": 640, "height": 480, "avg_frame_rate": "0/0" } ],
  "format": { "duration": "0.000000" }
}"#;
        let (width, height, fps, duration) = parse_probe_json(json).unwrap();
        assert_eq!((width, height), (640, 480));
        assert_eq!(fps, MAX_FPS);
        assert_eq!(duration, 0.0);

        // The format duration is used when the stream has none.
        let json = r#"{
  "streams": [ { "width": 640, "height": 480, "avg_frame_rate": "30/1" } ],
  "format": { "duration": "7.500000" }
}"#;
        let (_, _, fps, duration) = parse_probe_json(json).unwrap();
        assert_eq!(fps, MAX_FPS, "30 fps is still above the terminal cap");
        assert_eq!(duration, 7.5);
    }

    #[test]
    fn probe_json_rejects_a_media_file_with_no_video_stream() {
        assert!(parse_probe_json(r#"{"streams": [], "format": {}}"#).is_err());
        assert!(parse_probe_json(r#"{"streams": [{"width": 0, "height": 0}]}"#).is_err());
        assert!(parse_probe_json("not json").is_err());
    }

    #[test]
    fn undecodable_bytes_report_an_error_status() {
        let mut engine = FfmpegEngine::new(
            Arc::new(Mutex::new(None)),
            tokio::sync::mpsc::unbounded_channel().0,
        );

        // The engine must never look loaded after a failed load, or the popup
        // would offer playback that cannot work.
        assert!(engine.load(b"definitely not a video").is_err());
        assert_eq!(engine.status(), PlayState::Error);
        assert!(!engine.is_busy());
        assert_eq!(engine.duration(), 0.0);

        // Playing without a session is inert: the engine drops back to
        // `Stopped` exactly as the audio engine does with nothing loaded.
        engine.play();
        assert_eq!(engine.status(), PlayState::Stopped);
    }

    #[test]
    fn a_generated_clip_loads_plays_seeks_and_deletes_its_scratch_file() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg is not installed");
            return;
        }

        // Nothing binary is committed: the clip is synthesised on the spot.
        // The scratch file doubles as the clip: ffmpeg probes the container
        // from content, and reusing the path means `clip` is unlinked with it.
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
                "testsrc=size=320x240:rate=25:duration=2",
                "-pix_fmt",
                "yuv420p",
                // The scratch file has no meaningful extension, so the muxer
                // has to be named; and `temp_file` already created it, which
                // ffmpeg refuses to overwrite unless told to (it exits 0 when
                // it declines, so a status check alone would not catch it).
                "-y",
                "-f",
                "mp4",
            ])
            .arg(&clip)
            .status()
            .expect("ffmpeg generates the test clip");
        assert!(status.success(), "ffmpeg failed to build the test clip");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let slot = Arc::new(Mutex::new(None));
        let mut engine = FfmpegEngine::new(Arc::clone(&slot), tx);

        let bytes = std::fs::read(&clip).unwrap();
        assert!(!bytes.is_empty(), "the generated clip is empty");
        let duration = engine.load(&bytes).unwrap();
        assert!(
            (duration - 2.0).abs() < 0.2,
            "unexpected probe duration {duration}"
        );
        assert_eq!(engine.status(), PlayState::Paused);
        assert!(engine.is_busy());

        let scratch = engine.temp.as_ref().unwrap().path().to_path_buf();
        assert!(scratch.exists(), "the container is spilled for the seek");

        engine.play();
        assert_eq!(engine.status(), PlayState::Playing);

        // The decoder paces itself, so wait for real frames rather than a tick.
        wait_until(Duration::from_secs(10), || {
            let _ = rx.try_recv();
            slot.try_lock()
                .map(|frame| frame.is_some())
                .unwrap_or(false)
        });
        let first = slot.try_lock().unwrap().take().unwrap();
        assert_eq!(first.to_rgb8().dimensions(), (320, 240));
        assert_eq!(first.to_rgb8().into_raw().len(), 320 * 240 * 3);

        wait_until(Duration::from_secs(5), || engine.position() > 0.2);

        engine.pause();
        let frozen = engine.position();
        assert_eq!(engine.status(), PlayState::Paused);
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            (engine.position() - frozen).abs() < 1e-6,
            "a paused video must freeze its position"
        );

        // A paused seek only rebases the clock; the file stays on disk and the
        // frozen picture stays on screen.
        engine.seek(1.0);
        assert!((engine.position() - 1.0).abs() < 1e-6);
        assert!(scratch.exists());

        // A seek while playing cannot rewind the child's output stream, so the
        // decode has to be killed and respawned at the new offset.
        engine.play();
        engine.seek(1.0);
        wait_until(Duration::from_secs(10), || engine.position() > 1.2);

        engine.stop();
        assert_eq!(engine.status(), PlayState::Stopped);
        assert!(!scratch.exists(), "stopping must delete the scratch file");
        assert!(!engine.is_busy());
        assert_eq!(engine.position(), 0.0);
    }

    #[test]
    fn a_video_that_reaches_the_end_can_be_replayed() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg is not installed");
            return;
        }

        // The scratch file doubles as the clip: ffmpeg probes the container
        // from content, and reusing the path means `clip` is unlinked with it.
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
                "testsrc=size=64x64:rate=25:duration=1",
                "-pix_fmt",
                "yuv420p",
                // The scratch file has no meaningful extension, so the muxer
                // has to be named; and `temp_file` already created it, which
                // ffmpeg refuses to overwrite unless told to (it exits 0 when
                // it declines, so a status check alone would not catch it).
                "-y",
                "-f",
                "mp4",
            ])
            .arg(&clip)
            .status()
            .expect("ffmpeg generates the test clip");
        assert!(status.success());

        let bytes = std::fs::read(&clip).unwrap();
        assert!(!bytes.is_empty(), "the generated clip is empty");
        let mut engine = FfmpegEngine::new(
            Arc::new(Mutex::new(None)),
            tokio::sync::mpsc::unbounded_channel().0,
        );
        engine.load(&bytes).unwrap();
        engine.play();

        // EOF is reported through `status`, like rodio's emptied sink.
        wait_until(Duration::from_secs(15), || {
            engine.status() == PlayState::Stopped
        });
        assert!(
            (engine.position() - engine.duration()).abs() < 0.5,
            "the position should sit at the end, got {}",
            engine.position()
        );

        // The space bar after the end restarts it rather than doing nothing.
        engine.play();
        assert_eq!(engine.status(), PlayState::Playing);
        assert!(
            engine.position() < 0.5,
            "a replay starts from the beginning"
        );

        engine.stop();
    }

    #[test]
    fn play_cue_leaves_the_video_session_alone() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg is not installed");
            return;
        }

        // The scratch file doubles as the clip: ffmpeg probes the container
        // from content, and reusing the path means `clip` is unlinked with it.
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
                "testsrc=size=64x64:rate=25:duration=1",
                "-pix_fmt",
                "yuv420p",
                // The scratch file has no meaningful extension, so the muxer
                // has to be named; and `temp_file` already created it, which
                // ffmpeg refuses to overwrite unless told to (it exits 0 when
                // it declines, so a status check alone would not catch it).
                "-y",
                "-f",
                "mp4",
            ])
            .arg(&clip)
            .status()
            .expect("ffmpeg generates the test clip");
        assert!(status.success());

        let bytes = std::fs::read(&clip).unwrap();
        assert!(!bytes.is_empty(), "the generated clip is empty");
        let mut engine = FfmpegEngine::new(
            Arc::new(Mutex::new(None)),
            tokio::sync::mpsc::unbounded_channel().0,
        );
        engine.load(&bytes).unwrap();

        let position = engine.position();
        let scratch = engine.temp.as_ref().unwrap().path().to_path_buf();
        engine.play_cue(b"cue bytes").unwrap();

        assert_eq!(engine.status(), PlayState::Paused, "a cue is invisible");
        assert_eq!(engine.position(), position);
        assert!(scratch.exists(), "a cue must not forget the session");
    }
}
