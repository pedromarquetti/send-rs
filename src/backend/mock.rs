use super::{BackendError, BackendEvent, Chat, ChatId, Message, Messenger};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::broadcast;
use tokio::time::{sleep, Duration};

pub struct MockMessenger {
    name: &'static str,
    state: Mutex<State>,
    tx: broadcast::Sender<BackendEvent>,
    incoming_chat: ChatId,
}

struct State {
    chats: Vec<Chat>,
    history: HashMap<ChatId, Vec<Message>>,
    outgoing_seq: u64,
}

impl MockMessenger {
    pub fn new(name: &'static str) -> Self {
        let (tx, _) = broadcast::channel(128);
        let (chats, history) = mock_data(name);
        let incoming_chat = chats.first().map(|c| c.id.clone()).expect("mock has chats");
        Self {
            name,
            state: Mutex::new(State {
                chats,
                history,
                outgoing_seq: 0,
            }),
            tx,
            incoming_chat,
        }
    }

    pub fn spawn_incoming_messages(&self) {
        let tx = self.tx.clone();
        let chat = self.incoming_chat.clone();
        tokio::spawn(async move {
            let mut n = 0u64;
            loop {
                sleep(Duration::from_secs(15)).await;
                n += 1;
                let msg = Message {
                    id: format!("mock-incoming-{n}"),
                    chat: chat.clone(),
                    sender: "Mock Bot".into(),
                    text: format!("simulated incoming message #{n}"),
                    timestamp: now(),
                    from_me: false,
                };
                if tx.send(BackendEvent::MessageReceived(msg)).is_err() {
                    return;
                }
            }
        });
    }
}

#[async_trait::async_trait]
impl Messenger for MockMessenger {
    fn platform(&self) -> &'static str {
        self.name
    }

    async fn is_authenticated(&self) -> bool {
        true
    }

    async fn chats(&self) -> Result<Vec<Chat>, BackendError> {
        let state = self.state.lock().expect("Expected valid state");
        Ok(state.chats.clone())
    }

    async fn read(&mut self, chat: &ChatId) -> Result<(), BackendError> {
        let mut state = self.state.lock().expect("Expected valid state");
        for entry in &mut state.chats {
            if entry.id == *chat {
                entry.unread = false;
                entry.unread_count = 0;
                break;
            }
        }
        Ok(())
    }

    async fn history(&self, chat: &ChatId) -> Result<Vec<Message>, BackendError> {
        let state = self.state.lock().expect("mock state poisoned");
        let messages = state.history.get(chat).cloned().unwrap_or_default();
        Ok(messages)
    }

    async fn send(&self, chat: &ChatId, text: &str) -> Result<(), BackendError> {
        let mut state = self.state.lock().expect("mock state poisoned");
        state.outgoing_seq += 1;
        let msg = Message {
            id: format!("mock-outgoing-{}", state.outgoing_seq),
            chat: chat.clone(),
            sender: "You".into(),
            text: text.to_string(),
            timestamp: now(),
            from_me: true,
        };
        state
            .history
            .entry(chat.clone())
            .or_default()
            .push(msg.clone());
        let updated_chat = state
            .chats
            .iter()
            .find(|c| c.id == *chat)
            .cloned()
            .map(|mut c| {
                c.last_message = Some(format!("You: {text}"));
                c
            });
        drop(state);

        self.tx
            .send(BackendEvent::MessageReceived(msg))
            .map_err(|_| BackendError::Other("event channel closed".into()))?;
        if let Some(chat) = updated_chat {
            self.tx
                .send(BackendEvent::ChatUpdated(chat))
                .map_err(|_| BackendError::Other("event channel closed".into()))?;
        }
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<BackendEvent> {
        self.tx.subscribe()
    }
}

fn mock_data(name: &'static str) -> (Vec<Chat>, HashMap<ChatId, Vec<Message>>) {
    let mut history = HashMap::new();
    match name {
        "Telegram" => {
            let news = ChatId::Telegram(101);
            let family = ChatId::Telegram(102);
            let alice = ChatId::Telegram(103);
            let chats = vec![
                Chat {
                    id: news.clone(),
                    name: "Telegram News".into(),
                    last_message: Some("MTProto v0.10 released".into()),
                    unread: false,
                    unread_count: 0,
                },
                Chat {
                    id: family.clone(),
                    name: "Family Group".into(),
                    last_message: Some("Mum: call me later".into()),
                    unread: true,
                    unread_count: 3,
                },
                Chat {
                    id: alice.clone(),
                    name: "Alice".into(),
                    last_message: Some("You: ok, see you".into()),
                    unread: false,
                    unread_count: 0,
                },
            ];
            history.insert(family.clone(), long_history(family));
            history.insert(
                news.clone(),
                vec![
                    message("tg-1", &news, "Telegram News", "MTProto v0.10 is out!", false),
                    message("tg-2", &news, "Telegram News", "Pure-Rust client library.", false),
                ],
            );
            history.insert(
                alice.clone(),
                vec![
                    message("tg-3", &alice, "Alice", "Are you coming tonight?", false),
                    message("tg-4", &alice, "You", "ok, see you", true),
                ],
            );
            (chats, history)
        }
        _ => {
            let bob = ChatId::WhatsApp("5511999990001@s.whatsapp.net".into());
            let design = ChatId::WhatsApp("5511999990002@s.whatsapp.net".into());
            let chats = vec![
                Chat {
                    id: bob.clone(),
                    name: "Bob".into(),
                    last_message: Some("Bob: did you push?".into()),
                    unread: true,
                    unread_count: 5,
                },
                Chat {
                    id: design.clone(),
                    name: "Design Team".into(),
                    last_message: Some("Lia: new figma board".into()),
                    unread: false,
                    unread_count: 0,
                },
            ];
            history.insert(bob.clone(), long_history(bob));
            history.insert(
                design.clone(),
                vec![
                    message("wa-1", &design, "Lia", "new figma board is up", false),
                    message("wa-2", &design, "You", "looks good!", true),
                ],
            );
            (chats, history)
        }
    }
}

fn long_history(chat: ChatId) -> Vec<Message> {
    let senders = ["Mum", "Dad", "Alex", "Sara", "Omar", "You"];
    let lines = [
        "hello everyone",
        "any plans for the weekend?",
        "I'm free on saturday",
        "let's do a video call",
        "works for me",
        "did you see the news?",
        "yes, crazy stuff",
        "we should talk about it live",
        "bringing pizza",
        "count me in",
        "what time?",
        "7pm sounds good",
        "can we do 8?",
        "sure",
        "I'll set a reminder",
        "perfect",
        "also, remember mom's birthday",
        "oh right, it's next tuesday",
        "I got her a plant",
        "she loves plants",
        "nice one",
        "who's bringing the cake?",
        "I will",
        "awesome",
        "see you all soon",
        "anyone seen the cat?",
        "she's under the couch again",
        "classic",
        "ok meeting adjourned",
        "bye!",
    ];
    lines
        .iter()
        .enumerate()
        .map(|(i, text)| {
            let from_me = senders[i % senders.len()] == "You";
            message(
                &format!("mock-{i}"),
                &chat,
                if from_me { "You" } else { senders[i % senders.len()] },
                text,
                from_me,
            )
        })
        .collect()
}

fn message(id: &str, chat: &ChatId, sender: &str, text: &str, from_me: bool) -> Message {
    Message {
        id: id.into(),
        chat: chat.clone(),
        sender: sender.into(),
        text: text.into(),
        timestamp: now(),
        from_me,
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before unix epoch")
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn chats_expose_platform_name() {
        let mock = MockMessenger::new("Telegram");
        let chats = mock.chats().await.unwrap();
        assert!(!chats.is_empty());
        assert_eq!(mock.platform(), "Telegram");
        assert!(mock.is_authenticated().await);
    }

    #[tokio::test]
    async fn send_emits_message_and_updates_history() {
        let mock = MockMessenger::new("Telegram");
        let mut rx = mock.subscribe();
        let chat = mock.chats().await.unwrap().remove(0).id;
        mock.send(&chat, "hi").await.unwrap();

        let mut saw_message = false;
        let mut saw_chat = false;
        while let Ok(event) = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
            match event.unwrap() {
                BackendEvent::MessageReceived(msg) => {
                    saw_message = msg.from_me && msg.text == "hi";
                }
                BackendEvent::ChatUpdated(c) => {
                    saw_chat = c.last_message.as_deref() == Some("You: hi");
                }
                _ => {}
            }
            if saw_message && saw_chat {
                break;
            }
        }
        assert!(saw_message, "expected a MessageReceived event");
        assert!(saw_chat, "expected a ChatUpdated event");
    }

    #[test]
    fn mock_data_is_deterministic() {
        let (chats, history) = mock_data("Telegram");
        assert_eq!(chats.len(), 3);
        assert!(history.values().all(|msgs| !msgs.is_empty()));
    }
}
