use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use std::time::Duration;
use tokio::sync::mpsc;

enum UiEvent {
    Key(KeyEvent),
    Ticker(String),
}

pub async fn run() -> Result<()> {
    let mut terminal = ratatui::init();
    let result = run_app(&mut terminal).await;
    ratatui::restore();
    result
}

async fn run_app(terminal: &mut DefaultTerminal) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();
    spawn_terminal_reader(tx.clone());
    tokio::spawn(async move {
        demo_ticker(tx).await;
    });

    let mut app = App::default();
    loop {
        let event = match rx.recv().await {
            Some(event) => event,
            None => break,
        };
        match event {
            UiEvent::Key(key) if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') => break,
            UiEvent::Key(key) if key.code == KeyCode::Char('q') => break,
            UiEvent::Key(_) => {}
            UiEvent::Ticker(text) => app.last_tick = text,
        }
        terminal.draw(|frame| app.draw(frame))?;
    }
    Ok(())
}

fn spawn_terminal_reader(tx: mpsc::UnboundedSender<UiEvent>) {
    std::thread::spawn(move || loop {
        match event::read() {
            Ok(Event::Key(key)) => {
                if tx.send(UiEvent::Key(key)).is_err() {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    });
}

// TODO: remove demo code
async fn demo_ticker(tx: mpsc::UnboundedSender<UiEvent>) {
    let mut tick = 0u64;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        tick += 1;
        if tx
            .send(UiEvent::Ticker(format!("tokio heartbeat #{tick}")))
            .is_err()
        {
            return;
        }
    }
}

#[derive(Default)]
struct App {
    last_tick: String,
}

impl App {
    fn draw(&self, frame: &mut Frame) {
        let vertical = Layout::vertical([Constraint::Fill(1), Constraint::Length(3)]).split(frame.area());
        let horizontal = Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(vertical[0]);

        let dialogs = Block::bordered().title(" Chats ");
        // TODO: The current chat should be the contact name/phone number
        let chat = Block::bordered().title(" Selected chat ");

        let status = Paragraph::new(Line::from(vec![
            Span::raw(self.last_tick.clone()),
            Span::raw("   "),
            Span::styled("q: quit", Style::default().add_modifier(Modifier::DIM)),
        ]))
        .block(Block::bordered().title(" Status "));

        frame.render_widget(dialogs, horizontal[0]);
        frame.render_widget(chat, horizontal[1]);
        frame.render_widget(status, vertical[1]);
    }
}
