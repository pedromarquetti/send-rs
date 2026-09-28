use crate::backend::ChatId;

use super::state::{SharedState, WhatsAppState};
use std::str::FromStr;
use std::sync::Arc;
use whatsapp_rust::Client;
use whatsapp_rust::prelude::Jid;

/// Canonicalize the chat key for a message sent by the user's own account.
///
/// Live self-messages arrive with the chat keyed by the account's own LID, but
/// the HistorySync snapshot (and thus the persisted cache) stores the
/// self-chat under the own phone number. Rewrite the former to the latter so
/// both paths land in the same chat and no transient LID chat appears that
/// would vanish on the next restart.
pub(super) fn own_chat_id(
    own_lid: Option<&Jid>,
    own_pn: Option<&Jid>,
    chat: ChatId,
    from_me: bool,
) -> ChatId {
    if !from_me {
        return chat;
    }

    let ChatId::WhatsApp(raw) = &chat else {
        return chat;
    };

    let is_own_lid = own_lid.is_some_and(|lid| lid.to_non_ad_string().as_str() == raw.as_str());

    if is_own_lid && let Some(pn) = own_pn {
        return ChatId::WhatsApp(pn.to_non_ad_string());
    }

    chat
}

/// Whether a chat key is the account's own self-chat (own LID or own phone
/// number). Used to label the "message yourself" thread "Myself" instead of a
/// formatted number.
pub(super) fn is_self_chat(chat: &ChatId, own_lid: Option<&str>, own_pn: Option<&str>) -> bool {
    let ChatId::WhatsApp(raw) = chat else {
        return false;
    };
    own_lid == Some(raw.as_str()) || own_pn == Some(raw.as_str())
}

/// Build the `@s.whatsapp.net` chat id for a phone-number user-part.
pub(super) fn pn_chat_id(pn_user: &str) -> ChatId {
    ChatId::WhatsApp(Jid::pn(pn_user).to_non_ad_string())
}

/// Rewrite a `@lid` chat key to its phone-keyed twin when the local LID↔PN
/// mapping knows the peer and the PN row already exists (the row the user
/// opens). Peers never addressed by phone keep the LID key.
pub(super) fn fold_lid_key(key: &str, st: &WhatsAppState) -> ChatId {
    let Ok(jid) = Jid::from_str(key) else {
        return ChatId::WhatsApp(key.to_string());
    };
    if !jid.is_lid() {
        return ChatId::WhatsApp(key.to_string());
    }

    let Some(pn_user) = st.lid_pn.get(jid.user_base()) else {
        return ChatId::WhatsApp(key.to_string());
    };

    let pn_chat = pn_chat_id(pn_user);
    if st.chats.iter().any(|c| c.id == pn_chat) {
        pn_chat
    } else {
        ChatId::WhatsApp(key.to_string())
    }
}

/// Canonicalize a chat key to the conversation row the TUI actually opens.
///
/// WhatsApp addresses 1:1 threads by LID on the wire, but the phone's history
/// sync may key the same thread by the peer's phone number. A message or
/// conversation that arrives under one form must land in the row the user is
/// looking at, so a `@lid` chat is rewritten to its phone-keyed row when that
/// row already exists. The mapping is learned from the client's local LID↔PN
/// cache (and persisted) on first sight (offline callers pass `None` and only
/// fold through what is already known); see [`fold_lid_key`] for the read-only
/// side that never needs a client.
pub(super) async fn canonical_chat_id(
    client: Option<&Arc<Client>>,
    state: &SharedState,
    chat: ChatId,
) -> ChatId {
    let ChatId::WhatsApp(raw) = &chat else {
        return chat;
    };

    let Ok(jid) = Jid::from_str(raw) else {
        return chat;
    };

    if !jid.is_lid() {
        return chat;
    }

    let known = state.read().await.lid_pn.contains_key(jid.user_base());

    if !known {
        let Some(client) = client else {
            return chat;
        };
        let Ok(Some(entry)) = client.get_lid_pn_entry(&jid).await else {
            return chat;
        };
        state
            .write()
            .await
            .lid_pn
            .insert(jid.user_base().to_string(), entry.phone_number.to_string());
    }

    let st = state.read().await;
    fold_lid_key(raw, &st)
}
