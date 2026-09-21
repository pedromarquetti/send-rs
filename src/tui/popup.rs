use ratatui::prelude::*;
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::backend::{Message, MessageAction};
use crate::helpers::{calc_height, popup_area, wrap_text};
use crate::tui::chat::chat_widget::format_timestamp;
use crate::tui::image::{ImageWidget, ImageWidgetState};
use crate::tui::state::PopupState;

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum PopupKind {
    Info(String),
    Error(String),
    Warn(String),
    Message(Message),
    Image(ImagePopup),
    Question(String),
}

pub struct ImagePopup {
    pub msg: Message,
    pub view: ImageWidgetState,
}

impl std::fmt::Debug for ImagePopup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImagePopup")
            .field("msg", &self.msg)
            .finish_non_exhaustive()
    }
}

impl Default for PopupKind {
    fn default() -> Self {
        Self::Info(String::new())
    }
}

#[derive(Default)]
pub struct PopUp {}

impl PopUp {
    pub fn new() -> Self {
        Self {}
    }
    pub fn handle_data_render(&self, data: &String, area: Rect, buf: &mut Buffer, block: Block) {
        let width = 80.min(area.width.saturating_sub(4));
        let content_width = width.saturating_sub(2) as usize;
        let msg = format!("{data}\n\nPress ESC to close");
        let height = calc_height(&msg, width, area, false);
        let popup_area = popup_area(area, width, height);
        let lines: Vec<Line> = msg
            .lines()
            .flat_map(|line| {
                let chunks = wrap_text(line, content_width);
                if chunks.is_empty() {
                    vec![Line::from(Span::raw(""))]
                } else {
                    chunks
                        .into_iter()
                        .map(|c| Line::from(Span::raw(c)))
                        .collect()
                }
            })
            .collect();
        let paragraph = Paragraph::new(lines).block(block);
        Clear.render(popup_area, buf);
        Widget::render(paragraph, popup_area, buf);
    }
}

impl StatefulWidget for &mut PopUp {
    type State = PopupState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let width = 80.min(area.width.saturating_sub(4));
        let content_width = width.saturating_sub(2) as usize;

        match &mut state.popup_type {
            PopupKind::Question(data) => {
                let total_lines = data.lines().count();
                let popup_height = (total_lines as u16 + 3)
                    .min(area.height.saturating_sub(4))
                    .max(5);
                let popup_area = popup_area(area, width, popup_height);

                Clear.render(popup_area, buf);

                let block = Block::bordered()
                    .title(" ? ")
                    .border_style(Style::default().fg(Color::Red).bg(Color::Black));

                let options: Vec<Span> = vec![Span::from(" [1] Yes "), Span::from(" [2] No ")];

                Paragraph::new(format!("{data}\n\n{}", Line::from(options)))
                    .scroll((state.scroll_idx as u16, 0))
                    .block(block.clone())
                    .render(popup_area, buf);
            }

            PopupKind::Error(data) => {
                let block = Block::bordered()
                    .title(" Error ")
                    .border_style(Style::default().fg(Color::Red).bg(Color::Black));
                self.handle_data_render(data, area, buf, block);
            }

            PopupKind::Info(data) => {
                let block = Block::bordered()
                    .title(" Info ")
                    .border_style(Style::default().fg(Color::Blue).bg(Color::Black));
                self.handle_data_render(data, area, buf, block);
            }

            PopupKind::Warn(data) => {
                let block = Block::bordered()
                    .title(" Warning ")
                    .border_style(Style::default().fg(Color::Yellow).bg(Color::Black));
                self.handle_data_render(data, area, buf, block);
            }

            PopupKind::Message(msg) => {
                let block = Block::bordered()
                    .title(" Message ")
                    .border_style(Style::default().fg(Color::Cyan).bg(Color::Black));

                let mut content_lines: Vec<Line> = Vec::new();
                let status = if msg.failed {
                    " ⚠ failed"
                } else if msg.pending {
                    " … sending"
                } else {
                    ""
                };

                let head = format!("{} {}{status}", msg.sender, format_timestamp(msg.timestamp));

                content_lines.push(Line::from(Span::styled(
                    head,
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                )));

                if let Some(media) = &msg.media {
                    let media_label = format!("  {}: click to show", media.kind.label());
                    content_lines.push(Line::from(Span::styled(
                        media_label,
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )));
                    content_lines.push(Line::from(Span::styled(
                        "  Terminal media preview is not implemented yet ",
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    )));
                    if let Some(caption) = media
                        .caption
                        .as_ref()
                        .filter(|caption| !caption.trim().is_empty())
                    {
                        content_lines.push(Line::from(Span::raw("")));
                        for text_line in caption.lines() {
                            let chunks = wrap_text(text_line, content_width);
                            for chunk in chunks {
                                content_lines.push(Line::from(Span::raw(chunk)));
                            }
                        }
                    }
                }

                // Quote the original message this one replies to (if any).
                if let Some(reply) = &msg.reply_ctx {
                    content_lines.push(Line::from(Span::styled(
                        "  ─ Reply to:",
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::BOLD),
                    )));
                    content_lines.push(Line::from(Span::styled(
                        format!("    {} {}", reply.sender, format_timestamp(reply.timestamp)),
                        Style::default().fg(Color::DarkGray),
                    )));
                    for text_line in reply.text.lines() {
                        let chunks = wrap_text(text_line, content_width);
                        for chunk in chunks {
                            content_lines.push(Line::from(Span::styled(
                                format!("    {chunk}"),
                                Style::default().fg(Color::DarkGray),
                            )));
                        }
                    }
                    content_lines.push(Line::from(Span::raw("")));
                }

                if msg.media.is_none() {
                    for text_line in msg.text.lines() {
                        let chunks = wrap_text(text_line, content_width);
                        if chunks.is_empty() {
                            content_lines.push(Line::from(Span::raw("")));
                        } else {
                            for chunk in chunks {
                                content_lines.push(Line::from(Span::raw(chunk)));
                            }
                        }
                    }

                    if msg.text.is_empty() {
                        content_lines.push(Line::from(Span::raw("")));
                    }
                }

                let options_line = message_actions_line(msg);

                let total_lines = content_lines.len();
                let popup_height = (total_lines as u16 + 3)
                    .min(area.height.saturating_sub(4))
                    .max(5);
                let popup_area = popup_area(area, width, popup_height);

                Clear.render(popup_area, buf);

                let inner = popup_area.inner(Margin {
                    vertical: 1,
                    horizontal: 1,
                });

                let split = Layout::vertical([
                    Constraint::Fill(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                ])
                .split(inner);

                let viewport_height = split[0].height as usize;
                state.scroll_idx = state
                    .scroll_idx
                    .min(total_lines.saturating_sub(viewport_height));

                let paragraph = Paragraph::new(content_lines)
                    .scroll((state.scroll_idx as u16, 0))
                    .block(block);
                paragraph.render(popup_area, buf);

                let options_paragraph = Paragraph::new(options_line);
                options_paragraph.render(split[2], buf);
            }

            PopupKind::Image(ImagePopup { msg, view }) => {
                let max_width = area.width.saturating_sub(2);
                let width = ((area.width as u32 * 9 / 10) as u16)
                    .clamp(30.min(max_width), max_width.max(30));
                let max_height = area.height.saturating_sub(1);
                let height = ((area.height as u32 * 9 / 10) as u16)
                    .clamp(5.min(max_height), max_height.max(5));
                let popup_area = popup_area(area, width, height);

                Clear.render(popup_area, buf);

                let block = Block::bordered()
                    .title(format!(" {} {} ", msg.sender, format_timestamp(msg.timestamp)))
                    .border_style(Style::default().fg(Color::Cyan).bg(Color::Black));

                Widget::render(&block, popup_area, buf);

                let inner = popup_area.inner(Margin {
                    vertical: 1,
                    horizontal: 1,
                });

                let mut caption_lines: Vec<Line> = Vec::new();
                if let Some(caption) = msg
                    .media
                    .as_ref()
                    .and_then(|media| media.caption.clone())
                    .filter(|caption| !caption.trim().is_empty())
                {
                    for text_line in caption.lines() {
                        for chunk in wrap_text(text_line, inner.width as usize) {
                            caption_lines.push(Line::from(Span::raw(chunk)));
                        }
                    }
                }

                let split = Layout::vertical([
                    Constraint::Fill(1),
                    Constraint::Length(caption_lines.len() as u16),
                    Constraint::Length(1),
                ])
                .split(inner);

                ImageWidget.render(split[0], buf, view);

                if !caption_lines.is_empty() {
                    Paragraph::new(caption_lines).render(split[1], buf);
                }

                Paragraph::new(message_actions_line(msg)).render(split[2], buf);
            }
        }
    }
}

/// `[1] Reply [2] Edit ...` action bar shared by the Message and Image popups.
fn message_actions_line(msg: &Message) -> Line<'static> {
    let options_spans: Vec<Span> = msg
        .msg_actions
        .iter()
        .enumerate()
        .map(|(i, action)| {
            let label = match action {
                MessageAction::Reply => format!("[{}] Reply", i + 1),
                MessageAction::Edit => format!("[{}] Edit", i + 1),
                MessageAction::Delete => format!("[{}] Delete", i + 1),
                MessageAction::Retry => format!("[{}] Retry", i + 1),
            };
            Span::styled(format!("  {label}"), Style::default().fg(Color::White))
        })
        .collect();

    Line::from(options_spans)
}
