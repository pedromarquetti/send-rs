#![allow(dead_code)]

mod backend;
mod config;
mod notify;
mod tui;

use anyhow::Result;
use backend::mock::MockMessenger;
use backend::Messenger;

#[tokio::main]
async fn main() -> Result<()> {
    let first_boot = !config::Config::exists();
    let config = config::Config::load()?.unwrap_or_default();
    let keymap = config.keys.parse()?;
    if first_boot {
        config.save()?;
    }

    let mock_telegram = MockMessenger::new("Telegram");
    mock_telegram.spawn_incoming_messages();
    let mock_whatsapp = MockMessenger::new("WhatsApp");
    mock_whatsapp.spawn_incoming_messages();
    let messengers: Vec<Box<dyn Messenger>> = vec![Box::new(mock_telegram), Box::new(mock_whatsapp)];

    tui::run(config, keymap, messengers, first_boot).await
}
