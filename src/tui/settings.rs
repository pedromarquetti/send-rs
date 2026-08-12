use ratatui::prelude::*;
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, StatefulWidget, Widget};

use crate::config::ProvidersConfig;

pub struct Settings<'a> {
    providers: &'a ProvidersConfig,
}

impl<'a> Settings<'a> {
    pub fn new(providers: &'a ProvidersConfig) -> Self {
        Self { providers }
    }
}

impl StatefulWidget for Settings<'_> {
    type State = ListState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let layout = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).split(area);
        let block = Block::bordered()
            .title(" Settings ")
            .border_style(Style::default().fg(Color::Yellow));

        let providers = [
            ("Telegram", self.providers.telegram),
            ("WhatsApp", self.providers.whatsapp),
        ];
        let items: Vec<ListItem> = providers
            .iter()
            .map(|(name, enabled)| {
                let marker = if *enabled { "[x]" } else { "[ ]" };
                let status = if *enabled { "enabled" } else { "disabled" };
                ListItem::new(Line::from(Span::raw(format!(
                    "{marker} {name} — {status} (Enter toggles, connect is mocked)"
                ))))
            })
            .collect();
        let list = List::new(items)
            .block(block)
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        StatefulWidget::render(list, layout[0], buf, state);

        let hint = Paragraph::new(Span::styled(
            "j/k move  Enter toggle  Esc close",
            Style::default().fg(Color::DarkGray),
        ));
        Widget::render(hint, layout[1], buf);
    }
}
