use super::{BackendError, BackendEvent, Chat, ChatId, Message, Messenger};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tokio::time::{Duration, sleep};

const NEWS_ID: ChatId = ChatId::Telegram(101);
const FAMILY_ID: ChatId = ChatId::Telegram(102);
const ALICE_ID: ChatId = ChatId::Telegram(103);
const ECHO_ID: ChatId = ChatId::Telegram(104);
const ECHO_SENDER: &str = "Echo Bot";

pub struct MockMessenger {
    name: &'static str,
    state: Arc<Mutex<State>>,
    tx: broadcast::Sender<BackendEvent>,
    incoming_chat: ChatId,
    echo_chat: ChatId,
}

struct State {
    chats: Vec<Chat>,
    history: HashMap<ChatId, Vec<Message>>,
    outgoing_seq: u64,
}

struct MockData {
    chats: Vec<Chat>,
    history: HashMap<ChatId, Vec<Message>>,
    incoming_chat: ChatId,
    echo_chat: ChatId,
}

impl MockMessenger {
    pub fn new(name: &'static str) -> Self {
        let (tx, _) = broadcast::channel(128);
        let data = mock_data(name);
        Self {
            name,
            state: Arc::new(Mutex::new(State {
                chats: data.chats,
                history: data.history,
                outgoing_seq: 0,
            })),
            tx,
            incoming_chat: data.incoming_chat,
            echo_chat: data.echo_chat,
        }
    }

    pub fn spawn_incoming_messages(&self) {
        self.spawn_incoming_messages_every(Duration::from_secs(15));
    }

    fn spawn_incoming_messages_every(&self, interval: Duration) {
        let tx = self.tx.clone();
        let chat = self.incoming_chat.clone();
        let state = self.state.clone();
        let name = self.name;
        tokio::spawn(async move {
            let mut n = 0u64;
            loop {
                sleep(interval).await;
                n += 1;
                let msg = Message {
                    id: format!("mock-{name}-incoming-{n}"),
                    chat: chat.clone(),
                    sender: format!("{name} Mock"),
                    text: format!("simulated incoming message #{n}"),
                    timestamp: now(),
                    from_me: false,
                };
                let mut state = state.lock().expect("mock state poisoned");
                state
                    .history
                    .entry(chat.clone())
                    .or_default()
                    .push(msg.clone());
                drop(state);
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

    async fn set_read(&mut self, chat: &ChatId) -> Result<(), BackendError> {
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

        if chat == &self.echo_chat {
            self.spawn_echo(chat.clone(), text);
        }
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<BackendEvent> {
        self.tx.subscribe()
    }
}

impl MockMessenger {
    fn spawn_echo(&self, chat: ChatId, text: &str) {
        let tx = self.tx.clone();
        let state = self.state.clone();
        let sender = ECHO_SENDER.to_string();
        let text = text.to_string();
        tokio::spawn(async move {
            sleep(Duration::from_millis(150)).await;
            let mut state = state.lock().expect("mock state poisoned");
            state.outgoing_seq += 1;
            let msg = Message {
                id: format!("mock-echo-{}", state.outgoing_seq),
                chat: chat.clone(),
                sender: sender.clone(),
                text: text.clone(),
                timestamp: now(),
                from_me: false,
            };
            state
                .history
                .entry(chat.clone())
                .or_default()
                .push(msg.clone());
            let updated_chat = state
                .chats
                .iter()
                .find(|c| c.id == chat)
                .cloned()
                .map(|mut c| {
                    c.last_message = Some(format!("{sender}: {text}"));
                    c
                });
            drop(state);
            if tx.send(BackendEvent::MessageReceived(msg)).is_err() {
                return;
            }
            if let Some(chat) = updated_chat {
                let _ = tx.send(BackendEvent::ChatUpdated(chat));
            }
        });
    }
}

fn mock_data(name: &'static str) -> MockData {
    let mut history: HashMap<ChatId, Vec<Message>> = HashMap::new();
    match name {
        "Telegram" => {
            let chats = vec![
                Chat {
                    id: NEWS_ID.clone(),
                    contact_name: "Telegram News".into(),
                    last_message: Some("MTProto v0.10 released".into()),
                    unread: false,
                    unread_count: 0,
                    ..Default::default()
                },
                Chat {
                    id: FAMILY_ID.clone(),
                    contact_name: "Family Group".into(),
                    last_message: Some("Mum: call me later".into()),
                    unread: true,
                    unread_count: 3,
                    ..Default::default()
                },
                Chat {
                    id: ALICE_ID.clone(),
                    contact_name: "Alice".into(),
                    last_message: Some("You: ok, see you".into()),
                    unread: false,
                    unread_count: 0,
                    ..Default::default()
                },
                Chat {
                    id: ECHO_ID.clone(),
                    contact_name: ECHO_SENDER.into(),
                    last_message: None,
                    unread: false,
                    unread_count: 0,
                    ..Default::default()
                },
            ];

            history.insert(FAMILY_ID.clone(), long_history(FAMILY_ID.clone()));
            history.insert(
                NEWS_ID.clone(),
                vec![
                    message(
                        "tg-1",
                        &NEWS_ID,
                        "Telegram News",
                        "MTProto v0.10 is out!",
                        false,
                    ),
                    message(
                        "tg-2",
                        &NEWS_ID,
                        "Telegram News",
                        "Pure-Rust client library.",
                        false,
                    ),
                ],
            );
            history.insert(
                ALICE_ID.clone(),
                vec![
                    message("tg-3", &ALICE_ID, "Alice", "Are you coming tonight?", false),
                    message("tg-4", &ALICE_ID, "You", "ok, see you", true),
                ],
            );
            MockData {
                chats,
                history,
                incoming_chat: FAMILY_ID,
                echo_chat: ECHO_ID,
            }
        }
        _ => {
            let bob = ChatId::WhatsApp("5511999990001@s.whatsapp.net".into());
            let design = ChatId::WhatsApp("5511999990002@s.whatsapp.net".into());
            let echo = ChatId::WhatsApp("5511999990003@s.whatsapp.net".into());
            let chats = vec![
                Chat {
                    id: bob.clone(),
                    contact_name: "Bob".into(),
                    last_message: Some("Bob: did you push?".into()),
                    unread: true,
                    unread_count: 5,
                    ..Default::default()
                },
                Chat {
                    id: design.clone(),
                    contact_name: "Design Team".into(),
                    last_message: Some("Lia: new figma board".into()),
                    unread: false,
                    unread_count: 0,
                    ..Default::default()
                },
                Chat {
                    id: echo.clone(),
                    contact_name: ECHO_SENDER.into(),
                    last_message: None,
                    unread: false,
                    unread_count: 0,
                    ..Default::default()
                },
            ];
            history.insert(
                bob.clone(),
                vec![
                    message("wa-0a", &bob, "Bob", "suh", false),
                    message("wa-0b", &bob, "You", "suh", true),
                ],
            );
            history.insert(
                design.clone(),
                vec![
                    message("wa-1", &design, "Lia", "new figma board is up", false),
                    message("wa-2", &design, "You", "looks good!", true),
                ],
            );
            MockData {
                chats,
                history,
                incoming_chat: bob,
                echo_chat: echo,
            }
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
                if from_me {
                    "You"
                } else {
                    senders[i % senders.len()]
                },
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
        let data = mock_data("Telegram");
        assert_eq!(data.chats.len(), 4);
        assert!(data.history.values().all(|msgs| !msgs.is_empty()));
        assert_eq!(data.incoming_chat, ChatId::Telegram(102));
        assert_eq!(data.echo_chat, ChatId::Telegram(104));
        assert!(
            data.chats
                .iter()
                .any(|c| c.id == data.echo_chat && c.contact_name == ECHO_SENDER)
        );
    }

    #[tokio::test]
    async fn echo_chat_replies_to_sent_message() {
        let mock = MockMessenger::new("Telegram");
        let mut rx = mock.subscribe();
        let echo = mock.echo_chat.clone();
        mock.send(&echo, "hello there").await.unwrap();

        let echoed = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match rx.recv().await {
                    Ok(BackendEvent::MessageReceived(msg)) if !msg.from_me && msg.chat == echo => {
                        break msg;
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => panic!("channel closed before echo"),
                }
            }
        })
        .await
        .expect("echo reply never arrived");

        assert_eq!(echoed.text, "hello there");
        assert_eq!(echoed.sender, ECHO_SENDER);
        let history = mock.history(&echo).await.unwrap();
        assert!(history.iter().any(|m| m.id == echoed.id));
    }

    #[tokio::test]
    async fn incoming_messages_are_persisted_to_history() {
        let mock = MockMessenger::new("Telegram");
        let incoming = mock.incoming_chat.clone();
        mock.spawn_incoming_messages_every(Duration::from_millis(50));

        tokio::time::sleep(Duration::from_millis(250)).await;

        let history = mock.history(&incoming).await.unwrap();
        assert!(
            history.iter().any(|m| m.sender == "Telegram Mock"),
            "expected a persisted incoming message"
        );
    }
}
