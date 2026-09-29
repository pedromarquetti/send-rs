//! Notification payloads: what a new message is reduced to before it reaches a
//! notification sink (a sound cue, an OS notification).
//!
//! This module owns the *policy* — which messages are worth telling the user
//! about, and what to say — plus the two sinks it decides between: cue bytes for
//! the player, and an [`OsNotifier`] for the desktop. Firing a sink is still the
//! caller's job, so nothing here touches the audio device, the terminal, or a
//! blocking platform call.
//!
//! [`Notice`] is deliberately provider-neutral: a [`Provider`] tag for routing
//! and a display title, never a JID, a peer id or anything else a provider
//! invented. That is also why the provider is carried *alongside* the chat
//! rather than derived from it: `ChatId::to_provider` panics for the "Myself"
//! conversation, which must never be a notification target.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use notify_rust::Notification;
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

/// A platform notification sink: whatever the desktop shows when the user is
/// looking at something else.
///
/// `show()` on a desktop notification API is a blocking round trip to the
/// session bus (or the platform equivalent), so the *caller* is responsible for
/// running this off the event loop. Keeping the trait synchronous and free of
/// any runtime is what lets the policy above be tested without a bus.
pub trait OsNotifier: Send + Sync {
    /// Show `notice` to the user.
    ///
    /// `played_cue` is true when this notice already produced a sound of its
    /// own, in which case the platform must stay silent: one message, one noise.
    fn notify(&self, notice: &Notice, played_cue: bool) -> Result<(), String>;
}

/// The real sink, picked by the compile target: a freedesktop toast over D-Bus,
/// a macOS banner, or a Windows action-center notification.
pub struct DesktopNotifier;

impl OsNotifier for DesktopNotifier {
    fn notify(&self, notice: &Notice, played_cue: bool) -> Result<(), String> {
        let mut notification = Notification::new();
        notification
            .appname("sender")
            .summary(&notice.title)
            .body(&notice.body);

        if played_cue {
            // Empty sound name on every platform cfg (an XDG `SoundName` hint on
            // Linux, the notification sound on macOS/Windows), so the cue we
            // already played is not repeated by the desktop.
            notification.sound_name("");
        }

        notification
            .show()
            .map(|_| ())
            .map_err(|err| err.to_string())
    }
}

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
/// `visible` is the caller's answer to "is the user looking at this right now":
/// true only when the chat is open *and* the window has the focus. A visible
/// message is never announced, because the user is already reading it.
///
/// This is deliberately **not** the same test the chat list uses to raise an
/// unread badge. The badge asks only whether the chat is open, because an open
/// chat puts the message on screen as soon as the user returns. The notification
/// also has to consider whether the user is at the terminal at all, so a message
/// in the open chat is still announced when the window is unfocused — while
/// staying unbadged, since it is already in the open chat's history.
///
/// `contact_name` is the chat-list name the caller has already resolved. It is
/// only the *first* choice for the title: in a group chat the sender is just a
/// member, so falling back to the sender's name would replace the group name on
/// every message.
pub fn to_notice(
    message: &Message,
    provider: Provider,
    contact_name: &str,
    visible: bool,
) -> Option<Notice> {
    // Our own messages, and the ones the user is looking at right now.
    if message.from_me || visible {
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
    /// The global desktop switch, independent of `sound`: either channel can be
    /// used alone.
    os: bool,
    /// The desktop sink, behind the trait so tests can observe what would have
    /// been shown without a session bus.
    desktop: Arc<dyn OsNotifier>,
    /// Cue bytes per messenger, read once at construction.
    sounds: HashMap<Provider, Arc<[u8]>>,
    /// How long one messenger stays quiet after announcing. Zero announces
    /// every message.
    debounce: Duration,
    /// Recently announced messages, oldest first. Providers redeliver messages
    /// (WhatsApp emits `MessageReceived` even for a duplicate it has already
    /// stored), so the same id arriving twice must notify once.
    recent: VecDeque<PlayKey>,
    /// When each messenger last announced.
    last_announce: HashMap<Provider, Instant>,
}

/// What one [`Notice`] should produce, and the sinks to produce it with.
///
/// Both channels are decided together and returned together: they are
/// independent switches, so `sound` alone, `os` alone and both at once are all
/// normal configurations, and a notice may have to feed both. `None` on either
/// field means *that channel* stays silent for this notice — which is not the
/// same as the whole notice being suppressed.
pub struct Dispatch {
    /// The desktop sink to notify, or `None` when the OS channel is off or the
    /// notice was suppressed.
    pub os: Option<Arc<dyn OsNotifier>>,
    /// Cue bytes for the player, or `None` when the sound channel is off, this
    /// messenger has no configured file, or the notice was suppressed.
    pub cue: Option<Arc<[u8]>>,
}

impl Notifier {
    /// Read every configured sound file once, so a typo'd path is one log line
    /// at startup instead of a failed read per incoming message. An unreadable
    /// path mutes that messenger's sound and nothing else.
    pub fn new(config: &NotificationsConfig, desktop: Arc<dyn OsNotifier>) -> Self {
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
            os: config.os,
            desktop,
            sounds,
            debounce: Duration::from_millis(config.debounce_ms),
            recent: VecDeque::new(),
            last_announce: HashMap::new(),
        }
    }

    /// Decide what `notice` should produce: which sinks fire, and with what.
    ///
    /// This is the single entry point for both channels, so the deduplication
    /// and cooldown rules below cannot drift apart between them — a burst that
    /// is coalesced into one cue is also coalesced into one desktop
    /// notification.
    ///
    /// `now` is passed in rather than read here so the cooldown is testable
    /// without sleeping.
    pub fn dispatch(&mut self, notice: &Notice, now: Instant) -> Dispatch {
        // Both channels off: touch no state at all, so ids are not burned
        // against the deduplication ring while nothing can be announced.
        if !self.sound && !self.os {
            return Dispatch {
                os: None,
                cue: None,
            };
        }

        let key = PlayKey {
            chat: notice.chat.clone(),
            message_id: notice.message_id.clone(),
        };

        // Remember the id *before* the cooldown check, so a message suppressed
        // by the cooldown is still deduplicated: a redelivery of it must not
        // sneak an announcement through once the window closes.
        if self.recent.contains(&key) {
            debug!(?key, "notification suppressed: message already announced");
            return Dispatch {
                os: None,
                cue: None,
            };
        }

        self.recent.push_back(key);

        if self.recent.len() > RECENT_IDS {
            self.recent.pop_front();
        }

        if let Some(last) = self.last_announce.get(&notice.provider)
            && now.duration_since(*last) < self.debounce
        {
            debug!(
                ?notice.provider,
                "notification suppressed: inside the per-messenger cooldown"
            );

            return Dispatch {
                os: None,
                cue: None,
            };
        }

        self.last_announce.insert(notice.provider, now);

        Dispatch {
            os: self.os.then(|| Arc::clone(&self.desktop)),
            // No file for this messenger mutes the *sound* only: the desktop
            // notification still goes out.
            cue: if self.sound {
                self.sounds.get(&notice.provider).cloned()
            } else {
                None
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{MediaKind, MessageId, MessageMedia};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

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

    /// Records what would have been shown to the desktop, so the OS channel can
    /// be asserted without a session bus. `played_cue` is recorded too, since
    /// "we already made the noise" is the one thing the platform must not
    /// second-guess.
    #[derive(Default)]
    struct StubOsNotifier {
        shown: Mutex<Vec<(Notice, bool)>>,
    }

    impl OsNotifier for StubOsNotifier {
        fn notify(&self, notice: &Notice, played_cue: bool) -> Result<(), String> {
            self.shown
                .lock()
                .expect("stub not poisoned")
                .push((notice.clone(), played_cue));
            Ok(())
        }
    }

    /// A notifier plus a handle on the stub standing in for the desktop, so a
    /// test can assert on both the policy and what it would have shown.
    fn notifier(
        os: bool,
        sound: bool,
        debounce_ms: u64,
        sounds: &[(Provider, PathBuf)],
    ) -> (Notifier, Arc<StubOsNotifier>) {
        let desktop = Arc::new(StubOsNotifier::default());
        let notifier = Notifier::new(
            &NotificationsConfig {
                os,
                sound,
                debounce_ms,
                sounds: sounds.iter().map(|(p, path)| (*p, path.clone())).collect(),
            },
            desktop.clone(),
        );
        (notifier, desktop)
    }

    /// Fire a dispatch the way the event loop does, so the tests exercise the
    /// same two steps the UI takes.
    fn fire(dispatch: &Dispatch, notice: &Notice) {
        if let Some(desktop) = &dispatch.os {
            desktop
                .notify(notice, dispatch.cue.is_some())
                .expect("stub");
        }
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
    fn a_visible_message_is_never_notified() {
        assert!(
            to_notice(
                &message(ChatId::Telegram(1), "Alice", "hi"),
                Provider::Telegram,
                "Alice",
                true,
            )
            .is_none(),
            "the user is looking at this chat, so there is nothing to announce"
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
        let (mut notifier, desktop) =
            notifier(true, false, 0, &[(Provider::Telegram, tone_path())]);
        let notice = notice_for(ChatId::Telegram(1), "a");

        let dispatch = notifier.dispatch(&notice, Instant::now());
        assert!(
            dispatch.cue.is_none(),
            "sound = false must silence a configured messenger too"
        );
        assert!(
            dispatch.os.is_some(),
            "the switches are independent: os = true must still notify"
        );
        fire(&dispatch, &notice);
        assert!(
            !desktop.shown.lock().expect("stub not poisoned")[0].1,
            "no cue was played, so the desktop must not be told to stay silent"
        );
    }

    #[test]
    fn a_messenger_without_a_sound_file_is_muted() {
        let (mut notifier, _) = notifier(false, true, 0, &[(Provider::Telegram, tone_path())]);
        let now = Instant::now();

        assert!(
            notifier
                .dispatch(
                    &notice_from(Provider::WhatsApp, ChatId::Telegram(1), "a"),
                    now
                )
                .cue
                .is_none(),
            "a messenger with no configured sound stays silent"
        );
        assert!(
            notifier
                .dispatch(
                    &notice_from(Provider::Telegram, ChatId::Telegram(2), "b"),
                    now
                )
                .cue
                .is_some(),
            "one messenger's sound must not depend on another's"
        );
    }

    #[test]
    fn an_unreadable_sound_file_mutes_only_its_own_messenger() {
        let (mut notifier, desktop) = notifier(
            true,
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
        let notice = notice_for(ChatId::Telegram(1), "a");

        let dispatch = notifier.dispatch(&notice, Instant::now());
        assert!(
            dispatch.cue.is_none(),
            "a path that could not be read leaves that messenger's sound silent"
        );
        fire(&dispatch, &notice);
        assert_eq!(
            desktop.shown.lock().expect("stub not poisoned").len(),
            1,
            "a missing sound file must not silence the desktop notification"
        );
    }

    #[test]
    fn a_redelivered_message_is_announced_once() {
        let (mut notifier, desktop) = notifier(true, true, 0, &[(Provider::Telegram, tone_path())]);
        let notice = notice_for(ChatId::Telegram(1), "dup");
        let now = Instant::now();

        let first = notifier.dispatch(&notice, now);
        assert!(first.cue.is_some(), "the first delivery announces");
        fire(&first, &notice);

        let second = notifier.dispatch(&notice, now);
        assert!(
            second.cue.is_none() && second.os.is_none(),
            "providers redeliver messages; the second copy must stay silent on both channels"
        );
        assert_eq!(
            desktop.shown.lock().expect("stub not poisoned").len(),
            1,
            "a deduped id must never reach the desktop notifier either"
        );
    }

    #[test]
    fn a_burst_within_the_window_collapses_into_one_announcement() {
        let (mut notifier, desktop) =
            notifier(true, true, 1500, &[(Provider::Telegram, tone_path())]);
        let start = Instant::now();

        let first = notice_for(ChatId::Telegram(1), "a");
        let dispatch = notifier.dispatch(&first, start);
        assert!(dispatch.cue.is_some() && dispatch.os.is_some());
        fire(&dispatch, &first);

        for (n, id) in ["b", "c", "d"].iter().enumerate() {
            let at = start + Duration::from_millis(200 * (n as u64 + 1));
            let dispatch = notifier.dispatch(&notice_for(ChatId::Telegram(1), id), at);
            assert!(
                dispatch.cue.is_none() && dispatch.os.is_none(),
                "a group dump must not become a notification storm on either channel"
            );
        }

        // Past the window, a new message is news again.
        assert!(
            notifier
                .dispatch(
                    &notice_for(ChatId::Telegram(1), "e"),
                    start + Duration::from_millis(1500)
                )
                .cue
                .is_some()
        );
        assert_eq!(
            desktop.shown.lock().expect("stub not poisoned").len(),
            1,
            "the cooldown coalesces the desktop channel exactly like the cue"
        );
    }

    #[test]
    fn a_zero_window_announces_every_message() {
        let (mut notifier, desktop) = notifier(true, true, 0, &[(Provider::Telegram, tone_path())]);
        let now = Instant::now();

        for id in ["a", "b", "c"] {
            let notice = notice_for(ChatId::Telegram(1), id);
            let dispatch = notifier.dispatch(&notice, now);
            assert!(
                dispatch.cue.is_some(),
                "debounce_ms = 0 opts out of coalescing"
            );
            fire(&dispatch, &notice);
        }

        assert_eq!(
            desktop.shown.lock().expect("stub not poisoned").len(),
            3,
            "an opted-out cooldown must not mute the desktop either"
        );
    }

    #[test]
    fn the_window_is_per_messenger() {
        let (mut notifier, _) = notifier(
            false,
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
                .dispatch(&notice_for(ChatId::Telegram(1), "tg"), now)
                .cue
                .is_some()
        );
        assert!(
            notifier.dispatch(&whatsapp, now).cue.is_some(),
            "Telegram's announcement must not consume WhatsApp's window"
        );
        assert!(
            notifier
                .dispatch(&notice_for(ChatId::Telegram(3), "tg2"), now)
                .cue
                .is_none(),
            "Telegram is still inside its own window"
        );
    }

    #[test]
    fn the_remembered_ids_are_bounded() {
        let (mut notifier, _) = notifier(false, true, 0, &[(Provider::Telegram, tone_path())]);
        let now = Instant::now();

        for n in 0..=RECENT_IDS {
            let id = format!("m{n}");
            assert!(
                notifier
                    .dispatch(&notice_for(ChatId::Telegram(1), &id), now)
                    .cue
                    .is_some()
            );
        }

        // The very first id has been pushed out of the ring, so a redelivery of
        // it is treated as new rather than growing the ring forever.
        assert!(
            notifier
                .dispatch(&notice_for(ChatId::Telegram(1), "m0"), now)
                .cue
                .is_some()
        );
    }

    #[test]
    fn both_switches_off_announces_nothing_and_remembers_nothing() {
        let (mut off, desktop) = notifier(false, false, 0, &[(Provider::Telegram, tone_path())]);
        let notice = notice_for(ChatId::Telegram(1), "a");

        let dispatch = off.dispatch(&notice, Instant::now());
        assert!(dispatch.cue.is_none() && dispatch.os.is_none());
        fire(&dispatch, &notice);
        assert!(desktop.shown.lock().expect("stub not poisoned").is_empty());

        // A notifier built with notifications on must still see this id as
        // new: a dispatch with both channels off must not burn the id against
        // the deduplication ring.
        let (mut enabled, _) = notifier(true, true, 0, &[(Provider::Telegram, tone_path())]);
        assert!(
            enabled.dispatch(&notice, Instant::now()).cue.is_some(),
            "an id must not be spent while notifications were off"
        );
    }

    #[test]
    fn a_played_cue_tells_the_desktop_to_stay_silent() {
        let (mut notifier, desktop) = notifier(true, true, 0, &[(Provider::Telegram, tone_path())]);
        let notice = notice_for(ChatId::Telegram(1), "a");

        let dispatch = notifier.dispatch(&notice, Instant::now());
        assert!(dispatch.cue.is_some() && dispatch.os.is_some());
        fire(&dispatch, &notice);

        let shown = desktop.shown.lock().expect("stub not poisoned");
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].0, notice, "the desktop shows the notice itself");
        assert!(
            shown[0].1,
            "we played the cue, so the desktop must not make a second noise"
        );
    }

    #[test]
    fn the_os_switch_alone_needs_no_sound_file() {
        // The whole point of the two switches being independent: notifications
        // with no audio configured at all, on a messenger with no cue file.
        let (mut notifier, desktop) = notifier(true, false, 0, &[]);
        let notice = notice_for(ChatId::Telegram(1), "a");

        let dispatch = notifier.dispatch(&notice, Instant::now());
        assert!(dispatch.cue.is_none() && dispatch.os.is_some());
        fire(&dispatch, &notice);
        assert_eq!(desktop.shown.lock().expect("stub not poisoned").len(), 1);
    }
}
