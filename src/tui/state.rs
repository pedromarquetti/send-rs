use anyhow::Result;
use ratatui::style::{Color, Style};
use ratatui::widgets::ListState;
use ratatui_textarea::{TextArea, WrapMode};
use std::collections::HashMap;

use crate::backend::{
    BackendError, BackendEvent, Chat, ChatId, LoginStepState, Message, MessageId, MessengerKind,
    Provider,
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

#[derive(Clone)]
pub struct RetryDraft {
    pub chat: ChatId,
    pub text: String,
    pub message_id: Option<MessageId>,
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

    /// True once the initial chat list has finished loading from all providers.
    /// While false, the chat pane shows the loading widget instead of a blank
    /// terminal (non-blocking startup).
    pub chats_loaded: bool,

    /// Monotonic counter for generating local/synthetic echo message ids.
    pub local_seq: u64,
    pub chat_load_generation: u64,
    pub history_refresh_in_flight: bool,
    pub sidebar_sync_in_flight: bool,
    pub backend_status: Option<String>,
    pub retry_draft: Option<RetryDraft>,
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
    /// True while an auth step RPC is in flight (drives the loading widget).
    pub submitting: bool,
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
        write.set_cursor_line_style(Style::default());
        write.set_wrap_mode(WrapMode::WordOrGlyph);

        let mut login_input = TextArea::default();
        login_input.set_cursor_style(Style::default().fg(Color::Yellow));
        login_input.set_cursor_line_style(Style::default());
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
            chats_loaded: false,
            local_seq: 0,
            chat_load_generation: 0,
            history_refresh_in_flight: false,
            sidebar_sync_in_flight: false,
            backend_status: None,
            retry_draft: None,
        };

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

    /// Apply a previously fetched chat list to the TUI chat state, preserving
    /// each chat's saved scroll position.
    pub fn apply_chats(&mut self, chats: Vec<Chat>) {
        let saved_scrolls: HashMap<ChatId, usize> = self
            .chat_state
            .chats
            .iter()
            .map(|chat| (chat.id.clone(), chat.scroll))
            .collect();
        let chat_list: Vec<Chat> = chats
            .into_iter()
            .map(|chat| {
                let scroll = saved_scrolls.get(&chat.id).copied().unwrap_or(0);
                Chat { scroll, ..chat }
            })
            .collect();
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
        debug!(total = len, "Chat list applied");
    }

    pub async fn rebuild_chats(&mut self) -> Vec<BackendError> {
        debug!("Rebuilding chat list from all providers");
        let (chats, errors) = fetch_all_chats(&self.messengers, &self.config.providers).await;
        self.apply_chats(chats);
        self.chats_loaded = true;
        errors
    }

    /// Apply the result of a (background) chat fetch: populate the chat list,
    /// mark loading as complete, and surface any provider errors as a popup.
    /// Shared by the TUI event loop and tests so error-surfacing logic stays in
    /// one place.
    pub fn apply_fetched(&mut self, chats: Vec<Chat>, errors: Vec<BackendError>) {
        self.apply_chats(chats);
        self.chats_loaded = true;
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
                // Invalidate a background history request when Escape is used
                // before the selected chat finishes opening.
                self.chat_load_generation = self.chat_load_generation.wrapping_add(1);
                if self.chat_state.open_chat.is_some() {
                    self.chat_state.open_chat = None;
                    Focus::Chat
                } else {
                    Focus::ChatList
                }
            }
            Focus::Chat => {
                self.chat_load_generation = self.chat_load_generation.wrapping_add(1);
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

                // The edit/reply should not persist if user dismisses Write
                self.chat_state.pending_reply = None;
                self.chat_state.pending_edit = None;

                self.write.clear();
                Focus::Chat
            }
            Focus::Popup => Focus::ChatList,
        };
    }

    pub fn cancel_chat_load(&mut self) {
        self.chat_load_generation = self.chat_load_generation.wrapping_add(1);
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

    pub fn begin_chat_load(&mut self, index: usize) -> Option<(Chat, u64, MessengerKind)> {
        let chat = self.chat_state.chats.get(index).cloned()?;

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

        self.chat_load_generation = self.chat_load_generation.wrapping_add(1);
        self.chat_state.open_chat = None;
        self.chat_state.chats[index].unread = false;
        self.chat_state.chats[index].unread_count = 0;
        self.write.clear();

        if let Some(draft) = self.chat_state.load_draft(&chat.id) {
            self.write.insert_str(draft);
        }

        self.focus = Focus::Chat;

        let messenger = self
            .messengers
            .iter()
            .find(|m| m.provider() == chat.id.to_provider())?
            .clone();

        Some((chat, self.chat_load_generation, messenger))
    }

    pub fn apply_chat_load(
        &mut self,
        chat: Chat,
        generation: u64,
        result: std::result::Result<Vec<Message>, BackendError>,
    ) {
        // Return early if user closed the chat and opened a new one
        if generation != self.chat_load_generation
            || self.chat_state.selected_chat().map(|c| &c.id) != Some(&chat.id)
        {
            return;
        }

        match result {
            Ok(messages) => {
                let history_len = messages.len();
                self.chat_state.open_chat = Some(OpenChat {
                    chat,
                    history: messages,
                });
                self.chat_state.restore_message_selection(history_len);
            }
            Err(e) => {
                error!(chat = %chat.contact_name, error = %e, "Failed to load history");
                self.create_popup(PopupKind::Error(format!(
                    "{} failed to load history for {}: {e}",
                    chat.id.platform(),
                    chat.contact_name
                )));
            }
        }
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

        let editing = self.chat_state.pending_edit.take();

        let reply_to = self.chat_state.pending_reply.take().map(|i| i.message_id);

        if let Some(message) = editing {
            // Editing an existing message: update in place, no echo.
            let result = match self.chat_owner(&chat.id) {
                Some(messenger) => messenger.edit(&chat.id, &message.message_id, &text).await,
                None => Err(BackendError::Other("no messenger for this chat".into())),
            };

            match result {
                Ok(()) => {
                    self.write.clear();
                    self.chat_state.drafts.remove(&chat.id);
                    self.chat_state
                        .update_message_text(&message.message_id, &text);
                }
                Err(e) => {
                    self.create_popup(PopupKind::Error(format!(
                        "{} failed to edit message: {e}",
                        chat.id.platform()
                    )));
                }
            }
            return;
        }

        let send_result = match self.chat_owner(&chat.id) {
            Some(messenger) => messenger.send(&chat.id, &text, reply_to.clone()).await,
            None => Err(BackendError::Other("no messenger for this chat".into())),
        };

        match send_result {
            Ok(confirmed) => {
                self.write.clear();
                self.chat_state.drafts.remove(&chat.id);
                self.chat_state.push_incoming(confirmed);
            }
            Err(e) => {
                self.retry_draft = Some(RetryDraft {
                    chat: chat.id.clone(),
                    text,
                    message_id: reply_to,
                });
                self.create_popup(PopupKind::Error(format!(
                    "{} failed to send message: {e}, press 1 to retry",
                    chat.id.platform()
                )));
            }
        }
    }

    pub async fn retry_message(&mut self) {
        let Some(draft) = self.retry_draft.take() else {
            return;
        };

        let send_result = match self.chat_owner(&draft.chat) {
            Some(messenger) => {
                messenger
                    .send(&draft.chat, &draft.text, draft.message_id.clone())
                    .await
            }
            None => Err(BackendError::Other("no messenger for this chat".into())),
        };

        match send_result {
            Ok(confirmed) => {
                self.write.clear();
                self.chat_state.drafts.remove(&draft.chat);
                self.chat_state.push_incoming(confirmed);
            }
            Err(e) => {
                let chat = draft.chat.clone();
                self.retry_draft = Some(draft);
                self.create_popup(PopupKind::Error(format!(
                    "{} failed to resend message: {e}, press 1 to retry",
                    chat.platform()
                )));
            }
        }
    }

    /// Set up the write box to reply to `id`. The next send will quote it.
    pub fn reply_to_message(&mut self, id: &MessageId) {
        let Some(message) = self.chat_state.find_message(id).cloned() else {
            return;
        };

        self.chat_state.pending_reply = Some(message);
        self.chat_state.pending_edit = None;
        self.focus = Focus::Write;
    }

    /// Load `id`'s text into the write box for editing. The next send will
    /// issue an edit instead of a new message. Returns false if not found.
    pub fn edit_message(&mut self, id: &MessageId) -> bool {
        let Some(msg) = self.chat_state.find_message(id).cloned() else {
            return false;
        };
        self.write.clear();
        self.write.insert_str(&msg.text);
        self.chat_state.pending_edit = Some(msg);
        self.chat_state.pending_reply = None;
        self.focus = Focus::Write;
        true
    }

    /// Delete a message. Local/pending ones are simply dropped from view;
    /// confirmed ones are removed from the backend too.
    pub async fn delete_message(&mut self, id: &MessageId) {
        let Some(chat) = self.chat_state.selected_chat().cloned() else {
            return;
        };

        if id.is_local() {
            self.chat_state.remove_message(id);
            return;
        }

        let result = match self.chat_owner(&chat.id) {
            Some(messenger) => messenger.delete(&chat.id, id).await,
            None => Ok(()),
        };

        match result {
            Ok(()) => {
                self.chat_state.remove_message(id);
            }
            Err(e) => {
                self.create_popup(PopupKind::Error(format!(
                    "{} failed to delete message: {e}",
                    chat.id.platform()
                )));
            }
        }
    }

    /// Gets the owner of the current chat
    pub(crate) fn chat_owner(&self, chat: &ChatId) -> Option<&MessengerKind> {
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
                    let errors = self.rebuild_chats().await;
                    self.create_popup(PopupKind::Info(format!("{name} disabled")));
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
        if let Err(e) = self.start_login(provider) {
            self.create_popup(PopupKind::Error(format!("could not start login: {e}")));
        }
    }

    pub fn start_login(&mut self, provider: Provider) -> Result<()> {
        let mut input = TextArea::default();
        input.set_cursor_style(Style::default().fg(Color::Yellow));
        input.set_cursor_line_style(Style::default());

        self.login_state = Some(LoginState {
            provider,
            step: 0,
            error: None,
            login_input: input,
            submitting: false,
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

        if let Some(ref mut ls) = self.login_state {
            ls.submitting = true;
        }

        let result = {
            let messenger = self.provider_to_messenger_mut(provider).unwrap();
            messenger.login_step(step, &input).await
        };

        if let Some(ref mut ls) = self.login_state {
            ls.submitting = false;
        }

        match result {
            Ok(LoginStepState::Done) => {
                let provider = self.login_state.as_ref().map(|s| s.provider);

                // Enable the provider in config and save
                if let Some(provider) = provider {
                    if !provider.is_enabled(&self.config.providers) {
                        provider.toggle_enabled(&mut self.config.providers);
                    }
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

                let errors = self.rebuild_chats().await;
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
            BackendEvent::Status(status) => {
                self.backend_status = (!status.is_empty()).then_some(status);
            }
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
                    chat.last_message = Some(crate::helpers::message_preview(
                        &message.sender,
                        &message.text,
                    ));

                    if !message.from_me && !is_open {
                        chat.unread = true;
                        chat.unread_count = chat.unread_count.saturating_add(1);
                        debug!(chat = ?message.chat, "TUI set unread");
                    }
                } else {
                    let preview = crate::helpers::message_preview(&message.sender, &message.text);
                    let mut new_chat = Chat {
                        id: message.chat.clone(),
                        contact_name: message.sender.clone(),
                        last_message: Some(preview.clone()),
                        unread: !message.from_me,
                        unread_count: if message.from_me { 0 } else { 1 },
                        ..Default::default()
                    };
                    if message.from_me {
                        new_chat.unread = false;
                    }
                    self.chat_state.upsert_chat(new_chat);
                    debug!(chat = ?message.chat, "TUI MessageReceived: inserted sidebar chat from push event");
                }
            }

            BackendEvent::MessageUpdated(message) => {
                self.chat_state.update_message(message.clone());
                self.refresh_sidebar_preview(&message.chat);
            }

            BackendEvent::MessageDeleted { chat, message_ids } => {
                let Some(chat) = chat else {
                    return;
                };
                for message_id in message_ids {
                    self.chat_state.remove_message(&message_id);
                }
                self.refresh_sidebar_preview(&chat);
                if let Some(open) = self.chat_state.open_chat.as_ref()
                    && open.chat.id == chat
                {
                    self.chat_state.message_list_state.select(None);
                    let len = self
                        .chat_state
                        .open_chat
                        .as_ref()
                        .map(|o| o.history.len())
                        .unwrap_or(0);
                    if len > 0 {
                        self.chat_state
                            .message_list_state
                            .select(Some(len.saturating_sub(1)));
                    }
                }
            }

            BackendEvent::ChatUpdated(chat) => {
                debug!(chat = ?chat.id, "TUI ChatUpdated");
                self.chat_state.upsert_chat(chat);
            }
        }
    }
}

impl AppState {
    fn refresh_sidebar_preview(&mut self, chat_id: &ChatId) {
        let latest = self
            .chat_state
            .open_chat
            .as_ref()
            .filter(|open| open.chat.id == *chat_id)
            .and_then(|open| open.history.last())
            .map(|message| crate::helpers::message_preview(&message.sender, &message.text));

        if let Some((_, chat)) = self.chat_state.find_mut(chat_id) {
            chat.last_message = latest;
        }
    }
}

/// Fetch the chat list from all enabled providers, returning raw chats and any
/// per-provider errors. Does not mutate any state and does not borrow [`AppState`],
/// so it may be driven from a background task that owns its own messenger/config
/// clones (used for non-blocking startup).
pub async fn fetch_all_chats(
    messengers: &[MessengerKind],
    providers: &crate::config::ProvidersConfig,
) -> (Vec<Chat>, Vec<BackendError>) {
    let mut chat_list = Vec::new();
    let mut errors: Vec<BackendError> = Vec::new();

    for messenger in messengers {
        let provider = messenger.provider();

        if !provider.is_enabled(providers) {
            continue;
        }

        if !messenger.is_authenticated().await {
            warn!(
                provider = provider.name(),
                "Skipping chat fetch for unauthorized provider"
            );
            continue;
        }

        match messenger.chats().await {
            Ok(chats) => chat_list.extend(chats),
            Err(e) => {
                warn!(provider = provider.name(), error = %e, "Failed to fetch chats");
                errors.push(e);
            }
        }
    }
    (chat_list, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockMessenger;
    use crate::backend::{Message, MessageId, Messenger};
    use tokio::sync::broadcast;

    async fn app_state() -> AppState {
        let mut config = Config::default();
        config.providers.telegram.enabled = true;
        let keymap = config.keys.parse().unwrap();
        let mock = MockMessenger::new("Telegram");
        mock.spawn_incoming_messages();
        let messengers = vec![MessengerKind::Stub(Box::new(mock))];
        let mut app = AppState::new(config, keymap, messengers, false).await;
        // Mirror non-blocking startup: independently fetch and apply chats.
        let (chats, _) = fetch_all_chats(&app.messengers, &app.config.providers).await;
        app.apply_chats(chats);
        app.chats_loaded = true;
        app
    }

    /// Fetch chats from the state's messengers and apply them (plus any errors),
    /// as the TUI event loop does on `ChatsLoaded`.
    async fn fetch_and_apply(state: &mut AppState) {
        let (chats, errors) = fetch_all_chats(&state.messengers, &state.config.providers).await;
        state.apply_fetched(chats, errors);
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
                message_id: "incoming".into(),
                chat: ChatId::Telegram(101),
                sender: "Telegram News".into(),
                text: "breaking".into(),
                timestamp: 0,
                from_me: false,
                msg_actions: Vec::new(),
                reply_to_id: None,
                reply_ctx: None,
                pending: false,
                failed: false,
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
                message_id: "incoming".into(),
                chat: ChatId::Telegram(103),
                sender: "Alice".into(),
                text: "hi".into(),
                timestamp: 0,
                from_me: false,
                msg_actions: Vec::new(),
                reply_to_id: None,
                reply_ctx: None,
                pending: false,
                failed: false,
            }),
        );

        assert!(state.chat_state.chats[2].unread);
        assert_eq!(state.chat_state.chats[2].unread_count, 1);
    }

    #[tokio::test]
    async fn incoming_message_to_unknown_chat_inserts_sidebar_entry() {
        let mut state = app_state().await;
        let before = state.chat_state.chats.len();

        state.handle_backend_event(
            Provider::Telegram,
            BackendEvent::MessageReceived(Message {
                message_id: "new-chat".into(),
                chat: ChatId::Telegram(999),
                sender: "New contact".into(),
                text: "hello there".into(),
                timestamp: 0,
                from_me: false,
                msg_actions: Vec::new(),
                reply_to_id: None,
                reply_ctx: None,
                pending: false,
                failed: false,
            }),
        );

        assert_eq!(state.chat_state.chats.len(), before + 1);
        assert_eq!(state.chat_state.chats[0].id, ChatId::Telegram(999));
        assert_eq!(
            state.chat_state.chats[0].last_message.as_deref(),
            Some("New contact: hello there")
        );
        assert!(state.chat_state.chats[0].unread);
        assert_eq!(state.chat_state.chats[0].unread_count, 1);
    }

    #[tokio::test]
    async fn message_deleted_event_removes_open_history_message() {
        let mut state = app_state().await;
        state.chat_state.chat_list_state.select(Some(0));
        state.select_chat(0).await;

        let removed = state
            .chat_state
            .open_chat
            .as_ref()
            .and_then(|open| open.history.first())
            .map(|message| message.message_id.clone())
            .unwrap();

        state.handle_backend_event(
            Provider::Telegram,
            BackendEvent::MessageDeleted {
                chat: Some(ChatId::Telegram(101)),
                message_ids: vec![removed.clone()],
            },
        );

        assert!(
            !state
                .chat_state
                .open_chat
                .as_ref()
                .unwrap()
                .history
                .iter()
                .any(|message| message.message_id == removed)
        );
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

        async fn reply_context(
            &self,
            _chat: &ChatId,
            _message_id: &MessageId,
        ) -> Result<Option<crate::backend::ReplyContext>, BackendError> {
            Ok(None)
        }

        async fn send(
            &self,
            chat: &ChatId,
            text: &str,
            _reply_to: Option<MessageId>,
        ) -> Result<Message, BackendError> {
            match self.send_result.lock().unwrap().take() {
                Some(Err(err)) => Err(err),
                _ => Ok(Message {
                    message_id: "stub-sent".into(),
                    chat: chat.clone(),
                    sender: "You".into(),
                    text: text.to_string(),
                    timestamp: 0,
                    from_me: true,
                    msg_actions: Vec::new(),
                    reply_to_id: None,
                    reply_ctx: None,
                    pending: false,
                    failed: false,
                }),
            }
        }

        async fn delete(&self, _chat: &ChatId, _id: &MessageId) -> Result<(), BackendError> {
            Ok(())
        }

        async fn edit(
            &self,
            _chat: &ChatId,
            _id: &MessageId,
            _text: &str,
        ) -> Result<(), BackendError> {
            Ok(())
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
        let mut state = AppState::new(config, keymap, messengers, false).await;
        fetch_and_apply(&mut state).await;
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
        fetch_and_apply(&mut state).await;
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
        fetch_and_apply(&mut state).await;
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
        fetch_and_apply(&mut state).await;
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
        let mut state = AppState::new(config, keymap, messengers, false).await;
        fetch_and_apply(&mut state).await;
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
