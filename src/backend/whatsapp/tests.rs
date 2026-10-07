use crate::helpers::now;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};
use whatsapp_rust::prelude::{Event, Jid, MessageBuilderExt, MessageInfo, wa};
use whatsapp_rust::transport::DisconnectReason;
use whatsapp_rust::types::events::{
    ArchiveUpdate, BatchOrigin, ClearChatUpdate, ConnectFailureReason, Connected,
    DeleteMessageForMeUpdate, Disconnected, InboundMessage, LoggedOut, MarkChatAsReadUpdate,
    MessageBatch, MuteUpdate, PairingQrCode, PresenceUpdate,
};
use whatsapp_rust::wacore::download::MediaType;
use whatsapp_rust::wacore::types::message::EditAttribute;

use super::convert::{
    format_pn, message_actions, name_from_lid_pn, resolve_conversation_name, resolve_sender_name,
    to_senders_msg,
};
use super::ids::{canonical_chat_id, canonical_jid_chat_id, fold_lid_key, own_chat_id};
use super::media::{MediaRef, handle_wa_media, ref_media_type, wa_media_ref};
use super::messenger::WhatsAppMessenger;
use super::state::{SharedState, WhatsAppState};
use super::sync::{
    CdnFields, handle_history_sync, outbound_media_message, presence_label,
    should_skip_conversation,
};
use crate::backend::{
    BackendError, BackendEvent, Chat, ChatId, MediaKind, Message, MessageAction, MessageId,
    MessageMedia, Messenger,
};
use chrono::TimeZone;
use std::time::Duration;
use whatsapp_rust::prelude::MessageField;
use whatsapp_rust::wacore::types::message::MessageSource;

fn pn(num: &str) -> Jid {
    Jid::pn(num)
}

fn text_message(text: &str) -> wa::Message {
    wa::Message::text(text.to_string())
}

fn msg_info(chat: &Jid, sender: &Jid, push_name: &str, id: &str, from_me: bool) -> MessageInfo {
    MessageInfo {
        source: MessageSource {
            chat: chat.clone(),
            sender: sender.clone(),
            is_from_me: from_me,
            ..Default::default()
        },
        id: id.into(),
        push_name: push_name.into(),
        timestamp: chrono::Utc.timestamp_opt(1000, 0).unwrap(),
        ..Default::default()
    }
}

fn build_msg(id: &str, text: &str, chat: &ChatId, from_me: bool) -> Message {
    Message {
        message_id: MessageId(id.to_string()),
        chat_id: chat.clone(),
        sender: "Alice".into(),
        author_id: None,
        text: text.to_string(),
        timestamp: 1000,
        from_me,
        msg_actions: message_actions(from_me),
        media: None,
        reply_to_id: None,
        reply_ctx: None,
        pending: false,
        failed: false,
    }
}

/// Build a HistorySync message record mirroring the shape the bot decodes.
fn web_msg(
    chat: &str,
    participant: Option<&str>,
    id: &str,
    ts: u64,
    from_me: bool,
    push_name: &str,
    text: &str,
) -> wa::HistorySyncMsg {
    wa::HistorySyncMsg {
        message: MessageField::some(wa::WebMessageInfo {
            key: MessageField::some(wa::MessageKey {
                id: Some(id.to_string()),
                remote_jid: Some(chat.to_string()),
                from_me: Some(from_me),
                participant: participant.map(str::to_string),
            }),
            message: MessageField::some(wa::Message::text(text.to_string())),
            message_timestamp: Some(ts),
            push_name: Some(push_name.to_string()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn conversation(id: &str, messages: Vec<wa::HistorySyncMsg>) -> wa::Conversation {
    wa::Conversation {
        id: id.to_string(),
        messages,
        ..Default::default()
    }
}

#[test]
fn skip_filter_drops_ghost_lid_rows() {
    let ghost = wa::Conversation {
        id: "255202829570287@lid".to_string(),
        ..Default::default()
    };
    assert!(
        should_skip_conversation(&ghost).is_some(),
        "never-started LID row with no pn is a ghost"
    );

    // A real LID chat has thread metadata and/or a peer phone number.
    let real_lid = wa::Conversation {
        id: "255202829570287@lid".to_string(),
        pn_jid: Some("15550000001@s.whatsapp.net".to_string()),
        ..Default::default()
    };
    assert!(should_skip_conversation(&real_lid).is_none());

    let real_pn = wa::Conversation {
        id: "15550000002@s.whatsapp.net".to_string(),
        ..Default::default()
    };
    assert!(should_skip_conversation(&real_pn).is_none());

    // A group the account left (read_only) is not a chat anymore.
    let left_group = wa::Conversation {
        id: "111222333@g.us".to_string(),
        read_only: Some(true),
        ..Default::default()
    };
    assert!(should_skip_conversation(&left_group).is_some());

    // A live joined group is kept.
    let joined_group = wa::Conversation {
        id: "111222333@g.us".to_string(),
        read_only: Some(false),
        ..Default::default()
    };
    assert!(should_skip_conversation(&joined_group).is_none());

    // pnh duplicate / marketing / suspended / parent community are junk.
    for conv in [
        wa::Conversation {
            id: "255202829570287@lid".to_string(),
            pnh_duplicate_lid_thread: Some(true),
            ..Default::default()
        },
        wa::Conversation {
            id: "15550000003@s.whatsapp.net".to_string(),
            is_marketing_message_thread: Some(true),
            ..Default::default()
        },
        wa::Conversation {
            id: "15550000003@s.whatsapp.net".to_string(),
            suspended: Some(true),
            ..Default::default()
        },
    ] {
        assert!(should_skip_conversation(&conv).is_some());
    }
}

#[test]
fn lid_conversation_resolves_name_via_pn_jid() {
    let mut pushnames = HashMap::new();
    pushnames.insert(
        "15550000001@s.whatsapp.net".to_string(),
        "Alice".to_string(),
    );
    let empty = HashMap::new();
    let no_verified = HashSet::new();

    // LID-keyed thread; the contact name must come from the PN pushname.
    let lid = wa::Conversation {
        id: "255202829570287@lid".to_string(),
        pn_jid: Some("15550000001@s.whatsapp.net".to_string()),
        ..Default::default()
    };
    assert_eq!(
        resolve_conversation_name(&lid, &pushnames, &empty, &no_verified),
        ("Alice".to_string(), false),
        "LID chat resolves through pn_jid"
    );

    // PN-keyed thread without a name/display_name falls back to pushname.
    let pn_chat = wa::Conversation {
        id: "15550000001@s.whatsapp.net".to_string(),
        ..Default::default()
    };
    assert_eq!(
        resolve_conversation_name(&pn_chat, &pushnames, &empty, &no_verified),
        ("Alice".to_string(), false)
    );

    // A LID thread with no PN, no pushname and no usync name now falls
    // back to the formatted phone number when usync supplied it.
    let mut usync = HashMap::new();
    usync.insert(
        "15550000004@lid".to_string(),
        "+15 55 0000-0004".to_string(),
    );
    let lid_learned = wa::Conversation {
        id: "15550000004@lid".to_string(),
        ..Default::default()
    };
    assert_eq!(
        resolve_conversation_name(&lid_learned, &pushnames, &usync, &no_verified),
        ("+15 55 0000-0004".to_string(), false)
    );

    // A verified business peer: name resolves from usync with verified=true.
    usync.insert(
        "15550000003@s.whatsapp.net".to_string(),
        "ACME Bots".to_string(),
    );
    let mut verified = HashSet::new();
    verified.insert("15550000003@s.whatsapp.net".to_string());
    let biz = wa::Conversation {
        id: "15550000005@lid".to_string(),
        pn_jid: Some("15550000003@s.whatsapp.net".to_string()),
        ..Default::default()
    };
    assert_eq!(
        resolve_conversation_name(&biz, &pushnames, &usync, &verified),
        ("ACME Bots".to_string(), true),
        "usync entry found via pn_jid overrides the raw LID"
    );

    // A LID thread with no PN, no pushname, no usync name and no phone
    // fallback keeps the bare LID.
    let bare = wa::Conversation {
        id: "255202829570287@lid".to_string(),
        ..Default::default()
    };
    assert_eq!(
        resolve_conversation_name(&bare, &pushnames, &empty, &no_verified),
        ("255202829570287".to_string(), false)
    );
}

#[test]
fn format_pn_matches_whatsapp_layout() {
    assert_eq!(format_pn("15550000003"), "+15 55 000-0003");
    assert_eq!(format_pn("15551234567"), "+15 55 123-4567");
    assert_eq!(format_pn("123"), "+123");
    assert_eq!(format_pn("15550000004@lid"), "+15 55 000-0004");
}

#[test]
fn name_from_lid_pn_formats_numbers_for_lid_keyed_chats() {
    // Fake LID↔PN mappings learned locally from message traffic, with no
    // push name cached: the formatted number must be the fallback so a
    // LID-keyed chat never shows as a bare LID.
    let empty = HashMap::new();
    let cases = [
        ("15550000001", "+15 55 000-0001"),
        ("15550000002", "+15 55 000-0002"),
        ("15550000003", "+15 55 000-0003"),
        ("15550000004", "+15 55 000-0004"),
    ];
    for (pn, expected) in cases {
        assert_eq!(name_from_lid_pn(pn, &empty), expected, "for {pn}");
    }
}

#[test]
fn name_from_lid_pn_prefers_synced_pushname() {
    let mut pushnames = HashMap::new();
    pushnames.insert(
        "15550000002@s.whatsapp.net".to_string(),
        "Test Pushname".to_string(),
    );
    assert_eq!(name_from_lid_pn("15550000002", &pushnames), "Test Pushname");
}

#[test]
fn upsert_keeps_known_group_title_when_sync_carries_raw_jid() {
    let mut st = WhatsAppState::default();
    let group = "15550000001-111222333@g.us";
    let chat_id = ChatId::jid_to_chat_id(group);

    // Full sync record with the real subject.
    let titled = wa::Conversation {
        id: group.to_string(),
        name: Some("Teste".to_string()),
        ..Default::default()
    };

    st.upsert_conversation(&titled, chat_id.clone());
    assert_eq!(st.chats[0].contact_name, "Teste");

    // Periodic sync record whose subject is the raw JID (empty on the
    // phone): must not clobber the known title.
    let raw = wa::Conversation {
        id: group.to_string(),
        name: Some(group.to_string()),
        ..Default::default()
    };
    st.upsert_conversation(&raw, chat_id.clone());
    assert_eq!(
        st.chats[0].contact_name, "Teste",
        "a raw-JID subject must not downgrade the group title"
    );

    // A genuinely new group without a subject renders its JID until a
    // real title arrives.
    let new_group = "111222333444@g.us";
    let fresh = wa::Conversation {
        id: new_group.to_string(),
        name: Some(new_group.to_string()),
        ..Default::default()
    };
    st.upsert_conversation(&fresh, ChatId::jid_to_chat_id(new_group));
    assert_eq!(st.chats[1].contact_name, new_group);
}

#[test]
fn upsert_snapshot_clears_cached_unread() {
    let chat = ChatId::jid_to_chat_id("15550000001@s.whatsapp.net");
    // A chat that was unread when the app last shut down...
    let mut st = WhatsAppState {
        chats: vec![Chat {
            id: chat.clone(),
            contact_name: "Alice".to_string(),
            unread: true,
            unread_count: 3,
            ..Default::default()
        }],
        ..Default::default()
    };

    // ...is cleared once the phone has read it and the sync snapshot says 0.
    let read_conv = wa::Conversation {
        id: "15550000001@s.whatsapp.net".to_string(),
        unread_count: Some(0),
        ..Default::default()
    };
    st.upsert_conversation(&read_conv, chat.clone());
    assert!(!st.chats[0].unread);
    assert_eq!(st.chats[0].unread_count, 0);

    // A snapshot with unread still bumps a previously-read chat.
    let unread_conv = wa::Conversation {
        id: "15550000001@s.whatsapp.net".to_string(),
        unread_count: Some(2),
        ..Default::default()
    };
    st.upsert_conversation(&unread_conv, chat.clone());
    assert!(st.chats[0].unread);
    assert_eq!(st.chats[0].unread_count, 2);
}

#[test]
fn upsert_snapshot_without_pin_preserves_applied_fixed() {
    // WhatsApp syncs `pinned` as None in practice; pins arrive only via
    // PinUpdate. A snapshot must never reset a pinned chat back to false.
    let chat = ChatId::jid_to_chat_id("15550000001@s.whatsapp.net");
    let mut st = WhatsAppState {
        chats: vec![Chat {
            id: chat.clone(),
            contact_name: "Alice".to_string(),
            fixed: true,
            ..Default::default()
        }],
        ..Default::default()
    };

    let conv = wa::Conversation {
        id: "15550000001@s.whatsapp.net".to_string(),
        ..Default::default()
    };
    st.upsert_conversation(&conv, chat.clone());
    assert!(
        st.chats[0].fixed,
        "a None-pinned history sync must not unpin an applied pin"
    );

    // A sync that does carry a pin value is authoritative: Some(1) keeps it,
    // Some(0) clears it.
    let pinned = wa::Conversation {
        id: "15550000001@s.whatsapp.net".to_string(),
        pinned: Some(1),
        ..Default::default()
    };
    st.upsert_conversation(&pinned, chat.clone());
    assert!(st.chats[0].fixed);

    let unpinned = wa::Conversation {
        id: "15550000001@s.whatsapp.net".to_string(),
        pinned: Some(0),
        ..Default::default()
    };
    st.upsert_conversation(&unpinned, chat.clone());
    assert!(!st.chats[0].fixed, "Some(0) is an explicit unpin");
}

#[test]
fn apply_pin_shuffled_replay_converges_to_newest_change() {
    // A full-sync pin replay delivers a chat's historical pin/unpin
    // actions in arbitrary order; the newest change timestamp must win
    // even when a stale unpin arrives last.
    let chat = ChatId::jid_to_chat_id("15550000001@s.whatsapp.net");
    let mut st = WhatsAppState {
        chats: vec![Chat {
            id: chat.clone(),
            ..Default::default()
        }],
        ..Default::default()
    };

    assert!(st.apply_pin(&chat, true, 1_000));
    assert!(
        !st.apply_pin(&chat, true, 2_000),
        "re-pinning is a no-op, but its timestamp is recorded"
    );
    // Old unpin (500) delivered after both — must not clobber the pin.
    assert!(!st.apply_pin(&chat, false, 500));
    assert!(st.chats[0].fixed, "newest repin (2000) must remain applied");

    // The change timestamps ride along: a genuinely newer unpin clears it.
    assert!(st.apply_pin(&chat, false, 3_000));
    assert!(!st.chats[0].fixed);
}

#[test]
fn apply_pin_stale_unpin_cannot_clobber_newer_repin() {
    let chat = ChatId::jid_to_chat_id("15550000001@s.whatsapp.net");
    let mut st = WhatsAppState {
        chats: vec![Chat {
            id: chat.clone(),
            fixed: true,
            ..Default::default()
        }],
        pin_state: HashMap::from([("15550000001@s.whatsapp.net".to_string(), (2_000, true))]),
        ..Default::default()
    };

    assert!(!st.apply_pin(&chat, false, 500));
    assert!(st.chats[0].fixed, "stale unpin must be ignored");
}

#[test]
fn apply_pin_remembered_for_later_row_creation() {
    // Pins commonly replay before any history sync, so the chat row does
    // not exist yet; the state must be remembered and honored when the
    // row is later created unpinned by a history-sync upsert.
    let chat = ChatId::jid_to_chat_id("15550000001@s.whatsapp.net");
    let mut st = WhatsAppState::default();

    assert!(
        !st.apply_pin(&chat, true, 1_000),
        "no row yet, nothing changed"
    );

    let conv = wa::Conversation {
        id: "15550000001@s.whatsapp.net".to_string(),
        ..Default::default()
    };
    st.upsert_conversation(&conv, chat);
    assert!(
        st.chats[0].fixed,
        "history sync must honor the remembered pin"
    );
}

#[tokio::test]
async fn canonical_chat_id_folds_known_lid_pin_to_phone_row() {
    // The PinUpdate handler canonicalizes the wire LID the same way live
    // messages are folded, so an offline client (mapping already cached)
    // resolves the pin to the phone-keyed row the list stores.
    let pn_chat = ChatId::WhatsApp("15550000001@s.whatsapp.net".to_string());
    let state: SharedState = Arc::new(RwLock::new(WhatsAppState {
        chats: vec![Chat {
            id: pn_chat.clone(),
            ..Default::default()
        }],
        lid_pn: HashMap::from([("255202829570287".to_string(), "15550000001".to_string())]),
        ..Default::default()
    }));

    let lid = ChatId::jid_to_chat_id("255202829570287@lid");
    assert_eq!(canonical_chat_id(None, &state, lid).await, pn_chat);
}

/// WhatsApp addresses 1:1 threads by LID on the wire while the chat list and
/// the open chat are keyed by the phone number, so every event that names a
/// conversation has to fold its JID through the same resolver a message does.
/// A handler comparing the raw LID would miss the phone-keyed row and apply
/// its update to nothing the user can see.
#[tokio::test]
async fn canonical_jid_chat_id_folds_a_wire_lid_to_the_phone_keyed_row() {
    let pn_chat = ChatId::WhatsApp("15550000001@s.whatsapp.net".to_string());
    let state: SharedState = Arc::new(RwLock::new(WhatsAppState {
        chats: vec![Chat {
            id: pn_chat.clone(),
            ..Default::default()
        }],
        lid_pn: HashMap::from([("255202829570287".to_string(), "15550000001".to_string())]),
        ..Default::default()
    }));

    let lid = Jid::lid("255202829570287");
    assert_eq!(canonical_jid_chat_id(None, &state, &lid).await, pn_chat);
    // A phone-keyed event is already canonical and stays put.
    assert_eq!(
        canonical_jid_chat_id(None, &state, &pn("15550000001")).await,
        pn_chat
    );
}

/// The fold must never rename a chat: with no mapping, or with no phone-keyed
/// row, the LID is what the user opens, so the event has to land there.
#[tokio::test]
async fn canonical_jid_chat_id_keeps_an_unresolved_lid_row() {
    let lid_chat = ChatId::WhatsApp("255202829570287@lid".to_string());
    let unmapped: SharedState = Arc::new(RwLock::new(WhatsAppState {
        chats: vec![Chat {
            id: lid_chat.clone(),
            ..Default::default()
        }],
        ..Default::default()
    }));
    assert_eq!(
        canonical_jid_chat_id(None, &unmapped, &Jid::lid("255202829570287")).await,
        lid_chat
    );

    // Mapped, but the phone-keyed row is not on the list.
    let mapped_no_row: SharedState = Arc::new(RwLock::new(WhatsAppState {
        chats: vec![Chat {
            id: lid_chat.clone(),
            ..Default::default()
        }],
        lid_pn: HashMap::from([("255202829570287".to_string(), "15550000001".to_string())]),
        ..Default::default()
    }));
    assert_eq!(
        canonical_jid_chat_id(None, &mapped_no_row, &Jid::lid("255202829570287")).await,
        lid_chat
    );
}

/// End-to-end for the app-state events that used to compare a raw wire JID:
/// archive and mark-read addressed to a LID must land on the phone-keyed row
/// the chat list renders.
#[tokio::test]
async fn archive_and_mark_read_reach_the_phone_keyed_row_through_a_lid_jid() {
    let dir = tempfile::tempdir().unwrap();
    let mut messenger =
        WhatsAppMessenger::new(dir.path().join("wa.db").to_string_lossy().to_string())
            .await
            .expect("dormant construction should succeed offline");
    let client = messenger.current_client();

    let pn_chat = ChatId::WhatsApp("15550000001@s.whatsapp.net".to_string());
    let state: SharedState = Arc::new(RwLock::new(WhatsAppState {
        chats: vec![Chat {
            id: pn_chat.clone(),
            unread: true,
            unread_count: 3,
            last_message_ts: Some(1_700_000_000),
            ..Default::default()
        }],
        lid_pn: HashMap::from([("255202829570287".to_string(), "15550000001".to_string())]),
        ..Default::default()
    }));
    let (tx, mut rx) = broadcast::channel(16);
    let qr: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));
    let lid = Jid::lid("255202829570287");

    WhatsAppMessenger::handle_event(
        &Arc::new(Event::ArchiveUpdate(
            ArchiveUpdate::builder()
                .jid(lid.clone())
                .timestamp(chrono::Utc::now())
                .action(Box::new(wa::sync_action_value::ArchiveChatAction {
                    archived: Some(true),
                    ..Default::default()
                }))
                .from_full_sync(false)
                .build(),
        )),
        &client,
        &state,
        &tx,
        &qr,
        dir.path(),
    )
    .await;
    WhatsAppMessenger::handle_event(
        &Arc::new(Event::MarkChatAsReadUpdate(
            MarkChatAsReadUpdate::builder()
                .jid(lid)
                .timestamp(chrono::Utc::now())
                .action(Box::new(wa::sync_action_value::MarkChatAsReadAction {
                    read: Some(true),
                    ..Default::default()
                }))
                .from_full_sync(false)
                .build(),
        )),
        &client,
        &state,
        &tx,
        &qr,
        dir.path(),
    )
    .await;

    let chat = state.read().await.chats[0].clone();
    assert_eq!(chat.id, pn_chat, "the phone-keyed row is the visible one");
    assert_eq!(chat.status.as_deref(), Some("archived"));
    assert!(!chat.unread);
    assert_eq!(chat.unread_count, 0);
    assert_eq!(
        chat.last_message_ts,
        Some(1_700_000_000),
        "a status event must not move the chat in the list"
    );

    let mut archived = 0;
    let mut read = 0;
    while let Ok(event) = rx.try_recv() {
        match event {
            BackendEvent::ChatUpdated(chat) if chat.status.as_deref() == Some("archived") => {
                archived += 1;
            }
            BackendEvent::UnreadUpdated { unread: false, .. } => read += 1,
            _ => {}
        }
    }
    assert_eq!(archived, 1, "archive must be announced once");
    assert_eq!(read, 1, "mark-as-read must be announced once");

    messenger.disconnect().await.expect("dormant shutdown");
}

#[test]
fn upsert_self_chat_is_named_myself() {
    let mut st = WhatsAppState {
        own_lid: Some("123456789012345@lid".to_string()),
        own_pn: Some("15551234567@s.whatsapp.net".to_string()),
        ..Default::default()
    };

    // Self-chat keyed by the own phone number, even with a pushname that
    // would otherwise resolve.
    let conv = wa::Conversation {
        id: "15551234567@s.whatsapp.net".to_string(),
        name: Some("Test".to_string()),
        ..Default::default()
    };
    st.upsert_conversation(&conv, ChatId::jid_to_chat_id("15551234567@s.whatsapp.net"));
    assert_eq!(st.chats[0].contact_name, "Myself");
    assert!(!st.chats[0].verified);

    // Self-chat keyed by the own LID (no phone twin yet) is Myself too.
    let lid_conv = wa::Conversation {
        id: "123456789012345@lid".to_string(),
        conversation_timestamp: Some(1),
        ..Default::default()
    };
    st.upsert_conversation(&lid_conv, ChatId::jid_to_chat_id("123456789012345@lid"));
    assert_eq!(st.chats[1].contact_name, "Myself");

    // A peer chat is untouched by the self-chat rule.
    let peer = wa::Conversation {
        id: "15559998877@s.whatsapp.net".to_string(),
        name: Some("Alice".to_string()),
        ..Default::default()
    };
    st.upsert_conversation(&peer, ChatId::jid_to_chat_id("15559998877@s.whatsapp.net"));
    assert_eq!(st.chats[2].contact_name, "Alice");
}

#[test]
fn upsert_from_message_names_self_chat_myself() {
    let msg = Message {
        message_id: "s-1".into(),
        chat_id: ChatId::jid_to_chat_id("15551234567@s.whatsapp.net"),
        sender: "You".into(),
        text: "hello myself".into(),
        timestamp: 1,
        from_me: true,
        author_id: None,
        media: None,
        msg_actions: Vec::new(),
        reply_to_id: None,
        reply_ctx: None,
        pending: false,
        failed: false,
    };

    let mut st = WhatsAppState {
        own_pn: Some("15551234567@s.whatsapp.net".to_string()),
        ..Default::default()
    };
    st.upsert_chat_from_message(ChatId::jid_to_chat_id("15551234567@s.whatsapp.net"), &msg);
    assert_eq!(st.chats[0].contact_name, "Myself");
    assert_eq!(st.chats[0].unread_count, 0);

    // A from-me message to a peer row still uses "You".
    let mut st2 = WhatsAppState {
        own_pn: Some("15551234567@s.whatsapp.net".to_string()),
        ..Default::default()
    };
    st2.upsert_chat_from_message(ChatId::jid_to_chat_id("15559998877@s.whatsapp.net"), &msg);
    assert_eq!(st2.chats[0].contact_name, "You");

    // A self row that already exists (seeded from a previous cache with the
    // formatted own number) is renamed by an arriving self-message.
    let mut st3 = WhatsAppState {
        own_pn: Some("15551234567@s.whatsapp.net".to_string()),
        chats: vec![Chat {
            id: ChatId::WhatsApp("15551234567@s.whatsapp.net".to_string()),
            contact_name: "+15 55 123-4567".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };
    st3.upsert_chat_from_message(ChatId::jid_to_chat_id("15551234567@s.whatsapp.net"), &msg);
    assert_eq!(st3.chats[0].contact_name, "Myself");
    assert!(!st3.chats[0].verified);
}

#[test]
fn name_self_chats_renames_cached_self_rows() {
    let mut st = WhatsAppState {
        own_lid: Some("123456789012345@lid".to_string()),
        own_pn: Some("15551234567@s.whatsapp.net".to_string()),
        chats: vec![
            Chat {
                id: ChatId::WhatsApp("15551234567@s.whatsapp.net".to_string()),
                contact_name: "+15 55 123-4567".to_string(),
                verified: true,
                ..Default::default()
            },
            Chat {
                id: ChatId::WhatsApp("123456789012345@lid".to_string()),
                contact_name: "123456789012345".to_string(),
                ..Default::default()
            },
            Chat {
                id: ChatId::WhatsApp("15559998877@s.whatsapp.net".to_string()),
                contact_name: "Alice".to_string(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };

    st.name_self_chats();

    assert_eq!(st.chats[0].contact_name, "Myself");
    assert!(!st.chats[0].verified);
    assert_eq!(st.chats[1].contact_name, "Myself");
    assert_eq!(st.chats[2].contact_name, "Alice");
}

#[test]
fn resolve_stale_senders_upgrades_bare_dm_via_pushnames() {
    let chat = ChatId::jid_to_chat_id("15550000001@s.whatsapp.net");
    let mut msg = build_msg("m1", "hi", &chat, false);
    msg.sender = "15550000001".into();
    msg.author_id = Some("15550000001@s.whatsapp.net".into());

    let mut st = WhatsAppState {
        chats: vec![Chat {
            id: chat.clone(),
            contact_name: "Alice".into(),
            ..Default::default()
        }],
        pushnames: [(
            "15550000001@s.whatsapp.net".to_string(),
            "Alice".to_string(),
        )]
        .into_iter()
        .collect(),
        history: [(chat.clone(), vec![msg])].into_iter().collect(),
        ..Default::default()
    };

    let repaired = st.resolve_stale_senders();
    assert_eq!(repaired, 1);
    assert_eq!(st.history[&chat][0].sender, "Alice");
}

#[test]
fn resolve_stale_senders_uses_dm_contact_when_no_pushname() {
    let chat = ChatId::jid_to_chat_id("15550000002@s.whatsapp.net");
    let mut msg = build_msg("m1", "hi", &chat, false);
    msg.sender = "15550000002".into();
    msg.author_id = Some("15550000002@s.whatsapp.net".into());

    // No pushname/lid mappings at all: the usync-learned chat display name
    // is the safe fallback for a 1:1 chat.
    let mut st = WhatsAppState {
        chats: vec![Chat {
            id: chat.clone(),
            contact_name: "Beatriz".into(),
            ..Default::default()
        }],
        history: [(chat.clone(), vec![msg])].into_iter().collect(),
        ..Default::default()
    };

    let repaired = st.resolve_stale_senders();
    assert_eq!(repaired, 1);
    assert_eq!(st.history[&chat][0].sender, "Beatriz");
}

#[test]
fn resolve_stale_senders_never_labels_group_rows_with_title() {
    let group = "15550000001-111222333@g.us";
    let chat = ChatId::jid_to_chat_id(group);

    // History-synced row: the author is the group's own JID (no
    // `key.participant` in the blob) — nothing to resolve.
    let mut hist = build_msg("g-1", "hey", &chat, false);
    hist.sender = "15550000001-111222333".into();
    hist.author_id = Some(group.to_string());

    // Live-captured row: a real participant JID, but unknown to the maps.
    // Must stay bare — never rewritten to the group title.
    let mut member = build_msg("g-2", "yo", &chat, false);
    member.sender = "15550000003".into();
    member.author_id = Some("15550000003@s.whatsapp.net".into());

    let mut st = WhatsAppState {
        chats: vec![Chat {
            id: chat.clone(),
            contact_name: "My Group".into(),
            ..Default::default()
        }],
        history: [(chat.clone(), vec![hist, member])].into_iter().collect(),
        ..Default::default()
    };

    let repaired = st.resolve_stale_senders();
    assert_eq!(repaired, 0);
    assert_eq!(st.history[&chat][0].sender, "15550000001-111222333");
    assert_eq!(st.history[&chat][1].sender, "15550000003");
}

#[test]
fn resolve_stale_senders_names_known_group_member() {
    let group = "15550000001-111222333@g.us";
    let chat = ChatId::jid_to_chat_id(group);
    let peer = "15550000003@s.whatsapp.net";
    let mut msg = build_msg("g-1", "hey", &chat, false);
    msg.sender = "15550000003".into();
    msg.author_id = Some(peer.to_string());

    let mut st = WhatsAppState {
        chats: vec![Chat {
            id: chat.clone(),
            contact_name: "My Group".into(),
            ..Default::default()
        }],
        pushnames: [(peer.to_string(), "Bob".to_string())]
            .into_iter()
            .collect(),
        history: [(chat.clone(), vec![msg])].into_iter().collect(),
        ..Default::default()
    };

    let repaired = st.resolve_stale_senders();
    assert_eq!(repaired, 1);
    assert_eq!(st.history[&chat][0].sender, "Bob");
}

#[test]
fn resolve_stale_senders_never_downgrades_a_known_name() {
    let chat = ChatId::jid_to_chat_id("15550000004@s.whatsapp.net");
    let mut msg = build_msg("m1", "hi", &chat, false);
    msg.sender = "Beatriz".into();
    msg.author_id = Some("15550000004@s.whatsapp.net".into());

    // The stored pushname is gone from the maps and there is no chat
    // contact to fall back on: keep the better stored name.
    let mut st = WhatsAppState {
        chats: vec![Chat {
            id: chat.clone(),
            contact_name: String::new(),
            ..Default::default()
        }],
        history: [(chat.clone(), vec![msg])].into_iter().collect(),
        ..Default::default()
    };

    let repaired = st.resolve_stale_senders();
    assert_eq!(repaired, 0);
    assert_eq!(st.history[&chat][0].sender, "Beatriz");
}

#[test]
fn edit_rewrites_stored_message_via_protocol_message() {
    let chat = ChatId::jid_to_chat_id("15550000001@s.whatsapp.net");
    let mut st = WhatsAppState::default();
    st.history.insert(
        chat.clone(),
        vec![build_msg("orig-1", "before", &chat, false)],
    );
    st.chats.push(Chat {
        id: chat.clone(),
        last_message_ts: Some(999),
        ..Default::default()
    });

    let wa_msg = wa::Message {
        protocol_message: MessageField::some(wa::message::ProtocolMessage {
            key: MessageField::some(wa::MessageKey {
                id: Some("orig-1".to_string()),
                ..Default::default()
            }),
            edited_message: MessageField::some(wa::Message::text("after".to_string())),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut info = msg_info(
        &pn("15550000001"),
        &pn("15550000001"),
        "Alice",
        "orig-1",
        false,
    );
    info.edit = EditAttribute::MessageEdit;

    let updated = st
        .apply_message_edit(&chat, &info, &wa_msg)
        .expect("existing target must be edited");
    assert_eq!(updated.text, "after");
    assert_eq!(updated.message_id, MessageId("orig-1".into()));
    assert_eq!(st.history[&chat][0].text, "after");
    assert_eq!(st.chats[0].last_message_ts, Some(1000));
}

#[test]
fn edit_rewrites_stored_message_via_top_level_edited_message() {
    let chat = ChatId::jid_to_chat_id("15550000001@s.whatsapp.net");
    let mut st = WhatsAppState::default();
    st.history.insert(
        chat.clone(),
        vec![build_msg("orig-1", "before", &chat, false)],
    );

    let wa_msg = wa::Message {
        edited_message: MessageField::some(wa::message::FutureProofMessage {
            message: MessageField::some(wa::Message::text("after".to_string())),
        }),
        ..Default::default()
    };
    // No protocol_message key: the target falls back to the stanza id.
    let mut info = msg_info(
        &pn("15550000001"),
        &pn("15550000001"),
        "Alice",
        "orig-1",
        false,
    );
    info.edit = EditAttribute::MessageEdit;

    let updated = st
        .apply_message_edit(&chat, &info, &wa_msg)
        .expect("existing target must be edited");
    assert_eq!(updated.text, "after");
}

#[test]
fn edit_of_unknown_message_is_deferred_until_its_base_arrives() {
    let chat = ChatId::jid_to_chat_id("15550000001@s.whatsapp.net");
    let mut st = WhatsAppState::default();
    st.history.insert(
        chat.clone(),
        vec![build_msg("orig-1", "before", &chat, false)],
    );

    let wa_msg = wa::Message {
        protocol_message: MessageField::some(wa::message::ProtocolMessage {
            key: MessageField::some(wa::MessageKey {
                id: Some("later-9".to_string()),
                ..Default::default()
            }),
            edited_message: MessageField::some(wa::Message::text("after".to_string())),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut info = msg_info(
        &pn("15550000001"),
        &pn("15550000001"),
        "Alice",
        "later-9",
        false,
    );
    info.edit = EditAttribute::MessageEdit;

    // Nothing to rewrite yet: the edit is held for its base instead of dropped.
    assert!(st.apply_message_edit(&chat, &info, &wa_msg).is_none());
    assert!(st.apply_deferred_edits(&chat).is_empty());
    assert!(
        st.pending_edits
            .contains_key(&(MessageId("later-9".into()), chat.clone())),
        "an unresolved edit must stay pending"
    );
    assert_eq!(st.history[&chat][0].text, "before");
}

/// The convergence the pending map exists for: an edit that overtook its base
/// message is applied as soon as the message is ingested, whether the base came
/// from a live batch or a history-sync chunk.
#[test]
fn a_deferred_edit_applies_once_its_base_is_cached() {
    let chat = ChatId::jid_to_chat_id("15550000001@s.whatsapp.net");
    let mut st = WhatsAppState::default();
    let edit_msg = |id: &str, text: &str| wa::Message {
        protocol_message: MessageField::some(wa::message::ProtocolMessage {
            key: MessageField::some(wa::MessageKey {
                id: Some(id.to_string()),
                ..Default::default()
            }),
            edited_message: MessageField::some(wa::Message::text(text.to_string())),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut info = msg_info(
        &pn("15550000001"),
        &pn("15550000001"),
        "Alice",
        "base-1",
        false,
    );
    info.edit = EditAttribute::MessageEdit;

    assert!(
        st.apply_message_edit(&chat, &info, &edit_msg("base-1", "rewritten"))
            .is_none()
    );

    // The base lands afterwards (cold start: live edit, then history chunk).
    st.history.insert(
        chat.clone(),
        vec![build_msg("base-1", "original", &chat, false)],
    );
    let applied = st.apply_deferred_edits(&chat);

    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].text, "rewritten");
    assert_eq!(st.history[&chat][0].text, "rewritten");
    assert!(
        st.pending_edits.is_empty(),
        "a replayed edit must not be held again"
    );
    assert!(
        st.apply_deferred_edits(&chat).is_empty(),
        "a replayed edit is applied exactly once"
    );
}

/// An edit whose base never arrives (a message we do not cache) must not grow
/// state without limit.
#[test]
fn unresolved_edits_are_capped() {
    let chat = ChatId::jid_to_chat_id("15550000001@s.whatsapp.net");
    let mut st = WhatsAppState::default();
    let edit_msg = |id: &str| wa::Message {
        protocol_message: MessageField::some(wa::message::ProtocolMessage {
            key: MessageField::some(wa::MessageKey {
                id: Some(id.to_string()),
                ..Default::default()
            }),
            edited_message: MessageField::some(wa::Message::text("after".to_string())),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut info = msg_info(&pn("15550000001"), &pn("15550000001"), "Alice", "x", false);
    info.edit = EditAttribute::MessageEdit;

    for i in 0..200 {
        assert!(
            st.apply_message_edit(&chat, &info, &edit_msg(&format!("gone-{i}")))
                .is_none()
        );
    }

    assert!(
        st.pending_edits.len() <= 64,
        "unresolved edits must stay bounded, got {}",
        st.pending_edits.len()
    );
}

#[test]
fn edit_with_empty_body_is_ignored() {
    let chat = ChatId::jid_to_chat_id("15550000001@s.whatsapp.net");
    let mut st = WhatsAppState::default();
    st.history.insert(
        chat.clone(),
        vec![build_msg("orig-1", "before", &chat, false)],
    );

    let wa_msg = wa::Message {
        protocol_message: MessageField::some(wa::message::ProtocolMessage {
            key: MessageField::some(wa::MessageKey {
                id: Some("orig-1".to_string()),
                ..Default::default()
            }),
            edited_message: MessageField::some(wa::Message::text("".to_string())),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut info = msg_info(
        &pn("15550000001"),
        &pn("15550000001"),
        "Alice",
        "orig-1",
        false,
    );
    info.edit = EditAttribute::MessageEdit;

    assert!(st.apply_message_edit(&chat, &info, &wa_msg).is_none());
    assert_eq!(st.history[&chat][0].text, "before");
}

#[test]
fn second_edit_overwrites_the_first() {
    let chat = ChatId::jid_to_chat_id("15550000001@s.whatsapp.net");
    let mut st = WhatsAppState::default();
    st.history
        .insert(chat.clone(), vec![build_msg("orig-1", "v1", &chat, false)]);
    let edit_msg = |text: &str| wa::Message {
        protocol_message: MessageField::some(wa::message::ProtocolMessage {
            key: MessageField::some(wa::MessageKey {
                id: Some("orig-1".to_string()),
                ..Default::default()
            }),
            edited_message: MessageField::some(wa::Message::text(text.to_string())),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut info = msg_info(
        &pn("15550000001"),
        &pn("15550000001"),
        "Alice",
        "orig-1",
        false,
    );
    info.edit = EditAttribute::MessageEdit;

    st.apply_message_edit(&chat, &info, &edit_msg("v2"))
        .expect("first edit");
    assert_eq!(st.history[&chat][0].text, "v2");
    st.apply_message_edit(&chat, &info, &edit_msg("v3"))
        .expect("second edit");
    assert_eq!(st.history[&chat][0].text, "v3");
}

#[test]
fn edit_message_text_rewrites_stored_message_and_refreshes_preview() {
    let chat = ChatId::jid_to_chat_id("15550000001@s.whatsapp.net");
    let mut st = WhatsAppState::default();
    st.history.insert(
        chat.clone(),
        vec![build_msg("orig-1", "before", &chat, false)],
    );
    st.chats.push(Chat {
        id: chat.clone(),
        last_message_ts: Some(999),
        ..Default::default()
    });

    let updated = st
        .edit_message_text(&chat, &MessageId("orig-1".into()), "after")
        .expect("found");
    assert_eq!(updated.text, "after");
    assert_eq!(updated.message_id, MessageId("orig-1".into()));
    assert_eq!(st.history[&chat][0].text, "after");
    assert_eq!(st.chats[0].last_message_ts, Some(1000));
}

#[test]
fn edit_message_text_ignores_unknown_message() {
    let chat = ChatId::jid_to_chat_id("15550000001@s.whatsapp.net");
    let mut st = WhatsAppState::default();
    st.history.insert(
        chat.clone(),
        vec![build_msg("orig-1", "before", &chat, false)],
    );
    assert!(
        st.edit_message_text(&chat, &MessageId("nope".into()), "after")
            .is_none()
    );
    assert_eq!(st.history[&chat][0].text, "before");
}

#[test]
fn chat_id_drops_device_qualifier() {
    let jids = [
        Jid::from_str("15551234567@s.whatsapp.net").unwrap(),
        pn("15551234567").with_device(7),
        pn("15551234567"),
    ];
    let expected = ChatId::WhatsApp("15551234567@s.whatsapp.net".to_string());
    for jid in &jids {
        assert_eq!(
            ChatId::jid_to_chat_id(&jid.to_string()),
            expected,
            "for {jid}"
        );
    }
}

#[test]
fn chat_id_from_str_normalizes_group() {
    let id = ChatId::jid_to_chat_id("123456789@g.us");
    assert_eq!(id, ChatId::WhatsApp("123456789@g.us".to_string()));
    // A non-parseable string is preserved rather than dropped.
    assert_eq!(
        ChatId::jid_to_chat_id("garbage-not-a-jid"),
        ChatId::WhatsApp("garbage-not-a-jid".to_string())
    );
}

#[test]
fn sender_uses_push_name_then_cache_then_jid() {
    let sender = pn("15550000001");
    let mut pushnames = HashMap::new();
    pushnames.insert(
        "15550000001@s.whatsapp.net".to_string(),
        "Cached Name".to_string(),
    );
    let no_lid_pn = HashMap::new();

    // 1. Message push name wins.
    assert_eq!(
        resolve_sender_name("Alice", &sender, &pushnames, &no_lid_pn),
        "Alice"
    );
    // 2. Falls back to the push-name cache.
    assert_eq!(
        resolve_sender_name("", &sender, &pushnames, &no_lid_pn),
        "Cached Name"
    );
    // 3. Falls back to the bare JID user part.
    let empty = HashMap::new();
    assert_eq!(
        resolve_sender_name("", &sender, &empty, &no_lid_pn),
        "15550000001"
    );
}

#[test]
fn sender_resolves_lid_author_via_lid_pn_mapping() {
    let sender = Jid::lid("255202829570287");
    let mut pushnames = HashMap::new();
    pushnames.insert(
        "15550000007@s.whatsapp.net".to_string(),
        "Test Sender".to_string(),
    );
    let mut lid_pn = HashMap::new();
    lid_pn.insert("255202829570287".to_string(), "15550000007".to_string());

    // Cached push name for the peer's phone number wins over the bare LID.
    assert_eq!(
        resolve_sender_name("", &sender, &pushnames, &lid_pn),
        "Test Sender"
    );

    // No cached name: formatted number, never a bare LID.
    let empty = HashMap::new();
    assert_eq!(
        resolve_sender_name("", &sender, &empty, &lid_pn),
        "+15 55 000-0007"
    );

    // Unknown mapping: falls back to the bare LID user part.
    let no_lid_pn = HashMap::new();
    assert_eq!(
        resolve_sender_name("", &sender, &empty, &no_lid_pn),
        "255202829570287"
    );
}

#[test]
fn normalize_inbound_detects_from_me_and_unknown() {
    let state = WhatsAppState::default();
    let chat = pn("15551234567");
    let info = msg_info(&chat, &chat, "Alice", "stanza-1", false);
    let msg = to_senders_msg(
        ChatId::jid_to_chat_id(&chat.to_string()),
        &info,
        &text_message("hello"),
        &state,
    );
    assert_eq!(msg.sender, "Alice");
    assert!(!msg.from_me);
    assert_eq!(
        msg.chat_id,
        ChatId::WhatsApp("15551234567@s.whatsapp.net".to_string())
    );
    assert!(msg.msg_actions.contains(&MessageAction::Reply));
    assert!(!msg.msg_actions.contains(&MessageAction::Edit));

    let mine = msg_info(&chat, &chat, "", "stanza-2", true);
    let m = to_senders_msg(
        ChatId::jid_to_chat_id(&chat.to_string()),
        &mine,
        &text_message("hi"),
        &state,
    );
    assert_eq!(m.sender, "You");
    assert!(m.from_me);
    assert!(m.msg_actions.contains(&MessageAction::Edit));
    assert!(m.msg_actions.contains(&MessageAction::Delete));
}

#[test]
fn own_chat_id_rewrites_self_lid_messages_to_own_pn() {
    let own_pn = Jid::pn("15551234567");
    let own_lid = Jid::lid("123456789012345");

    // Self message keyed by our own LID -> own PN chat.
    let lid_chat = ChatId::WhatsApp("123456789012345@lid".to_string());
    assert_eq!(
        own_chat_id(Some(&own_lid), Some(&own_pn), lid_chat.clone(), true),
        ChatId::WhatsApp("15551234567@s.whatsapp.net".to_string())
    );

    // A same-JID chat addressed to someone else (not from_me) is untouched.
    let other_lid_chat = ChatId::WhatsApp("777777777777777@lid".to_string());
    assert_eq!(
        own_chat_id(Some(&own_lid), Some(&own_pn), other_lid_chat.clone(), false),
        other_lid_chat
    );

    // From-me message in a personal chat is left alone.
    let dm_chat = ChatId::WhatsApp("15559998877@s.whatsapp.net".to_string());
    assert_eq!(
        own_chat_id(Some(&own_lid), Some(&own_pn), dm_chat.clone(), true),
        dm_chat
    );

    // Missing own credentials fall back to the original chat.
    assert_eq!(own_chat_id(None, None, lid_chat.clone(), true), lid_chat);
}

#[test]
fn whatsapp_state_round_trips_through_json() {
    let mut state = WhatsAppState::default();
    state.chats.push(Chat {
        id: ChatId::WhatsApp("15551234567@s.whatsapp.net".to_string()),
        contact_name: "Self".to_string(),
        ..Default::default()
    });
    state.history.insert(
        ChatId::WhatsApp("15551234567@s.whatsapp.net".to_string()),
        vec![to_senders_msg(
            ChatId::jid_to_chat_id(&pn("15551234567").to_string()),
            &msg_info(&pn("15551234567"), &pn("15551234567"), "Self", "s-1", true),
            &text_message("hello"),
            &state,
        )],
    );
    state.pushnames.insert(
        "15559998877@s.whatsapp.net".to_string(),
        "Alice".to_string(),
    );

    let encoded = serde_json::to_string(&state).unwrap();
    let restored: WhatsAppState = serde_json::from_str(&encoded).unwrap();
    let encoded_again = serde_json::to_string(&restored).unwrap();

    assert_eq!(encoded_again, encoded);
    assert_eq!(restored.chats.len(), 1);
    assert_eq!(restored.history.len(), 1);
    assert_eq!(
        restored
            .pushnames
            .get("15559998877@s.whatsapp.net")
            .unwrap(),
        "Alice"
    );
    assert_eq!(
        restored
            .history
            .get(&ChatId::WhatsApp("15551234567@s.whatsapp.net".to_string()))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn whatsapp_state_load_falls_back_on_tui_chats_schema() {
    // `chats.json` historically held a bare Vec<Chat> (the TUI's
    // persistence schema), which is incompatible with WhatsAppState.
    // Loading it must never panic; it degrades to a fresh state so a cold
    // restart can rebuild from sync + usync instead of churning on a
    // mismatched file.
    let path = std::env::temp_dir().join("senders_wa_tui_schema_test.json");
    let foreign = serde_json::to_string(&[Chat {
        id: ChatId::WhatsApp("15550000006@s.whatsapp.net".to_string()),
        contact_name: "Test User".to_string(),
        ..Default::default()
    }])
    .unwrap();
    std::fs::write(&path, foreign).unwrap();

    let state = WhatsAppState::load_from(path.to_string_lossy().as_ref());
    std::fs::remove_file(&path).ok();

    assert!(state.chats.is_empty());
    assert!(state.pushnames.is_empty());
}

#[tokio::test]
async fn history_sync_ingests_chats_oldest_to_newest_and_resolves_group_senders() {
    let (tx, _rx) = broadcast::channel(128);
    let state: SharedState = Arc::new(RwLock::new(WhatsAppState::default()));

    // One-to-one chat with a saved name in `display_name` (name empty).
    let dm_messages = vec![
        web_msg(
            "15550000002@s.whatsapp.net",
            None,
            "dm-1",
            1000,
            false,
            "DmName",
            "first",
        ),
        web_msg(
            "15550000002@s.whatsapp.net",
            None,
            "dm-2",
            1001,
            true,
            "own",
            "reply",
        ),
    ];
    let dm = wa::Conversation {
        id: "15550000002@s.whatsapp.net".to_string(),
        name: None,
        display_name: Some("Dm Contact".to_string()),
        messages: dm_messages,
        pinned: Some(1),
        unread_count: Some(3),
        ..Default::default()
    };

    // Group chat: subject name, sender resolved from participant push name.
    let group_messages = vec![
        web_msg(
            "111222333@g.us",
            Some("15550000003@s.whatsapp.net"),
            "g-1",
            2000,
            false,
            "Bob",
            "hey group",
        ),
        web_msg("111222333@g.us", None, "g-2", 2001, true, "own", "yes"),
    ];
    let group_conv = wa::Conversation {
        id: "111222333@g.us".to_string(),
        name: Some("My Group".to_string()),
        messages: group_messages,
        ..Default::default()
    };

    let pushnames = vec![wa::Pushname {
        id: Some("15550000003@s.whatsapp.net".to_string()),
        pushname: Some("Bob".to_string()),
    }];

    let hs = wa::HistorySync {
        conversations: vec![dm, group_conv],
        pushnames,
        ..Default::default()
    };

    handle_history_sync(&hs, None, &state, &tx, Path::new("/tmp/wa_test_cache.json")).await;
    let s = state.read().await;

    assert_eq!(s.chats.len(), 2);

    let dm_chat = s
        .chats
        .iter()
        .find(|c| c.id == ChatId::WhatsApp("15550000002@s.whatsapp.net".to_string()))
        .unwrap();

    assert_eq!(dm_chat.contact_name, "Dm Contact");
    assert!(dm_chat.fixed, "pinned conversation should be marked fixed");
    assert_eq!(dm_chat.unread_count, 3);
    assert!(dm_chat.unread);
    // Recency timestamp from the newest message.
    assert_eq!(dm_chat.last_message_ts, Some(1001));

    let group_chat = s
        .chats
        .iter()
        .find(|c| c.id == ChatId::WhatsApp("111222333@g.us".to_string()))
        .unwrap();
    assert_eq!(group_chat.contact_name, "My Group");

    // History oldest-to-newest (dm-1 before dm-2).
    let dm_history = s
        .history
        .get(&ChatId::WhatsApp("15550000002@s.whatsapp.net".to_string()))
        .unwrap();
    let ids: Vec<_> = dm_history.iter().map(|m| m.message_id.0.clone()).collect();
    assert_eq!(ids, vec!["dm-1".to_string(), "dm-2".to_string()]);

    // Group message sender resolved to the participant's name, not the group title.
    let group_history = s
        .history
        .get(&ChatId::WhatsApp("111222333@g.us".to_string()))
        .unwrap();
    let bob = group_history.iter().find(|m| !m.from_me).unwrap();
    assert_eq!(bob.sender, "Bob");
    assert_ne!(bob.sender, "My Group");
}

#[tokio::test]
async fn history_sync_deduplicates_and_skips_empty_messages() {
    let (tx, _rx) = broadcast::channel(128);
    let state: SharedState = Arc::new(RwLock::new(WhatsAppState::default()));
    let chat_id = "15550000009@s.whatsapp.net";

    // Two copies of the same stanza + one empty protocol record.
    let messages = vec![
        web_msg(chat_id, None, "dup-1", 1000, false, "Alice", "hello"),
        web_msg(chat_id, None, "dup-1", 1000, false, "Alice", "hello"),
        wa::HistorySyncMsg::default(),
    ];
    let hs = wa::HistorySync {
        conversations: vec![conversation(chat_id, messages)],
        pushnames: vec![],
        ..Default::default()
    };

    handle_history_sync(&hs, None, &state, &tx, Path::new("/tmp/wa_test_cache.json")).await;
    let s = state.read().await;
    let history = s
        .history
        .get(&ChatId::WhatsApp(chat_id.to_string()))
        .unwrap();
    assert_eq!(
        history.len(),
        1,
        "duplicate + empty must collapse to one message"
    );
}

#[tokio::test]
async fn history_sync_persists_media_refs_for_on_demand_download() {
    let (tx, _rx) = broadcast::channel(128);
    let state: SharedState = Arc::new(RwLock::new(WhatsAppState::default()));
    let chat_id = "15550000004@s.whatsapp.net";

    let img = wa::HistorySyncMsg {
        message: MessageField::some(wa::WebMessageInfo {
            key: MessageField::some(wa::MessageKey {
                id: Some("wa-img-1".to_string()),
                remote_jid: Some(chat_id.to_string()),
                from_me: Some(false),
                participant: None,
            }),
            message: MessageField::some(wa::Message {
                image_message: MessageField::some(wa::message::ImageMessage {
                    direct_path: Some("/v/t62.7118-24/12345_67890".into()),
                    media_key: Some(vec![0, 1, 2, 3]),
                    file_sha256: Some(vec![4, 5, 6, 7]),
                    file_enc_sha256: Some(vec![8, 9, 10, 11]),
                    file_length: Some(1024),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            message_timestamp: Some(1000),
            push_name: Some("Alice".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };

    let hs = wa::HistorySync {
        conversations: vec![conversation(chat_id, vec![img])],
        pushnames: vec![],
        ..Default::default()
    };

    handle_history_sync(&hs, None, &state, &tx, Path::new("/tmp/wa_test_cache.json")).await;

    let s = state.read().await;
    let r = s
        .media_refs
        .get("wa-img-1")
        .expect("media ref kept for on-demand download");
    assert_eq!(r.direct_path, "/v/t62.7118-24/12345_67890");
    assert_eq!(r.file_length, 1024);

    // The reference must survive a serialize/deserialize round trip so
    // cached messages keep working after a restart.
    let encoded = serde_json::to_string(&*s).unwrap();
    assert!(encoded.contains("media_refs"));
    let restored: WhatsAppState = serde_json::from_str(&encoded).unwrap();
    assert!(restored.media_refs.contains_key("wa-img-1"));
}

#[tokio::test]
async fn history_sync_replay_does_not_duplicate_cached_messages() {
    let (tx, mut rx) = broadcast::channel(128);
    let state: SharedState = Arc::new(RwLock::new(WhatsAppState::default()));
    let chat_id = "15550000009@s.whatsapp.net";
    let cache = Path::new("/tmp/wa_test_cache.json");

    let hs = wa::HistorySync {
        conversations: vec![conversation(
            chat_id,
            vec![
                web_msg(chat_id, None, "wa-replay-1", 1000, false, "Alice", "hi"),
                web_msg(chat_id, None, "wa-replay-2", 1001, false, "Alice", "again"),
            ],
        )],
        pushnames: vec![],
        ..Default::default()
    };

    handle_history_sync(&hs, None, &state, &tx, cache).await;
    let chat = ChatId::WhatsApp(chat_id.to_string());
    let first = {
        let s = state.read().await;
        (
            s.history.get(&chat).map(Vec::len).expect("history"),
            s.by_stanza_id.len(),
        )
    };
    assert_eq!(first.0, 2);

    // `require_full_sync` re-requests the backfill on every connect, so the
    // server replays conversations we may already hold. The ingest has to be
    // idempotent or each cold start would double the cached history.
    handle_history_sync(&hs, None, &state, &tx, cache).await;
    {
        let s = state.read().await;
        assert_eq!(s.history.get(&chat).map(Vec::len), Some(first.0));
        assert_eq!(s.by_stanza_id.len(), first.1);
    }

    // A replay is a cold-start recovery, not new traffic: it must not announce
    // anything, so nothing may come out as `MessageReceived`.
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok())
            .any(|event| matches!(event, BackendEvent::MessageReceived(_))),
        "history sync must not emit MessageReceived"
    );
}

#[tokio::test]
async fn cached_voice_note_gains_a_media_ref_on_resync() {
    let (tx, _rx) = broadcast::channel(128);
    let state: SharedState = Arc::new(RwLock::new(WhatsAppState::default()));
    let chat_id = "15550000008@s.whatsapp.net";

    // Simulate a cache written before audio refs existed: the voice note
    // is already in `state.history` (restored from disk) with no
    // `media_refs` entry. The app used to `continue` past the capture for
    // such messages, so they stayed unfetchable (the ⚠ popup state).
    let cached = Message {
        message_id: MessageId("wa-cached-audio".into()),
        chat_id: ChatId::WhatsApp(chat_id.to_string()),
        sender: "Alice".into(),
        author_id: None,
        text: String::new(),
        timestamp: 1000,
        from_me: false,
        msg_actions: Vec::new(),
        media: Some(MessageMedia {
            kind: MediaKind::Audio {
                duration_secs: Some(12),
                is_voice: true,
                waveform: None,
            },
            caption: None,
            file_name: None,
        }),
        reply_to_id: None,
        reply_ctx: None,
        pending: false,
        failed: false,
    };
    state
        .write()
        .await
        .history
        .insert(ChatId::WhatsApp(chat_id.to_string()), vec![cached]);

    // A later HistorySync re-delivers the same stanza with the full CDN
    // fields: the ref must be captured even though the message is already
    // cached.
    let voice = wa::HistorySyncMsg {
        message: MessageField::some(wa::WebMessageInfo {
            key: MessageField::some(wa::MessageKey {
                id: Some("wa-cached-audio".to_string()),
                remote_jid: Some(chat_id.to_string()),
                from_me: Some(false),
                participant: None,
            }),
            message: MessageField::some(wa::Message {
                audio_message: MessageField::some(wa::message::AudioMessage {
                    direct_path: Some("/v/t62.7118-24/9876_54321/mp3".into()),
                    media_key: Some(vec![0, 1, 2, 3]),
                    file_sha256: Some(vec![4, 5, 6, 7]),
                    file_enc_sha256: Some(vec![8, 9, 10, 11]),
                    file_length: Some(4096),
                    seconds: Some(12),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            message_timestamp: Some(1000),
            push_name: Some("Alice".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let hs = wa::HistorySync {
        conversations: vec![conversation(chat_id, vec![voice])],
        pushnames: vec![],
        ..Default::default()
    };

    handle_history_sync(&hs, None, &state, &tx, Path::new("/tmp/wa_test_cache.json")).await;

    let s = state.read().await;
    let r = s
        .media_refs
        .get("wa-cached-audio")
        .expect("resynced cached message gains its audio ref");
    assert!(matches!(r.kind, MediaKind::Audio { .. }));
    assert_eq!(r.direct_path, "/v/t62.7118-24/9876_54321/mp3");
    assert_eq!(r.file_length, 4096);
    // The stored message is not duplicated by the resync.
    assert_eq!(s.history[&ChatId::WhatsApp(chat_id.to_string())].len(), 1);
}

#[test]
fn image_media_ref_extracts_cdn_fields_and_skips_incomplete_images() {
    let img = wa::Message {
        image_message: MessageField::some(wa::message::ImageMessage {
            direct_path: Some("/v/t62.7118-24/12345_67890".into()),
            media_key: Some(vec![0, 1, 2, 3]),
            file_sha256: Some(vec![4, 5, 6, 7]),
            file_enc_sha256: Some(vec![8, 9, 10, 11]),
            file_length: Some(1024),
            ..Default::default()
        }),
        ..Default::default()
    };
    let r = wa_media_ref(&img).expect("complete image yields a ref");
    assert_eq!(r.kind, MediaKind::Image);
    assert_eq!(r.direct_path, "/v/t62.7118-24/12345_67890");
    assert_eq!(r.media_key, vec![0, 1, 2, 3]);
    assert_eq!(r.file_length, 1024);

    // A text message has no media.
    assert!(wa_media_ref(&text_message("hello")).is_none());

    // An image missing its direct path cannot be re-downloaded by reference.
    let no_path = wa::Message {
        image_message: MessageField::some(wa::message::ImageMessage {
            media_key: Some(vec![0, 1, 2, 3]),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(wa_media_ref(&no_path).is_none());
}

#[test]
fn audio_media_ref_extracts_cdn_fields_for_voice_notes() {
    let audio = wa::Message {
        audio_message: MessageField::some(wa::message::AudioMessage {
            direct_path: Some("/v/t62.7118-24/9876_5432/mp3".into()),
            media_key: Some(vec![0, 1, 2, 3]),
            file_sha256: Some(vec![4, 5, 6, 7]),
            file_enc_sha256: Some(vec![8, 9, 10, 11]),
            file_length: Some(4096),
            ptt: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    let r = wa_media_ref(&audio).expect("complete audio yields a ref");
    assert!(matches!(r.kind, MediaKind::Audio { .. }));
    assert_eq!(r.direct_path, "/v/t62.7118-24/9876_5432/mp3");
    assert_eq!(r.media_key, vec![0, 1, 2, 3]);
    assert_eq!(r.file_length, 4096);

    // An audio message missing its media key cannot be fetched later.
    let no_key = wa::Message {
        audio_message: MessageField::some(wa::message::AudioMessage {
            direct_path: Some("/v/foo".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(wa_media_ref(&no_key).is_none());
}

#[test]
fn video_media_ref_extracts_cdn_fields_and_skips_incomplete_videos() {
    let clip = wa::Message {
        video_message: MessageField::some(wa::message::VideoMessage {
            direct_path: Some("/v/t62.7166-24/11111_2222/mp4".into()),
            media_key: Some(vec![0, 1, 2, 3]),
            file_sha256: Some(vec![4, 5, 6, 7]),
            file_enc_sha256: Some(vec![8, 9, 10, 11]),
            file_length: Some(8192),
            ..Default::default()
        }),
        ..Default::default()
    };
    let r = wa_media_ref(&clip).expect("complete video yields a ref");
    assert_eq!(r.kind, MediaKind::Video);
    assert_eq!(r.direct_path, "/v/t62.7166-24/11111_2222/mp4");
    assert_eq!(r.media_key, vec![0, 1, 2, 3]);
    assert_eq!(r.file_sha256, vec![4, 5, 6, 7]);
    assert_eq!(r.file_enc_sha256, vec![8, 9, 10, 11]);
    assert_eq!(r.file_length, 8192);

    // A video missing its direct path cannot be re-downloaded by reference.
    let no_path = wa::Message {
        video_message: MessageField::some(wa::message::VideoMessage {
            media_key: Some(vec![0, 1, 2, 3]),
            file_length: Some(8192),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(wa_media_ref(&no_path).is_none());
}

#[test]
fn a_stored_video_ref_downloads_as_a_video() {
    // The ref's kind decides the `MediaType` that decrypts the stream, and
    // the kinds use different CDN keys.
    assert_eq!(ref_media_type(&MediaKind::Video), Some(MediaType::Video));
    assert_eq!(ref_media_type(&MediaKind::Image), Some(MediaType::Image));
    assert_eq!(
        ref_media_type(&MediaKind::Audio {
            duration_secs: Some(1),
            is_voice: true,
            waveform: None,
        }),
        Some(MediaType::Audio),
    );
    // Kinds that are never captured into `media_refs` have nothing to fetch.
    assert_eq!(ref_media_type(&MediaKind::Sticker), None);
    assert_eq!(ref_media_type(&MediaKind::Document), None);
}

#[test]
fn audio_duration_is_captured_from_voice_note_seconds() {
    let audio = wa::Message {
        audio_message: MessageField::some(wa::message::AudioMessage {
            direct_path: Some("/v/audio".into()),
            media_key: Some(vec![1, 2, 3]),
            file_sha256: Some(vec![4, 5, 6]),
            file_enc_sha256: Some(vec![7, 8, 9]),
            file_length: Some(2048),
            seconds: Some(42),
            ..Default::default()
        }),
        ..Default::default()
    };
    let media = handle_wa_media(&audio).expect("audio yields media");
    assert_eq!(
        media.kind,
        MediaKind::Audio {
            duration_secs: Some(42),
            is_voice: false,
            waveform: None,
        }
    );

    // Text messages carry no media at all.
    assert!(handle_wa_media(&text_message("hello")).is_none());
}

#[test]
fn fold_lid_key_rewrites_known_lid_when_pn_row_exists() {
    let pn_chat = ChatId::WhatsApp("15550000007@s.whatsapp.net".to_string());

    let mut folded = WhatsAppState::default();
    folded.chats.push(Chat {
        id: pn_chat.clone(),
        ..Default::default()
    });
    folded
        .lid_pn
        .insert("255202829570287".to_string(), "15550000007".to_string());

    // Mapped + row exists: folded to the phone-keyed row.
    assert_eq!(fold_lid_key("255202829570287@lid", &folded), pn_chat);
    // A plain phone-keyed chat is untouched.
    assert_eq!(fold_lid_key("15550000007@s.whatsapp.net", &folded), pn_chat);

    // LID with no mapping stays put.
    assert_eq!(
        fold_lid_key("777777777777777@lid", &folded),
        ChatId::WhatsApp("777777777777777@lid".to_string())
    );

    // Mapped but no phone-keyed row on the list stays put (the peer was
    // never addressed by phone, so the LID row is what the user opens).
    let mut no_row = WhatsAppState::default();
    no_row
        .lid_pn
        .insert("255202829570287".to_string(), "15550000007".to_string());
    assert_eq!(
        fold_lid_key("255202829570287@lid", &no_row),
        ChatId::WhatsApp("255202829570287@lid".to_string())
    );

    // Unparseable keys are never rewritten.
    assert_eq!(
        fold_lid_key("not-a-jid", &folded),
        ChatId::WhatsApp("not-a-jid".to_string())
    );
}

#[test]
fn presence_label_formats_online_and_last_seen() {
    assert_eq!(presence_label(true, None), Some("online".to_string()));
    assert_eq!(
        presence_label(true, Some(1_800_000_000)),
        Some("online".to_string())
    );
    assert_eq!(presence_label(false, None), None);
    assert_eq!(
        presence_label(false, Some(now() - 300)),
        Some("last seen 5m ago".to_string())
    );
    assert_eq!(
        presence_label(false, Some(now() - 7200)),
        Some("last seen 2h ago".to_string())
    );
    assert_eq!(
        presence_label(false, Some(now() - 172800)),
        Some("last seen 2d ago".to_string())
    );
}

fn sample_cdn() -> CdnFields {
    CdnFields {
        url: "https://cdn.example/u".to_string(),
        direct_path: "/d".to_string(),
        media_key: [1u8; 32],
        file_enc_sha256: [2u8; 32],
        file_sha256: [3u8; 32],
        file_length: 4096,
        media_key_timestamp: 1_700_000_000,
        streaming_sidecar: Some(vec![9, 9, 9]),
    }
}

#[test]
fn outbound_image_message_maps_cdn_fields_and_context() {
    let msg = outbound_media_message(
        MediaKind::Image,
        sample_cdn(),
        "pic.jpg",
        Some("caption"),
        Some(wa::ContextInfo {
            stanza_id: Some("stanza-1".into()),
            ..Default::default()
        }),
    )
    .unwrap();
    let im = msg.image_message.as_option().unwrap();
    assert_eq!(im.url.as_deref(), Some("https://cdn.example/u"));
    assert_eq!(im.direct_path.as_deref(), Some("/d"));
    assert_eq!(im.media_key.as_deref(), Some(&[1u8; 32][..]));
    assert_eq!(im.file_enc_sha256.as_deref(), Some(&[2u8; 32][..]));
    assert_eq!(im.file_sha256.as_deref(), Some(&[3u8; 32][..]));
    assert_eq!(im.file_length, Some(4096));
    assert_eq!(im.media_key_timestamp, Some(1_700_000_000));
    assert_eq!(im.caption.as_deref(), Some("caption"));
    assert_eq!(
        im.context_info.as_option().unwrap().stanza_id.as_deref(),
        Some("stanza-1")
    );
}

#[test]
fn outbound_video_and_audio_carry_streaming_sidecar() {
    let video =
        outbound_media_message(MediaKind::Video, sample_cdn(), "clip.mp4", Some("hi"), None)
            .unwrap();
    let vm = video.video_message.as_option().unwrap();
    assert_eq!(vm.streaming_sidecar.as_deref(), Some(&[9, 9, 9][..]));
    assert_eq!(vm.caption.as_deref(), Some("hi"));
    assert!(!vm.context_info.is_set());

    let audio = outbound_media_message(
        MediaKind::Audio {
            duration_secs: Some(2),
            is_voice: true,
            waveform: Some(vec![64; 16]),
        },
        sample_cdn(),
        "note.ogg",
        None,
        None,
    )
    .unwrap();
    let am = audio.audio_message.as_option().unwrap();
    assert_eq!(am.streaming_sidecar.as_deref(), Some(&[9, 9, 9][..]));
    assert_eq!(am.mimetype.as_deref(), Some("audio/ogg; codecs=opus"));
    assert_eq!(am.seconds, Some(2));
    assert_eq!(am.ptt, Some(true));
    assert_eq!(am.waveform.as_deref(), Some(&[64u8; 16][..]));
}

#[test]
fn outbound_document_sets_file_name() {
    let doc = outbound_media_message(MediaKind::Document, sample_cdn(), "report.pdf", None, None)
        .unwrap();
    let dm = doc.document_message.as_option().unwrap();
    assert_eq!(dm.file_name.as_deref(), Some("report.pdf"));
    assert_eq!(dm.mimetype.as_deref(), Some("application/octet-stream"));
}

#[test]
fn own_sent_audio_keeps_a_persistable_media_ref() {
    let msg = outbound_media_message(
        MediaKind::Audio {
            duration_secs: Some(2),
            is_voice: true,
            waveform: Some(vec![64; 16]),
        },
        sample_cdn(),
        "note.ogg",
        None,
        None,
    )
    .unwrap();

    let r = wa_media_ref(&msg).expect("own-sent audio carries a re-derivable ref");
    match &r.kind {
        MediaKind::Audio {
            duration_secs,
            is_voice,
            waveform,
        } => {
            assert_eq!(*duration_secs, Some(2));
            assert!(is_voice);
            assert_eq!(waveform.as_deref(), Some(&[64u8; 16][..]));
        }
        other => panic!("expected audio kind, got {other:#?}"),
    }
    assert_eq!(r.direct_path, "/d");
    assert_eq!(r.media_key, [1u8; 32]);
    assert_eq!(r.file_sha256, [3u8; 32]);
    assert_eq!(r.file_enc_sha256, [2u8; 32]);
    assert_eq!(r.file_length, 4096);

    // The send path keys the ref by the returned stanza/message id; the
    // persisted cache round-trips that lookup for `media_bytes`.
    let mut state = WhatsAppState::default();
    state.media_refs.insert("stanza-42".into(), r.clone());
    let restored: WhatsAppState =
        serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
    assert!(
        matches!(
            restored.media_refs.get("stanza-42"),
            Some(MediaRef {
                kind: MediaKind::Audio { .. },
                ..
            })
        ),
        "own-sent clip must stay resolvable after a restart"
    );
}

#[test]
fn upsert_chat_from_message_creates_row_for_outgoing_self_chat() {
    let mut state = WhatsAppState {
        own_lid: Some("1555000000100@lid".into()),
        ..WhatsAppState::default()
    };
    let chat = ChatId::jid_to_chat_id("1555000000100@lid");

    let mut msg = build_msg("s1", "note.ogg", &chat, true);
    msg.timestamp = 9000;

    state.upsert_chat_from_message(chat.clone(), &msg);

    let row = state
        .chats
        .iter()
        .find(|c| c.id == chat)
        .expect("an outgoing send creates the chat row");
    assert_eq!(row.contact_name, "Myself");
    assert_eq!(row.last_message_ts, Some(9000));
    assert!(!row.unread);
    assert_eq!(row.unread_count, 0);
}

#[test]
fn outbound_audio_round_trips_through_disk_cache() {
    let wa_msg = outbound_media_message(
        MediaKind::Audio {
            duration_secs: Some(2),
            is_voice: true,
            waveform: Some(vec![64; 16]),
        },
        sample_cdn(),
        "note.ogg",
        None,
        None,
    )
    .unwrap();

    let mut state = WhatsAppState {
        own_lid: Some("1555000000100@lid".into()),
        ..WhatsAppState::default()
    };
    let chat = ChatId::jid_to_chat_id("1555000000100@lid");

    let msg = Message {
        message_id: MessageId("stanza-99".into()),
        chat_id: chat.clone(),
        sender: "You".into(),
        author_id: None,
        text: "Audio, click to show".into(),
        timestamp: 9000,
        from_me: true,
        msg_actions: message_actions(true),
        media: Some(MessageMedia {
            kind: MediaKind::Audio {
                duration_secs: Some(2),
                is_voice: true,
                waveform: Some(vec![64; 16]),
            },
            caption: None,
            file_name: Some("note.ogg".into()),
        }),
        reply_to_id: None,
        reply_ctx: None,
        pending: false,
        failed: false,
    };
    state
        .history
        .entry(chat.clone())
        .or_default()
        .push(msg.clone());
    state
        .media_refs
        .insert("stanza-99".into(), wa_media_ref(&wa_msg).unwrap());
    state.upsert_chat_from_message(chat.clone(), &msg);

    let path = std::env::temp_dir().join(format!(
        "senders_wa_outbound_test_{}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    state.save_to(&path);
    let restored = WhatsAppState::load_from(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);

    let hist = restored.history.get(&chat).expect("history survives");
    assert_eq!(hist.len(), 1);
    assert_eq!(hist[0].message_id, msg.message_id);
    assert_eq!(
        hist[0].media.as_ref().unwrap().kind,
        MediaKind::Audio {
            duration_secs: Some(2),
            is_voice: true,
            waveform: Some(vec![64; 16]),
        }
    );
    assert!(
        restored.media_refs.contains_key("stanza-99"),
        "the persisted media ref keeps the clip resolvable after restart"
    );
    let row = restored
        .chats
        .iter()
        .find(|c| c.id == chat)
        .expect("chat row survives");
    assert_eq!(row.contact_name, "Myself");
    assert_eq!(row.last_message_ts, Some(9000));
}

#[test]
fn outbound_sticker_builds_sticker_message() {
    let msg =
        outbound_media_message(MediaKind::Sticker, sample_cdn(), "s.webp", None, None).unwrap();
    let sm = msg.sticker_message.as_option().unwrap();
    assert_eq!(sm.url.as_deref(), Some("https://cdn.example/u"));
    assert_eq!(sm.direct_path.as_deref(), Some("/d"));
    assert_eq!(sm.media_key.as_deref(), Some(&[1u8; 32][..]));
    assert_eq!(sm.mimetype.as_deref(), Some("image/webp"));
}

#[test]
fn outbound_unsupported_kind_is_rejected() {
    let err = outbound_media_message(MediaKind::Unsupported, sample_cdn(), "x.bin", None, None)
        .err()
        .unwrap();
    assert!(
        err.to_string().contains("unsupported media kind"),
        "unexpected error: {err}"
    );
}

/// A group reply names the quoted author's wire JID as `participant`, taken
/// from the stored `author_id` (`info.source.sender`), never derived from the
/// display name or the group JID. `remote_jid` stays unset: replies are
/// same-chat, and WA Web only emits it for cross-chat quotes.
#[test]
fn group_reply_serializes_the_quoted_authors_participant() {
    let chat = ChatId::WhatsApp("120363400000000000@g.us".to_string());
    let mut st = WhatsAppState::default();
    let mut x = build_msg("stanza-x", "from X", &chat, false);
    x.author_id = Some("15550000001@s.whatsapp.net".to_string());
    let mut y = build_msg("stanza-y", "from Y", &chat, false);
    y.author_id = Some("15550000002@s.whatsapp.net".to_string());
    st.history.insert(chat.clone(), vec![x, y]);

    let ctx = st
        .reply_context_info(&chat, &MessageId("stanza-x".into()))
        .expect("a cached target resolves");
    assert_eq!(ctx.stanza_id.as_deref(), Some("stanza-x"));
    assert_eq!(
        ctx.participant.as_deref(),
        Some("15550000001@s.whatsapp.net"),
        "the reply names X, never Y"
    );
    assert!(
        ctx.remote_jid.is_none(),
        "same-chat replies set no remote_jid"
    );

    // The quoted preview comes from the remembered raw proto when available
    // and is simply omitted otherwise (attribution never depends on it).
    assert!(
        !ctx.quoted_message.is_set(),
        "no raw proto remembered yet, so no quoted preview"
    );
    st.remember_raw("stanza-x", text_message("from X"));
    let ctx = st
        .reply_context_info(&chat, &MessageId("stanza-x".into()))
        .unwrap();
    assert!(
        ctx.quoted_message.is_set(),
        "a remembered raw proto yields the quoted preview"
    );
}

/// Distinct (stanza, author) pairs resolve independently — quoting one
/// message can never yield the other's author — and the lookup passes for
/// both phone-number and LID author JIDs.
#[test]
fn distinct_group_authors_never_cross_attribute_in_either_jid_form() {
    let chat = ChatId::WhatsApp("120363400000000001@g.us".to_string());
    let mut st = WhatsAppState::default();
    let mut lid_msg = build_msg("stanza-lid", "from the LID user", &chat, false);
    lid_msg.author_id = Some("255202829570287@lid".to_string());
    let mut pn_msg = build_msg("stanza-pn", "from the phone user", &chat, false);
    pn_msg.author_id = Some("15550000003@s.whatsapp.net".to_string());
    st.history.insert(chat.clone(), vec![lid_msg, pn_msg]);

    let lid_ctx = st
        .reply_context_info(&chat, &MessageId("stanza-lid".into()))
        .unwrap();
    let pn_ctx = st
        .reply_context_info(&chat, &MessageId("stanza-pn".into()))
        .unwrap();

    assert_eq!(lid_ctx.stanza_id.as_deref(), Some("stanza-lid"));
    assert_eq!(lid_ctx.participant.as_deref(), Some("255202829570287@lid"));
    assert_eq!(pn_ctx.stanza_id.as_deref(), Some("stanza-pn"));
    assert_eq!(
        pn_ctx.participant.as_deref(),
        Some("15550000003@s.whatsapp.net")
    );
    assert_ne!(
        lid_ctx.participant, pn_ctx.participant,
        "cross-quoting must never swap the authors"
    );
}

/// Text and every supported outbound media kind embed the identical context
/// resolved once for the reply — attribution cannot diverge per kind.
#[test]
fn text_and_every_media_kind_carry_the_same_participant() {
    let chat = ChatId::WhatsApp("120363400000000002@g.us".to_string());
    let mut st = WhatsAppState::default();
    let mut target = build_msg("stanza-q", "quoted", &chat, false);
    target.author_id = Some("15550000004@s.whatsapp.net".to_string());
    st.history.insert(chat.clone(), vec![target]);

    let ctx = st
        .reply_context_info(&chat, &MessageId("stanza-q".into()))
        .unwrap();

    let text = wa::Message::text_with_context("reply", ctx.clone());
    let text_ctx = text
        .extended_text_message
        .as_option()
        .and_then(|m| m.context_info.as_option())
        .expect("text reply embeds the context");
    assert_eq!(
        text_ctx.participant.as_deref(),
        Some("15550000004@s.whatsapp.net")
    );

    let kinds = [
        MediaKind::Image,
        MediaKind::Video,
        MediaKind::Audio {
            duration_secs: Some(1),
            is_voice: false,
            waveform: None,
        },
        MediaKind::Document,
        MediaKind::Sticker,
    ];
    for kind in kinds {
        let msg =
            outbound_media_message(kind.clone(), sample_cdn(), "f.bin", None, Some(ctx.clone()))
                .unwrap();
        let embedded = msg
            .image_message
            .as_option()
            .map(|m| &m.context_info)
            .or_else(|| msg.video_message.as_option().map(|m| &m.context_info))
            .or_else(|| msg.audio_message.as_option().map(|m| &m.context_info))
            .or_else(|| msg.document_message.as_option().map(|m| &m.context_info))
            .or_else(|| msg.sticker_message.as_option().map(|m| &m.context_info))
            .and_then(|field| field.as_option())
            .unwrap_or_else(|| panic!("{kind:?} must embed the reply context"));
        assert_eq!(
            embedded.participant.as_deref(),
            Some("15550000004@s.whatsapp.net"),
            "{kind:?} carries the same participant as text"
        );
        assert_eq!(
            embedded.stanza_id.as_deref(),
            Some("stanza-q"),
            "{kind:?} quotes the right stanza"
        );
    }
}

/// A direct-message reply carries no group-only `participant` — the same
/// group-only convention `MessageKey::participant` follows upstream.
#[test]
fn a_direct_message_reply_omits_the_group_participant() {
    let chat = ChatId::WhatsApp("15550000009@s.whatsapp.net".to_string());
    let mut st = WhatsAppState::default();
    let mut peer = build_msg("stanza-dm", "yo", &chat, false);
    peer.author_id = Some("15550000009@s.whatsapp.net".to_string());
    st.history.insert(chat.clone(), vec![peer]);

    let ctx = st
        .reply_context_info(&chat, &MessageId("stanza-dm".into()))
        .unwrap();
    assert_eq!(ctx.stanza_id.as_deref(), Some("stanza-dm"));
    assert!(
        ctx.participant.is_none(),
        "DM quotes carry no group participant"
    );
    assert!(ctx.remote_jid.is_none());
}

/// A reply to our own local echo (recorded before the server echoed the
/// message back, so it has no author) resolves the account's own wire
/// identity — LID first, since groups address us by LID — and fails
/// explicitly when the identity is not known yet.
#[test]
fn a_reply_to_an_own_message_uses_the_accounts_wire_identity() {
    let chat = ChatId::WhatsApp("120363400000000003@g.us".to_string());
    // `build_msg` records no author — exactly what a local echo looks like.
    let own_echo = || build_msg("stanza-mine", "sent", &chat, true);

    let mut st = WhatsAppState {
        own_lid: Some("255202829570288@lid".into()),
        own_pn: Some("15550000010@s.whatsapp.net".into()),
        ..Default::default()
    };
    st.history.insert(chat.clone(), vec![own_echo()]);
    let ctx = st
        .reply_context_info(&chat, &MessageId("stanza-mine".into()))
        .unwrap();
    assert_eq!(
        ctx.participant.as_deref(),
        Some("255202829570288@lid"),
        "the LID form is preferred: groups address us by LID"
    );

    let mut pn_only = WhatsAppState {
        own_pn: Some("15550000010@s.whatsapp.net".into()),
        ..Default::default()
    };
    pn_only.history.insert(chat.clone(), vec![own_echo()]);
    let ctx = pn_only
        .reply_context_info(&chat, &MessageId("stanza-mine".into()))
        .unwrap();
    assert_eq!(
        ctx.participant.as_deref(),
        Some("15550000010@s.whatsapp.net"),
        "an account without a LID falls back to its phone form"
    );

    let mut unknown = WhatsAppState::default();
    unknown.history.insert(chat.clone(), vec![own_echo()]);
    let err = unknown
        .reply_context_info(&chat, &MessageId("stanza-mine".into()))
        .unwrap_err();
    assert!(
        err.to_string().contains("own wire identity"),
        "no identity, no reply: {err}"
    );
}

/// A reply target this session cannot resolve fails outright. `send`
/// resolves the context before any upload and before `send_message`, so no
/// message is sent and (through `send_outbound`) no local echo is inserted
/// for a reply that could be attributed ambiguously.
#[test]
fn an_unavailable_reply_target_fails_instead_of_sending_ambiguously() {
    let chat = ChatId::WhatsApp("120363400000000004@g.us".to_string());
    let st = WhatsAppState::default();

    let err = st
        .reply_context_info(&chat, &MessageId("stanza-unknown".into()))
        .unwrap_err();
    assert!(
        err.to_string().contains("not in this chat's history"),
        "unexpected error: {err}"
    );

    // A stored target with no recoverable author is equally unresolvable:
    // the error must not silently attribute the quote to the group.
    let mut st = WhatsAppState::default();
    let orphan = {
        let mut msg = build_msg("stanza-orphan", "???", &chat, false);
        msg.author_id = None;
        msg
    };
    st.history.insert(chat.clone(), vec![orphan]);
    let err = st
        .reply_context_info(&chat, &MessageId("stanza-orphan".into()))
        .unwrap_err();
    assert!(
        err.to_string().contains("no known author"),
        "unexpected error: {err}"
    );
}

/// Inbound replies keep `reply_stanza_id` paired with the correct chat and
/// author even when LID and phone identifiers refer to different participants
/// in the same group.
#[test]
fn inbound_reply_metadata_stays_paired_with_its_author() {
    let chat = ChatId::WhatsApp("120363400000000005@g.us".to_string());
    let mut st = WhatsAppState::default();
    let mut x = build_msg("stanza-x", "from X", &chat, false);
    x.author_id = Some("255202829570287@lid".into());
    x.sender = "X".into();
    let mut y = build_msg("stanza-y", "from Y", &chat, false);
    y.author_id = Some("15550000002@s.whatsapp.net".into());
    y.sender = "Y".into();
    st.history.insert(chat.clone(), vec![x, y]);
    for id in ["stanza-x", "stanza-y"] {
        st.by_stanza_id
            .insert(id.to_string(), (chat.clone(), MessageId(id.to_string())));
    }

    let group = Jid::from_str("120363400000000005@g.us").unwrap();
    let peer = pn("15550000001");
    let quote = |id: &str| {
        wa::Message::text_with_context(
            "reply",
            wa::ContextInfo {
                stanza_id: Some(id.to_string()),
                ..Default::default()
            },
        )
    };

    for (quoted, author) in [("stanza-x", "X"), ("stanza-y", "Y")] {
        let info = msg_info(&group, &peer, "Zed", &format!("r-{quoted}"), false);
        let normalized = to_senders_msg(chat.clone(), &info, &quote(quoted), &st);
        assert_eq!(normalized.reply_to_id, Some(MessageId(quoted.to_string())));
        let chain = normalized
            .reply_ctx
            .unwrap_or_else(|| panic!("reply chain for {quoted} resolves"));
        assert_eq!(chain.id, MessageId(quoted.to_string()));
        assert_eq!(
            chain.sender, author,
            "the chain names the quoted message's own author, not the other one"
        );
    }
}

// -- Dormant lifecycle --

/// A messenger that is constructed but never started must be fully inert: no
/// transport, so no `Connected` and no `QrCode` reach a subscriber. Local-only
/// work (sqlite store, JSON cache) is expected and allowed while dormant.
#[tokio::test]
async fn a_dormant_messenger_emits_no_events() {
    let dir = std::env::temp_dir().join(format!("senders-wa-dormant-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let store_path = dir.join("wa.db");

    let mut messenger = WhatsAppMessenger::new(store_path.to_string_lossy().to_string())
        .await
        .expect("dormant construction should succeed offline");
    let mut events = messenger.subscribe();

    assert!(
        tokio::time::timeout(Duration::from_millis(400), events.recv())
            .await
            .is_err(),
        "a dormant provider must not emit Connected or a QR code"
    );
    assert!(!messenger.is_authenticated().await);

    messenger.disconnect().await.expect("dormant shutdown");
}

/// The cold-start recovery inputs are established at construction time and must
/// survive the lifecycle move: a dormant messenger already carries the full-sync
/// history configuration the server needs to redeliver messages received while
/// the client was offline.
#[tokio::test]
async fn construction_keeps_the_full_sync_history_config() {
    let dir = std::env::temp_dir().join(format!("senders-wa-props-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let store_path = dir.join("wa.db");

    let mut messenger = WhatsAppMessenger::new(store_path.to_string_lossy().to_string())
        .await
        .expect("dormant construction should succeed offline");

    let device = messenger
        .current_client()
        .persistence_manager()
        .get_device_snapshot();
    let props = &device.device_props;
    assert_eq!(props.require_full_sync, Some(true));
    // UWP is the platform label the full-sync row belongs to, not an identity
    // claim: it travels with `require_full_sync` as one decision.
    assert_eq!(
        props.platform_type,
        Some(whatsapp_rust::waproto::whatsapp::device_props::PlatformType::UWP)
    );
    let history = props
        .history_sync_config
        .as_option()
        .expect("history config");
    assert_eq!(history.full_sync_days_limit, Some(365));
    assert_eq!(history.on_demand_ready, Some(true));
    assert_eq!(history.complete_on_demand_ready, Some(true));

    messenger.disconnect().await.expect("dormant shutdown");
}

/// One connection must produce exactly one `Connected`. It used to produce two:
/// `BotBuilder::on_connected` and the `Event::Connected` arm of the catch-all
/// `on_event` handler are both interested in `EventKind::Connected`, and the
/// library's concurrent delivery hands the event to every interested callback —
/// so the TUI rebuilt its chat list twice per connect. Only `on_event` is
/// registered now, and this pins that the surviving path emits once.
#[tokio::test]
async fn one_connection_emits_a_single_connected_event() {
    let dir = tempfile::tempdir().unwrap();
    let mut messenger =
        WhatsAppMessenger::new(dir.path().join("wa.db").to_string_lossy().to_string())
            .await
            .expect("dormant construction should succeed offline");

    let client = messenger.current_client();
    let state: SharedState = Arc::new(RwLock::new(WhatsAppState::default()));
    let (tx, mut rx) = broadcast::channel(16);
    let qr: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));

    WhatsAppMessenger::handle_event(
        &Arc::new(Event::Connected(Connected::builder().build())),
        &client,
        &state,
        &tx,
        &qr,
        dir.path(),
    )
    .await;

    // `handle_connect` emits its `Connected` before returning, so the whole
    // burst is already queued; the enrichment task it spawns may add more.
    let mut connected = 0;
    while let Ok(event) = rx.try_recv() {
        if matches!(event, BackendEvent::Connected) {
            connected += 1;
        }
    }
    assert_eq!(
        connected, 1,
        "one Event::Connected must yield exactly one BackendEvent::Connected"
    );

    messenger.disconnect().await.expect("dormant shutdown");
}

/// The same fan-out duplicate hit the pairing path: `on_qr_code` and the
/// `Event::PairingQrCode` arm both fired per issued code. One code must now
/// reach the login screen once, with the code it should render.
#[tokio::test]
async fn one_pairing_qr_code_emits_a_single_qr_event() {
    let dir = tempfile::tempdir().unwrap();
    let mut messenger =
        WhatsAppMessenger::new(dir.path().join("wa.db").to_string_lossy().to_string())
            .await
            .expect("dormant construction should succeed offline");

    let client = messenger.current_client();
    let state: SharedState = Arc::new(RwLock::new(WhatsAppState::default()));
    let (tx, mut rx) = broadcast::channel(16);
    let qr: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));

    let code = "2/abc+def+ghi==";
    WhatsAppMessenger::handle_event(
        &Arc::new(Event::PairingQrCode(
            PairingQrCode::builder()
                .code(code.to_string())
                .timeout(Duration::from_secs(20))
                .build(),
        )),
        &client,
        &state,
        &tx,
        &qr,
        dir.path(),
    )
    .await;

    let emitted: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|event| match event {
            BackendEvent::QrCode(code) => Some(code),
            _ => None,
        })
        .collect();
    assert_eq!(emitted, vec![code.to_string()]);
    assert_eq!(qr.read().await.as_deref(), Some(code));

    messenger.disconnect().await.expect("dormant shutdown");
}

#[test]
fn cache_reopen_preserves_normalized_state_and_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let cache_path = dir.path().join("wp_cache.json");

    let chat = ChatId::WhatsApp("15550000002@s.whatsapp.net".to_string());
    let mut state = WhatsAppState::default();
    state.chats.push(Chat {
        id: chat.clone(),
        contact_name: "Alice".to_string(),
        ..Default::default()
    });
    state
        .history
        .insert(chat.clone(), vec![build_msg("wa-1", "hi", &chat, false)]);
    state.pushnames.insert(
        "15550000002@s.whatsapp.net".to_string(),
        "Alice".to_string(),
    );
    state
        .lid_pn
        .insert("100000000000001".to_string(), "15550000002".to_string());
    state.media_refs.insert(
        "stanza-1".to_string(),
        MediaRef {
            kind: MediaKind::Image,
            direct_path: "/d".to_string(),
            media_key: vec![1u8; 32],
            file_sha256: vec![3u8; 32],
            file_enc_sha256: vec![2u8; 32],
            file_length: 128,
        },
    );

    state.save_to(&cache_path);
    let restored = WhatsAppState::load_from(cache_path.to_string_lossy().as_ref());

    assert_eq!(restored.chats.len(), 1);
    assert_eq!(restored.chats[0].contact_name, "Alice");
    assert_eq!(restored.history.get(&chat).map(Vec::len), Some(1));
    assert_eq!(
        restored
            .pushnames
            .get("15550000002@s.whatsapp.net")
            .map(String::as_str),
        Some("Alice")
    );
    assert_eq!(
        restored.lid_pn.get("100000000000001").map(String::as_str),
        Some("15550000002")
    );
    assert!(matches!(
        restored.media_refs.get("stanza-1"),
        Some(MediaRef {
            file_length: 128,
            ..
        })
    ));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&cache_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "WA cache must stay owner-only");
    }
}

/// The SQLite store persists the account's signal session material. Reopening
/// a dormant messenger on the same `wa.db` (after a clean shutdown) must
/// recover the same device identity rather than minting a new one.
#[tokio::test]
async fn sqlite_session_survives_dormant_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("wa.db").to_string_lossy().to_string();

    let mut first = WhatsAppMessenger::new(store_path.clone())
        .await
        .expect("dormant construction should succeed offline");
    let device = first
        .current_client()
        .persistence_manager()
        .get_device_snapshot();
    let registration_id = device.registration_id;
    let signed_pre_key_id = device.signed_pre_key_id;
    let identity = device.identity_key.public_key.public_key_bytes().to_vec();
    first.disconnect().await.expect("dormant shutdown");

    let mut second = WhatsAppMessenger::new(store_path)
        .await
        .expect("reopen should succeed offline");
    let reopened = second
        .current_client()
        .persistence_manager()
        .get_device_snapshot();

    assert_eq!(reopened.registration_id, registration_id);
    assert_eq!(reopened.signed_pre_key_id, signed_pre_key_id);
    assert_eq!(
        reopened.identity_key.public_key.public_key_bytes(),
        identity.as_slice(),
        "a reopen must not mint a new identity key"
    );

    second.disconnect().await.expect("dormant shutdown");
}

/// A dormant `WhatsAppMessenger` with the collaborators `handle_event` needs,
/// scoped to a temporary cache directory.
async fn adapter() -> (
    WhatsAppMessenger,
    SharedState,
    broadcast::Sender<BackendEvent>,
    Arc<RwLock<Option<String>>>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let messenger = WhatsAppMessenger::new(dir.path().join("wa.db").to_string_lossy().to_string())
        .await
        .expect("dormant construction should succeed offline");
    let state: SharedState = Arc::new(RwLock::new(WhatsAppState::default()));
    let (tx, _) = broadcast::channel(64);
    let qr: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));
    (messenger, state, tx, qr, dir)
}

/// Everything queued since the last drain, in arrival order.
fn drained(rx: &mut broadcast::Receiver<BackendEvent>) -> Vec<BackendEvent> {
    let mut out = Vec::new();
    while let Ok(event) = rx.try_recv() {
        out.push(event);
    }
    out
}

/// A phone-keyed row plus its history — the state a syncd update or a live
/// batch expects to find.
async fn seed_row(state: &SharedState, chat: ChatId, messages: Vec<Message>) {
    let last_message_ts = messages.iter().map(|m| m.timestamp).max();
    let mut guard = state.write().await;
    guard.chats.push(Chat {
        id: chat.clone(),
        last_message_ts,
        ..Default::default()
    });
    if !messages.is_empty() {
        guard.history.insert(chat, messages);
    }
    drop(guard);
}

fn inbound(chat: &Jid, id: &str, text: &str) -> InboundMessage {
    InboundMessage::builder()
        .message(Arc::new(text_message(text)))
        .info(Arc::new(msg_info(chat, chat, "Alice", id, false)))
        .build()
}

/// A WhatsApp edit re-sends the ORIGINAL stanza id with an edit payload.
fn edit_inbound(chat: &Jid, id: &str, text: &str) -> InboundMessage {
    let mut info = msg_info(chat, chat, "Alice", id, false);
    info.edit = EditAttribute::MessageEdit;
    InboundMessage::builder()
        .message(Arc::new(wa::Message {
            protocol_message: MessageField::some(wa::message::ProtocolMessage {
                key: MessageField::some(wa::MessageKey {
                    id: Some(id.to_string()),
                    ..Default::default()
                }),
                edited_message: MessageField::some(wa::Message::text(text.to_string())),
                ..Default::default()
            }),
            ..Default::default()
        }))
        .info(Arc::new(info))
        .build()
}

fn messages_event(messages: Vec<InboundMessage>) -> Event {
    Event::Messages(
        MessageBatch::builder()
            .messages(Arc::from(messages))
            .origin(BatchOrigin::Live)
            .build(),
    )
}

#[tokio::test]
async fn a_live_message_reaches_history_and_the_chat_list() {
    let (mut messenger, state, tx, qr, dir) = adapter().await;
    let client = messenger.current_client();
    let mut rx = tx.subscribe();
    let chat_jid = pn("15550000008");

    WhatsAppMessenger::handle_event(
        &Arc::new(messages_event(vec![inbound(&chat_jid, "m-1", "hello")])),
        &client,
        &state,
        &tx,
        &qr,
        dir.path(),
    )
    .await;

    let chat = ChatId::WhatsApp("15550000008@s.whatsapp.net".into());
    let guard = state.read().await;
    let history = guard.history.get(&chat).expect("history bucket");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].text, "hello");
    let row = guard.chats.iter().find(|c| c.id == chat).expect("chat row");
    assert_eq!(
        row.last_message_ts,
        Some(1000),
        "the row must join the list by recency"
    );
    drop(guard);

    let events = drained(&mut rx);
    assert_eq!(events.len(), 1, "one message, one announcement: {events:?}");
    assert!(
        matches!(&events[0], BackendEvent::MessageReceived(m) if m.text == "hello"),
        "got {events:?}"
    );

    messenger.disconnect().await.expect("dormant shutdown");
}

#[tokio::test]
async fn an_edit_updates_and_broadcasts_the_stored_message() {
    let (mut messenger, state, tx, qr, dir) = adapter().await;
    let client = messenger.current_client();
    let mut rx = tx.subscribe();
    let chat_jid = pn("15550000008");

    WhatsAppMessenger::handle_event(
        &Arc::new(messages_event(vec![inbound(&chat_jid, "m-1", "before")])),
        &client,
        &state,
        &tx,
        &qr,
        dir.path(),
    )
    .await;
    let _ = drained(&mut rx);

    WhatsAppMessenger::handle_event(
        &Arc::new(messages_event(vec![edit_inbound(
            &chat_jid, "m-1", "after",
        )])),
        &client,
        &state,
        &tx,
        &qr,
        dir.path(),
    )
    .await;

    let chat = ChatId::WhatsApp("15550000008@s.whatsapp.net".into());
    let guard = state.read().await;
    assert_eq!(guard.history[&chat][0].text, "after");
    assert_eq!(
        guard.history[&chat].len(),
        1,
        "an edit rewrites the message rather than adding one"
    );
    drop(guard);

    let events = drained(&mut rx);
    assert_eq!(events.len(), 1, "one edit, one update: {events:?}");
    assert!(
        matches!(&events[0], BackendEvent::MessageUpdated(m) if m.text == "after"),
        "got {events:?}"
    );

    messenger.disconnect().await.expect("dormant shutdown");
}

/// The Phase 4 deferral guarantee, observed at the boundary: an edit that
/// arrives before the message it rewrites is held, then applied and broadcast
/// the moment that message lands.
#[tokio::test]
async fn an_edit_that_outruns_its_base_converges_at_the_boundary() {
    let (mut messenger, state, tx, qr, dir) = adapter().await;
    let client = messenger.current_client();
    let mut rx = tx.subscribe();
    let chat_jid = pn("15550000008");

    WhatsAppMessenger::handle_event(
        &Arc::new(messages_event(vec![edit_inbound(
            &chat_jid, "m-1", "after",
        )])),
        &client,
        &state,
        &tx,
        &qr,
        dir.path(),
    )
    .await;
    assert!(
        drained(&mut rx).is_empty(),
        "an edit with no base message has nothing to report yet"
    );

    WhatsAppMessenger::handle_event(
        &Arc::new(messages_event(vec![inbound(&chat_jid, "m-1", "before")])),
        &client,
        &state,
        &tx,
        &qr,
        dir.path(),
    )
    .await;

    let chat = ChatId::WhatsApp("15550000008@s.whatsapp.net".into());
    let guard = state.read().await;
    assert_eq!(guard.history[&chat][0].text, "after");
    drop(guard);

    let events = drained(&mut rx);
    assert_eq!(
        events.len(),
        2,
        "the base and the deferred edit: {events:?}"
    );
    assert!(
        matches!(&events[0], BackendEvent::MessageReceived(_)),
        "got {events:?}"
    );
    assert!(
        matches!(&events[1], BackendEvent::MessageUpdated(m) if m.text == "after"),
        "got {events:?}"
    );

    messenger.disconnect().await.expect("dormant shutdown");
}

#[tokio::test]
async fn clearing_a_chat_reports_every_message_it_drops() {
    let (mut messenger, state, tx, qr, dir) = adapter().await;
    let client = messenger.current_client();
    let mut rx = tx.subscribe();
    let chat = ChatId::WhatsApp("15550000008@s.whatsapp.net".into());
    seed_row(
        &state,
        chat.clone(),
        vec![
            build_msg("m-1", "one", &chat, false),
            build_msg("m-2", "two", &chat, false),
        ],
    )
    .await;

    WhatsAppMessenger::handle_event(
        &Arc::new(Event::ClearChatUpdate(
            ClearChatUpdate::builder()
                .jid(pn("15550000008"))
                .delete_starred(false)
                .delete_media(false)
                .timestamp(chrono::Utc::now())
                .action(Box::new(wa::sync_action_value::ClearChatAction::default()))
                .from_full_sync(false)
                .build(),
        )),
        &client,
        &state,
        &tx,
        &qr,
        dir.path(),
    )
    .await;

    let events = drained(&mut rx);
    assert_eq!(events.len(), 1, "one clear, one announcement: {events:?}");
    match &events[0] {
        BackendEvent::MessageDeleted {
            chat: Some(cleared),
            message_ids,
        } => {
            assert_eq!(cleared, &chat);
            assert_eq!(
                message_ids,
                &vec![MessageId("m-1".into()), MessageId("m-2".into())],
                "the UI needs every id it must drop"
            );
        }
        other => panic!("expected MessageDeleted, got {other:?}"),
    }

    let guard = state.read().await;
    assert!(
        guard.history.get(&chat).is_none_or(|h| h.is_empty()),
        "cleared history must be gone"
    );
    assert_eq!(
        guard.chats[0].last_message_ts, None,
        "an empty chat no longer has recency"
    );
    drop(guard);

    messenger.disconnect().await.expect("dormant shutdown");
}

#[tokio::test]
async fn delete_for_me_removes_and_reports_only_the_target_message() {
    let (mut messenger, state, tx, qr, dir) = adapter().await;
    let client = messenger.current_client();
    let mut rx = tx.subscribe();
    let chat = ChatId::WhatsApp("15550000008@s.whatsapp.net".into());
    seed_row(
        &state,
        chat.clone(),
        vec![
            build_msg("m-1", "one", &chat, false),
            build_msg("m-2", "two", &chat, false),
        ],
    )
    .await;

    WhatsAppMessenger::handle_event(
        &Arc::new(Event::DeleteMessageForMeUpdate(
            DeleteMessageForMeUpdate::builder()
                .chat_jid(pn("15550000008"))
                .message_id("m-2".to_string())
                .from_me(false)
                .timestamp(chrono::Utc::now())
                .action(Box::new(
                    wa::sync_action_value::DeleteMessageForMeAction::default(),
                ))
                .from_full_sync(false)
                .build(),
        )),
        &client,
        &state,
        &tx,
        &qr,
        dir.path(),
    )
    .await;

    let events = drained(&mut rx);
    assert_eq!(events.len(), 1, "got {events:?}");
    match &events[0] {
        BackendEvent::MessageDeleted {
            chat: Some(target),
            message_ids,
        } => {
            assert_eq!(target, &chat);
            assert_eq!(message_ids, &vec![MessageId("m-2".into())]);
        }
        other => panic!("expected MessageDeleted, got {other:?}"),
    }

    let guard = state.read().await;
    let history = &guard.history[&chat];
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].message_id, MessageId("m-1".into()));
    assert_eq!(guard.chats[0].last_message_ts, Some(1000));
    drop(guard);

    messenger.disconnect().await.expect("dormant shutdown");
}

#[tokio::test]
async fn presence_updates_the_status_label_without_touching_recency() {
    let (mut messenger, state, tx, qr, dir) = adapter().await;
    let client = messenger.current_client();
    let mut rx = tx.subscribe();
    let chat = ChatId::WhatsApp("15550000008@s.whatsapp.net".into());
    seed_row(
        &state,
        chat.clone(),
        vec![build_msg("m-1", "hi", &chat, false)],
    )
    .await;

    WhatsAppMessenger::handle_event(
        &Arc::new(Event::Presence(
            PresenceUpdate::builder()
                .from(pn("15550000008"))
                .unavailable(false)
                .build(),
        )),
        &client,
        &state,
        &tx,
        &qr,
        dir.path(),
    )
    .await;

    let events = drained(&mut rx);
    assert_eq!(
        events.len(),
        1,
        "the row changed, so it is announced: {events:?}"
    );
    assert!(
        matches!(&events[0], BackendEvent::ChatUpdated(c) if c.status.as_deref() == Some("online")),
        "got {events:?}"
    );
    let guard = state.read().await;
    let row = guard.chats.iter().find(|c| c.id == chat).unwrap();
    assert_eq!(row.status.as_deref(), Some("online"));
    assert_eq!(
        row.last_message_ts,
        Some(1000),
        "presence must never move a chat in the list"
    );
    drop(guard);

    let _ = drained(&mut rx);
    WhatsAppMessenger::handle_event(
        &Arc::new(Event::Presence(
            PresenceUpdate::builder()
                .from(pn("15550000008"))
                .unavailable(true)
                .last_seen(chrono::Utc.timestamp_opt(900, 0).unwrap())
                .build(),
        )),
        &client,
        &state,
        &tx,
        &qr,
        dir.path(),
    )
    .await;

    let guard = state.read().await;
    let row = guard.chats.iter().find(|c| c.id == chat).unwrap();
    assert!(
        row.status
            .as_deref()
            .is_some_and(|s| s.starts_with("last seen ")),
        "expected a last-seen label, got {:?}",
        row.status
    );
    drop(guard);

    messenger.disconnect().await.expect("dormant shutdown");
}

#[tokio::test]
async fn a_mute_marks_the_visible_chat_row() {
    let (mut messenger, state, tx, qr, dir) = adapter().await;
    let client = messenger.current_client();
    let mut rx = tx.subscribe();
    let chat = ChatId::WhatsApp("15550000008@s.whatsapp.net".into());
    seed_row(&state, chat.clone(), vec![]).await;

    WhatsAppMessenger::handle_event(
        &Arc::new(Event::MuteUpdate(
            MuteUpdate::builder()
                .jid(pn("15550000008"))
                .timestamp(chrono::Utc::now())
                .action(Box::new(wa::sync_action_value::MuteAction {
                    muted: Some(true),
                    ..Default::default()
                }))
                .from_full_sync(false)
                .build(),
        )),
        &client,
        &state,
        &tx,
        &qr,
        dir.path(),
    )
    .await;

    let events = drained(&mut rx);
    assert_eq!(events.len(), 1, "got {events:?}");
    assert!(
        matches!(&events[0], BackendEvent::ChatUpdated(c) if c.status.as_deref() == Some("muted")),
        "got {events:?}"
    );
    let guard = state.read().await;
    assert_eq!(
        guard.chats[0].last_message_ts, None,
        "a mute must not invent recency"
    );
    drop(guard);

    messenger.disconnect().await.expect("dormant shutdown");
}

#[tokio::test]
async fn a_server_logout_surfaces_as_a_disconnect() {
    let (mut messenger, state, tx, qr, dir) = adapter().await;
    let client = messenger.current_client();
    let mut rx = tx.subscribe();

    WhatsAppMessenger::handle_event(
        &Arc::new(Event::LoggedOut(Box::new(
            LoggedOut::builder()
                .on_connect(false)
                .reason(ConnectFailureReason::LoggedOut)
                .build(),
        ))),
        &client,
        &state,
        &tx,
        &qr,
        dir.path(),
    )
    .await;

    let events = drained(&mut rx);
    assert_eq!(events.len(), 1, "one logout, one announcement: {events:?}");
    assert!(
        matches!(&events[0], BackendEvent::Disconnected(msg) if msg.contains("logged out")),
        "got {events:?}"
    );
    assert!(
        qr.read().await.is_none(),
        "a logout is not a pairing opportunity"
    );

    messenger.disconnect().await.expect("dormant shutdown");
}

#[tokio::test]
async fn a_clean_stream_recycle_is_transient_not_terminal() {
    let (mut messenger, state, tx, qr, dir) = adapter().await;
    let client = messenger.current_client();
    let mut rx = tx.subscribe();

    // A graceful, server-initiated recycle (EOF, or a normal close frame) is
    // the routine reconnect path: it must surface as a transient "reconnecting"
    // status — never a terminal error — while the run task stays alive.
    WhatsAppMessenger::handle_event(
        &Arc::new(Event::Disconnected(
            Disconnected::builder()
                .reason(DisconnectReason::StreamEnded)
                .build(),
        )),
        &client,
        &state,
        &tx,
        &qr,
        dir.path(),
    )
    .await;

    let events = drained(&mut rx);
    assert_eq!(events.len(), 1, "one recycle, one announcement: {events:?}");
    assert!(
        matches!(&events[0], BackendEvent::Status(s) if s.contains("reconnecting")),
        "a clean recycle is transient: got {events:?}"
    );

    // A genuine transport failure (read/IO error) must stay loud so it never
    // hides behind reconnect noise.
    WhatsAppMessenger::handle_event(
        &Arc::new(Event::Disconnected(
            Disconnected::builder()
                .reason(DisconnectReason::ReadError("connection reset".into()))
                .build(),
        )),
        &client,
        &state,
        &tx,
        &qr,
        dir.path(),
    )
    .await;

    let events = drained(&mut rx);
    assert_eq!(events.len(), 1, "got {events:?}");
    assert!(
        matches!(&events[0], BackendEvent::Disconnected(msg) if msg.contains("Disconnected!")),
        "a transport failure must stay loud: got {events:?}"
    );

    messenger.disconnect().await.expect("dormant shutdown");
}

/// Write paths must fail loudly on a provider that never authenticated: a
/// silently swallowed error is indistinguishable from a sent message.
#[tokio::test]
async fn write_operations_surface_not_authenticated_instead_of_success() {
    let (mut messenger, _, _, _, _) = adapter().await;
    let chat = ChatId::WhatsApp("15550000008@s.whatsapp.net".into());
    let id = MessageId("m-1".into());

    assert!(
        matches!(
            messenger
                .send(
                    &chat,
                    &crate::backend::OutboundMessage::Text { text: "hi".into() },
                    None
                )
                .await,
            Err(BackendError::NotAuthenticated)
        ),
        "send must not report success while logged out"
    );
    assert!(matches!(
        messenger.edit(&chat, &id, "x").await,
        Err(BackendError::NotAuthenticated)
    ));
    assert!(matches!(
        messenger.delete(&chat, &id).await,
        Err(BackendError::NotAuthenticated)
    ));
    assert!(matches!(
        messenger.set_read(&chat).await,
        Err(BackendError::NotAuthenticated)
    ));

    messenger.disconnect().await.expect("dormant shutdown");
}

/// `disconnect` tears down exactly once, and a provider that is already shut
/// down can never come back (`start` returns before it reaches the bot).
#[tokio::test]
async fn disconnect_is_idempotent_and_silent() {
    let (mut messenger, _, tx, _, _) = adapter().await;
    let mut rx = tx.subscribe();

    assert!(messenger.disconnect().await.is_ok());
    assert!(
        messenger.disconnect().await.is_ok(),
        "the second teardown is a no-op, not an error"
    );

    // Restarting belongs to `start()` (exercised through `StubMessenger` at
    // the TUI level, since a real `start()` would open the network); a
    // disconnect alone never emits — the bot was never running.
    assert!(
        rx.try_recv().is_err(),
        "a provider shut down before it ever started emits nothing"
    );
}
