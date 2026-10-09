//! WhatsApp provider, built on `whatsapp-rust`.
//!
//! Split by concern so each piece stays readable on its own:
//!
//! - [`messenger`]: the `WhatsAppMessenger` type, its lifecycle, and the
//!   `Messenger` implementation the TUI talks to.
//! - [`events`]: routing of inbound `whatsapp-rust` events (connect, QR,
//!   sync-side chat list mutations) into `BackendEvent`s.
//! - [`state`]: the persisted account snapshot: chats, history, pushnames.
//! - [`ids`]: chat addressing, including `@lid` <-> `@s.whatsapp.net` folding.
//! - [`convert`]: `waproto` messages and names to the provider-neutral types.
//! - [`media`]: media references and kind detection.
//! - [`sync`]: history sync, conversation filtering, name enrichment.
//! - [`transport`]: address-racing WebSocket connect.
//!
//! # Lifecycle
//!
//! ```text
//! constructed/dormant -> explicitly enabled -> started -> connected/paired
//! constructed/dormant -> disabled (no transport, no provider fetch)
//! started -> disconnected/reconnecting
//! started -> graceful shutdown
//! ```
//!
//! `WhatsAppMessenger::new` only opens the local sqlite store and JSON cache,
//! and `subscribe` only installs a receiver: neither may start network work.
//! [`Messenger::start`](crate::backend::Messenger::start) is the single
//! transition into `started`, it is idempotent, and the TUI calls it after every
//! backend receiver is registered so the first `Connected` / `QrCode` event is
//! never dropped. A provider that stays `dormant` therefore opens no WebSocket,
//! emits no QR/`Connected`, fetches no chats and posts no notifications.

mod convert;
mod events;
mod ids;
mod media;
mod messenger;
mod state;
mod sync;
mod transport;

#[cfg(test)]
mod tests;

pub use messenger::WhatsAppMessenger;
