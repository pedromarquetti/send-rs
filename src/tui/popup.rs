use ratatui::prelude::*;
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use crate::helpers::{calc_height, popup_area};
use crate::tui::state::PopupState;

pub enum PopupKind {
    Info(String),
    Error(String),
    Warn(String),
}

impl Default for PopupKind {
    fn default() -> Self {
        Self::Info(String::new())
    }
}

#[derive(Default)]
pub struct PopUp {}

impl PopUp {
    pub fn new() -> Self {
        Self {}
    }
    pub fn handle_data_render(&self, data: &String, area: Rect, buf: &mut Buffer, block: Block) {
        let width = 80.min(area.width.saturating_sub(4));
        let msg = format!("{data}\n\nPress ESC to close");
        let height = calc_height(&msg, width, area, false);
        let popup_area = popup_area(area, width, height);
        let paragraph = Paragraph::new(msg)
            // .scroll((self.idx as u16, 0))
            .wrap(Wrap { trim: false })
            .block(block);
        Clear.render(popup_area, buf);
        Widget::render(paragraph, popup_area, buf);
    }
}

impl StatefulWidget for &mut PopUp {
    type State = PopupState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        match &state.popup_type {
            PopupKind::Error(data) => {
                let block = Block::bordered()
                    .title("Error")
                    .border_style(Style::default().fg(Color::Red).bg(Color::Black));
                self.handle_data_render(data, area, buf, block);
            }
            PopupKind::Info(data) => {
                let block = Block::bordered()
                    .title("Info")
                    .border_style(Style::default().fg(Color::Blue).bg(Color::Black));
                self.handle_data_render(data, area, buf, block);
            }
            PopupKind::Warn(data) => {
                let block = Block::bordered()
                    .title("Warning")
                    .border_style(Style::default().fg(Color::Yellow).bg(Color::Black));
                self.handle_data_render(data, area, buf, block);
            }
        }
    }
}
