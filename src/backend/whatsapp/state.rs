use crate::backend::{Chat, ChatId, Message, MessageId};

use super::convert::{resolve_conversation_name, resolve_sender_name};
use super::ids::is_self_chat;
use super::media::MediaRef;
use super::sync::should_skip_conversation;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, warn};
use whatsapp_rust::Client;
use whatsapp_rust::prelude::{Jid, MessageExt, MessageInfo, wa};
use whatsapp_rust::wacore_binary::JidExt;
use whatsapp_rust::waproto::whatsapp::Message as WaMessage;

pub(super) type SharedState = Arc<RwLock<WhatsAppState>>;

/// In-memory mirror of the account's conversations and messages, populated from
/// `Event::HistorySync` and `Event::Messages` delivered by the whatsapp-rust
/// bot. SQLite persists the session/auth credentials; this cache drives the TUI.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub(super) struct WhatsAppState {
    pub(super) chats: Vec<Chat>,
    // BUG: WhatsApp is still not delivering messages properly on cold starts:
    // Messages received during our app's inactivity are not being fetched... Sometimes?
    pub(super) history: HashMap<ChatId, Vec<Message>>,
    /// Stanza is a XML-like structure exchanged between client and server.
    /// Each Stanza contains data that identifies and populates the message
    /// So basically, Stanza == Message
    pub(super) by_stanza_id: HashMap<String, (ChatId, MessageId)>,

    /// CDN references for image, video and audio messages, keyed by stanza id.
    /// Persisted with the cache so that media from earlier sessions still
    /// downloads after a restart (the raw `wa::Message` itself is not
    /// deserializable). No media bytes are ever stored — only the fields that
    /// fetch them on demand. The cache (including these hashes) is written
    /// owner-only through [`crate::file::write_owner_only`].
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub(super) media_refs: HashMap<String, MediaRef>,

    /// JID -> push name cache populated from HistorySync pushnames.
    pub(super) pushnames: HashMap<String, String>,
    /// JID string -> display name learned from usync (the peer's username or
    /// verified business name), keyed the way `pushnames` are (peer PN or LID).
    /// Names the phone itself declines to sync still resolve to something
    /// human-readable across restarts.
    #[serde(default)]
    pub(super) usync_names: HashMap<String, String>,
    /// JID strings (same keys as `usync_names`) whose peer is a verified
    /// business; the TUI renders a check mark next to the name.
    #[serde(default)]
    pub(super) usync_verified: HashSet<String>,
    /// JID strings already queried through usync, kept so a peer that has no
    /// username/business name is not re-queried on every history sync.
    #[serde(default)]
    pub(super) usync_attempted: HashSet<String>,
    /// Epoch seconds of the first `Event::HistorySync` after a connect. The
    /// connect-time retry loop parks on `None`: it must not stop re-requesting
    /// the snapshot just because the chat list was seeded from the cache
    /// (`state.chats` is non-empty even when no conversation sync ever lands).
    #[serde(default)]
    pub(super) sync_seen_at: Option<i64>,
    /// LID user-part -> phone-number user-part, learned from the client's local
    /// LID↔PN cache (`Client::get_lid_pn_entry`). Lets live `@lid` messages and
    /// conversations route to the phone-keyed chat row the user opens, and gives
    /// LID senders a readable name.
    ///
    /// INFO: LID JID definitions:
    /// JID => Group/Contact ids in the format <Phone Number>@<server>
    /// LID (Long ID) => Non-Phone number id for user or group
    #[serde(default)]
    pub(super) lid_pn: HashMap<String, String>,
    /// Peer JID string -> (online, last_seen) learned from `Event::Presence`.
    #[serde(default)]
    pub(super) presence: HashMap<String, (bool, Option<i64>)>,
    /// Canonical chat key (the same string the list row uses) -> (millisecond
    /// epoch of the last pin change applied, pinned). A full-sync pin replay
    /// delivers every chat's historical pin/unpin actions in arbitrary order,
    /// so bare last-write-wins would leave `fixed` at whatever mutation
    /// happened to arrive last. Keying by change-time (newest wins) makes the
    /// replay converge to the phone's current pin state regardless of order,
    /// and lets pins survive even when they arrive before the chat row exists.
    #[serde(default)]
    pub(super) pin_state: HashMap<String, (i64, bool)>,
    /// The account's own LID and phone-number JIDs (`to_non_ad_string`), used
    /// to label the self-chat ("Myself") instead of the formatted phone number.
    /// Learned from `client.lid()`/`client.pn()` on connect and history sync
    /// (an Option so the cache survives before the first learn).
    #[serde(default)]
    pub(super) own_lid: Option<String>,
    #[serde(default)]
    pub(super) own_pn: Option<String>,
    /// Edits whose base message is not in `history` yet, keyed by
    /// `(base message id, chat)`. An edit can outrun its base — on a cold
    /// start a live edit arrives before the history chunk that carries the
    /// message it rewrites — so it is held here and replayed by
    /// [`WhatsAppState::apply_deferred_edits`] when the base lands instead of
    /// being dropped. Runtime-only (never persisted: a new session re-receives
    /// the message and the edit with it) and capped by
    /// [`PENDING_EDIT_LIMIT`], since an edit whose base never arrives is not
    /// worth keeping.
    #[serde(skip)]
    pub(super) pending_edits: HashMap<(MessageId, ChatId), String>,
}

/// How many unresolved edits to hold before dropping the newest. Generous
/// next to the handful of edits a single sync burst replays, but bounded so a
/// stream of edits for messages we never cache cannot grow state without limit.
const PENDING_EDIT_LIMIT: usize = 64;

impl WhatsAppState {
    /// Restore a previously persisted account snapshot (chats, history,
    /// pushnames) so a cold restart does not start with an empty chat list or
    /// empty history. Missing/corrupt cache falls back to a fresh state.
    pub(super) fn load_from(cache_path: &str) -> Self {
        // `file::read_to_string` tightens the cache to owner-only as it opens
        // it (it holds message history), so no separate repair step is needed.
        match crate::file::read_to_string(Path::new(cache_path)) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Persist the account snapshot to the JSON cache. Best-effort:
    /// a failure only logs (the next successful save retries) and never aborts ingestion.
    pub(super) fn save_to(&self, cache_path: &Path) {
        let raw = match serde_json::to_string_pretty(self) {
            Ok(raw) => raw,
            Err(e) => {
                warn!("Failed to serialize WA state: {e}");
                return;
            }
        };

        if let Err(e) = crate::file::write_owner_only(cache_path, raw.as_bytes()) {
            warn!(
                "Failed to write WA cache {}: {e}",
                cache_path.to_string_lossy()
            );
        }
    }

    /// Remember the account's own LID and phone-number JIDs for self-chat
    /// labelling. Idempotent; both are `None` until the client knows them.
    /// Also heals cached self-chat rows (labelled with the formatted own number
    /// by earlier versions) the moment the identity is learned.
    pub(super) fn learn_own_identity(&mut self, client: &Client) {
        self.own_lid = client.lid().map(|j| j.to_non_ad_string());
        self.own_pn = client.pn().map(|j| j.to_non_ad_string());
        self.name_self_chats();
    }

    /// Re-derive cached message senders from the current pushname/LID↔PN maps.
    ///
    /// The first history sync lands before the pushnames bundle, freezing the
    /// newest 1:1 messages with bare numbers; the dup guard later skips the
    /// rows, so they never get a second chance to resolve. Sweep every stored
    /// `!from_me` message: upgrade a bare sender to anything now known (a
    /// pushname, a LID↔PN mapping, or a 1:1 chat's own display name), and
    /// re-bind already-named rows when the peer renamed — but never downgrade
    /// a known name back to a bare id when the maps lost it. Group rows whose
    /// author is the group's own JID have no recoverable identity (HistorySync
    /// omits `key.participant`) and are left untouched; group rows that did
    /// capture a real participant via the live path still resolve by name.
    /// Returns how many senders were rewritten.
    pub(super) fn resolve_stale_senders(&mut self) -> usize {
        let chats = self.chats.clone();
        let mut rewrites: Vec<(ChatId, MessageId, String)> = Vec::new();

        for (chat_key, messages) in &self.history {
            let ChatId::WhatsApp(raw) = chat_key else {
                continue;
            };

            let chat_is_group = Jid::from_str(raw).map(|j| j.is_group()).unwrap_or(false);
            let chat_contact = chats
                .iter()
                .find(|c| c.id == *chat_key)
                .map(|c| c.contact_name.clone())
                .filter(|n| !n.trim().is_empty());

            for m in messages.iter().filter(|m| !m.from_me) {
                let Some(author) = m.author_id.as_deref() else {
                    continue;
                };

                let Ok(author_jid) = Jid::from_str(author) else {
                    continue;
                };

                let bare = author_jid.user_base();
                if bare.is_empty() {
                    continue;
                }

                let resolved = resolve_sender_name("", &author_jid, &self.pushnames, &self.lid_pn);

                // Unresolvable from the maps: only a 1:1 chat's own display
                // name (usync-enriched) is a safe fallback. Group rows must
                // keep the participant's raw id — applying the group title
                // would label every message with the chat.
                let name = if resolved == bare {
                    if !chat_is_group && let Some(contact) = chat_contact.as_ref() {
                        contact.clone()
                    } else {
                        continue;
                    }
                } else {
                    resolved
                };

                // Upgrade a bare stored name to anything better; otherwise
                // only rebind to another real name when the maps know the
                // author differently. Never downgrade to a bare id.
                if (m.sender == bare || name != bare) && name != m.sender {
                    rewrites.push((chat_key.clone(), m.message_id.clone(), name));
                }
            }
        }

        let repaired = rewrites.len();
        for (chat, id, name) in rewrites {
            if let Some(h) = self.history.get_mut(&chat)
                && let Some(m) = h.iter_mut().find(|m| m.message_id == id)
            {
                m.sender = name;
            }
        }
        repaired
    }

    /// Rename any cached row that addresses the account's own LID or phone
    /// number to "Myself". WhatsApp keeps the self-thread local on the phone
    /// (it may never arrive via HistorySync), so rows seeded from the cache
    /// need this sweep to converge.
    pub(super) fn name_self_chats(&mut self) {
        for chat in self.chats.iter_mut() {
            if is_self_chat(&chat.id, self.own_lid.as_deref(), self.own_pn.as_deref()) {
                chat.contact_name = "Myself".to_string();
                chat.verified = false;
            }
        }
    }

    /// Rewrite the text of a stored message (by its original id) and refresh
    /// the chat list preview. Used both for edits arriving back from the server
    /// and for local edits confirmed by the send ack (which the server never
    /// re-delivers in the self-chat). Returns the updated copy, or `None` when
    /// the target is not in the cache.
    pub(super) fn edit_message_text(
        &mut self,
        chat: &ChatId,
        id: &MessageId,
        text: &str,
    ) -> Option<Message> {
        let existing = self
            .history
            .get_mut(chat)?
            .iter_mut()
            .find(|m| m.message_id == *id)?;
        existing.text = text.to_string();
        let updated = existing.clone();
        self.refresh_last_message_ts(chat);
        Some(updated)
    }

    /// Apply a WhatsApp message edit to the stored history.
    ///
    /// Edits arrive as a new `Event::Messages` entry whose stanza id is the
    /// ORIGINAL message's, carrying either the re-written body top-level
    /// (`Message.edited_message`, peeled by `get_base_message`) or the
    /// decrypted secret-encrypted variant inside
    /// `protocol_message.edited_message`. The stored copy is located by the
    /// original message id, its text swapped, the preview refreshed, and the
    /// updated message returned for a `BackendEvent::MessageUpdated` broadcast.
    ///
    /// Returns the updated message, or `None` when the stanza carries no text
    /// to apply. An edit whose base is not cached yet is *not* dropped: it is
    /// held in `pending_edits` for [`WhatsAppState::apply_deferred_edits`] to
    /// replay once the base is ingested, which is the only way an edit that
    /// outran its message survives.
    pub(super) fn apply_message_edit(
        &mut self,
        chat: &ChatId,
        info: &MessageInfo,
        wa_msg: &WaMessage,
    ) -> Option<Message> {
        let target_id = wa_msg
            .protocol_message
            .as_option()
            .and_then(|pm| pm.key.as_option())
            .and_then(|key| key.id.as_deref())
            .map(|id| MessageId(id.to_string()))
            .unwrap_or_else(|| MessageId(info.id.to_string()));

        let new_text = wa_msg
            .protocol_message
            .as_option()
            .and_then(|pm| pm.edited_message.as_option())
            .and_then(|m| m.get_base_message().text_content())
            .or_else(|| wa_msg.get_base_message().text_content())
            .unwrap_or("")
            .to_string();

        if new_text.is_empty() {
            return None;
        }

        if let Some(updated) = self.edit_message_text(chat, &target_id, &new_text) {
            return Some(updated);
        }

        if self.pending_edits.len() < PENDING_EDIT_LIMIT {
            self.pending_edits
                .insert((target_id, chat.clone()), new_text);
        }
        None
    }

    /// Replay the edits held in `pending_edits` whose base message is now
    /// cached, returning each edited message for broadcast. Called after every
    /// ingest so an edit that arrived before its base converges as soon as the
    /// message lands, whether that is a live batch or a history-sync chunk.
    pub(super) fn apply_deferred_edits(&mut self, chat: &ChatId) -> Vec<Message> {
        // Resolved before applying: an edit mutates `history` and drops its
        // own `pending_edits` entry, so the candidate set is collected first.
        let ready: Vec<(MessageId, String)> = self
            .pending_edits
            .iter()
            .filter(|((_, pending_chat), _)| pending_chat == chat)
            .filter(|((id, _), _)| {
                self.history
                    .get(chat)
                    .is_some_and(|h| h.iter().any(|m| &m.message_id == id))
            })
            .map(|((id, _), text)| (id.clone(), text.clone()))
            .collect();

        let mut applied = Vec::new();
        for (id, text) in ready {
            if let Some(updated) = self.edit_message_text(chat, &id, &text) {
                applied.push(updated);
                self.pending_edits.remove(&(id, chat.clone()));
            }
        }
        applied
    }

    /// Apply a pin/unpin event for a chat, keyed by its change timestamp.
    /// WhatsApp full-sync replays every chat's historical pin actions in
    /// arbitrary order, so bare last-write-wins would leave `fixed` at
    /// whatever mutation arrived last; the newest change-time always wins
    /// here, making the replay converge to the phone's current pin state.
    /// The pin state is recorded even when no row exists yet (pins commonly
    /// replay before the first history sync lands) so a later
    /// `upsert_conversation` can honor it.
    ///
    /// Returns `true` when the chat row's `fixed` flag actually changed.
    pub(super) fn apply_pin(&mut self, chat: &ChatId, pinned: bool, ts_millis: i64) -> bool {
        let ChatId::WhatsApp(key) = chat else {
            return false;
        };

        let stale = self
            .pin_state
            .get(key)
            .is_some_and(|(applied_ts, _)| *applied_ts >= ts_millis);
        if stale {
            return false;
        }

        self.pin_state.insert(key.clone(), (ts_millis, pinned));

        let Some(chat_row) = self.chats.iter_mut().find(|c| c.id == *chat) else {
            return false;
        };

        if chat_row.fixed == pinned {
            return false;
        }

        chat_row.fixed = pinned;
        true
    }

    /// Insert or update the chat list entry for `chat` from a freshly normalized
    /// message, bumping recency timestamp and unread state. Used by the live
    /// `Event::Messages` path.
    pub(super) fn upsert_chat_from_message(&mut self, chat: ChatId, msg: &Message) {
        let last_message_ts = msg.timestamp;

        match self.chats.iter_mut().find(|c| c.id == chat) {
            Some(c) => {
                c.last_message_ts = Some(last_message_ts);

                if is_self_chat(&chat, self.own_lid.as_deref(), self.own_pn.as_deref()) {
                    c.contact_name = "Myself".to_string();
                    c.verified = false;
                }

                if !msg.from_me {
                    c.unread = true;
                    c.unread_count += 1;
                }
            }
            None => {
                let sender = if msg.from_me {
                    if is_self_chat(&chat, self.own_lid.as_deref(), self.own_pn.as_deref()) {
                        "Myself".to_string()
                    } else {
                        "You".to_string()
                    }
                } else {
                    msg.sender.clone()
                };

                let contact_name = if sender.is_empty() || sender == "Unknown" {
                    match &chat {
                        ChatId::WhatsApp(jid) => Jid::from_str(jid)
                            .map(|j| j.user_base().to_string())
                            .unwrap_or_else(|_| jid.clone()),
                        _ => "Unknown".to_string(),
                    }
                } else {
                    sender
                };

                self.chats.push(Chat {
                    id: chat.clone(),
                    contact_name,
                    last_message_ts: Some(last_message_ts),
                    unread: !msg.from_me,
                    unread_count: if msg.from_me { 0 } else { 1 },
                    ..Default::default()
                });
            }
        }
    }

    /// Upsert the given conversation into the chat list using history-sync
    /// metadata (name, unread, pinned). Preserves any newer `last_message` set
    /// by live data. Conversations that are not really chats on the phone (see
    /// [`should_skip_conversation`]) are not inserted.
    pub(super) fn upsert_conversation(&mut self, conv: &wa::Conversation, chat: ChatId) {
        if let Some(reason) = should_skip_conversation(conv) {
            debug!("WA skipping conversation id={:?} reason={reason}", &conv.id);
            return;
        }

        let (contact_name, verified) =
            if is_self_chat(&chat, self.own_lid.as_deref(), self.own_pn.as_deref()) {
                ("Myself".to_string(), false)
            } else {
                resolve_conversation_name(
                    conv,
                    &self.pushnames,
                    &self.usync_names,
                    &self.usync_verified,
                )
            };

        let unread_count = conv.unread_count.unwrap_or(0) as i32;

        // WhatsApp syncs `pinned` as None in practice (pins arrive only via
        // PinUpdate), so a sync must never clobber an applied pin back to
        // false. Write `fixed` only when the conversation carries a pin
        // value, falling back to any recorded pin state (pins often replay
        // before the row exists and are remembered by `apply_pin`).
        let fixed = conv
            .pinned
            .map(|pinned| pinned > 0)
            .or_else(|| match &chat {
                ChatId::WhatsApp(key) => self.pin_state.get(key).map(|(_, pinned)| *pinned),
                _ => None,
            });

        let exists = self.chats.iter().any(|c| c.id == chat);
        let name_for_log = contact_name.clone();

        // Periodic syncs occasionally carry a group record whose `name` is the
        // raw JID (empty subject on the primary). Never downgrade a known
        // group title to its raw JID — keep the existing name until a record
        // with the real subject arrives.
        let is_raw_jid_name = contact_name == conv.id;

        match self.chats.iter_mut().find(|c| c.id == chat) {
            Some(c) => {
                if !is_raw_jid_name {
                    c.contact_name = contact_name;
                    c.verified = verified;
                }

                // The HistorySync snapshot is the server's authoritative unread
                // state (reading on the phone lowers it there); do not keep a
                // stale grow-only high-water mark that resurrects read chats.
                c.unread_count = unread_count;
                c.unread = unread_count > 0;
                if let Some(fixed) = fixed {
                    c.fixed = fixed;
                }
            }
            None => self.chats.push(Chat {
                id: chat,
                contact_name,
                unread: unread_count > 0,
                unread_count,
                fixed: fixed.unwrap_or(false),
                verified,
                ..Default::default()
            }),
        }
        debug!(
            "WA upsert_conversation id={:?} name={:?} existed={}",
            &conv.id, name_for_log, exists,
        );
    }

    /// Refresh the `last_message_ts` for a chat from its newest stored history
    /// message (used by history sync after ingestion).
    pub(super) fn refresh_last_message_ts(&mut self, chat: &ChatId) {
        let last_message_ts = self
            .history
            .get(chat)
            .and_then(|h| h.iter().max_by_key(|m| m.timestamp))
            .map(|lm| lm.timestamp);

        if let Some(c) = self.chats.iter_mut().find(|c| c.id == *chat) {
            c.last_message_ts = last_message_ts;
        }
    }
}
