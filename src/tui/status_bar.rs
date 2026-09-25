use ratatui::prelude::*;
use ratatui::widgets::{Paragraph, Widget};

use crate::tui::state::Focus;

pub struct StatusBarWidget {
    curr_chat: Option<String>,
    focus: Focus,
    status: Option<String>,
}

impl StatusBarWidget {
    pub fn new(curr_chat: Option<String>, focus: Focus, status: Option<String>) -> Self {
        StatusBarWidget {
            curr_chat,
            focus,
            status,
        }
    }
}

impl Widget for StatusBarWidget {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        let hint = match self.focus {
            // TODO: make these hints actually follow the user-defined keys! this is currently
            // hardcoded
            Focus::ChatList => {
                "Ctrl+C quit; j/k move (up/down); Enter select chat; Tab switch focus; i select chat + write; s settings; "
            }
            Focus::Chat => {
                "j/k scroll; G bottom; g Top; PgUp/PgDn page; i write; Tab switch focus; s settings; Ctrl+C quit"
            }
            Focus::Write => {
                "Enter send; Alt+Enter newline; Esc back; Tab switch focus; Ctrl+C quit"
            }
            Focus::Popup => "Esc close popup; g scroll to top",
        };

        let mut spans: Vec<Span<'_>> = Vec::new();
        if let Some(chat) = &self.curr_chat {
            spans.push(Span::styled(
                format!("[{chat}] "),
                Style::default().fg(Color::Cyan).bold(),
            ));
        }
        if let Some(status) = self.status {
            spans.push(Span::styled(status, Style::default().fg(Color::Yellow)));
        } else {
            spans.push(Span::styled(hint, Style::default().fg(Color::DarkGray)));
        }

        Paragraph::new(Line::from(spans)).render(area, buf);
    }
}
