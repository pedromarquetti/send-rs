use anyhow::Result;
use ratatui::style::{Color, Style};
use ratatui::widgets::ListState;
use ratatui_textarea::{TextArea, WrapMode};
use std::collections::{HashMap, HashSet};

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
    pub chatlist_sync_in_flight: bool,
    pub chatlist_sync_pending: usize,
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
            chatlist_sync_in_flight: false,
            chatlist_sync_pending: 0,
            backend_status: None,
            retry_draft: None,
        };

        if let Ok(chat_cache) = Config::load_chats() {
            debug!(chat_count = chat_cache.len(), "Loaded persisted chat list");
            app.apply_chats(chat_cache);
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

    /// True while the chat-list search UI is engaged and focused.
    pub fn chat_list_search_active(&self) -> bool {
        self.focus == Focus::ChatList && self.chat_state.search.is_active()
    }

    /// True while the chat-list search input is focused and being typed in.
    pub fn chat_list_search_typing(&self) -> bool {
        self.focus == Focus::ChatList && self.chat_state.search.is_inserting()
    }

    /// Apply a previously fetched chat list to the TUI chat state, preserving
    /// each chat's saved scroll position. Providers that appear in a fresh,
    /// non-empty snapshot are treated as authoritative: rows they no longer
    /// report (e.g. a WhatsApp LID twin folded into its PN row by the backend
    /// merge) are pruned. Chat state for an enabled provider that returned no
    /// live entries at all (e.g. WhatsApp during a cold start, where the
    /// realtime client has not re-synced yet) is kept rather than wiped out.
    pub fn apply_chats(&mut self, chats: Vec<Chat>) {
        let selected_id = self.chat_state.selected_chat().map(|chat| chat.id.clone());
        let saved_scrolls: HashMap<ChatId, usize> = self
            .chat_state
            .chats
            .iter()
            .map(|chat| (chat.id.clone(), chat.scroll))
            .collect();

        let mut seen = HashSet::new();
        let mut chat_list: Vec<Chat> = chats
            .into_iter()
            .filter(|chat| seen.insert(chat.id.clone()))
            .map(|chat| {
                let scroll = saved_scrolls.get(&chat.id).copied().unwrap_or(0);
                Chat { scroll, ..chat }
            })
            .collect();
        debug!(total = chat_list.len(), "Applying deduplicated chat list");

        let live_providers: HashSet<Provider> =
            chat_list.iter().map(|chat| chat.id.to_provider()).collect();

        // Preserve only rows whose provider is missing entirely from the fresh
        // snapshot (cold start / still reconnecting) or is disabled. A provider
        // that returned ANY chats is authoritative, so a stale row it no longer
        // reports (e.g. a merged-away LID twin) must be pruned, never kept.
        let preserved: Vec<Chat> = self
            .chat_state
            .chats
            .iter()
            .filter(|old| match &old.id {
                ChatId::Myself => false,
                id => {
                    let provider = id.to_provider();
                    !chat_list.iter().any(|c| c.id == *id)
                        && !live_providers.contains(&provider)
                        && provider.is_enabled(&self.config.providers)
                }
            })
            .cloned()
            .collect();

        chat_list.extend(preserved);

        self.chat_state.chats = chat_list;
        self.chat_state.sort_pinned_then_recent();
        self.chat_state.refresh_search_with_id(selected_id);
        debug!(
            total = self.chat_state.chats.len(),
            "Chat list applied"
        );
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

        self.persist_chats();
    }

    /// Persist the current chat list so it survives a cold restart.
    pub(crate) fn persist_chats(&self) {
        if let Err(e) = Config::save_chats(&self.chat_state.chats) {
            error!(error = %e, "Failed to persist chat list");
        }
    }

    /// Update the chat list entry for `chat_id` after a poll refresh detected new messages.
    /// Sets the recency timestamp from the newest message in `history` and re-sorts,
    /// keeping the currently selected chat selected.
    pub fn update_chat_list_from_poll(&mut self, chat_id: &ChatId, history: &[Message]) {
        let Some((_, chat)) = self.chat_state.find_mut(chat_id) else {
            return;
        };

        let Some(newest) = history.last() else {
            return;
        };

        // Never regress: a partial poll history must not lower the recency ts.
        if chat.last_message_ts.is_some_and(|t| newest.timestamp <= t) {
            return;
        }

        chat.last_message_ts = Some(newest.timestamp);

        self.chat_state.sort_pinned_then_recent();
        self.chat_state.refresh_search();
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

    pub fn begin_chat_load(&mut self, index: usize) -> Option<(Chat, u64, MessengerKind)> {
        let src = self.chat_state.resolve_open_index(index)?;
        let chat = self.chat_state.chats.get(src).cloned()?;

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
        self.chat_state.chats[src].unread = false;
        self.chat_state.chats[src].unread_count = 0;
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
        status: Option<String>,
    ) {
        // Return early if user closed the chat and opened a new one
        if generation != self.chat_load_generation
            || self.chat_state.selected_chat().map(|c| &c.id) != Some(&chat.id)
        {
            return;
        }

        // The presence subscription was sent before history loaded; surface
        // the peer's status on the chat list row and, when the load succeeds,
        // on the freshly opened chat.
        if let Some(status) = status.as_deref()
            && let Some(chat) = self.chat_state.chats.iter_mut().find(|c| c.id == chat.id)
        {
            chat.status = Some(status.to_string());
        }

        match result {
            Ok(messages) => {
                let history_len = messages.len();

                self.chat_state.open_chat = Some(OpenChat {
                    chat: chat.clone(),
                    history: messages,
                    has_more_history: true,
                });

                if let (Some(status), Some(open)) =
                    (status.as_deref(), self.chat_state.open_chat.as_mut())
                {
                    open.chat.status = Some(status.to_string());
                }

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

    /// Apply a lazy-history page produced by `App::load_more_history` to the
    /// open chat, stitching the older messages in front of the current history.
    /// The chat-id guard is the only check needed: fetching runs in a single
    /// in-flight slot (`App::chat_load_task`), so a page can never be overtaken
    /// by another page for the same or a different chat.
    pub fn apply_history_page(
        &mut self,
        chat_id: &ChatId,
        result: std::result::Result<Vec<Message>, BackendError>,
    ) {
        let Some(open) = self.chat_state.open_chat.as_mut() else {
            return;
        };
        if open.chat.id != *chat_id {
            return;
        }

        match result {
            Ok(older) if !older.is_empty() => {
                let existing = open.history.clone();
                let filtered = older
                    .into_iter()
                    .filter(|msg| {
                        !existing
                            .iter()
                            .any(|item| item.message_id == msg.message_id)
                    })
                    .collect::<Vec<_>>();

                if !filtered.is_empty() {
                    let count = filtered.len();
                    self.chat_state.prepend_history(chat_id, filtered);
                    self.chat_state.message_list_state.select(Some(count));
                }
            }
            Ok(_) => {
                open.has_more_history = false;
            }
            Err(err) => {
                open.has_more_history = false;
                error!(chat = ?chat_id, error = %err, "Lazy history load failed");
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

                let sent_ts = confirmed.timestamp;
                let sent_chat_id = confirmed.chat.clone();

                self.chat_state.upsert_chat(Chat {
                    id: sent_chat_id,
                    last_message_ts: Some(sent_ts),
                    ..Default::default()
                });

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

                let sent_ts = confirmed.timestamp;
                let sent_chat_id = confirmed.chat.clone();

                self.chat_state.upsert_chat(Chat {
                    id: sent_chat_id,
                    last_message_ts: Some(sent_ts),
                    ..Default::default()
                });

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

        if message.pending || message.failed {
            return;
        }

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

        if !msg.from_me || msg.pending || msg.failed {
            return false;
        }

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

        let Some(message) = self.chat_state.find_message(id).cloned() else {
            return;
        };

        if !message.from_me || message.pending || message.failed {
            self.create_popup(PopupKind::Error(format!(
                "{} cannot delete this message",
                chat.id.platform()
            )));
            return;
        }

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
            match self.config.save_config() {
                Ok(()) => {
                    // If the user is currently viewing a chat belonging to the
                    // disabled provider, close it so the UI does not retain an
                    // inactive-provider conversation after its chats vanish.
                    if let Some(open) = &self.chat_state.open_chat
                        && open.chat.id.to_provider() == provider
                        && !matches!(open.chat.id, ChatId::Myself)
                    {
                        self.write.clear();
                        self.chat_state.open_chat = None;
                    }

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
            Some(m) => match m {
                MessengerKind::WhatsApp(w) => {
                    w.start();
                    m
                }

                _ => m,
            },
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
            match self.config.save_config() {
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

        // Not authenticated — check if the provider supports login steps. For
        // WhatsApp this starts the (inactive) transport so its event-driven QR
        // pairing flow begins generating codes, then navigates to the login
        // screen. The provider is marked enabled up front (pairing is in
        // progress); cancelling via Esc disables it again, and the Connected
        // event leaves it enabled and triggers the chat rebuild.
        let steps = messenger.login_steps();
        if steps.is_empty() {
            self.create_popup(PopupKind::Error(format!(
                "{name}: not authenticated and no login flow available"
            )));
            return;
        }

        if provider == Provider::WhatsApp && !provider.is_enabled(&self.config.providers) {
            provider.toggle_enabled(&mut self.config.providers);
            if let Err(e) = self.config.save_config() {
                self.create_popup(PopupKind::Error(format!("could not save config: {e}")));
            }
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
                    match self.config.save_config() {
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
                // Clamp so a single-step flow (e.g. WhatsApp QR) cannot
                // advance past the last login screen into an index error.
                let step_count = self
                    .login_state
                    .as_ref()
                    .and_then(|ls| self.provider_to_messenger(ls.provider))
                    .map(|steps| steps.login_steps().len());

                if let Some(ref mut ls) = self.login_state {
                    let next = match step_count {
                        Some(total) => (ls.step + 1).min(total.saturating_sub(1)),
                        None => ls.step + 1,
                    };

                    ls.step = next;
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
        if let Some(login) = self.login_state.take() {
            if let Some(messenger) = self.provider_to_messenger_mut(login.provider) {
                messenger.cancel_login().await;
            }

            // Cancelling an in-progress WhatsApp pairing that never
            // authenticated disables the provider again (its QR kept coming
            // from the still-running bot). This keeps the provider disabled
            // until authentication actually completes.
            if login.provider == Provider::WhatsApp
                && login.provider.is_enabled(&self.config.providers)
            {
                login.provider.toggle_enabled(&mut self.config.providers);

                if let Err(e) = self.config.save_config() {
                    self.create_popup(PopupKind::Error(format!("could not save config: {e}")));
                }
            }
        }
        self.screen = Screen::Main;
    }

    pub fn handle_backend_event(&mut self, provider: Provider, event: BackendEvent) {
        match event {
            BackendEvent::Connected => {
                info!(?provider, "Backend connected");

                // A newly authenticated provider becomes usable: close any
                // active pairing/login screen and enable it in config. The
                // chat-list rebuild is kicked off from the TUI event loop,
                // which owns the UI channel needed to deliver the result.
                if provider == Provider::WhatsApp {
                    let was_pairing = self
                        .login_state
                        .as_ref()
                        .is_some_and(|ls| ls.provider == provider);

                    if was_pairing {
                        self.login_state = None;
                        self.screen = Screen::Main;
                    }

                    self.backend_status = None;

                    if !provider.is_enabled(&self.config.providers) {
                        provider.toggle_enabled(&mut self.config.providers);

                        match self.config.save_config() {
                            Ok(()) => {}
                            Err(e) => self.create_popup(PopupKind::Error(e.to_string())),
                        };
                    }
                }
            }
            BackendEvent::Status(status) => {
                // TODO: make this auto dissapear
                self.backend_status = (!status.is_empty()).then_some(status);
            }
            BackendEvent::Disconnected(message) => {
                warn!(message, "Backend disconnected");
                self.create_popup(PopupKind::Error(message));
            }
            BackendEvent::QrCode(code) => {
                info!("WhatsApp QR code received");

                // if wp qr pair ok, remove login screen
                // TODO: check if there's a better way to do this
                match code.as_str() {
                    "ok" => {
                        self.login_state = None;
                        self.screen = Screen::Main;
                        self.config.providers.whatsapp = true;
                        let _ = self.config.save_config().map_err(|err| {
                            self.create_popup(PopupKind::Error(format!(
                                "Error saving config after Qr code pair {err}"
                            )));
                        });
                    }
                    _ => {
                        self.backend_status =
                            Some("Scan this WhatsApp QR code with your phone".to_string());
                    }
                }

                // Pairing is shown through the login screen (which reads the QR
                // payload live each frame). Only surface it when the provider is
                // currently enabled (i.e. the user is pairing) and we are not
                // already sitting on its pairing screen.
                //
                // Once the user cancels pairing, `cancel_login` disables the
                // provider, so a late/refresh QrCode event must not yank them
                // back into the login screen.
                let already_showing = self
                    .login_state
                    .as_ref()
                    .is_some_and(|ls| ls.provider == provider);
                if provider == Provider::WhatsApp
                    && provider.is_enabled(&self.config.providers)
                    && !already_showing
                {
                    let _ = self.start_login(provider);
                }
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
                    // FIX: don't change selected idx when new messages come
                    self.chat_state.message_list_state.select(Some(len - 1));
                }

                let open_chat_id = self
                    .chat_state
                    .open_chat
                    .as_ref()
                    .map(|o| o.chat.id.clone());

                let previous_unread_count = self
                    .chat_state
                    .chats
                    .iter()
                    .find(|chat| chat.id == message.chat)
                    .map(|chat| chat.unread_count)
                    .unwrap_or(0);

                let chatlist_row = self.chat_state.chats.iter().find(|c| c.id == message.chat);
                let chatlist_found = chatlist_row.is_some();
                let existing_name = chatlist_row
                    .map(|c| c.contact_name.clone())
                    .filter(|n| !n.is_empty() && n != "Unknown" && n != "You");

                // Messages never rename an existing chat list row: for a group
                // chat the sender is a member, so the member's name would
                // clobber the group title on every inbound message. The
                // backend owns titles (ChatList/ChatUpdated); only genuinely
                // new chats get a sender-derived fallback so something
                // renders until the first authoritative list arrives.
                let contact_name = existing_name.unwrap_or_else(|| {
                    if message.sender == "Unknown" {
                        String::new()
                    } else {
                        message.sender.clone()
                    }
                });

                let mut chat_update = Chat {
                    id: message.chat.clone(),
                    contact_name: contact_name.clone(),
                    last_message_ts: Some(message.timestamp),
                    ..Default::default()
                };

                if !message.from_me && !is_open {
                    chat_update.unread = true;
                    chat_update.unread_count = previous_unread_count.saturating_add(1);
                }

                // Guard against polluting the chat list with a brand-new chat whose
                // sender name we cannot resolve (sender came through as
                // "Unknown" -> empty). Such a chat would only render as an
                // "Unnamed chat" and, when the provider is mid-lifecycle, can
                // appear spuriously. Existing chats still get their preview
                // bumped; only genuinely-new nameless entries are suppressed.
                if chatlist_found || !contact_name.is_empty() {
                    self.chat_state.upsert_chat(chat_update);
                }

                if is_open {
                    self.chat_state.mark_read(&message.chat);
                }

                let text_preview: String = message.text.chars().take(60).collect();
                debug!(
                    chat = ?message.chat,
                    from_me = message.from_me,
                    is_open,
                    chatlist_found,
                    open_chat_id = ?open_chat_id,
                    text_preview = %text_preview,
                    "TUI MessageReceived",
                );

                if !message.from_me && !is_open {
                    debug!(chat = ?message.chat, "TUI set unread");
                }

                if !chatlist_found {
                    debug!(chat = ?message.chat, "TUI MessageReceived: inserted chat list entry from push event");
                }
            }

            BackendEvent::MessageUpdated(message) => {
                self.chat_state.update_message(message.clone());
                self.refresh_chat_list_timestamp(&message.chat);
            }

            BackendEvent::MessageDeleted { chat, message_ids } => {
                let Some(chat) = chat else {
                    return;
                };

                for message_id in message_ids {
                    self.chat_state.remove_message(&message_id);
                }

                self.refresh_chat_list_timestamp(&chat);

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

            BackendEvent::UnreadUpdated {
                chat,
                unread,
                unread_count,
            } => {
                let is_open = self.chat_state.is_open(&chat);

                if let Some((_, entry)) = self.chat_state.find_mut(&chat) {
                    if is_open {
                        entry.unread = false;
                        entry.unread_count = 0;
                    } else {
                        entry.unread = unread;
                        entry.unread_count = unread_count;
                    }
                    self.persist_chats();
                }
            }

            BackendEvent::ChatUpdated(chat) => {
                debug!(chat = ?chat.id, "TUI ChatUpdated");

                if let Some(open) = self.chat_state.open_chat.as_mut()
                    && open.chat.id == chat.id
                {
                    open.chat.status = chat.status.clone();
                }

                self.chat_state.upsert_chat(chat);
            }
            BackendEvent::ChatRemoved { chat } => {
                debug!(chat = ?chat.id, "TUI ChatRemoved");

                self.chat_state.chats.retain(|c| c.id != chat.id);
                self.chat_state.refresh_search();

                if let Some(open) = self.chat_state.open_chat.as_mut()
                    && open.chat.id == chat.id
                {
                    self.chat_state.open_chat = None;
                }

                self.persist_chats();
            }
            BackendEvent::ChatList(chats) => {
                debug!(provider = ?provider, total = chats.len(), "TUI ChatList applied");

                if self.chat_state.reconcile_provider_chats(provider, chats) {
                    self.persist_chats();
                }
            }
        }
    }
}

impl AppState {
    fn refresh_chat_list_timestamp(&mut self, chat_id: &ChatId) {
        let latest = self
            .chat_state
            .open_chat
            .as_ref()
            .filter(|open| open.chat.id == *chat_id)
            .and_then(|open| open.history.last())
            .map(|message| message.timestamp);

        let Some((_, chat)) = self.chat_state.find_mut(chat_id) else {
            return;
        };

        let Some(ts) = latest else {
            return;
        };

        // Never regress: an edit to a non-newest message (or a non-open chat)
        // must not wipe or lower the row's recency timestamp.
        if chat.last_message_ts.is_none_or(|since| ts > since) {
            chat.last_message_ts = Some(ts);
            let selected_id = self.chat_state.selected_chat().map(|chat| chat.id.clone());
            self.chat_state.sort_pinned_then_recent();

            if let Some(selected_id) = selected_id {
                let selected = self
                    .chat_state
                    .chats
                    .iter()
                    .position(|chat| chat.id == selected_id);
                self.chat_state.chat_list_state.select(selected);
            }
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
            warn!("Provider {:#?} disabled! Skipping chat fetch", provider);
            continue;
        }

        if !messenger.is_authenticated().await {
            warn!(
                "Skipping chat fetch for unauthorized provider {}",
                provider.name()
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
    use ratatui::crossterm::event::{KeyCode, KeyEvent};
    use tokio::sync::broadcast;

    /// Test helper: synchronously open the chat at the given *visible*
    /// chat-list index, loading history through the messenger directly. Mirrors
    /// the async open flow the production event loop drives via
    /// `begin_chat_load` + `apply_chat_load`.
    async fn select_chat(state: &mut AppState, index: usize) {
        let Some(src) = state.chat_state.search.map_to_source(index) else {
            return;
        };
        let Some(chat) = state.chat_state.chats.get(src).cloned() else {
            return;
        };

        debug!(chat = %chat.contact_name, id = ?chat.id, "Opening chat");

        // Save current draft and message selection before switching
        if let Some(current_chat_id) = state
            .chat_state
            .open_chat
            .as_ref()
            .map(|o| o.chat.id.clone())
        {
            let text = state.write.lines().join("\n");
            state.chat_state.save_draft(&current_chat_id, text);
            state.chat_state.save_message_selection();
        }

        let read_result = match state.chat_owner_mut(&chat.id) {
            Some(messenger) => messenger.set_read(&chat.id).await,
            None => Ok(()),
        };

        if let Err(e) = read_result {
            warn!(chat = %chat.contact_name, error = %e, "Failed to mark chat as read");
            state.create_popup(PopupKind::Error(format!(
                "{} failed to mark chat as read: {e}",
                chat.id.platform()
            )));
            return;
        }

        state.chat_state.chats[src].unread = false;
        state.chat_state.chats[src].unread_count = 0;

        let status = match state.chat_owner(&chat.id) {
            Some(messenger) => messenger.status(&chat.id).await.ok().flatten(),
            None => None,
        };

        if let Some(status) = status.clone() {
            state.chat_state.chats[src].status = Some(status.clone());
        }

        let history_result = match state.chat_owner(&chat.id) {
            Some(messenger) => messenger.history(&chat.id).await,
            None => Ok(Vec::new()),
        };
        match history_result {
            Ok(messages) => {
                let history_len = messages.len();
                debug!(chat = %chat.contact_name, messages = history_len, "History loaded");

                state.chat_state.open_chat = Some(OpenChat {
                    chat: Chat {
                        status: status.clone(),
                        ..chat.clone()
                    },
                    history: messages,
                    has_more_history: true,
                });

                state.chat_state.restore_message_selection(history_len);
            }
            Err(e) => {
                error!(chat = %chat.contact_name, error = %e, "Failed to load history");
                state.chat_state.open_chat = None;
                state.create_popup(PopupKind::Error(format!(
                    "{} failed to load history for {}: {e}",
                    chat.id.platform(),
                    chat.contact_name
                )));
            }
        }

        // Load draft for the new chat
        state.write.clear();
        if let Some(draft) = state.chat_state.load_draft(&chat.id) {
            state.write.insert_str(draft);
        }

        state.focus = Focus::Chat;
    }

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

    /// Build an `AppState` hosting both a Telegram and a WhatsApp mock, with the
    /// given `whatsapp` enabled flag. Both mocks are authenticated by default.
    async fn dual_provider_state(whatsapp: bool) -> AppState {
        let mut config = Config::default();
        config.providers.telegram.enabled = true;
        config.providers.whatsapp = whatsapp;
        let keymap = config.keys.parse().unwrap();
        let tg = MockMessenger::new("Telegram");
        tg.spawn_incoming_messages();
        let wa = MockMessenger::new("WhatsApp");
        wa.spawn_incoming_messages();
        let messengers = vec![
            MessengerKind::Stub(Box::new(tg)),
            MessengerKind::Stub(Box::new(wa)),
        ];
        let mut app = AppState::new(config, keymap, messengers, false).await;
        fetch_and_apply(&mut app).await;
        app
    }

    #[tokio::test]
    async fn fetch_all_chats_excludes_disabled_whatsapp() {
        let state = dual_provider_state(/* whatsapp */ false).await;
        assert!(
            state
                .chat_state
                .chats
                .iter()
                .all(|c| c.id.to_provider() == Provider::Telegram),
            "disabled WhatsApp chats must not be fetched"
        );
        assert!(!state.chat_state.chats.is_empty());
    }

    #[tokio::test]
    async fn disabling_whatsapp_removes_only_whatsapp_chats() {
        let mut state = dual_provider_state(/* whatsapp */ true).await;
        assert!(
            state
                .chat_state
                .chats
                .iter()
                .any(|c| c.id.to_provider() == Provider::WhatsApp)
        );

        // Flip the flag off in memory and reconcile — the same rebuild the
        // settings toggle drives — leaving WhatsApp chats out but Telegram in.
        state.config.providers.whatsapp = false;
        let errors = state.rebuild_chats().await;
        assert!(
            errors.is_empty(),
            "reconcile must surface, not hide, errors"
        );
        assert!(
            state
                .chat_state
                .chats
                .iter()
                .all(|c| c.id.to_provider() == Provider::Telegram),
            "only WhatsApp chats should be removed on disable"
        );
        assert!(
            state
                .chat_state
                .chats
                .iter()
                .any(|c| c.id.to_provider() == Provider::Telegram)
        );
    }

    #[tokio::test]
    async fn disabling_whatsapp_closes_an_open_whatsapp_chat() {
        let mut state = dual_provider_state(/* whatsapp */ true).await;
        let wa_idx = state
            .chat_state
            .chats
            .iter()
            .position(|c| c.id.to_provider() == Provider::WhatsApp)
            .expect("expected a WhatsApp chat when enabled");
        state.chat_state.chat_list_state.select(Some(wa_idx));
        select_chat(&mut state, wa_idx).await;
        assert!(
            matches!(
                state.chat_state.open_chat.as_ref().map(|o| &o.chat.id),
                Some(ChatId::WhatsApp(_))
            ),
            "test precondition: a WhatsApp chat should be open"
        );

        // Disable WhatsApp. The toggle closes any open chat belonging to the
        // disabled provider so the UI does not retain a stale conversation.
        state.config.providers.whatsapp = false;
        if let Some(open) = &state.chat_state.open_chat
            && open.chat.id.to_provider() == Provider::WhatsApp
        {
            state.write.clear();
            state.chat_state.open_chat = None;
        }
        state.rebuild_chats().await;

        assert!(
            state.chat_state.open_chat.is_none(),
            "an open WhatsApp chat must be closed when WhatsApp is disabled"
        );
        assert!(
            state
                .chat_state
                .chats
                .iter()
                .all(|c| c.id.to_provider() == Provider::Telegram),
            "WhatsApp chats must be removed after disabling"
        );
    }

    #[tokio::test]
    async fn apply_history_page_prepends_older_and_filters_duplicates() {
        let mut state = app_state().await;
        state.config.providers.whatsapp = false;
        state.chat_state.chat_list_state.select(Some(0));
        select_chat(&mut state, 0).await;

        let chat_id = state.chat_state.open_chat.as_ref().unwrap().chat.id.clone();
        let before = state.chat_state.open_chat.as_ref().unwrap().history.len();
        let first_existing = state
            .chat_state
            .open_chat
            .as_ref()
            .unwrap()
            .history
            .first()
            .map(|m| m.message_id.clone())
            .expect("opened chat must have history");

        let older = vec![
            Message {
                message_id: "older-1".into(),
                chat: chat_id.clone(),
                sender: "Alice".into(),
                author_id: None,
                text: "older still".into(),
                timestamp: -1000,
                from_me: false,
                msg_actions: Vec::new(),
                media: None,
                reply_to_id: None,
                reply_ctx: None,
                pending: false,
                failed: false,
            },
            // A duplicate of an already-cached message must be filtered away.
            Message {
                message_id: first_existing,
                chat: chat_id.clone(),
                sender: "Alice".into(),
                author_id: None,
                text: "duplicate".into(),
                timestamp: -1000,
                from_me: false,
                msg_actions: Vec::new(),
                media: None,
                reply_to_id: None,
                reply_ctx: None,
                pending: false,
                failed: false,
            },
        ];

        state.apply_history_page(&chat_id, Ok(older));

        let open = state.chat_state.open_chat.as_ref().unwrap();
        assert!(
            open.has_more_history,
            "a non-empty page keeps lazy loading enabled"
        );
        assert_eq!(open.history.len(), before + 1, "duplicate must be filtered");
        assert_eq!(
            open.history.first().map(|m| &m.message_id),
            Some(&MessageId("older-1".into())),
            "the older page is stitched in front"
        );
        assert_eq!(
            state.chat_state.message_list_state.selected(),
            Some(1),
            "selection lands on the first message of the freshly prepended page"
        );
    }

    #[tokio::test]
    async fn apply_history_page_empty_or_error_disables_lazy_load() {
        let mut state = app_state().await;
        state.config.providers.whatsapp = false;
        state.chat_state.chat_list_state.select(Some(0));
        select_chat(&mut state, 0).await;

        let chat_id = state.chat_state.open_chat.as_ref().unwrap().chat.id.clone();
        let len = state.chat_state.open_chat.as_ref().unwrap().history.len();

        state.apply_history_page(&chat_id, Ok(Vec::new()));
        assert!(
            !state
                .chat_state
                .open_chat
                .as_ref()
                .unwrap()
                .has_more_history,
            "an empty page means the server has no older messages"
        );
        assert_eq!(
            state.chat_state.open_chat.as_ref().unwrap().history.len(),
            len,
            "an empty page must not alter the history"
        );

        state
            .chat_state
            .open_chat
            .as_mut()
            .unwrap()
            .has_more_history = true;
        state.apply_history_page(&chat_id, Err(BackendError::Other("expired".into())));
        assert!(
            !state
                .chat_state
                .open_chat
                .as_ref()
                .unwrap()
                .has_more_history,
            "a failed fetch also stops lazy loading for this chat"
        );
    }

    #[tokio::test]
    async fn apply_history_page_ignores_results_for_another_chat() {
        let mut state = app_state().await;
        state.config.providers.whatsapp = false;
        state.chat_state.chat_list_state.select(Some(0));
        select_chat(&mut state, 0).await;

        let len = state.chat_state.open_chat.as_ref().unwrap().history.len();
        let other_id = ChatId::Telegram(999_999);

        state.apply_history_page(
            &other_id,
            Ok(vec![Message {
                message_id: "stale".into(),
                chat: other_id.clone(),
                sender: "Alice".into(),
                author_id: None,
                text: "for another chat".into(),
                timestamp: -1000,
                from_me: false,
                msg_actions: Vec::new(),
                media: None,
                reply_to_id: None,
                reply_ctx: None,
                pending: false,
                failed: false,
            }]),
        );

        let open = state.chat_state.open_chat.as_ref().unwrap();
        assert_eq!(
            open.history.len(),
            len,
            "stale result must not touch the open chat"
        );
        assert!(
            open.has_more_history,
            "a result for another chat cannot turn off lazy loading"
        );
    }

    #[tokio::test]
    async fn boot_has_no_selection_and_empty_pane() {
        let state = app_state().await;
        assert_eq!(state.chat_state.chats.len(), 4);
        assert!(state.selected_chat_idx().is_none());
        assert!(state.chat_state.open_chat.is_none());
    }

    #[tokio::test]
    async fn applying_chats_deduplicates_chat_ids() {
        let mut state = app_state().await;
        let duplicate = state.chat_state.chats[0].clone();
        let duplicate_id = duplicate.id.clone();
        // Snapshot at least as large as the loaded set so the grow-only guard
        // does not kick in; the injected duplicate must still be collapsed.
        let snapshot: Vec<_> = state
            .chat_state
            .chats
            .iter()
            .cloned()
            .chain([duplicate])
            .collect();

        state.apply_chats(snapshot);

        assert_eq!(state.chat_state.chats.len(), 4);
        assert_eq!(
            state
                .chat_state
                .chats
                .iter()
                .filter(|chat| chat.id == duplicate_id)
                .count(),
            1,
            "duplicate chat id must be deduplicated"
        );
    }

    #[tokio::test]
    async fn opening_a_chat_clears_the_committed_search_filter() {
        let mut state = app_state().await;
        state.apply_chats(vec![
            crate::backend::Chat {
                id: ChatId::Telegram(1),
                contact_name: "Alpha".into(),
                ..Default::default()
            },
            crate::backend::Chat {
                id: ChatId::Telegram(2),
                contact_name: "Bob".into(),
                ..Default::default()
            },
            crate::backend::Chat {
                id: ChatId::Telegram(3),
                contact_name: "Charlie".into(),
                ..Default::default()
            },
        ]);

        state.chat_state.begin_search();
        state.chat_state.search_input(KeyEvent::from(KeyCode::Char('b')));
        state.chat_state.commit_search();

        assert!(state.chat_list_search_active());
        assert_eq!(state.chat_state.visible_indices().len(), 1, "only Bob matches");
        assert_eq!(
            state.chat_state.selected_chat().map(|c| c.contact_name.as_str()),
            Some("Bob")
        );

        // Opening the highlighted match (visible index 0 -> source index 1)
        // must drop the filter and re-select the opened chat in the full list.
        assert_eq!(
            state.chat_state.resolve_open_index(0),
            Some(1),
            "visible index resolves to the matching source chat"
        );
        assert!(
            !state.chat_state.search.is_active(),
            "opening a chat must clear the active search"
        );
        assert_eq!(state.chat_state.chats.len(), 3, "full list restored");
        assert_eq!(
            state.selected_chat_idx(),
            Some(1),
            "opened chat re-selected by id after the filter drops"
        );
        assert_eq!(
            state.chat_state.selected_chat().map(|c| c.contact_name.as_str()),
            Some("Bob")
        );
    }

    #[tokio::test]
    async fn apply_chats_prunes_stale_rows_missing_from_live_snapshot() {
        let mut state = app_state().await;
        state.config.providers.whatsapp = true;
        let original_ids: Vec<_> = state
            .chat_state
            .chats
            .iter()
            .map(|chat| chat.id.clone())
            .collect();
        assert!(
            original_ids.len() > 1,
            "test precondition: a populated chat list"
        );

        // Two WhatsApp rows: one the backend still reports, one it merged away
        // (simulates a stale LID twin the live snapshot no longer carries).
        state.chat_state.chats.push(crate::backend::Chat {
            id: ChatId::WhatsApp("wa-keep".into()),
            contact_name: "WA Keep".into(),
            ..Default::default()
        });
        state.chat_state.chats.push(crate::backend::Chat {
            id: ChatId::WhatsApp("wa-orphan".into()),
            contact_name: "WA Orphan".into(),
            ..Default::default()
        });

        // The snapshot is non-empty and includes WhatsApp: the stale row the
        // provider no longer reports is pruned, while original rows are kept.
        let fresh: Vec<_> = original_ids
            .iter()
            .map(|id| crate::backend::Chat {
                id: id.clone(),
                contact_name: "refresh".into(),
                ..Default::default()
            })
            .chain(std::iter::once(crate::backend::Chat {
                id: ChatId::WhatsApp("wa-keep".into()),
                contact_name: "WA Keep".into(),
                ..Default::default()
            }))
            .collect();
        state.apply_chats(fresh);

        // Original Telegram rows must survive a live WhatsApp snapshot.
        for id in &original_ids {
            assert!(
                state.chat_state.chats.iter().any(|c| &c.id == id),
                "original chat {id:?} must survive"
            );
        }
        assert!(
            state
                .chat_state
                .chats
                .iter()
                .any(|c| c.id == ChatId::WhatsApp("wa-keep".into())),
            "a row present in the live snapshot must survive"
        );
        assert!(
            !state
                .chat_state
                .chats
                .iter()
                .any(|c| c.id == ChatId::WhatsApp("wa-orphan".into())),
            "a WhatsApp row absent from a live WhatsApp snapshot must be pruned"
        );
    }

    #[tokio::test]
    async fn cold_start_keeps_enabled_provider_chats_when_live_fetch_empty() {
        let mut state = app_state().await;
        state.config.providers.whatsapp = true;
        state.chat_state.chats.push(crate::backend::Chat {
            id: ChatId::WhatsApp("wa-1".into()),
            contact_name: "WA One".into(),
            ..Default::default()
        });
        state.chat_state.chats.push(crate::backend::Chat {
            id: ChatId::WhatsApp("wa-2".into()),
            contact_name: "WA Two".into(),
            ..Default::default()
        });

        // The live fetch yields only Telegram chats: there is no WhatsApp
        // messenger connected (WhatsApp does not re-sync its history on a cold
        // restart), so the persisted WhatsApp chats must be preserved.
        let (telegram_only, _errors) =
            fetch_all_chats(&state.messengers, &state.config.providers).await;
        assert!(
            telegram_only
                .iter()
                .all(|c| c.id.to_provider() == Provider::Telegram),
            "test precondition: live fetch has no WhatsApp chats"
        );

        state.apply_chats(telegram_only);

        assert!(
            state
                .chat_state
                .chats
                .iter()
                .any(|c| c.id == ChatId::WhatsApp("wa-1".into())),
            "persisted WhatsApp chats must survive a cold start without a live sync"
        );
        assert!(
            state
                .chat_state
                .chats
                .iter()
                .any(|c| c.id == ChatId::WhatsApp("wa-2".into()))
        );
    }

    #[tokio::test]
    async fn apply_fetched_drops_disabled_provider_chats() {
        let mut state = app_state().await;
        state.chat_state.chats.push(crate::backend::Chat {
            id: ChatId::WhatsApp("wa-1".into()),
            contact_name: "WA One".into(),
            ..Default::default()
        });
        state.config.providers.whatsapp = false;

        let (telegram_only, _errors) =
            fetch_all_chats(&state.messengers, &state.config.providers).await;
        state.apply_fetched(telegram_only, Vec::new());

        assert!(
            state
                .chat_state
                .chats
                .iter()
                .all(|c| c.id.to_provider() == Provider::Telegram),
            "disabled providers' chats must be dropped"
        );
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
        select_chat(&mut state, 0).await;
        assert_eq!(state.chat_state.chats[0].scroll, 0);
        state.chat_state.chats[0].scroll = 10;

        state.chat_state.chat_list_state.select(Some(1));
        select_chat(&mut state, 1).await;
        assert_eq!(state.chat_state.chats[1].scroll, 0);

        state.chat_state.chat_list_state.select(Some(0));
        select_chat(&mut state, 0).await;
        assert_eq!(state.chat_state.chats[0].scroll, 10);
    }

    #[tokio::test]
    async fn open_chat_tracks_the_opened_conversation() {
        let mut state = app_state().await;

        state.chat_state.chat_list_state.select(Some(1));
        select_chat(&mut state, 1).await;

        let current = state.chat_state.open_chat.unwrap();
        assert_eq!(current.chat.id, ChatId::Telegram(102));
        assert!(!current.history.is_empty());
    }

    #[tokio::test]
    async fn incoming_message_updates_open_chat_without_unread() {
        let mut state = app_state().await;

        state.chat_state.chat_list_state.select(Some(0));
        select_chat(&mut state, 0).await;

        assert_eq!(
            state.chat_state.open_chat.as_ref().unwrap().history.len(),
            2
        );
        assert!(!state.chat_state.chats[0].unread);
        assert_eq!(state.chat_state.chats[0].unread_count, 0);

        state.handle_backend_event(
            Provider::Telegram,
            BackendEvent::MessageReceived(Message {
                message_id: "incoming".into(),
                chat: ChatId::Telegram(103),
                sender: "Alice".into(),
                author_id: None,
                text: "breaking".into(),
                timestamp: 1000,
                from_me: false,
                msg_actions: Vec::new(),
                media: None,
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
        assert_eq!(state.chat_state.chats[0].last_message_ts, Some(1000));
        assert!(!state.chat_state.chats[0].unread);
    }

    #[tokio::test]
    async fn incoming_message_to_closed_chat_marks_unread() {
        let mut state = app_state().await;
        state.chat_state.chat_list_state.select(Some(0));
        select_chat(&mut state, 0).await;

        state.handle_backend_event(
            Provider::Telegram,
            BackendEvent::MessageReceived(Message {
                message_id: "incoming".into(),
                chat: ChatId::Telegram(101),
                sender: "Alice".into(),
                author_id: None,
                text: "hi".into(),
                timestamp: 0,
                from_me: false,
                msg_actions: Vec::new(),
                media: None,
                reply_to_id: None,
                reply_ctx: None,
                pending: false,
                failed: false,
            }),
        );

        let chat = state
            .chat_state
            .chats
            .iter()
            .find(|chat| chat.id == ChatId::Telegram(101))
            .expect("incoming chat remains in chat list");
        assert!(chat.unread);
        assert_eq!(chat.unread_count, 1);
    }

    #[tokio::test]
    async fn unread_update_does_not_mark_open_chat_unread() {
        let mut state = app_state().await;
        state.chat_state.chat_list_state.select(Some(0));
        select_chat(&mut state, 0).await;

        state.handle_backend_event(
            Provider::Telegram,
            BackendEvent::UnreadUpdated {
                chat: ChatId::Telegram(103),
                unread: true,
                unread_count: 4,
            },
        );

        let chat = state
            .chat_state
            .chats
            .iter()
            .find(|chat| chat.id == ChatId::Telegram(103))
            .expect("open chat remains in chat list");
        assert!(!chat.unread);
        assert_eq!(chat.unread_count, 0);
    }

    #[tokio::test]
    async fn status_update_refreshes_chat_list_and_open_chat() {
        let mut state = app_state().await;
        state.chat_state.chat_list_state.select(Some(0));
        select_chat(&mut state, 0).await;

        state.handle_backend_event(
            Provider::Telegram,
            BackendEvent::ChatUpdated(Chat {
                id: ChatId::Telegram(103),
                status: Some("last seen 5m ago".into()),
                ..Default::default()
            }),
        );

        assert_eq!(
            state.chat_state.chats[0].status.as_deref(),
            Some("last seen 5m ago")
        );
        assert_eq!(
            state
                .chat_state
                .open_chat
                .as_ref()
                .and_then(|open| open.chat.status.as_deref()),
            Some("last seen 5m ago")
        );
    }

    #[tokio::test]
    async fn closed_chat_status_update_preserves_existing_chat_list_metadata() {
        let mut state = app_state().await;
        let original_name = state.chat_state.chats[1].contact_name.clone();
        let original_ts = state.chat_state.chats[1].last_message_ts;

        state.handle_backend_event(
            Provider::Telegram,
            BackendEvent::ChatUpdated(Chat {
                id: ChatId::Telegram(102),
                status: Some("online".into()),
                ..Default::default()
            }),
        );

        let chat = &state.chat_state.chats[1];
        assert_eq!(chat.contact_name, original_name);
        assert_eq!(chat.last_message_ts, original_ts);
        assert_eq!(chat.status.as_deref(), Some("online"));
    }

    #[tokio::test]
    async fn incoming_message_to_unknown_chat_inserts_chat_list_entry() {
        let mut state = app_state().await;
        let before = state.chat_state.chats.len();

        state.handle_backend_event(
            Provider::Telegram,
            BackendEvent::MessageReceived(Message {
                message_id: "new-chat".into(),
                chat: ChatId::Telegram(999),
                sender: "New contact".into(),
                author_id: None,
                text: "hello there".into(),
                timestamp: 1000,
                from_me: false,
                msg_actions: Vec::new(),
                media: None,
                reply_to_id: None,
                reply_ctx: None,
                pending: false,
                failed: false,
            }),
        );

        assert_eq!(state.chat_state.chats.len(), before + 1);
        assert_eq!(state.chat_state.chats[0].id, ChatId::Telegram(999));
        assert_eq!(state.chat_state.chats[0].last_message_ts, Some(1000));
        assert!(state.chat_state.chats[0].unread);
        assert_eq!(state.chat_state.chats[0].unread_count, 1);
    }

    #[tokio::test]
    async fn message_never_renames_existing_chat_list_chat_to_sender() {
        let mut state = app_state().await;

        state.chat_state.upsert_chat(Chat {
            id: ChatId::Telegram(777),
            contact_name: "Car Budget".into(),
            ..Default::default()
        });

        // Group-style message: the sender is a member, not the chat itself.
        state.handle_backend_event(
            Provider::Telegram,
            BackendEvent::MessageReceived(Message {
                message_id: "g-1".into(),
                chat: ChatId::Telegram(777),
                sender: "A Member".into(),
                author_id: None,
                text: "updated".into(),
                timestamp: 0,
                from_me: false,
                msg_actions: Vec::new(),
                media: None,
                reply_to_id: None,
                reply_ctx: None,
                pending: false,
                failed: false,
            }),
        );

        let chat = state
            .chat_state
            .chats
            .iter()
            .find(|c| c.id == ChatId::Telegram(777))
            .expect("existing chat stays in chat list");
        assert_eq!(
            chat.contact_name, "Car Budget",
            "a message sender must never rename the row title"
        );
        assert_eq!(chat.last_message_ts, Some(0));
        assert_eq!(chat.unread_count, 1);
    }

    #[tokio::test]
    async fn unknown_sender_message_does_not_insert_nameless_chat_list_chat() {
        let mut state = app_state().await;
        let before = state.chat_state.chats.len();

        // A brand-new chat with an unresolved ("Unknown") sender must not
        // appear as an "Unnamed chat" entry in the chat list.
        state.handle_backend_event(
            Provider::Telegram,
            BackendEvent::MessageReceived(Message {
                message_id: "transient".into(),
                chat: ChatId::Telegram(4242),
                sender: "Unknown".into(),
                author_id: None,
                text: "system ping".into(),
                timestamp: 0,
                from_me: false,
                msg_actions: Vec::new(),
                media: None,
                reply_to_id: None,
                reply_ctx: None,
                pending: false,
                failed: false,
            }),
        );

        assert_eq!(
            state.chat_state.chats.len(),
            before,
            "an unresolved sender must not create a nameless chat list chat"
        );
        assert!(
            state
                .chat_state
                .chats
                .iter()
                .all(|c| c.id != ChatId::Telegram(4242)),
            "no entry should exist for the unknown-chat id"
        );
    }

    #[tokio::test]
    async fn message_deleted_event_removes_open_history_message() {
        let mut state = app_state().await;
        state.chat_state.chat_list_state.select(Some(0));
        select_chat(&mut state, 0).await;

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
        select_chat(&mut state, 0).await;
        state.write.insert_str("hello from chat 0");

        state.chat_state.chat_list_state.select(Some(1));
        select_chat(&mut state, 1).await;

        assert_eq!(
            state.chat_state.load_draft(&ChatId::Telegram(103)),
            Some("hello from chat 0")
        );
    }

    #[tokio::test]
    async fn draft_is_loaded_when_selecting_chat() {
        let mut state = app_state().await;

        state.chat_state.chat_list_state.select(Some(0));
        select_chat(&mut state, 0).await;
        state.write.insert_str("draft message");

        state.chat_state.chat_list_state.select(Some(1));
        select_chat(&mut state, 1).await;

        state.chat_state.chat_list_state.select(Some(0));
        select_chat(&mut state, 0).await;

        assert_eq!(state.write.lines().join("\n"), "draft message");
    }

    #[tokio::test]
    async fn draft_is_cleared_on_exit() {
        let mut state = app_state().await;

        state.chat_state.chat_list_state.select(Some(0));
        select_chat(&mut state, 0).await;
        state.write.insert_str("unsent text");

        state.focus = Focus::Chat;
        state.cycle_focus();

        assert!(state.write.lines().join("\n").is_empty());
        assert_eq!(
            state.chat_state.load_draft(&ChatId::Telegram(103)),
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
        select_chat(&mut state, 0).await;
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
        select_chat(&mut state, 0).await;
        state.write.insert_str("msg for chat 0");

        state.chat_state.chat_list_state.select(Some(1));
        select_chat(&mut state, 1).await;
        state.write.insert_str("msg for chat 1");

        state.chat_state.chat_list_state.select(Some(0));
        select_chat(&mut state, 0).await;

        assert_eq!(
            state.chat_state.load_draft(&ChatId::Telegram(103)),
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
                    author_id: None,
                    text: text.to_string(),
                    timestamp: 0,
                    from_me: true,
                    msg_actions: Vec::new(),
                    media: None,
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
        select_chat(&mut state, 0).await;
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
        select_chat(&mut state, 0).await;
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

    #[tokio::test]
    async fn qr_event_opens_login_only_when_provider_is_enabled() {
        let mut config = Config::default();
        config.providers.whatsapp = true;
        let keymap = config.keys.parse().unwrap();
        let wa = MockMessenger::new("WhatsApp");
        wa.spawn_incoming_messages();
        let messengers = vec![MessengerKind::Stub(Box::new(wa))];
        let mut state = AppState::new(config, keymap, messengers, false).await;
        state.chats_loaded = true;

        // Provider enabled + no login_state → QrCode should open login.
        state.handle_backend_event(Provider::WhatsApp, BackendEvent::QrCode("test".into()));
        assert!(
            state.login_state.is_some(),
            "login should open when provider is enabled"
        );

        // Simulate user cancelling pairing (Esc): login_state cleared,
        // provider disabled. A second QrCode must NOT reopen login.
        state.login_state = None;
        state.config.providers.whatsapp = false;
        state.handle_backend_event(Provider::WhatsApp, BackendEvent::QrCode("refresh".into()));
        assert!(
            state.login_state.is_none(),
            "QrCode must not reopen login after cancel (provider disabled)"
        );
    }

    #[tokio::test]
    async fn cancel_login_disables_whatsapp_when_pairing() {
        // Write config to a temp dir to avoid touching the real user config.
        let tmp =
            std::path::PathBuf::from(format!("/tmp/senders-test-cancel-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let old = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: test runs single-threaded within this process;
        // no concurrent reads/writes of XDG_CONFIG_HOME.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &tmp) }

        let mut config = Config::default();
        config.providers.whatsapp = true;
        let keymap = config.keys.parse().unwrap();
        let wa = MockMessenger::new("WhatsApp");
        wa.spawn_incoming_messages();
        let messengers = vec![MessengerKind::Stub(Box::new(wa))];
        let mut state = AppState::new(config, keymap, messengers, false).await;

        // Simulate an active pairing session: login_state set for WhatsApp.
        state.screen = Screen::Login;
        state.login_state = Some(LoginState {
            provider: Provider::WhatsApp,
            step: 0,
            error: None,
            login_input: TextArea::default(),
            submitting: false,
        });

        state.cancel_login().await;

        // Login dismissed.
        assert!(state.login_state.is_none());
        assert!(
            matches!(state.screen, Screen::Main),
            "cancel should return to Main"
        );
        // Provider disabled.
        assert!(
            !state.config.providers.whatsapp,
            "WhatsApp should be disabled after cancel"
        );

        // Verify the config file on disk was also updated.
        let saved = Config::load().unwrap().expect("config should be saved");
        assert!(
            !saved.providers.whatsapp,
            "saved config should have whatsapp disabled"
        );

        // Restore XDG_CONFIG_HOME.
        // SAFETY: see above.
        unsafe {
            match old {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
