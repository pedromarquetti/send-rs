use ratatui::prelude::*;
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, StatefulWidget};

use crate::backend::Chat;
use crate::tui::search::SearchState;
use crate::tui::state::Focus;

pub struct ChatList<'a> {
    chats: Vec<&'a Chat>,
    curr_tag: Option<&'static str>,
    focus: Focus,
    search: &'a SearchState,
}

impl<'a> ChatList<'a> {
    pub fn new(
        curr_tag: Option<&'static str>,
        chats: Vec<&'a Chat>,
        focus: Focus,
        search: &'a SearchState,
    ) -> Self {
        Self {
            chats,
            focus,
            curr_tag,
            search,
        }
    }
}

impl StatefulWidget for ChatList<'_> {
    type State = ListState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let border = if self.focus == Focus::ChatList {
            match self.curr_tag {
                Some("TG") => Style::default().fg(Color::Blue),
                Some("WA") => Style::default().fg(Color::Green),
                _ => Style::default(),
            }
        } else {
            Style::default()
        };

        let block = Block::bordered()
            .title(" Chats ")
            .title_bottom(search_title(self.search))
            .border_style(border);

        if self.chats.is_empty() {
            let message = if self.search.is_active() {
                "No matching chats."
            } else {
                "No chats yet.\nPress s to enable a provider."
            };
            let paragraph = Paragraph::new(message).block(block);
            paragraph.render(area, buf);
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

                if item.fixed {
                    spans.push(Span::styled("📌 ", Style::default().fg(Color::Yellow)));
                }

                let name_style = if item.unread {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };

                let display_name = item.contact_name.trim();

                if item.verified {
                    spans.push(Span::styled("✓ ", Style::default().fg(Color::LightGreen)));
                }

                spans.push(Span::styled(display_name.to_string(), name_style));

                if let Some(status) = item.status_label() {
                    let bullet = match status {
                        "online" => Span::from(" •").style(Style::new().fg(Color::Green)),
                        _ => Span::from(""),
                    };

                    spans.push(bullet);
                }

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

/// Bottom-title line for the chat list block: the search input when a search
/// is active, otherwise empty. The cursor is drawn as a highlighted cell only
/// while the user is actually typing.
fn search_title(search: &SearchState) -> Line<'static> {
    if !search.is_active() {
        return Line::default();
    }

    let label = Span::styled(" Search: ", Style::default().fg(Color::DarkGray));
    let text = search.query_text().to_string();
    let body = Style::default().fg(Color::Gray);

    if search.is_inserting() {
        let mut chars: Vec<char> = text.chars().collect();
        let col = search.cursor_col().min(chars.len());
        let before: String = chars[..col].iter().collect();
        let cursor = if col < chars.len() {
            chars.remove(col)
        } else {
            ' '
        };

        Line::from(vec![
            label,
            Span::styled(before, body),
            Span::styled(
                cursor.to_string(),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ])
    } else {
        Line::from(vec![label, Span::styled(text, body)])
    }
}
