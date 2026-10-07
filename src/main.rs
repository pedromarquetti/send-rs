// dead_code is only allowed in tests
#![cfg_attr(test, allow(dead_code))]

mod audio;
mod backend;
mod config;
mod file;
mod helpers;
mod notify;
mod tui;

use anyhow::Result;
use backend::MessengerKind;
use clap::Parser;
use tracing::{error, info};

use crate::{
    backend::{mock::MockMessenger, telegram::TelegramMessenger, whatsapp::WhatsAppMessenger},
    config::Config,
};

/// Command-line arguments
#[derive(Parser, Debug)]
#[command(name = "sendrs", version, about = "Unified WhatsApp + Telegram TUI")]
struct Args {
    /// Run with mock providers (no credentials needed, demo data only)
    #[arg(long, short)]
    mock: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // In mock mode, isolate config to a temp directory so we never touch
    // the user's real config.toml, chats.json, session DBs, or wa.db.
    if args.mock {
        let mock_config_dir =
            std::env::temp_dir().join(format!("senders-mock-{}", std::process::id()));

        file::create_dir_all(&mock_config_dir)?;

        Config::set_config_dir_override(mock_config_dir.clone())?;

        info!(
            "Mock mode: using isolated config dir {}",
            mock_config_dir.display()
        );
    }

    let log_path = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("sender")
        .join("sender.log");

    if let Some(parent) = log_path.parent() {
        let _ = file::create_dir_all(parent);
    }

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    // this is a fix for an audio playback integration i was having:
    // text was being written to the TUI instead of being logged/displayed properly
    // TODO: do a recheck if this is needed
    // rustix only exposes `stdio` on Unix, so Windows keeps stderr on the console.
    #[cfg(not(windows))]
    let stderr_redirected = rustix::stdio::dup2_stderr(&log_file).is_ok();
    #[cfg(not(windows))]
    if !stderr_redirected {
        eprintln!(
            "warning: could not redirect stderr to {}",
            log_path.display()
        );
    }
    #[cfg(windows)]
    let stderr_redirected = false;

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

    #[cfg(not(windows))]
    info!(
        stderr_redirected,
        "stderr redirected to the log file so audio diagnostics stay out of the TUI"
    );
    #[cfg(windows)]
    info!("stderr is not redirected on Windows: audio diagnostics may reach the console");

    let first_boot = !config::Config::exists();
    let mut config = config::Config::load()?.unwrap_or_default();
    let keymap = config.keys.parse()?;

    if first_boot {
        config.save_config()?;
    }

    info!("sender v{} starting", env!("CARGO_PKG_VERSION"));

    let mut messengers: Vec<MessengerKind> = Vec::new();

    if args.mock {
        // Mock mode: only initialize mock providers, skip real ones entirely.
        // This gives a clean demo experience without credentials.
        info!("Mock mode: initializing mock providers");

        let tg_mock = MockMessenger::new("Telegram");
        tg_mock.spawn_incoming_messages();
        messengers.push(MessengerKind::Mock(backend::Provider::Telegram, tg_mock));

        let wa_mock = MockMessenger::new("WhatsApp");
        wa_mock.spawn_incoming_messages();
        messengers.push(MessengerKind::Mock(backend::Provider::WhatsApp, wa_mock));

        config.providers.telegram.enabled = true;
        config.providers.whatsapp = true;
    } else {
        // normal, non-mock run
        // TODO: Check if grammers can be configured so our client tells Telegram to not send
        // notifications to other devices while Send-rs is open
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

        // Construct WhatsApp even while disabled, so the settings screen can
        // offer an explicit "enable" action. Construction is local-only (sqlite
        // store + JSON cache); the transport is opened later by `start()`, which
        // the TUI calls after registering the backend event receivers and only
        // for a provider the configuration enables. See the whatsapp module
        // docs for the full lifecycle invariant.
        {
            let whatsapp_path = Config::user_config_dir()?.join("wa.db");

            if let Some(parent) = whatsapp_path.parent() {
                let _ = file::create_dir_all(parent);
            }

            match WhatsAppMessenger::new(whatsapp_path.to_string_lossy().to_string()).await {
                Ok(wa) => {
                    if config.providers.whatsapp {
                        info!("WhatsApp: enabled, provider will start in the TUI");
                    } else {
                        info!("WhatsApp: enabled=false, provider stays dormant");
                    }
                    messengers.push(MessengerKind::WhatsApp(wa));
                }
                Err(e) => {
                    error!("WhatsApp init failed: {e}");
                }
            }
        }
    }

    // In mock mode we don't want to open settings on first boot; land in the chat list.
    let open_settings = first_boot && !args.mock;

    tui::run(config, keymap, messengers, open_settings).await
}
