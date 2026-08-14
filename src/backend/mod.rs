pub mod mock;
pub mod telegram;
pub mod whatsapp;
use anyhow::Result;
use tokio::sync::broadcast;

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

#[derive(Debug, Clone)]
pub struct Message {
    pub id: String,
    pub chat: ChatId,
    pub sender: String,
    pub text: String,
    pub timestamp: i64,
    pub from_me: bool,
}

#[derive(Debug, Clone)]
pub enum BackendEvent {
    Connected,
    Disconnected(String),
    MessageReceived(Message),
    ChatUpdated(Chat),
}

#[derive(Debug, thiserror::Error)]
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
