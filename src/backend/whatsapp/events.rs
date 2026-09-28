use crate::backend::{BackendError, BackendEvent, ChatId, MessageId};

use super::convert::to_senders_msg;
use super::ids::{canonical_chat_id, own_chat_id};
use super::media::wa_media_ref;
use super::messenger::WhatsAppMessenger;
use super::state::SharedState;
use super::sync::{
    enrich_chat_names, find_peer_chat, handle_history_sync, learn_lid_pn, presence_keys,
    presence_label, remove_chat,
};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::sync::broadcast::Sender;
use tracing::{debug, info, warn};
use whatsapp_rust::Client;
use whatsapp_rust::prelude::{Event, Server};
use whatsapp_rust::wacore::types::message::EditAttribute;
use whatsapp_rust::wacore_binary::JidExt;

impl WhatsAppMessenger {
    /// Called once when the underlying server connection is established.
    /// Past here, message routing is done by `handle_event`.
    pub(super) fn handle_connect(
        client: &Arc<Client>,
        tx: &Sender<BackendEvent>,
        state: &SharedState,
        cache_path: &Path,
    ) {
        info!("WhatsApp connected");

        let client = client.clone();
        let tx_for_enrich = tx.clone();
        let state = state.clone();
        let cache_path = cache_path.to_path_buf();

        tokio::spawn(async move {
            // Backfill names for chats already cached (or seeded from the TUI)
            // right away; this also republishes the list so stale names get
            // replaced without waiting for a sync.
            enrich_chat_names(&client, &state, &cache_path, &tx_for_enrich).await;
            // Note: no `request_syncd_snapshot_recovery` here. That call asks
            // the primary to re-send the *app-state* `regular_high` collection
            // (contact/chats metadata), not conversation history, and every
            // response in practice failed to decompress ("data error"). A full
            // history backfill (which re-delivers conversations received while
            // we were closed, CDN media fields included) is negotiated via the
            // `require_full_sync` device prop instead.
        });

        let _ = tx.send(BackendEvent::Connected);
    }

    /// Called when a QR code is required for pairing.
    pub(super) async fn handle_qr(
        code: &str,
        timeout: std::time::Duration,
        tx: &Sender<BackendEvent>,
        current_qr: &Arc<RwLock<Option<String>>>,
    ) {
        info!("WhatsApp QR code (valid {}s)", timeout.as_secs());
        *current_qr.write().await = Some(code.to_string());
        let _ = tx.send(BackendEvent::QrCode(code.to_string()));
    }

    /// Called when the WhatsApp session is logged out / invalidated.
    pub(super) fn handle_logged_out(tx: &Sender<BackendEvent>) {
        warn!("WhatsApp logged out");
        let _ = tx.send(BackendEvent::Disconnected(
            "WhatsApp logged out".to_string(),
        ));
    }

    /// Dispatch a routed `Event` coming from the server stream.
    /// Also handles reconnect/QR state and the chat list mutations (deletion,
    /// clear, mute/archive/pin/mark-as-read) that arrive over the syncd channel.
    pub(super) async fn handle_event(
        event: &Arc<Event>,
        client: &Arc<Client>,
        state: &SharedState,
        tx: &Sender<BackendEvent>,
        current_qr: &Arc<RwLock<Option<String>>>,
        cache_path: &Path,
    ) {
        match &**event {
            Event::Connected(_) => {
                let _ = tx.send(BackendEvent::Connected);
            }
            Event::Disconnected(e) => {
                let _ = tx.send(BackendEvent::Disconnected(format!(
                    "WhatsApp: Disconnected! {:?}",
                    e
                )));
            }
            Event::StreamError(e) => {
                let err = format!("WhatsApp: ! {:?}", e);
                let _ = tx.send(BackendEvent::Error(err.clone(), BackendError::Other(err)));
            }
            Event::ConnectFailure(e) => {
                let err = format!("WhatsApp: ! {:?}", e);
                let _ = tx.send(BackendEvent::Error(err.clone(), BackendError::Other(err)));
            }
            Event::PairingQrCode(q) => {
                *current_qr.write().await = Some(q.code.clone());
                let _ = tx.send(BackendEvent::QrCode(q.code.clone()));
            }
            Event::PairSuccess(_) => {
                let _ = tx.send(BackendEvent::QrCode(String::from("ok")));
            }
            Event::Messages(batch) => {
                // Learn the account's own JIDs for self-chat labelling; cheap
                // and idempotent, and live messages may precede the first
                // history sync.
                state.write().await.learn_own_identity(client);
                for inbound in batch.messages.iter() {
                    debug!(
                        "Handling inbound WP Message: push_name={:?} from_me={} source - {:?}",
                        inbound.info.push_name, inbound.info.source.is_from_me, inbound.info.source,
                    );

                    // Skip status/24h broadcasts and any
                    // message with no payload: they do not
                    // belong in the chat list.
                    if inbound.info.source.chat.server == Server::Broadcast
                        || inbound.info.source.chat.is_status_broadcast()
                    {
                        continue;
                    }

                    let chat = own_chat_id(
                        client.lid().as_ref(),
                        client.pn().as_ref(),
                        ChatId::jid_to_chat_id(&inbound.info.source.chat.to_string()),
                        inbound.info.source.is_from_me,
                    );

                    // Live messages are keyed by the peer's LID; fold to the
                    // phone-keyed row the user opens before touching state.
                    let chat = canonical_chat_id(Some(client), state, chat).await;

                    // Resolve a LID author (group message) to a phone number
                    // before the ingest names it; the lookup is async over the
                    // client's sqlite and must stay outside the write lock.
                    if !inbound.info.source.is_from_me {
                        learn_lid_pn(client, &inbound.info.source.sender, state).await;
                    }

                    let mut state = state.write().await;

                    // A WhatsApp edit re-sends the ORIGINAL stanza id with an
                    // edit payload: update the stored message instead of
                    // treating it as a new or duplicate message. The batch-level
                    // save below persists the change.
                    if inbound.info.edit == EditAttribute::MessageEdit {
                        let updated = state
                            .apply_message_edit(&chat, &inbound.info, &inbound.message)
                            .map(BackendEvent::MessageUpdated);
                        drop(state);
                        if let Some(ev) = updated {
                            let _ = tx.send(ev);
                        }
                        continue;
                    }

                    let msg = to_senders_msg(chat.clone(), &inbound.info, &inbound.message, &state);

                    if msg.text.is_empty() && msg.media.is_none() {
                        drop(state);
                        continue;
                    }

                    let stanza_id = inbound.info.id.to_string();

                    state
                        .by_stanza_id
                        .insert(stanza_id.clone(), (chat.clone(), msg.message_id.clone()));

                    if let Some(media_ref) = wa_media_ref(&inbound.message) {
                        state.media_refs.insert(stanza_id.clone(), media_ref);
                    }

                    let is_dup = state
                        .history
                        .get(&chat)
                        .map(|h| h.iter().any(|m| m.message_id == msg.message_id))
                        .unwrap_or(false);

                    if !is_dup {
                        state
                            .history
                            .entry(chat.clone())
                            .or_default()
                            .push(msg.clone());
                    }

                    state.upsert_chat_from_message(chat.clone(), &msg);

                    drop(state);
                    let _ = tx.send(BackendEvent::MessageReceived(msg));
                }
                state.read().await.save_to(cache_path);
            }
            Event::HistorySync(hs) => {
                info!(
                    "WA HistorySync event received sync_type={} order={:?} progress={:?} bytes={}",
                    hs.sync_type(),
                    hs.chunk_order(),
                    hs.progress(),
                    hs.compressed_bytes().len(),
                );

                match hs.get() {
                    Some(parsed) => {
                        info!(
                            "WA HistorySync decoded conversations={} pushnames={}",
                            parsed.conversations.len(),
                            parsed.pushnames.len(),
                        );
                        handle_history_sync(parsed, Some(client), state, tx, cache_path).await;
                        state.read().await.save_to(cache_path);

                        // Backfill names the phone did not sync (pushnames) from
                        // usync, for chat partners without a saved contact name.
                        let client = client.clone();
                        let state = state.clone();
                        let cache_path = cache_path.to_path_buf();
                        let tx = tx.clone();
                        tokio::spawn(async move {
                            enrich_chat_names(&client, &state, &cache_path, &tx).await;
                        });
                    }
                    None => {
                        warn!(
                            "WA HistorySync decode FAILED sync_type={} bytes={}",
                            hs.sync_type(),
                            hs.compressed_bytes().len(),
                        );
                    }
                }
            }
            Event::DeleteChatUpdate(u) => {
                if let Some(chat) = remove_chat(state, &u.jid).await {
                    let _ = tx.send(BackendEvent::ChatRemoved { chat });
                }
            }
            Event::ClearChatUpdate(u) => {
                let chat_id = ChatId::jid_to_chat_id(&u.jid.to_string());
                let mut state = state.write().await;
                let ids: Vec<MessageId> = state
                    .history
                    .get(&chat_id)
                    .map(|h| h.iter().map(|m| m.message_id.clone()).collect())
                    .unwrap_or_default();
                if ids.is_empty() {
                    return;
                }
                if let Some(history) = state.history.get_mut(&chat_id) {
                    history.clear();
                }
                state.refresh_last_message_ts(&chat_id);
                drop(state);
                let _ = tx.send(BackendEvent::MessageDeleted {
                    chat: Some(chat_id),
                    message_ids: ids,
                });
            }
            Event::DeleteMessageForMeUpdate(u) => {
                let chat_id = ChatId::jid_to_chat_id(&u.chat_jid.to_string());
                let id = MessageId(u.message_id.clone());
                let mut state = state.write().await;
                let mut removed = false;
                if let Some(history) = state.history.get_mut(&chat_id) {
                    let before = history.len();
                    history.retain(|m| m.message_id != id);
                    removed = history.len() != before;
                }
                if removed {
                    state.refresh_last_message_ts(&chat_id);
                    drop(state);
                    let _ = tx.send(BackendEvent::MessageDeleted {
                        chat: Some(chat_id),
                        message_ids: vec![id],
                    });
                }
            }
            Event::MuteUpdate(u) => {
                let mut state = state.write().await;
                if let Some(chat) = state
                    .chats
                    .iter_mut()
                    .find(|c| c.id == ChatId::jid_to_chat_id(&u.jid.to_string()))
                {
                    chat.status = Some(if u.action.muted.unwrap_or(false) {
                        "muted".to_string()
                    } else {
                        "".to_string()
                    });
                    let _ = tx.send(BackendEvent::ChatUpdated(chat.clone()));
                }
            }
            Event::PinUpdate(u) => {
                debug!("Pinning event! {:?}", u);
                // WhatsApp pins 1:1 threads by LID on the wire, but the chat
                // list rows are canonicalized to their phone-keyed twins; fold
                // the pin JID the same way live messages are folded (async
                // LID↔PN lookup must stay outside the write lock).
                let chat = canonical_chat_id(
                    Some(client),
                    state,
                    ChatId::jid_to_chat_id(&u.jid.to_string()),
                )
                .await;

                let mut guard = state.write().await;

                // Full-sync pin replays deliver historical mutations out of
                // order; only the newest change timestamp per chat is
                // authoritative, so a stale unpin can never clobber the
                // current pin (see `WhatsAppState::apply_pin`).
                let pinned = u.action.pinned.unwrap_or(false);
                let changed = guard.apply_pin(&chat, pinned, u.timestamp.timestamp_millis());

                if changed {
                    // Republish the whole list so the TUI reflects both pin
                    // and unpin; `ChatUpdated` via `upsert_chat` can only
                    // ever set `fixed = true` and could not clear one.
                    let snapshot = guard.chats.clone();
                    drop(guard);
                    state.read().await.save_to(cache_path);

                    let _ = tx.send(BackendEvent::ChatList(snapshot));
                }
            }
            Event::ArchiveUpdate(u) => {
                let mut state = state.write().await;
                if let Some(chat) = state
                    .chats
                    .iter_mut()
                    .find(|c| c.id == ChatId::jid_to_chat_id(&u.jid.to_string()))
                {
                    chat.status = Some(if u.action.archived.unwrap_or(false) {
                        "archived".to_string()
                    } else {
                        "".to_string()
                    });
                    let _ = tx.send(BackendEvent::ChatUpdated(chat.clone()));
                }
            }
            Event::MarkChatAsReadUpdate(u) => {
                let mut guard = state.write().await;
                if let Some(chat) = guard
                    .chats
                    .iter_mut()
                    .find(|c| c.id == ChatId::jid_to_chat_id(&u.jid.to_string()))
                {
                    chat.unread = false;
                    chat.unread_count = 0;
                    let _ = tx.send(BackendEvent::UnreadUpdated {
                        chat: chat.id.clone(),
                        unread: false,
                        unread_count: 0,
                    });
                }
                drop(guard);
                state.read().await.save_to(cache_path);
            }
            Event::OfflineSyncPreview(p) => {
                info!(
                    "WA offline sync started: {} items ({} messages)",
                    p.total, p.messages
                );
            }
            Event::OfflineSyncInterrupted(i) => {
                warn!(
                    "WA offline sync interrupted: {}/{} items delivered",
                    i.delivered, i.total
                );
            }
            Event::OfflineSyncCompleted(c) => {
                info!("WA offline sync completed: {} items", c.count);
                // The drain replayed live messages straight into history; re-run
                // name enrichment and republish the list so everything surfaced
                // immediately even when the snapshot PDO fails to decompress.
                let client = client.clone();
                let state_for_enrich = state.clone();
                let cache_path = cache_path.to_path_buf();
                let tx_for_enrich = tx.clone();

                tokio::spawn(async move {
                    enrich_chat_names(&client, &state_for_enrich, &cache_path, &tx_for_enrich)
                        .await;
                });

                let st = state.read().await;
                let snapshot = st.chats.clone();

                drop(st);

                let _ = tx.send(BackendEvent::ChatList(snapshot));
            }
            Event::Presence(u) => {
                // TODO: Consider simplifying this:
                // if unavailable {
                // todo!();
                // }
                //
                // Only track peers that have a chat row; unknown LIDs would
                // otherwise grow the map without bounds.
                let online = !u.unavailable;
                let last_seen = u.last_seen.map(|ts| ts.timestamp());
                let label = presence_label(online, last_seen);

                let mut st = state.write().await;
                let Some(chat) = find_peer_chat(&st, &u.from) else {
                    return;
                };
                for key in presence_keys(&u.from, &st) {
                    st.presence.insert(key, (online, last_seen));
                }
                // Surface the learned status in the open chat and chat list; only
                // broadcast when the row actually changes to avoid a flood of
                // identical ChatUpdated events while a peer is active.
                let Some(row) = st.chats.iter_mut().find(|c| c.id == chat) else {
                    return;
                };
                let changed = match (&row.status, &label) {
                    (Some(current), Some(next)) => current != next,
                    (Some(_), None) => true,
                    (None, None) => false,
                    (None, Some(_)) => true,
                };
                if changed {
                    row.status = label;
                    let updated = row.clone();
                    drop(st);
                    let _ = tx.send(BackendEvent::ChatUpdated(updated));
                }
            }
            other => {
                debug!(
                    "Unhandled WhatsApp event: {:?}, {:?}",
                    std::mem::discriminant(other),
                    other
                );
            }
        }
    }
}
