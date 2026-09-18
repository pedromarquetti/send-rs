use std::time::SystemTime;

use qrcode::{QrCode, render::unicode::Dense1x2};
use ratatui::{layout::Flex, prelude::*};

use crate::backend::MessageAction;

/// Relative "x ago" label for an epoch-seconds timestamp. Accepts any integer
/// unix-time type (`i32`, `i64`, …) so both backends share one implementation.
pub fn relative(ts: impl Into<i64>) -> String {
    let ts = ts.into();
    let delta = now().saturating_sub(ts).max(0);
    match delta {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", delta / 60),
        3600..=86399 => format!("{}h ago", delta / 3600),
        _ => format!("{}d ago", delta / 86400),
    }
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before unix epoch")
        .as_secs() as i64
}

pub fn available_message_actions(message: &crate::backend::Message) -> Vec<MessageAction> {
    if message.pending || message.failed {
        return Vec::new();
    }

    let mut actions = Vec::with_capacity(3);
    actions.push(MessageAction::Reply);

    if message.from_me {
        actions.extend([MessageAction::Edit, MessageAction::Delete]);
    }

    actions
}

/// helper function to create a centered rect using up certain percentage of the available rect `r`
pub fn popup_area(area: Rect, width: u16, height: u16) -> Rect {
    let vertical = Layout::vertical([Constraint::Length(height)]).flex(Flex::Center);

    let horizontal = Layout::horizontal([Constraint::Length(width)]).flex(Flex::Center);

    let [area] = vertical.areas(area);
    let [area] = horizontal.areas(area);
    area
}

pub fn calc_height(msg: &str, width: u16, area: Rect, footer: bool) -> u16 {
    let available_width = width.saturating_sub(4) as usize; // Account for borders and padding
    let mut total_lines = 0;

    for line in msg.lines() {
        if line.is_empty() {
            total_lines += 1;
        } else {
            // Calculate wrapped lines for this text line
            let chars = line.chars().count();
            let wrapped_lines = (chars / available_width).max(1);
            total_lines += wrapped_lines;
        }
    }

    // Add lines for footer message
    if footer {
        total_lines += 2; // "Press ESC to clear error" + empty line
    }

    // Add padding for borders
    (total_lines as u16)
        .saturating_add(4)
        .min(area.height.saturating_sub(4))
}

/// Render a QR code payload as a monospace string suitable for a terminal.
///
/// Uses the crate's [`qrcode::render::unicode::Dense1x2`] renderer, which packs
/// two vertical modules into each half-block glyph. This halves the line count
/// (e.g. a 45×45 module QR becomes 45 cols × ~23 lines) while keeping the code
/// square on screen, so it fits on short terminals and remains scannable.
///
/// Falls back to the raw payload (or a notice) when the payload cannot be
/// encoded as a QR code, so pairing output is never dropped silently.
pub fn render_qr(payload: &str) -> String {
    if payload.trim().is_empty() {
        return String::from("Waiting for a QR code...");
    }

    match QrCode::new(payload) {
        Ok(code) => code.render::<Dense1x2>().quiet_zone(true).build(),
        Err(_) => format!("QR encoding failed; scan this pairing payload:\n{payload}"),
    }
}

/// Word-wraps a text line into chunks that fit within `max_width` characters.
/// Words exceeding the limit are truncated and get `…` appended.
pub fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![text.to_string()];
    }

    let words: Vec<&str> = text.split_whitespace().collect();

    if words.is_empty() {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut current = String::new();

    for word in words {
        let word_width = word.chars().count();
        let fits = !current.is_empty() && current.chars().count() + 1 + word_width <= max_width;
        if current.is_empty() {
            if word_width > max_width {
                let truncated: String = word.chars().take(max_width.saturating_sub(1)).collect();
                chunks.push(format!("{truncated}…"));
            } else {
                current = word.to_string();
            }
        } else if fits {
            current.push(' ');
            current.push_str(word);
        } else {
            chunks.push(std::mem::take(&mut current));
            if word_width > max_width {
                let truncated: String = word.chars().take(max_width.saturating_sub(1)).collect();
                chunks.push(format!("{truncated}…"));
            } else {
                current = word.to_string();
            }
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push(text.to_string());
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::available_message_actions;
    use crate::backend::{ChatId, Message, MessageAction, MessageId};

    fn sample_message(from_me: bool, pending: bool, failed: bool) -> Message {
        Message {
            message_id: MessageId::from("msg-1"),
            chat: ChatId::Myself,
            sender: if from_me { "You".into() } else { "Them".into() },
            author_id: None,
            text: "hello".to_string(),
            timestamp: 0,
            from_me,
            msg_actions: Vec::new(),
            media: None,
            reply_to_id: None,
            reply_ctx: None,
            pending,
            failed,
        }
    }

    #[test]
    fn incoming_messages_only_offer_reply() {
        let message = sample_message(false, false, false);
        let actions = available_message_actions(&message);
        assert_eq!(actions, vec![MessageAction::Reply]);
    }

    #[test]
    fn outgoing_messages_offer_edit_and_delete() {
        let message = sample_message(true, false, false);
        let actions = available_message_actions(&message);
        assert_eq!(
            actions,
            vec![
                MessageAction::Reply,
                MessageAction::Edit,
                MessageAction::Delete
            ]
        );
    }

    #[test]
    fn pending_or_failed_messages_offer_no_actions() {
        let pending = sample_message(true, true, false);
        let failed = sample_message(false, false, true);
        assert!(available_message_actions(&pending).is_empty());
        assert!(available_message_actions(&failed).is_empty());
    }

    #[test]
    fn relative_buckets_are_deterministic_across_the_clock() {
        // Offsets are large enough that the bucket never straddles a boundary,
        // so these hold regardless of when the test runs.
        let anchor = super::now();
        assert_eq!(super::relative(anchor + 60), "just now");
        assert_eq!(super::relative(anchor - 35), "just now");
        assert_eq!(super::relative(anchor - 300), "5m ago");
        assert_eq!(super::relative(anchor - 7200), "2h ago");
        assert_eq!(super::relative(anchor - 90000), "1d ago");
    }

    #[test]
    fn relative_accepts_narrower_integer_time_types() {
        let anchor = super::now();
        assert_eq!(super::relative(anchor), super::relative(anchor as i32));
    }

    #[test]
    fn render_qr_produces_a_block_qr_for_a_valid_payload() {
        let art = super::render_qr("wa.me/some-pairing-payload");
        assert!(!art.is_empty(), "QR should render to a non-empty string");
        // A rendered QR contains the dark block glyph and newlines.
        assert!(art.contains('█'), "QR should use the dark block character");
        assert!(art.contains('\n'), "QR should be multi-line");
        // Quiet zone means a leading blank line of spaces.
        let first = art.lines().next().unwrap_or_default();
        assert!(
            first.chars().all(|c| c == ' '),
            "quiet zone should be blank"
        );

        // Dense1x2 packs two vertical modules per line, so the result is compact
        // (~half as many rows as columns). This keeps the QR square on screen
        // while fitting short terminals, and is the property login.rs relies on
        // to size the QR area.
        let width = art.lines().map(|l| l.chars().count()).max().unwrap_or(0);
        let height = art.lines().count();
        assert!(
            height < width,
            "QR should be packed vertically (height {height} < width {width})"
        );
    }

    #[test]
    fn render_qr_fits_a_small_terminal() {
        // A realistic WhatsApp pairing payload must render compactly enough to
        // fit a roughly 50-col x 30-row login box (borders/rows included).
        let art =
            super::render_qr("2@uc_GT9B2wFQ6s8dVçY7tXm4pRjLnH3cKaZbNeMfDhOgPiAkJwE0xS1yT5uVzC9qWo");
        let width = art.lines().map(|l| l.chars().count()).max().unwrap_or(0);
        let height = art.lines().count();
        assert!(
            width <= 50 && height <= 30,
            "QR should fit a small terminal, got {width}x{height}"
        );
    }

    #[test]
    fn render_qr_falls_back_for_empty_payload() {
        let art = super::render_qr("");
        assert!(
            art.contains("Waiting for a QR code..."),
            "empty payload should show a waiting notice, got: {art:?}"
        );
    }
}
