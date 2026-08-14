use anyhow::{Context, Result};
use ratatui::style::{Color, Style};
use ratatui::widgets::ListState;
use ratatui_textarea::TextArea;
use std::collections::HashMap;

use crate::backend::{BackendError, BackendEvent, Chat, ChatId, Messenger};
use crate::config::{Config, Keymap};
use crate::tui::chat::{ChatState, OpenChat};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    ChatList,
    Chat,
    Write,
    Overlay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Main,
    Settings,
}

pub enum OverlayKind {
    Info,
    Error,
    Warn,
}

pub struct Overlay {
    pub title: String,
    pub kind: OverlayKind,
    pub message: String,
    prev_focus: Focus,
}

/// Common application state shared across pages.
pub struct AppState {
    pub config: Config,
    pub keymap: Keymap,
    pub messengers: Vec<Box<dyn Messenger>>,

    /// main chat state handler
    pub chat_state: ChatState,

    /// Selection state for the settings provider list.
    pub settings_state: ListState,

    pub focus: Focus,
    pub screen: Screen,
    pub write: TextArea<'static>,
    pub overlay: Option<Overlay>,
    pub running: bool,
}

impl AppState {
    pub async fn new(
        config: Config,
        keymap: Keymap,
        messengers: Vec<Box<dyn Messenger>>,
        open_settings: bool,
    ) -> Result<Self> {
        let mut write = TextArea::default();
        write.set_placeholder_text("Write a message...");
        write.set_cursor_style(Style::default().fg(Color::Yellow));

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
            overlay: None,
            chat_state: Default::default(),
            running: true,
        };
        app.rebuild_chats().await.context("loading chats")?;
        if open_settings {
            app.show_info(
                "Welcome! Press Enter on a provider to enable it. Connections are mocked for now.",
            );
        }
        Ok(app)
    }

    pub fn selected_chat_idx(&self) -> Option<usize> {
        self.chat_state.chat_list_state.selected()
    }

    pub async fn rebuild_chats(&mut self) -> Result<()> {
        let saved_scrolls: HashMap<ChatId, usize> = self
            .chat_state
            .chats
            .iter()
            .map(|chat| (chat.id.clone(), chat.scroll))
            .collect();
        let mut entries = Vec::new();
        for messenger in &self.messengers {
            let enabled = match messenger.platform() {
                "Telegram" => self.config.providers.telegram,
                "WhatsApp" => self.config.providers.whatsapp,
                _ => true,
            };
            if !enabled {
                continue;
            }
            for chat in messenger.chats().await? {
                let scroll = saved_scrolls.get(&chat.id).copied().unwrap_or(0);
                entries.push(Chat { scroll, ..chat });
            }
        }
        self.chat_state.chats = entries;
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
        Ok(())
    }

    pub fn show_info(&mut self, message: impl Into<String>) {
        self.overlay = Some(Overlay {
            title: "Info".into(),
            kind: OverlayKind::Info,
            message: message.into(),
            prev_focus: self.focus,
        });
        self.focus = Focus::Overlay;
    }

    pub fn show_error(&mut self, message: impl Into<String>) {
        self.overlay = Some(Overlay {
            title: "Error".into(),
            kind: OverlayKind::Error,
            message: message.into(),
            prev_focus: self.focus,
        });
        self.focus = Focus::Overlay;
    }

    pub fn dismiss_overlay(&mut self) {
        if let Some(overlay) = self.overlay.take() {
            self.focus = overlay.prev_focus;
        }
    }

    pub fn cycle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::ChatList => Focus::Chat,
            Focus::Chat => Focus::ChatList,
            Focus::Write => Focus::ChatList,
            Focus::Overlay => Focus::ChatList,
        };
    }

    pub async fn select_chat(&mut self, index: usize) {
        let Some(chat) = self.chat_state.chats.get(index).cloned() else {
            return;
        };

        self.chat_state.chats[index].unread = false;
        self.chat_state.chats[index].unread_count = 0;
        let read_result = match self.messenger_for_mut(&chat.id) {
            Some(messenger) => messenger.set_read(&chat.id).await,
            None => Ok(()),
        };
        if let Err(e) = read_result {
            self.show_error(format!("failed to mark as read: {e}"));
            return;
        }

        let history_result = match self.messenger_for(&chat.id) {
            Some(messenger) => messenger.history(&chat.id).await,
            None => Ok(Vec::new()),
        };
        match history_result {
            Ok(messages) => {
                self.chat_state.open_chat = Some(OpenChat {
                    chat: chat.clone(),
                    history: messages,
                });
            }
            Err(e) => {
                self.chat_state.open_chat = None;
                self.show_error(format!("failed to load history: {e}"));
            }
        }
        self.focus = Focus::Chat;
    }

    pub async fn send_message(&mut self) {
        let text = self.write.lines().join("\n");
        if text.trim().is_empty() {
            return;
        }

        let Some(chat) = self.chat_state.selected_chat() else {
            self.show_error("No chat selected");
            return;
        };

        let send_result = match self.messenger_for(&chat.id) {
            Some(messenger) => messenger.send(&chat.id, &text).await,
            None => Err(BackendError::Other("no messenger for this chat".into())),
        };
        match send_result {
            Ok(()) => {
                self.write.clear();
            }
            Err(e) => self.show_error(format!("failed to send: {e}")),
        }
    }

    fn messenger_for<'a>(&'a self, chat: &ChatId) -> Option<&'a dyn Messenger> {
        self.messengers
            .iter()
            .find(|m| m.platform() == chat.platform())
            .map(|m| m.as_ref())
    }

    fn messenger_for_mut<'a>(
        &'a mut self,
        chat: &ChatId,
    ) -> Option<&'a mut (dyn Messenger + 'static)> {
        self.messengers
            .iter_mut()
            .find(|m| m.platform() == chat.platform())
            .map(|m| m.as_mut())
    }

    pub async fn toggle_provider(&mut self, provider_index: usize) {
        let name = match provider_index {
            0 => "Telegram",
            1 => "WhatsApp",
            _ => return,
        };
        let enabled = match provider_index {
            0 => {
                self.config.providers.telegram = !self.config.providers.telegram;
                self.config.providers.telegram
            }
            1 => {
                self.config.providers.whatsapp = !self.config.providers.whatsapp;
                self.config.providers.whatsapp
            }
            _ => return,
        };
        match self.config.save() {
            Ok(()) => match self.rebuild_chats().await {
                Ok(()) => {
                    if enabled {
                        self.show_info(format!("{name} enabled (mock connection)"));
                    } else {
                        self.show_info(format!("{name} disabled"));
                    }
                }
                Err(e) => self.show_error(format!("failed to refresh chats: {e}")),
            },
            Err(e) => self.show_error(format!("could not save config: {e}")),
        }
    }

    pub fn handle_backend_event(&mut self, _messenger_index: usize, event: BackendEvent) {
        match event {
            BackendEvent::Connected => {}
            BackendEvent::Disconnected(message) => self.show_error(message),
            BackendEvent::MessageReceived(message) => {
                let is_open = self.chat_state.push_incoming(message.clone());
                let selected_id = self
                    .selected_chat_idx()
                    .and_then(|i| self.chat_state.chats.get(i))
                    .map(|c| c.id.clone());
                if let Some((_, chat)) = self.chat_state.find_mut(&message.chat) {
                    chat.last_message = Some(format!("{}: {}", message.sender, message.text));
                    if !message.from_me && !is_open && selected_id.as_ref() != Some(&message.chat) {
                        chat.unread = true;
                        chat.unread_count = chat.unread_count.saturating_add(1);
                    }
                }
            }

            BackendEvent::ChatUpdated(chat) => {
                if let Some(entry) = self
                    .chat_state
                    .chats
                    .iter_mut()
                    .find(|entry| entry.id == chat.id)
                {
                    entry.contact_name = chat.contact_name;
                    entry.last_message = chat.last_message;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Message;
    use crate::backend::mock::MockMessenger;

    async fn app_state() -> AppState {
        let mut config = Config::default();
        config.providers.telegram = true;
        let keymap = config.keys.parse().unwrap();
        let mock = MockMessenger::new("Telegram");
        mock.spawn_incoming_messages();
        let messengers: Vec<Box<dyn Messenger>> = vec![Box::new(mock)];
        AppState::new(config, keymap, messengers, false)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn boot_has_no_selection_and_empty_pane() {
        let state = app_state().await;
        assert_eq!(state.chat_state.chats.len(), 4);
        assert!(state.selected_chat_idx().is_none());
        assert!(state.chat_state.open_chat.is_none());
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
            0,
            BackendEvent::MessageReceived(Message {
                id: "incoming".into(),
                chat: ChatId::Telegram(101),
                sender: "Telegram News".into(),
                text: "breaking".into(),
                timestamp: 0,
                from_me: false,
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
            0,
            BackendEvent::MessageReceived(Message {
                id: "incoming".into(),
                chat: ChatId::Telegram(103),
                sender: "Alice".into(),
                text: "hi".into(),
                timestamp: 0,
                from_me: false,
            }),
        );

        assert!(state.chat_state.chats[2].unread);
        assert_eq!(state.chat_state.chats[2].unread_count, 1);
    }
}
