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

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use crate::backend::{ChatId, Message, MessageId, Provider};
use crate::config::NotificationsConfig;
use crate::tui::PlayKey;

/// Longest title a notice carries, in characters.
const MAX_TITLE_CHARS: usize = 60;
/// Longest body a notice carries, in characters.
const MAX_BODY_CHARS: usize = 140;
/// How many recently seen messages are remembered for deduplication. Bounded
/// because a long session would otherwise grow it without limit; far more than
/// the duplicates any provider actually redelivers.
const RECENT_IDS: usize = 64;

/// A new message the user cannot currently see, reduced to what a notification
/// can show. Produced by [`to_notice`], consumed by the notification sinks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub provider: Provider,
    pub chat: ChatId,
    /// Carries the message's identity so a provider that delivers the same
    /// message twice cannot notify twice. Opaque and provider-neutral, like
    /// every other id in the backend layer.
    pub message_id: MessageId,
    /// Chat name, else the sender, else the messenger name.
    pub title: String,
    /// The message text, else a label for its media. Empty when the message
    /// carries neither (a service message with no text): a titled notification
    /// with no body is still meaningful, and the sound cue ignores it anyway.
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
        message_id: message.message_id.clone(),
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

/// Turns notices into notification cues, applying the suppression rules.
///
/// Suppression lives here rather than in the audio worker on purpose: it keeps
/// `AppState` about *policy* ("a background message yields a notice") and the
/// player about *sound*, with no shared state between them. A cue that is
/// dropped is a `debug!` line, never a popup and never a counter.
pub struct Notifier {
    /// The global sound switch. A messenger also needs its own entry in
    /// `sounds`: a messenger absent from it is muted.
    sound: bool,
    /// Cue bytes per messenger, read once at construction.
    sounds: HashMap<Provider, Arc<[u8]>>,
    /// How long one messenger stays quiet after cueing. Zero cues on every
    /// message.
    debounce: Duration,
    /// Recently cued messages, oldest first. Providers redeliver messages
    /// (WhatsApp emits `MessageReceived` even for a duplicate it has already
    /// stored), so the same id arriving twice must notify once.
    recent: VecDeque<PlayKey>,
    /// When each messenger last cued.
    last_cue: HashMap<Provider, Instant>,
}

impl Notifier {
    /// Read every configured sound file once, so a typo'd path is one log line
    /// at startup instead of a failed read per incoming message. An unreadable
    /// path mutes that messenger and nothing else.
    pub fn new(config: &NotificationsConfig) -> Self {
        let mut sounds = HashMap::new();

        for provider in Provider::all() {
            let provider = *provider;
            // Absent or blank is the same thing: this messenger is silent.
            let Some(path) = config.sound_for(provider) else {
                continue;
            };

            match std::fs::read(path) {
                Ok(bytes) => {
                    debug!(?provider, bytes = bytes.len(), "notification sound loaded");
                    sounds.insert(provider, Arc::from(bytes.into_boxed_slice()));
                }
                Err(err) => warn!(
                    ?provider,
                    path = %path.display(),
                    error = %err,
                    "notification sound could not be read; this messenger will be silent"
                ),
            }
        }

        Self {
            sound: config.sound,
            sounds,
            debounce: Duration::from_millis(config.debounce_ms),
            recent: VecDeque::new(),
            last_cue: HashMap::new(),
        }
    }

    /// The cue to play for `notice`, or `None` if this message stays silent.
    ///
    /// `now` is passed in rather than read here so the cooldown is testable
    /// without sleeping.
    pub fn cue(&mut self, notice: &Notice, now: Instant) -> Option<Arc<[u8]>> {
        if !self.sound {
            return None;
        }

        // No file for this messenger: muted, whether that means the user never
        // configured one or the file could not be read.
        let bytes = self.sounds.get(&notice.provider)?.clone();

        let key = PlayKey {
            chat: notice.chat.clone(),
            message_id: notice.message_id.clone(),
        };

        // Remember the id *before* the cooldown check, so a message suppressed
        // by the cooldown is still deduplicated: a redelivery of it must not
        // sneak a cue through once the window closes.
        if self.recent.contains(&key) {
            debug!(?key, "notification suppressed: message already announced");
            return None;
        }

        self.recent.push_back(key);

        if self.recent.len() > RECENT_IDS {
            self.recent.pop_front();
        }

        if let Some(last) = self.last_cue.get(&notice.provider)
            && now.duration_since(*last) < self.debounce
        {
            debug!(
                ?notice.provider,
                "notification suppressed: inside the per-messenger cooldown"
            );
            return None;
        }

        self.last_cue.insert(notice.provider, now);
        Some(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{MediaKind, MessageId, MessageMedia};
    use std::path::{Path, PathBuf};

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

    /// An inbound message with an explicit id, for the deduplication rules.
    fn message_from(chat: ChatId, id: &str) -> Message {
        let mut message = message(chat, "Alice", "hi");
        message.message_id = MessageId::from(id);
        message
    }

    /// The repo's own one-second wav, standing in for a user's sound file.
    fn tone_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tui/player/engine/fixtures/tone.wav")
    }

    fn notifier(sound: bool, debounce_ms: u64, sounds: &[(Provider, PathBuf)]) -> Notifier {
        Notifier::new(&NotificationsConfig {
            os: false,
            sound,
            debounce_ms,
            sounds: sounds.iter().map(|(p, path)| (*p, path.clone())).collect(),
        })
    }

    fn notice_for(chat: ChatId, id: &str) -> Notice {
        notice_from(Provider::Telegram, chat, id)
    }

    fn notice_from(provider: Provider, chat: ChatId, id: &str) -> Notice {
        let notice = to_notice(&message_from(chat, id), provider, "Alice", false)
            .expect("a background message is worth a notice");
        assert_eq!(notice.provider, provider);
        notice
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

    #[test]
    fn the_sound_switch_mutes_every_messenger() {
        let mut notifier = notifier(false, 0, &[(Provider::Telegram, tone_path())]);

        assert!(
            notifier
                .cue(&notice_for(ChatId::Telegram(1), "a"), Instant::now())
                .is_none(),
            "sound = false must silence a configured messenger too"
        );
    }

    #[test]
    fn a_messenger_without_a_sound_file_is_muted() {
        let mut notifier = notifier(true, 0, &[(Provider::Telegram, tone_path())]);

        assert!(
            notifier
                .cue(
                    &notice_from(Provider::WhatsApp, ChatId::Telegram(1), "a"),
                    Instant::now()
                )
                .is_none(),
            "a messenger with no configured sound stays silent"
        );
        assert!(
            notifier
                .cue(
                    &notice_from(Provider::Telegram, ChatId::Telegram(2), "b"),
                    Instant::now()
                )
                .is_some(),
            "one messenger's sound must not depend on another's"
        );
    }

    #[test]
    fn an_unreadable_sound_file_mutes_only_its_own_messenger() {
        let mut notifier = notifier(
            true,
            0,
            &[
                (
                    Provider::Telegram,
                    PathBuf::from("/nonexistent/sender/tone.wav"),
                ),
                (Provider::WhatsApp, tone_path()),
            ],
        );

        assert!(
            notifier
                .cue(&notice_for(ChatId::Telegram(1), "a"), Instant::now())
                .is_none(),
            "a path that could not be read leaves that messenger silent"
        );
    }

    #[test]
    fn a_redelivered_message_cues_once() {
        let mut notifier = notifier(true, 0, &[(Provider::Telegram, tone_path())]);
        let now = Instant::now();

        assert!(
            notifier
                .cue(&notice_for(ChatId::Telegram(1), "dup"), now)
                .is_some(),
            "the first delivery announces"
        );
        assert!(
            notifier
                .cue(&notice_for(ChatId::Telegram(1), "dup"), now)
                .is_none(),
            "providers redeliver messages; the second copy must stay silent"
        );
    }

    #[test]
    fn a_burst_within_the_window_collapses_to_one_cue() {
        let mut notifier = notifier(true, 1500, &[(Provider::Telegram, tone_path())]);
        let start = Instant::now();

        assert!(
            notifier
                .cue(&notice_for(ChatId::Telegram(1), "a"), start)
                .is_some()
        );
        for (n, id) in ["b", "c", "d"].iter().enumerate() {
            let at = start + Duration::from_millis(200 * (n as u64 + 1));
            assert!(
                notifier
                    .cue(&notice_for(ChatId::Telegram(1), id), at)
                    .is_none(),
                "a group dump must not become a cue storm"
            );
        }

        // Past the window, a new message is news again.
        assert!(
            notifier
                .cue(
                    &notice_for(ChatId::Telegram(1), "e"),
                    start + Duration::from_millis(1500)
                )
                .is_some()
        );
    }

    #[test]
    fn a_zero_window_cues_every_message() {
        let mut notifier = notifier(true, 0, &[(Provider::Telegram, tone_path())]);
        let now = Instant::now();

        for id in ["a", "b", "c"] {
            assert!(
                notifier
                    .cue(&notice_for(ChatId::Telegram(1), id), now)
                    .is_some(),
                "debounce_ms = 0 opts out of coalescing"
            );
        }
    }

    #[test]
    fn the_window_is_per_messenger() {
        let mut notifier = notifier(
            true,
            1500,
            &[
                (Provider::Telegram, tone_path()),
                (Provider::WhatsApp, tone_path()),
            ],
        );
        let now = Instant::now();

        let whatsapp = notice_from(Provider::WhatsApp, ChatId::Telegram(2), "wa");

        assert!(
            notifier
                .cue(&notice_for(ChatId::Telegram(1), "tg"), now)
                .is_some()
        );
        assert!(
            notifier.cue(&whatsapp, now).is_some(),
            "Telegram's cue must not consume WhatsApp's window"
        );
        assert!(
            notifier
                .cue(&notice_for(ChatId::Telegram(3), "tg2"), now)
                .is_none(),
            "Telegram is still inside its own window"
        );
    }

    #[test]
    fn the_remembered_ids_are_bounded() {
        let mut notifier = notifier(true, 0, &[(Provider::Telegram, tone_path())]);
        let now = Instant::now();

        for n in 0..=RECENT_IDS {
            let id = format!("m{n}");
            assert!(
                notifier
                    .cue(&notice_for(ChatId::Telegram(1), &id), now)
                    .is_some()
            );
        }

        // The very first id has been pushed out of the ring, so a redelivery of
        // it is treated as new rather than growing the ring forever.
        assert!(
            notifier
                .cue(&notice_for(ChatId::Telegram(1), "m0"), now)
                .is_some()
        );
    }
}
