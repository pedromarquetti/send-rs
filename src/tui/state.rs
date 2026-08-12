use anyhow::{Context, Result};
use ratatui::style::{Color, Style};
use ratatui::widgets::ListState;
use ratatui_textarea::TextArea;

use crate::backend::{BackendError, BackendEvent, Chat, Message, Messenger};
use crate::config::{Config, Keymap};
use crate::tui::chat::ChatViewState;

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
    pub chat_owner: Vec<usize>,
    pub chat_tags: Vec<&'static str>,
    pub chats: Vec<Chat>,
    /// Selection state for the chat list sidebar.
    pub chat_list_state: ListState,
    /// Scroll state for the chat history view.
    pub chat_view_state: ChatViewState,
    /// Selection state for the settings provider list.
    pub settings_state: ListState,
    pub focus: Focus,
    pub screen: Screen,
    pub history: Vec<Message>,
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
            chat_owner: Vec::new(),
            chat_tags: Vec::new(),
            chats: Vec::new(),
            chat_list_state: ListState::default().with_selected(None),
            chat_view_state: ChatViewState::default(),
            settings_state: ListState::default().with_selected(Some(0)),
            focus: Focus::ChatList,
            screen: if open_settings {
                Screen::Settings
            } else {
                Screen::Main
            },
            history: Vec::new(),
            write,
            overlay: None,
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

    pub fn selected_chat(&self) -> Option<usize> {
        self.chat_list_state.selected()
    }

    pub async fn rebuild_chats(&mut self) -> Result<()> {
        let mut chats = Vec::new();
        let mut owners = Vec::new();
        let mut tags = Vec::new();
        for (index, messenger) in self.messengers.iter().enumerate() {
            let enabled = match messenger.platform() {
                "Telegram" => self.config.providers.telegram,
                "WhatsApp" => self.config.providers.whatsapp,
                _ => true,
            };
            if !enabled {
                continue;
            }
            let tag: &'static str = match messenger.platform() {
                "Telegram" => "TG",
                "WhatsApp" => "WA",
                _ => "??",
            };
            for chat in messenger.chats().await? {
                chats.push(chat);
                owners.push(index);
                tags.push(tag);
            }
        }
        self.chats = chats;
        self.chat_owner = owners;
        self.chat_tags = tags;
        let len = self.chats.len();
        if len == 0 {
            self.chat_list_state.select(None);
        } else {
            let index = self.chat_list_state.selected().unwrap_or(0).min(len - 1);
            self.chat_list_state.select(Some(index));
        }
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
        let Some(chat) = self.chats.get(index).cloned() else {
            return;
        };
        let owner = self.chat_owner[index];

        self.chats[index].unread = false;
        self.chats[index].unread_count = 0;
        let read_result = match self.messengers.get_mut(owner) {
            Some(messenger) => messenger.read(&chat.id).await,
            None => Ok(()),
        };
        if let Err(e) = read_result {
            self.show_error(format!("failed to mark as read: {e}"));
            return;
        }

        let history_result = match self.messengers.get(owner) {
            Some(messenger) => messenger.history(&chat.id).await,
            None => Ok(Vec::new()),
        };
        match history_result {
            Ok(messages) => self.history = messages,
            Err(e) => {
                self.history = Vec::new();
                self.show_error(format!("failed to load history: {e}"));
            }
        }
        self.chat_view_state.scroll = 0;
        self.focus = Focus::Chat;
    }

    pub async fn send_message(&mut self) {
        let text = self.write.lines().join("\n");
        if text.trim().is_empty() {
            return;
        }
        let Some(selected) = self.selected_chat() else {
            self.show_error("no chat selected");
            return;
        };
        if selected >= self.chats.len() {
            self.show_error("no chat selected");
            return;
        }
        let chat = self.chats[selected].clone();
        let owner = self.chat_owner[selected];
        let send_result = match self.messengers.get(owner) {
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
                let current_selection = self.selected_chat().unwrap_or(0);
                let is_current = self
                    .chats
                    .get(current_selection)
                    .map(|chat| chat.id == message.chat)
                    .unwrap_or(false);
                if is_current {
                    self.history.push(message.clone());
                }
                if let Some((index, chat)) = self
                    .chats
                    .iter_mut()
                    .enumerate()
                    .find(|(_, chat)| chat.id == message.chat)
                {
                    chat.last_message = Some(format!("{}: {}", message.sender, message.text));
                    if !message.from_me && index != current_selection {
                        chat.unread = true;
                        chat.unread_count = chat.unread_count.saturating_add(1);
                    }
                }
            }

            BackendEvent::ChatUpdated(chat) => {
                if let Some(entry) = self.chats.iter_mut().find(|entry| entry.id == chat.id) {
                    entry.name = chat.name;
                    entry.last_message = chat.last_message;
                }
            }
        }
    }
}
