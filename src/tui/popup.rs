use ratatui::prelude::*;
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::backend::{Message, MessageAction};
use crate::helpers::{calc_height, popup_area};
use crate::tui::chat::chat_widget::{format_timestamp, wrap_text};
use crate::tui::state::PopupState;

#[derive(Debug)]
pub enum PopupKind {
    Info(String),
    Error(String),
    Warn(String),
    Message(Message),
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
        match &state.popup_type {
            PopupKind::Error(data) => {
                let block = Block::bordered()
                    .title("Error")
                    .border_style(Style::default().fg(Color::Red).bg(Color::Black));
                self.handle_data_render(data, area, buf, block);
            }
            PopupKind::Info(data) => {
                let block = Block::bordered()
                    .title("Info")
                    .border_style(Style::default().fg(Color::Blue).bg(Color::Black));
                self.handle_data_render(data, area, buf, block);
            }
            PopupKind::Warn(data) => {
                let block = Block::bordered()
                    .title("Warning")
                    .border_style(Style::default().fg(Color::Yellow).bg(Color::Black));
                self.handle_data_render(data, area, buf, block);
            }
            PopupKind::Message(msg) => {
                let block = Block::bordered()
                    .title(" Message ")
                    .border_style(Style::default().fg(Color::Cyan).bg(Color::Black));

                let width = 80.min(area.width.saturating_sub(4));
                let content_width = width.saturating_sub(2) as usize; // inner width minus border

                let mut content_lines: Vec<Line> = Vec::new();
                let head = format!("{} {}", msg.sender, format_timestamp(msg.timestamp));
                content_lines.push(Line::from(Span::styled(
                    head,
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                )));
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

                let options_spans: Vec<Span> = msg
                    .options
                    .iter()
                    .enumerate()
                    .map(|(i, opt)| {
                        let label = match opt {
                            MessageAction::Reply => format!("[{}] Reply", i + 1),
                            MessageAction::Edit => format!("[{}] Edit", i + 1),
                        };
                        Span::styled(format!("  {label}"), Style::default().fg(Color::White))
                    })
                    .collect();
                let options_line = Line::from(options_spans);

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
        }
    }
}
