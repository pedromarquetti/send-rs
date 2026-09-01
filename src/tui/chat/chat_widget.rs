use ratatui::prelude::*;
use ratatui::widgets::{
    Block, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
    StatefulWidget, Widget,
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

        let title: Line<'_> = state
            .selected_chat()
            .map(|chat| {
                let name = chat.contact_name.trim();
                let status = chat.status_label().unwrap_or("");

                let label = if name.is_empty() {
                    "Unnamed chat".to_string()
                } else {
                    name.to_string()
                };

                let bullet = match status {
                    "online" => Span::from("•").style(Style::new().fg(Color::Green)),
                    "" => Span::from(""),
                    _ => Span::from("•"),
                };

                Line::from(vec![
                    Span::raw(format!(" {} ", label)),
                    Span::from(bullet),
                    Span::styled(
                        format!(" {status} "),
                        Style::new()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    ),
                ])
            })
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
            let inner = msgs_area.inner(Margin {
                vertical: 1,
                horizontal: 1,
            });

            let columns = Layout::horizontal([
                Constraint::Fill(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(inner);

            let content = columns[0];
            let scrollbar_col = columns[2];

            let max_lines = Some(5);
            let item_heights: Vec<usize> = history
                .iter()
                .map(|msg| message_line_count(msg, content.width, max_lines).0)
                .collect();

            let total_lines: usize = item_heights.iter().sum();

            let visible = inner.height as usize;
            state.visible_page = visible;

            let items: Vec<ListItem> = history
                .iter()
                .map(|msg| ListItem::new(message_lines(msg, content.width, max_lines)))
                .collect();

            let highlight = if self.focus == Focus::Chat {
                match tag {
                    "TG" => Style::default()
                        .bg(Color::LightBlue)
                        .fg(Color::Black)
                        .add_modifier(Modifier::BOLD),
                    "WA" => Style::default()
                        .bg(Color::Green)
                        .fg(Color::Black)
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
                .highlight_style(highlight)
                .highlight_symbol("> ");

            block.render(msgs_area, buf);
            StatefulWidget::render(list, content, buf, &mut state.message_list_state);

            if total_lines > visible {
                let first_visible_item = state.message_list_state.offset();
                let scrollbar_position: usize = item_heights[..first_visible_item].iter().sum();
                let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
                let mut scrollbar_state = ScrollbarState::new(total_lines.saturating_sub(visible))
                    .position(scrollbar_position)
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

        let compose_context = state
            .pending_edit
            .as_ref()
            .or(state.pending_reply.as_ref())
            .or_else(|| state.selected_message());

        match compose_context {
            Some(msg) => {
                if state.pending_edit.is_some() {
                    self.write.set_block(
                        Block::bordered()
                            .title(format!(" Write - Editing {} ", msg.text))
                            .border_style(write_border),
                    );
                } else if state.pending_reply.is_some() {
                    self.write.set_block(
                        Block::bordered()
                            .title(format!(
                                " Write - Replying to '{}': {} ",
                                msg.sender, msg.text
                            ))
                            .border_style(write_border),
                    );
                } else {
                    self.write.set_block(
                        Block::bordered()
                            .title(" Write ")
                            .border_style(write_border),
                    );
                }
            }
            None => {
                self.write.set_block(
                    Block::bordered()
                        .title(" Write ")
                        .border_style(write_border),
                );
            }
        }

        Widget::render(&*self.write, write_area, buf);
    }
}

/// Word-wraps a text line into chunks that fit within `max_width` characters.
/// Words exceeding the limit are truncated and get `…` appended.
pub(crate) fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![text.to_string()];
    }

    let words: Vec<&str> = text.split_whitespace().collect();

    if words.is_empty() {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    for word in words {
        if current.is_empty() {
            if word.len() > max_width {
                let truncated: String = word.chars().take(max_width.saturating_sub(1)).collect();
                chunks.push(format!("{truncated}…"));
            } else {
                current = word.to_string();
            }
        } else if current.len() + 1 + word.len() <= max_width {
            current.push(' ');
            current.push_str(word);
        } else {
            chunks.push(std::mem::take(&mut current));
            if word.len() > max_width {
                let truncated: String = word.chars().take(max_width.saturating_sub(1)).collect();
                chunks.push(format!("{truncated}…"));
            } else {
                current = word.to_string();
            }
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push(text.to_string());
    }
    chunks
}

/// Renders one message as one or more physical lines, word-wrapped to fit `width` columns.
/// If `max_lines` is set, output is capped and a "Press Enter for full message…" hint is added.
/// A reply indicator ("X replied") is rendered as a line above the message, and outgoing
/// messages that are still pending (or failed) are shown grayed out.
fn message_lines(message: &Message, width: u16, max_lines: Option<usize>) -> Vec<Line<'static>> {
    let sender = if message.from_me {
        "You".to_string()
    } else {
        message.sender.clone()
    };

    let mut result: Vec<Line> = Vec::new();

    if message.reply_to_id.is_some() {
        let who = if message.from_me {
            "You".to_string()
        } else {
            message.sender.clone()
        };

        result.push(Line::from(Span::styled(
            format!("  {who} replied"),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )));
    }

    // Gray out messages that haven't been confirmed on the server yet.
    let pending_style = message.pending || message.failed;
    let body_style = Style::default().fg(if pending_style {
        Color::DarkGray
    } else {
        Color::Gray
    });

    let status_suffix = if message.failed {
        " ⚠ failed"
    } else if message.pending {
        " …"
    } else {
        ""
    };

    let head = format!(
        "{sender} {}{status_suffix}",
        format_timestamp(message.timestamp)
    );

    let header_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);

    let header_width = head.len();

    let mut any_truncated = false;
    let mut text_lines = message.text.lines();

    match text_lines.next() {
        Some(first) => {
            let available = width.saturating_sub(2) as usize;
            let text_avail = available.saturating_sub(header_width + 2);
            let first_chunks = if text_avail > 0 {
                wrap_text(first, text_avail)
            } else {
                wrap_text(first, available.max(1))
            };
            if first.len() > first_chunks.first().map_or(0, |c| c.len()) {
                any_truncated = true;
            }
            if let Some((first_chunk, rest)) = first_chunks.split_first() {
                result.push(Line::from(vec![
                    Span::styled(head.clone(), header_style),
                    Span::raw("  "),
                    Span::styled(first_chunk.clone(), body_style),
                ]));
                for chunk in rest {
                    result.push(Line::from(Span::styled(chunk.clone(), body_style)));
                }
            }
        }
        None => {
            result.push(Line::from(vec![
                Span::styled(head, header_style),
                Span::raw("  "),
            ]));
        }
    }

    for line in text_lines {
        let available = width.saturating_sub(2) as usize;
        let chunks = wrap_text(line, available);
        if line.len() > chunks.first().map_or(0, |c| c.len()) {
            any_truncated = true;
        }
        for chunk in chunks {
            result.push(Line::from(Span::styled(chunk, body_style)));
        }
    }

    // Check if we need to truncate due to max_lines
    let hint_line = Line::from(Span::styled(
        "  Press Enter for full message…",
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    ));

    if let Some(max) = max_lines
        && result.len() > max
    {
        result.truncate(max);
        result.push(hint_line);
        return result;
    }

    if any_truncated {
        result.push(hint_line);
    }

    result
    // TODO:  make this better, 'right align' was not looking good
    //
    // if message.from_me {
    //     result
    //         // .into_iter()
    //         // .map(|line| line.alignment(Alignment::Right))
    //         // .collect()
    // } else {
    //     result
    // }
}

/// Counts the number of visual lines a message occupies at a given width, accounting for
/// word-wrap. Also indicates whether the message was truncated (for the hint line).
fn message_line_count(message: &Message, width: u16, max_lines: Option<usize>) -> (usize, bool) {
    let sender = if message.from_me {
        "You".to_string()
    } else {
        message.sender.clone()
    };
    let head = format!("{sender} {}", format_timestamp(message.timestamp));
    let header_width = head.len();
    let wrap_width = width.saturating_sub(2) as usize;
    let mut count = 0usize;
    let mut any_truncated = false;

    if message.reply_to_id.is_some() {
        count += 1; // reply indicator line
    }

    let mut text_lines = message.text.lines();
    match text_lines.next() {
        Some(first) => {
            let text_avail = wrap_width.saturating_sub(header_width + 2).max(1);
            let chunks = wrap_text(first, text_avail);
            if first.len() > chunks.first().map_or(0, |c| c.len()) {
                any_truncated = true;
            }
            count += chunks.len().max(1);
        }
        None => {
            count += 1;
        }
    }

    for line in text_lines {
        let chunks = wrap_text(line, wrap_width);
        if line.len() > chunks.first().map_or(0, |c| c.len()) {
            any_truncated = true;
        }
        count += chunks.len().max(1);
    }

    if any_truncated {
        count += 1;
    }

    if let Some(max) = max_lines
        && count > max
    {
        return (max + 1, true); // +1 for the hint line
    }

    (count, any_truncated)
}

/// Number of visual rows the Write box needs for the given text at `width` columns, counting
/// soft-wrapped lines with word-boundary wrapping.
fn write_visual_rows(lines: &[String], width: u16) -> usize {
    let wrap_width = width.saturating_sub(2) as usize;
    let mut count = 0;
    for line in lines {
        let chunks = wrap_text(line, wrap_width);
        count += chunks.len().max(1);
    }
    count.max(1)
}

/// Height of the Write box in rows: two border rows plus up to `max_lines` content rows, never
/// exceeding `area_height`.
fn write_box_height(content_rows: usize, max_lines: usize, area_height: u16) -> u16 {
    let rows = content_rows.clamp(1, max_lines.max(1));
    ((rows + 2) as u16).min(area_height)
}

pub(crate) fn format_timestamp(secs: i64) -> String {
    let Some(datetime) = chrono::DateTime::from_timestamp(secs, 0) else {
        return String::new();
    };
    datetime
        .with_timezone(&chrono::Local)
        .format("%d/%m/%Y %H:%M")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::ChatId;

    fn message(text: &str) -> Message {
        Message {
            message_id: "m".into(),
            chat: ChatId::Telegram(1),
            sender: "Alice".into(),
            text: text.into(),
            timestamp: 0,
            from_me: false,
            msg_actions: Vec::new(),
            media: None,
            reply_to_id: None,
            reply_ctx: None,
            pending: false,
            failed: false,
        }
    }

    #[test]
    fn message_lines_splits_on_newlines() {
        let lines = message_lines(&message("first\nsecond\nthird"), 80, None);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].to_string().contains("Alice"));
        assert!(lines[0].to_string().contains("first"));
        assert_eq!(lines[1].to_string(), "second");
        assert_eq!(lines[2].to_string(), "third");
    }

    #[test]
    fn message_lines_empty_text_keeps_header() {
        let lines = message_lines(&message(""), 80, None);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].to_string().contains("Alice"));
    }

    #[test]
    fn message_lines_word_wraps_long_text() {
        let msg = message("hello world this is a long line");
        let lines = message_lines(&msg, 20, None);
        // Header "Alice 01-01 00:00" = 17 chars, text_avail = 20-2-17-2 = -1 → fallback
        // Should produce multiple lines
        assert!(lines.len() > 1);
    }

    #[test]
    fn message_lines_adds_hint_when_truncated() {
        let msg = message("abcdefghijklmnopqrstuvwxyz0123456789");
        let lines = message_lines(&msg, 10, None);
        let last = lines.last().unwrap().to_string();
        assert!(last.contains("Press Enter"));
    }

    #[test]
    fn message_line_count_single_line_message() {
        let msg = message("hello");
        // "Alice 01-01 00:00  hello" = ~25 chars, at width 80 → 1 line, no truncation
        let (count, truncated) = message_line_count(&msg, 80, None);
        assert_eq!(count, 1);
        assert!(!truncated);
    }

    #[test]
    fn message_line_count_multiline_message() {
        let msg = message("line1\nline2\nline3");
        let (count, _truncated) = message_line_count(&msg, 80, None);
        assert!(count >= 3);
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
