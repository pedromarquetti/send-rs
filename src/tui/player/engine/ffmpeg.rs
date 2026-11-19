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
use std::io::{self, BufRead, BufReader, Read};
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
use crate::tui::player::{MediaEngine, PlayState};
use crate::tui::{UiEvent, video_redraw_wake};

/// Cap on the decode frame rate: a source faster than this is decoded at this
/// rate instead, so a high-fps source does not burn CPU on frames the terminal
/// never shows. Also the fallback when a container does not declare a frame
/// rate.
/// NOTE: increasing this actually improved video playback, maybe we should confirm again if this
/// makes any difference
const MAX_FPS: f64 = 60.0;

/// Bound on the decode width. The widget fits every frame to the popup area
/// afterwards, so decoding a 4K source at full size would only cost time.
const MAX_WIDTH: u32 = 1280;

/// How long `load` waits for the poster frame's decode before giving up and
/// leaving the popup on its placeholder.
const POSTER_FRAME_TIMEOUT: Duration = Duration::from_secs(5);

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
        "stream_side_data=rotation",
        "-show_entries",
        "stream_tags=rotate",
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
///
/// Autorotation is turned off and the container's display rotation is applied
/// with an explicit filter instead, before the scale and against the displayed
/// (post-rotation) geometry. `-noautorotate` is an input option, so it must
/// precede the `-i` it applies to.
fn decode_args(
    path: &Path,
    position: f64,
    width: u32,
    height: u32,
    fps: f64,
    rotation: i32,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["-hide_banner", "-loglevel", "error"]
        .iter()
        .map(OsString::from)
        .collect();

    // `-nostdin` is mandatory: the child shares the TUI's stdin and would
    // otherwise swallow keystrokes.
    args.push("-nostdin".into());

    // The explicit `orientation_filter` below is the only rotation; leaving
    // ffmpeg's own autorotation on would rotate a rotated source twice.
    args.push("-noautorotate".into());

    if position > 0.0 {
        args.push("-ss".into());
        args.push(format!("{position:.3}").into());
    }

    args.push("-i".into());
    args.push(path.as_os_str().to_os_string());
    // Deliberately video-only: this pipe carries raw rgb24 frames, which
    // cannot interleave audio. The container's first audio stream is decoded
    // independently by `FfmpegAudioEngine`, which has its own fallback when the
    // track is missing or unsupported.
    args.push("-an".into());
    args.push("-sn".into());
    args.push("-vf".into());
    let scale = format!("scale={width}:{height}");
    let filter = match orientation_filter(rotation) {
        Some(orientation) => format!("{orientation},{scale}"),
        None => scale,
    };
    args.push(filter.into());
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

/// The ffmpeg filter that undoes the container's display rotation, or `None`
/// when the frame is already upright.
fn orientation_filter(rotation: i32) -> Option<&'static str> {
    match rotation.rem_euclid(360) {
        90 => Some("transpose=2"),
        180 => Some("hflip,vflip"),
        270 => Some("transpose=1"),
        _ => None,
    }
}

/// The dimensions a frame is displayed at once the container's rotation is
/// applied: a quarter turn swaps the coded width and height.
fn display_dimensions(width: u32, height: u32, rotation: i32) -> (u32, u32) {
    if matches!(rotation.rem_euclid(360), 90 | 270) {
        (height, width)
    } else {
        (width, height)
    }
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

    // Both sides are pinned to at least 2: even-rounding a one-pixel edge down
    // to zero would make `scale` reject the graph.
    (
        (width - (width % 2)).max(2),
        ((scaled as u32) - ((scaled as u32) % 2)).max(2),
    )
}

/// Container-declared display rotation, in degrees.
///
/// Modern mp4/mov store it as `Display Matrix` side data; older containers use
/// a `rotate` tag. A value that is absent, unparsable or not a quarter turn
/// reads as no rotation.
fn parse_rotation(stream: &serde_json::Value) -> i32 {
    let matrix = stream["side_data_list"]
        .as_array()
        .and_then(|sides| sides.iter().find_map(|side| side["rotation"].as_f64()));
    let tag = stream["tags"]["rotate"]
        .as_str()
        .and_then(|raw| raw.trim().parse::<f64>().ok());

    matrix
        .or(tag)
        .map(|deg| ((deg / 90.0).round() as i32 * 90).rem_euclid(360))
        .unwrap_or(0)
}

/// Read coded geometry, frame rate, duration and display rotation out of
/// `ffprobe`'s JSON.
///
/// Only a video stream with usable dimensions is playable; a missing or
/// unparsable frame rate or duration falls back rather than failing the load,
/// because neither is needed to start decoding.
fn parse_probe_json(json: &str) -> Result<(u32, u32, f64, f64, i32), String> {
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

    let rotation = parse_rotation(stream);

    // A stream's own duration is more accurate than the container's, but many
    // mp4s only fill one of the two.
    let duration = stream["duration"]
        .as_str()
        .or_else(|| probe["format"]["duration"].as_str())
        .and_then(|raw| raw.trim().parse::<f64>().ok())
        .filter(|secs| *secs > 0.0)
        .unwrap_or(0.0);

    Ok((width, height, fps, duration, rotation))
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
pub(super) fn temp_file() -> io::Result<NamedTempFile> {
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
    /// Display rotation declared by the container, in degrees; applied as an
    /// explicit filter so the decoded frames are upright.
    rotation: i32,
    fps: f64,
    duration: f64,
    /// The running decode, if any.
    child: Option<Child>,
    reader: Option<JoinHandle<()>>,
    /// Drains the decode child's stderr into the application log, so ffmpeg
    /// diagnostics never reach the terminal and never block on a full pipe.
    stderr_reader: Option<JoinHandle<()>>,
    /// Set by the decode thread when its frame pipe ends (`ended`) or breaks
    /// mid-frame (`failed`). `ended` means only that stdout is exhausted; the
    /// engine still checks the child's exit status before calling it a success.
    /// The engine only reads these while it still considers itself playing, so
    /// a decode killed by `pause`/`stop` cannot be mistaken for a failure.
    ended: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    timing: Timing,
    status: PlayState,
    frame_slot: Arc<Mutex<Option<DynamicImage>>>,
    wake: UnboundedSender<UiEvent>,
    /// Coalescing latch shared with the event loop: `video_redraw_wake` only
    /// sends a `UiEvent::Redraw` when no video wake is already pending, so a
    /// renderer that falls behind drops obsolete wakes instead of buffering
    /// one per decoded frame.
    redraw_pending: Arc<AtomicBool>,
}

impl FfmpegEngine {
    /// Build an engine publishing decoded frames into `frame_slot` and asking
    /// the UI to redraw through `wake`. Both are handed over rather than
    /// wired in later because the player builds its engine on a worker thread;
    /// `redraw_pending` is the same latch the event loop re-arms on every
    /// received wake.
    pub fn new(
        frame_slot: Arc<Mutex<Option<DynamicImage>>>,
        wake: UnboundedSender<UiEvent>,
        redraw_pending: Arc<AtomicBool>,
    ) -> Self {
        Self {
            temp: None,
            width: 0,
            height: 0,
            rotation: 0,
            fps: MAX_FPS,
            duration: 0.0,
            child: None,
            reader: None,
            stderr_reader: None,
            ended: Arc::new(AtomicBool::new(false)),
            failed: Arc::new(AtomicBool::new(false)),
            timing: Timing::default(),
            status: PlayState::Stopped,
            frame_slot,
            wake,
            redraw_pending,
        }
    }

    /// Decode a single frame at the start into the slot, then stop.
    ///
    /// `load` deliberately does not start playback, and nothing else decodes
    /// before the first `play`, so without this the popup would sit on its
    /// "Loading video…" placeholder until the user pressed space — the frame
    /// has to be on screen *before* playback starts, not as a side effect of it.
    /// The timing clock is left stopped, so the session still reports paused at
    /// zero and the space bar begins playback from the top.
    fn preroll_poster_frame(&mut self) {
        let Some(path) = self.temp.as_ref().map(|temp| temp.path().to_path_buf()) else {
            return;
        };

        if let Err(err) = self.spawn_decode(&path, 0.0) {
            // Leave the session paused and let the popup keep showing its
            // placeholder; the first `play` retries and surfaces the error.
            warn!(error = %err, "video poster frame could not be decoded");
            self.kill_decode();
            return;
        }

        let deadline = Instant::now() + POSTER_FRAME_TIMEOUT;
        while Instant::now() < deadline {
            if self.frame_slot.try_lock().is_ok_and(|slot| slot.is_some()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        // Whether the frame arrived or not, the decode is torn down here: this
        // exists to seed the slot, not to run.
        self.kill_decode();

        debug!("video poster frame ready, still paused at the start");
    }

    /// Kill the running decode and wait for its reader threads, if any.
    fn kill_decode(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }

        self.join_decode_threads();
    }

    /// Wait out the frame reader and the stderr drainer.
    ///
    /// Separate from [`Self::kill_decode`] because a decode that ended on its
    /// own has already been reaped, and re-sending a signal to a reaped pid
    /// could reach an unrelated process.
    fn join_decode_threads(&mut self) {
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(stderr_reader) = self.stderr_reader.take() {
            let _ = stderr_reader.join();
        }
    }

    /// Clear the shared frame slot, waiting out whoever holds it.
    fn clear_frame(&self) {
        let mut slot = loop {
            if let Ok(slot) = self.frame_slot.try_lock() {
                break slot;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        *slot = None;
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
                self.rotation,
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // Never inherit the terminal: ffmpeg's diagnostics must not paint
            // over the alternate-screen TUI. They are piped and drained into
            // the application log instead.
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| format!("cannot run ffmpeg: {err}"))?;

        let Some(mut stdout) = child.stdout.take() else {
            let _ = child.kill();
            return Err("ffmpeg was spawned without a frame pipe".into());
        };
        let stderr = child.stderr.take();

        // Fresh per-session flags: a decode the engine itself killed has
        // already been joined, so it cannot write to these.
        self.ended = Arc::new(AtomicBool::new(false));
        self.failed = Arc::new(AtomicBool::new(false));

        let (width, height, fps) = (self.width, self.height, self.fps);
        let slot = Arc::clone(&self.frame_slot);
        let wake = self.wake.clone();
        let redraw_pending = Arc::clone(&self.redraw_pending);
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
                        video_redraw_wake(&wake, &redraw_pending);
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

        // Draining stderr keeps the pipe from filling and blocking ffmpeg, and
        // routes its diagnostics into `sender.log` rather than the terminal.
        if let Some(stderr) = stderr {
            self.stderr_reader = Some(std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let line = line.trim_end();
                    if !line.is_empty() {
                        warn!(diagnostic = line, "ffmpeg reported a problem");
                    }
                }
            }));
        }

        self.child = Some(child);
        Ok(())
    }
}

impl MediaEngine for FfmpegEngine {
    fn load(&mut self, bytes: &[u8]) -> Result<f64, String> {
        self.kill_decode();

        let loaded = (|| -> Result<(NamedTempFile, u32, u32, f64, f64, i32), String> {
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

            let (src_width, src_height, fps, duration, rotation) =
                parse_probe_json(&String::from_utf8_lossy(&output.stdout))?;
            // The scale targets the displayed geometry: autorotation is off, so
            // the explicit rotation filter below turns the coded frame before
            // it reaches `scale`.
            let (display_width, display_height) =
                display_dimensions(src_width, src_height, rotation);
            let (width, height) = scaled_dimensions(display_width, display_height, MAX_WIDTH);

            Ok((temp, width, height, fps, duration, rotation))
        })();

        let (temp, width, height, fps, duration, rotation) = match loaded {
            Ok(loaded) => loaded,
            Err(err) => {
                // `temp` never escaped the closure, so the file (if it was
                // created) is already unlinked by the time we get here.
                self.temp = None;
                self.width = 0;
                self.height = 0;
                self.rotation = 0;
                self.duration = 0.0;
                self.timing.stop();
                self.status = PlayState::Error;
                warn!(error = %err, "video load failed");
                return Err(err);
            }
        };

        debug!(
            width,
            height, rotation, fps, duration, "video loaded, paused at the start"
        );

        // The popup shows the previous session's last frame until the new
        // decode publishes one, so clear it with the new media.
        self.clear_frame();
        self.temp = Some(temp);
        self.width = width;
        self.height = height;
        self.rotation = rotation;
        self.fps = fps;
        self.duration = duration;
        self.timing.stop();
        self.status = PlayState::Paused;

        // Seed the slot so the popup can show a frame while still paused.
        self.preroll_poster_frame();

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

        if playing {
            if let Err(err) = self.spawn_decode(&path, target) {
                warn!(error = %err, "video seek could not restart the decode");
                self.status = PlayState::Error;
            }
            return;
        }

        // A paused session must show the frame at the sought position: decode
        // exactly one frame, publish it, and stop the decode again.
        if let Err(err) = self.spawn_decode(&path, target) {
            warn!(error = %err, "paused seek could not decode a frame");
            self.status = PlayState::Error;
            return;
        }

        let deadline = Instant::now() + POSTER_FRAME_TIMEOUT;
        while Instant::now() < deadline {
            if self.frame_slot.try_lock().is_ok_and(|slot| slot.is_some()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        self.kill_decode();
        // status remains `PlayState::Paused`.
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
        self.fps = MAX_FPS;
        self.duration = 0.0;
        self.timing.stop();
        self.status = PlayState::Stopped;

        // Forget the last picture too, so a stale frame cannot outlive the
        // session it belongs to. This waits for a busy slot rather than
        // skipping the clear: the popup is going away and a frame left in
        // the slot would resurface in the next one.
        self.clear_frame();
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
        if self.status != PlayState::Playing {
            return self.status;
        }

        if self.failed.load(Ordering::Relaxed) {
            self.kill_decode();
            self.timing.pause(self.duration);
            self.status = PlayState::Error;
            return self.status;
        }

        if !self.ended.load(Ordering::Relaxed) {
            return self.status;
        }

        // stdout reached end of stream, but that alone is not proof of success:
        // ffmpeg still exits non-zero for media it cannot decode, even when it
        // has already emitted whole frames. Only the exit status separates a
        // clean EOF from such a failure.
        let exit = self
            .child
            .as_mut()
            .and_then(|child| child.try_wait().ok().flatten());
        let Some(exit) = exit else {
            // The child is still finalizing; settle on the next poll.
            return self.status;
        };

        // `try_wait` already reaped the child, so only its reader threads are
        // left to join; signalling a reaped pid could reach an unrelated
        // process.
        self.child = None;
        self.join_decode_threads();
        self.timing.pause(self.duration);
        self.status = if exit.success() {
            // A clean EOF: the session is finished but its file is kept, so a
            // later `play` restarts it from the beginning.
            PlayState::Stopped
        } else {
            warn!(
                code = exit.code(),
                "video decode exited with a failure status"
            );
            PlayState::Error
        };
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
                "stream_side_data=rotation",
                "-show_entries",
                "stream_tags=rotate",
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
            0,
        ));

        assert_eq!(
            args,
            vec![
                "-hide_banner",
                "-loglevel",
                "error",
                // The child shares the TUI's stdin, so it must never read it.
                "-nostdin",
                // Rotation is applied by the explicit filter, never implicitly.
                "-noautorotate",
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
    fn decode_args_rotate_before_scaling_a_rotated_source() {
        // A 90-degree source is decoded upright before the scale, and the
        // scale targets the displayed (portrait) geometry.
        let args = args_of(&decode_args(
            Path::new("/tmp/clip.bin"),
            0.0,
            240,
            320,
            25.0,
            90,
        ));

        let filter = args
            .windows(2)
            .find_map(|pair| (pair[0] == "-vf").then(|| pair[1].clone()))
            .expect("the decode carries a filter");
        assert_eq!(filter, "transpose=2,scale=240:320");
        assert!(args.contains(&"-noautorotate".to_string()));
    }

    #[test]
    fn decode_args_omit_a_seek_at_the_start() {
        let args = args_of(&decode_args(
            Path::new("/tmp/clip.bin"),
            0.0,
            64,
            48,
            12.0,
            0,
        ));

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
        // A one-pixel source must not even-round down to a zero-width `scale`.
        assert_eq!(scaled_dimensions(1, 100, 1280), (2, 100));
        assert_eq!(scaled_dimensions(100, 1, 1280), (100, 2));
    }

    #[test]
    fn orientation_filter_maps_each_cardinal_rotation_like_autorotate() {
        // The exact filters ffmpeg's own `-autorotate` would have inserted, so
        // disabling it and applying these yields identical pixels.
        assert_eq!(orientation_filter(0), None);
        assert_eq!(orientation_filter(90), Some("transpose=2"));
        assert_eq!(orientation_filter(180), Some("hflip,vflip"));
        assert_eq!(orientation_filter(270), Some("transpose=1"));
        // Negative and out-of-range values normalise to a quarter turn.
        assert_eq!(orientation_filter(-90), Some("transpose=1"));
        assert_eq!(orientation_filter(-180), Some("hflip,vflip"));
        assert_eq!(orientation_filter(450), Some("transpose=2"));
        // A non-cardinal angle has no transposition to apply.
        assert_eq!(orientation_filter(45), None);
    }

    #[test]
    fn display_dimensions_swap_only_for_a_quarter_turn() {
        assert_eq!(display_dimensions(320, 240, 0), (320, 240));
        assert_eq!(display_dimensions(320, 240, 90), (240, 320));
        assert_eq!(display_dimensions(320, 240, 180), (320, 240));
        assert_eq!(display_dimensions(320, 240, 270), (240, 320));
        assert_eq!(display_dimensions(320, 240, -90), (240, 320));
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

        let (width, height, fps, duration, rotation) = parse_probe_json(json).unwrap();
        assert_eq!((width, height), (320, 240));
        assert_eq!(fps, 25.0, "a rate below the cap passes through untouched");
        assert_eq!(duration, 2.0);
        assert_eq!(rotation, 0, "an unrotated source has no display rotation");

        // A rate above the cap is decoded at the cap instead.
        let json = r#"{
  "streams": [
    {
      "width": 320,
      "height": 240,
      "avg_frame_rate": "120/1",
      "duration": "2.000000"
    }
  ],
  "format": { "duration": "2.000000" }
}"#;
        let (_, _, fps, _, _) = parse_probe_json(json).unwrap();
        assert_eq!(fps, MAX_FPS, "decode is capped at MAX_FPS");
    }

    #[test]
    fn probe_json_reads_the_display_matrix_rotation() {
        // The shape `ffprobe -show_entries stream_side_data=rotation` produces
        // for a portrait clip encoded as landscape plus a display matrix.
        let json = r#"{
  "streams": [
    {
      "width": 320,
      "height": 240,
      "avg_frame_rate": "25/1",
      "side_data_list": [ { "rotation": 90 } ]
    }
  ]
}"#;
        let (width, height, _, _, rotation) = parse_probe_json(json).unwrap();
        assert_eq!((width, height), (320, 240), "the coded frame is landscape");
        assert_eq!(rotation, 90);
        assert_eq!(display_dimensions(width, height, rotation), (240, 320));

        // ffprobe reports the opposite quarter turns as negative values.
        for (raw, expected) in [("-90", 270), ("-180", 180), ("270", 270)] {
            let json = format!(
                r#"{{ "streams": [ {{ "width": 320, "height": 240, "side_data_list": [ {{ "rotation": {raw} }} ] }} ] }}"#
            );
            let (_, _, _, _, rotation) = parse_probe_json(&json).unwrap();
            assert_eq!(rotation, expected, "rotation tag {raw}");
        }
    }

    #[test]
    fn probe_json_reads_the_legacy_rotate_tag() {
        // Containers without a display matrix fall back to the `rotate` tag.
        let json = r#"{
  "streams": [
    { "width": 640, "height": 480, "tags": { "rotate": "90" } }
  ]
}"#;
        let (_, _, _, _, rotation) = parse_probe_json(json).unwrap();
        assert_eq!(rotation, 90);
    }

    #[test]
    fn probe_json_falls_back_for_rate_and_duration() {
        // A container with no stream duration and no declared frame rate: the
        // rate falls back to the cap and the duration to "unknown" (0.0).
        let json = r#"{
  "streams": [ { "width": 640, "height": 480, "avg_frame_rate": "0/0" } ],
  "format": { "duration": "0.000000" }
}"#;
        let (width, height, fps, duration, _) = parse_probe_json(json).unwrap();
        assert_eq!((width, height), (640, 480));
        assert_eq!(fps, MAX_FPS);
        assert_eq!(duration, 0.0);

        // The format duration is used when the stream has none.
        let json = r#"{
  "streams": [ { "width": 640, "height": 480, "avg_frame_rate": "30/1" } ],
  "format": { "duration": "7.500000" }
}"#;
        let (_, _, fps, duration, _) = parse_probe_json(json).unwrap();
        assert_eq!(fps, 30.0, "30 fps is below the cap, so it passes through");
        assert_eq!(duration, 7.5);
    }

    #[test]
    fn probe_json_rejects_a_media_file_with_no_video_stream() {
        assert!(parse_probe_json(r#"{"streams": [], "format": {}}"#).is_err());
        assert!(parse_probe_json(r#"{"streams": [{"width": 0, "height": 0}]}"#).is_err());
        assert!(parse_probe_json("not json").is_err());
    }

    /// Teardown must not depend on winning the slot race: a renderer holding
    /// it at exactly the wrong moment used to turn the cleanup `try_lock` into
    /// a silently skipped clear, leaving the stopped session's last picture in
    /// the shared slot for the next popup to inherit.
    #[test]
    fn stop_waits_out_a_held_frame_slot_instead_of_keeping_the_frame() {
        let slot = Arc::new(Mutex::new(None));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut engine = FfmpegEngine::new(Arc::clone(&slot), tx, Arc::new(AtomicBool::new(false)));

        // Leave a picture behind, as a running decode would.
        *slot.try_lock().unwrap() = Some(DynamicImage::ImageRgb8(RgbImage::new(2, 2)));

        // The renderer holds the slot across the shutdown.
        let held = slot.try_lock().unwrap();

        let (announced_tx, announced_rx) = std::sync::mpsc::channel();
        let stopping = std::thread::spawn(move || {
            let _ = announced_tx.send(());
            engine.stop();
            engine
        });
        announced_rx.recv().unwrap();

        // Give `stop` time to reach the clear while the slot is still held:
        // it has to wait for the slot, not give up on it.
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !stopping.is_finished(),
            "stop gave up on clearing the frame while the slot was held"
        );

        drop(held);
        let mut engine = stopping.join().unwrap();

        assert!(
            slot.try_lock().is_ok_and(|guard| guard.is_none()),
            "the stopped session's frame survived the shutdown"
        );
        assert_eq!(engine.status(), PlayState::Stopped);
    }

    /// The acceptance check for redraw coalescing: a renderer that never gets
    /// around to acknowledging a wake must not accumulate one pending wake per
    /// decoded frame — the channel stays at a single wake no matter how much
    /// video the decoder produces.
    #[test]
    fn a_slow_renderer_does_not_pile_up_redraw_wakes() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg is not installed");
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
                "testsrc=size=320x240:rate=25:duration=2",
                "-pix_fmt",
                "yuv420p",
                "-y",
                "-f",
                "mp4",
            ])
            .arg(&clip)
            .status()
            .expect("ffmpeg generates the test clip");
        assert!(status.success());

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let pending = Arc::new(AtomicBool::new(false));
        let slot = Arc::new(Mutex::new(None));
        let mut engine = FfmpegEngine::new(Arc::clone(&slot), tx, Arc::clone(&pending));

        let bytes = std::fs::read(&clip).unwrap();
        engine.load(&bytes).unwrap();
        engine.play();

        // No event loop is around to re-arm the latch: frames keep replacing
        // each other in the slot, and only the very first wake is accepted.
        wait_until(Duration::from_secs(10), || engine.position() > 0.2);
        assert_eq!(
            rx.len(),
            1,
            "a slow renderer must see one pending wake, not one per decoded frame"
        );
        assert!(
            pending.load(Ordering::SeqCst),
            "the next wake stays latched"
        );

        engine.stop();
    }

    /// The acceptance check for display orientation: a portrait clip stored as
    /// landscape plus a 90-degree display matrix must decode to a portrait
    /// frame whose content is actually rotated — not merely reshaped.
    #[test]
    fn a_rotated_portrait_clip_decodes_upright() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg is not installed");
            return;
        }

        // A landscape black clip with a white square in its top-left corner:
        // after a correct 90-degree rotation the square belongs in the
        // bottom-left corner of the portrait frame.
        let base = temp_file().expect("a scratch holder for the base clip");
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "color=black:size=320x240:rate=25:duration=1",
                "-vf",
                "drawbox=x=0:y=0:w=64:h=64:color=white:t=fill",
                "-pix_fmt",
                "yuv420p",
                "-y",
                "-f",
                "mp4",
            ])
            .arg(base.path())
            .status()
            .expect("ffmpeg generates the base clip");
        assert!(status.success(), "ffmpeg failed to build the base clip");

        // Copy the stream while attaching the display-matrix rotation.
        let rotated = temp_file().expect("a scratch holder for the rotated clip");
        let status = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
            .args(["-display_rotation", "90"])
            .arg("-i")
            .arg(base.path())
            .args(["-c", "copy", "-f", "mp4"])
            .arg(rotated.path())
            .status()
            .expect("ffmpeg attaches the display rotation");
        if !status.success() {
            eprintln!("skipping: this ffmpeg cannot attach a display rotation");
            return;
        }

        let slot = Arc::new(Mutex::new(None));
        let mut engine = FfmpegEngine::new(
            Arc::clone(&slot),
            tokio::sync::mpsc::unbounded_channel().0,
            Arc::new(AtomicBool::new(false)),
        );

        let bytes = std::fs::read(rotated.path()).unwrap();
        engine.load(&bytes).unwrap();

        wait_until(Duration::from_secs(10), || {
            slot.try_lock().is_ok_and(|guard| guard.is_some())
        });
        let frame = slot.try_lock().unwrap().take().unwrap();
        let rgb = frame.to_rgb8();

        assert_eq!(
            rgb.dimensions(),
            (240, 320),
            "a rotated portrait clip must decode portrait, not landscape"
        );

        let white = |x: u32, y: u32| rgb.get_pixel(x, y).0[0] > 128;
        assert!(
            white(8, 320 - 9),
            "the top-left corner square must rotate to the bottom-left"
        );
        assert!(
            !white(240 - 9, 8),
            "the opposite corner must stay black after the rotation"
        );

        engine.stop();
    }

    #[test]
    fn undecodable_bytes_report_an_error_status() {
        let mut engine = FfmpegEngine::new(
            Arc::new(Mutex::new(None)),
            tokio::sync::mpsc::unbounded_channel().0,
            Arc::new(AtomicBool::new(false)),
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

    /// The dangerous failure: media whose container probes cleanly but whose
    /// pixels cannot be decoded. ffmpeg exits non-zero without writing a frame,
    /// and the engine must not mistake the closed pipe for a finished video.
    #[test]
    fn a_video_that_probes_cleanly_but_cannot_decode_is_an_error() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg is not installed");
            return;
        }

        let holder = temp_file().expect("a scratch image holder");
        let image = holder.path().to_path_buf();
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x48:rate=25:duration=1",
                "-frames:v",
                "1",
                // The scratch file has no meaningful extension, so the muxer
                // has to be named.
                "-f",
                "image2",
                "-c:v",
                "png",
                "-y",
            ])
            .arg(&image)
            .status()
            .expect("ffmpeg generates the test image");
        assert!(status.success(), "ffmpeg failed to build the test image");

        let mut bytes = std::fs::read(&image).unwrap();

        // Scramble only an `IDAT` payload, leaving every chunk length and the
        // `IHDR` intact, so the header still probes even though decoding fails.
        let mut offset = 8;
        while offset + 8 <= bytes.len() {
            let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            if &bytes[offset + 4..offset + 8] == b"IDAT" {
                let end = (offset + 8 + length).min(bytes.len());
                for byte in &mut bytes[offset + 8..end] {
                    *byte ^= 0x55;
                }
                break;
            }
            offset += 12 + length;
        }

        let mut engine = FfmpegEngine::new(
            Arc::new(Mutex::new(None)),
            tokio::sync::mpsc::unbounded_channel().0,
            Arc::new(AtomicBool::new(false)),
        );

        // The probe accepts the container, so the load itself must succeed: the
        // failure only appears once ffmpeg tries to decode.
        engine.load(&bytes).expect("the header is still readable");
        assert_eq!(engine.status(), PlayState::Paused);

        engine.play();
        assert_eq!(engine.status(), PlayState::Playing);

        wait_until(Duration::from_secs(10), || {
            matches!(engine.status(), PlayState::Error | PlayState::Stopped)
        });
        assert_eq!(
            engine.status(),
            PlayState::Error,
            "undecodable media must not look like a clean EOF"
        );

        engine.stop();
    }

    /// ffmpeg must never write its diagnostics straight to the terminal: they
    /// are piped and drained into the application log. The engine's stderr is
    /// only ever taken into a drain thread when it was piped rather than
    /// inherited or nulled, so the live thread is the observable proof.
    #[test]
    fn decoding_drains_ffmpeg_diagnostics_instead_of_the_terminal() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg is not installed");
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
                "testsrc=size=64x64:rate=25:duration=1",
                "-pix_fmt",
                "yuv420p",
                "-y",
                "-f",
                "mp4",
            ])
            .arg(&clip)
            .status()
            .expect("ffmpeg generates the test clip");
        assert!(status.success());

        let bytes = std::fs::read(&clip).unwrap();
        let mut engine = FfmpegEngine::new(
            Arc::new(Mutex::new(None)),
            tokio::sync::mpsc::unbounded_channel().0,
            Arc::new(AtomicBool::new(false)),
        );
        engine.load(&bytes).unwrap();
        engine.play();

        assert!(
            engine.stderr_reader.is_some(),
            "ffmpeg stderr must be drained by a thread, not inherited"
        );
        assert!(
            engine
                .child
                .as_ref()
                .is_some_and(|child| child.stderr.is_none()),
            "the drain thread must own the child's stderr pipe"
        );

        engine.stop();
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
        let mut engine = FfmpegEngine::new(Arc::clone(&slot), tx, Arc::new(AtomicBool::new(false)));

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

        // A frame must be in the slot for the pause checks to mean anything.
        wait_until(Duration::from_secs(5), || {
            slot.try_lock().is_ok_and(|guard| guard.is_some())
        });

        engine.pause();
        let frozen = engine.position();
        assert_eq!(engine.status(), PlayState::Paused);
        assert!(
            engine.child.is_none(),
            "pause must reap the decoder instead of leaving it running"
        );
        assert!(engine.reader.is_none(), "pause must join the frame reader");
        assert!(
            engine.stderr_reader.is_none(),
            "pause must join the diagnostics drainer"
        );
        assert!(
            slot.try_lock().is_ok_and(|guard| guard.is_some()),
            "pausing must keep the displayed frame available"
        );
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
        assert!(
            engine.child.is_none(),
            "stop must leave no decoder child behind"
        );
        assert!(engine.reader.is_none(), "stop must join the frame reader");
        assert!(
            engine.stderr_reader.is_none(),
            "stop must join the diagnostics drainer"
        );
        assert!(!scratch.exists(), "stopping must delete the scratch file");
        assert!(
            slot.try_lock().is_ok_and(|guard| guard.is_none()),
            "stop must clear the shared frame slot"
        );
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
            Arc::new(AtomicBool::new(false)),
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
    fn dropping_a_playing_engine_kills_and_reaps_its_decoder() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg is not installed");
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
                "testsrc=size=64x64:rate=25:duration=2",
                "-pix_fmt",
                "yuv420p",
                "-y",
                "-f",
                "mp4",
            ])
            .arg(&clip)
            .status()
            .expect("ffmpeg generates the test clip");
        assert!(status.success());

        let bytes = std::fs::read(&clip).unwrap();
        let mut engine = FfmpegEngine::new(
            Arc::new(Mutex::new(None)),
            tokio::sync::mpsc::unbounded_channel().0,
            Arc::new(AtomicBool::new(false)),
        );
        engine.load(&bytes).unwrap();
        engine.play();
        wait_until(Duration::from_secs(10), || engine.position() > 0.1);

        assert!(
            engine.child.is_some(),
            "the test needs a live decoder to drop"
        );
        #[cfg(target_os = "linux")]
        let pid = engine.child.as_ref().unwrap().id();
        let scratch = engine.temp.as_ref().unwrap().path().to_path_buf();

        drop(engine);

        assert!(!scratch.exists(), "drop must unlink the scratch copy");
        #[cfg(target_os = "linux")]
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "drop must kill and reap the decoder child"
        );
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
            Arc::new(AtomicBool::new(false)),
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
