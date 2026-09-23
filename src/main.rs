// dead_code is only allowed in tests
#![cfg_attr(test, allow(dead_code))]

mod backend;
mod config;
mod helpers;
mod notify;
mod tui;

use anyhow::Result;
use backend::MessengerKind;
use tracing::{error, info};

use crate::{
    backend::{telegram::TelegramMessenger, whatsapp::WhatsAppMessenger},
    config::Config,
};

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

    // this is a fix for an audio playback integration i was having:
    // text was being written to the TUI instead of being logged/displayed properly
    // TODO: do a recheck if this is needed
    let stderr_redirected = rustix::stdio::dup2_stderr(&log_file).is_ok();
    if !stderr_redirected {
        eprintln!(
            "warning: could not redirect stderr to {}",
            log_path.display()
        );
    }

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

    // cpal (and other audio deps) emit through the `log` facade rather than
    // `tracing`; bridge it so their records land in sender.log too.
    let _ = tracing_log::LogTracer::init();

    info!(
        stderr_redirected,
        "stderr redirected to the log file so audio diagnostics stay out of the TUI"
    );

    let first_boot = !config::Config::exists();
    let config = config::Config::load()?.unwrap_or_default();
    let keymap = config.keys.parse()?;

    if first_boot {
        config.save_config()?;
    }

    info!("sender v{} starting", env!("CARGO_PKG_VERSION"));

    let mut messengers: Vec<MessengerKind> = Vec::new();

    // Initialize Telegram only when it is enabled or configured. A configured
    // but disabled account is retained so it can be enabled or authenticated
    // from settings; disabled providers are still excluded from chat loading.
    // Dialogs are fetched lazily on first chat-list sync; per-chat operations
    // self-heal against a cold dialog cache.
    let telegram = &config.providers.telegram;
    if telegram.enabled || telegram.has_credentials() {
        if !telegram.has_credentials() {
            info!("Telegram is enabled but has no credentials, skipping");
        } else {
            let session_dir = Config::user_config_dir()?;
            match TelegramMessenger::new(
                session_dir,
                telegram.api_id,
                &telegram.api_hash,
                config.sync_update_state_secs,
                telegram.enabled,
            )
            .await
            {
                Ok(tg) => {
                    info!("Telegram messenger initialized");
                    messengers.push(MessengerKind::Telegram(tg));
                }
                Err(e) => {
                    error!("Telegram init failed: {e}");
                }
            }
        }
    } else {
        info!("Telegram: disabled and not configured, skipping");
    }

    // Initialize WhatsApp when it is enabled. Unlike Telegram, WhatsApp pairing
    // is event-driven (QR code), so the messenger is constructed unconditionally
    // when enabled and the bot is started on first TUI subscription.
    {
        let whatsapp_path = Config::user_config_dir()?.join("wa.db");

        if let Some(parent) = whatsapp_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        match WhatsAppMessenger::new(whatsapp_path.to_string_lossy().to_string()).await {
            Ok(wa) => {
                info!("WhatsApp messenger initialized");
                // Start the transport only when enabled; a disabled provider
                // stays inert (no connection, no QR) until the user enables it.
                if config.providers.whatsapp {
                    wa.start();
                } else {
                    info!("WhatsApp: enabled=false, keeping provider inactive");
                }
                messengers.push(MessengerKind::WhatsApp(wa));
            }
            Err(e) => {
                error!("WhatsApp init failed: {e}");
            }
        }
    }

    tui::run(config, keymap, messengers, first_boot).await
}
