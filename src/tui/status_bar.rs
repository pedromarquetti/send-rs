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
            Focus::ChatList => {
                "Ctrl+C quit; j/k move (up/down); Enter select chat; Tab switch focus; i select chat + write; s settings; "
            }
            Focus::Chat => {
                "j/k scroll; G bottom; PgUp/PgDn page; i write; Tab switch focus; s settings; Ctrl+C quit"
            }
            Focus::Write => {
                "Enter send; Alt+Enter newline; Esc back; Tab switch focus; Ctrl+C quit"
            }
            Focus::Popup => "Esc close overlay",
        };

        let line = if let Some(status) = self.status {
            Span::styled(status, Style::default().fg(Color::Yellow))
        } else {
            Span::styled(hint, Style::default().fg(Color::DarkGray))
        };

        Paragraph::new(line).render(area, buf);
    }
}
