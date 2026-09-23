use ratatui::prelude::*;
use ratatui::widgets::{Block, Paragraph, Widget, Wrap};

/// Where a loading indication is displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadingArea {
    ChatList,
    Chat,
    Inline,
    FullScreen,
}

/// The single source of truth for "a loading indication is active and where".
/// Owned by [`crate::tui::state::AppState`]; renderers only consult it and let
/// every loading action claim the slot via `start_loading` and release it via
/// `finish_loading`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadingState {
    /// Which part of the UI the indication belongs to.
    pub area: LoadingArea,
    /// What is currently happening (e.g. the chat name being loaded).
    pub context: String,
    /// Animation frame for the active indication.
    pub spinner: LoadingSpinner,
}

/// Mutable spinner state so the animation rotates across draws.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LoadingSpinner {
    frame: u8,
}

impl LoadingSpinner {
    /// Advance to the next animation frame; call once per draw.
    pub fn tick(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

    /// Current frame index, mapped to a glyph via [`spinner_symbol`].
    pub(crate) fn frame(&self) -> u8 {
        self.frame
    }
}

const SPINNER: [&str; 4] = ["|", "/", "-", "\\"];

/// The rotating glyph for `frame` (the same sequence used by all loading
/// renderers, so they agree on the animation).
pub(crate) fn spinner_symbol(frame: u8) -> &'static str {
    SPINNER[(frame as usize) % SPINNER.len()]
}

/// Bordered full-area loading indicator with a spinning animation and an
/// optional contextual status line. Used to avoid leaving the user with a blank
/// terminal while the app starts up or a blocking operation is in flight.
pub struct LoadingWidget<'a> {
    /// Text describing what is currently happening (e.g. "Fetching chats...").
    info: Option<String>,
    /// Title shown in the surrounding border.
    title: &'a str,
    /// Current spinner frame index (0..3), producing a rotating animation.
    frame: u8,
}

impl<'a> LoadingWidget<'a> {
    pub fn new(title: &'a str, info: Option<String>, frame: u8) -> Self {
        Self { info, title, frame }
    }
}

impl Widget for LoadingWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let symbol = spinner_symbol(self.frame);

        let block = Block::bordered().title(Span::styled(
            format!(" {} ", self.title),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));

        let inner = block.inner(area);

        // Draw borders first so they sit behind the content.
        Widget::render(block, area, buf);

        let spinner_line = Line::from(vec![
            Span::styled(
                format!(" {symbol} "),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " Loading... ",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);

        let mut content: Vec<Line> = vec![Line::from(""), spinner_line];
        if let Some(info) = &self.info {
            content.push(Line::from(""));
            content.push(Line::from(Span::styled(
                info.to_string(),
                Style::default().fg(Color::DarkGray),
            )));
        }

        let para = Paragraph::new(content).wrap(Wrap { trim: false });
        para.render(inner, buf);
    }
}
