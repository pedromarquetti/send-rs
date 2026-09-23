use ratatui::prelude::*;
use ratatui::widgets::{
    Block, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
    StatefulWidget, Widget,
};
use ratatui_textarea::TextArea;

use crate::backend::Message;
use crate::helpers::wrap_text;
use crate::tui::chat::ChatState;
use crate::tui::state::Focus;

pub struct ChatWidget<'a> {
    focus: Focus,
    write: &'a mut TextArea<'static>,
    max_write_lines: usize,
    /// Precomputed bottom-title search bar (empty when no search is active).
    search_bar: Line<'static>,
    /// Active message-search query text used to highlight matches.
    message_needle: Option<String>,
}

impl<'a> ChatWidget<'a> {
    pub fn new(
        focus: Focus,
        write: &'a mut TextArea<'static>,
        max_write_lines: usize,
        search_bar: Line<'static>,
        message_needle: Option<String>,
    ) -> Self {
        Self {
            focus,
            write,
            max_write_lines: max_write_lines.max(1),
            search_bar,
            message_needle,
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
                let verified = if chat.verified { "✓ " } else { "" };
                let name = chat.contact_name.trim();
                let status = chat.status_label().unwrap_or("");

                let label = if name.is_empty() {
                    "Unnamed chat".to_string()
                } else {
                    format!("{verified}{name}")
                };

                let bullet = match status {
                    "online" => Span::from("•").style(Style::new().fg(Color::Green)),
                    "" => Span::from(""),
                    _ => Span::from("•").style(Style::new().fg(Color::DarkGray)),
                };

                Line::from(vec![
                    Span::raw(format!(" {} ", label)),
                    bullet,
                    Span::styled(
                        format!(" {status} "),
                        Style::new()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    ),
                ])
            })
            .unwrap_or_else(|| " No chat selected ".into());

        let block = Block::bordered()
            .title(title)
            .title_bottom(self.search_bar.clone())
            .border_style(border);

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

            let needle = self.message_needle.as_deref();

            // Render each message once; the ListItem line count IS the item
            // height, so the scrollbar and the list always agree.
            let visible = inner.height as usize;
            state.visible_page = visible;

            let mut item_heights: Vec<usize> = Vec::with_capacity(history.len());
            let mut total_lines = 0usize;
            //BUG: messages are appearing with split lines:
            // messages should appear like this
            //
            // not
            // like
            // this
            let items: Vec<ListItem> = history
                .iter()
                .map(|msg| {
                    let lines = message_lines(msg, content.width, max_lines, needle);
                    item_heights.push(lines.len());
                    total_lines += lines.len();
                    ListItem::new(lines)
                })
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

/// Renders one message as one or more physical lines, word-wrapped to fit `width` columns.
/// If `max_lines` is set, output is capped and a "Press Enter for full message…" hint is added.
/// A reply indicator ("X replied") is rendered as a line above the message, and outgoing
/// messages that are still pending (or failed) are shown grayed out.
fn message_lines(
    message: &Message,
    width: u16,
    max_lines: Option<usize>,
    needle: Option<&str>,
) -> Vec<Line<'static>> {
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

    // A chunk is only "truncated" when word-wrapping had to cut a word short
    // and append `…`. Merely soft-wrapping a multi-word line is not truncation:
    // the full text is still in the list, so no hint line is warranted.
    let mut any_truncated = false;
    let available = width.saturating_sub(2) as usize;
    // The first body chunk shares the header line, so it wraps one `header`
    // width shorter than the continuation lines.
    let text_avail = available.saturating_sub(header_width + 2);

    // Media messages render the caption plus the media-type label (or the
    // "click to show" fallback) instead of `message.text`; the label is gray +
    // italic while the caption keeps the normal body style.
    let media_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC);

    // Physical body lines without the header. Media messages lead with the
    // media-type label ("Image:") on its own line; the caption follows below.
    // Plain text messages render the body text directly.
    let body_lines: Vec<Line> = match &message.media {
        Some(media) => {
            let caption = media
                .caption
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty());

            match caption {
                Some(caption) => {
                    // Media message: the media-type label leads on its own
                    // line ("Image:"); the caption follows below, wrapped to
                    // the full body width.
                    let mut lines: Vec<Line> = Vec::new();
                    lines.push(Line::from(Span::styled(
                        format!("{}:", media.kind.label()),
                        media_style,
                    )));

                    for text_line in caption.lines() {
                        for chunk in wrap_text(text_line, available.max(1)) {
                            any_truncated |= chunk.ends_with('…');
                            lines.push(Line::from(body_spans(&chunk, needle, body_style)));
                        }
                    }

                    lines
                }
                None => vec![Line::from(vec![Span::styled(
                    format!("{}, click to show", media.kind.label()),
                    media_style,
                )])],
            }
        }
        None => {
            let mut lines: Vec<Line> = Vec::new();
            let mut text_lines = message.text.lines();

            if let Some(first) = text_lines.next() {
                let first_chunks = if text_avail > 0 {
                    wrap_text(first, text_avail)
                } else {
                    wrap_text(first, available.max(1))
                };
                any_truncated |= first_chunks.iter().any(|c| c.ends_with('…'));
                for chunk in first_chunks {
                    lines.push(Line::from(body_spans(&chunk, needle, body_style)));
                }
            }

            for line in text_lines {
                for chunk in wrap_text(line, available) {
                    any_truncated |= chunk.ends_with('…');
                    lines.push(Line::from(body_spans(&chunk, needle, body_style)));
                }
            }
            lines
        }
    };

    let mut body_lines = body_lines.into_iter();
    if let Some(first) = body_lines.next() {
        let mut spans = vec![Span::styled(head, header_style), Span::raw("  ")];
        spans.extend(first);
        result.push(Line::from(spans));

        result.extend(body_lines);
    } else {
        result.push(Line::from(vec![
            Span::styled(head, header_style),
            Span::raw("  "),
        ]));
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

/// Split `text` into styled spans, flagging every case-insensitive occurrence
/// of `needle` with a highlight. Without a needle the whole text keeps `style`.
fn body_spans(text: &str, needle: Option<&str>, style: Style) -> Vec<Span<'static>> {
    let needle = needle.map(str::to_lowercase);
    let Some(needle) = needle.filter(|n| !n.is_empty()) else {
        return vec![Span::styled(text.to_string(), style)];
    };

    let chars: Vec<char> = text.chars().collect();
    let needle_chars: Vec<char> = needle.chars().collect();
    let len = needle_chars.len();

    let mut segments: Vec<(String, bool)> = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        let is_match = i + len <= chars.len()
            && chars[i..i + len].iter().collect::<String>().to_lowercase() == needle;

        if is_match {
            let matched: String = chars[i..i + len].iter().collect();
            segments.push((matched, true));
            i += len;
        } else {
            match segments.last_mut() {
                Some((seg, false)) => seg.push(chars[i]),
                _ => segments.push((chars[i].to_string(), false)),
            }
            i += 1;
        }
    }

    let highlight = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);

    segments
        .into_iter()
        .map(|(seg, matched)| Span::styled(seg, if matched { highlight } else { style }))
        .collect()
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
    use crate::backend::{ChatId, MediaKind, MessageMedia};

    fn message(text: &str) -> Message {
        Message {
            message_id: "m".into(),
            chat: ChatId::Telegram(1),
            sender: "Alice".into(),
            author_id: None,
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

    fn media_message(kind: MediaKind, caption: Option<&str>) -> Message {
        let mut msg = message("ignored by the media render path");
        msg.media = Some(MessageMedia {
            kind,
            caption: caption.map(str::to_string),
            file_name: None,
            duration_secs: None,
            waveform: None,
        });
        msg
    }

    #[test]
    fn message_lines_splits_on_newlines() {
        let lines = message_lines(&message("first\nsecond\nthird"), 80, None, None);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].to_string().contains("Alice"));
        assert!(lines[0].to_string().contains("first"));
        assert_eq!(lines[1].to_string(), "second");
        assert_eq!(lines[2].to_string(), "third");
    }

    #[test]
    fn message_lines_empty_text_keeps_header() {
        let lines = message_lines(&message(""), 80, None, None);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].to_string().contains("Alice"));
    }

    #[test]
    fn message_lines_media_with_caption_puts_label_on_its_own_line() {
        let msg = media_message(MediaKind::Image, Some("sunset over the lake"));
        let lines = message_lines(&msg, 80, None, None);
        assert_eq!(lines.len(), 2, "media label line + caption line");
        let label_line = lines[0].to_string();
        assert!(
            label_line.contains("Alice 31/12/1969 21:00  Image:"),
            "the media-type label leads the first line, got: {label_line}"
        );
        assert!(
            !label_line.contains("sunset"),
            "the caption must not share the media label line, got: {label_line}"
        );
        let caption_line = lines[1].to_string();
        assert!(
            !caption_line.contains("ignored by the media render path"),
            "chat view renders `media`, not `message.text`, got: {caption_line}"
        );

        let italic: Vec<String> = lines[0]
            .spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::ITALIC))
            .map(|s| s.to_string())
            .collect();
        assert_eq!(italic, ["Image:"], "colon stays attached to the label");

        let caption_span = lines[1]
            .spans
            .iter()
            .find(|s| s.to_string().contains("sunset"))
            .unwrap();
        assert!(
            !caption_span.style.add_modifier.contains(Modifier::ITALIC),
            "caption keeps the plain body style"
        );
    }

    #[test]
    fn message_lines_media_caption_wraps_beneath_the_label() {
        let msg = media_message(
            MediaKind::Image,
            Some("a long caption that wraps across several lines at this narrow width"),
        );
        let lines = message_lines(&msg, 20, None, None);
        assert!(
            lines.len() >= 3,
            "label line + wrapped caption lines, got {}",
            lines.len()
        );
        assert!(lines[0].to_string().contains("Image:"));
        assert!(
            lines[1..].iter().all(|l| !l.to_string().contains("Image:")),
            "caption lines must not repeat the media label"
        );
    }

    #[test]
    fn message_lines_media_without_caption_shows_italic_fallback() {
        let msg = media_message(MediaKind::Image, None);
        let lines = message_lines(&msg, 80, None, None);
        assert_eq!(lines.len(), 1);
        let body = lines[0].spans.last().unwrap();
        assert_eq!(body.to_string(), "Image, click to show");
        assert_eq!(body.style.fg, Some(Color::DarkGray));
        assert!(body.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn message_lines_plain_text_is_unaffected_by_media_path() {
        let msg = message("hello, nothing special");
        let lines = message_lines(&msg, 80, None, None);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].to_string().contains("hello, nothing special"));
        assert!(lines[0].spans.last().unwrap().style.add_modifier.is_empty());
    }

    #[test]
    fn message_lines_word_wraps_long_text() {
        let msg = message("hello world this is a long line");
        let lines = message_lines(&msg, 20, None, None);
        // Header "Alice 01-01 00:00" = 17 chars, text_avail = 20-2-17-2 = -1 → fallback
        // Should produce multiple lines
        assert!(lines.len() > 1);
    }

    #[test]
    fn message_lines_adds_hint_when_truncated() {
        let msg = message("abcdefghijklmnopqrstuvwxyz0123456789");
        let lines = message_lines(&msg, 10, None, None);
        let last = lines.last().unwrap().to_string();
        assert!(last.contains("Press Enter"));
    }

    #[test]
    fn message_height_matches_line_count_for_height_mapping() {
        // The scrollbar height is the exact Line count from message_lines; the
        // two must never disagree.
        let msg = message("hello");
        assert_eq!(message_lines(&msg, 80, None, None).len(), 1);
    }

    #[test]
    fn message_height_counts_reply_indicator_and_wrapped_lines() {
        let mut msg = message("some words that get soft-wrapped at this narrow width");
        msg.reply_to_id = Some("x".into());
        let count = message_lines(&msg, 20, None, None).len();
        let plain = message_lines(
            &message("some words that get soft-wrapped at this narrow width"),
            20,
            None,
            None,
        )
        .len();
        assert_eq!(count, plain + 1, "reply indicator adds exactly one line");
    }

    #[test]
    fn message_height_caps_at_max_lines_with_hint() {
        let msg = message("a\nb\nc\nd\ne\nf");
        let lines = message_lines(&msg, 80, Some(5), None);
        assert_eq!(lines.len(), 6, "5 content lines + the hint line");
        assert!(lines.last().unwrap().to_string().contains("Press Enter"));
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
    fn body_spans_highlights_case_insensitive_matches() {
        let style = Style::default().fg(Color::Gray);
        let spans = body_spans("say Hello then hello again", Some("hello"), style);
        assert_eq!(
            spans.iter().map(|s| s.to_string()).collect::<String>(),
            "say Hello then hello again"
        );
        let highlighted = spans
            .iter()
            .filter(|s| s.style.bg == Some(Color::Yellow))
            .count();
        assert_eq!(highlighted, 2);
    }

    #[test]
    fn body_spans_without_needle_is_a_single_span() {
        let style = Style::default().fg(Color::Gray);
        let spans = body_spans("plain text", None, style);
        assert_eq!(spans.len(), 1);
    }

    #[test]
    fn body_spans_ignores_empty_needle() {
        let style = Style::default().fg(Color::Gray);
        let spans = body_spans("plain text", Some(""), style);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].to_string(), "plain text");
    }

    #[test]
    fn message_lines_highlights_needle_occurrences_in_body() {
        let lines = message_lines(&message("say hello there"), 80, None, Some("hello"));
        let highlighted = lines[0]
            .spans
            .iter()
            .filter(|s| s.style.bg == Some(Color::Yellow))
            .count();
        assert_eq!(highlighted, 1);
        assert!(lines[0].to_string().contains("hello"));
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
