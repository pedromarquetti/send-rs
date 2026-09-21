use std::sync::mpsc::{self, Receiver};

use image::DynamicImage;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, StatefulWidget, Widget};
use ratatui_image::errors::Errors;
use ratatui_image::picker::Picker;
use ratatui_image::thread::{ResizeRequest, ResizeResponse, ThreadProtocol};
use ratatui_image::{Resize, StatefulImage};
use tokio::sync::mpsc::UnboundedSender;

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
            let text_width = area.width.saturating_sub(2) as usize;
            let mut lines: Vec<Line> = wrap_text(&state.status, text_width)
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
            for result in state.results.try_iter() {
                if let Ok(completed) = result {
                    state.protocol.update_resized_protocol(completed);
                    encoded = true;
                }
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
}
