use ratatui::prelude::*;
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, StatefulWidget, Widget};

use crate::backend::Chat;
use crate::tui::state::Focus;

pub struct ChatList<'a> {
    chats: &'a [Chat],
    curr_tag: Option<&'static str>,
    focus: Focus,
}

impl<'a> ChatList<'a> {
    pub fn new(curr_tag: Option<&'static str>, chats: &'a [Chat], focus: Focus) -> Self {
        Self {
            chats,
            focus,
            curr_tag,
        }
    }
}

impl StatefulWidget for ChatList<'_> {
    type State = ListState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let border = if self.focus == Focus::ChatList {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };
        let block = Block::bordered().title(" Chats ").border_style(border);

        if self.chats.is_empty() {
            let message = Paragraph::new("No chats yet.\nPress s to enable a provider.")
                .block(block);
            message.render(area, buf);
            return;
        }

        let items: Vec<ListItem> = self
            .chats
            .iter()
            .map(|item| {
                let mut spans = Vec::new();
                spans.push(Span::styled(
                    format!("{:>2} ", item.id.tag()),
                    Style::default().fg(Color::DarkGray),
                ));

                let name_style = if item.unread {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                spans.push(Span::styled(item.contact_name.clone(), name_style));
                if item.unread_count > 0 {
                    spans.push(Span::styled(
                        format!(" ({})", item.unread_count),
                        Style::default().fg(Color::Yellow),
                    ));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();

        let highlight = if self.focus == Focus::ChatList {
            match self.curr_tag {
                Some("TG") => Style::default()
                    .bg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
                Some("WA") => Style::default()
                    .bg(Color::Green)
                    .add_modifier(Modifier::BOLD),
                _ => Style::default()
                    .bg(Color::Yellow)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            }
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        };
        let list = List::new(items)
            .block(block)
            .highlight_style(highlight)
            .highlight_symbol("> ");
        StatefulWidget::render(list, area, buf, state);
    }
}
