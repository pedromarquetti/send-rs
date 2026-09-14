# AGENTS.md

Sender is a ratatui TUI that unifies WhatsApp and Telegram in one interface. A
single binary (`senders`, Rust edition 2024, Tokio, async-trait) exposes both
providers behind one `Messenger` trait; the UI never touches provider-specific
types.

## Commands

- `cargo build` — compile (no network needed for build)
- `cargo test` — all tests are inline `#[cfg(test)]` modules; run fully offline.
  There are no integration/network tests. `cargo test` must pass before
  finishing.
- `cargo clippy -- -D warnings` — required; keep it clean (the repo treats
  clippy as a gate).
- `cargo run` — requires Telegram `api_id`/`api_hash`
  (`~/.config/senders/config.toml`) or it will skip Telegram. Logs append to
  `~/.local/share/sender/sender.log` (`tracing`, level controlled by
  `RUST_LOG`).

## Concepts

- Messenger/Provider - The messaging service this apps integrates with
  (Telegram/WhatsApp)
- chat - The page/widget that displays the conversation itself
- chat list - The list of chats returned by the Messenger
- message - Messages from the providers: a group of messages are rendered in a chat

## Project layout

- `src/backend/mod.rs` — `Messenger` trait,
  `Chat`/`Message`/`BackendEvent`/`BackendError`, `MessageId`/`ChatId` (provider
  routing), the `MessengerKind` delegation enum/macro.
- `src/backend/telegram.rs` — grammers implementation (reference backend).
- `src/backend/whatsapp.rs` — whatsapp-rust implementation (stub/in progress).
- `src/backend/mock.rs` — `MockMessenger` used by startup mode and tests.
- `src/config.rs` — keymap parsing, provider config, save/load.
- `src/main.rs` — logging, config load, provider construction.
- `src/tui/mod.rs` — event loop, key dispatch, rendering.
- `src/tui/state.rs` — `AppState`: focus, selection, drafts, login flow, backend
  events.
- `src/tui/chat/mod.rs` — `ChatState`, `OpenChat { chat, history }`, per-chat
  scroll/drafts.
- `src/tui/chat/chat_list.rs` — Chat list (displayed at the side)
- `chat_widget.rs` — Chat view.

## Backend provider model (core invariants)

- Every provider implements `Messenger` (`#[async_trait]`). `MessengerKind`
  wraps the concrete types and delegates via the `delegate!` macro; add new
  trait methods to BOTH the macro list and every impl.
- Providers must be provider-neutral at the boundary: map all errors to
  `BackendError` (`From<grammers_client::InvocationError>` and
  `From<whatsapp_rust::ClientError>` exist), and emit `BackendEvent`s over a
  `broadcast` channel (`subscribe()`).
- `ChatId` routes to a provider via `to_provider()`; never pass a chat to the
  wrong backend. `MessageId` is an opaque string — Telegram code converts via
  `to_i32()`.
- `Chat` carries `last_message_ts` (recency) and `pinned`; the chat list is
  merged across providers and sorted pinned-first, then by recency. Chat list
  work in progress — keep the `chat_list`/`ChatState` invariants intact.
- Event flow: provider → `BackendEvent` → `AppState::handle_backend_event` →
  `ChatState`. Push updates are authoritative where available; polling
  (`chat_poll_interval_secs`, `sidebar_sync_secs`) is the fallback.

## Backend integration docs — read before editing providers

### Telegram (grammers). Crate source is read-only reference:

`~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/grammers-client-0.10.0/`

- `src/client/net.rs` — `Client::new`, SenderPool runner spawn, `disconnect`
- `src/client/auth.rs` — `is_authorized`, `request_login_code`, `sign_in`,
  `bot_sign_in`, `check_password`, `sign_out`
- `src/client/messages.rs` — `send_message`, `edit_message`, `delete_messages`,
  `iter_messages`
- `src/client/dialogs.rs` — `iter_dialogs` (server returns pinned-first, then
  last-activity desc)
- `src/client/updates.rs` — `stream_updates` / `UpdatesConfiguration`
- `src/peer/dialog.rs`, `src/message/message.rs` — `Dialog`/`Message` (`date()`,
  `edit`, `delete`)
- `grammers-session-0.10.0/src/storages/sqlite.rs` — `SqliteSession`
- `examples/echo.rs`, `examples/dialogs.rs` — canonical lifecycle (connect is
  lazy; spawn the runner)
- Online: <https://docs.rs/grammers-client>,
  <https://codeberg.org/Lonami/grammers>

### WhatsApp (whatsapp-rust). Consult the official upstream source/docs; the local

dep is temporary and must not be treated as authoritative:

- Repo (source): <https://github.com/oxidezap/whatsapp-rust> — key files:
  `src/bot.rs` (Bot builder/lifecycle,
  `MessageContext::edit_message`/`revoke_message`), `src/client/messaging.rs`
  (`edit_message` + variants, send APIs), `src/send/actions.rs` +
  `src/send/mod.rs` (`revoke_message`, `RevokeType`, pins),
  `src/features/events.rs` + `src/features/message_edit.rs` (inbound event
  model), `src/features/newsletter.rs` (channel edit/revoke),
  `storages/sqlite-storage/` (`SqliteStore`), `wacore/src/store/traits.rs`
  (`Backend` trait).
- Protocol ground truth: <https://github.com/oxidezap/whatspec> (structured
  WhatsApp Web IR).
- Pin version expectations to the `whatsapp-rust` entry in `Cargo.toml` and
  check `Cargo.lock` before assuming any API; verify signatures in the pinned
  source above rather than guessing from memory.

Both deps are version-pinned (`grammers-client 0.10`, `whatsapp-rust` entry in
`Cargo.toml`). Do not suggest APIs from older/newer versions without checking
the pinned source above.

## Conventions

- Follow existing patterns (delegation macro, error mapping, event flow). Match
  the surrounding code's style; do not reformat unrelated code.
- Async/await everywhere; `async-trait` for trait impls; `tracing::*` for
  logging (never `println!`/`eprintln!`).
- New errors map to `BackendError`; keep provider names in the `Other(String)`
  payload for clarity.
- Add tests for behavior changes: backend tests use `MockMessenger`; `AppState`
  tests use `StubMessenger` (see `src/tui/state.rs` tests). Keep tests offline
  and deterministic.
- Ask before creating new structs/one-use-functions
- Do not create single use or 1 line functions.
- Always try to reuse already existing functions.
- Do not add provider-specific code to shared data. Shared structs (e.g.
  `Message`, `Chat`, the `Messenger` trait, `BackendEvent`) must stay
  provider-neutral. Example: Telegram does not know what a JID is.
- If a provider needs extra state, keep it in that provider's own module/state
  (always remember to check for single-use/already-existing methods).
- If you must put something in a shared struct, first confirm it is genuinely
  needed by the trait boundary, then make it modular/generic (opaque,
  provider-neutral naming and types) so any provider can implement it.

## Boundaries / do not touch

- Cargo registry crate sources (grammers) — read-only reference.
- Generated/session artifacts: `*.sqlite`, `wa.db`, `config.toml` are user data;
  do not commit or hard-code values (never commit `api_hash` or credentials).
- `Cargo.toml` pins `whatsapp-rust` with `default-features = false` +
  `sqlite-storage` (no bundled SQLite) to avoid duplicate `sqlite3` symbols
  against grammers-session's `libsql-ffi` — never enable
  `sqlite-storage-bundled`, and read the Cargo.toml comment before changing the
  dependency features.

## Security

- `api_id`/`api_hash`/session keys are user secrets. Never log them or print to
  terminal.
- Do not copy-paste rules or code from online sources verbatim without review.
- Do not, when creating tests, use real user data (such as phone numbers,
  passwords or names). This includes private ids/JIDs (`@lid`,
  `@s.whatsapp.net`, `@g.us` user parts), group ids, push names, or any value
  copied from the logs — those are the user's private data. Use clearly fake
  placeholders; the existing tests use the `1555000000X@s.whatsapp.net`,
  `NNN...@lid` and `NNN@g.us` conventions. - The data gathered from the logs is
  probably private, so, for testing, create mock
