use ratatui::prelude::*;
use ratatui::widgets::{Block, Paragraph, Widget, Wrap};

/// Reusable full-screen loading indicator with a spinning animation and an
/// optional contextual status line. Used to avoid leaving the user with a blank
/// terminal while the app starts up or a blocking operation is in flight.
///
/// Animate it by advancing a [`LoadingSpinner`] between draws (e.g. once per
/// terminal frame) and passing its frame to [`LoadingWidget::new`].
pub struct LoadingWidget<'a> {
    /// Text describing what is currently happening (e.g. "Fetching chats...").
    info: Option<String>,
    /// Title shown in the surrounding border.
    title: &'a str,
    /// Current spinner frame index (0..3), producing a rotating animation.
    frame: u8,
}

const SPINNER: [&str; 4] = ["|", "/", "-", "\\"];

/// Mutable spinner state so the animation rotates across draws.
#[derive(Debug, Default, Clone, Copy)]
pub struct LoadingSpinner {
    frame: u8,
}

impl LoadingSpinner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Advance to the next animation frame; call once per draw.
    pub fn tick(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }
}

impl<'a> LoadingWidget<'a> {
    pub fn new(title: &'a str, info: Option<String>, spinner: &mut LoadingSpinner) -> Self {
        Self {
            info,
            title,
            frame: spinner.frame,
        }
    }
}

impl Widget for LoadingWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let symbol = SPINNER[(self.frame as usize) % SPINNER.len()];

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
                symbol.to_string(),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " Loading...",
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
