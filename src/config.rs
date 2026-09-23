use anyhow::{Context, Result, anyhow, bail};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Serialize};
use std::{fs::create_dir_all, path::PathBuf};

use crate::backend::Chat;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub keys: KeymapConfig,
    pub providers: ProvidersConfig,
    pub max_write_lines: usize,
    /// How often (seconds) the TUI polls for new messages in the open chat.
    pub chat_poll_interval_secs: u64,
    /// How often (seconds) the Telegram client calls `sync_update_state`.
    /// This call saves chat data to the local Telegram DB
    pub sync_update_state_secs: u64,
    /// How often (seconds) the TUI polls for unread-count changes across all chats.
    pub chat_list_sync_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            keys: KeymapConfig::default(),
            providers: ProvidersConfig::default(),
            max_write_lines: 5,
            chat_poll_interval_secs: 10,
            sync_update_state_secs: 120,
            chat_list_sync_secs: 10,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ProvidersConfig {
    pub telegram: TelegramConfig,
    pub whatsapp: bool,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct TelegramConfig {
    pub enabled: bool,
    pub api_id: u32,
    pub api_hash: String,
}

impl TelegramConfig {
    pub fn has_credentials(&self) -> bool {
        self.api_id != 0 && !self.api_hash.is_empty()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct KeymapConfig {
    pub search_text: String,
    pub scroll_up: String,
    pub scroll_down: String,
    pub select: String,
    pub pane_prev: String,
    pub pane_next: String,
    pub dismiss: String,
    pub quit: String,
    pub open_settings: String,
    pub focus_write: String,
    pub scroll_to_top: String,
    pub scroll_to_bottom: String,
    pub send: String,
    pub newline: String,
    pub retry_connection: String,
    pub play_pause: String,
    pub seek_back: String,
    pub seek_forward: String,
}

impl Default for KeymapConfig {
    fn default() -> Self {
        Self {
            scroll_up: "k".into(),
            scroll_to_top: "g".into(),
            search_text: "/".into(),
            scroll_down: "j".into(),
            select: "enter".into(),
            pane_prev: "shift+tab".into(),
            pane_next: "tab".into(),
            dismiss: "esc".into(),
            quit: "ctrl+c".into(),
            open_settings: "s".into(),
            focus_write: "i".into(),
            scroll_to_bottom: "G".into(),
            send: "enter".into(),
            newline: "shift+enter".into(),
            retry_connection: "r".into(),
            play_pause: "space".into(),
            seek_back: "<".into(),
            seek_forward: ">".into(),
        }
    }
}

impl KeymapConfig {
    pub fn parse(&self) -> Result<Keymap> {
        Ok(Keymap {
            scroll_up: parse_key(&self.scroll_up)?,
            scroll_to_top: parse_key(&self.scroll_to_top)?,
            search_text: parse_key(&self.search_text)?,
            scroll_down: parse_key(&self.scroll_down)?,
            select: parse_key(&self.select)?,
            pane_prev: parse_key(&self.pane_prev)?,
            pane_next: parse_key(&self.pane_next)?,
            dismiss: parse_key(&self.dismiss)?,
            quit: parse_key(&self.quit)?,
            open_settings: parse_key(&self.open_settings)?,
            focus_write: parse_key(&self.focus_write)?,
            scroll_to_bottom: parse_key(&self.scroll_to_bottom)?,
            send: parse_key(&self.send)?,
            newline: parse_key(&self.newline)?,
            retry_connection: parse_key(&self.retry_connection)?,
            play_pause: parse_key(&self.play_pause)?,
            seek_back: parse_key(&self.seek_back)?,
            seek_forward: parse_key(&self.seek_forward)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    pub scroll_up: KeyEvent,
    pub search_text: KeyEvent,
    pub scroll_down: KeyEvent,
    pub select: KeyEvent,
    pub pane_next: KeyEvent,
    pub pane_prev: KeyEvent,
    pub dismiss: KeyEvent,
    pub quit: KeyEvent,
    pub open_settings: KeyEvent,
    pub focus_write: KeyEvent,
    pub scroll_to_top: KeyEvent,
    pub scroll_to_bottom: KeyEvent,
    pub send: KeyEvent,
    /// Enter (with a modifier) also inserts a newline; this key is honored if the terminal
    /// reports it (e.g. Shift+Enter on kitty-protocol terminals).
    pub newline: KeyEvent,
    /// Manually force a reconnection after a provider entered the
    /// "connection lost" state.
    pub retry_connection: KeyEvent,
    /// Toggle play/pause on the audio popup.
    pub play_pause: KeyEvent,
    /// Seek backward on the audio popup.
    pub seek_back: KeyEvent,
    /// Seek forward on the audio popup.
    pub seek_forward: KeyEvent,
}

/// Tests must never read or write a real user's config. Under `#[cfg(test)]`
/// every config path resolves to a single throwaway directory under the
/// system temp dir instead of `$XDG_CONFIG_HOME/senders`.
#[cfg(test)]
static TEST_CONFIG_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

impl Config {
    pub fn user_config_dir() -> Result<PathBuf> {
        #[cfg(test)]
        {
            if let Some(dir) = TEST_CONFIG_DIR.get() {
                return Ok(dir.clone());
            }
            let dir = std::env::temp_dir().join(format!("senders-test-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            let _ = TEST_CONFIG_DIR.set(dir.clone());
            Ok(dir)
        }

        #[cfg(not(test))]
        {
            dirs::config_dir()
                .or_else(dirs::home_dir)
                .map(|base| base.join("senders"))
                .ok_or_else(|| anyhow::anyhow!("could not determine a config directory"))
        }
    }

    pub fn config_file_path() -> Result<PathBuf> {
        Ok(Self::user_config_dir()?.join("config.toml"))
    }

    pub fn exists() -> bool {
        Self::config_file_path()
            .map(|path| path.exists())
            .unwrap_or(false)
    }

    pub fn load() -> Result<Option<Config>> {
        let path = Self::config_file_path()?;
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path)?;
        let config = toml::from_str(&raw)
            .with_context(|| format!("invalid config at {}", path.display()))?;
        Ok(Some(config))
    }

    /// main func to save the app config file
    pub fn save_config(&self) -> Result<()> {
        let path = Self::config_file_path()?;

        if let Some(parent) = path.parent() {
            create_dir_all(parent)?;
        }

        let raw = toml::to_string_pretty(self)?;
        std::fs::write(&path, raw)?;
        Ok(())
    }

    /// local list of chat list for faster boot time.
    pub fn save_chats(chats: &[Chat]) -> Result<()> {
        // Unit tests run from a clean slate: never write into a real user's
        // config dir, which would leak into other tests' `AppState::new`.
        if cfg!(test) {
            return Ok(());
        }

        let path = Self::user_config_dir()?.join("chats.json");

        if let Some(parent) = path.parent() {
            create_dir_all(parent)?;
        }

        let raw = serde_json::to_string_pretty(chats)?;
        std::fs::write(&path, raw)?;
        Ok(())
    }

    /// chats.json loader (chat list cache)
    pub fn load_chats() -> Result<Vec<Chat>> {
        // See `save_chats`: tests must not observe a real user's chat cache.
        if cfg!(test) {
            return Ok(vec![]);
        }

        let path = Self::user_config_dir()?.join("chats.json");

        if !path.exists() {
            return Err(anyhow!("Path does not exist"));
        }

        let raw = std::fs::read_to_string(&path)?;
        let chats = serde_json::from_str(&raw)?;
        Ok(chats)
    }
}

fn parse_key(spec: &str) -> Result<KeyEvent> {
    let parts: Vec<&str> = spec.trim().split('+').map(str::trim).collect();
    if parts.is_empty() || parts.iter().any(|p| p.is_empty()) {
        bail!("empty key spec '{spec}'");
    }

    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    for modifier in &parts[..parts.len() - 1] {
        match *modifier {
            "ctrl" | "control" => ctrl = true,
            "alt" | "option" => alt = true,
            "shift" => shift = true,
            "super" | "meta" | "cmd" | "cmdorctrl" => {}
            other => bail!("unsupported modifier '{other}' in '{spec}'"),
        }
    }

    let base = parts[parts.len() - 1];
    let code = match base {
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        "space" => KeyCode::Char(' '),
        "delete" => KeyCode::Delete,
        "insert" => KeyCode::Insert,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "pgup" | "pageup" => KeyCode::PageUp,
        "pgdn" | "pagedown" => KeyCode::PageDown,
        other
            if other.len() > 1
                && other.starts_with('f')
                && other[1..].chars().all(|c| c.is_ascii_digit()) =>
        {
            let number: u8 = other[1..].parse().expect("digits parse");
            if !(1..=12).contains(&number) {
                bail!("unsupported key '{other}' in '{spec}'");
            }
            KeyCode::F(number)
        }
        single if single.len() == 1 => {
            let c = single.chars().next().expect("single char");
            if !c.is_ascii() {
                bail!("non-ascii key '{single}' in '{spec}'");
            }
            if c.is_ascii_uppercase() {
                shift = true;
            }
            KeyCode::Char(c.to_ascii_lowercase())
        }
        other => bail!("unsupported key '{other}' in '{spec}'"),
    };

    let mut modifiers = KeyModifiers::NONE;
    if ctrl {
        modifiers |= KeyModifiers::CONTROL;
    }
    if alt {
        modifiers |= KeyModifiers::ALT;
    }
    if shift {
        modifiers |= KeyModifiers::SHIFT;
    }
    Ok(KeyEvent::new(code, modifiers))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse_to_expected_keys() {
        let keymap = KeymapConfig::default().parse().unwrap();
        assert_eq!(
            keymap.send,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
        );
        assert_eq!(
            keymap.newline,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)
        );
        assert_eq!(
            keymap.quit,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
        );
        assert_eq!(
            keymap.dismiss,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
        );
        assert_eq!(
            keymap.scroll_to_bottom,
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::SHIFT)
        );
        assert_eq!(
            keymap.retry_connection,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)
        );
        assert_eq!(
            keymap.play_pause,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)
        );
        assert_eq!(
            keymap.seek_back,
            KeyEvent::new(KeyCode::Char('<'), KeyModifiers::NONE)
        );
        assert_eq!(
            keymap.seek_forward,
            KeyEvent::new(KeyCode::Char('>'), KeyModifiers::NONE)
        );
    }

    #[test]
    fn uppercase_char_implies_shift() {
        let key = parse_key("J").unwrap();
        assert_eq!(key.code, KeyCode::Char('j'));
        assert!(key.modifiers.contains(KeyModifiers::SHIFT));
    }

    #[test]
    fn unknown_key_is_an_error() {
        assert!(parse_key("banana").is_err());
        assert!(parse_key("ctrl+banana").is_err());
    }

    #[test]
    fn config_round_trips_through_toml() {
        let mut config = Config::default();
        config.providers.telegram.enabled = true;
        config.max_write_lines = 3;
        let raw = toml::to_string_pretty(&config).unwrap();
        let back: Config = toml::from_str(&raw).unwrap();
        assert!(back.providers.telegram.enabled);
        assert!(!back.providers.whatsapp);
        assert_eq!(back.keys.send, "enter");
        assert_eq!(back.max_write_lines, 3);
    }

    #[test]
    fn missing_keys_fall_back_to_defaults() {
        let raw = "[providers]\nwhatsapp = true\n";
        let config: Config = toml::from_str(raw).unwrap();
        assert_eq!(config.keys.send, "enter");
        assert!(!config.providers.telegram.enabled);
        assert!(config.providers.whatsapp);
        assert_eq!(config.max_write_lines, 5);
    }

    #[test]
    fn partial_keys_fall_back_for_new_actions() {
        // A config written before `retry_connection` existed must still load
        // and pick up the default binding for the new action.
        let raw = "[keys]\nsearch_text = \"s\"\n";
        let config: Config = toml::from_str(raw).unwrap();
        assert_eq!(config.keys.search_text, "s");
        assert_eq!(config.keys.retry_connection, "r");
        assert_eq!(config.keys.play_pause, "space");
        assert_eq!(config.keys.seek_back, "<");
        assert_eq!(config.keys.seek_forward, ">");
    }

    #[test]
    fn test_config_io_uses_a_temp_dir() {
        // Under `#[cfg(test)]` every config path must resolve to a throwaway
        // temp directory, never a real user's config dir.
        let dir = Config::user_config_dir().unwrap();
        assert_eq!(
            dir.parent(),
            Some(std::env::temp_dir().as_path()),
            "test config dir must live in the system temp dir, got: {}",
            dir.display()
        );
        assert_eq!(
            Config::config_file_path().unwrap().parent(),
            Some(dir.as_path())
        );

        // And saves/loads round-trip through that temp dir (no real config
        // file was touched to write either of these).
        let reported = Config::config_file_path().unwrap();
        let cfg = Config {
            max_write_lines: 3,
            ..Config::default()
        };
        cfg.save_config().unwrap();
        let loaded = Config::load().unwrap().unwrap();
        assert_eq!(loaded.max_write_lines, 3);
        assert_eq!(reported, Config::config_file_path().unwrap());
    }
}
