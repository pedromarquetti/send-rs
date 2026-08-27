use std::collections::HashMap;

use ratatui::widgets::ListState;

use crate::backend::{Chat, ChatId, Message};

pub mod chat_list;
pub mod chat_widget;

/// The chat whose history is currently loaded, paired with its messages.
#[derive(Clone, Default)]
pub struct OpenChat {
    pub chat: Chat,
    pub history: Vec<Message>,
}

/// A chat in the sidebar combined with its app-level metadata.
#[derive(Clone, Default)]
pub struct ChatState {
    /// Selection state for the chat list sidebar.
    pub chat_list_state: ListState,
    pub chats: Vec<Chat>,
    /// responsible for keeping track of the actual visible page page on a chat
    pub visible_page: usize,
    /// The chat whose history is currently loaded, if any.
    pub open_chat: Option<OpenChat>,
    /// Per-chat draft messages (unsent text saved when switching chats).
    pub drafts: HashMap<ChatId, String>,
    /// Selection state for the message list in the currently open chat.
    pub message_list_state: ListState,
    /// Persisted message selection index per chat (restored when switching chats).
    pub selected_messages: HashMap<ChatId, usize>,
}

impl ChatState {
    pub fn get_tag(&self) -> Option<&'static str> {
        match self.selected_chat() {
            Some(c) => Some(c.id.tag()),
            None => None,
        }
    }
    pub fn selected_chat_mut(&mut self) -> Option<&mut Chat> {
        self.chat_list_state
            .selected()
            .and_then(move |sel| self.chats.get_mut(sel))
    }

    pub fn selected_chat(&self) -> Option<&Chat> {
        self.chat_list_state
            .selected()
            .and_then(|sel| self.chats.get(sel))
    }

    pub fn is_open(&self, id: &ChatId) -> bool {
        self.open_chat
            .as_ref()
            .map(|open| open.chat.id == *id)
            .unwrap_or(false)
    }

    pub fn push_incoming(&mut self, message: Message) -> bool {
        if self.is_open(&message.chat)
            && let Some(open) = &mut self.open_chat
        {
            open.history.push(message);
            true
        } else {
            false
        }
    }

    pub fn find_mut(&mut self, id: &ChatId) -> Option<(usize, &mut Chat)> {
        if !self.chats.iter().any(|chat| chat.id == *id) {
            let ids: Vec<_> = self.chats.iter().map(|c| format!("{:?}", c.id)).collect();
            tracing::trace!(search = ?id, sidebar = ?ids, "find_mut: chat not found in sidebar");
        }
        self.chats
            .iter_mut()
            .enumerate()
            .find(|(_, chat)| chat.id == *id)
    }

    /// Replace the open chat's history with freshly fetched data, preserving the
    /// current scroll position.
    pub fn refresh_chat_history(&mut self, chat_id: &ChatId, history: Vec<Message>) {
        if let Some(chat) = &mut self.open_chat
            && chat.chat.id == *chat_id
        {
            let old_len = chat.history.len();
            let old_selected = self.message_list_state.selected();
            chat.history = history;
            let new_len = chat.history.len();

            // Preserve the user's current scroll position, clamped to bounds.
            let new_selected = match old_selected {
                Some(idx) if new_len > 0 => Some(idx.min(new_len - 1)),
                _ if new_len > 0 => Some(new_len - 1),
                _ => None,
            };
            self.message_list_state.select(new_selected);
            // If the user was already at the bottom and new messages arrived, follow them.
            if old_len > 0 && old_selected == Some(old_len - 1) && new_len > old_len {
                self.message_list_state.select(Some(new_len - 1));
            }
        }
    }

    /// Save a draft for a chat. Empty/whitespace-only drafts are removed.
    pub fn save_draft(&mut self, id: &ChatId, text: String) {
        if text.trim().is_empty() {
            self.drafts.remove(id);
        } else {
            self.drafts.insert(id.clone(), text);
        }
    }

    /// Load the draft for a chat, or None if there is no draft.
    pub fn load_draft(&self, id: &ChatId) -> Option<&str> {
        self.drafts.get(id).map(|s| s.as_str())
    }

    /// Save the current message selection for the open chat.
    pub fn save_message_selection(&mut self) {
        if let (Some(chat_id), Some(idx)) = (
            self.open_chat.as_ref().map(|o| o.chat.id.clone()),
            self.message_list_state.selected(),
        ) {
            self.selected_messages.insert(chat_id, idx);
        }
    }

    /// Restore the message selection for a chat, defaulting to the last message.
    pub fn restore_message_selection(&mut self, history_len: usize) {
        if history_len == 0 {
            self.message_list_state.select(None);
            return;
        }
        let chat_id = match self.open_chat.as_ref().map(|o| o.chat.id.clone()) {
            Some(id) => id,
            None => return,
        };
        let idx = self
            .selected_messages
            .get(&chat_id)
            .copied()
            .unwrap_or(history_len - 1)
            .min(history_len - 1);
        self.message_list_state.select(Some(idx));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(id: ChatId, name: &str) -> Chat {
        Chat {
            id,
            contact_name: name.into(),
            ..Default::default()
        }
    }

    fn message(chat: ChatId) -> Message {
        Message {
            id: "m".into(),
            chat,
            sender: "Sender".into(),
            text: "hello".into(),
            timestamp: 0,
            from_me: false,
            options: Vec::new(),
        }
    }

    #[test]
    fn is_open_matches_only_the_loaded_chat() {
        let mut state = ChatState::default();
        assert!(!state.is_open(&ChatId::Telegram(1)));
        state.open_chat = Some(OpenChat {
            chat: chat(ChatId::Telegram(1), "A"),
            history: Vec::new(),
        });
        assert!(state.is_open(&ChatId::Telegram(1)));
        assert!(!state.is_open(&ChatId::Telegram(2)));
    }

    #[test]
    fn push_incoming_appends_only_to_open_chat() {
        let mut state = ChatState {
            open_chat: Some(OpenChat {
                chat: chat(ChatId::Telegram(1), "A"),
                history: Vec::new(),
            }),
            ..Default::default()
        };

        assert!(state.push_incoming(message(ChatId::Telegram(1))));
        assert_eq!(state.open_chat.as_ref().unwrap().history.len(), 1);

        assert!(!state.push_incoming(message(ChatId::Telegram(2))));
        assert_eq!(state.open_chat.as_ref().unwrap().history.len(), 1);
    }

    #[test]
    fn find_mut_returns_entry_with_position() {
        let mut state = ChatState {
            chats: vec![
                chat(ChatId::Telegram(1), "A"),
                chat(ChatId::Telegram(2), "B"),
            ],
            ..Default::default()
        };

        let (index, entry) = state.find_mut(&ChatId::Telegram(2)).unwrap();
        assert_eq!(index, 1);
        assert_eq!(entry.contact_name, "B");
        assert!(state.find_mut(&ChatId::Telegram(99)).is_none());
    }

    #[test]
    fn replace_history_preserves_scroll_position() {
        let id = ChatId::Telegram(1);
        let mut state = ChatState {
            open_chat: Some(OpenChat {
                chat: chat(id.clone(), "A"),
                history: vec![message(id.clone()); 10],
            }),
            ..Default::default()
        };

        // Simulate user scrolled to position 3 (not at bottom).
        state.message_list_state.select(Some(3));

        // Replace with 12 messages (2 new ones appended at end).
        let new_history: Vec<Message> = (0..12).map(|_| message(id.clone())).collect();
        state.refresh_chat_history(&id, new_history);

        // Selection should stay at 3 (preserved, since 3 < 12).
        assert_eq!(state.message_list_state.selected(), Some(3));
    }

    #[test]
    fn replace_history_follows_bottom_when_already_at_bottom() {
        let id = ChatId::Telegram(1);
        let mut state = ChatState {
            open_chat: Some(OpenChat {
                chat: chat(id.clone(), "A"),
                history: vec![message(id.clone()); 5],
            }),
            ..Default::default()
        };
        // User is at the bottom (index 4 = len - 1).
        state.message_list_state.select(Some(4));

        // Replace with 7 messages (2 new ones).
        let new_history: Vec<Message> = (0..7).map(|_| message(id.clone())).collect();
        state.refresh_chat_history(&id, new_history);

        // Should follow to the new bottom (index 6).
        assert_eq!(state.message_list_state.selected(), Some(6));
    }

    #[test]
    fn replace_history_noop_for_different_chat() {
        let mut state = ChatState {
            open_chat: Some(OpenChat {
                chat: chat(ChatId::Telegram(1), "A"),
                history: vec![message(ChatId::Telegram(1)); 5],
            }),
            ..Default::default()
        };
        state.message_list_state.select(Some(2));

        // Replace with a different chat id — should be a no-op.
        state.refresh_chat_history(
            &ChatId::Telegram(99),
            vec![message(ChatId::Telegram(99)); 3],
        );

        assert_eq!(state.message_list_state.selected(), Some(2));
        assert_eq!(state.open_chat.as_ref().unwrap().history.len(), 5);
    }

    #[test]
    fn replace_history_clamps_selection_when_history_shrinks() {
        let id = ChatId::Telegram(1);
        let mut state = ChatState {
            open_chat: Some(OpenChat {
                chat: chat(id.clone(), "A"),
                history: vec![message(id.clone()); 10],
            }),
            ..Default::default()
        };
        state.message_list_state.select(Some(8));

        // Replace with only 3 messages — selection should be clamped.
        let new_history: Vec<Message> = (0..3).map(|_| message(id.clone())).collect();
        state.refresh_chat_history(&id, new_history);

        assert_eq!(state.message_list_state.selected(), Some(2));
    }
}
