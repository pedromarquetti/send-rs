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
