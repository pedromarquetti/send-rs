use anyhow::Result;
use ratatui::style::{Color, Style};
use ratatui::widgets::ListState;
use ratatui_textarea::{TextArea, WrapMode};
use std::collections::HashMap;

use crate::backend::{
    BackendError, BackendEvent, Chat, ChatId, LoginStepState, Message, MessengerKind, Provider,
};
use crate::config::{Config, Keymap};
use crate::tui::chat::{ChatState, OpenChat};
use crate::tui::popup::PopupKind;
use tracing::{debug, error, info, warn};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    #[default]
    ChatList,
    Chat,
    Write,
    Popup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Main,
    Settings,
    Login,
}

/// Common application state shared across pages.
pub struct AppState {
    pub config: Config,
    pub keymap: Keymap,
    pub messengers: Vec<MessengerKind>,

    /// main chat state handler
    pub chat_state: ChatState,

    /// Selection state for the settings provider list.
    pub settings_state: ListState,

    pub focus: Focus,
    pub screen: Screen,
    pub write: TextArea<'static>,
    pub pop_up: Option<PopupState>,
    pub running: bool,

    /// Active login session, if any.
    pub login_state: Option<LoginState>,
}

pub struct LoginState {
    /// Which provider is being logged into.
    pub provider: Provider,
    /// Current step index within `MessengerKind::login_steps()`.
    pub step: usize,
    /// Error from the last submission, if any.
    pub error: Option<String>,
    /// Text input reused by the generic login screen.
    pub login_input: TextArea<'static>,
}

pub struct PopupState {
    pub popup_type: PopupKind,
    prev_focus: Focus,
    pub scroll_idx: usize,
}

impl AppState {
    pub async fn new(
        config: Config,
        keymap: Keymap,
        messengers: Vec<MessengerKind>,
        open_settings: bool,
    ) -> Self {
        let mut write = TextArea::default();
        write.set_placeholder_text("Write a message...");
        write.set_cursor_style(Style::default().fg(Color::Yellow));
        write.set_wrap_mode(WrapMode::WordOrGlyph);

        let mut login_input = TextArea::default();
        login_input.set_cursor_style(Style::default().fg(Color::Yellow));
        login_input.set_wrap_mode(WrapMode::WordOrGlyph);

        let mut app = Self {
            config,
            keymap,
            messengers,
            settings_state: ListState::default().with_selected(Some(0)),
            focus: Focus::ChatList,
            screen: if open_settings {
                Screen::Settings
            } else {
                Screen::Main
            },
            write,
            pop_up: None,
            chat_state: Default::default(),
            running: true,
            login_state: None,
        };
        let errors = app.rebuild_chats().await;
        if !errors.is_empty() {
            let msg = errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            app.create_popup(PopupKind::Error(msg));
        }
        if open_settings {
            app.create_popup(PopupKind::Info(String::from(
                "Welcome! Press Enter on a provider to enable it.",
            )));
        }
        app
    }

    pub fn selected_chat_idx(&self) -> Option<usize> {
        self.chat_state.chat_list_state.selected()
    }

    pub async fn rebuild_chats(&mut self) -> Vec<BackendError> {
        debug!("Rebuilding chat list from all providers");
        let saved_scrolls: HashMap<ChatId, usize> = self
            .chat_state
            .chats
            .iter()
            .map(|chat| (chat.id.clone(), chat.scroll))
            .collect();
        let mut chat_list = Vec::new();
        let mut errors: Vec<BackendError> = Vec::new();
        for messenger in &self.messengers {
            let provider = messenger.provider();
            if !provider.is_enabled(&self.config.providers) {
                continue;
            }
            match messenger.chats().await {
                Ok(chats) => {
                    for chat in chats {
                        let scroll = saved_scrolls.get(&chat.id).copied().unwrap_or(0);
                        chat_list.push(Chat { scroll, ..chat });
                    }
                }
                Err(e) => {
                    warn!(provider = provider.name(), error = %e, "Failed to fetch chats");
                    errors.push(e);
                }
            }
        }
        self.chat_state.chats = chat_list;
        let len = self.chat_state.chats.len();
        let index = if len == 0 {
            None
        } else {
            self.chat_state
                .chat_list_state
                .selected()
                .map(|i| i.min(len - 1))
        };
        self.chat_state.chat_list_state.select(index);
        debug!(total = len, "Chat list rebuilt");
        errors
    }

    /// Update the sidebar entry for `chat_id` after a poll refresh detected new messages.
    /// Updates the last_message preview from the newest message in `history`.
    pub fn update_sidebar_from_poll(&mut self, chat_id: &ChatId, history: &[Message]) {
        if let Some(chat) = self.chat_state.chats.iter_mut().find(|c| c.id == *chat_id)
            && let Some(newest) = history.last()
        {
            chat.last_message = Some(format!("{}: {}", newest.sender, newest.text));
        }
    }

    pub fn create_popup(&mut self, popup_type: PopupKind) {
        self.pop_up = Some(PopupState {
            popup_type,
            prev_focus: self.focus,
            scroll_idx: 0,
        })
    }

    pub fn dismiss_popup(&mut self) {
        if let Some(popup) = self.pop_up.take() {
            self.focus = popup.prev_focus;
        }
    }

    pub fn cycle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::ChatList => {
                if self.chat_state.open_chat.is_some() {
                    self.chat_state.open_chat = None;
                    Focus::Chat
                } else {
                    Focus::ChatList
                }
            }
            Focus::Chat => {
                // Save draft and clear write when exiting chat
                if let Some(chat_id) = self
                    .chat_state
                    .open_chat
                    .as_ref()
                    .map(|o| o.chat.id.clone())
                {
                    let text = self.write.lines().join("\n");
                    self.chat_state.save_draft(&chat_id, text);
                }
                self.write.clear();
                self.chat_state.open_chat = None;
                Focus::ChatList
            }
            Focus::Write => {
                // Save draft and clear write when exiting chat
                // + close the current chat
                if let Some(chat_id) = self
                    .chat_state
                    .open_chat
                    .as_ref()
                    .map(|o| o.chat.id.clone())
                {
                    let text = self.write.lines().join("\n");
                    self.chat_state.save_draft(&chat_id, text);
                }
                self.write.clear();
                Focus::Chat
            }
            Focus::Popup => Focus::ChatList,
        };
    }

    pub async fn select_chat(&mut self, index: usize) {
        let Some(chat) = self.chat_state.chats.get(index).cloned() else {
            return;
        };

        debug!(chat = %chat.contact_name, id = ?chat.id, "Opening chat");

        // Save current draft and message selection before switching
        if let Some(current_chat_id) = self
            .chat_state
            .open_chat
            .as_ref()
            .map(|o| o.chat.id.clone())
        {
            let text = self.write.lines().join("\n");
            self.chat_state.save_draft(&current_chat_id, text);
            self.chat_state.save_message_selection();
        }

        let read_result = match self.chat_owner_mut(&chat.id) {
            Some(messenger) => messenger.set_read(&chat.id).await,
            None => Ok(()),
        };
        if let Err(e) = read_result {
            warn!(chat = %chat.contact_name, error = %e, "Failed to mark chat as read");
            self.create_popup(PopupKind::Error(format!(
                "{} failed to mark chat as read: {e}",
                chat.id.platform()
            )));
            return;
        }

        self.chat_state.chats[index].unread = false;
        self.chat_state.chats[index].unread_count = 0;

        let history_result = match self.chat_owner(&chat.id) {
            Some(messenger) => messenger.history(&chat.id).await,
            None => Ok(Vec::new()),
        };
        match history_result {
            Ok(messages) => {
                let history_len = messages.len();
                debug!(chat = %chat.contact_name, messages = history_len, "History loaded");
                self.chat_state.open_chat = Some(OpenChat {
                    chat: chat.clone(),
                    history: messages,
                });
                self.chat_state.restore_message_selection(history_len);
            }
            Err(e) => {
                error!(chat = %chat.contact_name, error = %e, "Failed to load history");
                self.chat_state.open_chat = None;
                self.create_popup(PopupKind::Error(format!(
                    "{} failed to load history for {}: {e}",
                    chat.id.platform(),
                    chat.contact_name
                )));
            }
        }

        // Load draft for the new chat
        self.write.clear();
        if let Some(draft) = self.chat_state.load_draft(&chat.id) {
            self.write.insert_str(draft);
        }

        self.focus = Focus::Chat;
    }

    /// Inserts pasted text into the write box if it has focus.
    pub fn handle_paste(&mut self, text: String) {
        if self.focus == Focus::Write {
            self.write.insert_str(&text);
        }
        if let Some(state) = self.login_state.as_mut()
            && self.screen == Screen::Login
        {
            state.login_input.insert_str(&text);
        }
    }

    pub async fn send_message(&mut self) {
        let text = self.write.lines().join("\n");
        if text.trim().is_empty() {
            return;
        }

        let Some(chat) = self.chat_state.selected_chat().cloned() else {
            self.create_popup(PopupKind::Error(String::from("No chat selected")));
            return;
        };

        let send_result = match self.chat_owner(&chat.id) {
            Some(messenger) => messenger.send(&chat.id, &text).await,
            None => Err(BackendError::Other("no messenger for this chat".into())),
        };
        match send_result {
            Ok(()) => {
                self.write.clear();
                self.chat_state.drafts.remove(&chat.id);
            }
            Err(e) => self.create_popup(PopupKind::Error(format!(
                "{} failed to send message: {e}",
                chat.id.platform()
            ))),
        }
    }

    /// Gets the owner of the current chat
    fn chat_owner(&self, chat: &ChatId) -> Option<&MessengerKind> {
        let target = match chat {
            ChatId::Telegram(_) => Provider::Telegram,
            ChatId::WhatsApp(_) => Provider::WhatsApp,
            ChatId::Myself => return None,
        };
        self.messengers.iter().find(|m| m.provider() == target)
    }

    fn chat_owner_mut(&mut self, chat: &ChatId) -> Option<&mut MessengerKind> {
        let target = match chat {
            ChatId::Telegram(_) => Provider::Telegram,
            ChatId::WhatsApp(_) => Provider::WhatsApp,
            ChatId::Myself => return None,
        };
        self.messengers.iter_mut().find(|m| m.provider() == target)
    }

    pub(crate) fn provider_to_messenger(&self, provider: Provider) -> Option<&MessengerKind> {
        self.messengers.iter().find(|m| m.provider() == provider)
    }

    fn provider_to_messenger_mut(&mut self, provider: Provider) -> Option<&mut MessengerKind> {
        self.messengers
            .iter_mut()
            .find(|m| m.provider() == provider)
    }

    pub async fn toggle_provider(&mut self, provider: Provider) {
        let name = provider.name();

        // --- Disabling is always straightforward ---
        if provider.is_enabled(&self.config.providers) {
            provider.toggle_enabled(&mut self.config.providers);
            match self.config.save() {
                Ok(()) => {
                    let _ = self.rebuild_chats().await;
                    self.create_popup(PopupKind::Info(format!("{name} disabled")));
                }
                Err(e) => {
                    self.create_popup(PopupKind::Error(format!("could not save config: {e}")))
                }
            }
            return;
        }

        // --- Enabling ---
        // Verify API credentials exist
        if !provider.has_credentials(&self.config.providers) {
            self.create_popup(PopupKind::Error(format!(
                "{}: configure credentials in config.toml first",
                provider.name(),
            )));
            return;
        }

        // Verify messenger was initialized
        let messenger = match self.provider_to_messenger(provider) {
            Some(m) => m,
            None => {
                self.create_popup(PopupKind::Error(format!(
                    "{name}: messenger could not be initialized"
                )));
                return;
            }
        };

        // Check if already authenticated — if so, just enable
        if messenger.is_authenticated().await {
            provider.toggle_enabled(&mut self.config.providers);
            match self.config.save() {
                Ok(()) => {
                    let errors = self.rebuild_chats().await;
                    self.create_popup(PopupKind::Info(format!("{name} enabled")));
                    if !errors.is_empty() {
                        self.create_popup(PopupKind::Error(
                            errors
                                .iter()
                                .map(|e| e.to_string())
                                .collect::<Vec<_>>()
                                .join("\n"),
                        ));
                    }
                }
                Err(e) => {
                    self.create_popup(PopupKind::Error(format!("could not save config: {e}")))
                }
            }
            return;
        }

        // Not authenticated — check if provider supports login steps
        let steps = messenger.login_steps();
        if steps.is_empty() {
            self.create_popup(PopupKind::Error(format!(
                "{name}: not authenticated and no login flow available"
            )));
            return;
        }

        // Navigate to login screen
        // TODO: handle error
        let _ = self.start_login(provider);
    }

    pub fn start_login(&mut self, provider: Provider) -> Result<()> {
        let mut input = TextArea::default();
        input.set_cursor_style(Style::default().fg(Color::Yellow));

        self.login_state = Some(LoginState {
            provider,
            step: 0,
            error: None,
            login_input: input,
        });
        self.screen = Screen::Login;
        Ok(())
    }

    pub async fn submit_login(&mut self) {
        let Some(ref login) = self.login_state else {
            return;
        };
        let provider = login.provider;
        let step = login.step;
        let input = login.login_input.lines().join("\n").trim().to_string();

        if input.is_empty() {
            if let Some(ref mut ls) = self.login_state {
                ls.error = Some("input is empty".into());
            }
            return;
        }

        let result = {
            let messenger = self.provider_to_messenger_mut(provider).unwrap();
            messenger.login_step(step, &input).await
        };

        match result {
            Ok(LoginStepState::Done) => {
                let provider = self.login_state.as_ref().map(|s| s.provider);

                // Enable the provider in config and save
                if let Some(p) = provider {
                    p.toggle_enabled(&mut self.config.providers);
                    match self.config.save() {
                        Ok(()) => {}
                        Err(e) => {
                            self.create_popup(PopupKind::Warn(format!(
                                "Error saving config after submit: {e} - Check your config file"
                            )));
                        }
                    };
                }

                self.login_state = None;
                self.screen = Screen::Main;

                self.create_popup(PopupKind::Info(String::from("Logged in successfully")));

                // TODO: error is ignored, make error explicit
                let _ = self.rebuild_chats().await;
            }
            Ok(LoginStepState::NextStep) => {
                if let Some(ref mut ls) = self.login_state {
                    ls.step += 1;
                    ls.error = None;
                    ls.login_input.clear();
                }
            }
            Err(e) => {
                if let Some(ref mut ls) = self.login_state {
                    ls.error = Some(e.to_string());
                }
            }
        }
    }

    pub async fn cancel_login(&mut self) {
        if let Some(login) = self.login_state.take()
            && let Some(messenger) = self.provider_to_messenger_mut(login.provider)
        {
            messenger.cancel_login().await;
        }
        self.screen = Screen::Main;
    }

    pub fn handle_backend_event(&mut self, _provider: Provider, event: BackendEvent) {
        match event {
            BackendEvent::Connected => info!("Backend connected"),
            BackendEvent::Disconnected(message) => {
                warn!(message, "Backend disconnected");
                self.create_popup(PopupKind::Error(message));
            }
            BackendEvent::Error(context, err) => {
                error!(context, error = %err, "Backend error");
                self.create_popup(PopupKind::Error(format!("{context}: {err}")))
            }
            BackendEvent::MessageReceived(message) => {
                let is_open = self.chat_state.push_incoming(message.clone());
                if is_open
                    && let Some(len) = self.chat_state.open_chat.as_ref().map(|o| o.history.len())
                {
                    self.chat_state.message_list_state.select(Some(len - 1));
                }
                let open_chat_id = self
                    .chat_state
                    .open_chat
                    .as_ref()
                    .map(|o| o.chat.id.clone());
                let found = self.chat_state.find_mut(&message.chat);
                debug!(
                    chat = ?message.chat,
                    from_me = message.from_me,
                    is_open,
                    sidebar_found = found.is_some(),
                    open_chat_id = ?open_chat_id,
                    text_preview = &message.text[..message.text.len().min(60)],
                    "TUI MessageReceived",
                );
                if let Some((_, chat)) = found {
                    chat.last_message = Some(format!("{}: {}", message.sender, message.text));
                    if !message.from_me && !is_open {
                        chat.unread = true;
                        chat.unread_count = chat.unread_count.saturating_add(1);
                        debug!(chat = ?message.chat, "TUI set unread");
                    }
                } else {
                    debug!(chat = ?message.chat, "TUI MessageReceived: chat not found in sidebar");
                }
            }

            BackendEvent::ChatUpdated(chat) => {
                let found = self
                    .chat_state
                    .chats
                    .iter_mut()
                    .find(|entry| entry.id == chat.id);
                debug!(
                    chat = ?chat.id,
                    sidebar_found = found.is_some(),
                    "TUI ChatUpdated",
                );
                if let Some(entry) = found {
                    if !chat.contact_name.is_empty() {
                        entry.contact_name = chat.contact_name;
                    }
                    entry.last_message = chat.last_message;
                    entry.unread = chat.unread;
                    entry.unread_count = chat.unread_count;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockMessenger;
    use crate::backend::{Message, Messenger};
    use tokio::sync::broadcast;

    async fn app_state() -> AppState {
        let mut config = Config::default();
        config.providers.telegram.enabled = true;
        let keymap = config.keys.parse().unwrap();
        let mock = MockMessenger::new("Telegram");
        mock.spawn_incoming_messages();
        let messengers = vec![MessengerKind::Stub(Box::new(mock))];
        AppState::new(config, keymap, messengers, false).await
    }

    #[tokio::test]
    async fn boot_has_no_selection_and_empty_pane() {
        let state = app_state().await;
        assert_eq!(state.chat_state.chats.len(), 4);
        assert!(state.selected_chat_idx().is_none());
        assert!(state.chat_state.open_chat.is_none());
    }

    #[test]
    fn paste_goes_into_the_write_box_when_focused() {
        let mut state = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(app_state());
        state.focus = Focus::Write;
        state.handle_paste("line one\nline two".into());
        assert_eq!(
            state.write.lines().to_vec(),
            vec!["line one".to_string(), "line two".to_string()]
        );
    }

    #[test]
    fn paste_is_ignored_when_write_is_not_focused() {
        let mut state = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(app_state());
        state.focus = Focus::Chat;
        state.handle_paste("line one\nline two".into());
        assert_eq!(state.write.lines().to_vec(), vec![String::new()]);
    }

    #[tokio::test]
    async fn write_uses_word_wrap() {
        let state = app_state().await;
        assert_eq!(state.write.wrap_mode(), WrapMode::WordOrGlyph);
    }

    #[tokio::test]
    async fn scroll_is_remembered_per_chat() {
        let mut state = app_state().await;

        state.chat_state.chat_list_state.select(Some(0));
        state.select_chat(0).await;
        assert_eq!(state.chat_state.chats[0].scroll, 0);
        state.chat_state.chats[0].scroll = 10;

        state.chat_state.chat_list_state.select(Some(1));
        state.select_chat(1).await;
        assert_eq!(state.chat_state.chats[1].scroll, 0);

        state.chat_state.chat_list_state.select(Some(0));
        state.select_chat(0).await;
        assert_eq!(state.chat_state.chats[0].scroll, 10);
    }

    #[tokio::test]
    async fn open_chat_tracks_the_opened_conversation() {
        let mut state = app_state().await;

        state.chat_state.chat_list_state.select(Some(1));
        state.select_chat(1).await;

        let current = state.chat_state.open_chat.unwrap();
        assert_eq!(current.chat.id, ChatId::Telegram(102));
        assert!(!current.history.is_empty());
    }

    #[tokio::test]
    async fn incoming_message_updates_open_chat_without_unread() {
        let mut state = app_state().await;
        state.chat_state.chat_list_state.select(Some(0));
        state.select_chat(0).await;
        assert_eq!(
            state.chat_state.open_chat.as_ref().unwrap().history.len(),
            2
        );
        assert!(!state.chat_state.chats[0].unread);

        state.handle_backend_event(
            Provider::Telegram,
            BackendEvent::MessageReceived(Message {
                id: "incoming".into(),
                chat: ChatId::Telegram(101),
                sender: "Telegram News".into(),
                text: "breaking".into(),
                timestamp: 0,
                from_me: false,
                options: Vec::new(),
            }),
        );

        assert_eq!(
            state.chat_state.open_chat.as_ref().unwrap().history.len(),
            3
        );
        assert_eq!(
            state.chat_state.chats[0].last_message.as_deref(),
            Some("Telegram News: breaking")
        );
        assert!(!state.chat_state.chats[0].unread);
    }

    #[tokio::test]
    async fn incoming_message_to_closed_chat_marks_unread() {
        let mut state = app_state().await;
        state.chat_state.chat_list_state.select(Some(0));
        state.select_chat(0).await;

        state.handle_backend_event(
            Provider::Telegram,
            BackendEvent::MessageReceived(Message {
                id: "incoming".into(),
                chat: ChatId::Telegram(103),
                sender: "Alice".into(),
                text: "hi".into(),
                timestamp: 0,
                from_me: false,
                options: Vec::new(),
            }),
        );

        assert!(state.chat_state.chats[2].unread);
        assert_eq!(state.chat_state.chats[2].unread_count, 1);
    }

    #[tokio::test]
    async fn draft_is_saved_when_switching_chats() {
        let mut state = app_state().await;

        state.chat_state.chat_list_state.select(Some(0));
        state.select_chat(0).await;
        state.write.insert_str("hello from chat 0");

        state.chat_state.chat_list_state.select(Some(1));
        state.select_chat(1).await;

        assert_eq!(
            state.chat_state.load_draft(&ChatId::Telegram(101)),
            Some("hello from chat 0")
        );
    }

    #[tokio::test]
    async fn draft_is_loaded_when_selecting_chat() {
        let mut state = app_state().await;

        state.chat_state.chat_list_state.select(Some(0));
        state.select_chat(0).await;
        state.write.insert_str("draft message");

        state.chat_state.chat_list_state.select(Some(1));
        state.select_chat(1).await;

        state.chat_state.chat_list_state.select(Some(0));
        state.select_chat(0).await;

        assert_eq!(state.write.lines().join("\n"), "draft message");
    }

    #[tokio::test]
    async fn draft_is_cleared_on_exit() {
        let mut state = app_state().await;

        state.chat_state.chat_list_state.select(Some(0));
        state.select_chat(0).await;
        state.write.insert_str("unsent text");

        state.focus = Focus::Chat;
        state.cycle_focus();

        assert!(state.write.lines().join("\n").is_empty());
        assert_eq!(
            state.chat_state.load_draft(&ChatId::Telegram(101)),
            Some("unsent text")
        );
    }

    #[tokio::test]
    async fn empty_draft_is_not_stored() {
        let mut state = app_state().await;

        state
            .chat_state
            .save_draft(&ChatId::Telegram(101), "   ".into());
        assert!(
            state
                .chat_state
                .load_draft(&ChatId::Telegram(101))
                .is_none()
        );

        state
            .chat_state
            .save_draft(&ChatId::Telegram(101), "".into());
        assert!(
            state
                .chat_state
                .load_draft(&ChatId::Telegram(101))
                .is_none()
        );
    }

    #[tokio::test]
    async fn draft_is_removed_after_sending() {
        let mut state = app_state().await;

        state.chat_state.chat_list_state.select(Some(0));
        state.select_chat(0).await;
        state.write.insert_str("hello");

        state
            .chat_state
            .save_draft(&ChatId::Telegram(101), "hello".into());
        state.chat_state.drafts.remove(&ChatId::Telegram(101));

        assert!(
            state
                .chat_state
                .load_draft(&ChatId::Telegram(101))
                .is_none()
        );
    }

    #[tokio::test]
    async fn draft_per_chat_is_independent() {
        let mut state = app_state().await;

        state.chat_state.chat_list_state.select(Some(0));
        state.select_chat(0).await;
        state.write.insert_str("msg for chat 0");

        state.chat_state.chat_list_state.select(Some(1));
        state.select_chat(1).await;
        state.write.insert_str("msg for chat 1");

        state.chat_state.chat_list_state.select(Some(0));
        state.select_chat(0).await;

        assert_eq!(
            state.chat_state.load_draft(&ChatId::Telegram(101)),
            Some("msg for chat 0")
        );
        assert_eq!(
            state.chat_state.load_draft(&ChatId::Telegram(102)),
            Some("msg for chat 1")
        );
    }

    // -- StubMessenger for error propagation tests --

    struct StubMessenger {
        platform: &'static str,
        chats_result: std::sync::Mutex<Option<Result<Vec<Chat>, BackendError>>>,
        send_result: std::sync::Mutex<Option<Result<(), BackendError>>>,
        history_result: std::sync::Mutex<Option<Result<Vec<Message>, BackendError>>>,
        set_read_result: std::sync::Mutex<Option<Result<(), BackendError>>>,
        tx: broadcast::Sender<BackendEvent>,
    }

    impl StubMessenger {
        fn new(platform: &'static str) -> Self {
            let (tx, _) = broadcast::channel(16);
            Self {
                platform,
                chats_result: std::sync::Mutex::new(None),
                send_result: std::sync::Mutex::new(None),
                history_result: std::sync::Mutex::new(None),
                set_read_result: std::sync::Mutex::new(None),
                tx,
            }
        }

        fn with_chats_error(self, err: BackendError) -> Self {
            *self.chats_result.lock().unwrap() = Some(Err(err));
            self
        }

        fn with_send_error(self, err: BackendError) -> Self {
            *self.send_result.lock().unwrap() = Some(Err(err));
            self
        }

        fn with_history_error(self, err: BackendError) -> Self {
            *self.history_result.lock().unwrap() = Some(Err(err));
            self
        }

        fn with_set_read_error(self, err: BackendError) -> Self {
            *self.set_read_result.lock().unwrap() = Some(Err(err));
            self
        }

        fn with_chats(self, chats: Vec<Chat>) -> Self {
            *self.chats_result.lock().unwrap() = Some(Ok(chats));
            self
        }
    }

    #[async_trait::async_trait]
    impl Messenger for StubMessenger {
        fn platform(&self) -> &'static str {
            self.platform
        }

        async fn is_authenticated(&self) -> bool {
            true
        }

        async fn chats(&self) -> Result<Vec<Chat>, BackendError> {
            match self.chats_result.lock().unwrap().take() {
                Some(result) => result,
                None => Ok(Vec::new()),
            }
        }

        async fn set_read(&mut self, _chat: &ChatId) -> Result<(), BackendError> {
            match self.set_read_result.lock().unwrap().take() {
                Some(result) => result,
                None => Ok(()),
            }
        }

        async fn history(&self, _chat: &ChatId) -> Result<Vec<Message>, BackendError> {
            match self.history_result.lock().unwrap().take() {
                Some(result) => result,
                None => Ok(Vec::new()),
            }
        }

        async fn send(&self, _chat: &ChatId, _text: &str) -> Result<(), BackendError> {
            match self.send_result.lock().unwrap().take() {
                Some(result) => result,
                None => Ok(()),
            }
        }

        fn subscribe(&self) -> broadcast::Receiver<BackendEvent> {
            self.tx.subscribe()
        }

        async fn disconnect(&mut self) -> Result<(), BackendError> {
            Ok(())
        }

        async fn login(&mut self) -> Result<(), BackendError> {
            Ok(())
        }

        async fn logout(&mut self) -> Result<(), BackendError> {
            Ok(())
        }
    }

    // -- Error propagation tests --

    #[tokio::test]
    async fn messenger_chats_failure_is_nonfatal() {
        let good = MockMessenger::new("Telegram");
        let bad = StubMessenger::new("WhatsApp")
            .with_chats_error(BackendError::Other("connection refused".into()));
        let mut config = Config::default();
        config.providers.telegram.enabled = true;
        config.providers.whatsapp = true;
        let keymap = config.keys.parse().unwrap();
        let messengers = vec![
            MessengerKind::Stub(Box::new(good)),
            MessengerKind::Stub(Box::new(bad)),
        ];
        let state = AppState::new(config, keymap, messengers, false).await;
        assert_eq!(
            state.chat_state.chats.len(),
            4,
            "Telegram chats should still load"
        );
        let popup = state.pop_up.as_ref().expect("error popup should exist");
        match &popup.popup_type {
            PopupKind::Error(msg) => assert!(msg.contains("connection refused")),
            other => panic!("expected Error popup, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn messenger_send_failure_shows_overlay() {
        let chat = Chat {
            id: ChatId::Telegram(1),
            contact_name: "Test".into(),
            ..Default::default()
        };
        let stub = StubMessenger::new("Telegram")
            .with_chats(vec![chat])
            .with_send_error(BackendError::Other("rate limited".into()));
        let mut config = Config::default();
        config.providers.telegram.enabled = true;
        let keymap = config.keys.parse().unwrap();
        let mut state = AppState::new(
            config,
            keymap,
            vec![MessengerKind::Stub(Box::new(stub))],
            false,
        )
        .await;
        state.chat_state.chat_list_state.select(Some(0));
        state.write.insert_str("hello");
        state.send_message().await;
        let popup = state
            .pop_up
            .as_ref()
            .expect("send error popup should exist");
        match &popup.popup_type {
            PopupKind::Error(msg) => {
                assert!(msg.contains("Telegram"), "should mention provider: {msg}");
                assert!(msg.contains("rate limited"), "should include error: {msg}");
            }
            other => panic!("expected Error popup, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn messenger_history_failure_shows_overlay() {
        let chat = Chat {
            id: ChatId::Telegram(1),
            contact_name: "Test".into(),
            ..Default::default()
        };
        let stub = StubMessenger::new("Telegram")
            .with_chats(vec![chat])
            .with_history_error(BackendError::Other("timeout".into()));
        let mut config = Config::default();
        config.providers.telegram.enabled = true;
        let keymap = config.keys.parse().unwrap();
        let mut state = AppState::new(
            config,
            keymap,
            vec![MessengerKind::Stub(Box::new(stub))],
            false,
        )
        .await;
        state.chat_state.chat_list_state.select(Some(0));
        state.select_chat(0).await;
        let popup = state
            .pop_up
            .as_ref()
            .expect("history error popup should exist");
        match &popup.popup_type {
            PopupKind::Error(msg) => {
                assert!(msg.contains("Telegram"), "should mention provider: {msg}");
                assert!(msg.contains("history"), "should mention history: {msg}");
            }
            other => panic!("expected Error popup, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn messenger_set_read_failure_shows_overlay() {
        let chat = Chat {
            id: ChatId::Telegram(1),
            contact_name: "Test".into(),
            ..Default::default()
        };
        let stub = StubMessenger::new("Telegram")
            .with_chats(vec![chat])
            .with_set_read_error(BackendError::Other("permission denied".into()));
        let mut config = Config::default();
        config.providers.telegram.enabled = true;
        let keymap = config.keys.parse().unwrap();
        let mut state = AppState::new(
            config,
            keymap,
            vec![MessengerKind::Stub(Box::new(stub))],
            false,
        )
        .await;
        state.chat_state.chat_list_state.select(Some(0));
        state.select_chat(0).await;
        let popup = state
            .pop_up
            .as_ref()
            .expect("set_read error popup should exist");
        match &popup.popup_type {
            PopupKind::Error(msg) => {
                assert!(msg.contains("Telegram"), "should mention provider: {msg}");
                assert!(msg.contains("read"), "should mention read: {msg}");
            }
            other => panic!("expected Error popup, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn backend_error_event_shows_overlay() {
        let stub = StubMessenger::new("Telegram");
        let mut config = Config::default();
        config.providers.telegram.enabled = true;
        let keymap = config.keys.parse().unwrap();
        let mut state = AppState::new(
            config,
            keymap,
            vec![MessengerKind::Stub(Box::new(stub))],
            false,
        )
        .await;
        state.handle_backend_event(
            Provider::Telegram,
            BackendEvent::Error(
                "Telegram subscription failed".into(),
                BackendError::Other("stream closed".into()),
            ),
        );
        let popup = state
            .pop_up
            .as_ref()
            .expect("backend error popup should exist");
        match &popup.popup_type {
            PopupKind::Error(msg) => {
                assert!(
                    msg.contains("Telegram subscription failed"),
                    "should include context: {msg}"
                );
                assert!(msg.contains("stream closed"), "should include error: {msg}");
            }
            other => panic!("expected Error popup, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn multiple_messenger_boot_errors_are_combined() {
        let bad_tg = StubMessenger::new("Telegram")
            .with_chats_error(BackendError::Other("auth failed".into()));
        let bad_wa = StubMessenger::new("WhatsApp")
            .with_chats_error(BackendError::Other("connection refused".into()));
        let mut config = Config::default();
        config.providers.telegram.enabled = true;
        config.providers.whatsapp = true;
        let keymap = config.keys.parse().unwrap();
        let messengers = vec![
            MessengerKind::Stub(Box::new(bad_tg)),
            MessengerKind::Stub(Box::new(bad_wa)),
        ];
        let state = AppState::new(config, keymap, messengers, false).await;
        assert!(state.chat_state.chats.is_empty());
        let popup = state
            .pop_up
            .as_ref()
            .expect("combined error popup should exist");
        match &popup.popup_type {
            PopupKind::Error(msg) => {
                assert!(
                    msg.contains("auth failed"),
                    "should mention auth failed: {msg}"
                );
                assert!(
                    msg.contains("connection refused"),
                    "should mention connection refused: {msg}"
                );
            }
            other => panic!("expected Error popup, got {other:?}"),
        }
    }
}
