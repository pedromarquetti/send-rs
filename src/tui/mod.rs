use anyhow::Result;
use ratatui::crossterm::ExecutableCommand;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::mpsc;

use crate::backend::{BackendEvent, Messenger};
use crate::config::{Config, Keymap};
use crate::tui::chat::ChatView;
use crate::tui::chat_list::ChatList;
use crate::tui::settings::Settings;
use crate::tui::state::{AppState, Focus, Screen};
use crate::tui::status_bar::StatusBarWidget;

mod chat;
mod chat_list;
mod overlay;
mod settings;
mod state;
mod status_bar;

enum UiEvent {
    Key(KeyEvent),
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
    // TODO: check this AI generated func.
    // this is supposed to fix Shift+Enter for newline not working, but uncommenting this breaks
    // everything
    //
    // enable_keyboard_enhancement();
    let result = run_app(&mut terminal, config, keymap, messengers, open_settings).await;
    // disable_keyboard_enhancement();
    ratatui::restore();
    result
}

/// Ask the terminal to report modifier keys (Shift+Enter etc.) distinctly.
fn enable_keyboard_enhancement() {
    let _ = std::io::stdout().execute(PushKeyboardEnhancementFlags(
        KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES,
    ));
}

fn disable_keyboard_enhancement() {
    let _ = std::io::stdout().execute(PopKeyboardEnhancementFlags);
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

        // A dialog/overlay keeps focus and captures all keys until dismissed.
        if self.state.overlay.is_some() {
            if key == self.state.keymap.dismiss {
                self.state.dismiss_overlay();
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
        } else if key == km.chat_list_up {
            let index = self
                .state
                .settings_state
                .selected()
                .unwrap_or(0)
                .saturating_sub(1);
            self.state.settings_state.select(Some(index));
        } else if key == km.chat_list_down {
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

        if key == km.focus_write && self.state.selected_chat().is_some() {
            self.state.focus = Focus::Write;
            return;
        }

        match self.state.focus {
            Focus::ChatList => {
                if key == km.chat_list_up {
                    self.state.chat_list_state.select_previous();
                } else if key == km.chat_list_down {
                    self.state.chat_list_state.select_next();
                } else if key == km.select && !self.state.chats.is_empty() {
                    match self.state.selected_chat() {
                        Some(chat) => {
                            self.state.select_chat(chat).await;
                        }
                        None => {
                            self.state.show_error("No chat selected");
                        }
                    };
                }
            }
            Focus::Chat => {
                if key == km.history_up {
                    self.state.chat_view_state.scroll =
                        self.state.chat_view_state.scroll.saturating_add(1);
                } else if key == km.history_down {
                    self.state.chat_view_state.scroll =
                        self.state.chat_view_state.scroll.saturating_sub(1);
                } else if key.code == KeyCode::PageUp {
                    self.state.chat_view_state.scroll = self
                        .state
                        .chat_view_state
                        .scroll
                        .saturating_add(self.state.chat_view_state.page);
                } else if key.code == KeyCode::PageDown {
                    self.state.chat_view_state.scroll = self
                        .state
                        .chat_view_state
                        .scroll
                        .saturating_sub(self.state.chat_view_state.page);
                }
            }
            Focus::Write => {
                if key == km.send {
                    self.state.send_message().await;
                } else if key == km.newline {
                    self.state.write.insert_newline();
                } else {
                    self.state.write.input(key);
                }
            }
            Focus::Overlay => {}
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        if let Some(overlay) = &self.state.overlay {
            overlay::render(frame, overlay);
        }
        match self.state.screen {
            Screen::Main => self.draw_main(frame),
            Screen::Settings => self.draw_settings(frame),
        }
    }

    fn draw_main(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let vertical = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).split(area);
        let horizontal =
            Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)])
                .split(vertical[0]);
        self.draw_chat_list(frame, horizontal[0]);
        self.draw_chat_view(frame, horizontal[1]);
        let current = self
            .state
            .selected_chat()
            .and_then(|index| self.state.chats.get(index))
            .map(|chat| chat.name.clone());
        frame.render_widget(StatusBarWidget::new(current, self.state.focus), vertical[1]);
    }

    fn draw_chat_list(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.state.focus == Focus::ChatList;
        let widget = ChatList::new(&self.state.chats, &self.state.chat_tags, focused);
        frame.render_stateful_widget(widget, area, &mut self.state.chat_list_state);
    }

    fn draw_chat_view(&mut self, frame: &mut Frame, area: Rect) {
        let chat_focused = matches!(self.state.focus, Focus::Chat | Focus::Write);
        let write_focused = self.state.focus == Focus::Write;

        let selected = self.state.selected_chat();
        let opened = selected
            .and_then(|index| self.state.chats.get(index))
            .map(|chat| {
                self.state
                    .history_chat
                    .as_ref()
                    .map(|c| c == &chat.id)
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        let title = selected
            .and_then(|index| self.state.chats.get(index))
            .map(|chat| chat.name.clone())
            .unwrap_or_else(|| "No chat selected".into());
        let hint = if selected.is_some() && !opened {
            Some("Press Enter to open this chat")
        } else if selected.is_none() {
            Some("No chat selected. j/k move, Enter open.")
        } else {
            None
        };
        let history = if opened {
            self.state.history.as_slice()
        } else {
            &[]
        };

        let widget = ChatView::new(
            history,
            &title,
            hint,
            chat_focused,
            write_focused,
            &mut self.state.write,
        );
        let view_state = &mut self.state.chat_view_state;
        frame.render_stateful_widget(widget, area, view_state);
    }

    fn draw_settings(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let widget = Settings::new(&self.state.config.providers);
        frame.render_stateful_widget(widget, area, &mut self.state.settings_state);
    }
}
