use ratatui::prelude::*;
use ratatui::widgets::{Block, Paragraph, StatefulWidget, Widget, Wrap};
use ratatui_textarea::TextArea;

use crate::backend::Message;

/// Scroll state of the chat history view, tracked by the widget.
#[derive(Default)]
pub struct ChatViewState {
    pub scroll: usize,
    pub page: usize,
}

pub struct ChatView<'a> {
    history: &'a [Message],
    title: &'a str,
    chat_focused: bool,
    write_focused: bool,
    write: &'a mut TextArea<'static>,
}

impl<'a> ChatView<'a> {
    pub fn new(
        history: &'a [Message],
        title: &'a str,
        chat_focused: bool,
        write_focused: bool,
        write: &'a mut TextArea<'static>,
    ) -> Self {
        Self {
            history,
            title,
            chat_focused,
            write_focused,
            write,
        }
    }
}

impl StatefulWidget for ChatView<'_> {
    type State = ChatViewState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let border = if self.chat_focused {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };
        let title = format!(" {} ", self.title);

        let split = Layout::vertical([Constraint::Fill(1), Constraint::Length(3)]).split(area);
        let msgs_area = split[0];
        let write_area = split[1];

        let block = Block::bordered().title(title).border_style(border);
        let lines: Vec<Line> = self.history.iter().map(message_line).collect();

        let total = lines.len() as i64;
        // Content area excludes the two block border rows.
        let visible = (msgs_area.height.saturating_sub(2).max(1)) as i64;
        state.page = visible as usize;
        let bottom = (total - visible).max(0);
        state.scroll = state.scroll.min(bottom as usize);
        let scroll_top = (bottom - state.scroll as i64).max(0) as u16;

        let paragraph = Paragraph::new(lines)
            .block(block)
            .scroll((scroll_top, 0))
            .wrap(Wrap { trim: false });
        paragraph.render(msgs_area, buf);

        let write_border = if self.write_focused {
            Style::default().fg(Color::Yellow)
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
