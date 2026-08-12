use ratatui::layout::Alignment;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use crate::tui::state::{Overlay, OverlayKind};

pub fn render(frame: &mut Frame, overlay: &Overlay) {
    let area = frame.area();
    let width = 60.min(area.width.saturating_sub(4));
    let height = 5.min(area.height);
    if width == 0 || height == 0 {
        return;
    }
    let popup = Rect {
        x: (area.width - width) / 2,
        y: (area.height - height) / 2,
        width,
        height,
    };
    Clear.render(popup, frame.buffer_mut());
    let border = match overlay.kind {
        OverlayKind::Info => Style::default().fg(Color::Blue),
        OverlayKind::Error => Style::default().fg(Color::Red),
        OverlayKind::Warn => Style::default().fg(Color::Yellow),
    };
    let block = Block::bordered()
        .title(format!(" {} ", overlay.title.clone()))
        .title_bottom(" Press ESC to close this! ")
        .border_style(border);
    let paragraph = Paragraph::new(overlay.message.clone())
        .block(block)
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, popup);
}
