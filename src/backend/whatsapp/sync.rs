use crate::backend::{BackendError, BackendEvent, Chat, ChatId, MediaKind, Message, MessageId};
use crate::helpers::{now, relative};

use super::convert::{
    format_pn, name_from_lid_pn, resolve_conversation_name, resolve_sender_name, to_senders_msg,
};
use super::ids::{canonical_chat_id, fold_lid_key, pn_chat_id};
use super::media::wa_media_ref;
use super::state::{SharedState, WhatsAppState};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::broadcast::Sender;
use tracing::{debug, error, info, warn};
use whatsapp_rust::Client;
use whatsapp_rust::prelude::{Jid, MessageField, MessageInfo, Server, wa};
use whatsapp_rust::waproto::whatsapp::HistorySync;
use whatsapp_rust::waproto::whatsapp::Message as WaMessage;

/// Whether a history-sync `Conversation` record is not actually a chat on the
/// phone and should be excluded from the chat list.
///
/// A row that is flagged on the primary as archived / read-only / etc. is
/// still a real thread; only threads the primary itself no longer presents as
/// chats are dropped here. The "never-started LID" case catches ghost rows the
/// primary syncs without any thread metadata (no first/last timestamp, no
/// peer phone number): they are not chats one opened.
pub(super) fn should_skip_conversation(conv: &wa::Conversation) -> Option<&'static str> {
    if conv.is_parent_group.unwrap_or(false) {
        return Some("parent community group (no direct chat)");
    }

    if conv.is_marketing_message_thread.unwrap_or(false) {
        return Some("marketing message thread");
    }

    if conv.pnh_duplicate_lid_thread.unwrap_or(false) {
        return Some("pnh duplicate lid thread");
    }

    if conv.suspended.unwrap_or(false) {
        return Some("suspended conversation");
    }

    if conv.terminated.unwrap_or(false) {
        return Some("terminated conversation");
    }

    if conv.read_only.unwrap_or(false) {
        return Some("read-only (left/declined) conversation");
    }

    let is_lid = conv.id.ends_with("@lid");
    let never_lid_thread = is_lid
        && conv.pn_jid.is_none()
        && conv.conversation_timestamp.is_none()
        && conv.unread_count.is_none();
    if never_lid_thread {
        // LID-only row with no thread activity at all: the phone syncs it as a
        // ghost (a contact seen in groups, an account_id never opened).
        return Some("lid chat never started (ghost row)");
    }

    None
}

/// Remove a chat (and its history + stanza index) from the shared state.
pub(super) async fn remove_chat(state: &SharedState, jid: &Jid) -> Option<Chat> {
    let mut state = state.write().await;
    let chat_id = ChatId::jid_to_chat_id(&jid.to_string());
    let removed = state
        .chats
        .iter()
        .position(|c| c.id == chat_id)
        .map(|i| state.chats.remove(i));
    state.history.remove(&chat_id);
    state.by_stanza_id.retain(|_, (c, _)| c != &chat_id);
    removed
}

/// Merge chat rows keyed by a LID into their phone-keyed twins so a peer never
/// appears twice in the list and old offline `@lid` messages land in the row
/// the user opens. Purely local: resolves each LID from the client's LID↔PN
/// cache, folds history previews/unread, and drops the duplicate.
/// Heal chat rows split across LID/PN keys: one peer must never appear twice.
/// Folds any `@lid` row into its phone-keyed twin (moving unread, preview and
/// history). When `tx`/`cache_path` are given the merge is live-friendly —
/// `ChatRemoved` for the folded row, `ChatUpdated` for the survivor, a
/// `ChatList` republish, and a cache persist — so the running TUI drops the
/// stale row even when a snapshot shrink would otherwise be ignored.
/// Returns the number of rows folded.
pub(super) async fn merge_lid_duplicates(
    client: &Arc<Client>,
    state: &SharedState,
    tx: Option<&Sender<BackendEvent>>,
    cache_path: Option<&Path>,
) -> usize {
    let lids: Vec<String> = {
        let st = state.read().await;
        st.chats
            .iter()
            .filter_map(|c| match &c.id {
                ChatId::WhatsApp(raw) => Jid::from_str(raw)
                    .ok()
                    .filter(|j| j.is_lid())
                    .map(|j| j.user_base().to_string()),
                _ => None,
            })
            .collect()
    };

    let mut merged = 0usize;
    let mut folded: Vec<Chat> = Vec::new();
    for lid_bare in lids {
        let Ok(lid_jid) = Jid::from_str(&format!("{lid_bare}@lid")) else {
            continue;
        };
        let Ok(Some(entry)) = client.get_lid_pn_entry(&lid_jid).await else {
            continue;
        };

        let mut st = state.write().await;
        st.lid_pn
            .insert(lid_bare.clone(), entry.phone_number.to_string());
        let lid_chat = ChatId::WhatsApp(lid_jid.to_non_ad_string());
        let pn_chat = pn_chat_id(&entry.phone_number);
        if !st.chats.iter().any(|c| c.id == pn_chat) {
            continue;
        }

        let Some(orphan) = st
            .chats
            .iter()
            .position(|c| c.id == lid_chat)
            .map(|i| st.chats.remove(i))
            .inspect(|c| folded.push(c.clone()))
        else {
            continue;
        };

        if let Some(pn_row) = st.chats.iter_mut().find(|c| c.id == pn_chat) {
            pn_row.unread = pn_row.unread || orphan.unread;
            pn_row.unread_count = pn_row.unread_count.max(orphan.unread_count);

            pn_row.last_message_ts = pn_row.last_message_ts.max(orphan.last_message_ts);

            pn_row.verified = pn_row.verified || orphan.verified;
            pn_row.fixed = pn_row.fixed || orphan.fixed;
        }

        if let Some(history) = st.history.remove(&lid_chat) {
            let pn_history = st.history.entry(pn_chat.clone()).or_default();
            for m in history {
                if !pn_history.iter().any(|o| o.message_id == m.message_id) {
                    pn_history.push(m);
                }
            }
            pn_history.sort_by_key(|m| m.timestamp);
        }

        st.by_stanza_id.retain(|_, (c, _)| *c != lid_chat);
        merged += 1;
        info!("WA merged duplicate LID chat {lid_bare} into {pn_chat:?}");
    }

    if merged > 0 {
        if let Some(cache_path) = cache_path {
            state.read().await.save_to(cache_path);
        }
        if let Some(tx) = tx {
            for orphan in folded {
                let _ = tx.send(BackendEvent::ChatRemoved { chat: orphan });
            }

            let snapshot = state.read().await.chats.clone();
            let _ = tx.send(BackendEvent::ChatList(snapshot));
            let _ = tx.send(BackendEvent::Status("merged duplicate chats".into()));
        }
    }
    merged
}

/// Find the chat list row for a peer JID, trying the wire key and its
/// phone-keyed twin when the key is a LID.
pub(super) fn find_peer_chat(state: &WhatsAppState, jid: &Jid) -> Option<ChatId> {
    let chat = fold_lid_key(&jid.to_non_ad_string(), state);
    state.chats.iter().any(|c| c.id == chat).then_some(chat)
}

/// Store/lookup keys for a peer's presence state: the wire JID plus its
/// phone-keyed twin when the peer was reached by LID and the mapping is known.
/// [`Event::Presence`] writes every key and `status()` reads every key, so the
/// two always agree.
pub(super) fn presence_keys(jid: &Jid, st: &WhatsAppState) -> Vec<String> {
    let mut keys = vec![jid.to_non_ad_string()];
    if jid.is_lid()
        && let Some(pn_user) = st.lid_pn.get(jid.user_base())
    {
        keys.push(Jid::pn(pn_user).to_non_ad_string());
    }
    keys
}

/// Human label for a peer's known presence state, mirroring the Telegram
/// backend's wording (`"online"` / `"last seen …"`).
pub(super) fn presence_label(online: bool, last_seen: Option<i64>) -> Option<String> {
    if online {
        Some("online".to_string())
    } else {
        last_seen.map(|ts| format!("last seen {}", relative(ts)))
    }
}

/// Learn the phone-number mapping for a LID author outside any state write
/// lock (the client's local LID↔PN lookup awaits sqlite and can deadlock
/// against a held lock). Returns the peer's phone-number user-part once known;
/// non-LID JIDs and already-mapped LIDs return immediately.
pub(super) async fn learn_lid_pn(
    client: &Arc<Client>,
    sender: &Jid,
    state: &SharedState,
) -> Option<String> {
    if !sender.is_lid() {
        return None;
    }
    let bare = sender.user_base().to_string();
    if let Some(pn_user) = state.read().await.lid_pn.get(&bare) {
        return Some(pn_user.clone());
    }

    let Ok(Some(entry)) = client.get_lid_pn_entry(sender).await else {
        return None;
    };

    let pn_user = entry.phone_number.to_string();
    state.write().await.lid_pn.insert(bare, pn_user.clone());

    Some(pn_user)
}

pub(super) async fn handle_history_sync(
    history_sync: &HistorySync,
    client: Option<&Arc<Client>>,
    state: &SharedState,
    tx: &Sender<BackendEvent>,
    cache_path: &Path,
) {
    // Canonicalize every conversation key before taking the write lock: the
    // helper awaits the client's local LID↔PN cache and would otherwise
    // deadlock against a held write lock.
    let mut canonical: Vec<ChatId> = Vec::with_capacity(history_sync.conversations.len());
    for conversation in &history_sync.conversations {
        canonical.push(
            canonical_chat_id(
                client,
                state,
                ChatId::jid_to_chat_id(&conversation.id.to_string()),
            )
            .await,
        );
    }

    // Resolve unmapped LID authors (group participants with no push name) to
    // their phone numbers before the ingest pass names them: the lookup is
    // async over the client's sqlite and must stay outside the write lock.
    if let Some(client) = client {
        let mut need: Vec<Jid> = Vec::new();
        {
            let st = state.read().await;
            let mut seen: HashSet<String> = HashSet::new();
            for conversation in &history_sync.conversations {
                for history in conversation.messages.iter() {
                    let Some(web) = history.message.as_option() else {
                        continue;
                    };
                    let Some(key) = web.key.as_option() else {
                        continue;
                    };
                    if key.from_me.unwrap_or(false) {
                        continue;
                    }
                    if web
                        .push_name
                        .as_deref()
                        .is_some_and(|n| !n.trim().is_empty())
                    {
                        continue;
                    }

                    let sender = key
                        .participant
                        .as_deref()
                        .and_then(|p| Jid::from_str(p).ok())
                        .or_else(|| {
                            web.participant
                                .as_deref()
                                .and_then(|p| Jid::from_str(p).ok())
                        })
                        .unwrap_or_else(|| {
                            Jid::from_str(&conversation.id)
                                .unwrap_or_else(|_| Jid::new(&conversation.id, Server::Pn))
                        });

                    if sender.is_lid()
                        && sender.user_base().is_ascii()
                        && !st.lid_pn.contains_key(sender.user_base())
                        && seen.insert(sender.user_base().to_string())
                    {
                        need.push(sender);
                    }
                }
            }
        }
        for sender in &need {
            learn_lid_pn(client, sender, state).await;
        }
    }

    let shared = state;
    let mut state = state.write().await;

    if let Some(client) = client {
        state.learn_own_identity(client);
    }

    if state.sync_seen_at.is_none() {
        state.sync_seen_at = Some(now());
    }

    debug!(
        "WA handle_history_sync: {} conversations, {} pushnames",
        history_sync.conversations.len(),
        history_sync.pushnames.len(),
    );

    // Index the push names so both conversation and per-message resolutions can
    // reach them. Newer syncs overwrite stale entries keyed by the same JID.
    for pn in history_sync.pushnames.iter() {
        if let (Some(id), Some(name)) = (pn.id.as_deref(), pn.pushname.as_deref())
            && !name.trim().is_empty()
        {
            state.pushnames.insert(id.to_string(), name.to_string());
        }
    }

    for (conversation, chat) in history_sync.conversations.iter().zip(canonical.iter()) {
        let chat = chat.clone();

        state.upsert_conversation(conversation, chat.clone());

        let mut new_messages: Vec<(String, Message)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        debug!(
            "WA conversation id={:?} name={:?} messages={} unread={:?} pinned={:?}",
            &conversation.id,
            resolve_conversation_name(
                conversation,
                &state.pushnames,
                &state.usync_names,
                &state.usync_verified,
            )
            .0,
            conversation.messages.len(),
            conversation.unread_count,
            conversation.pinned,
        );

        // Collect messages (WhatsApp may stream them newest-first or across
        // chunks); chronological order is restored below by timestamp.
        for history in conversation.messages.iter() {
            let Some(web) = history.message.as_option() else {
                continue;
            };
            let Some(key) = web.key.as_option() else {
                continue;
            };
            let Some(stanza_id) = key.id.as_deref() else {
                continue;
            };
            let Some(msg) = web.message.as_option() else {
                continue;
            };

            // Dedupe within this sync chunk (status V3, repeats, etc.).
            if !seen.insert(stanza_id.to_string()) {
                continue;
            }

            // Keep the message's CDN reference while it may still be opened
            // with media, so the image/audio popup can re-download it on
            // demand. Persisted so it survives restarts too; only the fetch
            // fields are kept, never the media bytes. Captured BEFORE the
            // `already` skip below: caches written before audio refs existed
            // hold none, and a message already in `state.history` would
            // otherwise be skipped forever and stay unfetchable.
            if let Some(media_ref) = wa_media_ref(msg) {
                state.media_refs.insert(stanza_id.to_string(), media_ref);
            }

            let already = state
                .history
                .get(&chat)
                .map(|h| h.iter().any(|m| &*m.message_id == stanza_id))
                .unwrap_or(false);

            if already {
                continue;
            }

            let from_me = key.from_me.unwrap_or(false);

            let sender_jid = key
                .participant
                .as_deref()
                .and_then(|p| Jid::from_str(p).ok())
                .or_else(|| {
                    web.participant
                        .as_deref()
                        .and_then(|p| Jid::from_str(p).ok())
                })
                .unwrap_or_else(|| {
                    // Own messages and one-to-one chats are authored by the chat
                    // itself; the per-message participant slot is absent.
                    if !from_me && key.participant.is_none() && web.participant.is_none() {
                        error!(
                            "WA group message stanza_id={} has no participant identity \
                             (key or web) and is not from_me; falling back to group JID",
                            stanza_id,
                        );
                    }
                    Jid::from_str(&conversation.id)
                        .unwrap_or_else(|_| Jid::new(&conversation.id, Server::Pn))
                });

            let info = history_info(
                &conversation.id,
                stanza_id,
                from_me,
                &sender_jid,
                web,
                &state.pushnames,
                &state.lid_pn,
            );

            debug!(
                chat = %conversation.id,
                stanza_id = %stanza_id,
                from_me = from_me,
                key_participant = ?key.participant.as_deref(),
                msg_push_name = ?web.push_name.as_deref(),
                sender_jid = %sender_jid,
                resolved_sender = %info.push_name,
                already_cached = already,
                "WA history message sender resolution"
            );

            let mut normalized = to_senders_msg(chat.clone(), &info, msg, &state);
            normalized.timestamp = web
                .message_timestamp
                .map(|ts| ts as i64)
                .unwrap_or_else(now);

            if normalized.text.is_empty() && normalized.media.is_none() {
                continue;
            }

            new_messages.push((stanza_id.to_string(), normalized));
        }

        // Revert to chronological (oldest first) regardless of the order the
        // sync chunk delivered them in. WhatsApp may stream a conversation
        // newest-first or split across chunks; sorting by timestamp is the only
        // order guarantee we need to expose to the TUI.
        new_messages.sort_by_key(|(_, m)| m.timestamp);

        // Only accept a history-sync message if we do not already hold a newer
        // copy: older sync chunks must not overwrite live data.
        for (stanza_id, normalized) in new_messages {
            let dup = state
                .history
                .get(&chat)
                .map(|h| h.iter().any(|m| m.message_id == normalized.message_id))
                .unwrap_or(false);
            if dup {
                continue;
            }
            state.by_stanza_id.insert(
                stanza_id.clone(),
                (chat.clone(), normalized.message_id.clone()),
            );
            state
                .history
                .entry(chat.clone())
                .or_default()
                .push(normalized);
        }

        // Always finish with an up-to-date preview.
        state.refresh_last_message_ts(&chat);
    }

    // Re-resolve senders the ingest froze bare before the pushnames bundle
    // landed, and persist any repairs with the newly learned names.
    let repaired = state.resolve_stale_senders();
    let chats_total = state.chats.len();
    let chats_snapshot = state.chats.clone();

    drop(state);

    if repaired > 0 {
        info!("WA resolved {repaired} stale sender name(s)");
        shared.read().await.save_to(cache_path);
    }

    info!("WA handle_history_sync done: chats={}", chats_total);

    // Publish the whole dialog list as a single snapshot so a large history
    // sync is not flood-sent as hundreds of tiny ChatUpdated events over the
    // bounded broadcast channel.
    let _ = tx.send(BackendEvent::ChatList(chats_snapshot));
    let _ = tx.send(BackendEvent::Status("history synced".into()));

    // Second pass: a LID conversation ingested here may have canonicalized
    // before its phone-keyed twin existed, leaving two rows for one peer.
    // Fold them now that every row from this sync is present, persisting the
    // learned LID↔PN mappings and broadcasting the removal.
    if let Some(client) = client {
        merge_lid_duplicates(client, shared, Some(tx), Some(cache_path)).await;
    }
}

/// One round of usync enrichment for 1:1 chats that still show a bare number or
/// LID (the phone declined to sync a name for them via pushnames). Runs a
/// single `is_on_whatsapp` batch for every candidate, stores the learned name
/// (the peer's username, verified business name, or failing those the formatted
/// phone number) under both the peer LID and PN keys, refreshes the cached chat
/// rows, and persists the cache. Already-queried peers are skipped so a contact
/// without a username/business name is not re-queried on every history sync.
pub(super) async fn enrich_chat_names(
    client: &Arc<Client>,
    state: &SharedState,
    cache_path: &Path,
    tx: &Sender<BackendEvent>,
) {
    let candidates: Vec<Jid> = {
        let st = state.read().await;
        let own: Vec<String> = [client.pn(), client.lid()]
            .into_iter()
            .flatten()
            .map(|j| j.to_non_ad_string())
            .collect();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for chat in st.chats.iter() {
            let ChatId::WhatsApp(raw) = &chat.id else {
                continue;
            };
            let Ok(jid) = Jid::from_str(raw) else {
                continue;
            };
            if !jid.is_pn() && !jid.is_lid() {
                continue;
            }
            let key = jid.to_non_ad_string();
            if own.contains(&key)
                || st.usync_names.contains_key(raw)
                || st.usync_attempted.contains(raw)
                || !seen.insert(key)
            {
                continue;
            }
            out.push(jid);
        }
        out
    };

    if candidates.is_empty() {
        return;
    }

    debug!("WA usync enrichment: {}-chat batch", candidates.len());
    let mut updated = false;

    for chunk in candidates.chunks(100) {
        match client.contacts().is_on_whatsapp(chunk).await {
            Ok(results) => {
                // Read the push-name cache once for the whole batch; the write
                // lock is not held while awaiting the local LID→PN lookups below.
                let pushnames = state.read().await.pushnames.clone();
                let mut applied: Vec<(String, String, bool, Option<String>)> = Vec::new();

                for result in results {
                    let asked_key = result.jid.to_non_ad_string();
                    let pn_key = result.pn_jid.as_ref().map(|p| p.to_non_ad_string());
                    let mut name = result
                        .username
                        .as_deref()
                        .filter(|u| !u.trim().is_empty())
                        .map(|u| u.to_string())
                        .or_else(|| {
                            result
                                .verified_name
                                .as_ref()
                                .and_then(|v| v.name.clone())
                                .filter(|n| !n.trim().is_empty())
                        })
                        .or_else(|| {
                            // No profile name: a LID-keyed chat still wins a
                            // readable fallback when usync disclosed the peer's
                            // phone number.
                            result
                                .pn_jid
                                .as_ref()
                                .filter(|_| result.jid.is_lid())
                                .map(|pn| format_pn(pn.user_base()))
                        });

                    // usync hides the phone number of non-business 1:1 peers
                    // (privacy), so a username-less LID chat gets nothing from
                    // the server. Consult the client's local LID↔PN cache
                    // instead — it is populated from past message traffic — and
                    // fall back to the formatted number (or a push name when
                    // one happens to be cached under that PN).
                    if name.is_none() && result.jid.is_lid() {
                        match client.get_lid_pn_entry(&result.jid).await {
                            Ok(Some(entry)) => {
                                name = Some(name_from_lid_pn(&entry.phone_number, &pushnames));
                            }
                            Ok(None) => {}
                            Err(e) => {
                                warn!("WA LID→PN lookup failed for {asked_key}: {e}");
                            }
                        }
                    }

                    let Some(name) = name.filter(|n| !n.trim().is_empty()) else {
                        continue;
                    };

                    let business = result.is_business && result.verified_name.is_some();
                    applied.push((asked_key, name, business, pn_key));
                }

                let mut st = state.write().await;
                for (asked_key, name, business, pn_key) in applied {
                    st.usync_attempted.insert(asked_key.clone());
                    let mut keys = vec![asked_key];
                    if let Some(pn_key) = pn_key {
                        keys.push(pn_key);
                    }
                    for key in &keys {
                        st.usync_names.insert(key.clone(), name.clone());
                        if business {
                            st.usync_verified.insert(key.clone());
                        }
                    }
                    for key in &keys {
                        if let Some(chat) = st
                            .chats
                            .iter_mut()
                            .find(|c| matches!(&c.id, ChatId::WhatsApp(raw) if raw == key))
                        {
                            chat.contact_name = name.clone();
                            chat.verified = business;
                        }
                    }
                }
                updated = true;
            }
            Err(err) => {
                warn!(
                    "WA usync name enrichment failed for {} JIDs: {err}",
                    chunk.len(),
                );
            }
        }
    }
    if updated {
        // Learned names apply to stored messages too: re-resolve any senders
        // frozen bare when their history synced before usync knew them, then
        // persist one snapshot covering chats and messages.
        let mut st = state.write().await;
        let repaired = st.resolve_stale_senders();
        let chats_snapshot = st.chats.clone();

        st.save_to(cache_path);

        drop(st);

        if repaired > 0 {
            info!("WA usync resolved {repaired} stale sender name(s)");
        }

        let _ = tx.send(BackendEvent::ChatList(chats_snapshot));
        let _ = tx.send(BackendEvent::Status("names updated".into()));

        info!("WA usync name enrichment finished");
    }
}

/// Minimal [`MessageInfo`] for messages recovered from a HistorySync
/// conversation. Sets the authoritative per-message push name and author JID so
/// group senders render with their contact name rather than the group title.
fn history_info(
    chat: &str,
    stanza_id: &str,
    from_me: bool,
    sender_jid: &Jid,
    web: &wa::WebMessageInfo,
    pushnames: &HashMap<String, String>,
    lid_pn: &HashMap<String, String>,
) -> MessageInfo {
    let mut info = MessageInfo {
        id: stanza_id.into(),
        ..Default::default()
    };

    info.source.chat = Jid::from_str(chat).unwrap_or_else(|_| Jid::new(chat, Server::Pn));
    info.source.is_from_me = from_me;
    info.source.sender = sender_jid.clone();
    info.push_name = resolve_sender_name(
        web.push_name.as_deref().unwrap_or_default(),
        sender_jid,
        pushnames,
        lid_pn,
    )
    .into();
    info
}

/// The CDN/crypto fields a WhatsApp upload returns, mirrored from whatsapp-rust's
/// `UploadResponse` (which is `#[non_exhaustive]`, so it cannot be constructed
/// in tests or decomposed here).
pub(super) struct CdnFields {
    pub(super) url: String,
    pub(super) direct_path: String,
    pub(super) media_key: [u8; 32],
    pub(super) file_enc_sha256: [u8; 32],
    pub(super) file_sha256: [u8; 32],
    pub(super) file_length: u64,
    pub(super) media_key_timestamp: i64,
    pub(super) streaming_sidecar: Option<Vec<u8>>,
}

/// Build the outbound `wa::Message` for uploaded bytes, mapping the
/// provider-neutral kind onto WhatsApp's media sub-protos and carrying the
/// reply context. Deterministic given the CDN fields, so the mapping is
/// testable offline.
pub(super) fn outbound_media_message(
    kind: MediaKind,
    cdn: CdnFields,
    file_name: &str,
    caption: Option<&str>,
    reply_to: Option<&MessageId>,
) -> Result<WaMessage, BackendError> {
    let CdnFields {
        url,
        direct_path,
        media_key,
        file_enc_sha256,
        file_sha256,
        file_length,
        media_key_timestamp,
        streaming_sidecar,
    } = cdn;

    let caption = caption.map(str::to_string);
    let context_info = reply_to.map(|id| {
        Box::new(wa::ContextInfo {
            stanza_id: Some(id.to_string()),
            ..Default::default()
        })
    });

    let message = match kind {
        MediaKind::Image => WaMessage {
            image_message: MessageField::some(wa::message::ImageMessage {
                url: Some(url),
                direct_path: Some(direct_path),
                media_key: Some(media_key.to_vec()),
                file_enc_sha256: Some(file_enc_sha256.to_vec()),
                file_sha256: Some(file_sha256.to_vec()),
                file_length: Some(file_length),
                media_key_timestamp: Some(media_key_timestamp),
                mimetype: Some("image/jpeg".into()),
                caption,
                context_info: context_info
                    .map(|ci| MessageField::some(*ci))
                    .unwrap_or_default(),
                ..Default::default()
            }),
            ..Default::default()
        },
        MediaKind::Video => WaMessage {
            video_message: MessageField::some(wa::message::VideoMessage {
                url: Some(url),
                direct_path: Some(direct_path),
                media_key: Some(media_key.to_vec()),
                file_enc_sha256: Some(file_enc_sha256.to_vec()),
                file_sha256: Some(file_sha256.to_vec()),
                file_length: Some(file_length),
                media_key_timestamp: Some(media_key_timestamp),
                streaming_sidecar,
                mimetype: Some("video/mp4".into()),
                caption,
                context_info: context_info
                    .map(|ci| MessageField::some(*ci))
                    .unwrap_or_default(),
                ..Default::default()
            }),
            ..Default::default()
        },
        MediaKind::Audio {
            duration_secs,
            is_voice,
            waveform,
        } => WaMessage {
            audio_message: MessageField::some(wa::message::AudioMessage {
                url: Some(url),
                direct_path: Some(direct_path),
                media_key: Some(media_key.to_vec()),
                file_enc_sha256: Some(file_enc_sha256.to_vec()),
                file_sha256: Some(file_sha256.to_vec()),
                file_length: Some(file_length),
                media_key_timestamp: Some(media_key_timestamp),
                streaming_sidecar,
                seconds: duration_secs,
                ptt: Some(is_voice),
                waveform,
                mimetype: Some("audio/ogg; codecs=opus".into()),
                context_info: context_info
                    .map(|ci| MessageField::some(*ci))
                    .unwrap_or_default(),
                ..Default::default()
            }),
            ..Default::default()
        },
        MediaKind::Document => WaMessage {
            document_message: MessageField::some(wa::message::DocumentMessage {
                url: Some(url),
                direct_path: Some(direct_path),
                media_key: Some(media_key.to_vec()),
                file_enc_sha256: Some(file_enc_sha256.to_vec()),
                file_sha256: Some(file_sha256.to_vec()),
                file_length: Some(file_length),
                media_key_timestamp: Some(media_key_timestamp),
                mimetype: Some("application/octet-stream".into()),
                file_name: Some(file_name.to_string()),
                caption,
                context_info: context_info
                    .map(|ci| MessageField::some(*ci))
                    .unwrap_or_default(),
                ..Default::default()
            }),
            ..Default::default()
        },
        MediaKind::Sticker => WaMessage {
            sticker_message: MessageField::some(wa::message::StickerMessage {
                url: Some(url),
                direct_path: Some(direct_path),
                media_key: Some(media_key.to_vec()),
                mimetype: Some("image/webp".into()),
                file_length: Some(file_length),
                media_key_timestamp: Some(media_key_timestamp),
                context_info: context_info
                    .map(|ci| MessageField::some(*ci))
                    .unwrap_or_default(),
                ..Default::default()
            }),
            ..Default::default()
        },
        MediaKind::Unsupported => {
            return Err(BackendError::Other(
                "WhatsApp: unsupported media kind".into(),
            ));
        }
    };
    Ok(message)
}
