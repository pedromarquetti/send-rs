#![allow(dead_code)]

mod backend;
mod config;
mod helpers;
mod notify;
mod tui;

use anyhow::Result;
use backend::MessengerKind;
use backend::mock::MockMessenger;

#[tokio::main]
async fn main() -> Result<()> {
    let first_boot = !config::Config::exists();
    let config = config::Config::load()?.unwrap_or_default();
    let keymap = config.keys.parse()?;

    if first_boot {
        config.save()?;
    }

    let mut messengers: Vec<MessengerKind> = Vec::new();

    // Create the Telegram messenger if credentials are configured.
    // The `enabled` flag in config controls whether chats are actually loaded.
    if config.providers.telegram.has_credentials() {
        let session_dir = config::Config::config_dir()?;
        match backend::telegram::TelegramMessenger::new(
            session_dir,
            config.providers.telegram.api_id,
            &config.providers.telegram.api_hash,
        )
        .await
        {
            Ok(tg) => messengers.push(MessengerKind::Telegram(tg)),
            Err(e) => eprintln!("Telegram init failed: {e}"),
        }
    }

    // TODO: replace MockMessenger with real WhatsApp backend.
    // BUG: sqlite-storage disabled due to libsql-ffi / libsqlite3-sys symbol conflict.
    // whatsapp-rust's feature unification forces bundled SQLite even with
    // default-features = false. See Cargo.toml for details.
    let mock_whatsapp = MockMessenger::new("WhatsApp");
    mock_whatsapp.spawn_incoming_messages();
    messengers.push(MessengerKind::WhatsApp(mock_whatsapp));

    tui::run(config, keymap, messengers, first_boot).await
}
