use std::time::SystemTime;

use qrcode::{QrCode, render::unicode::Dense1x2};
use ratatui::{layout::Flex, prelude::*};

use crate::backend::MessageAction;

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

pub fn message_preview(sender: &str, text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        sender.to_string()
    } else {
        format!("{sender}: {trimmed}")
    }
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

/// Helper function for handling text rendering (Vec<Line> line wrappin)
pub fn parse_text<'l>(lines: &mut Vec<Line<'l>>, text: String, width: usize) {
    if !text.is_empty() {
        for line in text.lines() {
            if line.len() <= width {
                lines.push(Line::from(line.to_string()));
            } else {
                let words: Vec<&str> = line.split_whitespace().collect();
                let mut curr_line = String::new();

                for word in words {
                    if curr_line.len() + word.len() < width {
                        if !curr_line.is_empty() {
                            curr_line.push(' ');
                        }
                        curr_line.push_str(word);
                    } else {
                        if !curr_line.is_empty() {
                            lines.push(Line::from(curr_line.clone()));
                        }
                        curr_line = word.to_string();
                    }
                }
                if !curr_line.is_empty() {
                    lines.push(Line::from(curr_line));
                }
            }
        }
    }

    lines.push(Line::from(""));
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

#[cfg(test)]
mod tests {
    use super::available_message_actions;
    use crate::backend::{ChatId, Message, MessageAction, MessageId};

    fn sample_message(from_me: bool, pending: bool, failed: bool) -> Message {
        Message {
            message_id: MessageId::from("msg-1"),
            chat: ChatId::Myself,
            sender: if from_me { "You".into() } else { "Them".into() },
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
