use anyhow::Result;
use ratatui::crossterm::ExecutableCommand;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
};
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::{StatefulWidget, Widget};
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::mpsc;
use tokio::time::{Duration, MissedTickBehavior};
use tracing::{debug, error, info};

use crate::backend::{AuthSteps, BackendEvent, ChatId, MessageAction, MessengerKind, Provider};
use crate::config::{Config, Keymap};
use crate::tui::chat::chat_list::ChatList;
use crate::tui::chat::chat_widget::ChatWidget;
use crate::tui::loading::{LoadingSpinner, LoadingWidget};
use crate::tui::popup::PopupKind;
use crate::tui::settings::Settings;
use crate::tui::state::{AppState, Focus, Screen};
use crate::tui::status_bar::StatusBarWidget;

mod chat;
mod loading;
mod login;
mod popup;
mod settings;
mod state;
mod status_bar;

enum UiEvent {
    Key(KeyEvent),
    Paste(String),
    Backend(Provider, BackendEvent),
    Resize(u16, u16),
    HistoryRefresh(ChatId, Vec<crate::backend::Message>),
    ChatsLoaded {
        chats: Vec<crate::backend::Chat>,
        errors: Vec<crate::backend::BackendError>,
    },
}

pub async fn run(
    config: Config,
    keymap: Keymap,
    messengers: Vec<MessengerKind>,
    open_settings: bool,
) -> Result<()> {
    let mut terminal = ratatui::init();
    // Let the terminal send pasted text as a single bracketed-paste event instead of a stream of
    // raw keys, so multi-line paste cannot trigger Enter=send mid-paste.
    std::io::stdout().execute(EnableBracketedPaste)?;
    let result = run_app(&mut terminal, config, keymap, messengers, open_settings).await;
    let _ = std::io::stdout().execute(DisableBracketedPaste);
    ratatui::restore();
    result
}

async fn run_app(
    terminal: &mut DefaultTerminal,
    config: Config,
    keymap: Keymap,
    messengers: Vec<MessengerKind>,
    open_settings: bool,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();
    spawn_terminal_reader(tx.clone());

    // handling new events for each messenger type
    for messenger in messengers.iter() {
        let mut backend_rx = messenger.subscribe();
        let forward_tx = tx.clone();
        let provider = messenger.provider();
        tokio::spawn(async move {
            while let Ok(event) = backend_rx.recv().await {
                if forward_tx.send(UiEvent::Backend(provider, event)).is_err() {
                    break;
                }
            }
        });
    }

    let mut app = App::new(config, keymap, messengers, open_settings).await;
    info!("TUI started");

    // Kick off the initial chat list fetch in the background so the UI renders
    // immediately (non-blocking startup). Results arrive via `ChatsLoaded`.
    let chat_loader_tx = tx.clone();
    let loader_messengers = app.state.messengers.clone();
    let loader_providers = app.state.config.providers.clone();

    tokio::spawn(async move {
        let (chats, errors) =
            crate::tui::state::fetch_all_chats(&loader_messengers, &loader_providers).await;
        let _ = chat_loader_tx.send(UiEvent::ChatsLoaded { chats, errors });
    });

    let mut chat_poll_interval = tokio::time::interval(Duration::from_secs(
        app.state.config.chat_poll_interval_secs,
    ));
    chat_poll_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut sidebar_sync_interval =
        tokio::time::interval(Duration::from_secs(app.state.config.sidebar_sync_secs));
    sidebar_sync_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        terminal.draw(|frame| app.draw(frame))?;

        tokio::select! {
            biased;

            _ = chat_poll_interval.tick() => {
                let chat_id = match app.state.chat_state.open_chat.as_ref() {
                    Some(open) => open.chat.id.clone(),
                    None => continue,
                };

            let messenger_idx = app.state.messengers.iter().position(|m| {
                m.provider() == chat_id.to_provider()
                    });

                if let Some(idx) = messenger_idx {
                    match app.state.messengers[idx].history(&chat_id).await {
                        Ok(history) => {
                            debug!(chat = ?chat_id, msgs = history.len(), "Poll refresh OK");
                            app.state.update_sidebar_from_poll(&chat_id, &history);
                            app.state.chat_state.refresh_chat_history(&chat_id, history);
                        }
                        Err(e) => {
                            error!(chat = ?chat_id, error = %e, "Poll refresh failed");
                        }
                    }
                }
            }

            _ = sidebar_sync_interval.tick() => {
                for messenger in app.state.messengers.iter() {
                    match messenger.chats().await {
                        Ok(chats) => {
                            for chat in &chats {
                                if let Some(entry) = app.state.chat_state.chats.iter_mut().find(|e| e.id == chat.id) {
                                    let changed = entry.unread != chat.unread
                                        || entry.unread_count != chat.unread_count;
                                    if changed {
                                        debug!(
                                            chat = ?chat.id,
                                            old_unread = entry.unread,
                                            new_unread = chat.unread,
                                            old_count = entry.unread_count,
                                            new_count = chat.unread_count,
                                            "Sidebar sync: unread changed"
                                        );
                                        entry.unread = chat.unread;
                                        entry.unread_count = chat.unread_count;
                                    }
                                    if let Some(ref lm) = chat.last_message {
                                        entry.last_message = Some(lm.clone());
                                    }
                                }
                            }
                            debug!(provider = ?messenger.provider(), chats = chats.len(), "Sidebar sync OK");
                        }
                        Err(e) => {
                            error!(provider = ?messenger.provider(), error = %e, "Sidebar sync failed");
                        }
                    }
                }
            }
            event = rx.recv() => {
                let Some(event) = event else {
                    break;
                };
                match event {
                    UiEvent::Key(key) => app.handle_key(key).await,
                    UiEvent::Paste(text) => app.state.handle_paste(text),
                    UiEvent::Backend(provider, backend_event) => {
                        debug!(
                            provider = ?provider,
                            event = ?std::mem::discriminant(&backend_event),
                            "TUI processing backend event, pending render"
                        );
                        app.state.handle_backend_event(provider, backend_event);
                    }
                    UiEvent::Resize(..) => {}
                    UiEvent::HistoryRefresh(chat_id, history) => {
                        debug!(
                            chat = ?chat_id,
                            messages = history.len(),
                            "HistoryRefresh applied"
                        );
                        app.state.update_sidebar_from_poll(&chat_id, &history);
                        app.state.chat_state.refresh_chat_history(&chat_id, history);
                    }
                    UiEvent::ChatsLoaded { chats, errors } => {
                        debug!(
                            chats = chats.len(),
                            errors = errors.len(),
                            "ChatsLoaded applied"
                        );
                        app.state.apply_fetched(chats, errors);
                    }
                }
                if !app.state.running {
                    break;
                }
            }
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
    loading_spinner: LoadingSpinner,
}

impl App {
    async fn new(
        config: Config,
        keymap: Keymap,
        messengers: Vec<MessengerKind>,
        open_settings: bool,
    ) -> Self {
        Self {
            state: AppState::new(config, keymap, messengers, open_settings).await,
            loading_spinner: LoadingSpinner::new(),
        }
    }

    async fn handle_key(&mut self, key: KeyEvent) {
        // TODO: add a overlay menu / screen to display all the keymappings
        // TODO: Add a graceful shutdown? all messages should be sent before shutting down the
        // app - verify if possible
        if key == self.state.keymap.quit {
            self.state.running = false;
            return;
        }

        if self.state.pop_up.is_some() {
            self.handle_popup_key(key);
            return;
        }

        match self.state.screen {
            Screen::Settings => self.handle_settings_key(key).await,
            Screen::Main => self.handle_main_key(key).await,
            Screen::Login => self.handle_login_key(key).await,
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
            if index + 1 < Provider::all().len() {
                self.state.settings_state.select(Some(index + 1));
            }
        } else if key == km.select {
            let index = self.state.settings_state.selected().unwrap_or(0);
            let Some(&provider) = Provider::all().get(index) else {
                return;
            };
            self.state.toggle_provider(provider).await;
        }
    }

    async fn handle_login_key(&mut self, key: KeyEvent) {
        let km = self.state.keymap.clone();
        if key == km.dismiss {
            self.state.cancel_login().await;
        } else if key == km.send {
            self.state.submit_login().await;
        } else if let Some(ref mut ls) = self.state.login_state {
            ls.login_input.input(key);
        }
    }

    async fn handle_main_key(&mut self, key: KeyEvent) {
        let km = self.state.keymap.clone();

        if key == km.dismiss {
            match self.state.focus {
                Focus::Write => {
                    self.state.cycle_focus();
                }
                Focus::Chat => {
                    self.state.chat_state.open_chat = None;
                    self.state.cycle_focus();
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

        if key == km.focus_write && self.state.focus != Focus::Write {
            match &self.state.chat_state.open_chat {
                Some(_) => {
                    self.state.focus = Focus::Write;
                    return;
                }
                None => match self.state.selected_chat_idx() {
                    Some(chat) => {
                        self.state.select_chat(chat).await;
                        self.state.focus = Focus::Write;
                        return;
                    }
                    None => {
                        return;
                    }
                },
            }
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
                    self.state.chat_state.message_list_state.select_previous();
                } else if key == km.scroll_down {
                    self.state.chat_state.message_list_state.select_next();
                } else if key == km.scroll_to_bottom {
                    let last = self
                        .state
                        .chat_state
                        .open_chat
                        .as_ref()
                        .map(|o| o.history.len().saturating_sub(1));
                    self.state.chat_state.message_list_state.select(last);
                } else if key.code == KeyCode::PageUp {
                    let page = self.state.chat_state.visible_page;
                    let current = self
                        .state
                        .chat_state
                        .message_list_state
                        .selected()
                        .unwrap_or(0);
                    let new = current.saturating_sub(page);
                    self.state.chat_state.message_list_state.select(Some(new));
                } else if key.code == KeyCode::PageDown {
                    // TODO: use scroll_up_by/ scroll_down_by here
                    let page = self.state.chat_state.visible_page;
                    let current = self
                        .state
                        .chat_state
                        .message_list_state
                        .selected()
                        .unwrap_or(0);
                    let new = current.saturating_add(page);
                    let max = self
                        .state
                        .chat_state
                        .open_chat
                        .as_ref()
                        .map(|o| o.history.len().saturating_sub(1))
                        .unwrap_or(0);
                    self.state
                        .chat_state
                        .message_list_state
                        .select(Some(new.min(max)));
                } else if key == km.select
                    && let Some(idx) = self.state.chat_state.message_list_state.selected()
                    && let Some(open) = &self.state.chat_state.open_chat
                    && let Some(msg) = open.history.get(idx)
                {
                    let mut msg = msg.clone();
                    msg.options = vec![MessageAction::Reply, MessageAction::Edit];
                    self.state.create_popup(PopupKind::Message(msg));
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
            Focus::Popup => {}
        }
    }

    fn handle_popup_key(&mut self, key: KeyEvent) {
        if key == self.state.keymap.dismiss {
            self.state.dismiss_popup();
        }
        let km = self.state.keymap.clone();
        let popup = match self.state.pop_up.as_mut() {
            Some(p) => p,
            None => {
                return;
            }
        };

        if key == km.scroll_up {
            popup.scroll_idx = popup.scroll_idx.saturating_sub(1);
        } else if key == km.scroll_down {
            popup.scroll_idx = popup.scroll_idx.saturating_add(1);
        }

        if let PopupKind::Message(msg) = &popup.popup_type
            && let KeyCode::Char(c) = key.code
            && let Some(digit) = c.to_digit(10)
        {
            let idx = (digit as usize).saturating_sub(1);
            if idx < msg.options.len() {
                // TODO: implement MessageAction actions
                match msg.options[idx] {
                    MessageAction::Reply => {
                        self.state
                            .create_popup(PopupKind::Info(String::from("reply")));
                    }
                    MessageAction::Edit => {
                        self.state
                            .create_popup(PopupKind::Info(String::from("edit")));
                    }
                }
                self.state.dismiss_popup();
            }
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        self.loading_spinner.tick();
        match self.state.screen {
            Screen::Main => self.draw_main(frame),
            Screen::Settings => {
                Settings::new(&self.state.config.providers).render(
                    frame.area(),
                    frame.buffer_mut(),
                    &mut self.state.settings_state,
                );
            }
            Screen::Login => {
                if let Some(login_state) = self.state.login_state.as_ref() {
                    let provider = login_state.provider;
                    let step = login_state.step;
                    if let Some(messenger) = self.state.provider_to_messenger(provider) {
                        let steps = messenger.login_steps();
                        let placeholder = messenger.login_placeholder(step);
                        let masked = steps.get(step) == Some(&AuthSteps::Password);

                        if let Some(ls) = self.state.login_state.as_mut() {
                            // BUG: this is not working: pressing enter to go to next
                            // step is not triggering a loading widget
                            if ls.submitting {
                                let step_label =
                                    steps.get(step).map(|s| s.to_string()).unwrap_or_default();
                                LoadingWidget::new(
                                    &format!(" {} — {} ", provider.name(), step_label),
                                    Some(format!("Waiting for {} ...", provider.name())),
                                    &mut self.loading_spinner,
                                )
                                .render(frame.area(), frame.buffer_mut());
                                return;
                            }

                            ls.login_input.set_placeholder_text(placeholder);

                            if masked {
                                ls.login_input.set_mask_char('●');
                            } else {
                                ls.login_input.clear_mask_char();
                            }

                            login::LoginScreen {
                                provider_name: provider.name(),
                                steps: &steps,
                            }
                            .render(
                                frame.area(),
                                frame.buffer_mut(),
                                ls,
                            );
                        }
                    }
                }
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

        if !self.state.chats_loaded && self.state.chat_state.chats.is_empty() {
            LoadingWidget::new(
                " Sender ",
                Some("Fetching chats...".to_string()),
                &mut self.loading_spinner,
            )
            .render(horizontal[0], frame.buffer_mut());
        }

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

        if let Some(open) = &self.state.chat_state.open_chat {
            debug!(
                chat = %open.chat.contact_name,
                history_len = open.history.len(),
                focus = ?self.state.focus,
                "TUI render complete"
            );
        }
    }
}
