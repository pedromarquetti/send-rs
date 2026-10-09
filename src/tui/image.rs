use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, SyncSender};

use image::DynamicImage;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect, Size};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, StatefulWidget, Widget};
use ratatui_image::ResizeEncodeRender;
use ratatui_image::errors::Errors;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::thread::{ResizeRequest, ResizeResponse, ThreadProtocol};
use ratatui_image::{Resize, StatefulImage};
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;
use tracing::debug;

use super::UiEvent;
use crate::helpers::wrap_text;

/// Marker widget; all state lives in [`ImageWidgetState`].
pub struct ImageWidget;

/// Per-popup state for rendering an image with `ratatui-image`.
///
/// A worker thread owns the (re-)encode loop and reports finished frames back
/// over a sync `mpsc` channel; this mirrors `ratatui-image/examples/thread.rs`.
/// Drop the state to stop the worker (the channel sender closes and the thread
/// exits).
pub struct ImageWidgetState {
    protocol: ThreadProtocol,
    results: Receiver<Result<ResizeResponse, Errors>>,
    has_image: bool,
    status: String,
}

// TODO: implement image caching for faster loading: check if possible
impl ImageWidgetState {
    /// Spawn the encode worker; `wake` is signalled with `UiEvent::Redraw`
    /// whenever a (re-)encoded frame is ready so the UI repaints promptly.
    pub(super) fn new(wake: UnboundedSender<UiEvent>) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<ResizeRequest>();
        let (result_tx, results) = mpsc::channel();

        std::thread::spawn(move || {
            while let Ok(request) = request_rx.recv() {
                let _ = result_tx.send(request.resize_encode());
                let _ = wake.send(UiEvent::Redraw);
            }
        });

        Self {
            protocol: ThreadProtocol::new(request_tx, None),
            results,
            has_image: false,
            status: "Loading image…".to_string(),
        }
    }

    /// Replace the currently displayed image and re-encode it for the next
    /// terminal resize.
    pub(super) fn set_image(&mut self, picker: &Picker, image: DynamicImage) {
        self.protocol
            .replace_protocol(picker.new_resize_protocol(image));
        self.has_image = true;
        self.status.clear();
    }

    /// Show `message` in place of the image (decode/fetch failure, etc.).
    pub(super) fn set_error(&mut self, message: String) {
        self.protocol.empty_protocol();
        self.has_image = false;
        self.status = message;
    }
}

impl StatefulWidget for ImageWidget {
    type State = ImageWidgetState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        for completed in state.results.try_iter().flatten() {
            state.protocol.update_resized_protocol(completed);
        }

        if !state.has_image {
            render_placeholder(area, buf, &state.status);
            return;
        }

        let Some(size) = state.protocol.size_for(Resize::Fit(None), area.into()) else {
            // First frame not encoded yet; keep the previous frame (or blank).
            return;
        };
        let image_area = Rect {
            x: area.x + (area.width.saturating_sub(size.width)) / 2,
            y: area.y + (area.height.saturating_sub(size.height)) / 2,
            width: size.width.min(area.width),
            height: size.height.min(area.height),
        };

        StatefulImage::new().render(image_area, buf, &mut state.protocol);
    }
}

/// Marker widget; all state lives in [`VideoWidgetState`].
pub struct VideoWidget;

/// Per-popup state for rendering a decoded video frame with `ratatui-image`.
///
/// Unlike the image widget, a video is a stream: the engine publishes the
/// newest decoded frame into a shared slot and this widget feeds those frames
/// through its own encode worker, at most one encode in flight. The widget
/// renders the last frame that came *back* from the worker, so the previously
/// displayed frame keeps drawing for the whole encode window instead of the
/// viewport blanking once per frame. Drop the state to stop the worker (the
/// channel sender closes and the thread exits).
pub struct VideoWidgetState {
    /// The engine's "latest frame wins" slot.
    frames: Arc<Mutex<Option<DynamicImage>>>,
    picker: Picker,
    /// The last frame that finished encoding; this is what gets rendered.
    encoded: Option<StatefulProtocol>,
    /// Capacity 1, so a second job cannot be queued behind the first.
    jobs: SyncSender<(StatefulProtocol, Size)>,
    /// Encoded frames come back on their own channel. `None` means the encode
    /// failed: the widget still has to hear back, or the in-flight flag would
    /// latch and the video would freeze on its last frame for good.
    results: Receiver<Option<StatefulProtocol>>,
    /// Whether a job is outstanding. The channel cannot report this, and
    /// without it a frame would be taken out of the slot on every draw.
    in_flight: bool,
    status: String,
}

impl VideoWidgetState {
    /// Spawn the encode worker; `wake` is signalled with `UiEvent::Redraw`
    /// whenever a frame is ready so the UI repaints promptly.
    pub(super) fn new(
        frames: Arc<Mutex<Option<DynamicImage>>>,
        picker: Picker,
        wake: UnboundedSender<UiEvent>,
    ) -> Self {
        let (job_tx, jobs) = mpsc::sync_channel::<(StatefulProtocol, Size)>(1);
        let (result_tx, results) = mpsc::channel();

        std::thread::spawn(move || {
            while let Ok((mut protocol, size)) = jobs.recv() {
                protocol.resize_encode(&Resize::Fit(None), size);

                // Only a protocol that actually reported `Ok(())` holds
                // drawable pixels. A zero-width or zero-height target makes
                // `resize_encode` return without recording a result at all, so
                // treating that as success would blank the viewport and leave
                // the status text up forever; treat it as a miss and keep the
                // last good frame.
                let encoded = matches!(protocol.last_encoding_result(), Some(Ok(())));

                if !encoded {
                    debug!(?size, "video frame encode produced no pixels");
                }

                let _ = result_tx.send(encoded.then_some(protocol));
                let _ = wake.send(UiEvent::Redraw);
            }
        });

        Self {
            frames,
            picker,
            encoded: None,
            jobs: job_tx,
            results,
            in_flight: false,
            status: "Loading video…".to_string(),
        }
    }

    /// Show `message` in place of the video (no ffmpeg on the system, an
    /// undecodable container, a download that never arrived).
    pub(super) fn set_error(&mut self, message: String) {
        self.encoded = None;
        self.status = message;
    }
}

impl StatefulWidget for VideoWidget {
    type State = VideoWidgetState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        if let Ok(encoded) = state.results.try_recv() {
            // A failed encode leaves the last good frame on screen rather than
            // blanking the viewport; either way the worker is idle again.
            if let Some(encoded) = encoded {
                state.encoded = Some(encoded);
                state.status.clear();
            }
            state.in_flight = false;
        }

        // A frame is taken out of the slot only when the worker is idle, so
        // frames the renderer cannot keep up with stay in the slot (the newest
        // one wins there) instead of queueing up behind the encoder.
        if !state.in_flight {
            let fresh = state
                .frames
                .try_lock()
                .ok()
                .and_then(|mut slot| slot.take())
                .map(|frame| state.picker.new_resize_protocol(frame));

            // No new frame but a moved area (a terminal resize) still needs the
            // displayed frame re-encoded for its new size.
            let needs_reencode = state.encoded.as_mut().is_some_and(|protocol| {
                protocol
                    .needs_resize(&Resize::Fit(None), area.into())
                    .is_some()
            });

            let job = if let Some(protocol) = fresh {
                let size = protocol.size_for(Resize::Fit(None), area.into());
                Some((protocol, size))
            } else if needs_reencode {
                state.encoded.take().map(|protocol| {
                    let size = protocol.size_for(Resize::Fit(None), area.into());
                    (protocol, size)
                })
            } else {
                None
            };

            if let Some(job) = job
                && state.jobs.try_send(job).is_ok()
            {
                state.in_flight = true;
            }
        }

        // While an encode is outstanding the previous frame is still here, so
        // playback never blanks; only before the first one does the
        // placeholder show.
        match &mut state.encoded {
            Some(protocol) => StatefulImage::new().render(area, buf, protocol),
            None => render_placeholder(area, buf, &state.status),
        }
    }
}

/// Draw `status` centred in `area`, dimmed, with no picture behind it.
///
/// Shared by the image and video widgets so the "Loading…" / error text path is
/// written once and both popups fail the same way.
fn render_placeholder(area: Rect, buf: &mut Buffer, status: &str) {
    let text_width = area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = wrap_text(status, text_width)
        .into_iter()
        .map(Line::from)
        .collect();
    let top_pad = (area.height as usize).saturating_sub(lines.len()) / 2;
    let mut padded = vec![Line::from(""); top_pad];

    padded.append(&mut lines);

    while padded.len() < area.height as usize {
        padded.push(Line::from(""));
    }

    Paragraph::new(padded)
        .alignment(Alignment::Center)
        .style(Style::default().fg(Color::DarkGray))
        .render(area, buf);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn state() -> ImageWidgetState {
        let (wake, _rx) = tokio::sync::mpsc::unbounded_channel();
        ImageWidgetState::new(wake)
    }

    #[test]
    fn placeholder_renders_status_text_centered() {
        let mut state = state();
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 5));
        ImageWidget.render(buf.area, &mut buf, &mut state);
        // 1 line in a 5-row area starting at row 2, centered horizontally.
        assert_eq!(buf.cell((3, 2)).unwrap().symbol(), "L");
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), " ");
    }

    #[test]
    fn error_restores_placeholder() {
        let mut state = state();
        let picker = Picker::halfblocks();
        let image = DynamicImage::new_rgb8(2, 2);
        state.set_image(&picker, image);
        state.set_error("Media not available".to_string());
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 5));
        ImageWidget.render(buf.area, &mut buf, &mut state);
        assert_eq!(buf.cell((6, 1)).unwrap().symbol(), "M");
    }

    #[test]
    fn image_renders_and_encodes_on_worker_thread() {
        let mut state = state();
        let picker = Picker::halfblocks();
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(2, 2, |x, y| {
            let on = (x + y) % 2 == 0;
            image::Rgb([
                if on { 220 } else { 0 },
                if x == 0 { 120 } else { 40 },
                if y == 0 { 200 } else { 30 },
            ])
        }));
        state.set_image(&picker, image);
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 5));
        ImageWidget.render(buf.area, &mut buf, &mut state);

        let mut encoded = false;
        for _ in 0..100 {
            for completed in state.results.try_iter().flatten() {
                state.protocol.update_resized_protocol(completed);
                encoded = true;
            }

            if encoded {
                break;
            }

            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(encoded, "encode worker never produced a frame");

        ImageWidget.render(buf.area, &mut buf, &mut state);
        assert_ne!(buf, Buffer::empty(Rect::new(0, 0, 20, 5)));
    }

    #[test]
    fn encode_failure_keeps_state_usable() {
        let mut state = state();
        let (request_tx, _request_rx) = mpsc::channel::<ResizeRequest>();
        let (_result_tx, results) = mpsc::channel();
        let mut err_state = ImageWidgetState {
            results,
            has_image: false,
            status: "nope".to_string(),
            protocol: ThreadProtocol::new(request_tx, None),
        };
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 5));
        ImageWidget.render(buf.area, &mut buf, &mut err_state);
        assert_eq!(buf.cell((8, 2)).unwrap().symbol(), "n");
        state.set_error("down".to_string());
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 5));
        ImageWidget.render(buf.area, &mut buf, &mut state);
        assert_eq!(buf.cell((8, 2)).unwrap().symbol(), "d");
    }

    fn assert_send<T: Send>() {}

    #[test]
    fn the_encoded_protocol_can_cross_to_the_encode_worker() {
        // The whole flicker-free design depends on this: if the protocol were
        // not `Send` it could not be handed to the worker and returned.
        assert_send::<StatefulProtocol>();
    }

    fn video() -> (Arc<Mutex<Option<DynamicImage>>>, VideoWidgetState) {
        let frames = Arc::new(Mutex::new(None));
        let (wake, _rx) = tokio::sync::mpsc::unbounded_channel();
        let state = VideoWidgetState::new(Arc::clone(&frames), Picker::halfblocks(), wake);

        (frames, state)
    }

    fn video_frame() -> DynamicImage {
        DynamicImage::ImageRgb8(image::RgbImage::from_fn(4, 4, |x, y| {
            image::Rgb([(x * 60) as u8, (y * 60) as u8, 128])
        }))
    }

    #[test]
    fn a_frame_is_only_taken_from_the_slot_while_the_worker_is_idle() {
        let (frames, mut state) = video();
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 5));

        *frames.blocking_lock() = Some(video_frame());
        VideoWidget.render(buf.area, &mut buf, &mut state);
        assert!(
            state.in_flight,
            "the first frame should have been dispatched"
        );
        assert!(
            frames.try_lock().unwrap().is_none(),
            "the dispatched frame must leave the slot"
        );

        // A frame decoded while the worker is busy has to stay put: draining
        // it every draw would queue frames the renderer never reaches.
        *frames.blocking_lock() = Some(video_frame());
        VideoWidget.render(buf.area, &mut buf, &mut state);
        assert!(
            frames.try_lock().unwrap().is_some(),
            "a backlog must not build up behind the encoder"
        );
        assert!(state.in_flight, "still one encode in flight");
    }

    #[test]
    fn a_completed_encode_is_drawn_and_a_stale_frame_is_left_alone() {
        let (frames, mut state) = video();
        let area = Rect::new(0, 0, 20, 5);
        let mut buf = Buffer::empty(area);

        *frames.blocking_lock() = Some(video_frame());
        VideoWidget.render(area, &mut buf, &mut state);
        let placeholder = buf.clone();

        let mut encoded = false;
        for _ in 0..200 {
            VideoWidget.render(area, &mut buf, &mut state);
            if state.encoded.is_some() {
                encoded = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(encoded, "encode worker never produced a frame");
        assert!(!state.in_flight);
        assert_ne!(buf, placeholder, "the frame must replace the placeholder");

        // Nothing new decoded and the area has not moved, so the same frame is
        // drawn again without another encode being dispatched.
        let shown = buf.clone();
        VideoWidget.render(area, &mut buf, &mut state);
        assert!(!state.in_flight, "a stale frame must not be re-encoded");
        assert_eq!(buf, shown);
    }

    #[test]
    fn video_placeholder_renders_status_text_centered() {
        let (_frames, mut state) = video();
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 5));
        VideoWidget.render(buf.area, &mut buf, &mut state);
        // 1 line in a 5-row area starting at row 2, centered horizontally.
        assert_eq!(buf.cell((3, 2)).unwrap().symbol(), "L");
    }

    #[test]
    fn video_error_renders_the_error_text() {
        let (_frames, mut state) = video();
        state.set_error("Media not available".to_string());
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 5));
        VideoWidget.render(buf.area, &mut buf, &mut state);
        assert_eq!(buf.cell((6, 1)).unwrap().symbol(), "M");
    }
}
