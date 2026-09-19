use std::collections::HashMap;

use ratatui::crossterm::event::KeyEvent;
use ratatui::widgets::ListState;

use crate::backend::{Chat, ChatId, Message, MessageAction, MessageId};
use crate::tui::search::SearchState;

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
    /// Reusable search/filter state for the chat list (matches by contact name).
    /// Filtering happens on `chats` — the full source list — while the visible
    /// view is selected through `SearchState::map_to_source`.
    pub search: SearchState,
    /// Reusable search state for messages in the open chat (matches by message
    /// text). `visible_indices` hold history indices of the matching messages.
    pub message_search: SearchState,
    /// History index selected when message search began, restored on cancel.
    message_search_anchor: Option<usize>,
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
    /// Enter chat-list search (insert mode) and live-filter on the query.
    pub fn begin_search(&mut self) {
        self.search.begin();
        self.refresh_search();
    }

    /// Exit insert mode, keeping the current filter applied.
    pub fn commit_search(&mut self) {
        self.search.commit();
        self.refresh_search();
    }

    /// Cancel chat-list search: drop the query and the filter. The selection is
    /// restored to the chat that was selected just before the filter dropped
    /// (derived while the filter is still applied, so the visible index still
    /// maps to the right source chat).
    pub fn clear_search(&mut self) {
        let anchor = self
            .selected_chat()
            .map(|chat| chat.id.clone())
            .or_else(|| search_anchor_id(self));

        self.search.clear();
        self.apply_search(anchor.as_ref());
    }

    /// Feed a key event to the search input and re-filter live.
    pub fn search_input(&mut self, key: KeyEvent) {
        self.search.input(key);
        self.refresh_search();
    }

    /// Recompute the filtered view from the current query, keeping the
    /// currently selected chat selected when it still matches (otherwise the
    /// first match wins). Falls back to the remembered anchor when the current
    /// selection was already cleared by an empty result set.
    pub fn refresh_search(&mut self) {
        let anchor = self
            .selected_chat()
            .map(|chat| chat.id.clone())
            .or_else(|| search_anchor_id(self));
        self.apply_search(anchor.as_ref());
    }

    /// Like [`ChatState::refresh_search`], but anchored on an explicit chat id
    /// (used when the source list was just replaced).
    pub fn refresh_search_with_id(&mut self, anchor: Option<ChatId>) {
        let anchor = anchor.or_else(|| search_anchor_id(self));
        self.apply_search(anchor.as_ref());
    }

    fn apply_search(&mut self, anchor: Option<&ChatId>) {
        let needle = self.search.query_text().trim().to_lowercase();

        if needle.is_empty() {
            // Unfiltered view: never force a selection; restore the anchor if
            // one is remembered (a full-list load keeps "nothing selected").
            self.search.set_visible(Vec::new());
            let anchor_id = anchor.cloned().or_else(|| search_anchor_id(self));
            let selection = anchor_id
                .as_ref()
                .and_then(|id| self.chats.iter().position(|chat| chat.id == *id));
            self.search.set_anchor(selection);
            self.chat_list_state.select(selection);
            return;
        }

        let indices: Vec<usize> = self
            .chats
            .iter()
            .enumerate()
            .filter(|(_, chat)| chat.contact_name.to_lowercase().contains(&needle))
            .map(|(idx, _)| idx)
            .collect();

        // Keep the selected chat when it still matches, else jump to the first
        // match. The resolved source index is remembered as the anchor so a
        // cancelled search lands back on a useful chat.
        let anchor_pos =
            anchor.and_then(|id| indices.iter().position(|&idx| self.chats[idx].id == *id));
        let selected_source = anchor_pos
            .map(|pos| indices[pos])
            .or_else(|| indices.first().copied());
        let selection = selected_source.and_then(|src| indices.iter().position(|&idx| idx == src));

        if selected_source.is_some() {
            self.search.set_anchor(selected_source);
        }

        self.search.set_visible(indices);
        self.chat_list_state.select(selection);
    }

    /// Enter message search (insert mode) over the open chat's history and
    /// live-filter on the query. No-op when no chat is open.
    pub fn begin_message_search(&mut self) {
        if self.open_chat.is_none() {
            return;
        }

        self.message_search_anchor = self.message_list_state.selected();
        self.message_search.begin();
        self.refresh_message_search();
    }

    /// Exit insert mode, keeping the current filter (and match) applied.
    pub fn commit_message_search(&mut self) {
        self.message_search.commit();
        self.refresh_message_search();
    }

    /// Cancel message search. While the user was still typing (nothing
    /// accepted), the selection returns to the message that was selected when
    /// the search began; once committed, the selected match is kept.
    pub fn clear_message_search(&mut self) {
        let was_inserting = self.message_search.is_inserting();
        self.message_search.clear();

        if was_inserting {
            let anchor = self.message_search_anchor;
            self.message_list_state.select(anchor.or_else(|| {
                self.open_chat
                    .as_ref()
                    .map(|o| o.history.len().saturating_sub(1))
            }));
        }
        self.message_search_anchor = None;
    }

    /// Feed a key event to the message search input and re-filter live.
    pub fn message_search_input(&mut self, key: KeyEvent) {
        self.message_search.input(key);
        self.refresh_message_search();
    }

    /// Recompute the current message-search matches, keeping the currently
    /// selected message when it still matches (otherwise the first match wins).
    /// Runs on user input (begin/typing/commit). Background history mutations
    /// use [`ChatState::refresh_message_search_if_active`] so the user's
    /// position is never yanked by a poll or an incoming message.
    pub fn refresh_message_search(&mut self) {
        self.apply_message_search();
    }

    /// Background path: the open chat's history changed while a message search
    /// is active (incoming message, poll result, prepend, edit). Recompute the
    /// match indices so highlights stay correct, but never move the user's
    /// selection — only live typing auto-jumps.
    fn refresh_message_search_if_active(&mut self) {
        if !self.message_search.is_active() {
            return;
        }

        let needle = self.message_search.query_text().trim().to_lowercase();

        self.message_search
            .set_visible(self.message_search_indices(&needle));
    }

    fn apply_message_search(&mut self) {
        let needle = self.message_search.query_text().trim().to_lowercase();
        let history_len = self
            .open_chat
            .as_ref()
            .map(|o| o.history.len())
            .unwrap_or(0);

        if needle.is_empty() {
            // Unfiltered view: restore the anchor (or the last message).
            self.message_search.set_visible(Vec::new());
            self.message_list_state
                .select(self.message_search_anchor.or(history_len.checked_sub(1)));
            return;
        }

        let indices = self.message_search_indices(&needle);

        // Keep the selected message when it still matches, else the first match.
        let current = self.message_list_state.selected();
        let selected = if current.is_some_and(|idx| indices.contains(&idx)) {
            current
        } else {
            indices.first().copied()
        };

        self.message_search.set_visible(indices);
        self.message_list_state.select(selected);
    }

    /// Source indices of the messages matching `needle` in the open chat's
    /// history (an empty needle yields an empty result set).
    fn message_search_indices(&self, needle: &str) -> Vec<usize> {
        if needle.is_empty() {
            return Vec::new();
        }
        self.open_chat
            .as_ref()
            .map(|open| {
                open.history
                    .iter()
                    .enumerate()
                    .filter(|(_, message)| message.text.to_lowercase().contains(needle))
                    .map(|(idx, _)| idx)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Resolve a visible chat-list index to its source index, dropping any active
    /// search filter in the process (opening a chat is a decision: the filter
    /// no longer applies). No-op when no filter is active.
    pub fn resolve_open_index(&mut self, index: usize) -> Option<usize> {
        let src = self.search.map_to_source(index)?;
        self.clear_search();
        Some(src)
    }

    /// Source indices of the currently visible chats, in display order.
    /// Empty when unfiltered (the full list is on display).
    pub fn visible_indices(&self) -> Vec<usize> {
        self.search.indices().to_vec()
    }

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

    pub fn selected_chat(&self) -> Option<&Chat> {
        let src = self
            .chat_list_state
            .selected()
            .and_then(|sel| self.search.map_to_source(sel))?;
        self.chats.get(src)
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
            self.refresh_message_search_if_active();
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
                    self.refresh_message_search_if_active();
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

            self.refresh_message_search_if_active();

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
            let removed = open.history.len() != before;

            if removed {
                self.refresh_message_search_if_active();
            }
            removed
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
            self.refresh_message_search_if_active();
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

            self.refresh_message_search_if_active();

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

    /// Sort the chat list: pinned chats first, then by most recent
    /// `last_message_ts`. The single authoritative order for the whole list,
    /// independent of whichever provider's snapshot arrived last.
    pub fn sort_pinned_then_recent(&mut self) {
        self.chats.sort_by_key(|chat| {
            (
                !chat.fixed,
                std::cmp::Reverse(chat.last_message_ts.unwrap_or(i64::MIN)),
            )
        });
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
        if let Some((_, entry)) = self.find_mut(&chat.id) {
            if !chat.contact_name.is_empty()
                && chat.contact_name != "Unknown"
                && chat.contact_name != "You"
            {
                entry.contact_name = chat.contact_name;

                // Only ever upgrade verification. Partial updates (e.g. the
                // row rebuilt for an incoming message) carry `verified: false`
                // by default and would otherwise silently strip a business
                // badge; authoritative lists apply their own verified flag
                // wholesale when they replace the row.
                if chat.verified {
                    entry.verified = true;
                }
            }

            if let Some(status) = chat.status.clone()
                && !status.trim().is_empty()
            {
                entry.status = Some(status);
            }

            if chat.fixed {
                entry.fixed = true;
            }

            let incoming_has_ts = chat.last_message_ts.is_some();
            let newer = match (chat.last_message_ts, entry.last_message_ts) {
                (Some(incoming), Some(existing)) => incoming > existing,
                (Some(_), None) => true,
                (None, _) => false,
            };

            if newer {
                entry.last_message_ts = chat.last_message_ts;
            }

            if chat.unread || chat.unread_count > 0 {
                entry.unread = chat.unread || chat.unread_count > 0;
            } else if !incoming_has_ts {
                entry.unread = false;
            }

            if chat.unread_count > 0 || !incoming_has_ts {
                entry.unread_count = chat.unread_count;
            }

            if entry.fixed || newer {
                self.sort_pinned_then_recent();
            }

            self.deduplicate_chats();
            self.refresh_search_with_id(selected_id);

            return true;
        }

        chat.scroll = 0;
        self.chats.push(chat);
        self.sort_pinned_then_recent();

        self.deduplicate_chats();
        self.refresh_search_with_id(selected_id);

        true
    }

    /// Reconcile one provider's successful dialog snapshot while preserving
    /// chats belonging to other providers and the current selection.
    ///
    /// Returns `true` when the chat list actually changed (any row added,
    /// removed or reordered). Reconciles that produce the identical list are
    /// no-ops, letting callers skip the persistence write and any churn.
    pub fn reconcile_provider_chats(
        &mut self,
        provider: crate::backend::Provider,
        chats: Vec<Chat>,
    ) -> bool {
        // A non-empty snapshot is authoritative: the backend always sends its
        // complete chat list here, so rows this provider no longer reports
        // (e.g. an LID twin folded into its PN row) are pruned. An EMPTY
        // snapshot is the cold-start case where the provider has not re-synced
        // yet, and must not wipe the fuller cached list.
        if chats.is_empty() {
            return false;
        }

        let before = self.chats.clone();
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

        self.sort_pinned_then_recent();
        self.deduplicate_chats();

        self.refresh_search_with_id(selected_id);

        before != self.chats
    }

    /// Replace the open chat's history with freshly fetched data, preserving the
    /// current scroll position.
    pub fn refresh_chat_history(&mut self, chat_id: &ChatId, history: Vec<Message>) {
        let mut handled = false;
        if let Some(chat) = &mut self.open_chat
            && chat.chat.id == *chat_id
        {
            handled = true;
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

        if handled {
            self.refresh_message_search_if_active();
        }
    }

    /// Prepend older messages to the open chat history without dropping the
    /// current selection or duplicate entries. This is the foundation for lazy
    /// history loading in phase 11.
    pub fn prepend_history(&mut self, chat_id: &ChatId, older: Vec<Message>) {
        let mut handled = false;
        if let Some(open) = &mut self.open_chat
            && open.chat.id == *chat_id
        {
            handled = true;
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

        if handled {
            self.refresh_message_search_if_active();
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

/// The id of the chat the search should land back on when its filter clears
/// (the last source index the search selection resolved to).
fn search_anchor_id(state: &ChatState) -> Option<ChatId> {
    state
        .search
        .anchor_source()
        .and_then(|src| state.chats.get(src))
        .map(|chat| chat.id.clone())
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::{KeyCode, KeyEvent};

    use crate::backend::Provider;

    use super::*;

    fn chat(id: ChatId, name: &str) -> Chat {
        Chat {
            id,
            contact_name: name.into(),
            ..Default::default()
        }
    }

    fn chat_with_ts(id: ChatId, name: &str, last_message_ts: i64) -> Chat {
        Chat {
            id,
            contact_name: name.into(),
            last_message_ts: Some(last_message_ts),
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

    fn message_with_text(chat: ChatId, text: &str) -> Message {
        let mut m = message(chat);
        m.message_id = MessageId::from(text);
        m.text = text.into();
        m
    }

    /// The chats the state would currently render (filtered or all), as the
    /// production `draw_main` assembles them via `visible_indices`.
    fn visible_chats(state: &ChatState) -> Vec<&Chat> {
        if state.search.is_filtered() {
            state
                .search
                .indices()
                .iter()
                .filter_map(|&idx| state.chats.get(idx))
                .collect()
        } else {
            state.chats.iter().collect()
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
            last_message_ts: Some(100),
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
            last_message_ts: Some(200),
            ..Default::default()
        });

        assert_eq!(state.chat_list_state.selected(), Some(0));
        assert_eq!(state.selected_chat().unwrap().contact_name, "B");
    }

    #[test]
    fn upsert_with_stale_or_missing_timestamp_does_not_reorder_or_regress() {
        let id = ChatId::Telegram(2);
        let mut state = ChatState {
            chats: vec![
                chat(ChatId::Telegram(1), "A"),
                {
                    let mut c = chat(id.clone(), "B");
                    c.last_message_ts = Some(200);
                    c
                },
                chat(ChatId::Telegram(3), "C"),
            ],
            ..Default::default()
        };

        // A stale timestamp (older than the stored one) must neither bump nor regress.
        state.upsert_chat(Chat {
            id: id.clone(),
            last_message_ts: Some(100),
            ..Default::default()
        });
        assert_eq!(state.chats[1].last_message_ts, Some(200));

        // A ts-less update (e.g. presence/status-only) must not reorder either.
        state.upsert_chat(Chat {
            id: id.clone(),
            contact_name: "B".into(),
            ..Default::default()
        });
        assert_eq!(state.chats[1].last_message_ts, Some(200));
        assert_eq!(state.chats[1].id, id);
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

    #[test]
    fn sort_pinned_then_recent_groups_pins_then_recency() {
        let mut state = ChatState::default();
        let mut pin_a = chat(ChatId::Telegram(1), "pin-a");
        pin_a.fixed = true;
        pin_a.last_message_ts = Some(50);
        let mut pin_b = chat(ChatId::Telegram(2), "pin-b");
        pin_b.fixed = true;
        pin_b.last_message_ts = Some(70);
        let mut recent = chat(ChatId::Telegram(3), "recent");
        recent.last_message_ts = Some(200);
        let mut old = chat(ChatId::Telegram(4), "old");
        old.last_message_ts = Some(100);
        let no_ts = chat(ChatId::Telegram(5), "no-ts");

        state.chats = vec![old, recent, pin_a, no_ts, pin_b];
        state.sort_pinned_then_recent();

        let ids: Vec<_> = state.chats.iter().map(|c| c.id.clone()).collect();
        assert_eq!(
            ids,
            vec![
                ChatId::Telegram(2), // pinned band first (recency within it)
                ChatId::Telegram(1),
                ChatId::Telegram(3), // then recency across the whole list
                ChatId::Telegram(4),
                ChatId::Telegram(5), // no timestamp sinks to the bottom
            ]
        );
    }

    #[test]
    fn upsert_activity_never_puts_chat_above_pins() {
        let mut state = ChatState::default();
        let setup = [
            (ChatId::Telegram(10), "pin1", 500, true),
            (ChatId::Telegram(11), "pin2", 400, true),
            (ChatId::Telegram(12), "chat1", 100, false),
            (ChatId::Telegram(13), "chat2", 200, false),
        ];
        for (id, name, ts, fixed) in setup {
            let mut c = chat(id, name);
            c.last_message_ts = Some(ts);
            c.fixed = fixed;
            state.upsert_chat(c);
        }

        // chat2 receives a new message: recency rises past both pinned chats,
        // but it must land at the TOP OF THE UNPINNED band, never above pins.
        state.upsert_chat(Chat {
            id: ChatId::Telegram(13),
            last_message_ts: Some(900),
            ..Default::default()
        });

        let ids: Vec<_> = state.chats.iter().map(|c| c.id.clone()).collect();
        assert_eq!(
            ids,
            vec![
                ChatId::Telegram(10),
                ChatId::Telegram(11),
                ChatId::Telegram(13),
                ChatId::Telegram(12),
            ]
        );
    }

    #[test]
    fn pinned_chat_incoming_message_stays_in_pinned_band() {
        let mut state = ChatState::default();
        let mut pin = chat(ChatId::Telegram(20), "pin");
        pin.fixed = true;
        pin.last_message_ts = Some(300);
        let mut unpinned = chat(ChatId::Telegram(21), "unpinned");
        unpinned.last_message_ts = Some(600);
        state.chats = vec![pin, unpinned];
        state.sort_pinned_then_recent();

        // A pinned chat becomes the most recent chat overall; it still belongs
        // in the pinned band, not at the top of the whole list's index 0.
        state.upsert_chat(Chat {
            id: ChatId::Telegram(20),
            last_message_ts: Some(1000),
            ..Default::default()
        });

        let ids: Vec<_> = state.chats.iter().map(|c| c.id.clone()).collect();
        assert_eq!(ids, vec![ChatId::Telegram(20), ChatId::Telegram(21)]);
        assert!(state.chats[0].fixed);
    }

    #[test]
    fn reconcile_order_is_independent_of_snapshot_arrival() {
        let tg = |id: i64, ts: i64| -> Chat {
            let mut c = chat(ChatId::Telegram(id), &format!("tg-{id}"));
            c.last_message_ts = Some(ts);
            c
        };
        let wa = |id: i64| -> Chat {
            let mut c = chat(
                ChatId::WhatsApp(format!("1555000000{id}@s.whatsapp.net")),
                &format!("wa-{id}"),
            );
            c.last_message_ts = Some(90 + id);
            c
        };

        let mut wp_first = ChatState::default();
        wp_first.reconcile_provider_chats(Provider::WhatsApp, vec![wa(1), wa(2)]);
        wp_first.reconcile_provider_chats(Provider::Telegram, vec![tg(1, 500), tg(2, 300)]);

        let mut tg_first = ChatState::default();
        tg_first.reconcile_provider_chats(Provider::Telegram, vec![tg(1, 500), tg(2, 300)]);
        tg_first.reconcile_provider_chats(Provider::WhatsApp, vec![wa(1), wa(2)]);

        // The merged view is normalized by recency, not by whichever
        // provider's snapshot landed last.
        assert_eq!(wp_first.chats, tg_first.chats);
        let ids: Vec<_> = wp_first.chats.iter().map(|c| c.id.clone()).collect();
        assert_eq!(
            ids,
            vec![
                ChatId::Telegram(1),                                   // 500
                ChatId::Telegram(2),                                   // 300
                ChatId::WhatsApp("15550000002@s.whatsapp.net".into()), // 92
                ChatId::WhatsApp("15550000001@s.whatsapp.net".into()), // 91
            ]
        );
    }

    #[test]
    fn reconcile_reorder_keeps_the_selected_chat_selected_in_a_filtered_view() {
        let mut state = ChatState {
            chats: vec![
                chat_with_ts(ChatId::Telegram(1), "alice", 5),
                chat_with_ts(ChatId::Telegram(2), "bob", 4),
                chat_with_ts(ChatId::Telegram(3), "carol", 3),
                chat_with_ts(ChatId::Telegram(4), "dave", 2),
            ],
            ..Default::default()
        };
        state.sort_pinned_then_recent();

        state.begin_search();
        state.search_input(KeyEvent::from(KeyCode::Char('a')));
        state.commit_search();
        assert_eq!(state.search.indices(), &[0, 2, 3]);

        state.chat_list_state.select(Some(1));
        assert_eq!(state.selected_chat().unwrap().contact_name, "carol");

        // A fresh snapshot bumps "dave" to the top, reordering the source list
        // behind the committed filter.
        state.reconcile_provider_chats(
            Provider::Telegram,
            vec![
                chat_with_ts(ChatId::Telegram(4), "dave", 100),
                chat_with_ts(ChatId::Telegram(1), "alice", 5),
                chat_with_ts(ChatId::Telegram(2), "bob", 4),
                chat_with_ts(ChatId::Telegram(3), "carol", 3),
            ],
        );

        assert_eq!(
            state.selected_chat().map(|c| c.id.clone()),
            Some(ChatId::Telegram(3)),
            "a reorder must re-anchor by chat id, not by the stale index"
        );
    }

    #[test]
    fn upsert_reorder_keeps_the_selected_chat_selected_in_a_filtered_view() {
        let mut state = ChatState {
            chats: vec![
                chat_with_ts(ChatId::Telegram(1), "alice", 5),
                chat_with_ts(ChatId::Telegram(2), "bob", 4),
                chat_with_ts(ChatId::Telegram(3), "carol", 3),
                chat_with_ts(ChatId::Telegram(4), "dave", 2),
            ],
            ..Default::default()
        };
        state.sort_pinned_then_recent();

        state.begin_search();
        state.search_input(KeyEvent::from(KeyCode::Char('a')));
        state.commit_search();
        assert_eq!(state.search.indices(), &[0, 2, 3]);

        state.chat_list_state.select(Some(1));
        assert_eq!(state.selected_chat().unwrap().contact_name, "carol");

        // "dave" gets a newer message and moves to the top of the list.
        state.upsert_chat(chat_with_ts(ChatId::Telegram(4), "dave", 100));

        assert_eq!(
            state.selected_chat().map(|c| c.id.clone()),
            Some(ChatId::Telegram(3)),
            "an upsert-triggered reorder must not switch the selection to the \
             first match"
        );
    }

    #[test]
    fn unchanged_reconcile_is_a_noop() {
        let mut state = ChatState::default();
        let snapshot = vec![chat(ChatId::Telegram(30), "tg")];
        assert!(state.reconcile_provider_chats(Provider::Telegram, snapshot.clone()));
        assert!(
            !state.reconcile_provider_chats(Provider::Telegram, snapshot),
            "re-applying the identical snapshot must report no change"
        );

        assert!(
            !state.reconcile_provider_chats(Provider::Telegram, Vec::new()),
            "an empty snapshot is a cold-start no-op, never a change"
        );
    }

    #[test]
    fn partial_update_never_strips_verified_badge() {
        let mut state = ChatState::default();
        let mut biz = chat(ChatId::WhatsApp("15550000001@s.whatsapp.net".into()), "Biz");
        biz.verified = true;
        state.chats.push(biz);
        state.sort_pinned_then_recent();

        // A message update rebuilds the row with `verified: false` by default;
        // the badge must survive the partial upsert.
        state.upsert_chat(Chat {
            id: ChatId::WhatsApp("15550000001@s.whatsapp.net".into()),
            contact_name: "Biz".into(),
            last_message_ts: Some(50),
            ..Default::default()
        });

        assert!(state.chats[0].verified);
    }

    #[test]
    fn search_filters_by_case_insensitive_contact_name() {
        let mut state = ChatState {
            chats: vec![
                chat(ChatId::Telegram(1), "Alice"),
                chat(ChatId::Telegram(2), "Bob"),
                chat(ChatId::Telegram(3), "alice in ops"),
            ],
            ..Default::default()
        };

        state.begin_search();
        state.search_input(KeyEvent::from(KeyCode::Char('a')));

        assert!(state.search.is_filtered());
        assert!(state.search.is_inserting());
        let names: Vec<&str> = visible_chats(&state)
            .iter()
            .map(|c| c.contact_name.as_str())
            .collect();
        assert_eq!(names, ["Alice", "alice in ops"]);
    }

    #[test]
    fn search_keeps_selection_when_still_matching_and_falls_back_to_first() {
        let mut state = ChatState {
            chats: vec![
                chat(ChatId::Telegram(1), "Alice"),
                chat(ChatId::Telegram(2), "Bob"),
                chat(ChatId::Telegram(3), "Charlie"),
            ],
            ..Default::default()
        };
        state.chat_list_state.select(Some(1));

        state.begin_search();
        state.search_input(KeyEvent::from(KeyCode::Char('b')));
        assert_eq!(state.selected_chat().unwrap().contact_name, "Bob");

        state.search_input(KeyEvent::from(KeyCode::Char('a')));
        assert!(
            state.selected_chat().is_none(),
            "no match left after 'ba', selection clears"
        );

        state.search_input(KeyEvent::from(KeyCode::Backspace));
        assert_eq!(
            state.selected_chat().unwrap().contact_name,
            "Bob",
            "re-matching re-selects the previously selected chat"
        );
    }

    #[test]
    fn empty_query_after_backspace_shows_the_full_list() {
        let mut state = ChatState {
            chats: vec![
                chat(ChatId::Telegram(1), "Alice"),
                chat(ChatId::Telegram(2), "Bob"),
                chat(ChatId::Telegram(3), "Charlie"),
            ],
            ..Default::default()
        };

        state.begin_search();
        state.search_input(KeyEvent::from(KeyCode::Char('z')));
        assert!(state.search.is_filtered());

        state.search_input(KeyEvent::from(KeyCode::Backspace));
        assert!(!state.search.is_filtered());
        assert_eq!(visible_chats(&state).len(), state.chats.len());
    }

    #[test]
    fn commit_keeps_the_filter_and_clear_restores_everything() {
        let mut state = ChatState {
            chats: vec![
                chat(ChatId::Telegram(1), "Alice"),
                chat(ChatId::Telegram(2), "Bob"),
                chat(ChatId::Telegram(3), "Charlie"),
            ],
            ..Default::default()
        };

        state.begin_search();
        state.search_input(KeyEvent::from(KeyCode::Char('e')));
        state.commit_search();

        assert!(!state.search.is_inserting());
        assert!(state.search.is_filtered());
        assert_eq!(visible_chats(&state).len(), 2);

        state.clear_search();
        assert!(!state.search.is_active());
        assert_eq!(visible_chats(&state).len(), 3);
    }

    #[test]
    fn selected_chat_maps_through_the_filtered_view() {
        let mut state = ChatState {
            chats: vec![
                chat(ChatId::Telegram(1), "Alice"),
                chat(ChatId::Telegram(2), "Bob"),
                chat(ChatId::Telegram(3), "Aaron"),
            ],
            ..Default::default()
        };

        state.begin_search();
        state.search_input(KeyEvent::from(KeyCode::Char('a')));

        // Displayed order is the source order: Alice (0) then Aaron (2).
        assert_eq!(visible_chats(&state).len(), 2);

        state.chat_list_state.select(Some(1));
        assert_eq!(state.selected_chat().unwrap().contact_name, "Aaron");
        assert_eq!(
            state.selected_chat().unwrap().id,
            ChatId::Telegram(3),
            "selection maps to the correct source chat"
        );
    }

    #[test]
    fn clear_search_preserves_the_previously_selected_chat() {
        let mut state = ChatState {
            chats: vec![
                chat(ChatId::Telegram(1), "Alice"),
                chat(ChatId::Telegram(2), "Bob"),
                chat(ChatId::Telegram(3), "Charlie"),
            ],
            ..Default::default()
        };
        state.chat_list_state.select(Some(2));

        state.begin_search();
        state.search_input(KeyEvent::from(KeyCode::Char('Z'))); // no match
        assert!(state.selected_chat().is_none());

        state.clear_search();
        assert_eq!(state.selected_chat().unwrap().contact_name, "Charlie");
    }

    #[test]
    fn upsert_while_filtering_keeps_the_filter_consistent() {
        let mut state = ChatState {
            chats: vec![
                chat(ChatId::Telegram(1), "Alice"),
                chat(ChatId::Telegram(2), "Bob"),
            ],
            ..Default::default()
        };

        state.begin_search();
        state.search_input(KeyEvent::from(KeyCode::Char('a')));
        let before = visible_chats(&state).len();

        state.upsert_chat(chat(ChatId::Telegram(3), "Zed"));

        assert_eq!(
            visible_chats(&state).len(),
            before,
            "a non-matching upsert must not change the filtered view"
        );
        assert!(state.search.is_filtered());
    }

    #[test]
    fn message_search_matches_case_insensitively_and_selects_first_match() {
        let mut state = ChatState {
            open_chat: Some(OpenChat {
                chat: chat(ChatId::Telegram(1), "A"),
                history: vec![
                    message_with_text(ChatId::Telegram(1), "hello"),
                    message_with_text(ChatId::Telegram(1), "hi"),
                    message_with_text(ChatId::Telegram(1), "HELLO"),
                ],
                has_more_history: true,
            }),
            ..Default::default()
        };
        state.message_list_state.select(Some(1));

        state.begin_message_search();
        assert!(state.message_search.is_inserting());

        state.message_search_input(KeyEvent::from(KeyCode::Char('h')));
        state.message_search_input(KeyEvent::from(KeyCode::Char('e')));

        // "he" matches index 0 ("Hello") and 2 ("HELLO"); the currently selected
        // message (1) no longer matches, so the first match wins.
        assert_eq!(state.message_search.indices(), &[0, 2]);
        assert_eq!(state.message_list_state.selected(), Some(0));
    }

    #[test]
    fn message_search_keeps_current_message_when_still_matching() {
        let mut state = ChatState {
            open_chat: Some(OpenChat {
                chat: chat(ChatId::Telegram(1), "A"),
                history: vec![
                    message_with_text(ChatId::Telegram(1), "foo"),
                    message_with_text(ChatId::Telegram(1), "bar"),
                    message_with_text(ChatId::Telegram(1), "foobar"),
                ],
                has_more_history: true,
            }),
            ..Default::default()
        };
        state.message_list_state.select(Some(2));

        state.begin_message_search();
        state.message_search_input(KeyEvent::from(KeyCode::Char('f')));

        assert_eq!(state.message_search.indices(), &[0, 2]);
        assert_eq!(
            state.message_list_state.selected(),
            Some(2),
            "the selected 'foobar' still matches, so selection is kept"
        );

        state.message_search_input(KeyEvent::from(KeyCode::Char('z')));
        assert!(
            state.message_search.indices().is_empty()
                && state.message_list_state.selected().is_none(),
            "no message contains 'fz', selection clears"
        );
    }

    #[test]
    fn message_search_no_match_then_backspace_restores_anchor() {
        let mut state = ChatState {
            open_chat: Some(OpenChat {
                chat: chat(ChatId::Telegram(1), "A"),
                history: vec![
                    message_with_text(ChatId::Telegram(1), "hello"),
                    message_with_text(ChatId::Telegram(1), "world"),
                ],
                has_more_history: true,
            }),
            ..Default::default()
        };
        state.message_list_state.select(Some(0));

        state.begin_message_search();
        state.message_search_input(KeyEvent::from(KeyCode::Char('q')));
        assert!(state.message_list_state.selected().is_none());

        state.message_search_input(KeyEvent::from(KeyCode::Backspace));
        assert!(!state.message_search.is_filtered());
        assert_eq!(
            state.message_list_state.selected(),
            Some(0),
            "an empty query restores the message selected before searching"
        );
    }

    #[test]
    fn commit_message_search_keeps_filter_and_selection() {
        let mut state = ChatState {
            open_chat: Some(OpenChat {
                chat: chat(ChatId::Telegram(1), "A"),
                history: vec![
                    message_with_text(ChatId::Telegram(1), "hello"),
                    message_with_text(ChatId::Telegram(1), "hi"),
                    message_with_text(ChatId::Telegram(1), "yo"),
                ],
                has_more_history: true,
            }),
            ..Default::default()
        };
        state.message_list_state.select(Some(2));

        state.begin_message_search();
        state.message_search_input(KeyEvent::from(KeyCode::Char('h')));
        state.commit_message_search();

        assert!(!state.message_search.is_inserting());
        assert!(state.message_search.is_filtered());
        assert_eq!(state.message_search.indices(), &[0, 1]);
        assert_eq!(state.message_list_state.selected(), Some(0));
    }

    #[test]
    fn clear_message_search_restores_anchor_while_typing() {
        let mut state = ChatState {
            open_chat: Some(OpenChat {
                chat: chat(ChatId::Telegram(1), "A"),
                history: vec![
                    message_with_text(ChatId::Telegram(1), "hello"),
                    message_with_text(ChatId::Telegram(1), "swell"),
                ],
                has_more_history: true,
            }),
            ..Default::default()
        };
        state.message_list_state.select(Some(1));

        state.begin_message_search();
        state.message_search_input(KeyEvent::from(KeyCode::Char('e')));

        state.clear_message_search();
        assert!(!state.message_search.is_active());
        assert_eq!(
            state.message_list_state.selected(),
            Some(1),
            "cancelling before accepting returns to the pre-search message"
        );
    }

    #[test]
    fn clear_after_commit_keeps_the_selected_match() {
        let mut state = ChatState {
            open_chat: Some(OpenChat {
                chat: chat(ChatId::Telegram(1), "A"),
                history: vec![
                    message_with_text(ChatId::Telegram(1), "hello"),
                    message_with_text(ChatId::Telegram(1), "hello again"),
                ],
                has_more_history: true,
            }),
            ..Default::default()
        };
        state.message_list_state.select(Some(0));

        state.begin_message_search();
        state.message_search_input(KeyEvent::from(KeyCode::Char('h')));
        state.commit_message_search();

        // The user browses on from the accepted match, then cancels the search.
        state.message_list_state.select(Some(1));
        state.clear_message_search();

        assert!(!state.message_search.is_active());
        assert_eq!(
            state.message_list_state.selected(),
            Some(1),
            "cancelling an accepted search must not jump back to the anchor"
        );
    }

    #[test]
    fn message_search_is_a_noop_without_an_open_chat() {
        let mut state = ChatState::default();
        state.begin_message_search();
        assert!(!state.message_search.is_active());
        assert!(!state.message_search.is_inserting());
    }

    #[test]
    fn incoming_message_refreshes_an_active_message_search() {
        let mut state = ChatState {
            open_chat: Some(OpenChat {
                chat: chat(ChatId::Telegram(1), "A"),
                history: vec![message_with_text(ChatId::Telegram(1), "hello")],
                has_more_history: true,
            }),
            ..Default::default()
        };

        state.begin_message_search();
        state.message_search_input(KeyEvent::from(KeyCode::Char('x')));
        assert!(state.message_search.indices().is_empty());

        state.push_incoming(message_with_text(ChatId::Telegram(1), "xylophone"));

        assert_eq!(state.message_search.indices(), &[1]);
        assert_eq!(
            state.message_list_state.selected(),
            None,
            "a background arrival refreshes the matches but must not yank the \
             selection; only live typing auto-jumps"
        );
    }

    #[test]
    fn incoming_message_does_not_yank_a_committed_search_selection() {
        let mut state = ChatState {
            open_chat: Some(OpenChat {
                chat: chat(ChatId::Telegram(1), "A"),
                history: vec![
                    message_with_text(ChatId::Telegram(1), "hello"),
                    message_with_text(ChatId::Telegram(1), "hi"),
                    message_with_text(ChatId::Telegram(1), "yo"),
                ],
                has_more_history: true,
            }),
            ..Default::default()
        };

        state.begin_message_search();
        state.message_search_input(KeyEvent::from(KeyCode::Char('h')));
        state.commit_message_search();
        assert_eq!(state.message_search.indices(), &[0, 1]);
        assert_eq!(state.message_list_state.selected(), Some(0));

        // The user browses on from the accepted match to a non-match.
        state.message_list_state.select(Some(2));

        state.push_incoming(message_with_text(ChatId::Telegram(1), "harp"));

        assert_eq!(state.message_search.indices(), &[0, 1, 3]);
        assert_eq!(
            state.message_list_state.selected(),
            Some(2),
            "new matching messages are highlighted but never steal the selection"
        );
    }

    #[test]
    fn periodic_history_refresh_does_not_yank_a_committed_search_selection() {
        let mut state = ChatState {
            open_chat: Some(OpenChat {
                chat: chat(ChatId::Telegram(1), "A"),
                history: vec![
                    message_with_text(ChatId::Telegram(1), "hello"),
                    message_with_text(ChatId::Telegram(1), "hi"),
                    message_with_text(ChatId::Telegram(1), "yo"),
                    message_with_text(ChatId::Telegram(1), "sup"),
                ],
                has_more_history: true,
            }),
            ..Default::default()
        };

        state.begin_message_search();
        state.message_search_input(KeyEvent::from(KeyCode::Char('h')));
        state.commit_message_search();
        assert_eq!(state.message_list_state.selected(), Some(0));

        // The user browses on from the accepted match to a non-match.
        state.message_list_state.select(Some(2));

        state.refresh_chat_history(
            &ChatId::Telegram(1),
            vec![message_with_text(ChatId::Telegram(1), "hiya")],
        );

        assert_eq!(state.message_search.indices(), &[0, 1, 4]);
        assert_eq!(
            state.message_list_state.selected(),
            Some(2),
            "the periodic poll recomputes the matches without moving the selection"
        );
    }

    #[test]
    fn prepend_history_keeps_message_search_consistent() {
        let mut state = ChatState {
            open_chat: Some(OpenChat {
                chat: chat(ChatId::Telegram(1), "A"),
                history: vec![
                    message_with_text(ChatId::Telegram(1), "hello"),
                    message_with_text(ChatId::Telegram(1), "world"),
                ],
                has_more_history: true,
            }),
            ..Default::default()
        };

        state.begin_message_search();
        state.message_search_input(KeyEvent::from(KeyCode::Char('h')));
        assert_eq!(state.message_search.indices(), &[0]);
        assert_eq!(state.message_list_state.selected(), Some(0));

        let older = vec![
            message_with_text(ChatId::Telegram(1), "aaa"),
            message_with_text(ChatId::Telegram(1), "bbb"),
        ];
        state.prepend_history(&ChatId::Telegram(1), older);

        assert_eq!(
            state.message_search.indices(),
            &[2],
            "after the prepend, 'hello' (the only 'h' match) sits at index 2"
        );
        assert_eq!(state.message_list_state.selected(), Some(2));
    }
}
