#![allow(dead_code)]

mod backend;
mod config;
mod helpers;
mod notify;
mod tui;

use anyhow::Result;
use backend::MessengerKind;
use backend::mock::MockMessenger;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    let log_path = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("sender")
        .join("sender.log");

    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_ansi(false)
        .with_timer(tracing_subscriber::fmt::time::ChronoLocal::new(
            "%Y-%m-%dT%H:%M:%S%.6f".to_string(),
        ))
        .with_writer(std::sync::Mutex::new(log_file))
        .init();

    let first_boot = !config::Config::exists();
    let config = config::Config::load()?.unwrap_or_default();
    let keymap = config.keys.parse()?;

    if first_boot {
        config.save()?;
    }

    info!("sender v{} starting", env!("CARGO_PKG_VERSION"));

    let mut messengers: Vec<MessengerKind> = Vec::new();

    // Create the Telegram messenger if credentials are configured.
    // The `enabled` flag in config controls whether chats are actually loaded.
    if config.providers.telegram.has_credentials() {
        let session_dir = config::Config::config_dir()?;
        match backend::telegram::TelegramMessenger::new(
            session_dir,
            config.providers.telegram.api_id,
            &config.providers.telegram.api_hash,
            config.sync_update_state_secs,
        )
        .await
        {
            Ok(tg) => {
                info!("Telegram messenger initialized");
                messengers.push(MessengerKind::Telegram(tg));
            }
            Err(e) => {
                tracing::error!("Telegram init failed: {e}");
            }
        }
    } else {
        info!("Telegram: no credentials configured, skipping");
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
