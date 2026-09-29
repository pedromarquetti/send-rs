//! Notification payloads: what a new message is reduced to before it reaches a
//! notification sink (a sound cue, an OS notification).
//!
//! This module owns the *policy* — which messages are worth telling the user
//! about, and what to say — and nothing else. Deciding which sink fires is the
//! caller's job, so nothing here touches audio, the terminal, or a platform
//! API.
//!
//! [`Notice`] is deliberately provider-neutral: a [`Provider`] tag for routing
//! and a display title, never a JID, a peer id or anything else a provider
//! invented. That is also why the provider is carried *alongside* the chat
//! rather than derived from it: `ChatId::to_provider` panics for the "Myself"
//! conversation, which must never be a notification target.

use crate::backend::{ChatId, Message, Provider};

/// Longest title a notice carries, in characters.
const MAX_TITLE_CHARS: usize = 60;
/// Longest body a notice carries, in characters.
const MAX_BODY_CHARS: usize = 140;

/// A new message the user cannot currently see, reduced to what a notification
/// can show. Produced by [`notice_for`], consumed by the notification sinks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub provider: Provider,
    pub chat: ChatId,
    pub title: String,
    pub body: String,
}

/// Whether an inbound `message` warrants a notification, and if so what to say.
///
/// The gate is the same test the chat list uses to raise an unread badge, so a
/// message can never end up unread-but-quiet or notified-but-read.
///
/// `contact_name` is the chat-list name the caller has already resolved. It is
/// only the *first* choice for the title: in a group chat the sender is just a
/// member, so falling back to the sender's name would replace the group name on
/// every message.
pub fn to_notice(
    message: &Message,
    provider: Provider,
    contact_name: &str,
    is_open: bool,
) -> Option<Notice> {
    // Our own messages, and the chat the user is reading right now. Same
    // condition that drives the unread badge.
    if message.from_me || is_open {
        return None;
    }

    // The "Myself" conversation mirrors what the user sends from their phone.
    // Those arrive as inbound messages (`from_me == false`), so pinging the user
    // for their own typing would be pure noise.
    if message.chat == ChatId::Myself {
        return None;
    }

    // "Unknown" is the placeholder the backends use when a name could not be
    // resolved, and "You" is the alias for the user's own account: neither may
    // ever reach a notification. (Same rejection set as
    // `ChatState::display_name_for`.)
    let usable_name = |name: &str| !name.trim().is_empty() && name != "Unknown" && name != "You";

    let title = if usable_name(contact_name) {
        contact_name
    } else if usable_name(&message.sender) {
        message.sender.as_str()
    } else {
        provider.name()
    };

    let body = match message.text.trim().is_empty() {
        false => message.text.clone(),
        // No text: name the media so the notification is not blank.
        true => match message.media.as_ref() {
            None => String::new(),
            Some(media) => match media
                .caption
                .as_deref()
                .filter(|caption| !caption.trim().is_empty())
            {
                Some(caption) => format!("{}: {caption}", media.kind.label()),
                None => media.kind.label().to_string(),
            },
        },
    };

    Some(Notice {
        provider,
        chat: message.chat.clone(),
        title: truncated(title, MAX_TITLE_CHARS),
        body: truncated(&body, MAX_BODY_CHARS),
    })
}

/// Cut `text` to `max` characters. A notification is the one place user text
/// leaves the app, so a long message must not be able to push everything else
/// off a banner.
fn truncated(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{MediaKind, MessageId, MessageMedia};

    /// An inbound text message. Tests only vary what a notice is meant to react
    /// to, so everything else is fixed.
    fn message(chat: ChatId, sender: &str, text: &str) -> Message {
        Message {
            message_id: MessageId::from("1"),
            chat,
            sender: sender.into(),
            author_id: None,
            text: text.into(),
            timestamp: 0,
            from_me: false,
            msg_actions: Vec::new(),
            media: None,
            reply_to_id: None,
            reply_ctx: None,
            pending: false,
            failed: false,
        }
    }

    fn with_media(mut message: Message, media: MessageMedia) -> Message {
        message.media = Some(media);
        message
    }

    #[test]
    fn a_background_inbound_message_yields_a_notice() {
        let notice = to_notice(
            &message(ChatId::Telegram(1), "Alice", "hi there"),
            Provider::Telegram,
            "Alice",
            false,
        )
        .expect("a message the user cannot see is worth a notification");

        assert_eq!(notice.provider, Provider::Telegram);
        assert_eq!(notice.chat, ChatId::Telegram(1));
        assert_eq!(notice.title, "Alice");
        assert_eq!(notice.body, "hi there");
    }

    #[test]
    fn the_open_chat_is_never_notified() {
        assert!(
            to_notice(
                &message(ChatId::Telegram(1), "Alice", "hi"),
                Provider::Telegram,
                "Alice",
                true,
            )
            .is_none(),
            "the user is reading this chat, so no unread badge and no notice"
        );
    }

    #[test]
    fn own_messages_are_never_notified() {
        let mut sent = message(ChatId::Telegram(1), "Alice", "mine");
        sent.from_me = true;

        assert!(
            to_notice(&sent, Provider::Telegram, "Alice", false).is_none(),
            "a message sent from another device is still our own"
        );
    }

    #[test]
    fn the_myself_chat_is_never_notified() {
        assert!(
            to_notice(
                &message(ChatId::Myself, "Alice", "sent from my phone"),
                Provider::WhatsApp,
                "Myself",
                false,
            )
            .is_none(),
            "the user wrote this from their phone; a cue would be noise"
        );
    }

    #[test]
    fn the_chat_name_wins_over_the_sender() {
        let notice = to_notice(
            &message(
                ChatId::WhatsApp("15550000001@s.whatsapp.net".into()),
                "Carol",
                "hi",
            ),
            Provider::WhatsApp,
            "Study Group",
            false,
        )
        .expect("notified");

        assert_eq!(notice.title, "Study Group");
    }

    #[test]
    fn the_title_falls_back_through_sender_then_messenger() {
        let unresolved_chat = ChatId::Telegram(1);

        let by_sender = to_notice(
            &message(unresolved_chat.clone(), "Alice", "hi"),
            Provider::Telegram,
            // The chat list has no usable name for this chat yet.
            "",
            false,
        )
        .expect("notified");
        assert_eq!(by_sender.title, "Alice");

        for placeholder in ["Unknown", "You"] {
            let notice = to_notice(
                &message(unresolved_chat.clone(), placeholder, "hi"),
                Provider::WhatsApp,
                placeholder,
                false,
            )
            .expect("notified");
            assert_eq!(
                notice.title, "WhatsApp",
                "{placeholder:?} must never reach a notification"
            );
        }
    }

    #[test]
    fn a_media_message_without_text_names_its_media() {
        let audio = with_media(
            message(ChatId::Telegram(1), "Alice", "  "),
            MessageMedia {
                kind: MediaKind::Audio {
                    duration_secs: Some(3),
                    is_voice: true,
                    waveform: None,
                },
                caption: None,
                file_name: None,
            },
        );

        let notice = to_notice(&audio, Provider::Telegram, "Alice", false).expect("notified");
        assert_eq!(notice.body, "Audio");

        let captioned = with_media(
            message(ChatId::Telegram(1), "Alice", ""),
            MessageMedia {
                kind: MediaKind::Image,
                caption: Some("the whiteboard".into()),
                file_name: None,
            },
        );

        let notice = to_notice(&captioned, Provider::Telegram, "Alice", false).expect("notified");
        assert_eq!(notice.body, "Image: the whiteboard");
    }

    #[test]
    fn a_message_with_neither_text_nor_media_has_an_empty_body() {
        let notice = to_notice(
            &message(ChatId::Telegram(1), "Alice", ""),
            Provider::Telegram,
            "Alice",
            false,
        )
        .expect("still notified");

        assert_eq!(notice.body, "");
    }

    #[test]
    fn long_text_is_cut_on_a_character_boundary() {
        let long = "á".repeat(MAX_BODY_CHARS + 50);
        let notice = to_notice(
            &message(ChatId::Telegram(1), "Alice", &long),
            Provider::Telegram,
            &long,
            false,
        )
        .expect("notified");

        assert_eq!(notice.body.chars().count(), MAX_BODY_CHARS);
        assert_eq!(notice.title.chars().count(), MAX_TITLE_CHARS);
        assert!(notice.body.chars().all(|c| c == 'á'));
    }
}
