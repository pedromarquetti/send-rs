use anyhow::Result;
use ratatui::crossterm::ExecutableCommand;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
};
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::{StatefulWidget, Widget};
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::mpsc;

use crate::backend::{BackendEvent, Messenger};
use crate::config::{Config, Keymap};
use crate::tui::chat::chat_list::ChatList;
use crate::tui::chat::chat_widget::ChatWidget;
use crate::tui::popup::PopupKind;
use crate::tui::settings::Settings;
use crate::tui::state::{AppState, Focus, Screen};
use crate::tui::status_bar::StatusBarWidget;

mod chat;
mod popup;
mod settings;
mod state;
mod status_bar;

enum UiEvent {
    Key(KeyEvent),
    Paste(String),
    Backend(usize, BackendEvent),
    Resize(u16, u16),
}

pub async fn run(
    config: Config,
    keymap: Keymap,
    messengers: Vec<Box<dyn Messenger>>,
    open_settings: bool,
) -> Result<()> {
    let mut terminal = ratatui::init();
    // Let the terminal send pasted text as a single bracketed-paste event instead of a stream of
    // raw keys, so multi-line paste cannot trigger Enter=send mid-paste.
    let _ = std::io::stdout().execute(EnableBracketedPaste)?;
    let result = run_app(&mut terminal, config, keymap, messengers, open_settings).await;
    let _ = std::io::stdout().execute(DisableBracketedPaste)?;
    ratatui::restore();
    result
}

async fn run_app(
    terminal: &mut DefaultTerminal,
    config: Config,
    keymap: Keymap,
    messengers: Vec<Box<dyn Messenger>>,
    open_settings: bool,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();
    spawn_terminal_reader(tx.clone());

    // handling new events for each messenger type
    for (index, messenger) in messengers.iter().enumerate() {
        let mut backend_rx = messenger.subscribe();
        let forward_tx = tx.clone();
        tokio::spawn(async move {
            while let Ok(event) = backend_rx.recv().await {
                if forward_tx.send(UiEvent::Backend(index, event)).is_err() {
                    break;
                }
            }
        });
    }

    let mut app = App::new(config, keymap, messengers, open_settings).await?;
    loop {
        terminal.draw(|frame| app.draw(frame))?;
        let Some(event) = rx.recv().await else {
            break;
        };
        match event {
            UiEvent::Key(key) => app.handle_key(key).await,
            UiEvent::Paste(text) => app.state.handle_paste(text),
            UiEvent::Backend(index, backend_event) => {
                app.state.handle_backend_event(index, backend_event);
            }
            UiEvent::Resize(..) => {}
        }
        if !app.state.running {
            break;
        }
    }
    Ok(())
}

fn spawn_terminal_reader(tx: mpsc::UnboundedSender<UiEvent>) {
    std::thread::spawn(move || {
        loop {
            match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    if tx.send(UiEvent::Key(key)).is_err() {
                        break;
                    }
                }
                Ok(Event::Paste(text)) => {
                    if tx.send(UiEvent::Paste(text)).is_err() {
                        break;
                    }
                }
                Ok(Event::Resize(width, height)) => {
                    if tx.send(UiEvent::Resize(width, height)).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });
}

struct App {
    state: AppState,
}

impl App {
    async fn new(
        config: Config,
        keymap: Keymap,
        messengers: Vec<Box<dyn Messenger>>,
        open_settings: bool,
    ) -> Result<Self> {
        Ok(Self {
            state: AppState::new(config, keymap, messengers, open_settings).await?,
        })
    }

    async fn handle_key(&mut self, key: KeyEvent) {
        if key == self.state.keymap.quit {
            self.state.running = false;
            return;
        }

        // Disable key detection (except Esc) on Popup mode
        if self.state.pop_up.is_some() {
            if key == self.state.keymap.dismiss {
                self.state.dismiss_popup();
            }
            return;
        }

        match self.state.screen {
            Screen::Settings => self.handle_settings_key(key).await,
            Screen::Main => self.handle_main_key(key).await,
        }
    }

    async fn handle_settings_key(&mut self, key: KeyEvent) {
        let km = self.state.keymap.clone();
        if key == km.dismiss || key == km.open_settings {
            self.state.screen = Screen::Main;
        } else if key == km.scroll_up {
            let index = self
                .state
                .settings_state
                .selected()
                .unwrap_or(0)
                .saturating_sub(1);
            self.state.settings_state.select(Some(index));
        } else if key == km.scroll_down {
            let index = self.state.settings_state.selected().unwrap_or(0);
            if index + 1 < 2 {
                self.state.settings_state.select(Some(index + 1));
            }
        } else if key == km.select {
            let index = self.state.settings_state.selected().unwrap_or(0);
            self.state.toggle_provider(index).await;
        }
    }

    async fn handle_main_key(&mut self, key: KeyEvent) {
        let km = self.state.keymap.clone();

        if key == km.dismiss {
            match self.state.focus {
                Focus::Write => {
                    self.state.focus = Focus::Chat;
                }
                Focus::Chat => {
                    self.state.chat_state.open_chat = None;
                    self.state.focus = Focus::ChatList;
                }
                _ => {}
            }
            return;
        }

        if key == km.open_settings && self.state.focus != Focus::Write {
            self.state.screen = Screen::Settings;
            return;
        }

        if key == km.pane_next {
            self.state.cycle_focus();
            return;
        }

        if key == km.focus_write
            && self.state.chat_state.open_chat.is_some()
            && self.state.focus != Focus::Write
        {
            self.state.focus = Focus::Write;
            return;
        }

        match self.state.focus {
            Focus::ChatList => {
                if key == km.scroll_up {
                    self.state.chat_state.chat_list_state.select_previous();
                } else if key == km.scroll_down {
                    self.state.chat_state.chat_list_state.select_next();
                } else if key == km.select
                    && !self.state.chat_state.chats.is_empty()
                    && self.state.chat_state.chat_list_state.selected().is_some()
                {
                    match self.state.selected_chat_idx() {
                        Some(chat) => {
                            self.state.select_chat(chat).await;
                        }
                        None => {
                            self.state
                                .create_popup(PopupKind::Error(String::from("No chat selected")));
                        }
                    };
                }
            }
            Focus::Chat => {
                if key == km.scroll_up {
                    if let Some(chat) = self.state.chat_state.selected_chat_mut() {
                        chat.scroll = chat.scroll.saturating_add(1);
                    }
                } else if key == km.scroll_down {
                    if let Some(chat) = self.state.chat_state.selected_chat_mut() {
                        chat.scroll = chat.scroll.saturating_sub(1);
                    }
                } else if key == km.scroll_to_bottom {
                    if let Some(chat) = self.state.chat_state.selected_chat_mut() {
                        chat.scroll = 0;
                    }
                } else if key.code == KeyCode::PageUp {
                    let page = self.state.chat_state.visible_page;
                    if let Some(chat) = self.state.chat_state.selected_chat_mut() {
                        chat.scroll = chat.scroll.saturating_add(page);
                    }
                } else if key.code == KeyCode::PageDown {
                    let page = self.state.chat_state.visible_page;
                    if let Some(chat) = self.state.chat_state.selected_chat_mut() {
                        chat.scroll = chat.scroll.saturating_sub(page);
                    }
                }
            }
            Focus::Write => {
                if key.kind != KeyEventKind::Press {
                    // Paste-sourced keys are never send/newline actions.
                    self.state.write.input(key);
                } else if key == km.send {
                    self.state.send_message().await;
                } else if key == km.newline || key.code == KeyCode::Enter {
                    // A configured newline key (Shift+Enter by default) inserts a newline.
                    // Any Enter with a modifier (Alt/Ctrl) also inserts a newline, since many
                    // terminals cannot report modifiers on Enter and treat it as plain Enter.
                    self.state.write.insert_newline();
                } else {
                    self.state.write.input(key);
                }
            }
            Focus::Overlay => {}
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        match self.state.screen {
            Screen::Main => self.draw_main(frame),
            Screen::Settings => {
                Settings::new(&self.state.config.providers).render(
                    frame.area(),
                    frame.buffer_mut(),
                    &mut self.state.settings_state,
                );
            }
        }

        // add this here to the popup actually clears the content below it
        if let Some(state) = &mut self.state.pop_up {
            popup::PopUp::new().render(frame.area(), frame.buffer_mut(), state);
        }
    }

    fn draw_main(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let vertical = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).split(area);
        let horizontal =
            Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)])
                .split(vertical[0]);

        ChatList::new(
            self.state.chat_state.get_tag(),
            &self.state.chat_state.chats,
            self.state.focus,
        )
        .render(
            horizontal[0],
            frame.buffer_mut(),
            &mut self.state.chat_state.chat_list_state,
        );

        ChatWidget::new(
            self.state.focus,
            &mut self.state.write,
            self.state.config.max_write_lines,
        )
        .render(
            horizontal[1],
            frame.buffer_mut(),
            &mut self.state.chat_state,
        );

        let name = self
            .state
            .chat_state
            .selected_chat()
            .map(|c| c.contact_name.clone());
        StatusBarWidget::new(name, self.state.focus).render(vertical[1], frame.buffer_mut());
    }
}
