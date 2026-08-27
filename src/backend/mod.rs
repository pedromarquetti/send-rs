pub mod mock;
pub mod telegram;
pub mod whatsapp;

use std::fmt::Display;

use anyhow::Result;
use tokio::sync::broadcast;

use crate::config::ProvidersConfig;

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
    Username,
    Phone,
    Password,
    Code,
}

impl Display for AuthSteps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Phone => f.write_str("phone"),
            Self::Code => f.write_str("code"),
            Self::Password => f.write_str("password"),
            Self::Username => f.write_str("username"),
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

#[derive(Debug, Default, Clone)]
pub struct Chat {
    pub id: ChatId,
    pub contact_name: String,
    pub last_message: Option<String>,
    /// keeps track of current chat scroll
    /// TODO: make this configurable? should the user be able to config if the chat keeps track of
    /// scroll?
    pub scroll: usize,
    // Keeps track of unread state for the chat
    pub unread: bool,
    pub unread_count: i32,
}

impl ChatId {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageAction {
    Reply,
    Edit,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub id: String,
    pub chat: ChatId,
    pub sender: String,
    pub text: String,
    pub timestamp: i64,
    pub from_me: bool,
    pub options: Vec<MessageAction>,
}

#[derive(Debug, Clone)]
pub enum BackendEvent {
    Connected,
    Disconnected(String),
    MessageReceived(Message),
    ChatUpdated(Chat),
    Error(String, BackendError),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BackendError {
    #[error("not authenticated")]
    NotAuthenticated,
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

#[async_trait::async_trait]
pub trait Messenger: Send + Sync {
    fn platform(&self) -> &'static str;
    async fn is_authenticated(&self) -> bool;
    /// get chat list for populating sidebar
    async fn chats(&self) -> Result<Vec<Chat>, BackendError>;
    /// mark a chat as read
    async fn set_read(&mut self, chat: &ChatId) -> Result<(), BackendError>;
    /// Requests the Messenger provider for chat history
    async fn history(&self, chat: &ChatId) -> Result<Vec<Message>, BackendError>;
    /// Handles sending Client >>> Messenger (provider) messages
    async fn send(&self, chat: &ChatId, text: &str) -> Result<(), BackendError>;
    /// Messenger provider >>> Client message handling
    fn subscribe(&self) -> broadcast::Receiver<BackendEvent>;
    /// Graceful shutdown: flush pending work, close transport, stop background tasks.
    async fn disconnect(&mut self) -> Result<(), BackendError>;
    /// Start authentication flow. For event-driven providers (WhatsApp) this is a no-op;
    /// the actual auth happens via BackendEvent callbacks.
    async fn login(&mut self) -> Result<(), BackendError>;
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
    Telegram(telegram::TelegramMessenger),
    WhatsApp(mock::MockMessenger),
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
        , async fn send(&self, chat: &ChatId, text: &str) -> Result<(), BackendError> ;
        , fn subscribe(&self) -> broadcast::Receiver<BackendEvent> ;
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
    fn dialog_carries_sidebar_fields() {
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
}
