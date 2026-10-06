use crate::backend::{
    AuthSteps, BackendError, BackendEvent, Chat, ChatId, LoginStepState, MediaKind, Message,
    MessageId, MessageMedia, Messenger, OutboundMessage, ReplyContext,
};
use crate::config::Config;
use crate::helpers::now;

use super::convert::message_actions;
use super::ids::fold_lid_key;
use super::media::{ref_media_type, wa_media_ref};
use super::state::{SharedState, WhatsAppState};
use super::sync::{
    CdnFields, find_peer_chat, merge_lid_duplicates, outbound_media_message, presence_keys,
    presence_label,
};
use super::transport::WebSocketTransportFactory;
use std::collections::HashSet;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::broadcast::Sender;
use tokio::sync::{RwLock, broadcast};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use whatsapp_rust::Client;
use whatsapp_rust::UploadOptions;
use whatsapp_rust::download::DownloadParams;
use whatsapp_rust::prelude::{Bot, EventDelivery, Jid, MessageBuilderExt, SqliteStore, wa};
use whatsapp_rust::wacore::download::MediaType;
use whatsapp_rust::wacore::store::DevicePropsOverride;
use whatsapp_rust::wacore_binary::JidExt;
use whatsapp_rust::waproto::whatsapp::device_props::{HistorySyncConfig, PlatformType};

#[derive(Clone)]
pub struct WhatsAppMessenger {
    client: Arc<Client>,
    /// The configured-but-not-yet-started bot. It is intentionally not spawned
    /// in `new()`: only [`Messenger::start`] claims it, which is what keeps a
    /// dormant provider free of any transport. Once claimed it is `None`, so a
    /// second `start()` is a no-op.
    bot: Arc<Mutex<Option<Bot>>>,
    run_task: Arc<Mutex<Option<JoinHandle<()>>>>,
    tx: Sender<BackendEvent>,
    state: SharedState,
    shutdown: Arc<AtomicBool>,
    /// The most recently issued pairing QR payload (if any), kept so the login
    /// screen can query it synchronously on every frame and refresh on expiry.
    current_qr: Arc<RwLock<Option<String>>>,
    /// Path to the JSON cache backing this account's chats/history/pushnames,
    /// derived from the sqlite store path so both survive the same data dir.
    cache_path: PathBuf,
}

impl WhatsAppMessenger {
    /// Open (or create) the sqlite store, build the bot with event callbacks,
    /// and return a messenger in the `constructed/dormant` state.
    ///
    /// Dormant is local-only work: the store, the JSON cache and the client are
    /// all prepared, but no transport is opened and no provider API is called.
    /// [`Messenger::start`] is the only way to reach the network.
    pub async fn new(store_path: String) -> Result<Self, BackendError> {
        let store = SqliteStore::new(&store_path)
            .await
            .map_err(|e| BackendError::Other(format!("WhatsApp: failed to open store: {e}")))?;

        // `wa.db` holds the account's session keys; its `-wal` (write-ahead
        // log) and `-shm` (shared memory) sidecars hold uncommitted pages of
        // the same data. SQLite creates them with the process umask, typically
        // world-readable, so tighten them now — before the first login or sync
        // can write account data into them.
        for suffix in ["", "-wal", "-shm"] {
            let artifact = format!("{store_path}{suffix}");
            if let Err(e) = crate::file::repair_owner_only(std::path::Path::new(&artifact)) {
                warn!("WhatsApp: could not tighten {artifact} permissions: {e}");
            }
        }

        let (tx, _) = broadcast::channel(128);

        // Dedicated WhatsApp cache. Must NOT share the TUI's `chats.json`:
        // that file holds a plain Vec<Chat> (see `file::write_chats`) while this
        // cache holds the full WhatsAppState (history + pushnames + usync), and
        // the two schemas silently clobbered each other, leaving WhatsApp with
        // an empty state on every cold start.
        let chat_cache = Config::user_config_dir()?.join("wp_cache.json");

        let mut state = WhatsAppState::load_from(chat_cache.to_string_lossy().as_ref());

        // First run with the dedicated cache (or a fresh account): seed the
        // chat list from the TUI's persisted rows so usync enrichment can
        // backfill names immediately, even before the first history sync lands.
        if state.chats.is_empty()
            && let Ok(chat_cache) = crate::file::read_chats()
        {
            state.chats = chat_cache
                .into_iter()
                .filter(|c| c.id.tag() == "WA")
                .collect();
        }

        // Cold-start sweep: history synced before the pushnames bundle arrived
        // froze 1:1 senders as bare numbers; re-resolve them now that the
        // cached maps are loaded, and persist the repairs before the UI renders.
        let repaired = state.resolve_stale_senders();
        if repaired > 0 {
            info!("WA startup resolved {repaired} stale sender name(s)");
            state.save_to(&chat_cache);
        }

        let state: SharedState = Arc::new(RwLock::new(state));
        let shutdown = Arc::new(AtomicBool::new(false));
        let current_qr: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));

        let builder = Bot::builder()
            .with_backend(store)
            // Upstream's `tokio-transport` default commits to the
            // single address `getaddrinfo` returns first, so a host that
            // resolves to an unreachable address first never connects; the
            // racing dial and its upstream evidence are documented in
            // `transport.rs`.
            .with_transport_factory(WebSocketTransportFactory::new())
            .with_device_props(
                DevicePropsOverride::new()
                    .with_platform_type(PlatformType::UWP)
                    .with_require_full_sync(true)
                    .with_history_sync_config(HistorySyncConfig {
                        full_sync_days_limit: Some(365),
                        on_demand_ready: Some(true),
                        complete_on_demand_ready: Some(true),
                        ..whatsapp_rust::wacore::store::device::default_history_sync_config()
                    }),
            )
            // Ordered, bounded delivery: every event mutates one shared state,
            // and the guarantees depend on arrival order (an edit must not
            // overtake its base message, a history chunk must not land after
            // the live message it already holds, a clear must not be undone by
            // a later poll). `Concurrent` spawns a task per event, so those
            // outcomes come down to scheduler luck. The mailbox absorbs the
            // gap to the drainer; past it the library drops and counts rather
            // than lagging without bound. Dispatch stays non-blocking, so a
            // slow callback delays later events but never the transport.
            .with_event_delivery(EventDelivery::Ordered { capacity: 1024 })
            .on_event({
                let tx = tx.clone();
                let state = state.clone();
                let current_qr = current_qr.clone();
                let cache_path = chat_cache.clone();
                move |event, client| {
                    let tx = tx.clone();
                    let state = state.clone();
                    let current_qr = current_qr.clone();
                    let cache_path = cache_path.clone();
                    async move {
                        Self::handle_event(&event, &client, &state, &tx, &current_qr, &cache_path)
                            .await;
                    }
                }
            });

        let bot = builder
            .build()
            .await
            .map_err(|e| BackendError::Other(format!("WhatsApp: failed to build bot: {e}")))?;

        let client = bot.client();

        // Heal chat rows split across LID/PN keys (older caches created one
        // row per key) before the TUI can render them: one peer must never
        // appear twice. Correct on a cold start too — at that point the LID↔PN
        // mappings are still empty, so each cached LID row is resolved afresh.
        merge_lid_duplicates(&client, &state, None, None).await;

        Ok(Self {
            client,
            bot: Arc::new(Mutex::new(Some(bot))),
            run_task: Arc::new(Mutex::new(None)),
            tx,
            state,
            shutdown,
            current_qr,
            cache_path: chat_cache,
        })
    }

    pub(crate) fn current_client(&self) -> Arc<Client> {
        self.client.clone()
    }

    async fn check_logged_in(&self) -> Result<(), BackendError> {
        if !self.current_client().is_logged_in() {
            return Err(BackendError::NotAuthenticated);
        }
        Ok(())
    }

    /// Take the configured bot out of its dormant slot and hand it back to the
    /// caller that owns the run loop. Returns `None` once the bot has been
    /// claimed, which is what makes [`Messenger::start`] idempotent.
    fn take_bot(&self) -> Option<Bot> {
        self.bot.lock().unwrap().take()
    }

    /// Adopt the handle of the spawned run loop together with its failure
    /// watcher, which reports an unexpected end of the loop as `Disconnected`.
    fn track_run_task(&self, run_task: JoinHandle<()>) {
        *self.run_task.lock().unwrap() = Some(run_task);
    }

    /// True once [`Messenger::disconnect`] ran; a shut-down provider must never
    /// be started again.
    fn is_shutting_down(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Messenger for WhatsAppMessenger {
    /// `dormant -> started`: spawn the bot exactly once, which is the first
    /// point at which this provider touches the network. Called by the TUI for
    /// an explicitly enabled provider, after every event receiver is
    /// registered, so the first `Connected` / `QrCode` event has a subscriber.
    fn start(&self) {
        if self.is_shutting_down() {
            return;
        }

        let Some(bot) = self.take_bot() else {
            return;
        };

        let handle = bot.spawn();
        let watch_tx = self.tx.clone();
        let shutdown = self.shutdown.clone();

        // Failure watcher: await the bot's run loop. If it ends for any reason
        // other than an intentional shutdown, surface that to the TUI so it does
        // not silently go blind.
        let run_task = tokio::spawn(async move {
            handle.await;
            if !shutdown.load(Ordering::SeqCst) {
                let _ = watch_tx.send(BackendEvent::Disconnected(
                    "WhatsApp: connection lost".to_string(),
                ));
            }
        });

        self.track_run_task(run_task);
    }

    async fn is_authenticated(&self) -> bool {
        self.client.clone().is_logged_in()
    }

    async fn chats(&self) -> Result<Vec<Chat>, BackendError> {
        let state = self.state.read().await;
        debug!("WA chats() returning {} chats", state.chats.len());
        Ok(state.chats.clone())
    }

    async fn set_read(&mut self, chat: &ChatId) -> Result<(), BackendError> {
        self.check_logged_in().await?;

        let ChatId::WhatsApp(jid_str) = chat else {
            return Ok(());
        };

        let jid = Jid::from_str(jid_str)
            .map_err(|e| BackendError::Other(format!("WhatsApp: invalid chat jid: {e}")))?;

        // WhatsApp marks a whole chat read against its newest incoming message
        // (the read watermark); `mark_as_read` no-ops on an empty id list, so a
        // chat with nothing unread — or none at all — simply sends no receipt.
        let (target_jid, latest_read, canonical) = {
            let state = self.state.read().await;
            let canonical = fold_lid_key(jid_str, &state);
            let target_jid = match &canonical {
                ChatId::WhatsApp(raw) => Jid::from_str(raw).ok().unwrap_or_else(|| jid.clone()),
                _ => jid.clone(),
            };
            let latest_read = state
                .history
                .get(&canonical)
                .and_then(|h| h.iter().filter(|m| !m.from_me).max_by_key(|m| m.timestamp))
                .map(|m| {
                    // Group receipts carry the author's participant slot so the
                    // sender can tell who read it.
                    let author = if jid.is_group() {
                        m.author_id.as_deref().and_then(|s| Jid::from_str(s).ok())
                    } else {
                        None
                    };
                    (m.message_id.to_string(), author)
                });
            (target_jid, latest_read, canonical)
        };

        if let Some((latest_id, author)) = latest_read {
            let client = self.current_client();
            let _ = client
                .mark_as_read(&target_jid, author.as_ref(), &[latest_id.as_str()])
                .await?;
        }

        let mut state = self.state.write().await;
        for c in state.chats.iter_mut() {
            if c.id == *chat || c.id == canonical {
                c.unread = false;
                c.unread_count = 0;
            }
        }
        drop(state);

        self.state.read().await.save_to(&self.cache_path);

        let _ = self.tx.send(BackendEvent::UnreadUpdated {
            chat: chat.clone(),
            unread: false,
            unread_count: 0,
        });
        Ok(())
    }

    async fn history(&self, chat: &ChatId) -> Result<Vec<Message>, BackendError> {
        let state = self.state.read().await;
        let chat = match chat {
            ChatId::WhatsApp(raw) => fold_lid_key(raw, &state),
            other => other.clone(),
        };
        // The cache is kept oldest-to-newest; re-sort defensively by timestamp so
        // the TUI always renders chronological order regardless of ingest path.
        let mut messages = state.history.get(&chat).cloned().unwrap_or_default();
        messages.sort_by_key(|m| m.timestamp);
        Ok(messages)
    }

    async fn history_page(
        &self,
        chat: &ChatId,
        _offset_id: Option<i32>,
        limit: usize,
    ) -> Result<Vec<Message>, BackendError> {
        self.check_logged_in().await?;

        // WhatsApp has no numeric before-message cursor, so the trait's
        // `Option<i32>` offset is unused here (the TUI always passes `None`,
        // since opaque message ids have no i32 form). The anchor is instead the
        // oldest message already cached — the topmost one the user can see —
        // derived from our own state, keeping `telegram.rs` and the shared
        // trait untouched. Resolve it (plus the cache row's key) before any
        // await so the state lock is not held across the network request.
        let (chat, anchor) = {
            let state = self.state.read().await;
            let chat = match chat {
                ChatId::WhatsApp(raw) => fold_lid_key(raw, &state),
                other => other.clone(),
            };

            // The cache is not kept globally sorted: each history-sync chunk
            // appends in arrival order (chunks often stream newest-first or an
            // older full sync after a recent one), so the first *inserted*
            // message is not necessarily the oldest. The on-demand cursor must
            // page before the chronologically oldest cached message, otherwise
            // the phone is asked for a window the cache already holds and
            // every returned message is deduplicated away.
            let anchor = state
                .history
                .get(&chat)
                .and_then(|h| h.iter().min_by_key(|m| m.timestamp))
                .cloned();
            (chat, anchor)
        };

        // No cached anchor (empty chat): there is no window left to page before.
        let Some(anchor) = anchor else {
            return Err(BackendError::Other(
                "WhatsApp: no older history available".into(),
            ));
        };

        let ChatId::WhatsApp(jid_str) = &chat else {
            return Err(BackendError::Other("WhatsApp: not a WhatsApp chat".into()));
        };
        let jid = Jid::from_str(jid_str)
            .map_err(|e| BackendError::Other(format!("WhatsApp: invalid chat jid: {e}")))?;

        // On-demand history sync: ask our own phone (via PDO) for the page of
        // messages before the oldest cached one. The phone answers asynchronously
        // with an ordinary HistorySync, which `handle_history_sync` merges into
        // the cache.
        self.current_client()
            .fetch_message_history(
                &jid,
                &anchor.message_id.to_string(),
                anchor.from_me,
                anchor.timestamp.saturating_mul(1000),
                limit.max(1) as i32,
            )
            .await
            .map_err(|e| BackendError::Other(format!("WhatsApp: history fetch failed: {e}")))?;

        // Wait (bounded) for the backfilled page to land in the cache, then hand
        // the grown page back so the TUI can prepend it. The wait runs in a
        // background task (see `App::load_more_history`), so the UI never
        // freezes; on a phone that ignores the request we fall back to the
        // current cache and the next scroll-up simply retries.
        let known: HashSet<String> = {
            let state = self.state.read().await;
            state
                .history
                .get(&chat)
                .map(|h| h.iter().map(|m| m.message_id.to_string()).collect())
                .unwrap_or_default()
        };

        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(10);

        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
            let changed = {
                let state = self.state.read().await;
                let now: HashSet<String> = state
                    .history
                    .get(&chat)
                    .map(|h| h.iter().map(|m| m.message_id.to_string()).collect())
                    .unwrap_or_default();
                now != known
            };

            if changed {
                break;
            }

            if tokio::time::Instant::now() >= deadline {
                debug!(chat = ?chat, "Timed out waiting for on-demand history sync");
                break;
            }
        }

        let state = self.state.read().await;
        let mut messages = state.history.get(&chat).cloned().unwrap_or_default();
        drop(state);
        messages.sort_by_key(|m| m.timestamp);
        Ok(messages)
    }

    async fn status(&self, chat: &ChatId) -> Result<Option<String>, BackendError> {
        if !self.client.clone().is_logged_in() {
            return Ok(None);
        }
        let ChatId::WhatsApp(raw) = chat else {
            return Ok(None);
        };
        let Ok(jid) = Jid::from_str(raw) else {
            return Ok(None);
        };
        if !jid.is_pn() && !jid.is_lid() {
            return Ok(None);
        }

        // Streaming subscription: answers with the current state immediately,
        // then keeps `Event::Presence` flowing from the linked device. Ignore
        // subscription errors (peers that never published presence): the read
        // below still returns whatever is already cached.
        let client = self.current_client();
        if let Err(e) = client.presence().subscribe(jid.clone()).await {
            warn!("WA presence subscribe failed for {raw}: {e}");
        }

        let st = self.state.read().await;

        if find_peer_chat(&st, &jid).is_none() {
            return Ok(None);
        }

        Ok(presence_keys(&jid, &st)
            .iter()
            .find_map(|k| st.presence.get(k))
            .copied()
            .and_then(|(online, last_seen)| presence_label(online, last_seen)))
    }

    async fn reply_context(
        &self,
        chat: &ChatId,
        message_id: &MessageId,
    ) -> Result<Option<ReplyContext>, BackendError> {
        let state = self.state.read().await;
        let context = state.history.get(chat).and_then(|messages| {
            messages
                .iter()
                .find(|m| m.message_id == *message_id)
                .and_then(|m| {
                    m.reply_to_id.as_ref().and_then(|rid| {
                        state
                            .history
                            .get(chat)
                            .into_iter()
                            .flatten()
                            .find(|o| o.message_id == *rid)
                            .map(|o| ReplyContext {
                                id: o.message_id.clone(),
                                sender: o.sender.clone(),
                                text: o.text.clone(),
                                timestamp: o.timestamp,
                            })
                    })
                })
        });
        Ok(context)
    }

    async fn media_bytes(
        &self,
        chat: &ChatId,
        message_id: &MessageId,
    ) -> Result<Option<Vec<u8>>, BackendError> {
        let ChatId::WhatsApp(_) = chat else {
            return Err(BackendError::Other("not a WhatsApp chat".into()));
        };

        // The persisted CDN reference covers media from earlier sessions too;
        // nothing here holds or caches the media bytes themselves. A missing
        // ref means the message predates ref capture (no HistorySync has
        // re-delivered it yet), NOT a download problem.
        let media_ref = self
            .state
            .read()
            .await
            .media_refs
            .get(&message_id.0)
            .cloned();

        let Some(r) = media_ref else {
            // A missing ref means the message was received while sendrs was
            // closed (or predates ref capture) and no HistorySync has
            // re-delivered it yet — NOT a download problem. `history_sync_seen`
            // records whether the full-history backfill has fired this session;
            // it only does once the `require_full_sync` device prop is honoured.
            let history_sync_seen = self.state.read().await.sync_seen_at.is_some();

            debug!(
                message_id = %message_id.0,
                history_sync_seen,
                "no WA media ref cached for message"
            );
            return Ok(None);
        };

        // The reference records which media kind it came from so the right
        // `MediaType` decrypts the stream (audio, image and video use
        // different keys).
        let Some(media_type) = ref_media_type(&r.kind) else {
            return Ok(None);
        };

        let params = DownloadParams::encrypted(
            r.direct_path,
            &r.media_key,
            &r.file_sha256,
            &r.file_enc_sha256,
            r.file_length,
            media_type,
        );

        debug!(message_id = ?message_id, "Fetching WA media bytes on demand");
        let client = self.current_client();
        let bytes = client.download(&params).await.map_err(BackendError::from)?;

        Ok((!bytes.is_empty()).then_some(bytes))
    }

    async fn send(
        &self,
        chat: &ChatId,
        msg: &OutboundMessage,
        reply_to: Option<MessageId>,
    ) -> Result<Message, BackendError> {
        self.check_logged_in().await?;

        let ChatId::WhatsApp(jid_str) = chat else {
            return Err(BackendError::Other("WhatsApp: invalid chat".into()));
        };

        let jid = Jid::from_str(jid_str)
            .map_err(|e| BackendError::Other(format!("WhatsApp: invalid chat jid: {e}")))?;
        let client = self.current_client();

        let (text_content, message, media, media_ref) = match msg {
            OutboundMessage::Text { text } => {
                let content = text.clone();
                let wa_msg = match &reply_to {
                    Some(id) => wa::Message::text_with_context(
                        text.clone(),
                        wa::ContextInfo {
                            stanza_id: Some(id.to_string()),
                            ..Default::default()
                        },
                    ),
                    None => wa::Message::text(text.clone()),
                };
                (content, wa_msg, None, None)
            }
            OutboundMessage::Media {
                kind,
                data,
                file_name,
                caption,
                ..
            } => {
                let media_type = match kind {
                    MediaKind::Image => MediaType::Image,
                    MediaKind::Video => MediaType::Video,
                    MediaKind::Audio { .. } => MediaType::Audio,
                    MediaKind::Document => MediaType::Document,
                    MediaKind::Sticker => MediaType::Sticker,
                    MediaKind::Unsupported => {
                        return Err(BackendError::Other(
                            "WhatsApp: unsupported media kind".into(),
                        ));
                    }
                };

                let upload = client
                    .upload(data.to_vec(), media_type, UploadOptions::new())
                    .await
                    .map_err(BackendError::from)?;
                let message = outbound_media_message(
                    kind.clone(),
                    CdnFields {
                        url: upload.url,
                        direct_path: upload.direct_path,
                        media_key: upload.media_key,
                        file_enc_sha256: upload.file_enc_sha256,
                        file_sha256: upload.file_sha256,
                        file_length: upload.file_length,
                        media_key_timestamp: upload.media_key_timestamp,
                        streaming_sidecar: upload.streaming_sidecar,
                    },
                    file_name,
                    caption.as_deref(),
                    reply_to.as_ref(),
                )?;
                let media = Some(MessageMedia {
                    kind: kind.clone(),
                    caption: caption.clone(),
                    file_name: Some(file_name.clone()),
                });
                // Own-sent media stays playable even if the server never echoes
                // it back: reuse the same ref extraction the inbound paths use,
                // so `media_bytes` re-downloads driven by the persisted stanza
                // id resolve for clips this session sent too.
                let media_ref = wa_media_ref(&message);
                let text_content = caption
                    .clone()
                    .filter(|c| !c.trim().is_empty())
                    .unwrap_or_else(|| format!("{}, click to show", kind.label()));

                (text_content, message, media, media_ref)
            }
        };

        let result = client
            .send_message(jid.clone(), message)
            .await
            .map_err(BackendError::from)?;

        let sent = Message {
            message_id: result.message_id.clone().into(),
            chat_id: chat.clone(),
            sender: "You".into(),
            author_id: None,
            text: text_content,
            timestamp: now(),
            from_me: true,
            msg_actions: message_actions(true),
            media,
            reply_to_id: reply_to.clone(),
            reply_ctx: None,
            pending: false,
            failed: false,
        };

        {
            let mut state = self.state.write().await;

            state.by_stanza_id.insert(
                result.message_id.clone(),
                (chat.clone(), sent.message_id.clone()),
            );

            // Persist the outbound CDN reference alongside the stanza id so
            // own-sent media (audio clips in particular) resolves through
            // `media_bytes` without waiting for any server echo.
            if let Some(media_ref) = media_ref {
                state
                    .media_refs
                    .insert(result.message_id.clone(), media_ref);
            }

            let is_dup = state
                .history
                .get(chat)
                .map(|h| h.iter().any(|m| m.message_id == sent.message_id))
                .unwrap_or(false);

            if !is_dup {
                state
                    .history
                    .entry(chat.clone())
                    .or_default()
                    .push(sent.clone());
            }

            // Ensure the target chat has a row (creating it for a brand-new
            // conversation) and mark it read — the send just took place.
            state.upsert_chat_from_message(chat.clone(), &sent);
            if let Some(c) = state.chats.iter_mut().find(|c| c.id == *chat) {
                c.unread = false;
                c.unread_count = 0;
            }
        }

        // Persist the send now instead of waiting for a later event: media sent
        // to the self-chat (audio clips in particular) never gets an inflowing
        // re-delivery, so an early quit must not lose it from the cache.
        self.state.read().await.save_to(&self.cache_path);

        let _ = self.tx.send(BackendEvent::MessageReceived(sent.clone()));
        Ok(sent)
    }

    async fn delete(&self, chat: &ChatId, id: &MessageId) -> Result<(), BackendError> {
        self.check_logged_in().await?;

        let ChatId::WhatsApp(jid_str) = chat else {
            return Ok(());
        };

        let jid = Jid::from_str(jid_str)
            .map_err(|e| BackendError::Other(format!("WhatsApp: invalid chat jid: {e}")))?;

        let client = self.current_client();

        client
            .revoke_message(jid, id.to_string(), whatsapp_rust::RevokeType::Sender)
            .await
            .map_err(BackendError::from)?;

        // The server may never re-deliver the revocation (notably in the
        // self-chat), so apply the deletion to the cached history locally and
        // broadcast it; the poll refresh would otherwise restore the message.
        let removed = {
            let mut state = self.state.write().await;
            let removed = state
                .history
                .get_mut(chat)
                .map(|msg_vec| {
                    let before = msg_vec.len();
                    msg_vec.retain(|m| m.message_id != *id);
                    before != msg_vec.len()
                })
                .unwrap_or(false);

            if removed {
                state
                    .by_stanza_id
                    .retain(|_, (c, mid)| !(*c == *chat && *mid == *id));
                state.media_refs.retain(|k, _| k != &id.0);
                state.refresh_last_message_ts(chat);
            }

            removed
        };

        if removed {
            let _ = self.tx.send(BackendEvent::MessageDeleted {
                chat: Some(chat.clone()),
                message_ids: vec![id.clone()],
            });
            self.state.read().await.save_to(&self.cache_path);
        }

        Ok(())
    }

    async fn edit(&self, chat: &ChatId, id: &MessageId, text: &str) -> Result<(), BackendError> {
        self.check_logged_in().await?;

        let ChatId::WhatsApp(jid_str) = chat else {
            return Ok(());
        };

        let jid = Jid::from_str(jid_str)
            .map_err(|e| BackendError::Other(format!("WhatsApp: invalid chat jid: {e}")))?;

        let client = self.current_client();

        client
            .edit_message(jid, id.to_string(), wa::Message::text(text.to_string()))
            .await
            .map_err(BackendError::from)?;

        // The server may never re-deliver the edit (notably in the self-chat),
        // so rewrite the cached history locally and broadcast the update; the
        // poll refresh would otherwise restore the previous text.
        let updated = {
            let mut state = self.state.write().await;
            state.edit_message_text(chat, id, text)
        };

        if let Some(updated) = updated {
            let _ = self.tx.send(BackendEvent::MessageUpdated(updated));
            self.state.read().await.save_to(&self.cache_path);
        }

        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<BackendEvent> {
        // Install the event subscription only. The bot is NOT started here: a
        // disabled provider must remain inactive (no transport, no QR) until the
        // user enables it via `start()`. Because the receiver is registered now,
        // a later `start()` can never drop the first `Connected` / `QrCode`
        // event for a subscribed TUI.
        self.tx.subscribe()
    }

    async fn disconnect(&mut self) -> Result<(), BackendError> {
        // Idempotent: only the first call performs the actual teardown, and it
        // is safe to call on a provider that was never started (disabled).
        if self.shutdown.swap(true, Ordering::SeqCst) {
            return Ok(());
        }

        // If the bot is running, flush persistence and stop the run loop before
        // returning. A never-started (disabled) instance has no run task to
        // await; it is already fully inert.
        self.current_client().disconnect().await;

        let run_task = self.run_task.lock().unwrap().take();
        if let Some(task) = run_task {
            let _ = task.await;
        }

        // Flush the final session state now that the run task is done and can
        // no longer overwrite it, so a clean exit right after a send never
        // drops the last messages from the cache.
        self.state.read().await.save_to(&self.cache_path);

        Ok(())
    }

    fn login_steps(&self) -> Vec<AuthSteps> {
        // A single event-driven QR step. The payload is read live on every
        // frame from `current_qr` so a replaced/refreshed code shows up
        // immediately in the login screen.
        vec![AuthSteps::QrCode(
            self.current_qr
                .try_read()
                .ok()
                .and_then(|qr| qr.clone())
                .unwrap_or_default(),
        )]
    }

    fn login_placeholder(&self, _step: usize) -> &'static str {
        "Scan with your phone's WhatsApp > Linked devices"
    }

    async fn login_step(
        &mut self,
        _step: usize,
        _input: &str,
    ) -> Result<LoginStepState, BackendError> {
        // The QR step has no text to submit. Consume the Enter as "keep
        // waiting", and report Done only once the account is actually paired.
        if self.is_authenticated().await {
            Ok(LoginStepState::Done)
        } else {
            Ok(LoginStepState::NextStep)
        }
    }
}
