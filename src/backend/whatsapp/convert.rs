use crate::backend::{ChatId, Message, MessageAction, MessageId, ReplyContext};

use super::media::handle_wa_media;
use super::state::WhatsAppState;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use tracing::debug;
use whatsapp_rust::prelude::{Jid, MessageExt, MessageInfo, wa};
use whatsapp_rust::wacore_binary::JidExt;
use whatsapp_rust::waproto::whatsapp::Message as WaMessage;

pub(super) fn message_actions(from_me: bool) -> Vec<MessageAction> {
    let mut actions = vec![MessageAction::Reply];
    if from_me {
        actions.extend([MessageAction::Edit, MessageAction::Delete]);
    }
    actions
}

/// Converts [`Message`] to [`WaMessage`] / [`MessageInfo`]
///
/// `info.source.chat` is the conversation JID (used as the chat id);
/// `info.source.sender` is the per-message author JID (the participant inside a
/// group, or the peer in a one-to-one chat). `info.push_name` already holds the
/// author's display name when `from_me` is false.
pub(super) fn to_senders_msg(
    chat: ChatId,
    info: &MessageInfo,
    msg: &WaMessage,
    state: &WhatsAppState,
) -> Message {
    let from_me = info.source.is_from_me;
    let sender = if from_me {
        "You".to_string()
    } else {
        resolve_sender_name(
            info.push_name.as_str(),
            &info.source.sender,
            &state.pushnames,
            &state.lid_pn,
        )
    };

    let media = handle_wa_media(msg);
    let text = if let Some(media) = &media {
        media
            .caption
            .clone()
            .filter(|c| !c.trim().is_empty())
            .unwrap_or_else(|| format!("{}, click to show", media.kind.label()))
    } else {
        msg.get_base_message()
            .text_content()
            .unwrap_or("")
            .to_string()
    };

    // INFO: view stanza definition in reply_stanza_id function docs
    let stanza_id = info.id.to_string();
    let reply_to_id = reply_stanza_id(msg).map(MessageId::from);

    let reply_ctx = reply_to_id.as_ref().and_then(|id| {
        state
            .by_stanza_id
            .get(&**id)
            .and_then(|(c, _)| state.history.get(c))
            .and_then(|msgs| msgs.iter().find(|m| m.message_id == *id))
            .map(|m| ReplyContext {
                id: m.message_id.clone(),
                sender: m.sender.clone(),
                text: m.text.clone(),
                timestamp: m.timestamp,
            })
    });

    Message {
        message_id: stanza_id.into(),
        chat,
        sender,
        author_id: Some(info.source.sender.to_string()),
        text,
        timestamp: info.timestamp.timestamp(),
        from_me,
        msg_actions: message_actions(from_me),
        media,
        reply_to_id,
        reply_ctx,
        pending: false,
        failed: false,
    }
}

/// Resolve a human-readable sender name for an inbound message.
///
/// Order: the message's own `push_name` (authoritative), then the push name
/// cache for the author JID. A LID author falls back to the phone-keyed push
/// name / formatted number via the local LID↔PN mapping. `"Unknown"` is
/// reserved for the truly unresolvable case so the TUI can keep a friendlier
/// fallback.
pub(super) fn resolve_sender_name(
    push_name: &str,
    sender: &Jid,
    pushnames: &HashMap<String, String>,
    lid_pn: &HashMap<String, String>,
) -> String {
    if !push_name.trim().is_empty() {
        return push_name.to_string();
    }

    let key = sender.to_non_ad_string();

    if let Some(name) = pushnames.get(&key)
        && !name.trim().is_empty()
    {
        return name.clone();
    }

    if sender.is_lid()
        && let Some(pn_user) = lid_pn.get(sender.user_base())
    {
        return name_from_lid_pn(pn_user, pushnames);
    }

    let bare = sender.user_base();

    if !bare.is_empty() {
        bare.to_string()
    } else {
        "Unknown".to_string()
    }
}

/// Format a peer phone number the way WhatsApp does for contacts it has no
/// name for: `15550000001` -> `+15 55 0000-0001`. Non-numeric input (a bare
/// LID, a JID string) degrades to `+{digits}`.
pub(super) fn format_pn(pn: &str) -> String {
    let digits: String = pn.chars().filter(|c| c.is_ascii_digit()).collect();

    if digits.len() < 8 {
        return format!("+{digits}");
    }

    let (cc, rest) = digits.split_at(2);
    let (area, number) = rest.split_at(2);
    let (prefix, suffix) = number.split_at(number.len() - 4);
    format!("+{cc} {area} {prefix}-{suffix}")
}

/// Name for a peer whose phone number we only learned locally from the LID↔PN
/// mapping (`Client::get_lid_pn_entry`): the synced push name wins when the
/// cache has one, otherwise the formatted number — the same fallback a thread
/// keyed by PN gets from `resolve_conversation_name`.
pub(super) fn name_from_lid_pn(pn_user: &str, pushnames: &HashMap<String, String>) -> String {
    let pn_key = Jid::pn(pn_user).to_non_ad_string();

    pushnames
        .get(&pn_key)
        .filter(|n| !n.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| format_pn(pn_user))
}

/// Resolve the display name for a conversation from a history-sync record.
///
/// For groups the subject (`conversation.name`) is authoritative. For one-to-one
/// chats the saved contact name (`conversation.name`) or display name wins, with
/// the push-name cache, a usync-learned name (username or verified business
/// name), the formatted phone number and a bare JID as fallbacks.
///
/// Chats may be keyed by LID (`...@lid`); the thread's `pn_jid` (when present) is used as the
/// push-name / usync lookup key so a LID-keyed chat still resolves to the
/// contact's saved name.
///
/// The returned `bool` flags verified-business peers so the TUI can render a check mark.
pub(super) fn resolve_conversation_name(
    conv: &wa::Conversation,
    pushnames: &HashMap<String, String>,
    usync_names: &HashMap<String, String>,
    usync_verified: &HashSet<String>,
) -> (String, bool) {
    if let Ok(jid) = Jid::from_str(&conv.id)
        && jid.is_group()
    {
        debug!("{jid} is a group named '{:?}' ", conv.name);
        return (
            conv.name
                .clone()
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| conv.id.clone()),
            false,
        );
    }

    // The peer's phone number when the thread is keyed by LID.
    let pn_key = conv
        .pn_jid
        .clone()
        .filter(|j| !j.trim().is_empty())
        .unwrap_or_else(|| conv.id.clone());

    if let Some(name) = conv
        .name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .or_else(|| conv.display_name.clone().filter(|n| !n.trim().is_empty()))
    {
        return (name, false);
    }

    if let Some(name) = pushnames.get(&pn_key).filter(|n| !n.trim().is_empty()) {
        return (name.clone(), false);
    }

    if let Some(name) = usync_names.get(&pn_key).filter(|n| !n.trim().is_empty()) {
        return (name.clone(), usync_verified.contains(&pn_key));
    }

    if let Ok(jid) = Jid::from_str(&pn_key)
        && jid.is_pn()
    {
        return (format_pn(jid.user_base()), false);
    }

    if let Ok(jid) = Jid::from_str(&conv.id) {
        let bare = jid.user_base().to_string();
        if !bare.is_empty() {
            return (bare, false);
        }
    }

    (conv.id.clone(), false)
}

/// The stanza id this message quotes, if any, from its `ContextInfo`.
///
/// Stanza is a XML-like structure exchanged between client and server.
/// Each Stanza contains data that identifies and populates the message
/// So basically, Stanza == Message
fn reply_stanza_id(msg: &wa::Message) -> Option<String> {
    let base = msg.get_base_message();
    let ctx = base
        .extended_text_message
        .as_option()
        .and_then(|etm| etm.context_info.as_option())
        .or_else(|| {
            base.image_message
                .as_option()
                .and_then(|i| i.context_info.as_option())
        })
        .or_else(|| {
            base.video_message
                .as_option()
                .and_then(|v| v.context_info.as_option())
        })
        .or_else(|| {
            base.document_message
                .as_option()
                .and_then(|d| d.context_info.as_option())
        });
    ctx.and_then(|c| c.stanza_id.clone())
}
