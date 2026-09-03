use crate::helpers::now;

use super::{
    BackendError, BackendEvent, Chat, ChatId, MediaKind, Message, MessageAction, MessageId,
    MessageMedia, Messenger, ReplyContext,
};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{RwLock, broadcast};
use tracing::{debug, info, warn};
use whatsapp_rust::prelude::{
    Bot, Event, Jid, MessageBuilderExt, MessageExt, MessageInfo, Server, SqliteStore, wa,
};

type SharedState = Arc<RwLock<WhatsAppState>>;

/// In-memory mirror of the account's conversations and messages, populated from
/// `Event::HistorySync` and `Event::Messages` delivered by the whatsapp-rust
/// bot. SQLite persists the session/auth credentials; this cache drives the TUI.
#[derive(Default)]
struct WhatsAppState {
    chats: Vec<Chat>,
    history: HashMap<ChatId, Vec<Message>>,
    /// Stanza is a XML-like structure exchanged between client and server.
    /// Each Stanza contains data that identifies and populates the message
    /// So basically, Stanza == Message
    by_stanza_id: HashMap<String, (ChatId, MessageId)>,
}

fn message_actions(from_me: bool) -> Vec<MessageAction> {
    let mut actions = vec![MessageAction::Reply];
    if from_me {
        actions.extend([MessageAction::Edit, MessageAction::Delete]);
    }
    actions
}

fn chat_id_from_jid(jid: &Jid) -> ChatId {
    ChatId::WhatsApp(jid.to_string())
}

fn wa_message_media(msg: &wa::Message) -> Option<MessageMedia> {
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

/// Build a normalized [`Message`] from a proto and its [`MessageInfo`].
fn normalize_inbound(info: &MessageInfo, msg: &wa::Message, state: &WhatsAppState) -> Message {
    let chat = chat_id_from_jid(&info.source.chat);
    let from_me = info.source.is_from_me;
    let sender = if from_me {
        "You".to_string()
    } else if !info.push_name.trim().is_empty() {
        info.push_name.clone()
    } else {
        info.source.sender.to_string()
    };

    let media = wa_message_media(msg);
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
    client: Arc<whatsapp_rust::Client>,
    /// The configured-but-not-yet-started bot. The bot is intentionally not
    /// spawned in `new()`: it is spun up lazily on the first [`Self::subscribe`]
    /// so the TUI (which subscribes before reading the event channel) can never
    /// miss the initial `Connected` / `QrCode` events, which a `broadcast`
    /// channel would otherwise drop for not-yet-registered receivers.
    bot: Arc<Mutex<Option<Bot>>>,
    run_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    tx: broadcast::Sender<BackendEvent>,
    state: SharedState,
    shutdown: Arc<AtomicBool>,
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
        let state: SharedState = Arc::new(RwLock::new(WhatsAppState::default()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let builder = Bot::builder()
            .with_backend(store)
            .on_connected({
                let tx = tx.clone();
                move |_client| {
                    let tx = tx.clone();
                    async move {
                        info!("WhatsApp connected");
                        let _ = tx.send(BackendEvent::Connected);
                    }
                }
            })
            .on_qr_code({
                let tx = tx.clone();
                move |code, timeout| {
                    let tx = tx.clone();
                    async move {
                        info!("WhatsApp QR code (valid {}s)", timeout.as_secs());
                        let _ = tx.send(BackendEvent::QrCode(code));
                    }
                }
            })
            .on_logged_out({
                let tx = tx.clone();
                move |_info| {
                    let tx = tx.clone();
                    async move {
                        warn!("WhatsApp logged out");
                        let _ = tx.send(BackendEvent::Disconnected(
                            "WhatsApp logged out".to_string(),
                        ));
                    }
                }
            })
            .on_event({
                let state = state.clone();
                let tx = tx.clone();
                move |event: Arc<Event>, _client: Arc<whatsapp_rust::Client>| {
                    let state = state.clone();
                    let tx = tx.clone();
                    async move {
                        let ev = event;
                        match &*ev {
                            Event::Connected(_) => {
                                let _ = tx.send(BackendEvent::Connected);
                            }
                            Event::Disconnected(_)
                            | Event::StreamError(_)
                            | Event::ConnectFailure(_) => {
                                let _ = tx.send(BackendEvent::Disconnected(
                                    "WhatsApp: Connection Failure!".into(),
                                ));
                            }
                            Event::PairingQrCode(q) => {
                                let _ = tx.send(BackendEvent::QrCode(q.code.clone()));
                            }
                            Event::Messages(batch) => {
                                for im in batch.messages.iter() {
                                    let mut state = state.write().await;
                                    let chat = chat_id_from_jid(&im.info.source.chat);
                                    let msg = normalize_inbound(&im.info, &im.message, &state);

                                    if msg.text.is_empty() && msg.media.is_none() {
                                        continue;
                                    }

                                    let stanza_id = im.info.id.to_string();

                                    state.by_stanza_id.insert(
                                        stanza_id.clone(),
                                        (chat.clone(), msg.message_id.clone()),
                                    );

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

                                    let contact_name = if im.info.push_name.trim().is_empty() {
                                        im.info.source.sender.to_string()
                                    } else {
                                        im.info.push_name.clone()
                                    };

                                    let preview = if msg.from_me {
                                        format!("You: {}", msg.text)
                                    } else {
                                        format!("{}: {}", contact_name, msg.text)
                                    };

                                    match state.chats.iter_mut().find(|c| c.id == chat) {
                                        Some(c) => {
                                            c.last_message = Some(preview);
                                            if !msg.from_me {
                                                c.unread = true;
                                                c.unread_count += 1;
                                            }
                                        }
                                        None => state.chats.push(Chat {
                                            id: chat.clone(),
                                            contact_name: contact_name.clone(),
                                            last_message: Some(preview),
                                            unread: !msg.from_me,
                                            unread_count: if msg.from_me { 0 } else { 1 },
                                            ..Default::default()
                                        }),
                                    }
                                    drop(state);
                                    let _ = tx.send(BackendEvent::MessageReceived(msg));
                                }
                            }
                            Event::HistorySync(hs) => {
                                if let Some(parsed) = hs.get() {
                                    handle_history_sync(parsed, &state, &tx).await;
                                }
                            }
                            other => {
                                debug!(
                                    "Unhandled WhatsApp event: {:?}",
                                    std::mem::discriminant(other)
                                );
                            }
                        }
                    }
                }
            });

        let bot = builder
            .build()
            .await
            .map_err(|e| BackendError::Other(format!("WhatsApp: failed to build bot: {e}")))?;

        let client = bot.client();

        Ok(Self {
            client,
            bot: Arc::new(Mutex::new(Some(bot))),
            run_task: Arc::new(Mutex::new(None)),
            tx,
            state,
            shutdown,
        })
    }

    fn current_client(&self) -> Arc<whatsapp_rust::Client> {
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
    fn start_bot(&self) -> bool {
        let bot = {
            match self.bot.lock().unwrap().take() {
                Some(bot) => bot,
                None => return false,
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
        true
    }
}

async fn handle_history_sync(
    hs: &wa::HistorySync,
    state: &SharedState,
    tx: &broadcast::Sender<BackendEvent>,
) {
    let mut state = state.write().await;

    for conversation in hs.conversations.iter() {
        let chat = ChatId::WhatsApp(conversation.id.clone());
        let contact_name = conversation
            .name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| conversation.id.clone());
        let unread_count = conversation.unread_count.unwrap_or(0) as i32;

        match state.chats.iter_mut().find(|c| c.id == chat) {
            Some(c) => {
                c.contact_name = contact_name.clone();
                c.unread_count = unread_count;
                c.unread = unread_count > 0;
            }
            None => state.chats.push(Chat {
                id: chat.clone(),
                contact_name: contact_name.clone(),
                unread: unread_count > 0,
                unread_count,
                ..Default::default()
            }),
        }

        let mut new_messages: Vec<(String, Message)> = Vec::new();

        for history in conversation.messages.iter() {
            let Some(web) = history.message.as_option() else {
                continue;
            };

            let Some(stanza_id) = web.key.as_option().and_then(|k| k.id.clone()) else {
                continue;
            };

            let Some(msg) = web.message.as_option() else {
                continue;
            };

            let already = state
                .history
                .get(&chat)
                .map(|h| h.iter().any(|m| &*m.message_id == stanza_id.as_str()))
                .unwrap_or(false);

            if already {
                continue;
            }

            let from_me = web
                .key
                .as_option()
                .map(|k| k.from_me.unwrap_or(false))
                .unwrap_or(false);

            let info = history_info(&conversation.id, from_me, conversation.name.as_ref());

            let mut normalized = normalize_inbound(&info, msg, &state);
            normalized.timestamp = web
                .message_timestamp
                .map(|ts| ts as i64)
                .unwrap_or_else(now);

            if normalized.text.is_empty() && normalized.media.is_none() {
                continue;
            }

            new_messages.push((stanza_id, normalized));
        }

        for (stanza_id, normalized) in new_messages {
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

        let last_message_text = state.history.get(&chat).and_then(|h| h.last()).map(|lm| {
            format!(
                "{}: {}",
                if lm.from_me { "You" } else { &lm.sender },
                lm.text
            )
        });

        if let Some(c) = state.chats.iter_mut().find(|c| c.id == chat) {
            c.last_message = last_message_text;
        }
    }
    drop(state);
    let _ = tx.send(BackendEvent::Status("history synced".into()));
}

/// Minimal [`MessageInfo`] for messages recovered from a HistorySync conversation.
fn history_info(chat: &str, from_me: bool, push_name: Option<&String>) -> MessageInfo {
    let mut info = MessageInfo::default();
    info.source.chat = Jid::from_str(chat).unwrap_or_else(|_| Jid::new(chat, Server::Pn));
    info.source.is_from_me = from_me;
    info.push_name = push_name.cloned().unwrap_or_default();
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
        Ok(state.chats.clone())
    }

    async fn set_read(&mut self, chat: &ChatId) -> Result<(), BackendError> {
        self.check_logged_in().await?;

        let ChatId::WhatsApp(jid_str) = chat else {
            return Ok(());
        };

        let jid = Jid::from_str(jid_str)
            .map_err(|e| BackendError::Other(format!("WhatsApp: invalid chat jid: {e}")))?;

        let client = self.current_client();
        let _ = client.mark_as_read(&jid, None, &[]).await;
        let mut state = self.state.write().await;

        for c in state.chats.iter_mut() {
            if c.id == *chat {
                c.unread = false;
                c.unread_count = 0;
            }
        }
        Ok(())
    }

    async fn history(&self, chat: &ChatId) -> Result<Vec<Message>, BackendError> {
        let state = self.state.read().await;
        let messages = state.history.get(chat).cloned().unwrap_or_default();
        Ok(messages)
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
        // The TUI subscribes before it ever reads from the channel, so starting
        // the bot here guarantees the first `Connected` / `QrCode` events are
        // delivered rather than dropped. Idempotent: only the first call spawns.
        self.start_bot();
        self.tx.subscribe()
    }

    async fn disconnect(&mut self) -> Result<(), BackendError> {
        // Idempotent: only the first call performs the actual teardown.
        if self.shutdown.swap(true, Ordering::SeqCst) {
            return Ok(());
        }

        // Ensure the bot is running the run loop before requesting shutdown, in
        // case nothing ever called `subscribe` (e.g. startup failed early).
        self.start_bot();

        // Graceful stop: disconnect (flushing the store) and wait for the bot's
        // run loop to exit before returning.
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

    async fn logout(&mut self) -> Result<(), BackendError> {
        self.check_logged_in().await?;
        self.current_client().disconnect().await;
        Ok(())
    }
}
