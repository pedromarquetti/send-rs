use crate::backend::AuthSteps;
use crate::helpers::relative;

use super::{
    BackendError, BackendEvent, Chat, ChatId, LoginStepState, MediaKind, Message, MessageAction,
    MessageId, MessageMedia, Messenger, ReplyContext,
};
use anyhow::Result;
use grammers_client::client::UpdatesConfiguration;
use grammers_client::peer::Dialog;
use grammers_client::sender::SenderPool;
use grammers_client::session::storages::SqliteSession;
use grammers_client::update::Update;
use grammers_client::{Client, SignInError};
use grammers_session::updates::UpdatesLike;
use grammers_tl_types as tl;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::sync::{RwLock, broadcast, mpsc};
use tracing::{debug, error, info, warn};

type CachedChats = Option<(Instant, Vec<Chat>)>;

fn message_actions(from_me: bool) -> Vec<MessageAction> {
    let mut actions = vec![MessageAction::Reply];
    if from_me {
        actions.extend([MessageAction::Edit, MessageAction::Delete]);
    }

    actions
}

fn telegram_media_kind(msg: &grammers_client::message::Message) -> Option<MediaKind> {
    match msg.media()? {
        grammers_client::media::Media::Photo(_) => Some(MediaKind::Image),
        grammers_client::media::Media::Document(doc) => {
            let raw = doc.raw;

            if raw.video {
                return Some(MediaKind::Video);
            }

            if raw.voice {
                return Some(MediaKind::Audio);
            };

            Some(MediaKind::Document)
        }
        grammers_client::media::Media::Sticker(_) => Some(MediaKind::Sticker),
        grammers_client::media::Media::Geo(_) => Some(MediaKind::Unsupported),
        grammers_client::media::Media::Dice(_) => Some(MediaKind::Unsupported),
        grammers_client::media::Media::Venue(_) => Some(MediaKind::Unsupported),
        grammers_client::media::Media::GeoLive(_) => Some(MediaKind::Unsupported),
        grammers_client::media::Media::WebPage(_) => Some(MediaKind::Unsupported),
        grammers_client::media::Media::Contact(_) => Some(MediaKind::Unsupported),
        grammers_client::media::Media::Poll(_) => Some(MediaKind::Unsupported),
        _ => Some(MediaKind::Unsupported),
    }
}

fn telegram_message_media(msg: &grammers_client::message::Message) -> Option<MessageMedia> {
    let kind = telegram_media_kind(msg)?;
    let caption = (!msg.text().trim().is_empty()).then(|| msg.text().to_string());

    // Document-backed media carry a file name and (audio/video) a duration;
    // Photos do not. `doc.duration()` reads the `DocumentAttributeAudio`/
    // `DocumentAttributeVideo` attributes, so voice notes report their length
    // before the TUI decodes the file.
    let (file_name, duration_secs) = match msg.media() {
        Some(grammers_client::media::Media::Document(doc)) => (
            doc.name().map(str::to_string),
            doc.duration().map(|d| d.round() as u32),
        ),
        _ => (None, None),
    };

    Some(MessageMedia {
        kind,
        caption,
        file_name,
        duration_secs,
        waveform: None,
    })
}

fn telegram_message_text(msg: &grammers_client::message::Message) -> String {
    if let Some(media) = telegram_message_media(msg) {
        return media
            .caption
            .filter(|caption| !caption.trim().is_empty())
            .unwrap_or_else(|| format!("{}, click to show", media.kind.label()));
    }

    msg.text().to_string()
}

fn telegram_message_sender(msg: &grammers_client::message::Message, from_me: bool) -> String {
    if from_me {
        return "You".to_string();
    }

    msg.sender()
        .and_then(|p| p.name())
        .or_else(|| msg.peer().and_then(|p| p.name()))
        .unwrap_or("Unknown")
        .to_string()
}

fn telegram_message_id(msg: &grammers_client::message::Message) -> MessageId {
    msg.id().to_string().into()
}

fn telegram_message_reply_to(msg: &grammers_client::message::Message) -> Option<MessageId> {
    msg.reply_to_message_id().map(|id| id.to_string().into())
}

fn normalize_telegram_message(
    msg: &grammers_client::message::Message,
    chat: &ChatId,
    own_chat: Option<&ChatId>,
    reply_ctx: Option<ReplyContext>,
    pending: bool,
    failed: bool,
) -> Message {
    let from_me = msg.outgoing() || own_chat == Some(chat);
    let sender = telegram_message_sender(msg, from_me);
    let media = telegram_message_media(msg);

    let text = match &media {
        Some(media) => match media.caption.clone() {
            Some(caption) => {
                format!("{}: {}", media.kind.label(), caption)
            }
            None => {
                format!("{}, click to show", media.kind.label())
            }
        },
        None => msg.text().to_string(),
    };

    // let text = media
    //     .as_ref()
    //     .and_then(|media| media.caption.clone())
    //     .filter(|caption| !caption.trim().is_empty())
    //     .unwrap_or_else(|| {
    //         media
    //             .as_ref()
    //             .map(|media| format!("{}, click to show", media.kind.label()))
    //             .unwrap_or_else(|| msg.text().to_string())
    //     });

    Message {
        message_id: telegram_message_id(msg),
        chat: chat.clone(),
        sender,
        author_id: None,
        text,
        timestamp: msg.date().timestamp(),
        from_me,
        msg_actions: message_actions(from_me),
        media,
        reply_to_id: telegram_message_reply_to(msg),
        reply_ctx,
        pending,
        failed,
    }
}

async fn self_user_id(client: &Client) -> Option<ChatId> {
    client
        .get_me()
        .await
        .ok()
        .and_then(|user| user.id().bare_id().map(ChatId::Telegram))
}

fn message_chat_id(msg: &grammers_client::message::Message) -> Option<ChatId> {
    msg.peer()
        .and_then(|peer| peer.id().bare_id())
        .or_else(|| msg.peer_id().bare_id())
        .filter(|id| *id != 0)
        .map(ChatId::Telegram)
}

fn pretty_peer_name(msg: &grammers_client::message::Message, fallback: &str) -> String {
    msg.peer()
        .and_then(|peer| peer.name())
        .map(str::to_string)
        .unwrap_or_else(|| fallback.to_string())
}

fn telegram_status_label(status: &grammers_tl_types::enums::UserStatus) -> Option<String> {
    match status {
        grammers_tl_types::enums::UserStatus::Empty => None,
        grammers_tl_types::enums::UserStatus::Online(_) => Some("online".to_string()),
        grammers_tl_types::enums::UserStatus::Offline(status) => {
            Some(format!("last seen {}", relative(status.was_online)))
        }
        grammers_tl_types::enums::UserStatus::Recently(_) => Some("recently".to_string()),
        grammers_tl_types::enums::UserStatus::LastWeek(_) => Some("last week".to_string()),
        grammers_tl_types::enums::UserStatus::LastMonth(_) => Some("last month".to_string()),
    }
}

/// The active login step. Stored between `request_code` and `sign_in` calls
/// so the TUI can collect user input across multiple frames.
///
/// Added clippy::large_enum_variant because the mem-usage here is temporary
#[allow(clippy::large_enum_variant)]
pub enum LoginToken {
    CodeRequested {
        token: grammers_client::client::LoginToken,
    },
    PasswordRequired {
        token: grammers_client::client::PasswordToken,
    },
}

#[derive(Clone)]
pub struct TelegramMessenger {
    session: Arc<SqliteSession>,
    api_id: u32,
    client: Arc<RwLock<Client>>,
    api_hash: String,
    tx: broadcast::Sender<BackendEvent>,
    login_token: Arc<RwLock<Option<LoginToken>>>,
    cached_dialogs: Arc<StdMutex<Vec<Dialog>>>,
    chat_refresh: Arc<Mutex<()>>,
    cached_chats: Arc<Mutex<CachedChats>>,
    reply_contexts: Arc<Mutex<HashMap<(ChatId, MessageId), ReplyContext>>>,
    flood_wait_until: Arc<Mutex<Option<Instant>>>,
    sync_update_state_secs: u64,
    shutdown: Arc<AtomicBool>,
    cancel_refresh: Arc<AtomicBool>,
    connection_lost: Arc<AtomicBool>,
    /// Whether the provider is enabled in the config. A disabled messenger
    /// never starts its update listener, so it makes no requests and emits no
    /// `Disconnected`/`Status` events.
    enabled: Arc<AtomicBool>,
    /// The pool's `updates` receiver, kept here so the update listener can be
    /// started lazily on the first [`Self::subscribe`] (mirroring WhatsApp's
    /// lazy bot start). Holding it here also means an enabled-but-not-yet-started
    /// messenger stays silent: no listener, no events.
    updates: Arc<StdMutex<Option<mpsc::UnboundedReceiver<UpdatesLike>>>>,
}

impl TelegramMessenger {
    /// Open (or create) the session, build a [`SenderPool`] and [`Client`].
    /// Spawns the pool runner in a background task.
    /// Does **not** attempt to log in – call [`Messenger::is_authenticated`]
    /// or [`Messenger::login`] afterwards.
    ///
    /// The update listener is started lazily on the first [`Self::subscribe`]
    /// and only when `enabled`, so a configured-but-disabled account is retained
    /// without making any network requests or emitting any events.
    pub async fn new(
        session_dir: PathBuf,
        api_id: u32,
        api_hash: &str,
        sync_update_state_secs: u64,
        enabled: bool,
    ) -> Result<Self, BackendError> {
        let session_path = session_dir.join("tg_session.sqlite");
        let session = Arc::new(
            SqliteSession::open(&session_path)
                .await
                .map_err(|e| BackendError::Other(format!("failed to open TG session: {e}")))?,
        );

        let pool = SenderPool::new(Arc::clone(&session), api_id as i32);

        // The runner must be spawned; it drives all network I/O.
        let runner = pool.runner;
        let updates = pool.updates;
        tokio::spawn(async move { runner.run().await });

        let client = Client::new(pool.handle);

        let (tx, _) = broadcast::channel(128);

        let messenger = Self {
            session,
            api_id,
            client: Arc::new(RwLock::new(client)),
            api_hash: api_hash.to_string(),
            tx,
            login_token: Arc::new(RwLock::new(None)),
            cached_dialogs: Arc::new(StdMutex::new(Vec::new())),
            chat_refresh: Arc::new(Mutex::new(())),
            cached_chats: Arc::new(Mutex::new(None)),
            reply_contexts: Arc::new(Mutex::new(HashMap::new())),
            flood_wait_until: Arc::new(Mutex::new(None)),
            sync_update_state_secs,
            shutdown: Arc::new(AtomicBool::new(false)),
            cancel_refresh: Arc::new(AtomicBool::new(false)),
            connection_lost: Arc::new(AtomicBool::new(false)),
            enabled: Arc::new(AtomicBool::new(enabled)),
            updates: Arc::new(StdMutex::new(Some(updates))),
        };

        Ok(messenger)
    }

    async fn current_client(&self) -> Client {
        self.client.read().await.clone()
    }

    async fn replace_client_with_new_pool(&self) -> mpsc::UnboundedReceiver<UpdatesLike> {
        let pool = SenderPool::new(Arc::clone(&self.session), self.api_id as i32);
        let runner = pool.runner;
        tokio::spawn(async move { runner.run().await });
        let client = Client::new(pool.handle);
        *self.client.write().await = client;
        pool.updates
    }

    /// Start the update listener now if the provider is enabled and the
    /// startup `updates` receiver has not been consumed yet. Mirrors WhatsApp's
    /// lazy bot start: registering the first subscriber before the listener
    /// runs guarantees the initial `Connected` event is never dropped by the
    /// `broadcast` channel for a not-yet-registered receiver.
    fn maybe_start_listener(&self) {
        if !self.enabled.load(Ordering::SeqCst) {
            return;
        }

        let updates = self.updates.lock().unwrap().take();
        if let Some(updates) = updates {
            self.spawn_update_listener(updates);
        }
    }

    /// Reflect the configured enabled state on the messenger. A disabled
    /// messenger never starts its listener (no requests, no events); enabling
    /// it later starts the listener if it has not been started already.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
        self.maybe_start_listener();
    }

    fn spawn_update_listener(&self, updates: mpsc::UnboundedReceiver<UpdatesLike>) {
        const MAX_RECONNECT_ATTEMPTS: u32 = 5;

        let telegram = self.clone();

        tokio::spawn(async move {
            let mut current_updates = Some(updates);
            let mut reconnect_attempt = 0u32;

            loop {
                let Some(updates) = current_updates.take() else {
                    break;
                };

                if telegram.shutdown.load(Ordering::SeqCst) {
                    break;
                }

                let current_client = telegram.current_client().await;

                let config = UpdatesConfiguration {
                    catch_up: false,
                    ..Default::default()
                };

                let mut stream = match current_client.stream_updates(updates, config).await {
                    Ok(s) => s,
                    Err(e) => {
                        let reason = format!("Telegram update stream failed to start: {e}");
                        warn!(reason);
                        let _ = telegram.tx.send(BackendEvent::Disconnected(reason.clone()));
                        let Some(delay) = telegram
                            .handle_stream_failure(
                                &reason,
                                reconnect_attempt,
                                !current_client.is_authorized().await.unwrap_or(false),
                                MAX_RECONNECT_ATTEMPTS,
                            )
                            .await
                        else {
                            break;
                        };
                        reconnect_attempt += 1;
                        if telegram.shutdown.load(Ordering::SeqCst) || delay.is_zero() {
                            break;
                        }
                        tokio::time::sleep(delay).await;
                        current_updates = Some(telegram.replace_client_with_new_pool().await);
                        continue;
                    }
                };

                telegram.connection_lost.store(false, Ordering::SeqCst);
                reconnect_attempt = 0;
                let _ = telegram.tx.send(BackendEvent::Connected);
                telegram.clear_status();

                info!("Telegram update stream started");

                let mut own_chat = self_user_id(&current_client).await;
                let mut sync_interval = tokio::time::interval(std::time::Duration::from_secs(
                    telegram.sync_update_state_secs,
                ));
                let mut stream_reason = None;

                loop {
                    if telegram.shutdown.load(Ordering::SeqCst) {
                        if let Err(e) = stream.sync_update_state().await {
                            error!("Failed to sync update state before shutdown: {e}");
                        }
                        break;
                    }

                    tokio::select! {
                        biased;

                        _ = sync_interval.tick() => {
                            if let Err(e) = stream.sync_update_state().await {
                                error!("Failed to sync update state: {e}");
                            } else {
                                debug!("Update state synced to session");
                            }
                        }

                        update = stream.next() => {
                            match update {
                                Ok(update) => match update {
                                    Update::NewMessage(msg) => {
                                        let Some(chat_id) = message_chat_id(&msg) else {
                                            debug!(msg_id = msg.id(), "Skipping new message without usable peer metadata");
                                            continue;
                                        };

                                        if own_chat.is_none() {
                                            own_chat = self_user_id(&current_client).await;
                                        }

                                        let reply_ctx = reply_context(&current_client, &msg).await;

                                        let from_me =
                                            msg.outgoing() || own_chat.as_ref() == Some(&chat_id);

                                        let sender = telegram_message_sender(&msg, from_me);
                                        let text = telegram_message_text(&msg);

                                        let text_preview: String = text.chars().take(80).collect();
                                        debug!(
                                            sender,
                                            chat_id = ?chat_id,
                                            text_preview,
                                            "MessageReceived"
                                        );

                                        let message = normalize_telegram_message(
                                            &msg,
                                            &chat_id,
                                            own_chat.as_ref(),
                                            reply_ctx,
                                            false,
                                            false,
                                        );

                                        let last_message_ts = msg.date().timestamp();
                                        let _ = telegram.tx.send(BackendEvent::MessageReceived(message.clone()));

                                        *telegram.cached_chats.lock().await = None;

                                        let _ = telegram.tx.send(BackendEvent::ChatUpdated(Chat {
                                            id: chat_id,
                                            contact_name: pretty_peer_name(&msg, &sender),
                                            last_message_ts: Some(last_message_ts),
                                            status: None,
                                            fixed: false,
                                            verified: false,
                                            scroll: 0,
                                            unread: false,
                                            unread_count: 0,
                                        }));
                                    }
                                    Update::MessageEdited(msg) => {
                                        let Some(chat_id) = message_chat_id(&msg) else {
                                            debug!(msg_id = msg.id(), "Skipping edited message without usable peer metadata");
                                            continue;
                                        };

                                        if own_chat.is_none() {
                                            own_chat = self_user_id(&current_client).await;
                                        }

                                        let last_message_ts = msg.date().timestamp();
                                        let message = normalize_telegram_message(
                                            &msg,
                                            &chat_id,
                                            own_chat.as_ref(),
                                            None,
                                            false,
                                            false,
                                        );

                                        let _ = telegram.tx.send(BackendEvent::MessageUpdated(message));

                                        *telegram.cached_chats.lock().await = None;

                                        debug!(
                                            chat_id = ?chat_id,
                                            last_message_ts,
                                            "MessageUpdated"
                                        );
                                    }
                                    Update::MessageDeleted(del) => {
                                        let chat = del.channel_id().map(ChatId::Telegram);
                                        let message_ids = del
                                            .messages()
                                            .iter()
                                            .map(|id| id.to_string().into())
                                            .collect::<Vec<_>>();

                                        if message_ids.is_empty() {
                                            continue;
                                        }

                                        debug!(chat = ?chat, ids = message_ids.len(), "MessageDeleted");
                                        let _ = telegram.tx.send(BackendEvent::MessageDeleted { chat, message_ids });

                                        *telegram.cached_chats.lock().await = None;
                                    }
                                    Update::Raw(raw) => {
                                        if let tl::enums::Update::UserStatus(ref u) = *raw {
                                            let chat_id = ChatId::Telegram(u.user_id);
                                            let status = telegram_status_label(&u.status);
                                            debug!(
                                                chat_id = ?chat_id,
                                                status = ?status,
                                                "UserStatus update"
                                            );
                                            let _ = telegram.tx.send(BackendEvent::ChatUpdated(Chat {
                                                id: chat_id,
                                                contact_name: String::new(),
                                                last_message_ts: None,
                                                status,
                                                fixed: false,
                                                verified: false,
                                                scroll: 0,
                                                unread: false,
                                                unread_count: 0,
                                            }));
                                        } else if let tl::enums::Update::ReadHistoryInbox(ref u) = *raw {
                                            let peer_id = match &u.peer {
                                                tl::enums::Peer::User(u) => u.user_id,
                                                tl::enums::Peer::Chat(c) => c.chat_id,
                                                tl::enums::Peer::Channel(c) => c.channel_id,
                                            };

                                            let chat_id = ChatId::Telegram(peer_id);
                                            debug!(
                                                chat_id = ?chat_id,
                                                still_unread = u.still_unread_count,
                                                max_id = u.max_id,
                                                "ReadHistoryInbox"
                                            );
                                            let _ = telegram.tx.send(BackendEvent::UnreadUpdated {
                                                chat: chat_id,
                                                unread: u.still_unread_count > 0,
                                                unread_count: u.still_unread_count,
                                            });

                                            *telegram.cached_chats.lock().await = None;
                                        } else if let tl::enums::Update::ReadChannelInbox(ref u) = *raw {
                                            let chat_id = ChatId::Telegram(u.channel_id);
                                            debug!(
                                                chat_id = ?chat_id,
                                                still_unread = u.still_unread_count,
                                                max_id = u.max_id,
                                                "ReadChannelInbox"
                                            );
                                            let _ = telegram.tx.send(BackendEvent::UnreadUpdated {
                                                chat: chat_id,
                                                unread: u.still_unread_count > 0,
                                                unread_count: u.still_unread_count,
                                            });

                                            *telegram.cached_chats.lock().await = None;
                                        } else {
                                            debug!("Telegram update (unhandled): {raw:?}");
                                        }
                                    }
                                    _ => {}
                                },
                                Err(e) => {
                                    let reason = format!("Telegram update stream error: {e}");
                                    stream_reason = Some(reason.clone());
                                    if let Err(sync_err) = stream.sync_update_state().await {
                                        error!("Failed to sync update state before reconnect: {sync_err}");
                                    }
                                    let _ = telegram.tx.send(BackendEvent::Disconnected(reason));
                                    break;
                                }
                            }
                        }
                    }
                }

                if telegram.shutdown.load(Ordering::SeqCst) {
                    break;
                }

                let Some(reason) = stream_reason else {
                    break;
                };

                let Some(delay) = telegram
                    .handle_stream_failure(
                        &reason,
                        reconnect_attempt,
                        !current_client.is_authorized().await.unwrap_or(false),
                        MAX_RECONNECT_ATTEMPTS,
                    )
                    .await
                else {
                    break;
                };

                reconnect_attempt += 1;

                if delay.is_zero() {
                    break;
                }

                tokio::time::sleep(delay).await;

                if telegram.shutdown.load(Ordering::SeqCst) {
                    break;
                }

                current_updates = Some(telegram.replace_client_with_new_pool().await);
            }
        });
    }

    /// React to an update-stream failure.
    ///
    /// Returns `Some(delay)` when the listener should sleep and then retry with a
    /// fresh client pool, and `None` when the listener is done: shutdown /
    /// irrecoverable, a session that is no longer valid, or the bounded reconnect
    /// window was exhausted (the TUI shows a "connection lost" state and the user
    /// can press the retry key to [`TelegramMessenger::reconnect`]).
    async fn handle_stream_failure(
        &self,
        reason: &str,
        reconnect_attempt: u32,
        unauthenticated: bool,
        max_attempts: u32,
    ) -> Option<Duration> {
        if Self::is_session_invalid(reason, unauthenticated) {
            self.connection_lost.store(true, Ordering::SeqCst);
            let _ = self.tx.send(BackendEvent::AuthInvalid(reason.to_string()));
            return None;
        }

        if reconnect_attempt >= max_attempts {
            self.connection_lost.store(true, Ordering::SeqCst);
            let _ = self
                .tx
                .send(BackendEvent::Status("Telegram connection lost".to_string()));
            return None;
        }

        self.connection_lost.store(false, Ordering::SeqCst);
        let delay = Self::reconnect_delay(reason, reconnect_attempt, unauthenticated);
        if delay.is_zero() {
            return None;
        }
        let _ = self.tx.send(BackendEvent::Status(format!(
            "Telegram disconnected — reconnecting in {}s",
            delay.as_secs()
        )));
        Some(delay)
    }

    fn is_session_invalid(reason: &str, unauthenticated: bool) -> bool {
        let lowercase = reason.to_ascii_lowercase();
        lowercase.contains("not authenticated")
            || lowercase.contains("session_password_needed")
            || lowercase.contains("session_revoked")
            || lowercase.contains("session_invalid")
            || lowercase.contains("auth_key_unregistered")
            || lowercase.contains("auth_key_invalid")
            || lowercase.contains("invalid session")
            || lowercase.contains("invalid auth")
            || unauthenticated
    }

    fn reconnect_delay(reason: &str, attempt: u32, unauthenticated: bool) -> Duration {
        let lowercase = reason.to_ascii_lowercase();

        if lowercase.contains("dropped")
            || lowercase.contains("quit")
            || lowercase.contains("shutdown")
        {
            return Duration::ZERO;
        }

        if lowercase.contains("flood_wait") {
            return Duration::from_secs(30);
        }

        if Self::is_session_invalid(reason, unauthenticated) {
            return Duration::from_secs(30);
        }

        let backoff = 1u32 << attempt.min(5);
        Duration::from_secs(backoff.min(30) as u64)
    }

    // BUG: this is not auto updating, add to tokio refresh maybe?
    async fn status_for_peer_ref(
        &self,
        client: &Client,
        peer_ref: &grammers_session::types::PeerRef,
    ) -> Result<Option<String>, BackendError> {
        let peer = client.resolve_peer(*peer_ref).await?;
        let grammers_client::peer::Peer::User(user) = peer else {
            return Ok(None);
        };

        Ok(telegram_status_label(user.status()))
    }

    fn cache_dialogs(&self, dialogs: Vec<Dialog>) {
        *self.cached_dialogs.lock().unwrap() = dialogs;
    }

    fn find_dialog_peer_ref(
        &self,
        bare_id: i64,
    ) -> Option<grammers_client::session::types::PeerRef> {
        self.cached_dialogs
            .lock()
            .unwrap()
            .iter()
            .find(|d| d.peer.id().bare_id() == Some(bare_id))
            .map(|d| d.peer_ref())
    }

    async fn fetch_chats_once(&self) -> Result<Vec<Chat>, BackendError> {
        info!("Fetching Telegram dialogs...");

        let client = self.current_client().await;
        let mut dialogs_iter = client.iter_dialogs();
        let mut chats = Vec::new();
        let mut raw_dialogs = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();

        while let Some(dialog) = dialogs_iter.next().await? {
            let peer = dialog.peer();
            let Some(bare_id) = peer.id().bare_id().filter(|id| *id != 0) else {
                debug!("Skipping Telegram dialog without usable peer id");
                continue;
            };
            let id = ChatId::Telegram(bare_id);
            let contact_name = peer.name().unwrap_or("Unknown").to_string();

            if !seen_ids.insert(bare_id) {
                warn!(
                    bare_id,
                    contact_name = %contact_name,
                    "Skipping duplicate Telegram dialog peer"
                );
                continue;
            }

            let last_message_ts = dialog
                .last_message
                .as_ref()
                .map(|msg| msg.date().timestamp());

            let unread_count = match &dialog.raw {
                grammers_client::tl::enums::Dialog::Dialog(d) => d.unread_count,
                grammers_client::tl::enums::Dialog::Folder(_) => 0,
            };
            let fixed = match &dialog.raw {
                grammers_client::tl::enums::Dialog::Dialog(d) => d.pinned,
                grammers_client::tl::enums::Dialog::Folder(d) => d.pinned,
            };
            let status = self
                .status_for_peer_ref(&client, &dialog.peer_ref())
                .await
                .ok()
                .flatten();

            chats.push(Chat {
                id,
                contact_name,
                last_message_ts,
                status,
                fixed,
                verified: false,
                unread: unread_count > 0,
                unread_count,
                scroll: 0,
            });
            raw_dialogs.push(dialog);
        }

        info!(
            dialogs = chats.len(),
            names = ?chats
                .iter()
                .map(|chat| (&chat.contact_name, &chat.id))
                .collect::<Vec<_>>(),
            "Loaded Telegram dialogs"
        );

        self.cache_dialogs(raw_dialogs);

        Ok(chats)
    }

    fn clear_status(&self) {
        let _ = self.tx.send(BackendEvent::Status(String::new()));
    }

    /// Sleep for `wait`, checking the cancellation flag so an in-flight refresh
    /// aborts promptly when the user presses the cancel key. Returns
    /// `Err(FloodWait(remaining))` on cancellation so the caller defers the
    /// retry to the next chat-list sync tick.
    async fn sleep_with_cancel(&self, wait: Duration) -> Result<(), BackendError> {
        let start = Instant::now();
        loop {
            if self.cancel_refresh.load(Ordering::SeqCst) {
                let remaining = wait.saturating_sub(start.elapsed()).as_secs().max(1);
                warn!(remaining, "Telegram refresh cancelled by user");
                return Err(BackendError::FloodWait(remaining));
            }

            if start.elapsed() >= wait {
                return Ok(());
            }

            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

/// Fetches the quoted original message for an incoming reply and builds its
/// context for display. Returns `None` when `msg` is not a reply.
async fn reply_context(
    _client: &Client,
    msg: &grammers_client::message::Message,
) -> Option<ReplyContext> {
    msg.reply_to_message_id()?;

    let target: grammers_client::message::Message = match msg.get_reply().await {
        Ok(Some(m)) => m,
        _ => return None,
    };

    let id = target.id().to_string().into();
    let sender = if target.outgoing() {
        "You".to_string()
    } else {
        target
            .sender()
            .and_then(|p| p.name())
            .unwrap_or("Unknown")
            .to_string()
    };

    let text = target.text().to_string();
    let timestamp = target.date().timestamp();

    Some(ReplyContext {
        id,
        sender,
        text,
        timestamp,
    })
}

#[async_trait::async_trait]
impl Messenger for TelegramMessenger {
    async fn status(&self, chat: &ChatId) -> Result<Option<String>, BackendError> {
        let ChatId::Telegram(bare_id) = chat else {
            return Ok(None);
        };

        let client = self.current_client().await;
        let peer_ref = self
            .find_dialog_peer_ref(*bare_id)
            .or_else(|| {
                self.cached_dialogs
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|d| d.peer.id().bare_id() == Some(*bare_id))
                    .map(|d| d.peer_ref())
            })
            .ok_or_else(|| {
                BackendError::Other(format!("chat {bare_id} not found in dialog cache"))
            })?;

        self.status_for_peer_ref(&client, &peer_ref).await
    }

    async fn is_authenticated(&self) -> bool {
        self.current_client()
            .await
            .is_authorized()
            .await
            .unwrap_or(false)
    }

    async fn cancel_chat_refresh(&self) {
        self.cancel_refresh.store(true, Ordering::SeqCst);
    }

    async fn reconnect(&self) -> Result<(), BackendError> {
        let _ = self
            .tx
            .send(BackendEvent::Status(String::from("reconnecting...")));

        info!("reconnecting Telegram...");

        if self.shutdown.load(Ordering::SeqCst) || !self.enabled.load(Ordering::SeqCst) {
            return Ok(());
        }

        if !self.connection_lost.swap(false, Ordering::SeqCst) {
            return Ok(());
        }

        self.clear_status();
        let updates = self.replace_client_with_new_pool().await;
        self.spawn_update_listener(updates);
        Ok(())
    }

    async fn chats(&self) -> Result<Vec<Chat>, BackendError> {
        const MAX_REFRESH_WAIT: u64 = 120;

        self.cancel_refresh.store(false, Ordering::SeqCst);

        loop {
            let refresh = self.chat_refresh.lock().await;
            {
                let cached = self.cached_chats.lock().await;
                if let Some((loaded_at, chats)) = cached.as_ref()
                    && loaded_at.elapsed() < std::time::Duration::from_secs(5)
                {
                    debug!("Using cached Telegram dialogs");
                    return Ok(chats.clone());
                }
            }

            let cooldown = {
                let mut wait_until = self.flood_wait_until.lock().await;
                match *wait_until {
                    Some(until) if until > Instant::now() => Some(until),
                    Some(_) => {
                        *wait_until = None;
                        None
                    }
                    None => None,
                }
            };

            if let Some(until) = cooldown {
                let wait = until.saturating_duration_since(Instant::now());
                drop(refresh);
                let _ = self.tx.send(BackendEvent::RateLimited {
                    retry_after_secs: wait.as_secs().max(1),
                });
                self.sleep_with_cancel(wait).await?;
                continue;
            }

            match self.fetch_chats_once().await {
                Ok(chats) => {
                    *self.cached_chats.lock().await = Some((Instant::now(), chats.clone()));
                    self.clear_status();
                    return Ok(chats);
                }

                Err(BackendError::FloodWait(seconds)) if seconds <= MAX_REFRESH_WAIT => {
                    let until = Instant::now() + std::time::Duration::from_secs(seconds);
                    *self.flood_wait_until.lock().await = Some(until);
                    drop(refresh);
                    let _ = self.tx.send(BackendEvent::RateLimited {
                        retry_after_secs: seconds.max(1),
                    });
                    self.sleep_with_cancel(std::time::Duration::from_secs(seconds))
                        .await?;
                }
                Err(BackendError::FloodWait(seconds)) => {
                    drop(refresh);
                    let until = Instant::now() + std::time::Duration::from_secs(seconds);
                    *self.flood_wait_until.lock().await = Some(until);
                    let _ = self.tx.send(BackendEvent::RateLimited {
                        retry_after_secs: seconds.max(1),
                    });
                    return Err(BackendError::FloodWait(seconds));
                }
                Err(error) => {
                    drop(refresh);
                    return Err(error);
                }
            }
        }
    }

    async fn set_read(&mut self, chat: &ChatId) -> Result<(), BackendError> {
        let bare_id = match chat {
            ChatId::Telegram(id) => *id,
            _ => return Ok(()),
        };
        let peer_ref = self
            .find_dialog_peer_ref(bare_id)
            .ok_or_else(|| BackendError::Other("chat not found in dialog cache".into()))?;
        debug!(bare_id, "Marking chat as read");
        let client = self.current_client().await;
        client.mark_as_read(peer_ref).await?;
        Ok(())
    }

    async fn history(&self, chat: &ChatId) -> Result<Vec<Message>, BackendError> {
        self.history_page(chat, None, 100).await
    }

    async fn history_page(
        &self,
        chat: &ChatId,
        offset_id: Option<i32>,
        limit: usize,
    ) -> Result<Vec<Message>, BackendError> {
        let bare_id = match chat {
            ChatId::Telegram(id) => *id,
            _ => return Err(BackendError::Other("not a Telegram chat".into())),
        };

        let peer_ref = self
            .find_dialog_peer_ref(bare_id)
            .ok_or_else(|| BackendError::Other("chat not found in dialog cache".into()))?;

        debug!(bare_id, offset_id, limit, "Fetching paged message history");

        let client = self.current_client().await;
        let own_chat = self_user_id(&client).await;
        let mut messages_rev = Vec::new();
        let mut msg_iter = client.iter_messages(peer_ref).limit(limit.max(1));

        if let Some(offset) = offset_id {
            msg_iter = msg_iter.offset_id(offset);
        }

        while let Some(msg) = msg_iter.next().await? {
            let message =
                normalize_telegram_message(&msg, chat, own_chat.as_ref(), None, false, false);
            messages_rev.push(message);
        }

        messages_rev.reverse();
        debug!(bare_id, count = messages_rev.len(), "History page loaded");
        Ok(messages_rev)
    }

    async fn reply_context(
        &self,
        chat: &ChatId,
        message_id: &MessageId,
    ) -> Result<Option<ReplyContext>, BackendError> {
        let cache_key = (chat.clone(), message_id.clone());

        if let Some(context) = self.reply_contexts.lock().await.get(&cache_key).cloned() {
            return Ok(Some(context));
        }

        let bare_id = match chat {
            ChatId::Telegram(id) => *id,
            _ => return Err(BackendError::Other("not a Telegram chat".into())),
        };

        let peer_ref = self
            .find_dialog_peer_ref(bare_id)
            .ok_or_else(|| BackendError::Other("chat not found in dialog cache".into()))?;
        let id = message_id.to_i32().ok_or_else(|| {
            BackendError::Other("telegram reply needs a numeric message id".into())
        })?;

        let client = self.current_client().await;
        let message = client
            .get_messages_by_id(peer_ref, &[id])
            .await?
            .into_iter()
            .next()
            .flatten();

        match message {
            Some(message) => {
                let context = reply_context(&client, &message).await;

                if let Some(context) = &context {
                    self.reply_contexts
                        .lock()
                        .await
                        .insert(cache_key, context.clone());
                }

                Ok(context)
            }
            None => Ok(None),
        }
    }

    async fn send(
        &self,
        chat: &ChatId,
        text: &str,
        reply_to: Option<MessageId>,
    ) -> Result<Message, BackendError> {
        let bare_id = match chat {
            ChatId::Telegram(id) => *id,
            _ => return Err(BackendError::Other("not a Telegram chat".into())),
        };

        let peer_ref = self
            .find_dialog_peer_ref(bare_id)
            .ok_or_else(|| BackendError::Other("chat not found in dialog cache".into()))?;

        let mut input = grammers_client::message::InputMessage::new().text(text.to_string());
        if let Some(reply_id) = reply_to.as_ref().and_then(|id| id.to_i32()) {
            input = input.reply_to(Some(reply_id));
        }

        debug!(bare_id, "Sending message");
        let client = self.current_client().await;
        let sent = client.send_message(peer_ref, input).await?;

        let own_chat = Some(chat);
        let message = normalize_telegram_message(&sent, chat, own_chat, None, false, false);
        Ok(Message {
            reply_to_id: reply_to.clone(),
            ..message
        })
    }

    async fn delete(&self, chat: &ChatId, id: &MessageId) -> Result<(), BackendError> {
        let bare_id = match chat {
            ChatId::Telegram(id) => *id,
            _ => return Err(BackendError::Other("not a Telegram chat".into())),
        };
        let msg_id = id.to_i32().ok_or_else(|| {
            BackendError::Other("telegram delete needs a numeric message id".into())
        })?;

        let peer_ref = self
            .find_dialog_peer_ref(bare_id)
            .ok_or_else(|| BackendError::Other("chat not found in dialog cache".into()))?;

        debug!(bare_id, msg_id, "Deleting message");

        let client = self.current_client().await;
        client.delete_messages(peer_ref, &[msg_id]).await?;

        Ok(())
    }

    async fn edit(&self, chat: &ChatId, id: &MessageId, text: &str) -> Result<(), BackendError> {
        let bare_id = match chat {
            ChatId::Telegram(id) => *id,
            _ => return Err(BackendError::Other("not a Telegram chat".into())),
        };
        let msg_id = id.to_i32().ok_or_else(|| {
            BackendError::Other("telegram edit needs a numeric message id".into())
        })?;

        let peer_ref = self
            .find_dialog_peer_ref(bare_id)
            .ok_or_else(|| BackendError::Other("chat not found in dialog cache".into()))?;

        let input = grammers_client::message::InputMessage::new().text(text.to_string());
        let client = self.current_client().await;

        client.edit_message(peer_ref, msg_id, input).await?;
        Ok(())
    }

    async fn media_bytes(
        &self,
        chat: &ChatId,
        message_id: &MessageId,
    ) -> Result<Option<Vec<u8>>, BackendError> {
        let bare_id = match chat {
            ChatId::Telegram(id) => *id,
            _ => return Err(BackendError::Other("not a Telegram chat".into())),
        };
        let Some(id) = message_id.to_i32() else {
            return Ok(None);
        };

        let peer_ref = self
            .find_dialog_peer_ref(bare_id)
            .ok_or_else(|| BackendError::Other("chat not found in dialog cache".into()))?;

        debug!(bare_id, message_id = id, "Fetching media bytes on demand");
        let client = self.current_client().await;
        let Some(msg) = client
            .get_messages_by_id(peer_ref, &[id])
            .await?
            .into_iter()
            .flatten()
            .next()
        else {
            return Ok(None);
        };
        let Some(media) = msg.media() else {
            return Ok(None);
        };

        let mut bytes = Vec::new();
        let mut download = client.iter_download(&media);
        while let Some(chunk) = download.next().await? {
            bytes.extend_from_slice(&chunk);
        }
        Ok((!bytes.is_empty()).then_some(bytes))
    }

    fn subscribe(&self) -> broadcast::Receiver<BackendEvent> {
        let receiver = self.tx.subscribe();
        // Register the receiver above before starting the listener so the
        // initial `Connected` event is delivered and not dropped by the
        // `broadcast` channel having no subscribers yet.
        self.maybe_start_listener();
        receiver
    }

    async fn disconnect(&mut self) -> Result<(), BackendError> {
        self.shutdown.store(true, Ordering::SeqCst);

        let client = self.current_client().await;

        client.disconnect();

        let _ = self.tx.send(BackendEvent::Disconnected(
            "Telegram disconnected".to_string(),
        ));
        Ok(())
    }

    async fn cancel_login(&mut self) {
        *self.login_token.write().await = None;
    }

    fn login_steps(&self) -> Vec<AuthSteps> {
        vec![AuthSteps::Phone, AuthSteps::Code, AuthSteps::Password]
    }

    fn login_placeholder(&self, step: usize) -> &'static str {
        match step {
            0 => "+1234567890",
            1 => "12345",
            2 => "your 2FA password",
            _ => "",
        }
    }

    async fn login_step(
        &mut self,
        step: usize,
        input: &str,
    ) -> Result<LoginStepState, BackendError> {
        match step {
            0 => {
                info!(phone = input, "Telegram login: requesting code");
                let client = self.current_client().await;
                let token = client.request_login_code(input, &self.api_hash).await?;
                *self.login_token.write().await = Some(LoginToken::CodeRequested { token });
                Ok(LoginStepState::NextStep)
            }
            1 => {
                let token = self.login_token.write().await.take().ok_or_else(|| {
                    BackendError::Other("no login token – call request_code first".into())
                })?;

                match token {
                    LoginToken::CodeRequested { token } => {
                        info!("Telegram login: submitting code");

                        match self.current_client().await.sign_in(&token, input).await {
                            Ok(_user) => {
                                info!("Telegram login: success");
                                Ok(LoginStepState::Done)
                            }
                            Err(SignInError::PasswordRequired(password_token)) => {
                                info!("Telegram login: 2FA password required");
                                *self.login_token.write().await =
                                    Some(LoginToken::PasswordRequired {
                                        token: password_token,
                                    });
                                Ok(LoginStepState::NextStep)
                            }
                            Err(SignInError::InvalidCode) => {
                                warn!("Telegram login: invalid code");
                                Err(BackendError::Other("invalid code".into()))
                            }
                            Err(SignInError::SignUpRequired) => {
                                warn!("Telegram login: sign-up required");
                                Err(BackendError::Other(
                                "sign-up required – create an account with an official Telegram client first"
                                    .into(),
                            ))
                            }
                            Err(other) => {
                                error!("Telegram login failed: {other}");
                                Err(BackendError::Other(format!("sign-in failed: {other}")))
                            }
                        }
                    }
                    LoginToken::PasswordRequired { .. } => Err(BackendError::Other(
                        "call check_password instead of sign_in".into(),
                    )),
                }
            }
            2 => {
                let token = self
                    .login_token
                    .write()
                    .await
                    .take()
                    .ok_or_else(|| BackendError::Other("no password token".into()))?;

                match token {
                    LoginToken::PasswordRequired { token } => {
                        info!("Telegram login: submitting 2FA password");

                        match self
                            .current_client()
                            .await
                            .check_password(token, input.as_bytes())
                            .await
                        {
                            Ok(_user) => {
                                info!("Telegram login: 2FA success");
                                Ok(LoginStepState::Done)
                            }
                            Err(SignInError::InvalidPassword(_)) => {
                                warn!("Telegram login: invalid password");
                                Err(BackendError::Other("invalid password".into()))
                            }
                            Err(other) => {
                                error!("Telegram password check failed: {other}");
                                Err(BackendError::Other(format!(
                                    "password check failed: {other}"
                                )))
                            }
                        }
                    }
                    LoginToken::CodeRequested { .. } => Err(BackendError::Other(
                        "call sign_in instead of check_password".into(),
                    )),
                }
            }
            _ => Err(BackendError::Other("unknown login step".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_messenger(enabled: bool) -> (TelegramMessenger, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "senders_tg_lazy_{}_{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("")
                .replace(':', "_")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let messenger = TelegramMessenger::new(
            dir.clone(),
            12345,
            "0123456789abcdef0123456789abcdef",
            1,
            enabled,
        )
        .await
        .expect("messenger constructs offline");
        (messenger, dir)
    }

    fn updates_stored(messenger: &TelegramMessenger) -> bool {
        messenger.updates.lock().unwrap().is_some()
    }

    #[tokio::test]
    async fn listener_is_lazy_and_gated_on_enabled() {
        let (messenger, dir) = test_messenger(false).await;

        // Construction never starts the listener: the `updates` receiver stays
        // parked in the messenger.
        assert!(updates_stored(&messenger), "no eager listener");

        // A disabled messenger that is subscribed to must not spawn the
        // listener either (no requests, no events) — the receiver is kept.
        let receiver = messenger.subscribe();
        assert!(updates_stored(&messenger), "disabled subscribe stays inert");
        drop(receiver);

        // Enabling consumes the receiver exactly once and starts the listener;
        // further enable calls are no-ops.
        messenger.set_enabled(true);
        assert!(
            !updates_stored(&messenger),
            "enabled subscribe starts listener"
        );
        messenger.set_enabled(true);
        assert!(!updates_stored(&messenger), "listener started only once");

        messenger.shutdown.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
