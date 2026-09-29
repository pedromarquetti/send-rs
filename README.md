# Welcome to Sender!

<!--toc:start-->

- [Welcome to Sender!](#welcome-to-sender)
  - [System requirements](#system-requirements)
    - [Linux](#linux)
    - [macOS](#macos)
    - [Windows](#windows)
    - [Notifications](#notifications)
  - [Features](#features)
  - [Configuration](#configuration)
    - [Example `config.toml`:](#example-configtoml)
      - [Top-level options](#top-level-options)
      - [Notifications](#notifications-1)
      - [Providers](#providers)
    - [Telegram Setup](#telegram-setup)
    - [WhatsApp Setup](#whatsapp-setup)
  - [Controls / Key Bindings](#controls-key-bindings)
  - [Limitations](#limitations)
    - [Telegram (grammers / Telegram protocol)](#telegram-grammers-telegram-protocol)
    - [WhatsApp (whatsapp-rust / WhatsApp Web protocol)](#whatsapp-whatsapp-rust-whatsapp-web-protocol)
  - [Built with](#built-with)
  - [Inspirations](#inspirations)

<!--toc:end-->

Sender is an TUI app for interacting with Whatsapp AND telegram (maybe more in
the future?) in one TUI app!

> [!CAUTION]
> This is an unofficial client! Your account might be banned by the service
> provider! Use at your own risk

## System requirements

Building needs only the Rust toolchain plus the system audio libraries used for
**recording** (Playback needs no extra libs — opus-pure decodes Opus in pure
Rust):

### Requirements for **audio recording**

#### Linux

1. ALSA dev libraries

- Package names: `alsa-lib` (Arch/Manjaro), `libasound2-dev` (Debian/Ubuntu),
  `alsa-lib-devel` (Fedora/RHEL).

2. `pkg-config` (cpal uses ALSA for input).

#### macOS

- Xcode Command Line Tools — CoreAudio is linked automatically.

#### Windows

None — WASAPI is linked automatically.

At runtime, a working microphone/input device is required to use voice-note
recording.

### Requirements for OS Notifications

The **OS notification** goes through each platform's own mechanism, and on every
platform something outside `senders` has to be there to present it:

#### Linux

A D-Bus **session bus** plus a notification daemon that owns
`org.freedesktop.Notifications` (GNOME, KDE Plasma...). Sendrs uses
[notify-rust](https://docs.rs/crate/notify-rust/latest) as a notification sender
crate, check their docs for details. With no session bus, it fails immediately,
and `senders` logs it in `sender.log` and carries on. Sound cues are unaffected.

#### macOS — the presenting application is identified by the **bundle

identifier of the running process**, so `senders` has to be shipped inside an
`.app` bundle to be identified correctly. `appname("sender")` is ignored on
macOS, and an unbundled binary has no bundle identifier, so the notification
backend falls back to `com.apple.Finder` and a banner is attributed to the wrong
application. Sound cues are unaffected.

#### Windows

Toasts are attributed to an **AppUserModelID**. `senders` sets none, so the
notification backend falls back to PowerShell's ID: the toast still appears, but
grouped under and attributed to PowerShell. A real AUMID means registering a
Start-menu shortcut, which is an install-time step rather than something the app
can do for itself.

## Features

- [x] _**One-Executable-app**_ - no co dependencies, one binary to rule them
      all.
- [x] Configurable
- [x] Draft / Myself conversation - using wp/tg integrated self chat
- [ ] Help menu Popup to show keymaps
- [x] Descriptive error handling
- [x] WhatsApp integration
- [x] Message actions (reply, edit, delete) for whatsapp
- [ ] Copy messages' text to clipboard
- [x] Telegram integration
- [x] Message actions (reply, edit, delete) for telegram
- [x] **Telegram** - Support for endless chat scroll.
- [x] **WhatsApp** - Support for endless chat scroll.
- [x] Image rendering
- [x] Audio playback
- [ ] Send images - TODO: check if possible: send images + "paste to send"
- [x] Send audio
- [x] Support for notifications
- [x] Ordered chat list - All chats, ordered by pinned/most recent.
- [ ] Dedicated chatlist for each provider
- [x] Chat List/Message search.
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
chat_list_sync_secs = 10

[keys]
search_text = "/"
scroll_up = "k"
scroll_down = "j"
select = "enter"
pane_prev = "shift+tab"
pane_next = "tab"
dismiss = "esc"
quit = "ctrl+c"
open_settings = "s"
focus_write = "i"
scroll_to_top = "g"
scroll_to_bottom = "G"
send = "enter"
newline = "shift+enter"
retry_connection = "r"
record_voice = "a"
play_pause = "space"
seek_back = "<"
seek_forward = ">"

[providers.telegram]
enabled = true
api_id = 12345678
api_hash = "your_api_hash_here"

[providers]
whatsapp = false

[notifications]
os = true
sound = true
debounce_ms = 1500

[notifications.sounds]
telegram = "/home/you/sounds/tg.wav"
# A blank value mutes that messenger only, and deleting the key does the same.
whatsapp = ""
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
| `chat_list_sync_secs`     | `10`    | How often (in seconds) the TUI fetches the full chat list from the enabled providers (Telegram and WhatsApp) to detect **unread-count changes across all chats**. This is the mechanism that updates unread indicators on chats you are not currently viewing. Increase this if you notice high CPU usage; decrease it for snappier unread dots. |

#### Notifications

New messages in chats you are **not** currently viewing can raise a sound cue
and/or your OS's own notification. Both are off by default, and every key is
optional — a config file without a `[notifications]` table stays silent.

| Key                         | Default | Description                                                                                                                                                                                                                                  |
| --------------------------- | ------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `notifications.os`          | `false` | Global on/off switch for the **OS** notification (D-Bus toast / banner). Off means no desktop notification is ever requested, on any platform.                                                                                               |
| `notifications.sound`       | `false` | Global on/off switch for the **sound cue**. The two switches are independent, so you can run sound-only (no session bus needed) or notifications-only.                                                                                       |
| `notifications.debounce_ms` | `1500`  | How long one messenger coalesces a burst of new messages into a single notification. A group that drops 20 messages at once produces one sound _and_ one desktop notification. Use `0` to notify on **every** message.                       |
| `notifications.sounds`      | `{}`    | Per-messenger sound file, keyed by messenger name (`telegram`, `whatsapp`). **A messenger with no entry plays no sound** — this is how you silence one messenger only. A blank value (`""`) counts as no sound, same as leaving the key out. |

What happens per message: a provider that redelivers a message you already got
announces it once, and a message in the chat you are currently reading is never
announced. When a cue really was played, the desktop notification is shown
silently, so one message makes one noise. A sound file that is missing (or
unreadable) mutes that messenger's cue only; its desktop notifications still
arrive.

A cue never interrupts audio: while a voice note is loaded — **including while
it is only paused, not playing** — or playing, a cue is **dropped, not queued**,
and the voice note keeps its position and stays replayable. A cue dropped this
way is not played afterwards, so a busy audio session costs you notifications
rather than playback. There is no per-chat mute and no per-chat sound yet;
`notifications.sounds` is keyed per messenger.

Sound files go through the same decoder as the audio popup, so any format that
plays in the popup plays here too (`wav`, `mp3`, `flac`, `m4a`, `ogg`;
Opus-in-Ogg is decoded natively). Paths may be absolute or relative to the
directory you run `senders` from. A path that cannot be read is reported in
`sender.log` at startup and that messenger is then silent — it is not retried
per message.

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

> [!NOTE]
> WhatsApp scroll-to-top (PageUp / `k` at the first message) requests older
> messages from your primary phone via an on-demand history sync. The phone must
> be online and reachable; if it ignores the request, Senders times out after 10
> seconds and keeps whatever is currently cached (scroll up again later to
> retry).

## Controls / Key Bindings

| Keys        | Pane / Mode      | Action                                                                                                                                                                  |
| ----------- | ---------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `j/k`       | -                | go up (k) or down (j)                                                                                                                                                   |
| `Enter`     | Chat list        | select chat                                                                                                                                                             |
| `Tab`       | -                | Change pane focus (Chat list → Chat → Write)                                                                                                                            |
| `Esc`       | -                | Remove focus from 'write message' or dismiss error/info dialog                                                                                                          |
| `Ctrl+c`    | -                | Quit app                                                                                                                                                                |
| `s`         | Normal mode      | open settings                                                                                                                                                           |
| `i`         | Chat             | Focus on 'write message'                                                                                                                                                |
| `PgUp/PgDn` | Chat             | Page up / down on chat history (fixed, not configurable)                                                                                                                |
| `Enter`     | Write message    | Send message (configurable)                                                                                                                                             |
| `Alt+Enter` | Write message    | Insert newline. `Shift+Enter` also works but only on terminals that report modifier keys on Enter (kitty, foot, WezTerm, Alacritty; not GNOME Terminal or a plain TTY). |
| `Enter`     | Settings         | Select items (use j/k for scrolling)                                                                                                                                    |
| `r`         | Normal mode      | Force a reconnect when a provider is stuck in the "connection lost" state (e.g. after consecutive Telegram-stream failures)                                             |
| `/`         | Chat / Chat List | Search for Messages in Chat or Chats in Chat List                                                                                                                       |
| `g`         | Chat / Chat List | Go to the top of the list                                                                                                                                               |
| `a`         | Chat / Pop Up    | Press and hold to start audio recording -> release to send. Chat: send audio to chat. Select message: reply to message with audio                                       |
| `Space`     | Pop Up           | Play/pause media playback in Popup                                                                                                                                      |
| `<`         | Pop Up           | Seek back                                                                                                                                                               |
| `>`         | Pop Up           | Seek forward                                                                                                                                                            |

> All keys except `PgUp/PgDn` are configurable in the config file.

## Limitations

Only limitations imposed by the underlying libraries and their protocols —
things senders cannot implement regardless of effort. Features that the
libraries already support but senders has not wired up yet are not listed here.

### Telegram (grammers / Telegram protocol)

- **Chat list has no push updates.** The protocol delivers no update for a chat
  being added, removed, pinned or reordered; new chats appear only implicitly
  when they send a message. grammers' friendly update stream keeps these as raw,
  undocumented TL updates, so keeping the list in sync means re-listing dialogs
  (`iter_dialogs`), which senders does on a timer.
- **No account creation.** grammers' sign-in reports `SignUpRequired`: Telegram
  accounts can only be registered from official Telegram apps, never from a
  third-party client.
- **Rate limits and slow mode are server-enforced.** Flood-wait and slow-mode
  durations are fixed by Telegram; grammers only auto-sleeps short bounded
  flood-waits, so longer waits surface as errors the client can only wait out.

### WhatsApp (whatsapp-rust / WhatsApp Web protocol)

- **No server-side history for chats and groups.** Past messages arrive only as
  history-sync blobs uploaded by the linked phone; requesting older messages
  asks the phone and returns nothing while it is offline. Only newsletters are
  served history from the server.
- **No server "list chats" API.** There is no endpoint that returns the chat
  list; it must be reconstructed locally from history-sync blobs and push
  events.
- **No account creation.** Pairing can only link an already-existing WhatsApp
  account (QR code, pair code or passkey).
- **Queued offline sending is not possible.** Outbound messages require a live
  WebSocket connection.

## Built with

1. [grammers - Telegram/Rust integration](https://codeberg.org/Lonami/grammers)
1. [whatsapp-rust](https://github.com/oxidezap/whatsapp-rust)
1. [ratatui](https://ratatui.rs/)

## Inspirations

- [tgt](https://github.com/federicobruzzone/tgt)
- [WhatsGo](https://github.com/WinterSunset95/WhatsGo)
- [waha-tui](https://github.com/muhammedaksam/waha-tui)
