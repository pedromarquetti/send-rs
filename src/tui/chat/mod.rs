use std::collections::HashMap;

use ratatui::widgets::ListState;

use crate::backend::{Chat, ChatId, Message, MessageAction, MessageId};

pub mod chat_list;
pub mod chat_widget;

/// The chat whose history is currently loaded, paired with its messages.
#[derive(Clone, Default)]
pub struct OpenChat {
    pub chat: Chat,
    pub history: Vec<Message>,
    /// Whether a further older-page fetch is still worth trying. Once a page
    /// request returns empty, the chat is known to be at its oldest available
    /// history in this provider.
    pub has_more_history: bool,
}

/// A chat in the chat list combined with its app-level metadata.
#[derive(Clone, Default)]
pub struct ChatState {
    /// Selection state for the chat list.
    pub chat_list_state: ListState,
    pub chats: Vec<Chat>,
    /// responsible for keeping track of the actual visible page page on a chat
    pub visible_page: usize,
    /// The chat whose history is currently loaded, if any.
    pub open_chat: Option<OpenChat>,

    /// The message being replied to via the write box, if any. Set when the
    /// user initiates a reply; consumed (and cleared) on the next send.
    pub pending_reply: Option<Message>,

    /// The message currently being edited in the write box, if any. When set,
    /// the next `send` issues an edit instead of a new message.
    pub pending_edit: Option<Message>,

    /// Per-chat draft messages (unsent text saved when switching chats).
    pub drafts: HashMap<ChatId, String>,
    /// Selection state for the message list in the currently open chat.
    pub message_list_state: ListState,
    /// Persisted message selection index per chat (restored when switching chats).
    pub selected_messages: HashMap<ChatId, usize>,
}

impl ChatState {
    fn deduplicate_chats(&mut self) {
        let mut seen = std::collections::HashSet::new();
        let mut duplicate_ids = Vec::new();

        self.chats.retain(|chat| {
            if seen.insert(chat.id.clone()) {
                true
            } else {
                duplicate_ids.push(format!("{:?}", chat.id));
                false
            }
        });

        if !duplicate_ids.is_empty() {
            tracing::warn!(
                duplicates = ?duplicate_ids,
                "Removed duplicate chat IDs from chat list state"
            );
        }
    }

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
            // Dedup by id: a sent message may arrive both via our optimistic
            // echo and via the poll/update stream. Never show it twice.
            if open
                .history
                .iter()
                .any(|m| m.message_id == message.message_id)
            {
                return true;
            }
            open.history.push(message);
            true
        } else {
            false
        }
    }

    /// Replace a message in the open chat's history that matches `old_id`
    /// (e.g. swap a local pending echo for the confirmed server message).
    /// Returns `true` if a replacement happened.
    pub fn replace_message(&mut self, old_id: &MessageId, new: Message) -> bool {
        if let Some(open) = &mut self.open_chat {
            if old_id != &new.message_id {
                open.history
                    .retain(|msg| msg.message_id != new.message_id || msg.message_id == *old_id);
            }
            for msg in &mut open.history {
                if msg.message_id == *old_id {
                    *msg = new;
                    return true;
                }
            }
        }
        false
    }

    /// Flip an outgoing message to `failed` (kept, grayed, retryable).
    pub fn mark_failed(&mut self, id: &MessageId) -> bool {
        if let Some(open) = &mut self.open_chat
            && let Some(msg) = open.history.iter_mut().find(|m| m.message_id == *id)
        {
            msg.failed = true;
            msg.pending = false;
            if !msg.msg_actions.contains(&MessageAction::Retry) {
                msg.msg_actions.push(MessageAction::Retry);
            }
            true
        } else {
            false
        }
    }

    /// Remove a message from the open chat's history by id.
    pub fn remove_message(&mut self, id: &MessageId) -> bool {
        if let Some(open) = &mut self.open_chat {
            let before = open.history.len();
            open.history.retain(|m| m.message_id != *id);
            open.history.len() != before
        } else {
            false
        }
    }

    /// Update the text of a message already shown in the open chat.
    pub fn update_message_text(&mut self, id: &MessageId, text: &str) -> bool {
        if let Some(open) = &mut self.open_chat
            && let Some(message) = open
                .history
                .iter_mut()
                .find(|message| message.message_id == *id)
        {
            message.text = text.to_string();
            true
        } else {
            false
        }
    }

    pub fn update_message(&mut self, message: Message) -> bool {
        if let Some(open) = &mut self.open_chat
            && let Some(existing) = open
                .history
                .iter_mut()
                .find(|existing| existing.message_id == message.message_id)
        {
            *existing = message;
            true
        } else {
            false
        }
    }

    /// Find a message by id in the open chat's history.
    pub fn find_message(&self, id: &MessageId) -> Option<&Message> {
        self.open_chat
            .as_ref()
            .and_then(|open| open.history.iter().find(|m| m.message_id == *id))
    }

    /// The message currently selected in the open chat's list, if any.
    pub fn selected_message(&self) -> Option<&Message> {
        let idx = self.message_list_state.selected()?;

        self.open_chat
            .as_ref()
            .and_then(|open| open.history.get(idx))
    }

    pub fn find_mut(&mut self, id: &ChatId) -> Option<(usize, &mut Chat)> {
        if !self.chats.iter().any(|chat| chat.id == *id) {
            let ids: Vec<_> = self.chats.iter().map(|c| format!("{:?}", c.id)).collect();
            tracing::trace!(search = ?id, chat_list = ?ids, "find_mut: chat not found in chat list");
        }
        self.chats
            .iter_mut()
            .enumerate()
            .find(|(_, chat)| chat.id == *id)
    }

    pub fn mark_read(&mut self, id: &ChatId) {
        if let Some((_, chat)) = self.find_mut(id) {
            chat.unread = false;
            chat.unread_count = 0;
        }
    }

    fn sort_fixed_first(&mut self) {
        self.chats.sort_by_key(|chat| !chat.fixed);
    }

    /// Insert or update a chat_list entry from a push update while keeping
    /// the existing order intact and preserving user-visible scroll state.
    pub fn upsert_chat(&mut self, chat: Chat) -> bool {
        let mut chat = chat;
        self.deduplicate_chats();

        if chat.contact_name == "Unknown" || chat.contact_name == "You" {
            chat.contact_name.clear();
        }

        let selected_id = self.selected_chat().map(|selected| selected.id.clone());
        if let Some((pos, entry)) = self.find_mut(&chat.id) {
            if !chat.contact_name.is_empty()
                && chat.contact_name != "Unknown"
                && chat.contact_name != "You"
            {
                entry.contact_name = chat.contact_name;
                entry.verified = chat.verified;
            }

            if let Some(status) = chat.status.clone()
                && !status.trim().is_empty()
            {
                entry.status = Some(status);
            }

            if chat.fixed {
                entry.fixed = true;
            }

            let should_bump = chat.last_message.is_some();
            let has_last_message = chat.last_message.is_some();

            if let Some(last_message) = chat.last_message {
                entry.last_message = Some(last_message);
            }

            if chat.unread || chat.unread_count > 0 {
                entry.unread = chat.unread || chat.unread_count > 0;
            } else if !has_last_message {
                entry.unread = false;
            }

            if chat.unread_count > 0 || !has_last_message {
                entry.unread_count = chat.unread_count;
            }

            if entry.fixed {
                self.sort_fixed_first();
            } else if pos > 0 && should_bump {
                let item = self.chats.remove(pos);
                self.chats.insert(0, item);
            }

            if let Some(selected_id) = selected_id {
                let selected = self.chats.iter().position(|item| item.id == selected_id);
                self.chat_list_state.select(selected);
            }
            self.deduplicate_chats();
            return true;
        }

        chat.scroll = 0;
        self.chats.insert(0, chat);
        self.sort_fixed_first();

        if let Some(selected_id) = selected_id {
            let selected = self.chats.iter().position(|item| item.id == selected_id);
            self.chat_list_state.select(selected);
        }
        self.deduplicate_chats();
        true
    }

    /// Reconcile one provider's successful dialog snapshot while preserving
    /// chats belonging to other providers and the current selection.
    pub fn reconcile_provider_chats(
        &mut self,
        provider: crate::backend::Provider,
        chats: Vec<Chat>,
    ) {
        // A non-empty snapshot is authoritative: the backend always sends its
        // complete chat list here, so rows this provider no longer reports
        // (e.g. an LID twin folded into its PN row) are pruned. An EMPTY
        // snapshot is the cold-start case where the provider has not re-synced
        // yet, and must not wipe the fuller cached list.
        if chats.is_empty() {
            return;
        }

        let selected_id = self.selected_chat().map(|chat| chat.id.clone());

        self.chats.retain(|chat| {
            !matches!(
                (&chat.id, provider),
                (ChatId::Telegram(_), crate::backend::Provider::Telegram)
                    | (ChatId::WhatsApp(_), crate::backend::Provider::WhatsApp)
            )
        });

        let mut seen = std::collections::HashSet::new();

        for chat in chats {
            if seen.insert(chat.id.clone()) {
                self.chats.push(chat);
            }
        }

        self.sort_fixed_first();
        self.deduplicate_chats();

        if let Some(selected_id) = selected_id {
            self.chat_list_state
                .select(self.chats.iter().position(|chat| chat.id == selected_id));
        }
    }

    /// Replace the open chat's history with freshly fetched data, preserving the
    /// current scroll position.
    pub fn refresh_chat_history(&mut self, chat_id: &ChatId, history: Vec<Message>) {
        if let Some(chat) = &mut self.open_chat
            && chat.chat.id == *chat_id
        {
            let old_len = chat.history.len();
            let old_selected = self.message_list_state.selected();
            let mut merged = chat.history.clone();

            for message in history {
                if let Some(existing) = merged
                    .iter_mut()
                    .find(|item| item.message_id == message.message_id)
                {
                    let mut incoming = message;

                    if incoming.reply_to_id.is_none() {
                        incoming.reply_to_id = existing.reply_to_id.clone();
                    }

                    if incoming.reply_ctx.is_none() {
                        incoming.reply_ctx = existing.reply_ctx.clone();
                    }

                    *existing = incoming;
                } else {
                    merged.push(message);
                }
            }

            merged.sort_by_key(|message| message.timestamp);
            chat.history = merged;

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

    /// Prepend older messages to the open chat history without dropping the
    /// current selection or duplicate entries. This is the foundation for lazy
    /// history loading in phase 11.
    pub fn prepend_history(&mut self, chat_id: &ChatId, older: Vec<Message>) {
        if let Some(open) = &mut self.open_chat
            && open.chat.id == *chat_id
        {
            let current_len = open.history.len();
            let mut merged = Vec::new();

            for message in older {
                if !open
                    .history
                    .iter()
                    .any(|existing| existing.message_id == message.message_id)
                {
                    merged.push(message);
                }
            }

            if merged.is_empty() {
                return;
            }

            let prepend_count = merged.len();
            open.history.splice(0..0, merged);
            open.history.sort_by_key(|message| message.timestamp);

            let selected = self.message_list_state.selected().unwrap_or(0);

            self.message_list_state.select(Some(
                (selected + prepend_count).min(open.history.len().saturating_sub(1)),
            ));

            if current_len == 0 {
                self.message_list_state
                    .select(Some(open.history.len().saturating_sub(1)));
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
    use crate::backend::Provider;

    use super::*;

    fn chat(id: ChatId, name: &str) -> Chat {
        Chat {
            id,
            contact_name: name.into(),
            ..Default::default()
        }
    }

    fn message(chat: ChatId) -> Message {
        message_with_id(chat, "m", 0)
    }

    fn message_with_id(chat: ChatId, id: &str, timestamp: i64) -> Message {
        Message {
            message_id: id.into(),
            chat,
            sender: "Sender".into(),
            author_id: None,
            text: "hello".into(),
            timestamp,
            from_me: false,
            msg_actions: Vec::new(),
            media: None,
            reply_to_id: None,
            reply_ctx: None,
            pending: false,
            failed: false,
        }
    }

    #[test]
    fn is_open_matches_only_the_loaded_chat() {
        let mut state = ChatState::default();
        assert!(!state.is_open(&ChatId::Telegram(1)));
        state.open_chat = Some(OpenChat {
            chat: chat(ChatId::Telegram(1), "A"),
            history: Vec::new(),
            has_more_history: true,
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
                has_more_history: true,
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
    fn upsert_does_not_replace_known_name_with_unknown() {
        let id = ChatId::Telegram(1);
        let mut state = ChatState {
            chats: vec![chat(id.clone(), "Teste 1")],
            ..Default::default()
        };

        state.upsert_chat(chat(id, "Unknown"));

        assert_eq!(state.chats[0].contact_name, "Teste 1");
    }

    #[test]
    fn upsert_does_not_replace_private_contact_with_you() {
        let id = ChatId::Telegram(1);
        let mut state = ChatState {
            chats: vec![chat(id.clone(), "Chat1")],
            ..Default::default()
        };

        state.upsert_chat(chat(id, "You"));

        assert_eq!(state.chats[0].contact_name, "Chat1");
    }

    #[test]
    fn upsert_does_not_insert_unknown_name_as_chat_list_title() {
        let id = ChatId::Telegram(1);
        let mut state = ChatState::default();

        state.upsert_chat(Chat {
            id: id.clone(),
            contact_name: "Unknown".into(),
            last_message: Some("hi".into()),
            ..Default::default()
        });

        assert_eq!(state.chats[0].contact_name, "");
    }

    #[test]
    fn upsert_keeps_selected_chat_selected_when_bumped() {
        let selected_id = ChatId::Telegram(2);
        let mut state = ChatState {
            chats: vec![
                chat(ChatId::Telegram(1), "A"),
                chat(selected_id.clone(), "B"),
                chat(ChatId::Telegram(3), "C"),
            ],
            ..Default::default()
        };
        state.chat_list_state.select(Some(1));

        state.upsert_chat(Chat {
            id: selected_id,
            last_message: Some("B: newest".into()),
            ..Default::default()
        });

        assert_eq!(state.chat_list_state.selected(), Some(0));
        assert_eq!(state.selected_chat().unwrap().contact_name, "B");
    }

    #[test]
    fn replace_history_preserves_scroll_position() {
        let id = ChatId::Telegram(1);
        let mut state = ChatState {
            open_chat: Some(OpenChat {
                chat: chat(id.clone(), "A"),
                history: (0..10)
                    .map(|i| message_with_id(id.clone(), &format!("old-{i}"), i))
                    .collect(),
                has_more_history: true,
            }),
            ..Default::default()
        };

        // Simulate user scrolled to position 3 (not at bottom).
        state.message_list_state.select(Some(3));

        // Replace with 12 messages (2 new ones appended at end).
        let new_history: Vec<Message> = (0..12)
            .map(|i| message_with_id(id.clone(), &format!("new-{i}"), i))
            .collect();
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
                history: (0..5)
                    .map(|i| message_with_id(id.clone(), &format!("old-{i}"), i))
                    .collect(),
                has_more_history: true,
            }),
            ..Default::default()
        };
        // User is at the bottom (index 4 = len - 1).
        state.message_list_state.select(Some(4));

        // Merge a refreshed snapshot containing 7 messages.
        let new_history: Vec<Message> = (0..7)
            .map(|i| message_with_id(id.clone(), &format!("new-{i}"), i))
            .collect();
        state.refresh_chat_history(&id, new_history);

        // The existing five plus seven refreshed messages are retained.
        assert_eq!(state.message_list_state.selected(), Some(11));
    }

    #[test]
    fn replace_history_noop_for_different_chat() {
        let mut state = ChatState {
            open_chat: Some(OpenChat {
                chat: chat(ChatId::Telegram(1), "A"),
                history: vec![message(ChatId::Telegram(1)); 5],
                has_more_history: true,
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
    fn fixed_chats_stay_above_non_fixed() {
        let mut state = ChatState {
            chats: vec![
                chat(ChatId::Telegram(1), "A"),
                chat(ChatId::Telegram(2), "B"),
            ],
            ..Default::default()
        };

        state.upsert_chat(Chat {
            id: ChatId::Telegram(2),
            contact_name: "B".into(),
            fixed: true,
            status: Some("online".into()),
            ..Default::default()
        });

        assert!(state.chats[0].fixed);
        assert_eq!(state.chats[0].id, ChatId::Telegram(2));
        assert_eq!(state.chats[0].status.as_deref(), Some("online"));
    }

    #[test]
    fn replace_history_preserves_lazy_loaded_history() {
        let id = ChatId::Telegram(1);
        let mut state = ChatState {
            open_chat: Some(OpenChat {
                chat: chat(id.clone(), "A"),
                history: (0..10)
                    .map(|i| message_with_id(id.clone(), &format!("old-{i}"), i))
                    .collect(),
                has_more_history: true,
            }),
            ..Default::default()
        };
        state.message_list_state.select(Some(8));

        // A shorter refresh must not discard the older lazy-loaded messages.
        let new_history: Vec<Message> = (0..3)
            .map(|i| message_with_id(id.clone(), &format!("new-{i}"), i))
            .collect();
        state.refresh_chat_history(&id, new_history);

        assert_eq!(state.message_list_state.selected(), Some(8));
        assert_eq!(state.open_chat.as_ref().unwrap().history.len(), 13);
    }

    #[test]
    fn prepend_history_keeps_selection_on_same_message() {
        let id = ChatId::Telegram(1);
        let mut state = ChatState {
            open_chat: Some(OpenChat {
                chat: chat(id.clone(), "A"),
                history: (0..3)
                    .map(|i| message_with_id(id.clone(), &format!("current-{i}"), i))
                    .collect(),
                has_more_history: true,
            }),
            ..Default::default()
        };
        state.message_list_state.select(Some(1));

        let older = (0..2)
            .map(|i| message_with_id(id.clone(), &format!("older-{i}"), i - 2))
            .collect();
        state.prepend_history(&id, older);

        assert_eq!(state.open_chat.as_ref().unwrap().history.len(), 5);
        assert_eq!(state.message_list_state.selected(), Some(3));
        assert_eq!(
            state.open_chat.as_ref().unwrap().history[3].message_id,
            MessageId::from("current-1")
        );
    }

    #[test]
    fn prepend_history_ignores_duplicate_messages() {
        let id = ChatId::Telegram(1);
        let existing = message_with_id(id.clone(), "existing", 0);
        let mut state = ChatState {
            open_chat: Some(OpenChat {
                chat: chat(id.clone(), "A"),
                history: vec![existing.clone()],
                has_more_history: true,
            }),
            ..Default::default()
        };

        state.prepend_history(&id, vec![existing]);

        assert_eq!(state.open_chat.as_ref().unwrap().history.len(), 1);
    }

    #[test]
    fn provider_reconciliation_prunes_stale_rows_on_non_empty_snapshot() {
        let mut state = ChatState {
            chats: vec![
                chat(ChatId::Telegram(1), "Old Telegram"),
                chat(ChatId::Telegram(2), "To Be Pruned"),
                chat(ChatId::WhatsApp("wa-1".into()), "WhatsApp"),
            ],
            ..Default::default()
        };

        // A non-empty Telegram snapshot is authoritative: the missing
        // Telegram(2) is pruned, while the other provider's chat survives.
        state.reconcile_provider_chats(
            Provider::Telegram,
            vec![chat(ChatId::Telegram(1), "Updated Telegram")],
        );

        assert_eq!(state.chats.len(), 2);
        assert!(
            state
                .chats
                .iter()
                .any(|chat| chat.id == ChatId::WhatsApp("wa-1".into()))
        );
        assert!(
            !state
                .chats
                .iter()
                .any(|chat| chat.id == ChatId::Telegram(2)),
            "a non-empty snapshot must prune rows the provider no longer reports"
        );
        assert_eq!(
            state
                .chats
                .iter()
                .find(|chat| chat.id == ChatId::Telegram(1))
                .unwrap()
                .contact_name,
            "Updated Telegram",
            "an authoritative snapshot must update existing chats"
        );
    }

    #[test]
    fn provider_reconciliation_prunes_lid_twin_on_non_empty_snapshot() {
        let mut state = ChatState {
            chats: vec![
                chat(ChatId::WhatsApp("pn-x".into()), "Test Contact"),
                chat(ChatId::WhatsApp("lid-y".into()), "Test Contact"),
            ],
            ..Default::default()
        };

        // The backend merged the LID twin into the PN row, so the live
        // WhatsApp snapshot no longer carries lid-y.
        state.reconcile_provider_chats(
            Provider::WhatsApp,
            vec![chat(ChatId::WhatsApp("pn-x".into()), "Test Contact")],
        );

        assert_eq!(state.chats.len(), 1);
        assert!(
            state
                .chats
                .iter()
                .any(|chat| chat.id == ChatId::WhatsApp("pn-x".into()))
        );
        assert!(
            !state
                .chats
                .iter()
                .any(|chat| chat.id == ChatId::WhatsApp("lid-y".into())),
            "a WhatsApp LID twin absent from a live WhatsApp snapshot must be pruned"
        );
    }

    #[test]
    fn provider_reconciliation_applies_snapshot_with_at_least_as_many_chats() {
        let mut state = ChatState {
            chats: vec![
                chat(ChatId::Telegram(1), "Old Telegram"),
                chat(ChatId::WhatsApp("wa-1".into()), "WhatsApp"),
            ],
            ..Default::default()
        };

        // A same-size snapshot is a legitimate refresh: names are updated and
        // the other provider's chats are left alone.
        state.reconcile_provider_chats(
            Provider::Telegram,
            vec![chat(ChatId::Telegram(1), "Updated Telegram")],
        );

        assert_eq!(state.chats.len(), 2);
        assert!(
            state
                .chats
                .iter()
                .any(|chat| chat.id == ChatId::WhatsApp("wa-1".into()))
        );
        assert_eq!(
            state
                .chats
                .iter()
                .find(|chat| chat.id == ChatId::Telegram(1))
                .unwrap()
                .contact_name,
            "Updated Telegram"
        );
    }

    #[test]
    fn provider_reconciliation_preserves_snapshot_order() {
        let mut state = ChatState::default();

        state.reconcile_provider_chats(
            Provider::Telegram,
            vec![
                chat(ChatId::Telegram(1), "Newest"),
                chat(ChatId::Telegram(2), "Older"),
                chat(ChatId::Telegram(3), "Oldest"),
            ],
        );

        assert_eq!(
            state
                .chats
                .iter()
                .map(|chat| chat.id.clone())
                .collect::<Vec<_>>(),
            vec![
                ChatId::Telegram(1),
                ChatId::Telegram(2),
                ChatId::Telegram(3)
            ]
        );
    }

    #[test]
    fn empty_snapshot_does_not_wipe_provider_chats() {
        let mut state = ChatState {
            chats: vec![chat(ChatId::WhatsApp("wa-1".into()), "WhatsApp")],
            ..Default::default()
        };

        state.reconcile_provider_chats(Provider::WhatsApp, Vec::new());

        assert!(
            state
                .chats
                .iter()
                .any(|c| c.id == ChatId::WhatsApp("wa-1".into())),
            "an empty snapshot (cold start / no re-sync) must not clear the provider's chats"
        );
    }

    #[test]
    fn chat_serde_round_trip() {
        let original = vec![
            chat(ChatId::WhatsApp("g.us".into()), "Group"),
            Chat {
                id: ChatId::Telegram(123),
                contact_name: "TG".into(),
                unread_count: 3,
                ..Default::default()
            },
        ];

        let json = serde_json::to_string(&original).unwrap();
        let decoded: Vec<Chat> = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].id, ChatId::WhatsApp("g.us".into()));
        assert_eq!(decoded[0].contact_name, "Group");
        assert_eq!(decoded[1].id, ChatId::Telegram(123));
        assert_eq!(decoded[1].unread_count, 3);
    }
}
