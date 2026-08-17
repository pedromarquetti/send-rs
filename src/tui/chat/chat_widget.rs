use ratatui::prelude::*;
use ratatui::widgets::{
    Block, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, StatefulWidget, Widget, Wrap,
};
use ratatui_textarea::TextArea;

use crate::backend::Message;
use crate::tui::chat::ChatState;
use crate::tui::state::Focus;

pub struct ChatWidget<'a> {
    focus: Focus,
    write: &'a mut TextArea<'static>,
    max_write_lines: usize,
}

impl<'a> ChatWidget<'a> {
    pub fn new(focus: Focus, write: &'a mut TextArea<'static>, max_write_lines: usize) -> Self {
        Self {
            focus,
            write,
            max_write_lines: max_write_lines.max(1),
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
        let border = match self.focus {
            Focus::Chat | Focus::Write => match tag {
                "TG" => Style::default().fg(Color::LightBlue),
                "WA" => Style::default().fg(Color::Green),
                _ => Style::default().fg(Color::Yellow),
            },
            _ => Style::default(),
        };
        let title = state
            .selected_chat()
            .map(|chat| format!(" {} ", chat.contact_name))
            .unwrap_or_else(|| " No chat selected ".into());
        let block = Block::bordered().title(title).border_style(border);

        let content_width = area.width.saturating_sub(2);
        let write_height = write_box_height(
            write_visual_rows(self.write.lines(), content_width),
            self.max_write_lines,
            area.height,
        );
        let split =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(write_height)]).split(area);
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
            let lines: Vec<Line> = history.iter().flat_map(message_lines).collect();
            let inner = msgs_area.inner(Margin {
                vertical: 1,
                horizontal: 1,
            });
            let columns =
                Layout::horizontal([Constraint::Fill(1), Constraint::Length(1)]).split(inner);
            let content = columns[0];
            let scrollbar_col = columns[1];

            let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
            // Total wrapped content height (in visual rows) at the available width.
            let content_height = paragraph.line_count(content.width);
            let visible = inner.height as usize;
            let (scroll_top, max_scroll) = state
                .selected_chat_mut()
                .map(|chat| {
                    let (scroll, top, max) =
                        chat_scroll_offset(content_height, visible, chat.scroll);
                    chat.scroll = scroll;
                    (top, max)
                })
                .unwrap_or_else(|| (0, content_height.saturating_sub(visible)));

            block.render(msgs_area, buf);
            state.visible_page = visible;
            paragraph.scroll((scroll_top, 0)).render(content, buf);

            if content_height > visible {
                let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
                let mut scrollbar_state = ScrollbarState::new(max_scroll)
                    .position(scroll_top as usize)
                    .viewport_content_length(visible);
                StatefulWidget::render(scrollbar, scrollbar_col, buf, &mut scrollbar_state);
            }
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

        let write_border = if self.focus == Focus::Write {
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

/// Renders one message as one or more physical lines (multi-line messages keep their newlines).
fn message_lines(message: &Message) -> Vec<Line<'static>> {
    let sender = if message.from_me {
        "You".to_string()
    } else {
        message.sender.clone()
    };
    let head = format!("{sender} {}", format_timestamp(message.timestamp));
    let header = Span::styled(
        head,
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    let mut text_lines = message.text.lines();
    let mut result = Vec::new();
    match text_lines.next() {
        Some(first) => {
            result.push(Line::from(vec![
                header,
                Span::raw("  "),
                Span::raw(first.to_string()),
            ]));
            for line in text_lines {
                result.push(Line::from(Span::raw(line.to_string())));
            }
        }
        None => result.push(Line::from(vec![header, Span::raw("  ")])),
    }
    result
}

/// Computes the effective scroll position for a chat viewport.
///
/// `content_height` is the total wrapped content height in rows, `visible_height` the number of
/// rows that fit, and `chat_scroll` the amount scrolled away from the bottom (0 = at the bottom).
/// Returns the clamped `chat_scroll`, the top-row offset to hand to `Paragraph::scroll`, and the
/// maximum scroll amount (`content_height - visible_height`).
fn chat_scroll_offset(
    content_height: usize,
    visible_height: usize,
    chat_scroll: usize,
) -> (usize, u16, usize) {
    let max_scroll = content_height.saturating_sub(visible_height);
    let scroll = chat_scroll.min(max_scroll);
    (scroll, (max_scroll - scroll) as u16, max_scroll)
}

/// Number of visual rows the Write box needs for the given text at `width` columns, counting
/// soft-wrapped lines just like the `TextArea` does (word wrap with glyph fallback).
fn write_visual_rows(lines: &[String], width: u16) -> usize {
    let text: Vec<Line> = lines
        .iter()
        .map(|line| Line::from(Span::raw(line.clone())))
        .collect();
    Paragraph::new(text)
        .wrap(Wrap { trim: false })
        .line_count(width)
        .max(1)
}

/// Height of the Write box in rows: two border rows plus up to `max_lines` content rows, never
/// exceeding `area_height`.
fn write_box_height(content_rows: usize, max_lines: usize, area_height: u16) -> u16 {
    let rows = content_rows.clamp(1, max_lines.max(1));
    ((rows + 2) as u16).min(area_height)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::ChatId;

    fn message(text: &str) -> Message {
        Message {
            id: "m".into(),
            chat: ChatId::Telegram(1),
            sender: "Alice".into(),
            text: text.into(),
            timestamp: 0,
            from_me: false,
        }
    }

    #[test]
    fn message_lines_splits_on_newlines() {
        let lines = message_lines(&message("first\nsecond\nthird"));
        assert_eq!(lines.len(), 3);
        assert!(lines[0].to_string().contains("Alice"));
        assert!(lines[0].to_string().contains("first"));
        assert_eq!(lines[1].to_string(), "second");
        assert_eq!(lines[2].to_string(), "third");
    }

    #[test]
    fn message_lines_empty_text_keeps_header() {
        let lines = message_lines(&message(""));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].to_string().contains("Alice"));
    }

    #[test]
    fn scroll_offset_starts_at_the_bottom() {
        let (scroll, top, max) = chat_scroll_offset(100, 20, 0);
        assert_eq!(scroll, 0);
        assert_eq!(top, 80);
        assert_eq!(max, 80);
    }

    #[test]
    fn scroll_offset_counts_rows_from_the_bottom() {
        let (scroll, top, max) = chat_scroll_offset(100, 20, 10);
        assert_eq!(scroll, 10);
        assert_eq!(top, 70);
        assert_eq!(max, 80);
    }

    #[test]
    fn scroll_offset_is_clamped_to_content() {
        let (scroll, top, max) = chat_scroll_offset(100, 20, 500);
        assert_eq!(scroll, 80);
        assert_eq!(top, 0);
        assert_eq!(max, 80);
    }

    #[test]
    fn scroll_offset_is_zero_when_content_fits() {
        let (scroll, top, max) = chat_scroll_offset(10, 20, 5);
        assert_eq!(scroll, 0);
        assert_eq!(top, 0);
        assert_eq!(max, 0);
    }

    #[test]
    fn write_box_height_single_line_is_three_rows() {
        assert_eq!(write_box_height(0, 5, 40), 3);
        assert_eq!(write_box_height(1, 5, 40), 3);
    }

    #[test]
    fn write_box_height_grows_with_lines() {
        assert_eq!(write_box_height(2, 5, 40), 4);
        assert_eq!(write_box_height(5, 5, 40), 7);
    }

    #[test]
    fn write_box_height_is_capped_by_max_lines() {
        assert_eq!(write_box_height(9, 5, 40), 7);
        assert_eq!(write_box_height(4, 2, 40), 4);
    }

    #[test]
    fn write_box_height_never_exceeds_area() {
        assert_eq!(write_box_height(5, 5, 4), 4);
        assert_eq!(write_box_height(5, 5, 2), 2);
    }

    #[test]
    fn write_visual_rows_counts_wrapped_lines() {
        assert_eq!(write_visual_rows(&["hello".into()], 10), 1);
        assert_eq!(write_visual_rows(&["hello world".into()], 5), 2);
        assert_eq!(write_visual_rows(&["hello".into(), "world".into()], 100), 2);
        assert_eq!(write_visual_rows(&["".into()], 10), 1);
    }

    #[test]
    fn scrollbar_thumb_reaches_bottom_when_scrolled_to_bottom() {
        let visible = 5u16;
        let content_height = 20usize;
        let max_scroll = content_height - visible as usize;
        let mut buf = Buffer::empty(Rect::new(0, 0, 1, visible));
        let mut state = ScrollbarState::new(max_scroll)
            .position(max_scroll)
            .viewport_content_length(visible as usize);
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .render(buf.area, &mut buf, &mut state);
        let symbols: Vec<&str> = buf.content().iter().map(|c| c.symbol()).collect();
        assert_eq!(symbols.last().copied(), Some("█"));
        assert_eq!(*symbols.first().unwrap(), "║");
    }

    #[test]
    fn scrollbar_thumb_starts_at_top_when_scrolled_to_top() {
        let visible = 5u16;
        let content_height = 20usize;
        let max_scroll = content_height - visible as usize;
        let mut buf = Buffer::empty(Rect::new(0, 0, 1, visible));
        let mut state = ScrollbarState::new(max_scroll)
            .position(0)
            .viewport_content_length(visible as usize);
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .render(buf.area, &mut buf, &mut state);
        let symbols: Vec<&str> = buf.content().iter().map(|c| c.symbol()).collect();
        assert_eq!(*symbols.first().unwrap(), "█");
    }
}
