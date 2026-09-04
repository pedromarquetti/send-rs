use ratatui::layout::Flex;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Paragraph, StatefulWidget, Widget, Wrap};

use crate::backend::AuthSteps;
use crate::helpers::render_qr;
use crate::tui::state::LoginState;

pub struct LoginScreen<'a> {
    pub provider_name: &'a str,
    pub steps: &'a Vec<AuthSteps>,
}

// TODO: add loading text for waiting. Currently, logging-in just hangs with no display on what is
// happening - The user should know the app didn't freeze
impl StatefulWidget for LoginScreen<'_> {
    type State = LoginState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let step = &self.steps[state.step];
        let title = format!(" {} — {} ", self.provider_name, step);

        let block = Block::bordered()
            .title(Span::styled(
                title,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))
            .title_bottom(
                // Hint
                Line::from(Span::styled(
                    " Enter submit  Esc cancel ",
                    Style::default().fg(Color::DarkGray),
                )),
            )
            .border_style(Style::default().fg(Color::Cyan));

        // Step progress indicator
        let progress: Vec<Span> = self
            .steps
            .iter()
            .enumerate()
            .flat_map(|(i, name)| {
                let mut spans = vec![];
                if i > 0 {
                    spans.push(Span::raw(" → "));
                }
                let style = if i < state.step {
                    Style::default().fg(Color::Green) // completed
                } else if i == state.step {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD) // current
                } else {
                    Style::default().fg(Color::DarkGray) // upcoming
                };
                spans.push(Span::styled(name.to_string(), style));
                spans
            })
            .collect();

        let progress_line = Line::from(progress);

        // Error message
        let error_line = if let Some(ref err) = state.error {
            Line::from(Span::styled(
                format!("An error occurred: {err}"),
                Style::default().fg(Color::Red),
            ))
        } else {
            Line::from("")
        };

        // WhatsApp QR code block
        if let AuthSteps::QrCode(qr) = step {
            let qr_art = render_qr(qr);
            let qr_height = qr_art.lines().count().max(1) as u16;
            let qr_width = qr_art
                .lines()
                .map(|l| l.chars().count())
                .max()
                .unwrap_or(0)
                .max(1) as u16;

            // Reserve the progress line, then center the QR within the rest of
            // the box. Sizing to the QR's exact content height means the code
            // can never overflow the block vertically on short terminals.
            let [progress_area, body] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(0)])
                    .areas(block.inner(area));

            let progress_widget = Paragraph::new(progress_line);
            progress_widget.render(progress_area, buf);

            let [qr_area] = Layout::vertical([Constraint::Length(qr_height)])
                .flex(Flex::Center)
                .areas(body);
            let [qr_area] = Layout::horizontal([Constraint::Length(qr_width)])
                .flex(Flex::Center)
                .areas(qr_area);

            Paragraph::new(qr_art).render(qr_area, buf);

            Widget::render(block, area, buf);
            return;
        }

        let split = Layout::vertical([
            Constraint::Length(1), // progress
            Constraint::Length(1), // blank
            Constraint::Length(1), // input label
            Constraint::Length(4), // text area
            Constraint::Length(1), // blank
            Constraint::Min(1),    // error (wraps, fills remaining space)
            Constraint::Length(1), // hint
        ])
        .split(block.inner(area));

        let progress_widget = Paragraph::new(progress_line);
        progress_widget.render(split[0], buf);

        let label = Paragraph::new(Line::from(Span::styled(
            format!("{step}:"),
            Style::default().fg(Color::White),
        )));
        label.render(split[2], buf);

        // Render the textarea inside a bordered block
        let input_block = Block::bordered().border_style(Style::default().fg(Color::Yellow));
        state.login_input.set_block(input_block);
        let input_area = Rect {
            x: split[3].x,
            y: split[3].y,
            width: split[3].width,
            height: 3,
        };
        Widget::render(&state.login_input, input_area, buf);

        let error_widget = Paragraph::new(error_line).wrap(Wrap { trim: false });
        error_widget.render(split[5], buf);

        // Render the outer block last so borders draw over content
        Widget::render(block, area, buf);
    }
}
