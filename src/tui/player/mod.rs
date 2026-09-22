use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;
use tracing::error;

mod engine;

pub use engine::RodioEngine;

use super::UiEvent;
use crate::backend::{ChatId, MessageId};

/// Lifecycle of the audio session backing the popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayState {
    /// No media loaded / nothing is being played.
    Stopped,
    /// A message's bytes are being handed to the engine.
    Loading,
    Playing,
    Paused,
    /// The engine failed to load or decode the media; playback is unusable.
    Error,
}

/// Identifies which message an audio session belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayKey {
    pub chat: ChatId,
    pub message_id: MessageId,
}

/// A snapshot of the audio session reported by the player worker to the UI.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaybackState {
    pub source: PlayKey,
    pub status: PlayState,
    /// Seconds of audio played, as sampled by the engine.
    pub position: f64,
    /// Total duration in seconds; `0.0` when unknown.
    pub duration: f64,
    /// When `position` was sampled; used to interpolate while playing.
    pub updated_at: Instant,
}

impl PlaybackState {
    /// The position to display for `now`. While playing, the sampled position
    /// is extrapolated with the wall clock (clamped to the duration when it is
    /// known) so the progress bar advances between worker reports; otherwise
    /// the position is frozen.
    pub fn display_position(&self, now: Instant) -> f64 {
        if self.status == PlayState::Playing {
            let elapsed = now.saturating_duration_since(self.updated_at).as_secs_f64();

            let position = self.position + elapsed;

            if self.duration > 0.0 {
                position.min(self.duration)
            } else {
                position
            }
        } else {
            self.position
        }
    }
}

/// A pluggable audio engine, owned and driven by the player worker thread.
/// The concrete [`RodioEngine`] plays decoded bytes; a `StubEngine` drives the
/// UI-level tests.
pub trait MediaEngine: Send {
    /// Load `bytes` for playback. Returns the duration in seconds (`0.0` when
    /// unknown) or an error description; on error the engine must leave its
    /// status as [`PlayState::Error`].
    fn load(&mut self, bytes: &[u8]) -> Result<f64, String>;
    fn play(&mut self);
    fn pause(&mut self);
    /// Seek to an absolute position in seconds; callers clamp beforehand.
    fn seek(&mut self, position_secs: f64);
    fn stop(&mut self);
    fn position(&self) -> f64;
    fn duration(&self) -> f64;
    /// Poll the engine. `&mut self` so engines can transition on EOF while
    /// sampling (e.g. a sink that has emptied out).
    fn status(&mut self) -> PlayState;
}

/// Commands the UI sends to the player worker.
enum PlayerCommand {
    Load { source: PlayKey, bytes: Vec<u8> },
    Play,
    Pause,
    Seek { delta_secs: f64 },
    Stop,
}

/// How often the worker re-reports a playing session; reports are what make
/// the progress bar advance and wake the UI event loop.
const REPORT_INTERVAL: Duration = Duration::from_millis(500);

/// Handle the UI hands to the player worker thread. All methods just enqueue a
/// command; nothing here touches the audio side or blocks.
pub struct Player {
    tx: mpsc::Sender<PlayerCommand>,
}

impl Player {
    /// Spawn the worker thread that owns `engine` and drives it with commands
    /// from this handle, reporting snapshots over `ui_tx`.
    pub fn new(engine: Box<dyn MediaEngine>, ui_tx: UnboundedSender<UiEvent>) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(|| run_worker(rx, engine, ui_tx));
        Self { tx }
    }

    pub fn load(&self, source: PlayKey, bytes: Vec<u8>) {
        let _ = self.tx.send(PlayerCommand::Load { source, bytes });
    }

    pub fn play(&self) {
        let _ = self.tx.send(PlayerCommand::Play);
    }

    pub fn pause(&self) {
        let _ = self.tx.send(PlayerCommand::Pause);
    }

    /// Seek by a relative delta (positive = forward). The worker clamps it to
    /// the loaded duration.
    pub fn seek_by(&self, delta_secs: f64) {
        let _ = self.tx.send(PlayerCommand::Seek { delta_secs });
    }

    pub fn stop(&self) {
        let _ = self.tx.send(PlayerCommand::Stop);
    }
}

fn run_worker(
    rx: Receiver<PlayerCommand>,
    engine: Box<dyn MediaEngine>,
    ui_tx: UnboundedSender<UiEvent>,
) {
    let mut worker = Worker {
        engine,
        ui_tx,
        source: None,
        prev_status: PlayState::Stopped,
    };

    loop {
        match rx.recv_timeout(REPORT_INTERVAL) {
            Ok(command) => worker.handle(command),
            Err(RecvTimeoutError::Timeout) => worker.tick(),
            // The `Player` handle is gone; stop the engine and exit.
            Err(RecvTimeoutError::Disconnected) => {
                worker.engine.stop();
                break;
            }
        }
    }
}

struct Worker {
    engine: Box<dyn MediaEngine>,
    ui_tx: UnboundedSender<UiEvent>,
    source: Option<PlayKey>,
    prev_status: PlayState,
}

impl Worker {
    fn handle(&mut self, command: PlayerCommand) {
        match command {
            PlayerCommand::Load { source, bytes } => {
                self.source = Some(source);
                self.report(PlayState::Loading);

                if let Err(err) = self.engine.load(&bytes) {
                    error!(error = %err, "Failed to load audio");
                }
            }
            PlayerCommand::Play => self.engine.play(),
            PlayerCommand::Pause => self.engine.pause(),
            PlayerCommand::Seek { delta_secs } => {
                let position = self.engine.position() + delta_secs;
                let duration = self.engine.duration();
                let target = if duration > 0.0 {
                    position.clamp(0.0, duration)
                } else {
                    position.max(0.0)
                };

                self.engine.seek(target);
            }
            PlayerCommand::Stop => self.engine.stop(),
        }

        let status = self.engine.status();
        self.report(status);
    }

    /// Periodic refresh path: re-report while playing so the progress bar
    /// advances, and catch engine-side transitions (e.g. the stream reaching
    /// EOF).
    fn tick(&mut self) {
        let status = self.engine.status();
        if status == PlayState::Playing || status != self.prev_status {
            self.report(status);
        }
    }

    fn report(&mut self, status: PlayState) {
        let Some(source) = self.source.clone() else {
            return;
        };

        self.prev_status = status;

        let state = PlaybackState {
            source,
            status,
            position: self.engine.position(),
            duration: self.engine.duration(),
            updated_at: Instant::now(),
        };

        let _ = self.ui_tx.send(UiEvent::PlaybackState(state));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MessageId;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    struct StubInner {
        calls: Vec<&'static str>,
        position: f64,
        duration: f64,
        status: PlayState,
        load_result: Option<Result<f64, String>>,
    }

    struct StubEngine(Arc<Mutex<StubInner>>);

    impl StubEngine {
        fn new(duration: f64) -> (Self, Arc<Mutex<StubInner>>) {
            let inner = Arc::new(Mutex::new(StubInner {
                calls: Vec::new(),
                position: 0.0,
                duration,
                status: PlayState::Stopped,
                load_result: None,
            }));
            (Self(inner.clone()), inner)
        }
    }

    impl MediaEngine for StubEngine {
        fn load(&mut self, _bytes: &[u8]) -> Result<f64, String> {
            let mut inner = self.0.lock().unwrap();
            inner.calls.push("load");
            match inner.load_result.clone() {
                Some(Ok(duration)) => {
                    inner.duration = duration;
                    inner.position = 0.0;
                    inner.status = PlayState::Paused;
                    Ok(duration)
                }
                Some(Err(err)) => {
                    inner.status = PlayState::Error;
                    Err(err)
                }
                None => {
                    inner.position = 0.0;
                    inner.status = PlayState::Paused;
                    Ok(inner.duration)
                }
            }
        }

        fn play(&mut self) {
            let mut inner = self.0.lock().unwrap();
            inner.calls.push("play");
            inner.status = PlayState::Playing;
        }

        fn pause(&mut self) {
            let mut inner = self.0.lock().unwrap();
            inner.calls.push("pause");
            inner.status = PlayState::Paused;
        }

        fn seek(&mut self, position_secs: f64) {
            let mut inner = self.0.lock().unwrap();
            inner.calls.push("seek");
            inner.position = position_secs;
        }

        fn stop(&mut self) {
            let mut inner = self.0.lock().unwrap();
            inner.calls.push("stop");
            inner.position = 0.0;
            inner.status = PlayState::Stopped;
        }

        fn position(&self) -> f64 {
            self.0.lock().unwrap().position
        }

        fn duration(&self) -> f64 {
            self.0.lock().unwrap().duration
        }

        fn status(&mut self) -> PlayState {
            self.0.lock().unwrap().status
        }
    }

    fn key(chat: i64, id: &str) -> PlayKey {
        PlayKey {
            chat: ChatId::Telegram(chat),
            message_id: MessageId(id.to_string()),
        }
    }

    fn assert_playback(event: UiEvent, expected: PlayState) -> PlaybackState {
        match event {
            UiEvent::PlaybackState(state) => {
                assert_eq!(state.status, expected);
                state
            }
            _other => panic!("expected PlaybackState event, got a different UiEvent"),
        }
    }

    #[test]
    fn display_position_interpolates_while_playing_only() {
        let anchor = Instant::now();
        let source = key(1, "42");
        let playing = PlaybackState {
            source: source.clone(),
            status: PlayState::Playing,
            position: 3.0,
            duration: 10.0,
            updated_at: anchor,
        };
        assert_eq!(
            playing.display_position(anchor + Duration::from_secs(2)),
            5.0
        );
        assert_eq!(
            playing.display_position(anchor + Duration::from_secs(100)),
            10.0
        );

        let paused = PlaybackState {
            source,
            status: PlayState::Paused,
            position: 3.0,
            duration: 10.0,
            updated_at: anchor,
        };
        assert_eq!(
            paused.display_position(anchor + Duration::from_secs(2)),
            3.0
        );

        let unknown = PlaybackState {
            source: key(1, "43"),
            status: PlayState::Playing,
            position: 1.0,
            duration: 0.0,
            updated_at: anchor,
        };
        assert_eq!(
            unknown.display_position(anchor + Duration::from_secs(1)),
            2.0
        );
    }

    #[test]
    fn player_forwards_commands_and_reports_state() {
        let (engine, inner) = StubEngine::new(8.0);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let player = Player::new(Box::new(engine), tx);
        let src = key(1, "42");

        player.load(src.clone(), b"fake-ogg".to_vec());
        assert_eq!(
            assert_playback(rx.blocking_recv().unwrap(), PlayState::Loading).source,
            src
        );
        let loaded = assert_playback(rx.blocking_recv().unwrap(), PlayState::Paused);
        assert_eq!(loaded.position, 0.0);
        assert_eq!(loaded.duration, 8.0);

        player.play();
        assert_eq!(
            assert_playback(rx.blocking_recv().unwrap(), PlayState::Playing).status,
            PlayState::Playing
        );

        player.pause();
        assert_playback(rx.blocking_recv().unwrap(), PlayState::Paused);

        player.stop();
        assert_playback(rx.blocking_recv().unwrap(), PlayState::Stopped);

        assert_eq!(
            inner.lock().unwrap().calls,
            ["load", "play", "pause", "stop"]
        );
    }

    #[test]
    fn seek_is_clamped_to_duration_bounds() {
        let (engine, inner) = StubEngine::new(10.0);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let player = Player::new(Box::new(engine), tx);

        player.load(key(1, "42"), b"fake-ogg".to_vec());
        rx.blocking_recv().unwrap();
        rx.blocking_recv().unwrap();

        player.seek_by(4.0);
        assert_eq!(
            assert_playback(rx.blocking_recv().unwrap(), PlayState::Paused).position,
            4.0
        );
        assert_eq!(inner.lock().unwrap().position, 4.0);

        player.seek_by(100.0);
        assert_eq!(
            assert_playback(rx.blocking_recv().unwrap(), PlayState::Paused).position,
            10.0
        );

        player.seek_by(-100.0);
        assert_eq!(
            assert_playback(rx.blocking_recv().unwrap(), PlayState::Paused).position,
            0.0
        );

        assert_eq!(
            inner
                .lock()
                .unwrap()
                .calls
                .iter()
                .filter(|c| **c == "seek")
                .count(),
            3
        );
    }

    #[test]
    fn failed_load_reports_error_state() {
        let (engine, inner) = StubEngine::new(0.0);
        inner.lock().unwrap().load_result = Some(Err("boom".to_string()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let player = Player::new(Box::new(engine), tx);

        player.load(key(1, "42"), b"not-audio".to_vec());
        assert_playback(rx.blocking_recv().unwrap(), PlayState::Loading);
        assert_playback(rx.blocking_recv().unwrap(), PlayState::Error);
    }
}
