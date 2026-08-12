# Welcome to Sender!

Sender is an TUI app for interacting with Whatsapp AND telegram (maybe more in the future?) in one TUI app!

> [!CAUTION]
> This is an unofficial client! Your account might be banned by the service provider! Use at your own risk

## Features

- [x] ***One-Executable-app*** - no co dependencies, one binary to rule them all. 
- [ ] ***All-in-one app for messages***  - at first, Whatsapp + Telegram, but maybe more apps could be implemented?
- [x] Configurable
- [ ] Support for notifications 
- [ ] Multi platform
- [ ] Image rendering

## Controls

| Keys | Pane / Mode | Action |
| --------------- | --------------- | --------------- |
| `j/k` | Chat list | go up (k) or down (j) to select chat |
| `Enter` | Chat list | select chat |
| `Tab` | -  | Change pane focus (Chat list → Chat → Write) |
| `Esc` | - | Remove focus from 'write message' or dismiss error/info dialog |
| `Ctrl+c` | - | Quit app |
| `s` | Normal mode | open settings |
| `i` | Chat | Focus on 'write message' |
| `j/k` | Chat | Scroll up / down on chat history | 
| `PgUp/PgDn` | Chat | Page up / down on chat history (fixed, not configurable) |
| `Enter` | Write message | Send message (configurable) |
| `Shift+Enter` | Write message | Insert newline (configurable, requires a terminal that reports modified keys) |
| `j/k` / `Enter` / `Esc` | Settings | Move / toggle provider / close settings (reuses chat-list keys) |

> All keys except `PgUp/PgDn` are configurable in the config file.

## Configuration

The app reads and writes `config.toml` in your OS config directory:
`~/.config/senders/` on Linux (`$XDG_CONFIG_HOME`), `~/Library/Application Support/senders/` on macOS, `%APPDATA%\senders\` on Windows.

On first boot the app opens the settings screen (`s` in normal mode) so you can enable Telegram and/or WhatsApp. Connections are mocked until the real backends land.

Example `config.toml`:

```toml
[keys]
chat_list_up = "k"
chat_list_down = "j"
select = "enter"
pane_next = "tab"
dismiss = "esc"
quit = "ctrl+c"
open_settings = "s"
focus_write = "i"
history_up = "k"
history_down = "j"
send = "enter"
newline = "shift+enter"

[providers]
telegram = true
whatsapp = false
```

Key values are written as `key` or `modifier+key` (`ctrl+c`, `alt+enter`, `shift+tab`, `pgup`, `f1`, ...). Omitted keys fall back to the defaults above.


## Built with 

1. [grammers - Telegram/Rust integration](https://codeberg.org/Lonami/grammers) 
1. [whatsapp-rust](https://github.com/oxidezap/whatsapp-rust) 
1. [ratatui](https://ratatui.rs/) 

## Inspirations

- [tgt](https://github.com/federicobruzzone/tgt) 
- [WhatsGo](https://github.com/WinterSunset95/WhatsGo) 
- [waha-tui](https://github.com/muhammedaksam/waha-tui) 
