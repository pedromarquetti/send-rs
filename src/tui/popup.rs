use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::{prelude::*, widgets};
use std::fmt::Debug;
use std::time::Instant;

use crate::backend::{Message, MessageAction};
use crate::helpers::{calc_height, popup_area, wrap_text};
use crate::tui::chat::chat_widget::format_timestamp;
use crate::tui::image::{ImageWidget, ImageWidgetState, VideoWidget, VideoWidgetState};
use crate::tui::player::{PlayState, PlaybackState};
use crate::tui::state::PopupState;

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum PopupKind {
    Info(String),
    Error(String),
    Warn(String),
    Message(Message),
    Image(ImagePopup),
    Audio(AudioPopup),
    Video(VideoPopup),
    Question(String),
}

pub struct ImagePopup {
    pub msg: Message,
    pub view: ImageWidgetState,
}

impl Debug for ImagePopup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImagePopup")
            .field("msg", &self.msg)
            .finish_non_exhaustive()
    }
}

/// Audio playback popup. The live session lives in `AppState::playback`; this
/// field is a per-frame mirror refreshed at draw time so the widget has
/// something to render (dropped with the popup on dismiss).
pub struct AudioPopup {
    pub msg: Message,
    pub playback: Option<PlaybackState>,
    /// A human-readable, non-fatal error (e.g. a failed on-demand media
    /// download) rendered in the popup's bottom border instead of a bare ⚠.
    /// Only surfaces errors this process produced itself — native alsa-lib
    /// diagnostics never reach here (they go to the log via stderr redirect).
    pub error_note: Option<String>,
}

impl Debug for AudioPopup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioPopup")
            .field("msg", &self.msg)
            .field("playback", &self.playback)
            .field("error_note", &self.error_note)
            .finish()
    }
}

/// Video playback popup. `view` is the frame viewport fed by the engine's
/// latest-frame slot; `playback` is the per-frame mirror of the live session,
/// exactly as in [`AudioPopup`].
pub struct VideoPopup {
    pub msg: Message,
    pub view: VideoWidgetState,
    pub playback: Option<PlaybackState>,
    /// As in [`AudioPopup`]: no ffmpeg on the system, an undecodable
    /// container, a download that never arrived.
    pub error_note: Option<String>,
}

// `VideoWidgetState` is not `Debug`, hence the manual impl rather than a
// derive, mirroring `ImagePopup`/`AudioPopup`.
impl Debug for VideoPopup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoPopup")
            .field("msg", &self.msg)
            .field("playback", &self.playback)
            .field("error_note", &self.error_note)
            .finish()
    }
}

impl Default for PopupKind {
    fn default() -> Self {
        Self::Info(String::new())
    }
}

#[derive(Default)]
pub struct PopUp {
    msgr_tag: &'static str,
}

impl PopUp {
    pub fn new(msgr_tag: &'static str) -> Self {
        Self { msgr_tag }
    }

    pub fn handle_data_render(
        &self,
        data: &String,
        area: Rect,
        buf: &mut Buffer,
        block: Block,
        text_style: Style,
    ) {
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
        let paragraph = Paragraph::new(lines).block(block).style(text_style);
        Clear.render(popup_area, buf);
        Widget::render(paragraph, popup_area, buf);
    }
}

impl StatefulWidget for &mut PopUp {
    type State = PopupState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let width = 80.min(area.width.saturating_sub(4));
        let content_width = width.saturating_sub(2) as usize;

        let text_style = {
            if !state.focused {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            }
        };
        let border_style = {
            if !state.focused {
                Style::default().fg(Color::DarkGray)
            } else {
                match self.msgr_tag {
                    "TG" => Style::default().fg(Color::Blue),
                    "WA" => Style::default().fg(Color::Green),
                    _ => Style::default(),
                }
            }
        };

        match &mut state.popup_type {
            PopupKind::Question(data) => {
                let total_lines = data.lines().count();
                let popup_height = (total_lines as u16 + 3)
                    .min(area.height.saturating_sub(4))
                    .max(5);
                let popup_area = popup_area(area, width, popup_height);

                Clear.render(popup_area, buf);

                let block = Block::bordered().title(" ? ").border_style(border_style);

                let options: Vec<Span> = vec![Span::from(" [1] Yes "), Span::from(" [2] No ")];

                Paragraph::new(format!("{data}\n\n{}", Line::from(options)))
                    .style(text_style)
                    .block(block.clone())
                    .render(popup_area, buf);
            }

            PopupKind::Error(data) => {
                let block = Block::bordered()
                    .title(" Error ")
                    .border_style(Style::default().fg(Color::Red).bg(Color::Black));
                self.handle_data_render(data, area, buf, block, text_style);
            }

            PopupKind::Info(data) => {
                let block = Block::bordered()
                    .title(" Info ")
                    .border_style(Style::default().fg(Color::LightBlue).bg(Color::Black));
                self.handle_data_render(data, area, buf, block, text_style);
            }

            PopupKind::Warn(data) => {
                let block = Block::bordered()
                    .title(" Warning ")
                    .border_style(Style::default().fg(Color::Yellow).bg(Color::Black));
                self.handle_data_render(data, area, buf, block, text_style);
            }

            PopupKind::Message(msg) => {
                let block = Block::bordered()
                    .style(text_style)
                    .title(" Message ")
                    .border_style(border_style);

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
                    .style(text_style)
                    .scroll((state.scroll_idx as u16, 0))
                    .block(block);
                paragraph.render(popup_area, buf);

                let options_paragraph = Paragraph::new(options_line).style(text_style);
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
                    .style(text_style)
                    .title(format!(
                        " {} {} ",
                        msg.sender,
                        format_timestamp(msg.timestamp)
                    ))
                    .border_style(border_style);

                Widget::render(&block, popup_area, buf);

                let inner = popup_area.inner(Margin {
                    vertical: 1,
                    horizontal: 1,
                });

                let split_vert =
                    Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).split(inner);

                let split_hor =
                    Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .split(split_vert[0]);

                ImageWidget.render(split_hor[0], buf, view);

                if let Some(caption) = msg
                    .media
                    .as_ref()
                    .and_then(|media| media.caption.clone())
                    .filter(|caption| !caption.trim().is_empty())
                {
                    Paragraph::new(caption)
                        .scroll((state.scroll_idx as u16, 0))
                        .wrap(widgets::Wrap { trim: false })
                        .render(split_hor[1], buf);
                }

                Paragraph::new(message_actions_line(msg)).render(split_vert[1], buf);
            }

            PopupKind::Audio(AudioPopup {
                msg,
                playback,
                error_note,
            }) => {
                let block = Block::bordered()
                    .style(text_style)
                    .title(format!(
                        " {} {} ",
                        msg.sender,
                        format_timestamp(msg.timestamp)
                    ))
                    .border_style(border_style);

                let block = match error_note {
                    Some(note) => block.title_bottom(format!(" ⚠ {note} ")),
                    None => block,
                };

                let mut content_lines: Vec<Line> = Vec::new();

                let (status_icon, elapsed, duration, ratio) = playback_status(playback.as_ref());

                let progress = progress_strip(ratio, content_width);

                let duration_label = if duration > 0.0 {
                    format_duration(duration)
                } else {
                    "--:--".to_string()
                };

                content_lines.push(Line::from(Span::styled(
                    format!(
                        "  {status_icon} Audio · {} / {duration_label}",
                        format_duration(elapsed)
                    ),
                    border_style.add_modifier(Modifier::BOLD),
                )));

                content_lines.push(Line::from(Span::styled(progress, border_style)));

                let options_line = message_actions_line(msg);

                let total_lines = content_lines.len();
                let popup_height = (total_lines as u16 * 6).min(area.height);
                let popup_area = popup_area(area, width, popup_height);

                Clear.render(popup_area, buf);

                let inner = popup_area.inner(Margin {
                    vertical: 1,
                    horizontal: 1,
                });

                let split_vert = Layout::vertical([
                    Constraint::Fill(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                ])
                .split(inner);

                let split_hor =
                    Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .split(split_vert[0]);

                if let Some(caption) = msg
                    .media
                    .as_ref()
                    .and_then(|media| media.caption.clone())
                    .filter(|caption| !caption.trim().is_empty())
                {
                    Paragraph::new(caption)
                        .scroll((state.scroll_idx as u16, 0))
                        .wrap(widgets::Wrap { trim: false })
                        .render(split_hor[1], buf);
                }

                Paragraph::new(content_lines).render(split_hor[0], buf);

                block.render(popup_area, buf);

                let hint = Line::from(Span::styled(
                    "  space ▸ play/pause   < ▸ -5s   > ▸ +5s   esc ▸ close".to_string(),
                    Style::default().fg(Color::DarkGray),
                ));

                Paragraph::new(hint)
                    .style(text_style)
                    .render(split_vert[1], buf);
                Paragraph::new(options_line)
                    .style(text_style)
                    .render(split_vert[2], buf);
            }

            // <video> <caption>
            // <bottom content such as progress bar and actions>
            // TODO: check video fitting and rendering here - landscape video is not being redered
            // in landscape orientation
            PopupKind::Video(VideoPopup {
                msg,
                view,
                playback,
                error_note,
            }) => {
                let max_width = area.width.saturating_sub(2);
                let width = ((area.width as u32 * 9 / 10) as u16)
                    .clamp(30.min(max_width), max_width.max(30));

                let mut block = Block::bordered()
                    .style(text_style)
                    .title(format!(
                        " {} {} ",
                        msg.sender,
                        format_timestamp(msg.timestamp)
                    ))
                    .border_style(border_style);

                block = match error_note {
                    Some(note) => block.title_bottom(format!(" ⚠ {note} ")),
                    None => block,
                };

                let max_height = ((area.height as u32 * 9 / 10) as u16)
                    .min(area.height.saturating_sub(1))
                    .max(8);

                let popup_area = popup_area(area, width, max_height);

                Clear.render(popup_area, buf);
                Widget::render(&block, popup_area, buf);

                let inner = popup_area.inner(Margin {
                    vertical: 1,
                    horizontal: 1,
                });

                let split_vert = Layout::vertical([
                    Constraint::Fill(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                ])
                .split(inner);

                let split_hor =
                    Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .split(split_vert[0]);

                VideoWidget.render(split_hor[0], buf, view);

                let (status_icon, elapsed, duration, ratio) = playback_status(playback.as_ref());

                let duration_label = if duration > 0.0 {
                    format_duration(duration)
                } else {
                    "--:--".to_string()
                };

                Paragraph::new(Line::from(Span::styled(
                    format!(
                        "  {status_icon} Video · {} / {duration_label}",
                        format_duration(elapsed)
                    ),
                    border_style.add_modifier(Modifier::BOLD),
                )))
                .render(split_vert[1], buf);

                Paragraph::new(progress_strip(ratio, width.saturating_sub(2) as usize))
                    .render(split_vert[2], buf);

                if let Some(caption_line) = msg
                    .media
                    .as_ref()
                    .and_then(|media| media.caption.clone())
                    .filter(|caption| !caption.trim().is_empty())
                {
                    Paragraph::new(caption_line)
                        .scroll((state.scroll_idx as u16, 0))
                        .wrap(widgets::Wrap { trim: false })
                        .render(split_hor[1], buf);
                }

                Paragraph::new(message_actions_line(msg))
                    .style(text_style)
                    // .render(split[4], buf);
                    .render(split_vert[3], buf);

                Paragraph::new(Line::from(Span::styled(
                    "  space ▸ play/pause   < ▸ -5s   > ▸ +5s   esc ▸ close".to_string(),
                    Style::default().fg(Color::DarkGray),
                )))
                // .render(split[5], buf);
                .render(split_vert[4], buf);
            }
        }
    }
}

/// Transport icon, displayed elapsed seconds, duration and progress ratio for
/// a playback session. Shared by the audio and video popups so the two status
/// lines cannot drift apart.
fn playback_status(playback: Option<&PlaybackState>) -> (&'static str, f64, f64, f64) {
    match playback {
        Some(p) if p.status == PlayState::Error => ("⚠", p.position, p.duration, 0.0),
        Some(p) => {
            let pos = p.display_position(Instant::now());
            let ratio = if p.duration > 0.0 {
                (pos / p.duration).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let icon = match p.status {
                PlayState::Playing => "▶",
                PlayState::Paused | PlayState::Stopped => "⏸",
                PlayState::Loading => "…",
                PlayState::Error => "⚠",
            };
            (icon, pos, p.duration, ratio)
        }
        None => ("…", 0.0, 0.0, 0.0),
    }
}

fn format_duration(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    format!("{:02}:{:02}", total / 60, total % 60)
}

/// One-row progress strip, e.g. `███████░░░` with the label on the right.
fn progress_strip(ratio: f64, width: usize) -> String {
    let bar_width = width.saturating_sub(1);
    let filled = (bar_width as f64 * ratio).round() as usize;
    let empty = bar_width.saturating_sub(filled);
    format!("  {}{}", "█".repeat(filled), "░".repeat(empty))
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
                MessageAction::Copy => format!("[{}] Copy", i + 1),
            };
            Span::from(format!("  {label}"))
        })
        .collect();

    Line::from(options_spans)
}
