use crate::backend::AuthSteps;

use super::{
    BackendError, BackendEvent, Chat, ChatId, LoginStepState, Message, MessageAction, MessageId,
    Messenger, ReplyContext,
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
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;
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

async fn self_user_id(client: &Client) -> Option<ChatId> {
    client
        .get_me()
        .await
        .ok()
        .and_then(|user| user.id().bare_id().map(ChatId::Telegram))
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
    client: Client,
    api_hash: String,
    tx: broadcast::Sender<BackendEvent>,
    login_token: Arc<RwLock<Option<LoginToken>>>,
    cached_dialogs: Arc<StdMutex<Vec<Dialog>>>,
    chat_refresh: Arc<Mutex<()>>,
    cached_chats: Arc<Mutex<CachedChats>>,
    flood_wait_until: Arc<Mutex<Option<Instant>>>,
    sync_update_state_secs: u64,
}

impl TelegramMessenger {
    /// Open (or create) the session, build a [`SenderPool`] and [`Client`].
    /// Spawns the pool runner in a background task.
    /// Does **not** attempt to log in – call [`Messenger::is_authenticated`]
    /// or [`Messenger::login`] afterwards.
    pub async fn new(
        session_dir: PathBuf,
        api_id: u32,
        api_hash: &str,
        sync_update_state_secs: u64,
    ) -> Result<Self, BackendError> {
        let session_path = session_dir.join("tg_session.sqlite");
        let session = Arc::new(
            SqliteSession::open(&session_path)
                .await
                .map_err(|e| BackendError::Other(format!("failed to open TG session: {e}")))?,
        );

        let pool = SenderPool::new(session, api_id as i32);

        // The runner must be spawned; it drives all network I/O.
        let runner = pool.runner;
        let updates = pool.updates;
        tokio::spawn(async move { runner.run().await });

        let client = Client::new(pool.handle);

        let (tx, _) = broadcast::channel(128);

        let messenger = Self {
            client,
            api_hash: api_hash.to_string(),
            tx,
            login_token: Arc::new(RwLock::new(None)),
            cached_dialogs: Arc::new(StdMutex::new(Vec::new())),
            chat_refresh: Arc::new(Mutex::new(())),
            cached_chats: Arc::new(Mutex::new(None)),
            flood_wait_until: Arc::new(Mutex::new(None)),
            sync_update_state_secs,
        };

        messenger.spawn_update_listener(updates);
        Ok(messenger)
    }

    fn spawn_update_listener(&self, updates: mpsc::UnboundedReceiver<UpdatesLike>) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        let sync_secs = self.sync_update_state_secs;

        tokio::spawn(async move {
            let config = UpdatesConfiguration {
                catch_up: true,
                ..Default::default()
            };
            let mut stream = match client.stream_updates(updates, config).await {
                Ok(s) => s,
                Err(e) => {
                    error!("Telegram update stream failed to start: {e}");
                    return;
                }
            };
            info!("Telegram update stream started");
            let mut own_chat = self_user_id(&client).await;
            let mut sync_interval =
                tokio::time::interval(std::time::Duration::from_secs(sync_secs));

            loop {
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
                                    let peer = match msg.peer() {
                                        Some(p) => p,
                                        None => continue,
                                    };

                                    let chat_id =
                                        ChatId::Telegram(peer.id().bare_id().unwrap_or(0));

                                    if own_chat.is_none() {
                                        own_chat = self_user_id(&client).await;
                                    }

                                    let from_me =
                                        msg.outgoing() || own_chat.as_ref() == Some(&chat_id);

                                    let text = if msg.media().is_some() {
                                        "[media]".to_string()
                                    } else {
                                        msg.text().to_string()
                                    };

                                    let sender = if from_me {
                                        "You".to_string()
                                    } else {
                                        msg.sender()
                                            .and_then(|p| p.name())
                                            .unwrap_or("Unknown")
                                            .to_string()
                                    };

                                    let text_preview: String = text.chars().take(80).collect();
                                    debug!(
                                        sender,
                                        chat_id = ?chat_id,
                                        text_preview,
                                        "MessageReceived"
                                    );

                                    let message = Message {
                                        message_id: msg.id().to_string().into(),
                                        chat: chat_id,
                                        sender,
                                        text: text.clone(),
                                        timestamp: msg.date().timestamp(),
                                        from_me,
                                        msg_actions: message_actions(from_me),
                                        reply_to_id: msg.reply_to_message_id().map(|id| id.to_string().into()),
                                        reply_ctx: reply_context(&client, &msg).await,
                                        pending: false,
                                        failed: false,
                                    };
                                    let _ = tx.send(BackendEvent::MessageReceived(message));
                                }
                                Update::MessageEdited(msg) => {
                                    let peer = match msg.peer() {
                                        Some(p) => p,
                                        None => continue,
                                    };
                                    let chat_id =
                                        ChatId::Telegram(peer.id().bare_id().unwrap_or(0));

                                    if own_chat.is_none() {
                                        own_chat = self_user_id(&client).await;
                                    }

                                    let from_me =
                                        msg.outgoing() || own_chat.as_ref() == Some(&chat_id);
                                    let name = peer.name().unwrap_or("Unknown").to_string();
                                    let last_message = if msg.outgoing() {
                                        Some(format!("You: {}", msg.text()))
                                    } else {
                                        Some(format!("{name}: {}", msg.text()))
                                    };

                                    let message = Message {
                                        message_id: msg.id().to_string().into(),
                                        chat: chat_id.clone(),
                                        sender: if from_me {
                                            "You".into()
                                        } else {
                                            name.clone()
                                        },
                                        text: msg.text().to_string(),
                                        timestamp: msg.date().timestamp(),
                                        from_me,
                                        msg_actions: message_actions(from_me),
                                        reply_to_id: msg.reply_to_message_id().map(|id| id.to_string().into()),
                                        reply_ctx: None,
                                        pending: false,
                                        failed: false,
                                    };

                                    let _ = tx.send(BackendEvent::MessageUpdated(message));
                                    debug!(chat_id = ?chat_id, "ChatUpdated (edited)");
                                    let _ = tx.send(BackendEvent::ChatUpdated(Chat {
                                        id: chat_id,
                                        contact_name: name,
                                        last_message,
                                        scroll: 0,
                                        unread: false,
                                        unread_count: 0,
                                    }));
                                }
                                Update::Raw(raw) => {
                                    if let tl::enums::Update::ReadHistoryInbox(ref u) = *raw {
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
                                        let _ = tx.send(BackendEvent::ChatUpdated(Chat {
                                            id: chat_id,
                                            contact_name: String::new(),
                                            last_message: None,
                                            scroll: 0,
                                            unread: u.still_unread_count > 0,
                                            unread_count: u.still_unread_count,
                                        }));
                                    } else if let tl::enums::Update::ReadChannelInbox(ref u) = *raw {
                                        let chat_id = ChatId::Telegram(u.channel_id);
                                        debug!(
                                            chat_id = ?chat_id,
                                            still_unread = u.still_unread_count,
                                            max_id = u.max_id,
                                            "ReadChannelInbox"
                                        );
                                        let _ = tx.send(BackendEvent::ChatUpdated(Chat {
                                            id: chat_id,
                                            contact_name: String::new(),
                                            last_message: None,
                                            scroll: 0,
                                            unread: u.still_unread_count > 0,
                                            unread_count: u.still_unread_count,
                                        }));
                                    } else {
                                        debug!("Telegram update (unhandled): {raw:?}");
                                    }
                                }
                                _ => {}
                            },
                            Err(e) => {
                                error!("Telegram update stream error: {e}");
                                break;
                            }
                        }
                    }
                }
            }
        });
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
        let mut dialogs_iter = self.client.iter_dialogs();
        let mut chats = Vec::new();
        let mut raw_dialogs = Vec::new();

        while let Some(dialog) = dialogs_iter.next().await? {
            let peer = dialog.peer();
            let id = ChatId::Telegram(peer.id().bare_id().unwrap_or(0));
            let contact_name = peer.name().unwrap_or("Unknown").to_string();

            let last_message = dialog.last_message.as_ref().map(|msg| {
                let prefix = if msg.outgoing() {
                    "You".to_string()
                } else {
                    msg.sender()
                        .and_then(|p| p.name())
                        .unwrap_or("Unknown")
                        .to_string()
                };
                format!("{prefix}: {}", msg.text())
            });

            let unread_count = match &dialog.raw {
                grammers_client::tl::enums::Dialog::Dialog(d) => d.unread_count,
                grammers_client::tl::enums::Dialog::Folder(_) => 0,
            };

            chats.push(Chat {
                id,
                contact_name,
                last_message,
                unread: unread_count > 0,
                unread_count,
                scroll: 0,
            });
            raw_dialogs.push(dialog);
        }
        info!("Loaded {} Telegram dialogs", chats.len());
        self.cache_dialogs(raw_dialogs);
        Ok(chats)
    }

    fn report_status(&self, status: String) {
        let _ = self.tx.send(BackendEvent::Status(status));
    }

    fn clear_status(&self) {
        let _ = self.tx.send(BackendEvent::Status(String::new()));
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
    fn platform(&self) -> &'static str {
        "Telegram"
    }

    async fn is_authenticated(&self) -> bool {
        self.client.is_authorized().await.unwrap_or(false)
    }

    async fn chats(&self) -> Result<Vec<Chat>, BackendError> {
        const MAX_REFRESH_WAIT: u64 = 120;

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
                self.report_status(format!(
                    "Telegram rate limited; retrying in {}s",
                    wait.as_secs().max(1)
                ));
                tokio::time::sleep(wait).await;
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
                    self.report_status(format!(
                        "Telegram rate limited; retrying in {}s",
                        seconds.max(1)
                    ));
                    tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
                }
                Err(BackendError::FloodWait(seconds)) => {
                    drop(refresh);
                    self.report_status(format!(
                        "Telegram rate limited; retry deferred for {seconds}s"
                    ));
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
        self.client.mark_as_read(peer_ref).await?;
        Ok(())
    }

    async fn history(&self, chat: &ChatId) -> Result<Vec<Message>, BackendError> {
        let bare_id = match chat {
            ChatId::Telegram(id) => *id,
            _ => return Err(BackendError::Other("not a Telegram chat".into())),
        };

        let peer_ref = self
            .find_dialog_peer_ref(bare_id)
            .ok_or_else(|| BackendError::Other("chat not found in dialog cache".into()))?;

        debug!(bare_id, "Fetching message history");

        let own_chat = self_user_id(&self.client).await;
        let mut messages_rev = Vec::new();
        let mut msg_iter = self.client.iter_messages(peer_ref).limit(100);

        while let Some(msg) = msg_iter.next().await? {
            let from_me = msg.outgoing() || own_chat.as_ref() == Some(chat);
            let sender = if from_me {
                "You".to_string()
            } else {
                msg.sender()
                    .and_then(|p| p.name())
                    .unwrap_or("Unknown")
                    .to_string()
            };

            messages_rev.push(Message {
                message_id: msg.id().to_string().into(),
                chat: chat.clone(),
                sender,
                text: msg.text().to_string(),
                timestamp: msg.date().timestamp(),
                from_me,
                msg_actions: message_actions(from_me),
                reply_to_id: msg.reply_to_message_id().map(|id| id.to_string().into()),
                // Reply details are fetched lazily when the message is opened.
                reply_ctx: None,
                pending: false,
                failed: false,
            });
        }

        // grammers iter_messages defaults to newest-to-oldest; reverse so
        // oldest is first (chronological order for display).
        messages_rev.reverse();
        debug!(bare_id, count = messages_rev.len(), "History loaded");
        Ok(messages_rev)
    }

    async fn reply_context(
        &self,
        chat: &ChatId,
        message_id: &MessageId,
    ) -> Result<Option<ReplyContext>, BackendError> {
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

        let message = self
            .client
            .get_messages_by_id(peer_ref, &[id])
            .await?
            .into_iter()
            .next()
            .flatten();

        match message {
            Some(message) => Ok(reply_context(&self.client, &message).await),
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
        let sent = self.client.send_message(peer_ref, input).await?;

        Ok(Message {
            message_id: sent.id().to_string().into(),
            chat: chat.clone(),
            sender: "You".into(),
            text: text.to_string(),
            timestamp: sent.date().timestamp(),
            from_me: true,
            msg_actions: message_actions(true),
            reply_to_id: reply_to.clone(),
            reply_ctx: None,
            pending: false,
            failed: false,
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
        self.client.delete_messages(peer_ref, &[msg_id]).await?;
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
        self.client.edit_message(peer_ref, msg_id, input).await?;
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<BackendEvent> {
        self.tx.subscribe()
    }

    async fn disconnect(&mut self) -> Result<(), BackendError> {
        self.client.disconnect();
        Ok(())
    }

    async fn logout(&mut self) -> Result<(), BackendError> {
        self.client
            .sign_out()
            .await
            .map_err(|e| BackendError::Other(format!("sign-out failed: {e}")))?;
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
                let token = self
                    .client
                    .request_login_code(input, &self.api_hash)
                    .await?;
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
                        match self.client.sign_in(&token, input).await {
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
                        match self.client.check_password(token, input.as_bytes()).await {
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
