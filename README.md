# Welcome to Sender!

Sender is an TUI app for interacting with Whatsapp AND telegram (maybe more in
the future?) in one TUI app!

> [!CAUTION]
> This is an unofficial client! Your account might be banned by the service
> provider! Use at your own risk

## Features

- [x] _**One-Executable-app**_ - no co dependencies, one binary to rule them
      all.
- [x] Configurable
- [ ] Draft / Myself conversation
- [ ] Help menu Popup to show keymaps
- [ ] Descriptive error handling
- [ ] Message actions (reply, edit, delete)
- [ ] WhatsApp integration
- [ ] Telegram integration
- [ ] Support for notifications
- [ ] Image rendering

## Configuration

The app reads and writes `config.toml` in your OS config directory:
`~/.config/senders/` on Linux (`$XDG_CONFIG_HOME`),
`~/Library/Application Support/senders/` on macOS, `%APPDATA%\senders\` on
Windows.

On first boot the app opens the settings screen (`s` in normal mode) so you can
enable Telegram and/or WhatsApp. Connections are mocked until the real backends
land.

### Example `config.toml`:

```toml
max_write_lines = 5

[keys]
up = "k"
down = "j"
select = "enter"
pane_next = "tab"
dismiss = "esc"
quit = "ctrl+c"
open_settings = "s"
focus_write = "i"
send = "enter"
newline = "shift+enter"

[providers]
telegram = true
whatsapp = false
```

Key values are written as `key` or `modifier+key` (`ctrl+c`, `alt+enter`,
`shift+tab`, `pgup`, `f1`, ...). Omitted keys fall back to the defaults above.
`max_write_lines` controls how many text lines the Write box can grow to
(default `5`); the input wraps long lines and scrolls once it exceeds that.

### Telegram Setup

You need to get API credentials from my.telegram.org:

1. Go to https://my.telegram.org
2. Log in with your Telegram phone number (you'll get a confirmation code)
3. Go to "API development tools"
4. Fill in the form (App title, Short name, etc. — can be anything)
5. You'll receive an App api_id (number) and App api_hash (string)

## Controls

| Keys        | Pane / Mode   | Action                                                                                                                                                                  |
| ----------- | ------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `j/k`       | -             | go up (k) or down (j)                                                                                                                                                   |
| `Enter`     | Chat list     | select chat                                                                                                                                                             |
| `Tab`       | -             | Change pane focus (Chat list → Chat → Write)                                                                                                                            |
| `Esc`       | -             | Remove focus from 'write message' or dismiss error/info dialog                                                                                                          |
| `Ctrl+c`    | -             | Quit app                                                                                                                                                                |
| `s`         | Normal mode   | open settings                                                                                                                                                           |
| `i`         | Chat          | Focus on 'write message'                                                                                                                                                |
| `PgUp/PgDn` | Chat          | Page up / down on chat history (fixed, not configurable)                                                                                                                |
| `Enter`     | Write message | Send message (configurable)                                                                                                                                             |
| `Alt+Enter` | Write message | Insert newline. `Shift+Enter` also works but only on terminals that report modifier keys on Enter (kitty, foot, WezTerm, Alacritty; not GNOME Terminal or a plain TTY). |
| `Enter`     | Settings      | Select items (use j/k for scrolling)                                                                                                                                    |

> All keys except `PgUp/PgDn` are configurable in the config file.

## Built with

1. [grammers - Telegram/Rust integration](https://codeberg.org/Lonami/grammers)
1. [whatsapp-rust](https://github.com/oxidezap/whatsapp-rust)
1. [ratatui](https://ratatui.rs/)

## Inspirations

- [tgt](https://github.com/federicobruzzone/tgt)
- [WhatsGo](https://github.com/WinterSunset95/WhatsGo)
- [waha-tui](https://github.com/muhammedaksam/waha-tui)
