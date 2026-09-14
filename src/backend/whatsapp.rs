use crate::config::Config;
use crate::helpers::now;

use super::{
    AuthSteps, BackendError, BackendEvent, Chat, ChatId, LoginStepState, MediaKind, Message,
    MessageAction, MessageId, MessageMedia, Messenger, ReplyContext,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::broadcast::Sender;
use tokio::sync::{RwLock, broadcast};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use whatsapp_rust::Client;
use whatsapp_rust::prelude::{
    Bot, Event, Jid, MessageBuilderExt, MessageExt, MessageInfo, Server, SqliteStore, wa,
};
use whatsapp_rust::wacore_binary::JidExt;
use whatsapp_rust::waproto::whatsapp::HistorySync;
use whatsapp_rust::waproto::whatsapp::Message as WaMessage;

type SharedState = Arc<RwLock<WhatsAppState>>;

/// In-memory mirror of the account's conversations and messages, populated from
/// `Event::HistorySync` and `Event::Messages` delivered by the whatsapp-rust
/// bot. SQLite persists the session/auth credentials; this cache drives the TUI.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct WhatsAppState {
    // TODO: check if needed: controls wether the provider is actually enabled in the config and if
    // it should load
    enabled: bool,
    chats: Vec<Chat>,
    history: HashMap<ChatId, Vec<Message>>,
    /// Stanza is a XML-like structure exchanged between client and server.
    /// Each Stanza contains data that identifies and populates the message
    /// So basically, Stanza == Message
    by_stanza_id: HashMap<String, (ChatId, MessageId)>,

    /// JID -> push name cache populated from HistorySync pushnames.
    pushnames: HashMap<String, String>,
    /// JID string -> display name learned from usync (the peer's username or
    /// verified business name), keyed the way `pushnames` are (peer PN or LID).
    /// Names the phone itself declines to sync still resolve to something
    /// human-readable across restarts.
    #[serde(default)]
    usync_names: HashMap<String, String>,
    /// JID strings (same keys as `usync_names`) whose peer is a verified
    /// business; the TUI renders a check mark next to the name.
    #[serde(default)]
    usync_verified: HashSet<String>,
    /// JID strings already queried through usync, kept so a peer that has no
    /// username/business name is not re-queried on every history sync.
    #[serde(default)]
    usync_attempted: HashSet<String>,
    /// Epoch seconds of the first `Event::HistorySync` after a connect. The
    /// connect-time retry loop parks on `None`: it must not stop re-requesting
    /// the snapshot just because the chat list was seeded from the cache
    /// (`state.chats` is non-empty even when no conversation sync ever lands).
    #[serde(default)]
    sync_seen_at: Option<i64>,
    /// LID user-part -> phone-number user-part, learned from the client's local
    /// LID↔PN cache (`Client::get_lid_pn_entry`). Lets live `@lid` messages and
    /// conversations route to the phone-keyed chat row the user opens, and gives
    /// LID senders a readable name.
    #[serde(default)]
    lid_pn: HashMap<String, String>,
    /// Peer JID string -> (online, last_seen) learned from `Event::Presence`.
    #[serde(default)]
    presence: HashMap<String, (bool, Option<i64>)>,
    /// The account's own LID and phone-number JIDs (`to_non_ad_string`), used
    /// to label the self-chat ("Myself") instead of the formatted phone number.
    /// Learned from `client.lid()`/`client.pn()` on connect and history sync
    /// (an Option so the cache survives before the first learn).
    #[serde(default)]
    own_lid: Option<String>,
    #[serde(default)]
    own_pn: Option<String>,
}

fn message_actions(from_me: bool) -> Vec<MessageAction> {
    let mut actions = vec![MessageAction::Reply];
    if from_me {
        actions.extend([MessageAction::Edit, MessageAction::Delete]);
    }
    actions
}

/// Canonicalize the chat key for a message sent by the user's own account.
///
/// Live self-messages arrive with the chat keyed by the account's own LID, but
/// the HistorySync snapshot (and thus the persisted cache) stores the
/// self-chat under the own phone number. Rewrite the former to the latter so
/// both paths land in the same chat and no transient LID chat appears that
/// would vanish on the next restart.
fn own_chat_id(own_lid: Option<&Jid>, own_pn: Option<&Jid>, chat: ChatId, from_me: bool) -> ChatId {
    if !from_me {
        return chat;
    }

    let ChatId::WhatsApp(raw) = &chat else {
        return chat;
    };

    let is_own_lid = own_lid.is_some_and(|lid| lid.to_non_ad_string().as_str() == raw.as_str());

    if is_own_lid && let Some(pn) = own_pn {
        return ChatId::WhatsApp(pn.to_non_ad_string());
    }

    chat
}

/// Whether a chat key is the account's own self-chat (own LID or own phone
/// number). Used to label the "message yourself" thread "Myself" instead of a
/// formatted number.
fn is_self_chat(chat: &ChatId, own_lid: Option<&str>, own_pn: Option<&str>) -> bool {
    let ChatId::WhatsApp(raw) = chat else {
        return false;
    };
    own_lid == Some(raw.as_str()) || own_pn == Some(raw.as_str())
}

/// Build the `@s.whatsapp.net` chat id for a phone-number user-part.
fn pn_chat_id(pn_user: &str) -> ChatId {
    ChatId::WhatsApp(Jid::pn(pn_user).to_non_ad_string())
}

/// Rewrite a `@lid` chat key to its phone-keyed twin when the local LID↔PN
/// mapping knows the peer and the PN row already exists (the row the user
/// opens). Peers never addressed by phone keep the LID key.
fn fold_lid_key(key: &str, st: &WhatsAppState) -> ChatId {
    let Ok(jid) = Jid::from_str(key) else {
        return ChatId::WhatsApp(key.to_string());
    };
    if !jid.is_lid() {
        return ChatId::WhatsApp(key.to_string());
    }

    let Some(pn_user) = st.lid_pn.get(jid.user_base()) else {
        return ChatId::WhatsApp(key.to_string());
    };

    let pn_chat = pn_chat_id(pn_user);
    if st.chats.iter().any(|c| c.id == pn_chat) {
        pn_chat
    } else {
        ChatId::WhatsApp(key.to_string())
    }
}

/// Canonicalize a chat key to the conversation row the TUI actually opens.
///
/// WhatsApp addresses 1:1 threads by LID on the wire, but the phone's history
/// sync may key the same thread by the peer's phone number. A message or
/// conversation that arrives under one form must land in the row the user is
/// looking at, so a `@lid` chat is rewritten to its phone-keyed row when that
/// row already exists. The mapping is learned from the client's local LID↔PN
/// cache (and persisted) on first sight (offline callers pass `None` and only
/// fold through what is already known); see [`fold_lid_key`] for the read-only
/// side that never needs a client.
async fn canonical_chat_id(
    client: Option<&Arc<Client>>,
    state: &SharedState,
    chat: ChatId,
) -> ChatId {
    let ChatId::WhatsApp(raw) = &chat else {
        return chat;
    };
    let Ok(jid) = Jid::from_str(raw) else {
        return chat;
    };
    if !jid.is_lid() {
        return chat;
    }

    let known = state
        .read()
        .await
        .lid_pn
        .contains_key(jid.user_base());
    if !known {
        let Some(client) = client else {
            return chat;
        };
        let Ok(Some(entry)) = client.get_lid_pn_entry(&jid).await else {
            return chat;
        };
        state.write().await.lid_pn.insert(
            jid.user_base().to_string(),
            entry.phone_number.to_string(),
        );
    }

    let st = state.read().await;
    fold_lid_key(raw, &st)
}

/// Convert [`WaMessage`] to [`MessageMedia`]
fn handle_wa_media(msg: &WaMessage) -> Option<MessageMedia> {
    let base = msg.get_base_message();
    let kind = if base.image_message.is_set() {
        MediaKind::Image
    } else if base.video_message.is_set() {
        MediaKind::Video
    } else if base.audio_message.is_set() {
        MediaKind::Audio
    } else if base.sticker_message.is_set() {
        MediaKind::Sticker
    } else if base.document_message.is_set() {
        MediaKind::Document
    } else {
        return None;
    };

    let caption = base.get_caption().map(str::to_string);

    let file_name = base
        .document_message
        .as_option()
        .and_then(|d| d.file_name.clone());

    Some(MessageMedia {
        kind,
        caption,
        file_name,
    })
}

/// Converts [`Message`] to [`WaMessage`] / [`MessageInfo`]
///
/// `info.source.chat` is the conversation JID (used as the chat id);
/// `info.source.sender` is the per-message author JID (the participant inside a
/// group, or the peer in a one-to-one chat). `info.push_name` already holds the
/// author's display name when `from_me` is false.
fn to_senders_msg(
    chat: ChatId,
    info: &MessageInfo,
    msg: &WaMessage,
    state: &WhatsAppState,
) -> Message {
    let from_me = info.source.is_from_me;
    let sender = if from_me {
        "You".to_string()
    } else {
        resolve_sender_name(
            info.push_name.as_str(),
            &info.source.sender,
            &state.pushnames,
            &state.lid_pn,
        )
    };

    let media = handle_wa_media(msg);
    let text = if let Some(media) = &media {
        media
            .caption
            .clone()
            .filter(|c| !c.trim().is_empty())
            .unwrap_or_else(|| format!("{}, click to show", media.kind.label()))
    } else {
        msg.get_base_message()
            .text_content()
            .unwrap_or("")
            .to_string()
    };

    // INFO: view stanza definition in reply_stanza_id function docs
    let stanza_id = info.id.to_string();
    let reply_to_id = reply_stanza_id(msg).map(MessageId::from);

    let reply_ctx = reply_to_id.as_ref().and_then(|id| {
        state
            .by_stanza_id
            .get(&**id)
            .and_then(|(c, _)| state.history.get(c))
            .and_then(|msgs| msgs.iter().find(|m| m.message_id == *id))
            .map(|m| ReplyContext {
                id: m.message_id.clone(),
                sender: m.sender.clone(),
                text: m.text.clone(),
                timestamp: m.timestamp,
            })
    });

    Message {
        message_id: stanza_id.into(),
        chat,
        sender,
        author_id: Some(info.source.sender.to_string()),
        text,
        timestamp: info.timestamp.timestamp(),
        from_me,
        msg_actions: message_actions(from_me),
        media,
        reply_to_id,
        reply_ctx,
        pending: false,
        failed: false,
    }
}

/// Resolve a human-readable sender name for an inbound message.
///
/// Order: the message's own `push_name` (authoritative), then the push name
/// cache for the author JID. A LID author falls back to the phone-keyed push
/// name / formatted number via the local LID↔PN mapping. `"Unknown"` is
/// reserved for the truly unresolvable case so the TUI can keep a friendlier
/// fallback.
fn resolve_sender_name(
    push_name: &str,
    sender: &Jid,
    pushnames: &HashMap<String, String>,
    lid_pn: &HashMap<String, String>,
) -> String {
    if !push_name.trim().is_empty() {
        return push_name.to_string();
    }

    let key = sender.to_non_ad_string();

    if let Some(name) = pushnames.get(&key)
        && !name.trim().is_empty()
    {
        return name.clone();
    }

    if sender.is_lid()
        && let Some(pn_user) = lid_pn.get(sender.user_base())
    {
        return name_from_lid_pn(pn_user, pushnames);
    }

    let bare = sender.user_base();

    if !bare.is_empty() {
        bare.to_string()
    } else {
        "Unknown".to_string()
    }
}

/// Format a peer phone number the way WhatsApp does for contacts it has no
/// name for: `15550000001` -> `+15 55 0000-0001`. Non-numeric input (a bare
/// LID, a JID string) degrades to `+{digits}`.
fn format_pn(pn: &str) -> String {
    let digits: String = pn.chars().filter(|c| c.is_ascii_digit()).collect();

    if digits.len() < 8 {
        return format!("+{digits}");
    }

    let (cc, rest) = digits.split_at(2);
    let (area, number) = rest.split_at(2);
    let (prefix, suffix) = number.split_at(number.len() - 4);
    format!("+{cc} {area} {prefix}-{suffix}")
}

/// Name for a peer whose phone number we only learned locally from the LID↔PN
/// mapping (`Client::get_lid_pn_entry`): the synced push name wins when the
/// cache has one, otherwise the formatted number — the same fallback a thread
/// keyed by PN gets from `resolve_conversation_name`.
fn name_from_lid_pn(pn_user: &str, pushnames: &HashMap<String, String>) -> String {
    let pn_key = Jid::pn(pn_user).to_non_ad_string();

    pushnames
        .get(&pn_key)
        .filter(|n| !n.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| format_pn(pn_user))
}

/// Resolve the display name for a conversation from a history-sync record.
///
/// For groups the subject (`conversation.name`) is authoritative. For one-to-one
/// chats the saved contact name (`conversation.name`) or display name wins, with
/// the push-name cache, a usync-learned name (username or verified business
/// name), the formatted phone number and a bare JID as fallbacks.
///
/// Chats may be keyed by LID (`...@lid`); the thread's `pn_jid` (when present) is used as the
/// push-name / usync lookup key so a LID-keyed chat still resolves to the
/// contact's saved name.
///
/// The returned `bool` flags verified-business peers so the TUI can render a check mark.
fn resolve_conversation_name(
    conv: &wa::Conversation,
    pushnames: &HashMap<String, String>,
    usync_names: &HashMap<String, String>,
    usync_verified: &HashSet<String>,
) -> (String, bool) {
    if let Ok(jid) = Jid::from_str(&conv.id)
        && jid.is_group()
    {
        debug!("{jid} is a group named '{:?}' ", conv.name);
        return (
            conv.name
                .clone()
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| conv.id.clone()),
            false,
        );
    }

    // The peer's phone number when the thread is keyed by LID.
    let pn_key = conv
        .pn_jid
        .clone()
        .filter(|j| !j.trim().is_empty())
        .unwrap_or_else(|| conv.id.clone());

    if let Some(name) = conv
        .name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .or_else(|| conv.display_name.clone().filter(|n| !n.trim().is_empty()))
    {
        return (name, false);
    }

    if let Some(name) = pushnames.get(&pn_key).filter(|n| !n.trim().is_empty()) {
        return (name.clone(), false);
    }

    if let Some(name) = usync_names.get(&pn_key).filter(|n| !n.trim().is_empty()) {
        return (name.clone(), usync_verified.contains(&pn_key));
    }

    if let Ok(jid) = Jid::from_str(&pn_key)
        && jid.is_pn()
    {
        return (format_pn(jid.user_base()), false);
    }

    if let Ok(jid) = Jid::from_str(&conv.id) {
        let bare = jid.user_base().to_string();
        if !bare.is_empty() {
            return (bare, false);
        }
    }

    (conv.id.clone(), false)
}

/// Build a preview line for the chat list from the most recent message.
/// TODO: check this func: isn't this used elsewhere also?
fn preview_line(sender: &str, text: &str, from_me: bool) -> String {
    if from_me {
        format!("You: {text}")
    } else {
        format!("{sender}: {text}")
    }
}

/// The stanza id this message quotes, if any, from its `ContextInfo`.
///
/// Stanza is a XML-like structure exchanged between client and server.
/// Each Stanza contains data that identifies and populates the message
/// So basically, Stanza == Message
fn reply_stanza_id(msg: &wa::Message) -> Option<String> {
    let base = msg.get_base_message();
    let ctx = base
        .extended_text_message
        .as_option()
        .and_then(|etm| etm.context_info.as_option())
        .or_else(|| {
            base.image_message
                .as_option()
                .and_then(|i| i.context_info.as_option())
        })
        .or_else(|| {
            base.video_message
                .as_option()
                .and_then(|v| v.context_info.as_option())
        })
        .or_else(|| {
            base.document_message
                .as_option()
                .and_then(|d| d.context_info.as_option())
        });
    ctx.and_then(|c| c.stanza_id.clone())
}

#[derive(Clone)]
pub struct WhatsAppMessenger {
    client: Arc<Client>,
    /// The configured-but-not-yet-started bot. The bot is intentionally not
    /// spawned in `new()`: it is spun up lazily on the first [`Self::subscribe`]
    /// so the TUI (which subscribes before reading the event channel) can never
    /// miss the initial `Connected` / `QrCode` events, which a `broadcast`
    /// channel would otherwise drop for not-yet-registered receivers.
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
    /// and return a messenger bound to the live client. The bot is not started
    /// until [`Self::subscribe`] is first called.
    pub async fn new(store_path: String) -> Result<Self, BackendError> {
        let store = SqliteStore::new(&store_path)
            .await
            .map_err(|e| BackendError::Other(format!("WhatsApp: failed to open store: {e}")))?;

        let (tx, _) = broadcast::channel(128);

        // Dedicated WhatsApp cache. Must NOT share the TUI's `chats.json`:
        // that file holds a plain Vec<Chat> (see Config::save_chats) while this
        // cache holds the full WhatsAppState (history + pushnames + usync), and
        // the two schemas silently clobbered each other, leaving WhatsApp with
        // an empty state on every cold start.
        let chat_cache = Config::user_config_dir()?.join("wp_cache.json");

        let mut state = WhatsAppState::load_from(chat_cache.to_string_lossy().as_ref());

        // First run with the dedicated cache (or a fresh account): seed the
        // chat list from the TUI's persisted rows so usync enrichment can
        // backfill names immediately, even before the first history sync lands.
        if state.chats.is_empty()
            && let Ok(chat_cache) = Config::load_chats()
        {
            state.chats = chat_cache
                .into_iter()
                .filter(|c| c.id.tag() == "WA")
                .collect();
        }

        let state: SharedState = Arc::new(RwLock::new(state));
        let shutdown = Arc::new(AtomicBool::new(false));
        let current_qr: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));

        let builder = Bot::builder()
            .with_backend(store)
            .on_connected({
                let tx = tx.clone();
                let state = state.clone();
                let cache_path = chat_cache.clone();

                move |client| {
                    let tx = tx.clone();
                    let state = state.clone();
                    let cache_path = cache_path.clone();
                    async move {
                        Self::handle_connect(&client, &tx, &state, &cache_path);
                    }
                }
            })
            .on_qr_code({
                let tx = tx.clone();
                let current_qr = current_qr.clone();
                move |code, timeout| {
                    let tx = tx.clone();
                    let current_qr = current_qr.clone();
                    async move {
                        Self::handle_qr(&code, timeout, &tx, &current_qr).await;
                    }
                }
            })
            .on_logged_out({
                let tx = tx.clone();
                move |_info| {
                    let tx = tx.clone();
                    async move {
                        Self::handle_logged_out(&tx);
                    }
                }
            })
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

    /// Called once when the underlying server connection is established.
    /// Past here, message routing is done by `handle_event`.
    fn handle_connect(
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
            match client.request_syncd_snapshot_recovery("regular_high").await {
                Ok(response) => debug!("WA snapshot recovery accepted: {response}"),
                Err(e) => warn!("WA snapshot recovery failed: {e}"),
            }

            // Backfill names for chats already cached (or seeded from the TUI)
            // right away; this also republishes the list so stale names get
            // replaced without waiting for a sync.
            enrich_chat_names(&client, &state, &cache_path, &tx_for_enrich).await;

            // whatsapp-rust occasionally fails to decompress the snapshot PDO
            // ("data error") and never delivers the conversation list. Retry in
            // place until a history sync actually arrives, bounded, so a cold
            // start still recovers its history/pushnames. The chat list by
            // itself is not proof of delivery: it is seeded from the cache, so
            // a non-empty `chats` cannot gate the retry.
            for attempt in 0..2u32 {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                if state.read().await.sync_seen_at.is_some() {
                    break;
                }
                warn!(
                    "WA no history sync after recovery (attempt {}); re-requesting snapshot",
                    attempt + 1
                );
                if let Err(e) = client.request_syncd_snapshot_recovery("regular_high").await {
                    warn!("WA snapshot re-request failed: {e}");
                }
            }
        });

        let _ = tx.send(BackendEvent::Connected);
    }

    /// Called when a QR code is required for pairing.
    async fn handle_qr(
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
    fn handle_logged_out(tx: &Sender<BackendEvent>) {
        warn!("WhatsApp logged out");
        let _ = tx.send(BackendEvent::Disconnected(
            "WhatsApp logged out".to_string(),
        ));
    }

    /// Dispatch a routed `Event` coming from the server stream.
    /// Also handles reconnect/QR state and the chat list mutations (deletion,
    /// clear, mute/archive/pin/mark-as-read) that arrive over the syncd channel.
    async fn handle_event(
        event: &Arc<Event>,
        client: &Arc<Client>,
        state: &SharedState,
        tx: &Sender<BackendEvent>,
        current_qr: &Arc<RwLock<Option<String>>>,
        cache_path: &PathBuf,
    ) {
        match &**event {
            Event::Connected(_) => {
                let _ = tx.send(BackendEvent::Connected);
            }
            Event::Disconnected(_) | Event::StreamError(_) | Event::ConnectFailure(_) => {
                let _ = tx.send(BackendEvent::Disconnected(
                    "WhatsApp: Connection Failure!".into(),
                ));
            }
            Event::PairingQrCode(q) => {
                *current_qr.write().await = Some(q.code.clone());
                let _ = tx.send(BackendEvent::QrCode(q.code.clone()));
            }
            Event::Messages(batch) => {
                // Learn the account's own JIDs for self-chat labelling; cheap
                // and idempotent, and live messages may precede the first
                // history sync.
                state.write().await.learn_own_identity(client);
                for inbound in batch.messages.iter() {
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
                    let msg = to_senders_msg(chat.clone(), &inbound.info, &inbound.message, &state);

                    if msg.text.is_empty() && msg.media.is_none() {
                        drop(state);
                        continue;
                    }

                    let stanza_id = inbound.info.id.to_string();

                    state
                        .by_stanza_id
                        .insert(stanza_id.clone(), (chat.clone(), msg.message_id.clone()));

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
                        let cache_path = cache_path.clone();
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
                state.refresh_preview(&chat_id);
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
                    state.refresh_preview(&chat_id);
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
                let mut state = state.write().await;
                if let Some(chat) = state
                    .chats
                    .iter_mut()
                    .find(|c| c.id == ChatId::jid_to_chat_id(&u.jid.to_string()))
                {
                    chat.fixed = u.action.pinned.unwrap_or(false);
                    let _ = tx.send(BackendEvent::ChatUpdated(chat.clone()));
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
                let mut state = state.write().await;
                if let Some(chat) = state
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
                let cache_path = cache_path.clone();
                let tx_for_enrich = tx.clone();
                tokio::spawn(async move {
                    enrich_chat_names(&client, &state_for_enrich, &cache_path, &tx_for_enrich)
                        .await;
                });
                let st = state.read().await;
                let snapshot = sorted_chat_snapshot(&st);
                drop(st);
                let _ = tx.send(BackendEvent::ChatList(snapshot));
            }
            Event::Presence(u) => {
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
                // Surface the learned status in the open chat and sidebar; only
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

    fn current_client(&self) -> Arc<Client> {
        self.client.clone()
    }

    async fn check_logged_in(&self) -> Result<(), BackendError> {
        if !self.current_client().is_logged_in() {
            return Err(BackendError::NotAuthenticated);
        }
        Ok(())
    }

    /// Start the bot (once) if it has not been started yet, and return whether
    /// this call performed the start. Starting is idempotent: the first caller
    /// spawns the bot and its failure-watcher; later calls are no-ops.
    pub fn start(&self) {
        if self.shutdown.load(Ordering::SeqCst) {
            return;
        }

        let bot = {
            match self.bot.lock().unwrap().take() {
                Some(bot) => bot,
                None => return,
            }
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

        *self.run_task.lock().unwrap() = Some(run_task);
    }
}

impl WhatsAppState {
    /// Restore a previously persisted account snapshot (chats, history,
    /// pushnames) so a cold restart does not start with an empty chat list or
    /// empty history. Missing/corrupt cache falls back to a fresh state.
    fn load_from(cache_path: &str) -> Self {
        match std::fs::read_to_string(cache_path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Persist the account snapshot to the JSON cache. Best-effort:
    /// a failure only logs (the next successful save retries) and never aborts ingestion.
    fn save_to(&self, cache_path: &Path) {
        let raw = match serde_json::to_string_pretty(self) {
            Ok(raw) => raw,
            Err(e) => {
                warn!("Failed to serialize WA state: {e}");
                return;
            }
        };

        if let Some(parent) = std::path::Path::new(cache_path).parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            warn!("Failed to create WA cache dir: {e}");
            return;
        }

        if let Err(e) = std::fs::write(cache_path, raw) {
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
    fn learn_own_identity(&mut self, client: &Client) {
        self.own_lid = client.lid().map(|j| j.to_non_ad_string());
        self.own_pn = client.pn().map(|j| j.to_non_ad_string());
        self.name_self_chats();
    }

    /// Rename any cached row that addresses the account's own LID or phone
    /// number to "Myself". WhatsApp keeps the self-thread local on the phone
    /// (it may never arrive via HistorySync), so rows seeded from the cache
    /// need this sweep to converge.
    fn name_self_chats(&mut self) {
        for chat in self.chats.iter_mut() {
            if is_self_chat(&chat.id, self.own_lid.as_deref(), self.own_pn.as_deref()) {
                chat.contact_name = "Myself".to_string();
                chat.verified = false;
            }
        }
    }

    /// Insert or update the sidebar entry for `chat` from a freshly normalized
    /// message, bumping preview and unread state. Used by the live
    /// `Event::Messages` path.
    fn upsert_chat_from_message(&mut self, chat: ChatId, msg: &Message) {
        let preview = preview_line(&msg.sender, &msg.text, msg.from_me);

        match self.chats.iter_mut().find(|c| c.id == chat) {
            Some(c) => {
                c.last_message = Some(preview);

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
                        ChatId::WhatsApp(jid) => Jid::from_str(&jid)
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
                    last_message: Some(preview),
                    unread: !msg.from_me,
                    unread_count: if msg.from_me { 0 } else { 1 },
                    ..Default::default()
                });
            }
        }
    }

    /// Upsert the given conversation into the sidebar using history-sync
    /// metadata (name, unread, pinned). Preserves any newer `last_message` set
    /// by live data. Conversations that are not really chats on the phone (see
    /// [`should_skip_conversation`]) are not inserted.
    fn upsert_conversation(&mut self, conv: &wa::Conversation, chat: ChatId) {
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
        let fixed = conv.pinned.unwrap_or(0) > 0;

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
                c.unread_count = unread_count
                    .max(c.unread_count)
                    .max(if c.unread { 1 } else { 0 });
                c.unread = unread_count > 0 || c.unread;
                c.fixed = fixed;
            }
            None => self.chats.push(Chat {
                id: chat,
                contact_name,
                unread: unread_count > 0,
                unread_count,
                fixed,
                verified,
                ..Default::default()
            }),
        }
        debug!(
            "WA upsert_conversation id={:?} name={:?} existed={}",
            &conv.id, name_for_log, exists,
        );
    }

    /// Refresh the `last_message` preview for a chat from its newest stored
    /// history message (used by history sync after ingestion).
    fn refresh_preview(&mut self, chat: &ChatId) {
        let preview = self
            .history
            .get(chat)
            .and_then(|h| h.iter().max_by_key(|m| m.timestamp))
            .map(|lm| preview_line(&lm.sender, &lm.text, lm.from_me));

        if let Some(c) = self.chats.iter_mut().find(|c| c.id == *chat) {
            c.last_message = preview;
        }
    }
}

/// Whether a history-sync `Conversation` record is not actually a chat on the
/// phone and should be excluded from the chat list.
///
/// A row that is flagged on the primary as archived / read-only / etc. is
/// still a real thread; only threads the primary itself no longer presents as
/// chats are dropped here. The "never-started LID" case catches ghost rows the
/// primary syncs without any thread metadata (no first/last timestamp, no
/// peer phone number): they are not chats one opened.
fn should_skip_conversation(conv: &wa::Conversation) -> Option<&'static str> {
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

    // TODO: hide archived chats from the main list once the TUI has an
    // Archived section; until then they stay visible to match the phone.
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
async fn remove_chat(state: &SharedState, jid: &Jid) -> Option<Chat> {
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
/// `ChatRemoved` for the folded row, `ChatUpdated` for the survivor, a sorted
/// `ChatList` republish, and a cache persist — so the running TUI drops the
/// stale row even when a snapshot shrink would otherwise be ignored.
/// Returns the number of rows folded.
async fn merge_lid_duplicates(
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
            if pn_row.last_message.is_none() {
                pn_row.last_message = orphan.last_message;
            }
            pn_row.verified = pn_row.verified || orphan.verified;
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
        st.by_stanza_id
            .retain(|_, (c, _)| *c != lid_chat);
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
            let snapshot = sorted_chat_snapshot(&*state.read().await);
            let _ = tx.send(BackendEvent::ChatList(snapshot));
            let _ = tx.send(BackendEvent::Status("merged duplicate chats".into()));
        }
    }
    merged
}

/// Find the sidebar chat row for a peer JID, trying the wire key and its
/// phone-keyed twin when the key is a LID.
fn find_peer_chat(state: &WhatsAppState, jid: &Jid) -> Option<ChatId> {
    let chat = fold_lid_key(&jid.to_non_ad_string(), state);
    state
        .chats
        .iter()
        .any(|c| c.id == chat)
        .then_some(chat)
}

/// Store/lookup keys for a peer's presence state: the wire JID plus its
/// phone-keyed twin when the peer was reached by LID and the mapping is known.
/// [`Event::Presence`] writes every key and `status()` reads every key, so the
/// two always agree.
fn presence_keys(jid: &Jid, st: &WhatsAppState) -> Vec<String> {
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
fn presence_label(online: bool, last_seen: Option<i64>) -> Option<String> {
    if online {
        Some("online".to_string())
    } else {
        last_seen.map(|ts| format!("last seen {}", relative_time(ts)))
    }
}

/// Sidebar order for WhatsApp chats: pinned first, then most-recently-active.
/// WhatsApp syncs conversation rows without a reliable per-row timestamp, so
/// the newest stored message is the recency signal. `chats()` and every chat
/// list republish go through this so the running TUI never falls back to
/// insertion order.
fn sorted_chat_snapshot(st: &WhatsAppState) -> Vec<Chat> {
    let mut chats = st.chats.clone();
    chats.sort_by_key(|c| {
        let recency = st
            .history
            .get(&c.id)
            .and_then(|h| h.iter().max_by_key(|m| m.timestamp))
            .map(|m| m.timestamp)
            .unwrap_or(0);
        (!c.fixed, std::cmp::Reverse(recency))
    });
    chats
}

/// Learn the phone-number mapping for a LID author outside any state write
/// lock (the client's local LID↔PN lookup awaits sqlite and can deadlock
/// against a held lock). Returns the peer's phone-number user-part once known;
/// non-LID JIDs and already-mapped LIDs return immediately.
async fn learn_lid_pn(client: &Arc<Client>, sender: &Jid, state: &SharedState) -> Option<String> {
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
    state
        .write()
        .await
        .lid_pn
        .insert(bare, pn_user.clone());
    Some(pn_user)
}

/// Relative "x ago" label for an epoch-seconds timestamp (same shape as the
/// Telegram backend's `telegram_relative_time`).
fn relative_time(unix_seconds: i64) -> String {
    let delta = now().saturating_sub(unix_seconds).max(0);
    match delta {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", delta / 60),
        3600..=86399 => format!("{}h ago", delta / 3600),
        _ => format!("{}d ago", delta / 86400),
    }
}

async fn handle_history_sync(
    hs: &HistorySync,
    client: Option<&Arc<Client>>,
    state: &SharedState,
    tx: &Sender<BackendEvent>,
    cache_path: &Path,
) {
    // Canonicalize every conversation key before taking the write lock: the
    // helper awaits the client's local LID↔PN cache and would otherwise
    // deadlock against a held write lock.
    let mut canonical: Vec<ChatId> = Vec::with_capacity(hs.conversations.len());
    for conversation in &hs.conversations {
        canonical.push(
            canonical_chat_id(client, state, ChatId::jid_to_chat_id(&conversation.id.to_string()))
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
            for conversation in &hs.conversations {
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
        hs.conversations.len(),
        hs.pushnames.len(),
    );

    // Index the push names so both conversation and per-message resolutions can
    // reach them. Newer syncs overwrite stale entries keyed by the same JID.
    for pn in hs.pushnames.iter() {
        if let (Some(id), Some(name)) = (pn.id.as_deref(), pn.pushname.as_deref())
            && !name.trim().is_empty()
        {
            state.pushnames.insert(id.to_string(), name.to_string());
        }
    }

    for (conversation, chat) in hs.conversations.iter().zip(canonical.iter()) {
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
                .unwrap_or_else(|| {
                    // Own messages and one-to-one chats are authored by the chat
                    // itself; the per-message participant slot is absent.
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
        state.refresh_preview(&chat);
    }
    let chats_total = state.chats.len();
    let chats_snapshot = sorted_chat_snapshot(&state);
    drop(state);

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
async fn enrich_chat_names(
    client: &Arc<Client>,
    state: &SharedState,
    cache_path: &PathBuf,
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
        // Persist the learned names and immediately republish the dialog list
        // so the running TUI refreshes without waiting for the next history
        // sync (a cold start would otherwise keep showing cached bare JIDs
        // until a sync happens to re-announce the chats).
        let st = state.read().await;
        let chats_snapshot = sorted_chat_snapshot(&st);
        st.save_to(cache_path);
        drop(st);
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

#[async_trait::async_trait]
impl Messenger for WhatsAppMessenger {
    fn platform(&self) -> &'static str {
        "WhatsApp"
    }

    async fn is_authenticated(&self) -> bool {
        self.client.clone().is_logged_in()
    }

    async fn chats(&self) -> Result<Vec<Chat>, BackendError> {
        let state = self.state.read().await;
        debug!("WA chats() returning {} chats", state.chats.len());
        Ok(sorted_chat_snapshot(&state))
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
                .await;
        }

        let mut state = self.state.write().await;
        for c in state.chats.iter_mut() {
            if c.id == *chat || c.id == canonical {
                c.unread = false;
                c.unread_count = 0;
            }
        }
        drop(state);

        let _ = self.tx.send(BackendEvent::UnreadUpdated {
            chat: chat.clone(),
            unread: false,
            unread_count: 0,
        });
        Ok(())
    }

    async fn history(&self, chat: &ChatId) -> Result<Vec<Message>, BackendError> {
        // `history_page` intentionally uses the `Messenger` trait default (full
        // history load): the pinned whatsapp-rust has no before-message
        // pagination cursor, and mapping WhatsApp's opaque message id onto the
        // trait's numeric `Option<i32>` offset would silently drop or duplicate
        // messages. Pagination lands when a reliable opaque cursor exists.
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

    async fn send(
        &self,
        chat: &ChatId,
        text: &str,
        reply_to: Option<MessageId>,
    ) -> Result<Message, BackendError> {
        self.check_logged_in().await?;

        let ChatId::WhatsApp(jid_str) = chat else {
            return Err(BackendError::Other("WhatsApp: invalid chat".into()));
        };

        let jid = Jid::from_str(jid_str)
            .map_err(|e| BackendError::Other(format!("WhatsApp: invalid chat jid: {e}")))?;
        let client = self.current_client();

        let message = match &reply_to {
            Some(id) => wa::Message::text_with_context(
                text.to_string(),
                wa::ContextInfo {
                    stanza_id: Some(id.to_string()),
                    ..Default::default()
                },
            ),
            None => wa::Message::text(text.to_string()),
        };

        let result = client
            .send_message(jid.clone(), message)
            .await
            .map_err(BackendError::from)?;

        let sent = Message {
            message_id: result.message_id.clone().into(),
            chat: chat.clone(),
            sender: "You".into(),
            author_id: None,
            text: text.to_string(),
            timestamp: now(),
            from_me: true,
            msg_actions: message_actions(true),
            media: None,
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

            for c in state.chats.iter_mut() {
                if c.id == *chat {
                    c.last_message = Some(format!("You: {text}"));
                    c.unread = false;
                    c.unread_count = 0;
                }
            }
        }

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

        Ok(())
    }

    async fn login(&mut self) -> Result<(), BackendError> {
        // WhatsApp auth is event-driven (QR / pair code via BackendEvent).
        // The bot is paired over the network; nothing to do here.
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

    async fn logout(&mut self) -> Result<(), BackendError> {
        self.check_logged_in().await?;
        self.current_client().disconnect().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use whatsapp_rust::prelude::MessageField;

    fn pn(num: &str) -> Jid {
        Jid::pn(num)
    }

    fn text_message(text: &str) -> wa::Message {
        wa::Message::text(text.to_string())
    }

    fn msg_info(chat: &Jid, sender: &Jid, push_name: &str, id: &str, from_me: bool) -> MessageInfo {
        let mut info = MessageInfo::default();
        info.id = id.into();
        info.source.chat = chat.clone();
        info.source.sender = sender.clone();
        info.source.is_from_me = from_me;
        info.push_name = push_name.into();
        info.timestamp = chrono::Utc.timestamp_opt(1000, 0).unwrap();
        info
    }

    /// Build a HistorySync message record mirroring the shape the bot decodes.
    fn web_msg(
        chat: &str,
        participant: Option<&str>,
        id: &str,
        ts: u64,
        from_me: bool,
        push_name: &str,
        text: &str,
    ) -> wa::HistorySyncMsg {
        wa::HistorySyncMsg {
            message: MessageField::some(wa::WebMessageInfo {
                key: MessageField::some(wa::MessageKey {
                    id: Some(id.to_string()),
                    remote_jid: Some(chat.to_string()),
                    from_me: Some(from_me),
                    participant: participant.map(str::to_string),
                }),
                message: MessageField::some(wa::Message::text(text.to_string())),
                message_timestamp: Some(ts),
                push_name: Some(push_name.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn conversation(id: &str, messages: Vec<wa::HistorySyncMsg>) -> wa::Conversation {
        wa::Conversation {
            id: id.to_string(),
            messages,
            ..Default::default()
        }
    }

    #[test]
    fn skip_filter_drops_ghost_lid_rows() {
        let ghost = wa::Conversation {
            id: "255202829570287@lid".to_string(),
            ..Default::default()
        };
        assert!(
            should_skip_conversation(&ghost).is_some(),
            "never-started LID row with no pn is a ghost"
        );

        // A real LID chat has thread metadata and/or a peer phone number.
        let real_lid = wa::Conversation {
            id: "255202829570287@lid".to_string(),
            pn_jid: Some("15550000001@s.whatsapp.net".to_string()),
            ..Default::default()
        };
        assert!(should_skip_conversation(&real_lid).is_none());

        let real_pn = wa::Conversation {
            id: "15550000002@s.whatsapp.net".to_string(),
            ..Default::default()
        };
        assert!(should_skip_conversation(&real_pn).is_none());

        // A group the account left (read_only) is not a chat anymore.
        let left_group = wa::Conversation {
            id: "111222333@g.us".to_string(),
            read_only: Some(true),
            ..Default::default()
        };
        assert!(should_skip_conversation(&left_group).is_some());

        // A live joined group is kept.
        let joined_group = wa::Conversation {
            id: "111222333@g.us".to_string(),
            read_only: Some(false),
            ..Default::default()
        };
        assert!(should_skip_conversation(&joined_group).is_none());

        // pnh duplicate / marketing / suspended / parent community are junk.
        for conv in [
            wa::Conversation {
                id: "255202829570287@lid".to_string(),
                pnh_duplicate_lid_thread: Some(true),
                ..Default::default()
            },
            wa::Conversation {
                id: "15550000003@s.whatsapp.net".to_string(),
                is_marketing_message_thread: Some(true),
                ..Default::default()
            },
            wa::Conversation {
                id: "15550000003@s.whatsapp.net".to_string(),
                suspended: Some(true),
                ..Default::default()
            },
        ] {
            assert!(should_skip_conversation(&conv).is_some());
        }
    }

    #[test]
    fn lid_conversation_resolves_name_via_pn_jid() {
        let mut pushnames = HashMap::new();
        pushnames.insert(
            "15550000001@s.whatsapp.net".to_string(),
            "Alice".to_string(),
        );
        let empty = HashMap::new();
        let no_verified = HashSet::new();

        // LID-keyed thread; the contact name must come from the PN pushname.
        let lid = wa::Conversation {
            id: "255202829570287@lid".to_string(),
            pn_jid: Some("15550000001@s.whatsapp.net".to_string()),
            ..Default::default()
        };
        assert_eq!(
            resolve_conversation_name(&lid, &pushnames, &empty, &no_verified),
            ("Alice".to_string(), false),
            "LID chat resolves through pn_jid"
        );

        // PN-keyed thread without a name/display_name falls back to pushname.
        let pn_chat = wa::Conversation {
            id: "15550000001@s.whatsapp.net".to_string(),
            ..Default::default()
        };
        assert_eq!(
            resolve_conversation_name(&pn_chat, &pushnames, &empty, &no_verified),
            ("Alice".to_string(), false)
        );

        // A LID thread with no PN, no pushname and no usync name now falls
        // back to the formatted phone number when usync supplied it.
        let mut usync = HashMap::new();
        usync.insert(
            "15550000004@lid".to_string(),
            "+15 55 0000-0004".to_string(),
        );
        let lid_learned = wa::Conversation {
            id: "15550000004@lid".to_string(),
            ..Default::default()
        };
        assert_eq!(
            resolve_conversation_name(&lid_learned, &pushnames, &usync, &no_verified),
            ("+15 55 0000-0004".to_string(), false)
        );

        // A verified business peer: name resolves from usync with verified=true.
        usync.insert(
            "15550000003@s.whatsapp.net".to_string(),
            "ACME Bots".to_string(),
        );
        let mut verified = HashSet::new();
        verified.insert("15550000003@s.whatsapp.net".to_string());
        let biz = wa::Conversation {
            id: "15550000005@lid".to_string(),
            pn_jid: Some("15550000003@s.whatsapp.net".to_string()),
            ..Default::default()
        };
        assert_eq!(
            resolve_conversation_name(&biz, &pushnames, &usync, &verified),
            ("ACME Bots".to_string(), true),
            "usync entry found via pn_jid overrides the raw LID"
        );

        // A LID thread with no PN, no pushname, no usync name and no phone
        // fallback keeps the bare LID.
        let bare = wa::Conversation {
            id: "255202829570287@lid".to_string(),
            ..Default::default()
        };
        assert_eq!(
            resolve_conversation_name(&bare, &pushnames, &empty, &no_verified),
            ("255202829570287".to_string(), false)
        );
    }

    #[test]
    fn format_pn_matches_whatsapp_layout() {
        assert_eq!(format_pn("15550000003"), "+15 55 000-0003");
        assert_eq!(format_pn("15551234567"), "+15 55 123-4567");
        assert_eq!(format_pn("123"), "+123");
        assert_eq!(format_pn("15550000004@lid"), "+15 55 000-0004");
    }

    #[test]
    fn name_from_lid_pn_formats_numbers_for_lid_keyed_chats() {
        // Fake LID↔PN mappings learned locally from message traffic, with no
        // push name cached: the formatted number must be the fallback so a
        // LID-keyed chat never shows as a bare LID.
        let empty = HashMap::new();
        let cases = [
            ("15550000001", "+15 55 000-0001"),
            ("15550000002", "+15 55 000-0002"),
            ("15550000003", "+15 55 000-0003"),
            ("15550000004", "+15 55 000-0004"),
        ];
        for (pn, expected) in cases {
            assert_eq!(name_from_lid_pn(pn, &empty), expected, "for {pn}");
        }
    }

    #[test]
    fn name_from_lid_pn_prefers_synced_pushname() {
        let mut pushnames = HashMap::new();
        pushnames.insert(
            "15550000002@s.whatsapp.net".to_string(),
            "Test Pushname".to_string(),
        );
        assert_eq!(name_from_lid_pn("15550000002", &pushnames), "Test Pushname");
    }

    #[test]
    fn upsert_keeps_known_group_title_when_sync_carries_raw_jid() {
        let mut st = WhatsAppState::default();
        let group = "15550000001-111222333@g.us";
        let chat_id = ChatId::jid_to_chat_id(group);

        // Full sync record with the real subject.
        let titled = wa::Conversation {
            id: group.to_string(),
            name: Some("Teste".to_string()),
            ..Default::default()
        };

        st.upsert_conversation(&titled, chat_id.clone());
        assert_eq!(st.chats[0].contact_name, "Teste");

        // Periodic sync record whose subject is the raw JID (empty on the
        // phone): must not clobber the known title.
        let raw = wa::Conversation {
            id: group.to_string(),
            name: Some(group.to_string()),
            ..Default::default()
        };
        st.upsert_conversation(&raw, chat_id.clone());
        assert_eq!(
            st.chats[0].contact_name, "Teste",
            "a raw-JID subject must not downgrade the group title"
        );

        // A genuinely new group without a subject renders its JID until a
        // real title arrives.
        let new_group = "111222333444@g.us";
        let fresh = wa::Conversation {
            id: new_group.to_string(),
            name: Some(new_group.to_string()),
            ..Default::default()
        };
        st.upsert_conversation(&fresh, ChatId::jid_to_chat_id(new_group));
        assert_eq!(st.chats[1].contact_name, new_group);
    }

    #[test]
    fn upsert_self_chat_is_named_myself() {
        let mut st = WhatsAppState {
            own_lid: Some("123456789012345@lid".to_string()),
            own_pn: Some("15551234567@s.whatsapp.net".to_string()),
            ..Default::default()
        };

        // Self-chat keyed by the own phone number, even with a pushname that
        // would otherwise resolve.
        let conv = wa::Conversation {
            id: "15551234567@s.whatsapp.net".to_string(),
            name: Some("Test".to_string()),
            ..Default::default()
        };
        st.upsert_conversation(&conv, ChatId::jid_to_chat_id("15551234567@s.whatsapp.net"));
        assert_eq!(st.chats[0].contact_name, "Myself");
        assert!(!st.chats[0].verified);

        // Self-chat keyed by the own LID (no phone twin yet) is Myself too.
        let lid_conv = wa::Conversation {
            id: "123456789012345@lid".to_string(),
            conversation_timestamp: Some(1),
            ..Default::default()
        };
        st.upsert_conversation(&lid_conv, ChatId::jid_to_chat_id("123456789012345@lid"));
        assert_eq!(st.chats[1].contact_name, "Myself");

        // A peer chat is untouched by the self-chat rule.
        let peer = wa::Conversation {
            id: "15559998877@s.whatsapp.net".to_string(),
            name: Some("Alice".to_string()),
            ..Default::default()
        };
        st.upsert_conversation(&peer, ChatId::jid_to_chat_id("15559998877@s.whatsapp.net"));
        assert_eq!(st.chats[2].contact_name, "Alice");
    }

    #[test]
    fn upsert_from_message_names_self_chat_myself() {
        let msg = Message {
            message_id: "s-1".into(),
            chat: ChatId::jid_to_chat_id("15551234567@s.whatsapp.net"),
            sender: "You".into(),
            text: "hello myself".into(),
            timestamp: 1,
            from_me: true,
            author_id: None,
            media: None,
            msg_actions: Vec::new(),
            reply_to_id: None,
            reply_ctx: None,
            pending: false,
            failed: false,
        };

        let mut st = WhatsAppState {
            own_pn: Some("15551234567@s.whatsapp.net".to_string()),
            ..Default::default()
        };
        st.upsert_chat_from_message(ChatId::jid_to_chat_id("15551234567@s.whatsapp.net"), &msg);
        assert_eq!(st.chats[0].contact_name, "Myself");
        assert_eq!(st.chats[0].unread_count, 0);

        // A from-me message to a peer row still uses "You".
        let mut st2 = WhatsAppState {
            own_pn: Some("15551234567@s.whatsapp.net".to_string()),
            ..Default::default()
        };
        st2.upsert_chat_from_message(ChatId::jid_to_chat_id("15559998877@s.whatsapp.net"), &msg);
        assert_eq!(st2.chats[0].contact_name, "You");

        // A self row that already exists (seeded from a previous cache with the
        // formatted own number) is renamed by an arriving self-message.
        let mut st3 = WhatsAppState {
            own_pn: Some("15551234567@s.whatsapp.net".to_string()),
            chats: vec![Chat {
                id: ChatId::WhatsApp("15551234567@s.whatsapp.net".to_string()),
                contact_name: "+15 55 123-4567".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        st3.upsert_chat_from_message(ChatId::jid_to_chat_id("15551234567@s.whatsapp.net"), &msg);
        assert_eq!(st3.chats[0].contact_name, "Myself");
        assert!(!st3.chats[0].verified);
    }

    #[test]
    fn name_self_chats_renames_cached_self_rows() {
        let mut st = WhatsAppState {
            own_lid: Some("123456789012345@lid".to_string()),
            own_pn: Some("15551234567@s.whatsapp.net".to_string()),
            chats: vec![
                Chat {
                    id: ChatId::WhatsApp("15551234567@s.whatsapp.net".to_string()),
                    contact_name: "+15 55 123-4567".to_string(),
                    verified: true,
                    ..Default::default()
                },
                Chat {
                    id: ChatId::WhatsApp("123456789012345@lid".to_string()),
                    contact_name: "123456789012345".to_string(),
                    ..Default::default()
                },
                Chat {
                    id: ChatId::WhatsApp("15559998877@s.whatsapp.net".to_string()),
                    contact_name: "Alice".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        st.name_self_chats();

        assert_eq!(st.chats[0].contact_name, "Myself");
        assert!(!st.chats[0].verified);
        assert_eq!(st.chats[1].contact_name, "Myself");
        assert_eq!(st.chats[2].contact_name, "Alice");
    }

    #[test]
    fn chat_id_drops_device_qualifier() {
        let jids = [
            Jid::from_str("15551234567@s.whatsapp.net").unwrap(),
            pn("15551234567").with_device(7),
            pn("15551234567"),
        ];
        let expected = ChatId::WhatsApp("15551234567@s.whatsapp.net".to_string());
        for jid in &jids {
            assert_eq!(
                ChatId::jid_to_chat_id(&jid.to_string()),
                expected,
                "for {jid}"
            );
        }
    }

    #[test]
    fn chat_id_from_str_normalizes_group() {
        let id = ChatId::jid_to_chat_id("123456789@g.us");
        assert_eq!(id, ChatId::WhatsApp("123456789@g.us".to_string()));
        // A non-parseable string is preserved rather than dropped.
        assert_eq!(
            ChatId::jid_to_chat_id("garbage-not-a-jid"),
            ChatId::WhatsApp("garbage-not-a-jid".to_string())
        );
    }

    #[test]
    fn sender_uses_push_name_then_cache_then_jid() {
        let sender = pn("15550000001");
        let mut pushnames = HashMap::new();
        pushnames.insert(
            "15550000001@s.whatsapp.net".to_string(),
            "Cached Name".to_string(),
        );
        let no_lid_pn = HashMap::new();

        // 1. Message push name wins.
        assert_eq!(
            resolve_sender_name("Alice", &sender, &pushnames, &no_lid_pn),
            "Alice"
        );
        // 2. Falls back to the push-name cache.
        assert_eq!(
            resolve_sender_name("", &sender, &pushnames, &no_lid_pn),
            "Cached Name"
        );
        // 3. Falls back to the bare JID user part.
        let empty = HashMap::new();
        assert_eq!(
            resolve_sender_name("", &sender, &empty, &no_lid_pn),
            "15550000001"
        );
    }

    #[test]
    fn sender_resolves_lid_author_via_lid_pn_mapping() {
        let sender = Jid::lid("255202829570287");
        let mut pushnames = HashMap::new();
        pushnames.insert(
            "15550000007@s.whatsapp.net".to_string(),
            "Test Sender".to_string(),
        );
        let mut lid_pn = HashMap::new();
        lid_pn.insert("255202829570287".to_string(), "15550000007".to_string());

        // Cached push name for the peer's phone number wins over the bare LID.
        assert_eq!(
            resolve_sender_name("", &sender, &pushnames, &lid_pn),
            "Test Sender"
        );

        // No cached name: formatted number, never a bare LID.
        let empty = HashMap::new();
        assert_eq!(
            resolve_sender_name("", &sender, &empty, &lid_pn),
            "+15 55 000-0007"
        );

        // Unknown mapping: falls back to the bare LID user part.
        let no_lid_pn = HashMap::new();
        assert_eq!(
            resolve_sender_name("", &sender, &empty, &no_lid_pn),
            "255202829570287"
        );
    }

    #[test]
    fn sorted_chat_snapshot_orders_pinned_then_recency() {
        let old = Chat {
            id: ChatId::WhatsApp("15550000001@s.whatsapp.net".to_string()),
            contact_name: "Old chat".into(),
            fixed: false,
            ..Default::default()
        };
        let fresh = Chat {
            id: ChatId::WhatsApp("15550000002@s.whatsapp.net".to_string()),
            contact_name: "Fresh chat".into(),
            fixed: false,
            ..Default::default()
        };
        let pinned = Chat {
            id: ChatId::WhatsApp("15550000003@s.whatsapp.net".to_string()),
            contact_name: "Pinned chat".into(),
            fixed: true,
            ..Default::default()
        };
        let mut st = WhatsAppState {
            chats: vec![old.clone(), fresh.clone(), pinned.clone()],
            ..Default::default()
        };
        let msg = |m: &str, ts: i64, chat: &ChatId| Message {
            message_id: format!("m-{m}").into(),
            chat: chat.clone(),
            sender: "x".into(),
            text: m.into(),
            timestamp: ts,
            from_me: false,
            author_id: None,
            media: None,
            msg_actions: Vec::new(),
            reply_to_id: None,
            reply_ctx: None,
            pending: false,
            failed: false,
        };
        st.history.insert(
            old.id.clone(),
            vec![msg("old", 1000, &old.id)],
        );
        st.history.insert(
            fresh.id.clone(),
            vec![msg("fresh", 3000, &fresh.id)],
        );
        let order: Vec<_> = sorted_chat_snapshot(&st)
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(
            order,
            vec![
                ChatId::WhatsApp("15550000003@s.whatsapp.net".to_string()),
                ChatId::WhatsApp("15550000002@s.whatsapp.net".to_string()),
                ChatId::WhatsApp("15550000001@s.whatsapp.net".to_string()),
            ]
        );
    }

    #[test]
    fn normalize_inbound_detects_from_me_and_unknown() {
        let state = WhatsAppState::default();
        let chat = pn("15551234567");
        let info = msg_info(&chat, &chat, "Alice", "stanza-1", false);
        let msg = to_senders_msg(
            ChatId::jid_to_chat_id(&chat.to_string()),
            &info,
            &text_message("hello"),
            &state,
        );
        assert_eq!(msg.sender, "Alice");
        assert!(!msg.from_me);
        assert_eq!(
            msg.chat,
            ChatId::WhatsApp("15551234567@s.whatsapp.net".to_string())
        );
        assert!(msg.msg_actions.contains(&MessageAction::Reply));
        assert!(!msg.msg_actions.contains(&MessageAction::Edit));

        let mine = msg_info(&chat, &chat, "", "stanza-2", true);
        let m = to_senders_msg(
            ChatId::jid_to_chat_id(&chat.to_string()),
            &mine,
            &text_message("hi"),
            &state,
        );
        assert_eq!(m.sender, "You");
        assert!(m.from_me);
        assert!(m.msg_actions.contains(&MessageAction::Edit));
        assert!(m.msg_actions.contains(&MessageAction::Delete));
    }

    #[test]
    fn own_chat_id_rewrites_self_lid_messages_to_own_pn() {
        let own_pn = Jid::pn("15551234567");
        let own_lid = Jid::lid("123456789012345");

        // Self message keyed by our own LID -> own PN chat.
        let lid_chat = ChatId::WhatsApp("123456789012345@lid".to_string());
        assert_eq!(
            own_chat_id(Some(&own_lid), Some(&own_pn), lid_chat.clone(), true),
            ChatId::WhatsApp("15551234567@s.whatsapp.net".to_string())
        );

        // A same-JID chat addressed to someone else (not from_me) is untouched.
        let other_lid_chat = ChatId::WhatsApp("777777777777777@lid".to_string());
        assert_eq!(
            own_chat_id(Some(&own_lid), Some(&own_pn), other_lid_chat.clone(), false),
            other_lid_chat
        );

        // From-me message in a personal chat is left alone.
        let dm_chat = ChatId::WhatsApp("15559998877@s.whatsapp.net".to_string());
        assert_eq!(
            own_chat_id(Some(&own_lid), Some(&own_pn), dm_chat.clone(), true),
            dm_chat
        );

        // Missing own credentials fall back to the original chat.
        assert_eq!(own_chat_id(None, None, lid_chat.clone(), true), lid_chat);
    }

    #[test]
    fn whatsapp_state_round_trips_through_json() {
        let mut state = WhatsAppState::default();
        state.chats.push(Chat {
            id: ChatId::WhatsApp("15551234567@s.whatsapp.net".to_string()),
            contact_name: "Self".to_string(),
            ..Default::default()
        });
        state.history.insert(
            ChatId::WhatsApp("15551234567@s.whatsapp.net".to_string()),
            vec![to_senders_msg(
                ChatId::jid_to_chat_id(&pn("15551234567").to_string()),
                &msg_info(&pn("15551234567"), &pn("15551234567"), "Self", "s-1", true),
                &text_message("hello"),
                &state,
            )],
        );
        state.pushnames.insert(
            "15559998877@s.whatsapp.net".to_string(),
            "Alice".to_string(),
        );

        let encoded = serde_json::to_string(&state).unwrap();
        let restored: WhatsAppState = serde_json::from_str(&encoded).unwrap();
        let encoded_again = serde_json::to_string(&restored).unwrap();

        assert_eq!(encoded_again, encoded);
        assert_eq!(restored.chats.len(), 1);
        assert_eq!(restored.history.len(), 1);
        assert_eq!(
            restored
                .pushnames
                .get("15559998877@s.whatsapp.net")
                .unwrap(),
            "Alice"
        );
        assert_eq!(
            restored
                .history
                .get(&ChatId::WhatsApp("15551234567@s.whatsapp.net".to_string()))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn whatsapp_state_load_falls_back_on_tui_chats_schema() {
        // `chats.json` historically held a bare Vec<Chat> (the TUI's
        // persistence schema), which is incompatible with WhatsAppState.
        // Loading it must never panic; it degrades to a fresh state so a cold
        // restart can rebuild from sync + usync instead of churning on a
        // mismatched file.
        let path = std::env::temp_dir().join("senders_wa_tui_schema_test.json");
        let foreign = serde_json::to_string(&[Chat {
            id: ChatId::WhatsApp("15550000006@s.whatsapp.net".to_string()),
            contact_name: "Test User".to_string(),
            ..Default::default()
        }])
        .unwrap();
        std::fs::write(&path, foreign).unwrap();

        let state = WhatsAppState::load_from(path.to_string_lossy().as_ref());
        std::fs::remove_file(&path).ok();

        assert!(state.chats.is_empty());
        assert!(state.pushnames.is_empty());
    }

    #[tokio::test]
    async fn history_sync_ingests_chats_oldest_to_newest_and_resolves_group_senders() {
        let (tx, _rx) = broadcast::channel(128);
        let state: SharedState = Arc::new(RwLock::new(WhatsAppState::default()));

        // One-to-one chat with a saved name in `display_name` (name empty).
        let dm_messages = vec![
            web_msg(
                "15550000002@s.whatsapp.net",
                None,
                "dm-1",
                1000,
                false,
                "DmName",
                "first",
            ),
            web_msg(
                "15550000002@s.whatsapp.net",
                None,
                "dm-2",
                1001,
                true,
                "own",
                "reply",
            ),
        ];
        let dm = wa::Conversation {
            id: "15550000002@s.whatsapp.net".to_string(),
            name: None,
            display_name: Some("Dm Contact".to_string()),
            messages: dm_messages,
            pinned: Some(1),
            unread_count: Some(3),
            ..Default::default()
        };

        // Group chat: subject name, sender resolved from participant push name.
        let group_messages = vec![
            web_msg(
                "111222333@g.us",
                Some("15550000003@s.whatsapp.net"),
                "g-1",
                2000,
                false,
                "Bob",
                "hey group",
            ),
            web_msg("111222333@g.us", None, "g-2", 2001, true, "own", "yes"),
        ];
        let group_conv = wa::Conversation {
            id: "111222333@g.us".to_string(),
            name: Some("My Group".to_string()),
            messages: group_messages,
            ..Default::default()
        };

        let pushnames = vec![wa::Pushname {
            id: Some("15550000003@s.whatsapp.net".to_string()),
            pushname: Some("Bob".to_string()),
        }];

        let hs = wa::HistorySync {
            conversations: vec![dm, group_conv],
            pushnames,
            ..Default::default()
        };

        super::handle_history_sync(&hs, None, &state, &tx, Path::new("/tmp/wa_test_cache.json"))
            .await;
        let s = state.read().await;

        assert_eq!(s.chats.len(), 2);

        let dm_chat = s
            .chats
            .iter()
            .find(|c| c.id == ChatId::WhatsApp("15550000002@s.whatsapp.net".to_string()))
            .unwrap();

        assert_eq!(dm_chat.contact_name, "Dm Contact");
        assert!(dm_chat.fixed, "pinned conversation should be marked fixed");
        assert_eq!(dm_chat.unread_count, 3);
        assert!(dm_chat.unread);
        // Preview from the newest message.
        assert_eq!(dm_chat.last_message.as_deref(), Some("You: reply"));

        let group_chat = s
            .chats
            .iter()
            .find(|c| c.id == ChatId::WhatsApp("111222333@g.us".to_string()))
            .unwrap();
        assert_eq!(group_chat.contact_name, "My Group");

        // History oldest-to-newest (dm-1 before dm-2).
        let dm_history = s
            .history
            .get(&ChatId::WhatsApp("15550000002@s.whatsapp.net".to_string()))
            .unwrap();
        let ids: Vec<_> = dm_history.iter().map(|m| m.message_id.0.clone()).collect();
        assert_eq!(ids, vec!["dm-1".to_string(), "dm-2".to_string()]);

        // Group message sender resolved to the participant's name, not the group title.
        let group_history = s
            .history
            .get(&ChatId::WhatsApp("111222333@g.us".to_string()))
            .unwrap();
        let bob = group_history.iter().find(|m| !m.from_me).unwrap();
        assert_eq!(bob.sender, "Bob");
        assert_ne!(bob.sender, "My Group");
    }

    #[tokio::test]
    async fn history_sync_deduplicates_and_skips_empty_messages() {
        let (tx, _rx) = broadcast::channel(128);
        let state: SharedState = Arc::new(RwLock::new(WhatsAppState::default()));
        let chat_id = "15550000009@s.whatsapp.net";

        // Two copies of the same stanza + one empty protocol record.
        let messages = vec![
            web_msg(chat_id, None, "dup-1", 1000, false, "Alice", "hello"),
            web_msg(chat_id, None, "dup-1", 1000, false, "Alice", "hello"),
            wa::HistorySyncMsg::default(),
        ];
        let hs = wa::HistorySync {
            conversations: vec![conversation(chat_id, messages)],
            pushnames: vec![],
            ..Default::default()
        };

        super::handle_history_sync(&hs, None, &state, &tx, Path::new("/tmp/wa_test_cache.json"))
            .await;
        let s = state.read().await;
        let history = s
            .history
            .get(&ChatId::WhatsApp(chat_id.to_string()))
            .unwrap();
        assert_eq!(
            history.len(),
            1,
            "duplicate + empty must collapse to one message"
        );
    }

    #[test]
    fn fold_lid_key_rewrites_known_lid_when_pn_row_exists() {
        let pn_chat = ChatId::WhatsApp("15550000007@s.whatsapp.net".to_string());

        let mut folded = WhatsAppState::default();
        folded.chats.push(Chat {
            id: pn_chat.clone(),
            ..Default::default()
        });
        folded
            .lid_pn
            .insert("255202829570287".to_string(), "15550000007".to_string());

        // Mapped + row exists: folded to the phone-keyed row.
        assert_eq!(fold_lid_key("255202829570287@lid", &folded), pn_chat);
        // A plain phone-keyed chat is untouched.
        assert_eq!(fold_lid_key("15550000007@s.whatsapp.net", &folded), pn_chat);

        // LID with no mapping stays put.
        assert_eq!(
            fold_lid_key("777777777777777@lid", &folded),
            ChatId::WhatsApp("777777777777777@lid".to_string())
        );

        // Mapped but no phone-keyed row on the list stays put (the peer was
        // never addressed by phone, so the LID row is what the user opens).
        let mut no_row = WhatsAppState::default();
        no_row
            .lid_pn
            .insert("255202829570287".to_string(), "15550000007".to_string());
        assert_eq!(
            fold_lid_key("255202829570287@lid", &no_row),
            ChatId::WhatsApp("255202829570287@lid".to_string())
        );

        // Unparseable keys are never rewritten.
        assert_eq!(
            fold_lid_key("not-a-jid", &folded),
            ChatId::WhatsApp("not-a-jid".to_string())
        );
    }

    #[test]
    fn presence_label_formats_online_and_last_seen() {
        assert_eq!(presence_label(true, None), Some("online".to_string()));
        assert_eq!(
            presence_label(true, Some(1_800_000_000)),
            Some("online".to_string())
        );
        assert_eq!(presence_label(false, None), None);
        assert_eq!(
            presence_label(false, Some(now() - 300)),
            Some("last seen 5m ago".to_string())
        );
        assert_eq!(
            presence_label(false, Some(now() - 7200)),
            Some("last seen 2h ago".to_string())
        );
        assert_eq!(
            presence_label(false, Some(now() - 172800)),
            Some("last seen 2d ago".to_string())
        );
    }
}
