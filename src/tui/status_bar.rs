use ratatui::prelude::*;
use ratatui::widgets::{Paragraph, Widget};

use crate::tui::state::Focus;

pub struct StatusBarWidget {
    curr_chat: Option<String>,
    focus: Focus,
}

impl StatusBarWidget {
    pub fn new(curr_chat: Option<String>, focus: Focus) -> Self {
        StatusBarWidget { curr_chat, focus }
    }
}

impl Widget for StatusBarWidget {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        let hint = match self.focus {
            Focus::ChatList => {
                "j/k move (up/down); Enter select; Tab switch focus; i write; s settings; Ctrl+C quit"
            }
            Focus::Chat => {
                "j/k scroll; PgUp/PgDn page; i write; Tab switch focus; s settings; Ctrl+C quit"
            }
            Focus::Write => {
                "Enter send; Shift+Enter newline; Esc back; Tab switch focus; Ctrl+C quit"
            }
            Focus::Overlay => "Esc close overlay",
        };
        let line = match self.curr_chat {
            Some(chat) => Line::from(vec![
                Span::styled(chat, Style::default().add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled(hint, Style::default().fg(Color::DarkGray)),
            ]),
            None => Line::from(vec![
                Span::raw("  "),
                Span::styled(hint, Style::default().fg(Color::DarkGray)),
            ]),
        };

        Paragraph::new(line).render(area, buf);
    }
}
