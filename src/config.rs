use anyhow::{Context, Result, bail};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub keys: KeymapConfig,
    pub providers: ProvidersConfig,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ProvidersConfig {
    pub telegram: bool,
    pub whatsapp: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct KeymapConfig {
    pub chat_list_up: String,
    pub chat_list_down: String,
    pub select: String,
    pub pane_prev: String,
    pub pane_next: String,
    pub dismiss: String,
    pub quit: String,
    pub open_settings: String,
    pub focus_write: String,
    pub history_up: String,
    pub history_down: String,
    pub send: String,
    pub newline: String,
}

impl Default for KeymapConfig {
    fn default() -> Self {
        Self {
            chat_list_up: "k".into(),
            chat_list_down: "j".into(),
            select: "enter".into(),
            pane_prev: "shift+tab".into(),
            pane_next: "tab".into(),
            dismiss: "esc".into(),
            quit: "ctrl+c".into(),
            open_settings: "s".into(),
            focus_write: "i".into(),
            history_up: "k".into(),
            history_down: "j".into(),
            send: "enter".into(),
            newline: "shift+enter".into(),
        }
    }
}

impl KeymapConfig {
    pub fn parse(&self) -> Result<Keymap> {
        Ok(Keymap {
            chat_list_up: parse_key(&self.chat_list_up)?,
            chat_list_down: parse_key(&self.chat_list_down)?,
            select: parse_key(&self.select)?,
            pane_prev: parse_key(&self.pane_prev)?,
            pane_next: parse_key(&self.pane_next)?,
            dismiss: parse_key(&self.dismiss)?,
            quit: parse_key(&self.quit)?,
            open_settings: parse_key(&self.open_settings)?,
            focus_write: parse_key(&self.focus_write)?,
            history_up: parse_key(&self.history_up)?,
            history_down: parse_key(&self.history_down)?,
            send: parse_key(&self.send)?,
            newline: parse_key(&self.newline)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    pub chat_list_up: KeyEvent,
    pub chat_list_down: KeyEvent,
    pub select: KeyEvent,
    pub pane_next: KeyEvent,
    pub pane_prev: KeyEvent,
    pub dismiss: KeyEvent,
    pub quit: KeyEvent,
    pub open_settings: KeyEvent,
    pub focus_write: KeyEvent,
    pub history_up: KeyEvent,
    pub history_down: KeyEvent,
    pub send: KeyEvent,
    // BUG: this is not working
    pub newline: KeyEvent,
}

impl Config {
    pub fn config_dir() -> Result<PathBuf> {
        dirs::config_dir()
            .or_else(dirs::home_dir)
            .map(|base| base.join("senders"))
            .ok_or_else(|| anyhow::anyhow!("could not determine a config directory"))
    }

    pub fn config_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("config.toml"))
    }

    pub fn exists() -> bool {
        Self::config_path()
            .map(|path| path.exists())
            .unwrap_or(false)
    }

    pub fn load() -> Result<Option<Config>> {
        let path = Self::config_path()?;
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path)?;
        let config = toml::from_str(&raw)
            .with_context(|| format!("invalid config at {}", path.display()))?;
        Ok(Some(config))
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = toml::to_string_pretty(self)?;
        std::fs::write(&path, raw)?;
        Ok(())
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
        config.providers.telegram = true;
        let raw = toml::to_string_pretty(&config).unwrap();
        let back: Config = toml::from_str(&raw).unwrap();
        assert!(back.providers.telegram);
        assert!(!back.providers.whatsapp);
        assert_eq!(back.keys.send, "enter");
    }

    #[test]
    fn missing_keys_fall_back_to_defaults() {
        let raw = "[providers]\nwhatsapp = true\n";
        let config: Config = toml::from_str(raw).unwrap();
        assert_eq!(config.keys.send, "enter");
        assert!(!config.providers.telegram);
        assert!(config.providers.whatsapp);
    }
}
