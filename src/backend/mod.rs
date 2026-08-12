pub mod mock;
pub mod telegram;
pub mod whatsapp;

use anyhow::Result;
use tokio::sync::broadcast;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ChatId {
    Telegram(i64),
    WhatsApp(String),
}

#[derive(Debug, Clone)]
pub struct Chat {
    pub id: ChatId,
    pub name: String,
    pub last_message: Option<String>,
    pub unread: bool,
    pub unread_count: i32,
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
    async fn read(&mut self, chat: &ChatId) -> Result<(), BackendError>;
    async fn history(&self, chat: &ChatId) -> Result<Vec<Message>, BackendError>;
    async fn send(&self, chat: &ChatId, text: &str) -> Result<(), BackendError>;
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
            name: "Alice".into(),
            last_message: Some("hey".into()),
            unread: true,
            unread_count: 1,
        };
        assert_eq!(chat.name, "Alice");
        assert_eq!(chat.last_message.as_deref(), Some("hey"));
        assert!(chat.unread);
    }
}
