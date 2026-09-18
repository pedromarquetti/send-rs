pub mod mock;
pub mod telegram;
pub mod whatsapp;

use std::{fmt::Display, str::FromStr};

use anyhow::Result;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio::sync::broadcast;
use whatsapp_rust::Jid;

use crate::{
    backend::{telegram::TelegramMessenger, whatsapp::WhatsAppMessenger},
    config::ProvidersConfig,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
/// Main struct that defines available Messengers
pub enum Provider {
    Telegram,
    WhatsApp,
}

impl Provider {
    pub fn all() -> &'static [Provider] {
        &[Provider::Telegram, Provider::WhatsApp]
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Telegram => "Telegram",
            Self::WhatsApp => "WhatsApp",
        }
    }

    /// Check if provider is enabled in config file
    pub fn is_enabled(&self, providers: &ProvidersConfig) -> bool {
        match self {
            Provider::Telegram => providers.telegram.enabled,
            Provider::WhatsApp => providers.whatsapp,
        }
    }

    /// Toggle enabled in config
    pub fn toggle_enabled(&self, providers: &mut ProvidersConfig) {
        match self {
            Provider::Telegram => providers.telegram.enabled ^= true,
            Provider::WhatsApp => providers.whatsapp ^= true,
        }
    }

    pub fn has_credentials(&self, providers: &ProvidersConfig) -> bool {
        match self {
            Provider::Telegram => providers.telegram.has_credentials(),
            Provider::WhatsApp => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginStepState {
    /// Login completed successfully.
    Done,
    /// Login step accepted, advance to next step.
    NextStep,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthSteps {
    Phone,
    Password,
    Code,
    /// Event-driven pairing that shows a QR code instead of a text input.
    /// Carries the current QR payload (e.g. the WhatsApp pairing URL).
    QrCode(String),
}

impl Display for AuthSteps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Phone => f.write_str("phone"),
            Self::Code => f.write_str("code"),
            Self::Password => f.write_str("password"),
            Self::QrCode(_) => f.write_str("qr code"),
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Hash)]
pub enum ChatId {
    Telegram(i64),
    WhatsApp(String),
    #[default]
    // TODO: add a Myself conversation for storing messages
    Myself,
}

impl ChatId {
    /// Converts `&str` to ChatId, needed for whatsapp-rust
    pub fn jid_to_chat_id(data: &str) -> Self {
        match Jid::from_str(data) {
            Ok(jid) => Self::WhatsApp(jid.to_non_ad_string()),
            Err(_) => ChatId::WhatsApp(data.to_string()),
        }
    }

    pub fn to_provider(&self) -> Provider {
        match &self {
            ChatId::Telegram(_) => Provider::Telegram,
            ChatId::WhatsApp(_) => Provider::WhatsApp,
            ChatId::Myself => {
                panic!("ChatId does not have a Provider")
            }
        }
    }

    /// The provider platform this chat belongs to.
    pub fn platform(&self) -> &'static str {
        match self {
            ChatId::Telegram(_) => "Telegram",
            ChatId::WhatsApp(_) => "WhatsApp",
            ChatId::Myself => "",
        }
    }

    /// Short display tag used in the chat list.
    pub fn tag(&self) -> &'static str {
        match self {
            ChatId::Telegram(_) => "TG",
            ChatId::WhatsApp(_) => "WA",
            ChatId::Myself => "ME",
        }
    }
}

impl Serialize for ChatId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&match self {
            ChatId::Telegram(id) => format!("tg:{id}"),
            ChatId::WhatsApp(jid) => format!("wa:{jid}"),
            ChatId::Myself => "me".to_string(),
        })
    }
}

impl<'de> serde::Deserialize<'de> for ChatId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        match raw.split_once(':') {
            Some(("tg", id)) => id
                .parse::<i64>()
                .map(ChatId::Telegram)
                .map_err(serde::de::Error::custom),
            Some(("wa", jid)) => Ok(ChatId::WhatsApp(jid.to_string())),
            _ if raw == "me" => Ok(ChatId::Myself),
            _ => Err(serde::de::Error::custom("invalid ChatId")),
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Chat {
    pub id: ChatId,
    pub contact_name: String,
    pub last_message: Option<String>,
    pub status: Option<String>,
    /// True when the user has pinned/fixed this chat at the top of the list.
    pub fixed: bool,
    /// keeps track of current chat scroll
    /// TODO: make this configurable? should the user be able to config if the chat keeps track of
    /// scroll?
    pub scroll: usize,
    // Keeps track of unread state for the chat
    pub unread: bool,
    pub unread_count: i32,
    /// Whether the peer is a verified business (WhatsApp usync); the chat list
    /// and chat pane render a check mark next to the name.
    #[serde(default)]
    pub verified: bool,
}

impl Chat {
    pub fn status_label(&self) -> Option<&str> {
        self.status
            .as_deref()
            .filter(|status| !status.trim().is_empty())
    }
}

/// A universal message identifier. Opaque to the UI; backends interpret it
/// (e.g. Telegram parses it to a numeric i32 for edit/delete calls).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct MessageId(pub String);

impl MessageId {
    /// Parse this id as a raw i32 (Telegram message ids). Returns `None` for
    /// non-numeric ids (mock, WhatsApp, local/synthetic echoes).
    pub fn to_i32(&self) -> Option<i32> {
        self.0.parse().ok()
    }

    /// Whether this id refers to a local, not-yet-confirmed outgoing echo
    /// (created optimistically by the TUI while the send is in flight).
    pub fn is_local(&self) -> bool {
        self.0.starts_with("local-")
    }
}

impl From<String> for MessageId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for MessageId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl std::ops::Deref for MessageId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::fmt::Display for MessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Context about the original message a reply quotes: who wrote it, when, and
/// a preview of its text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplyContext {
    pub id: MessageId,
    pub sender: String,
    pub text: String,
    pub timestamp: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageAction {
    //TODO: implement "copy" -> the user should be able to easily copy message contents
    Reply,
    Edit,
    Delete,
    /// TUI-only pseudo-action shown on failed (outgoing) messages to re-send.
    Retry,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaKind {
    Image,
    Audio,
    Video,
    Document,
    Sticker,
    Unsupported,
}

impl MediaKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Image => "Image",
            Self::Audio => "Audio",
            Self::Video => "Video",
            Self::Document => "Document",
            Self::Sticker => "Sticker",
            Self::Unsupported => "Media",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageMedia {
    pub kind: MediaKind,
    pub caption: Option<String>,
    pub file_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub message_id: MessageId,
    pub chat: ChatId,
    pub sender: String,
    /// The author's raw provider-specific id (e.g. a WhatsApp participant LID).
    /// Opaque to the UI; used to route per-author protocol actions such as
    /// group read receipts. Providers that have no such id leave it unset.
    #[serde(default)]
    // TODO: check if this field can be merged with `sender`
    pub author_id: Option<String>,
    pub text: String,
    pub timestamp: i64,
    pub from_me: bool,
    pub msg_actions: Vec<MessageAction>,

    pub media: Option<MessageMedia>,

    pub reply_to_id: Option<MessageId>,

    /// Set when this message quotes an earlier one (the reply target).
    pub reply_ctx: Option<ReplyContext>,
    /// Truthy for an optimistic outgoing echo still awaiting confirmation.
    /// When `pending` and `failed` are both false the message is confirmed.
    pub pending: bool,
    /// Outgoing message that failed to send (no auto-delete; shown grayed
    /// with a Retry action available).
    pub failed: bool,
}

#[derive(Debug, Clone)]
pub enum BackendEvent {
    Connected,
    Disconnected(String),
    Status(String),
    MessageReceived(Message),
    MessageUpdated(Message),
    MessageDeleted {
        chat: Option<ChatId>,
        message_ids: Vec<MessageId>,
    },
    UnreadUpdated {
        chat: ChatId,
        unread: bool,
        unread_count: i32,
    },
    ChatUpdated(Chat),
    /// A chat was deleted on a linked device (removed entirely from the list).
    ChatRemoved {
        chat: Chat,
    },
    /// A complete provider chat-list snapshot, emitted once per history sync so
    /// a large burst is not flood-sent as many tiny events.
    ChatList(Vec<Chat>),
    QrCode(String),
    Error(String, BackendError),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BackendError {
    #[error("not authenticated")]
    NotAuthenticated,
    #[error("Telegram rate limited; retry after {0} seconds")]
    FloodWait(u64),
    #[error("operation failed: {0}")]
    Other(String),
}

impl From<anyhow::Error> for BackendError {
    fn from(err: anyhow::Error) -> Self {
        BackendError::Other(err.to_string())
    }
}

impl From<grammers_client::InvocationError> for BackendError {
    fn from(err: grammers_client::InvocationError) -> Self {
        use grammers_client::InvocationError;
        match err {
            InvocationError::Authentication(_) => BackendError::NotAuthenticated,
            InvocationError::Rpc(error)
                if error.code == 420 && error.name == "FLOOD_WAIT" && error.value.is_some() =>
            {
                BackendError::FloodWait(error.value.unwrap() as u64)
            }
            other => BackendError::Other(format!("Telegram: {other}")),
        }
    }
}

impl From<whatsapp_rust::ClientError> for BackendError {
    fn from(err: whatsapp_rust::ClientError) -> Self {
        use whatsapp_rust::ClientError;
        match err {
            ClientError::NotLoggedIn => BackendError::NotAuthenticated,
            ClientError::NotConnected => BackendError::Other("WhatsApp: not connected".into()),
            other => BackendError::Other(format!("WhatsApp: {other}")),
        }
    }
}

impl From<whatsapp_rust::SendError> for BackendError {
    fn from(err: whatsapp_rust::SendError) -> Self {
        use whatsapp_rust::SendError;
        match err {
            SendError::NotLoggedIn => BackendError::NotAuthenticated,
            SendError::Client(e) => BackendError::from(e),
            other => BackendError::Other(format!("WhatsApp: {other}")),
        }
    }
}

#[async_trait::async_trait]
pub trait Messenger: Send + Sync {
    fn platform(&self) -> &'static str;
    async fn is_authenticated(&self) -> bool;
    /// Fetch the chat list.
    async fn chats(&self) -> Result<Vec<Chat>, BackendError>;
    /// mark a chat as read
    async fn set_read(&mut self, chat: &ChatId) -> Result<(), BackendError>;
    /// Requests the Messenger provider for chat history.
    async fn history(&self, chat: &ChatId) -> Result<Vec<Message>, BackendError>;
    /// Fetch an older page of chat history, starting before `offset_id` when provided.
    /// The default implementation falls back to the full history load for providers that do not
    /// implement paginated loading yet.
    async fn history_page(
        &self,
        chat: &ChatId,
        offset_id: Option<i32>,
        limit: usize,
    ) -> Result<Vec<Message>, BackendError> {
        let _ = offset_id;
        let _ = limit;
        self.history(chat).await
    }
    /// Fetch full details for the message quoted by `message_id`, if any.
    async fn reply_context(
        &self,
        chat: &ChatId,
        message_id: &MessageId,
    ) -> Result<Option<ReplyContext>, BackendError>;
    /// Handles sending Client >>> Messenger (provider) messages.
    /// `reply_to` quotes an earlier message id when `Some`. Returns the
    /// confirmed sent message so the caller can reconcile an echo with reality.
    async fn send(
        &self,
        chat: &ChatId,
        text: &str,
        reply_to: Option<MessageId>,
    ) -> Result<Message, BackendError>;
    /// Delete an already-sent message.
    async fn delete(&self, chat: &ChatId, id: &MessageId) -> Result<(), BackendError>;
    /// Edit an already-sent message, replacing its text.
    async fn edit(&self, chat: &ChatId, id: &MessageId, text: &str) -> Result<(), BackendError>;
    /// Messenger provider >>> Client message handling
    fn subscribe(&self) -> broadcast::Receiver<BackendEvent>;
    /// Optional provider-specific status for a chat, such as Telegram online/last-seen information.
    /// The default is `None`; a provider may fill this in later without changing the UI contract.
    async fn status(&self, _chat: &ChatId) -> Result<Option<String>, BackendError> {
        Ok(None)
    }
    /// Graceful shutdown: flush pending work, close transport, stop background tasks.
    async fn disconnect(&mut self) -> Result<(), BackendError>;
    /// Start authentication flow. For event-driven providers (WhatsApp) this is a no-op;
    /// the actual auth happens via BackendEvent callbacks. Text-based providers drive the
    /// flow through [`Messenger::login_step`] instead, so the default is a successful no-op.
    async fn login(&mut self) -> Result<(), BackendError> {
        Ok(())
    }
    /// Destroy remote session (sign out / deregister device).
    async fn logout(&mut self) -> Result<(), BackendError>;

    /// Abort an in-progress login flow, discarding any pending tokens/state.
    async fn cancel_login(&mut self) {}

    /// Human-readable names for each login step (e.g. ["Phone", "Code", "Password"]).
    /// Empty vec means this provider has no text-based login flow.
    fn login_steps(&self) -> Vec<AuthSteps> {
        Vec::new()
    }

    /// Placeholder text shown in the input field for the given step index.
    fn login_placeholder(&self, _step: usize) -> &'static str {
        ""
    }

    /// Submit user input for the given step index.
    /// Return `Ok(LoginOutcome::Done)` on successful login,
    /// `Ok(LoginOutcome::NextStep)` to advance, or `Err` on failure.
    async fn login_step(
        &mut self,
        _step: usize,
        _input: &str,
    ) -> Result<LoginStepState, BackendError> {
        Err(BackendError::Other("login not supported".into()))
    }
}

pub enum MessengerKind {
    Telegram(TelegramMessenger),
    WhatsApp(WhatsAppMessenger),
    #[cfg(test)]
    Stub(Box<dyn Messenger>),
}

impl Clone for MessengerKind {
    fn clone(&self) -> Self {
        match self {
            Self::Telegram(m) => Self::Telegram(m.clone()),
            Self::WhatsApp(m) => Self::WhatsApp(m.clone()),
            #[cfg(test)]
            Self::Stub(_) => panic!("MessengerKind::Stub is not cloneable"),
        }
    }
}

macro_rules! delegate_match {
    ($self:expr, $name:ident, ($($args:expr),*), async) => {
        match $self {
            Self::Telegram(m) => m.$name($($args),*).await,
            Self::WhatsApp(m) => m.$name($($args),*).await,
            #[cfg(test)]
            Self::Stub(m) => m.$name($($args),*).await,
        }
    };
    ($self:expr, $name:ident, ($($args:expr),*), sync) => {
        match $self {
            Self::Telegram(m) => m.$name($($args),*),
            Self::WhatsApp(m) => m.$name($($args),*),
            #[cfg(test)]
            Self::Stub(m) => m.$name($($args),*),
        }
    };
}

macro_rules! delegate {
    () => {};
    (, async fn $name:ident(&self $(, $p:ident : $t:ty)*) -> $r:ty ; $($rest:tt)*) => {
        pub async fn $name(&self $(, $p: $t)*) -> $r {
            delegate_match!(self, $name, ($($p),*), async)
        }
        delegate!($($rest)*);
    };
    (, async fn $name:ident(&mut self $(, $p:ident : $t:ty)*) -> $r:ty ; $($rest:tt)*) => {
        pub async fn $name(&mut self $(, $p: $t)*) -> $r {
            delegate_match!(self, $name, ($($p),*), async)
        }
        delegate!($($rest)*);
    };
    (, fn $name:ident(&self $(, $p:ident : $t:ty)*) -> $r:ty ; $($rest:tt)*) => {
        pub fn $name(&self $(, $p: $t)*) -> $r {
            delegate_match!(self, $name, ($($p),*), sync)
        }
        delegate!($($rest)*);
    };
    (, fn $name:ident(&mut self $(, $p:ident : $t:ty)*) -> $r:ty ; $($rest:tt)*) => {
        pub fn $name(&mut self $(, $p: $t)*) -> $r {
            delegate_match!(self, $name, ($($p),*), sync)
        }
        delegate!($($rest)*);
    };
}

impl MessengerKind {
    pub fn is_enabled(&self, config: &ProvidersConfig) -> bool {
        Provider::is_enabled(&self.provider(), config)
    }

    pub fn provider(&self) -> Provider {
        match self {
            Self::Telegram(_) => Provider::Telegram,
            Self::WhatsApp(_) => Provider::WhatsApp,
            #[cfg(test)]
            Self::Stub(m) => match m.platform() {
                "Telegram" => Provider::Telegram,
                "WhatsApp" => Provider::WhatsApp,
                _ => panic!("unsupported test provider: {}", m.platform()),
            },
        }
    }

    delegate! {
        , async fn is_authenticated(&self) -> bool ;
        , async fn chats(&self) -> Result<Vec<Chat>, BackendError> ;
        , async fn set_read(&mut self, chat: &ChatId) -> Result<(), BackendError> ;
        , async fn history(&self, chat: &ChatId) -> Result<Vec<Message>, BackendError> ;
        , async fn history_page(&self, chat: &ChatId, offset_id: Option<i32>, limit: usize) -> Result<Vec<Message>, BackendError> ;
        , async fn reply_context(&self, chat: &ChatId, message_id: &MessageId) -> Result<Option<ReplyContext>, BackendError> ;
        , async fn send(&self, chat: &ChatId, text: &str, reply_to: Option<MessageId>) -> Result<Message, BackendError> ;
        , async fn delete(&self, chat: &ChatId, id: &MessageId) -> Result<(), BackendError> ;
        , async fn edit(&self, chat: &ChatId, id: &MessageId, text: &str) -> Result<(), BackendError> ;
        , fn subscribe(&self) -> broadcast::Receiver<BackendEvent> ;
        , async fn status(&self, chat: &ChatId) -> Result<Option<String>, BackendError> ;
        , async fn disconnect(&mut self) -> Result<(), BackendError> ;
        , async fn login(&mut self) -> Result<(), BackendError> ;
        , async fn logout(&mut self) -> Result<(), BackendError> ;
        , async fn cancel_login(&mut self) -> () ;
        , fn login_steps(&self) -> Vec<AuthSteps> ;
        , fn login_placeholder(&self, step: usize) -> &'static str ;
        , async fn login_step(&mut self, step: usize, input: &str) -> Result<LoginStepState, BackendError> ;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_id_is_unique_per_platform() {
        let telegram = ChatId::Telegram(42);
        let telegram_again = ChatId::Telegram(42);
        let whatsapp = ChatId::WhatsApp("42@s.whatsapp.net".into());
        assert_eq!(telegram, telegram_again);
        assert_ne!(telegram, whatsapp);
    }

    #[test]
    fn dialog_carries_chat_list_fields() {
        let chat = Chat {
            id: ChatId::WhatsApp("5511999999999@s.whatsapp.net".into()),
            contact_name: "Alice".into(),
            last_message: Some("hey".into()),
            unread: true,
            unread_count: 1,
            ..Default::default()
        };
        assert_eq!(chat.contact_name, "Alice");
        assert_eq!(chat.last_message.as_deref(), Some("hey"));
        assert!(chat.unread);
    }

    #[test]
    fn chat_status_label_ignores_empty_status() {
        let mut chat = Chat::default();
        assert_eq!(chat.status_label(), None);

        chat.status = Some("   ".into());
        assert_eq!(chat.status_label(), None);

        chat.status = Some("online".into());
        assert_eq!(chat.status_label(), Some("online"));
    }

    #[test]
    fn local_message_ids_are_distinguished_from_server_ids() {
        assert!(MessageId::from("local-1").is_local());
        assert!(!MessageId::from("12345").is_local());
        assert_eq!(MessageId::from("12345").to_i32(), Some(12345));
        assert_eq!(MessageId::from("local-1").to_i32(), None);
    }
}
