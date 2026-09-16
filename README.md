# Welcome to Sender!

<!--toc:start-->

- [Welcome to Sender!](#welcome-to-sender)
  - [Features](#features)
  - [Configuration](#configuration)
    - [Example `config.toml`:](#example-configtoml)
      - [Top-level options](#top-level-options)
      - [Providers](#providers)
    - [Telegram Setup](#telegram-setup)
    - [WhatsApp Setup](#whatsapp-setup)
  - [Controls / Key Bindings](#controls-key-bindings)
  - [Built with](#built-with)
  - [Inspirations](#inspirations)

<!--toc:end-->

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
- [ ] WhatsApp integration
- [ ] Message actions (reply, edit, delete) for whatsapp
- [ ] Telegram integration
- [ ] Message actions (reply, edit, delete) for telegram
- [ ] Support for notifications
- [x] **Telegram** - Support for endless chat scroll.
- [ ] **WhatsApp** - Support for endless chat scroll.
- [ ] Image rendering
- [ ] Audio playback
- [ ] Ordered chat list - All chats, ordered by pinned/most recent.
- [ ] Dedicated chatlist for each provider
- [ ] Chat List/Message search.
- [ ] Chat rename

## Configuration

The app reads and writes `config.toml` in your OS config directory:
`~/.config/senders/` on Linux (`$XDG_CONFIG_HOME`),
`~/Library/Application Support/senders/` on macOS, `%APPDATA%\senders\` on
Windows.

On first boot the app opens the settings screen (`s` in normal mode) so you can
enable Telegram and/or WhatsApp.

### Example `config.toml`:

```toml
max_write_lines = 5
chat_poll_interval_secs = 10
sync_update_state_secs = 60
sidebar_sync_secs = 10

[keys]
scroll_up = "k"
scroll_down = "j"
select = "enter"
pane_next = "tab"
pane_prev = "shift+tab"
dismiss = "esc"
quit = "ctrl+c"
open_settings = "s"
focus_write = "i"
scroll_to_bottom = "G"
send = "enter"
newline = "shift+enter"

[providers.telegram]
enabled = true
api_id = 12345678
api_hash = "your_api_hash_here"

[providers]
whatsapp = false
```

> [!NOTE]
> Key values are written as `key` or `modifier+key` (`ctrl+c`, `alt+enter`,
> `shift+tab`, `pgup`, `f1`, ...). Omitted keys fall back to the defaults above.

#### Top-level options

| Key                       | Default | Description                                                                                                                                                                                                                                                                                                                                      |
| ------------------------- | ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `max_write_lines`         | `5`     | How many text lines the Write box can grow to. The input wraps long lines and scrolls once it exceeds that.                                                                                                                                                                                                                                      |
| `chat_poll_interval_secs` | `10`    | Fallback interval (in seconds) for reconciling messages in the **currently open** chat (any provider) when push updates are unavailable. Lower values feel more responsive but use more resources.                                                                                                                                               |
| `sync_update_state_secs`  | `120`   | Telegram-only. How often (in seconds) the Telegram client persists its internal update-state (pts/qts/seq) to the session file. This does **not** call any Telegram API — it only saves local state so that `catch_up` on restart is faster. I recommend setting a high value, because it does consume Disk IO                                   |
| `sidebar_sync_secs`       | `10`    | How often (in seconds) the TUI fetches the full chat list from the enabled providers (Telegram and WhatsApp) to detect **unread-count changes across all chats**. This is the mechanism that updates unread indicators on chats you are not currently viewing. Increase this if you notice high CPU usage; decrease it for snappier unread dots. |

#### Providers

Telegram credentials are stored under `[providers.telegram]`. You can toggle
`enabled` from the settings screen, but `api_id` / `api_hash` must be set in the
config file first (see [Telegram Setup](#telegram-setup)).

### Telegram Setup

You need to get API credentials from my.telegram.org:

1. Go to https://my.telegram.org
2. Log in with your Telegram phone number (you'll get a confirmation code)
3. Go to "API development tools"
4. Fill in the form (App title, Short name, etc. — can be anything)
5. You'll receive an App api_id (number) and App api_hash (string)

Add them to your `config.toml`:

```toml
[providers.telegram]
enabled = true # This can also be enabled in-app
api_id = 12345678
api_hash = "your_api_hash_here"
```

On first launch the app will prompt you for your Telegram phone number and a
verification code. After a successful login the session is stored locally so you
won't need to log in again.

### WhatsApp Setup

WhatsApp has no credentials — pairing happens through a QR code:

1. Turn WhatsApp on in the settings screen (inside the app: **Settings > enable
   WhatsApp**), or set `whatsapp = true` in the config file:

   ```toml
   [providers]
   whatsapp = true
   ```

2. A QR code appears in the login screen. On your phone open **WhatsApp >
   Settings > Linked devices > Link a device** and scan it.

3. Once linked, the session is stored under `wa.db` in the same directory as the
   config file, so the next launch is already paired (no QR needed).

> [!NOTE]
> Closing Senders gracefully disconnects but **never logs your device out of
> WhatsApp** — your linked device stays active. To remove it, unlink it from the
> phone (WhatsApp > Linked devices) instead.

## Controls / Key Bindings

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
