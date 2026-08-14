use ratatui::prelude::*;
use ratatui::widgets::{Block, Paragraph, StatefulWidget, Widget, Wrap};
use ratatui_textarea::TextArea;

use crate::backend::Message;
use crate::tui::chat::ChatState;

pub struct ChatWidget<'a> {
    chat_focused: bool,
    write_focused: bool,
    write: &'a mut TextArea<'static>,
}

impl<'a> ChatWidget<'a> {
    pub fn new(chat_focused: bool, write_focused: bool, write: &'a mut TextArea<'static>) -> Self {
        Self {
            chat_focused,
            write_focused,
            write,
        }
    }
}

impl StatefulWidget for ChatWidget<'_> {
    type State = ChatState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let tag = state
            .selected_chat()
            .map(|chat| chat.id.tag())
            .unwrap_or_else(|| "ME");
        let border = if self.chat_focused {
            match tag {
                "TG" => Style::default().fg(Color::LightBlue),
                "WA" => Style::default().fg(Color::Green),
                _ => Style::default().fg(Color::Yellow),
            }
        } else {
            Style::default()
        };
        let title = state
            .selected_chat()
            .map(|chat| format!(" {} ", chat.contact_name))
            .unwrap_or_else(|| " No chat selected ".into());
        let block = Block::bordered().title(title).border_style(border);

        let split = Layout::vertical([Constraint::Fill(1), Constraint::Length(3)]).split(area);
        let msgs_area = split[0];
        let write_area = split[1];

        let opened = state.selected_chat().and_then(|chat| {
            state
                .open_chat
                .as_ref()
                .filter(|open| open.chat.id == chat.id)
                .map(|open| open.history.as_slice())
        });

        if let Some(history) = opened {
            let lines: Vec<Line> = history.iter().map(message_line).collect();
            let total = lines.len() as i64;
            // Content area excludes the two block border rows.
            // BUG: this is still breaking with larger lines/a lot of text
            // TODO: try making each message a ListItem so:
            // 1. each msg is selectable
            // 2. easier to wrap text
            // 3. msg interaction (reaction? reply .... )
            let visible = (msgs_area.height.saturating_sub(2).max(1)) as i64;
            state.visible_page = visible as usize;
            let bottom = (total - visible).max(0);
            let scroll = state
                .selected_chat_mut()
                .map(|chat| {
                    chat.scroll = chat.scroll.min(bottom as usize);
                    chat.scroll
                })
                .unwrap_or(0);
            let scroll_top = (bottom - scroll as i64).max(0) as u16;

            let paragraph = Paragraph::new(lines)
                .block(block)
                .scroll((scroll_top, 0))
                .wrap(Wrap { trim: false });
            paragraph.render(msgs_area, buf);
        } else {
            let hint = if state.selected_chat().is_some() {
                "Press Enter to open this chat"
            } else {
                "No chat selected. j/k move, Enter open."
            };
            let paragraph = Paragraph::new(Line::from(Span::styled(
                hint,
                Style::default().fg(Color::DarkGray),
            )))
            .block(block)
            .alignment(Alignment::Center);
            paragraph.render(msgs_area, buf);
        }

        let write_border = if self.write_focused {
            border
        } else {
            Style::default()
        };
        self.write.set_block(
            Block::bordered()
                .title(" Write ")
                .border_style(write_border),
        );
        Widget::render(&*self.write, write_area, buf);
    }
}

fn message_line(message: &Message) -> Line<'static> {
    let sender = if message.from_me {
        "You".to_string()
    } else {
        message.sender.clone()
    };
    let head = format!("{sender} {}", format_timestamp(message.timestamp));
    Line::from(vec![
        Span::styled(
            head,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::raw(message.text.clone()),
    ])
}

fn format_timestamp(secs: i64) -> String {
    let Some(datetime) = chrono::DateTime::from_timestamp(secs, 0) else {
        return String::new();
    };
    datetime
        .with_timezone(&chrono::Local)
        .format("%m-%d %H:%M")
        .to_string()
}
