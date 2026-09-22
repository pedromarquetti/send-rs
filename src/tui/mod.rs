use anyhow::Result;
use ratatui::crossterm::ExecutableCommand;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
};
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::{StatefulWidget, Widget};
use ratatui::{DefaultTerminal, Frame};
use ratatui_image::picker::{Picker, ProtocolType};
use tokio::sync::mpsc;
use tokio::time::{Duration, MissedTickBehavior};
use tracing::{debug, error, info, warn};

use crate::backend::{
    self, AuthSteps, BackendError, BackendEvent, Chat, ChatId, MediaKind, Message, MessageAction,
    MessageId, MessengerKind, Provider,
};
use crate::config::{Config, Keymap};
use crate::helpers::available_message_actions;
use crate::tui::chat::chat_list::ChatList;
use crate::tui::chat::chat_widget::ChatWidget;
use crate::tui::image::ImageWidgetState;
use crate::tui::loading::{LoadingSpinner, LoadingWidget};
use crate::tui::player::{PlaybackState, Player, RodioEngine};
use crate::tui::popup::{ImagePopup, PopupKind};
use crate::tui::settings::Settings;
use crate::tui::state::{AppState, Focus, Mode, Screen};
use crate::tui::status_bar::StatusBarWidget;

mod chat;
mod image;
mod loading;
mod login;
mod player;
mod popup;
mod search;
mod settings;
mod state;
mod status_bar;

enum UiEvent {
    Key(KeyEvent),
    Paste(String),
    Shutdown,
    Backend(Provider, BackendEvent),
    Resize(u16, u16),
    /// Wake signal: a background image (re)encode finished; redraw to apply it.
    Redraw,
    HistoryRefresh(ChatId, Vec<backend::Message>),
    HistoryRefreshResult(ChatId, Result<Vec<backend::Message>, BackendError>),
    /// Result of a backgrounded lazy-history load (scroll-up with the chat at its
    /// topmost message); applied by `AppState::apply_history_page`.
    HistoryPageResult(ChatId, Result<Vec<backend::Message>, BackendError>),
    ChatLoaded {
        chat: Chat,
        generation: u64,
        result: Result<Vec<backend::Message>, BackendError>,
        status: Option<String>,
    },
    ChatList(Provider, Result<Vec<Chat>, BackendError>),
    ChatsLoaded {
        chats: Vec<Chat>,
        errors: Vec<BackendError>,
    },
    /// Result of a backgrounded `Messenger::media_bytes` fetch for the image
    /// popup; applied by `App::apply_image_media` (a no-op if the popup for
    /// `message_id` is no longer open).
    ImageMedia {
        chat: ChatId,
        message_id: MessageId,
        result: Result<Option<Vec<u8>>, BackendError>,
    },
    /// Snapshot of the audio session from the player worker; applied by
    /// `AppState::apply_playback_state` (a no-op if no session is active for
    /// that message).
    PlaybackState(PlaybackState),
}

pub async fn run(
    config: Config,
    keymap: Keymap,
    messengers: Vec<MessengerKind>,
    open_settings: bool,
) -> Result<()> {
    let mut terminal = ratatui::init();
    // Let the terminal send pasted text as a single bracketed-paste event instead of a stream of
    // raw keys, so multi-line paste cannot trigger Enter=send mid-paste.
    std::io::stdout().execute(EnableBracketedPaste)?;
    let result = run_app(&mut terminal, config, keymap, messengers, open_settings).await;
    let _ = std::io::stdout().execute(DisableBracketedPaste);
    ratatui::restore();
    result
}

fn spawn_signal_listener(tx: mpsc::UnboundedSender<UiEvent>) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};

            let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT listener");
            let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM listener");

            tokio::select! {
                _ = sigint.recv() => {
                    let _ = tx.send(UiEvent::Shutdown);
                }
                _ = sigterm.recv() => {
                    let _ = tx.send(UiEvent::Shutdown);
                }
            }
        }

        #[cfg(windows)]
        {
            use tokio::signal::windows::{
                ctrl_break, ctrl_c, ctrl_close, ctrl_logoff, ctrl_shutdown,
            };

            let mut sigint = ctrl_c().expect("Ctrl-C listener");
            let mut sigbreak = ctrl_break().expect("Ctrl-Break listener");
            let mut sigclose = ctrl_close().expect("Ctrl-Close listener");
            let mut siglogoff = ctrl_logoff().expect("Ctrl-Logoff listener");
            let mut sigshutdown = ctrl_shutdown().expect("Ctrl-Shutdown listener");

            tokio::select! {
                _ = sigint.recv() => {
                    let _ = tx.send(UiEvent::Shutdown);
                }
                _ = sigbreak.recv() => {
                    let _ = tx.send(UiEvent::Shutdown);
                }
                _ = sigclose.recv() => {
                    let _ = tx.send(UiEvent::Shutdown);
                }
                _ = siglogoff.recv() => {
                    let _ = tx.send(UiEvent::Shutdown);
                }
                _ = sigshutdown.recv() => {
                    let _ = tx.send(UiEvent::Shutdown);
                }
            }
        }
    });
}

async fn run_app(
    terminal: &mut DefaultTerminal,
    config: Config,
    keymap: Keymap,
    messengers: Vec<MessengerKind>,
    open_settings: bool,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();

    // Query the terminal's image protocol once, before the input reader starts
    // competing for stdin. Any failure degrades to halfblocks instead of
    // aborting the app.
    let mut picker = match Picker::from_query_stdio() {
        Ok(picker) => picker,
        Err(err) => {
            warn!(%err, "failed to query terminal image protocol; using halfblocks");
            Picker::halfblocks()
        }
    };

    // ratatui-image can misdetect iTerm2 (ratatui/ratatui-image#158); pin the
    // native protocol when running inside it.
    let is_iterm2 = std::env::var("TERM_PROGRAM").is_ok_and(|v| v == "iTerm.app")
        || std::env::var("LC_TERMINAL").is_ok_and(|v| v == "iTerm2");

    if is_iterm2 && picker.protocol_type() != ProtocolType::Iterm2 {
        picker.set_protocol_type(ProtocolType::Iterm2);
    }

    info!(protocol = ?picker.protocol_type(), "terminal image protocol selected");

    spawn_terminal_reader(tx.clone());
    spawn_signal_listener(tx.clone());

    // handling new events for each messenger type
    for messenger in messengers.iter() {
        // removing this because wp login was broken
        // if !messenger.is_enabled(&config.providers) {
        //     continue;
        // }

        let mut backend_rx = messenger.subscribe();
        let forward_tx = tx.clone();
        let provider = messenger.provider();

        tokio::spawn(async move {
            loop {
                match backend_rx.recv().await {
                    Ok(event) => {
                        if forward_tx.send(UiEvent::Backend(provider, event)).is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!(
                            provider = ?provider,
                            skipped,
                            "Backend event lagged; dropping missed events"
                        );
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    let mut app = App::new(
        config,
        keymap,
        messengers,
        open_settings,
        picker,
        tx.clone(),
    )
    .await;

    info!("TUI started");

    let telegram_needs_login = if app.state.config.providers.telegram.enabled {
        match app.state.provider_to_messenger(Provider::Telegram) {
            Some(messenger) => !messenger.is_authenticated().await,
            None => false,
        }
    } else {
        false
    };

    if telegram_needs_login {
        app.state.start_login(Provider::Telegram)?;
    }

    // Kick off the initial chat list fetch in the background so the UI renders
    // immediately (non-blocking startup). Results arrive via `ChatsLoaded`.
    let chat_loader_tx = tx.clone();
    let loader_messengers = app.state.messengers.clone();
    let provider_configs = app.state.config.providers.clone();

    {
        let provider_configs = provider_configs.clone();
        tokio::spawn(async move {
            let (chats, errors) =
                crate::tui::state::fetch_all_chats(&loader_messengers, &provider_configs).await;
            let _ = chat_loader_tx.send(UiEvent::ChatsLoaded { chats, errors });
        });
    };

    let mut chat_poll_interval = tokio::time::interval(Duration::from_secs(
        app.state.config.chat_poll_interval_secs,
    ));

    chat_poll_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let chat_list_period = Duration::from_secs(app.state.config.chat_list_sync_secs);
    let mut chatlist_sync_interval = tokio::time::interval_at(
        tokio::time::Instant::now() + chat_list_period,
        chat_list_period,
    );

    chatlist_sync_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut status_clock = tokio::time::interval(Duration::from_secs(1));
    status_clock.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        terminal.draw(|frame| app.draw(frame))?;

        tokio::select! {
            biased;

            // Wakes the loop every second so the live flood-wait countdown and
            // reconnect hint re-render without needing an input or a backend
            // event to drive the redraw.
            _ = status_clock.tick() => {}

            _ = chat_poll_interval.tick() => {
                let chat_id = match app.state.chat_state.open_chat.as_ref() {
                    Some(open) => open.chat.id.clone(),
                    None => continue,
                };

            let messenger_idx = app.state.messengers.iter().position(|m| {
                m.provider() == chat_id.to_provider()
                    });

                if let Some(idx) = messenger_idx {
                    if app.state.history_refresh_in_flight {
                        continue;
                    }

                    app.state.history_refresh_in_flight = true;
                    let messenger = app.state.messengers[idx].clone();
                    let tx = tx.clone();
                    let chat_for_task = chat_id.clone();

                    tokio::spawn(async move {
                        let result = messenger.history(&chat_for_task).await;
                        let _ = tx.send(UiEvent::HistoryRefreshResult(chat_for_task, result));
                    });
                }
            }

            // load chatlist task
            _ = chatlist_sync_interval.tick() => {
                if !app.state.chats_loaded {
                    continue;
                }

                if app.state.chatlist_sync_in_flight {
                    continue;
                }

                app.state.chatlist_sync_in_flight = true;
                app.state.chatlist_sync_pending = app.state.messengers.len();

                for messenger in app.state.messengers.iter() {
                    let provider = messenger.provider();

                    if !provider.is_enabled(&provider_configs) {
                        warn!("Provider {:#?} disabled! Skipping chat fetch background task", provider);
                        continue;
                    }

                    let messenger = messenger.clone();
                    let tx = tx.clone();

                    tokio::spawn(async move {
                        let provider = messenger.provider();
                        let result = messenger.chats().await;
                        let _ = tx.send(UiEvent::ChatList(provider, result));
                    });
                }
            }

            event = rx.recv() => {
                let Some(event) = event else {
                    break;
                };
                match event {
                    UiEvent::Key(key) => app.handle_key(key).await,
                    UiEvent::Paste(text) => app.state.handle_paste(text),
                    UiEvent::Shutdown => {
                        app.state.running = false;
                        break;
                    }
                    UiEvent::Backend(provider, backend_event) => {
                        debug!(
                            provider = ?provider,
                            event = ?std::mem::discriminant(&backend_event),
                            "TUI processing backend event, pending render"
                        );

                        let rebuild = matches!(backend_event, BackendEvent::Connected);

                        app.state.handle_backend_event(provider, backend_event);

                        // On a newly established connection (e.g. WhatsApp just
                        // paired via QR), refresh the shared chat list so the
                        // new provider's dialogs appear without a restart.
                        if rebuild {
                            let chat_loader_tx = tx.clone();
                            let loader_messengers = app.state.messengers.clone();
                            let loader_providers = app.state.config.providers.clone();

                            tokio::spawn(async move {
                                let (chats, errors) = crate::tui::state::fetch_all_chats(
                                    &loader_messengers,
                                    &loader_providers,
                                )
                                .await;
                                let _ = chat_loader_tx
                                    .send(UiEvent::ChatsLoaded { chats, errors });
                            });
                        }
                    }
                    UiEvent::Resize(..) => {}
                    UiEvent::Redraw => {}
                    UiEvent::HistoryRefresh(chat_id, history) => {
                        debug!(
                            chat = ?chat_id,
                            messages = history.len(),
                            "HistoryRefresh applied"
                        );
                        app.state.update_chat_list_from_poll(&chat_id, &history);
                        app.state.chat_state.refresh_chat_history(&chat_id, history);
                    }
                    UiEvent::HistoryRefreshResult(chat_id, result) => {
                        app.state.history_refresh_in_flight = false;
                        if app.state.chat_state.open_chat.as_ref().map(|open| &open.chat.id)
                            != Some(&chat_id)
                        {
                            continue;
                        }
                        if let Ok(history) = result {
                            debug!(chat = ?chat_id, msgs = history.len(), "Poll refresh OK");
                            app.state.update_chat_list_from_poll(&chat_id, &history);
                            app.state.chat_state.refresh_chat_history(&chat_id, history);
                        } else if let Err(e) = result {
                            error!(chat = ?chat_id, error = %e, "Poll refresh failed");
                        }
                    }
                    UiEvent::HistoryPageResult(chat_id, result) => {
                        if app.state.chat_state.open_chat.as_ref().map(|open| &open.chat.id)
                            != Some(&chat_id)
                        {
                            continue;
                        }
                        app.state.apply_history_page(&chat_id, result);
                    }
                        UiEvent::ChatLoaded {
                            chat,
                            generation,
                            result,
                            status,
                        } => app.state.apply_chat_load(chat, generation, result, status),
                    UiEvent::ChatList(provider, result) => {
                                // TODO: hide archived chats from the main list once the TUI has an
                                    // Archived section; until then they stay visible to match the phone.

                        app.state.chatlist_sync_pending =
                            app.state.chatlist_sync_pending.saturating_sub(1);
                        app.state.chatlist_sync_in_flight = app.state.chatlist_sync_pending > 0;

                        match result {
                            Ok(chats) => {
                                // Skip the persistence write when nothing
                                // changed (both messenger syncs every
                                // `chat_list_sync_secs`; identical snapshots
                                // must not churn the cache file).
                                if app
                                    .state
                                    .chat_state
                                    .reconcile_provider_chats(provider, chats)
                                {
                                    app.state.persist_chats();
                                }
                                debug!(provider = ?provider, "Chat list sync OK");
                            }

                            Err(e) => error!(provider = ?provider, error = %e, "Chat list sync failed"),
                        }
                    }
                    UiEvent::ChatsLoaded { chats, errors } => {
                        debug!(
                            chats = chats.len(),
                            errors = errors.len(),
                            "ChatsLoaded applied"
                        );
                        app.state.apply_fetched(chats, errors);
                    }
                    UiEvent::ImageMedia {
                        chat,
                        message_id,
                        result,
                    } => app.apply_image_media(chat, message_id, result).await,
                    UiEvent::PlaybackState(playback) => {
                        app.state.apply_playback_state(playback);
                    }
                }
                if !app.state.running {
                    break;
                }
            }
        }
    }

    app.shutdown().await;
    Ok(())
}

fn spawn_terminal_reader(tx: mpsc::UnboundedSender<UiEvent>) {
    std::thread::spawn(move || {
        loop {
            match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    if tx.send(UiEvent::Key(key)).is_err() {
                        break;
                    }
                }
                Ok(Event::Paste(text)) => {
                    if tx.send(UiEvent::Paste(text)).is_err() {
                        break;
                    }
                }
                Ok(Event::Resize(width, height)) => {
                    if tx.send(UiEvent::Resize(width, height)).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });
}

struct App {
    state: AppState,
    loading_spinner: LoadingSpinner,
    picker: Picker,
    player: Player,
    tx: mpsc::UnboundedSender<UiEvent>,
    chat_load_task: Option<tokio::task::JoinHandle<()>>,
}

impl App {
    async fn new(
        config: Config,
        keymap: Keymap,
        messengers: Vec<MessengerKind>,
        open_settings: bool,
        picker: Picker,
        tx: mpsc::UnboundedSender<UiEvent>,
    ) -> Self {
        Self {
            state: AppState::new(config, keymap, messengers, open_settings).await,
            loading_spinner: LoadingSpinner::new(),
            picker,
            player: Player::new(Box::new(RodioEngine::default()), tx.clone()),
            tx,
            chat_load_task: None,
        }
    }

    fn start_chat_load(&mut self, index: usize) {
        self.cancel_chat_load();
        let Some((chat, generation, mut messenger)) = self.state.begin_chat_load(index) else {
            return;
        };

        let tx = self.tx.clone();
        self.chat_load_task = Some(tokio::spawn(async move {
            // Subscribe to presence and read the current user status while the
            // chat loads; WhatsApp only sends presence updates to chats it was
            // asked to track, so asking here is what makes Online/Last Seen
            // appear at all.
            let status = messenger.status(&chat.id).await.unwrap_or(None);
            let result = match messenger.set_read(&chat.id).await {
                Ok(()) => messenger.history(&chat.id).await,
                Err(e) => Err(e),
            };
            let _ = tx.send(UiEvent::ChatLoaded {
                chat,
                generation,
                result,
                status,
            });
        }));
    }

    fn cancel_chat_load(&mut self) {
        if let Some(task) = self.chat_load_task.take() {
            task.abort();
        }
        self.state.cancel_chat_load();
    }

    fn load_more_history(&mut self) {
        let Some(chat_id) = self
            .state
            .chat_state
            .open_chat
            .as_ref()
            .map(|open| open.chat.id.clone())
        else {
            return;
        };

        let Some(messenger) = self
            .state
            .provider_to_messenger(chat_id.to_provider())
            .cloned()
        else {
            return;
        };

        let Some(open) = self.state.chat_state.open_chat.as_ref() else {
            return;
        };

        if !open.has_more_history {
            return;
        }

        // Telegram paginates by numeric message id; WhatsApp's ids are opaque
        // (`to_i32()` → `None`), so the backend anchors on its own oldest cached
        // message instead. Fetch in a background task: WhatsApp's on-demand sync
        // can take seconds, and waiting on it inline would freeze the UI.
        let offset_id = open.history.first().and_then(|msg| msg.message_id.to_i32());

        self.cancel_chat_load();
        let tx = self.tx.clone();
        let task_chat = chat_id.clone();
        self.chat_load_task = Some(tokio::spawn(async move {
            let result = messenger.history_page(&task_chat, offset_id, 25).await;
            let _ = tx.send(UiEvent::HistoryPageResult(task_chat, result));
        }));
    }

    fn open_image_popup(&mut self, msg: Message) {
        debug!("Opening image for {:?}", msg.message_id);
        let Some(messenger) = self.state.chat_owner(&msg.chat).cloned() else {
            return;
        };

        let chat = msg.chat.clone();
        let message_id = msg.message_id.clone();

        self.state.create_popup(PopupKind::Image(ImagePopup {
            msg,
            view: ImageWidgetState::new(self.tx.clone()),
        }));

        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = messenger.media_bytes(&chat, &message_id).await;
            let _ = tx.send(UiEvent::ImageMedia {
                chat,
                message_id,
                result,
            });
        });
    }

    async fn apply_image_media(
        &mut self,
        chat: ChatId,
        message_id: MessageId,
        result: Result<Option<Vec<u8>>, BackendError>,
    ) {
        let Some(popup) = self.state.pop_up.as_mut() else {
            return;
        };

        let PopupKind::Image(image_popup) = &mut popup.popup_type else {
            return;
        };

        // Only apply if the popup still shows the same message.
        if image_popup.msg.chat != chat || image_popup.msg.message_id != message_id {
            return;
        }

        let view = &mut image_popup.view;

        match result {
            Ok(Some(bytes)) => {
                // Decode off the async runtime; image decoding is CPU-bound.
                let decoded =
                    tokio::task::spawn_blocking(move || ::image::load_from_memory(&bytes)).await;

                match decoded {
                    Ok(Ok(image)) => view.set_image(&self.picker, image),
                    Ok(Err(e)) => {
                        error!("Err Failed to decode image {e}");
                        view.set_error(format!("Failed to decode image: {e}"))
                    }
                    Err(e) => {
                        error!("Image decode task failed: {e}");
                        view.set_error(format!("Image decode task failed: {e}"))
                    }
                }
            }

            Ok(None) => {
                warn!("Media not available error!");
                view.set_error("Media not available".to_string())
            }
            Err(e) => {
                error!("Failed to load image: {e}");
                view.set_error(format!("Failed to load image: {e}"))
            }
        }
    }

    async fn shutdown(&mut self) {
        self.cancel_chat_load();
        self.player.stop();

        for messenger in &mut self.state.messengers {
            if let Err(err) = messenger.disconnect().await {
                error!(
                    provider = ?messenger.provider(),
                    error = %err,
                    "Messenger shutdown failed"
                );
            }
        }
        self.state.running = false;
    }

    async fn handle_key(&mut self, key: KeyEvent) {
        // TODO: add a overlay menu / screen to display all the keymappings
        if key == self.state.keymap.quit {
            self.state.running = false;
            return;
        }

        // Keep the input mode in sync with which pane is focused, so
        // app-level bindings can be gated on whether a text box has focus.
        self.state.update_mode();

        if self.state.pop_up.is_some() {
            self.handle_popup_key(key).await;
            return;
        }

        match self.state.screen {
            Screen::Settings => self.handle_settings_key(key).await,
            Screen::Main => self.handle_main_key(key).await,
            Screen::Login => self.handle_login_key(key).await,
        }
    }

    async fn handle_settings_key(&mut self, key: KeyEvent) {
        let km = self.state.keymap.clone();
        if key == km.dismiss || key == km.open_settings {
            self.state.screen = Screen::Main;
        } else if key == km.scroll_up {
            let index = self
                .state
                .settings_state
                .selected()
                .unwrap_or(0)
                .saturating_sub(1);
            self.state.settings_state.select(Some(index));
        } else if key == km.scroll_down {
            let index = self.state.settings_state.selected().unwrap_or(0);
            if index + 1 < Provider::all().len() {
                self.state.settings_state.select(Some(index + 1));
            }
        } else if key == km.select {
            let index = self.state.settings_state.selected().unwrap_or(0);
            let Some(&provider) = Provider::all().get(index) else {
                return;
            };
            self.state.toggle_provider(provider).await;
        }
    }

    async fn handle_login_key(&mut self, key: KeyEvent) {
        let km = self.state.keymap.clone();
        if key == km.dismiss {
            self.state.cancel_login().await;
        } else if key == km.send {
            self.state.submit_login().await;
        } else if let Some(ref mut ls) = self.state.login_state {
            ls.login_input.input(key);
        }
    }

    async fn handle_main_key(&mut self, key: KeyEvent) {
        let km = self.state.keymap.clone();
        let searching = self.state.chat_list_search_active() || self.state.message_search_active();

        // App-level bindings are inert while a text box has focus; only the
        // explicitly captured keys (esc/ctrl+c/enter, tab in the write box)
        // are handled, everything else reaches the text widget.
        let insert = self.state.mode == Mode::Insert;

        // Esc while a flood-wait countdown is active cancels the in-flight
        // refresh and defers it to the next chat-list sync tick, instead of
        // cycling focus like a regular dismiss.
        if !insert
            && key == km.dismiss
            && !searching
            && self.state.rate_limit.is_some()
            && self.state.cancel_rate_limit_wait().await
        {
            return;
        }

        // Manually force a reconnection after a provider entered the
        // "connection lost" state. No-op for providers without a persistent
        // transport or when no reconnect is pending.
        if !insert && key == km.retry_connection {
            if let Some(messenger) = self.state.provider_to_messenger(Provider::Telegram)
                && let Err(e) = messenger.reconnect().await
            {
                warn!(error = %e, "Manual reconnect failed");
            }
            return;
        }

        // While an active chat-list search is showing, Esc is handled inside the
        // ChatList branch (cancel the search) instead of cycling focus.
        if !insert && key == km.dismiss && !searching {
            if self.state.focus == Focus::Chat && self.state.chat_state.open_chat.is_none() {
                self.cancel_chat_load();
                self.state.focus = Focus::ChatList;
                return;
            }
            self.state.cycle_focus();
            return;
        }

        if !insert && key == km.open_settings {
            self.state.screen = Screen::Settings;
            return;
        }

        // Tab exits the write box even while typing (insert mode).
        if key == km.pane_next && (!insert || self.state.focus == Focus::Write) {
            self.state.cycle_focus();
            return;
        }

        // Jump from chat list to Write Box in selected chat
        if !insert && key == km.focus_write {
            match &self.state.chat_state.open_chat {
                Some(_) => {
                    self.state.focus = Focus::Write;
                    return;
                }
                None => match self.state.selected_chat_idx() {
                    Some(chat) => {
                        self.start_chat_load(chat);
                        self.state.focus = Focus::Write;
                        return;
                    }
                    None => {
                        return;
                    }
                },
            }
        }

        match self.state.focus {
            Focus::ChatList => {
                if self.state.chat_list_search_typing() {
                    if key == km.dismiss {
                        self.state.chat_state.clear_search();
                    } else if key == km.select {
                        self.state.chat_state.commit_search();
                    } else {
                        self.state.chat_state.search_input(key);
                    }
                    return;
                }

                if key == km.dismiss && searching {
                    self.state.chat_state.clear_search();
                } else if key == km.search_text {
                    self.state.chat_state.begin_search();
                } else if key == km.scroll_up {
                    self.state.chat_state.chat_list_state.select_previous();
                } else if key == km.scroll_to_bottom {
                    self.state.chat_state.chat_list_state.select_last();
                } else if key == km.scroll_down {
                    self.state.chat_state.chat_list_state.select_next();
                } else if key == km.select
                    && !self.state.chat_state.chats.is_empty()
                    && self.state.chat_state.chat_list_state.selected().is_some()
                {
                    match self.state.selected_chat_idx() {
                        Some(chat) => self.start_chat_load(chat),
                        None => {
                            self.state
                                .create_popup(PopupKind::Error(String::from("No chat selected")));
                        }
                    };
                }
            }
            Focus::Chat => {
                if self.state.message_search_typing() {
                    if key == km.dismiss {
                        self.state.chat_state.clear_message_search();
                    } else if key == km.select {
                        self.state.chat_state.commit_message_search();
                    } else {
                        self.state.chat_state.message_search_input(key);
                    }
                    return;
                }

                if key == km.dismiss && self.state.message_search_active() {
                    self.state.chat_state.clear_message_search();
                    return;
                }

                if key == km.search_text {
                    self.state.chat_state.begin_message_search();
                    return;
                }

                if key == km.scroll_up {
                    if self
                        .state
                        .chat_state
                        .message_list_state
                        .selected()
                        .is_some_and(|current| current == 0)
                    {
                        self.load_more_history();
                    } else {
                        self.state.chat_state.message_list_state.select_previous();
                    }
                } else if key == km.scroll_down {
                    self.state.chat_state.message_list_state.select_next();
                } else if key == km.scroll_to_bottom {
                    let last = self
                        .state
                        .chat_state
                        .open_chat
                        .as_ref()
                        .map(|o| o.history.len().saturating_sub(1));
                    self.state.chat_state.message_list_state.select(last);
                } else if key.code == KeyCode::PageUp {
                    if let Some(current) = self.state.chat_state.message_list_state.selected()
                        && current > 0
                    {
                        let page = self.state.chat_state.visible_page;
                        let new = current.saturating_sub(page);
                        self.state.chat_state.message_list_state.select(Some(new));
                    } else {
                        self.load_more_history();
                    }
                } else if key.code == KeyCode::PageDown {
                    // TODO: use scroll_up_by/ scroll_down_by here
                    let page = self.state.chat_state.visible_page;
                    let current = self
                        .state
                        .chat_state
                        .message_list_state
                        .selected()
                        .unwrap_or(0);
                    let new = current.saturating_add(page);
                    let max = self
                        .state
                        .chat_state
                        .open_chat
                        .as_ref()
                        .map(|o| o.history.len().saturating_sub(1))
                        .unwrap_or(0);
                    self.state
                        .chat_state
                        .message_list_state
                        .select(Some(new.min(max)));
                } else if key == km.select
                    && let Some(idx) = self.state.chat_state.message_list_state.selected()
                    && let Some(open) = &self.state.chat_state.open_chat
                    && let Some(msg) = open.history.get(idx)
                {
                    let mut msg = msg.clone();
                    if msg.reply_ctx.is_none()
                        && msg.reply_to_id.is_some()
                        && let Some(messenger) = self.state.chat_owner(&msg.chat)
                    {
                        match messenger.reply_context(&msg.chat, &msg.message_id).await {
                            Ok(context) => msg.reply_ctx = context,
                            Err(error) => {
                                tracing::debug!(
                                    message = %msg.message_id,
                                    %error,
                                    "Unable to load reply context for popup"
                                );
                            }
                        }
                    }

                    // Offer actions appropriate to the message's state.
                    msg.msg_actions = available_message_actions(&msg);

                    if msg
                        .media
                        .as_ref()
                        .is_some_and(|m| m.kind == MediaKind::Image)
                    {
                        self.open_image_popup(msg);
                    } else {
                        self.state.create_popup(PopupKind::Message(msg));
                    }
                }
            }
            Focus::Write => {
                // Esc leaves the write box (saving the draft); the global
                // dismiss binding is gated off while a text box has focus, so
                // it is captured here instead.
                if key == km.dismiss {
                    self.state.cycle_focus();
                    return;
                }

                if key.kind != KeyEventKind::Press {
                    // Paste-sourced keys are never send/newline actions.
                    self.state.write.input(key);
                } else if key == km.send {
                    self.state.send_message().await;
                } else if key == km.newline || key.code == KeyCode::Enter {
                    // A configured newline key (Shift+Enter by default) inserts a newline.
                    // Any Enter with a modifier (Alt/Ctrl) also inserts a newline, since many
                    // terminals cannot report modifiers on Enter and treat it as plain Enter.
                    self.state.write.insert_newline();
                } else {
                    self.state.write.input(key);
                }
            }
            Focus::Popup => {}
        }
    }

    async fn handle_popup_key(&mut self, key: KeyEvent) {
        if key == self.state.keymap.dismiss {
            self.state.dismiss_popup();
        }

        let km = self.state.keymap.clone();

        let popup = match self.state.pop_up.as_mut() {
            Some(p) => p,
            None => return,
        };

        if key == km.scroll_up {
            popup.scroll_idx = popup.scroll_idx.saturating_sub(1);
        } else if key == km.scroll_down {
            popup.scroll_idx = popup.scroll_idx.saturating_add(1);
        }

        let action = {
            if let PopupKind::Error(_) = &popup.popup_type
                && self.state.retry_draft.is_some()
                && key == KeyCode::Char('1').into()
            {
                self.state.dismiss_popup();
                self.state.retry_message().await;
                return;
            } else if let PopupKind::Message(msg) | PopupKind::Image(ImagePopup { msg, .. }) =
                &popup.popup_type
                && let KeyCode::Char(c) = key.code
                && let Some(digit) = c.to_digit(10)
            {
                let idx = (digit as usize).saturating_sub(1);
                (idx < msg.msg_actions.len())
                    .then(|| (msg.msg_actions[idx].clone(), msg.message_id.clone()))
            } else if let PopupKind::Question(_) = &popup.popup_type
                && let KeyCode::Char(c) = key.code
                && let Some(digit) = c.to_digit(10)
            {
                if let Some(msg) = &self.state.chat_state.selected_message() {
                    let idx = (digit as usize).saturating_sub(1);
                    (idx <= 2).then(|| (MessageAction::Delete, msg.message_id.clone()))
                } else {
                    None
                }
            } else {
                None
            }
        };

        if let Some((action, msg_id)) = action {
            self.state.dismiss_popup();
            match action {
                MessageAction::Reply => self.state.reply_to_message(&msg_id),
                MessageAction::Edit => {
                    self.state.edit_message(&msg_id);
                }
                MessageAction::Delete => {
                    self.state.create_popup(PopupKind::Question(String::from(
                        "Do you really want to delete this message?",
                    )));

                    if key == KeyCode::Char('1').into() {
                        self.state.delete_message(&msg_id).await;
                        self.state.dismiss_popup();
                    }

                    if key == KeyCode::Char('2').into() {
                        self.state.dismiss_popup();
                    }
                }
                MessageAction::Retry => {
                    let _ = msg_id;
                }
            }
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        self.loading_spinner.tick();
        match self.state.screen {
            Screen::Main => self.draw_main(frame),
            Screen::Settings => {
                Settings::new(&self.state.config.providers).render(
                    frame.area(),
                    frame.buffer_mut(),
                    &mut self.state.settings_state,
                );
            }

            Screen::Login => {
                if let Some(login_state) = self.state.login_state.as_ref() {
                    let provider = login_state.provider;
                    let step = login_state.step;
                    if let Some(messenger) = self.state.provider_to_messenger(provider) {
                        let steps = messenger.login_steps();
                        let placeholder = messenger.login_placeholder(step);
                        let masked = steps.get(step) == Some(&AuthSteps::Password);

                        if let Some(ls) = self.state.login_state.as_mut() {
                            if ls.submitting {
                                let step_label =
                                    steps.get(step).map(|s| s.to_string()).unwrap_or_default();
                                LoadingWidget::new(
                                    &format!(" {} — {} ", provider.name(), step_label),
                                    Some(format!("Waiting for {} ...", provider.name())),
                                    &mut self.loading_spinner,
                                )
                                .render(frame.area(), frame.buffer_mut());
                                return;
                            }

                            ls.login_input.set_placeholder_text(placeholder);

                            if masked {
                                ls.login_input.set_mask_char('●');
                            } else {
                                ls.login_input.clear_mask_char();
                            }

                            login::LoginScreen {
                                provider_name: provider.name(),
                                steps: &steps,
                            }
                            .render(
                                frame.area(),
                                frame.buffer_mut(),
                                ls,
                            );
                        }
                    }
                }
            }
        }

        // add this here to the popup actually clears the content below it
        if let Some(state) = &mut self.state.pop_up {
            popup::PopUp::new().render(frame.area(), frame.buffer_mut(), state);
        }
    }

    fn draw_main(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let vertical = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).split(area);

        let horizontal =
            Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)])
                .split(vertical[0]);

        let visible = self.state.chat_state.visible_indices();
        let visible_chats: Vec<&Chat> = if visible.is_empty() {
            self.state.chat_state.chats.iter().collect()
        } else {
            visible
                .iter()
                .filter_map(|&idx| self.state.chat_state.chats.get(idx))
                .collect()
        };
        ChatList::new(
            self.state.chat_state.get_tag(),
            visible_chats,
            self.state.focus,
            &self.state.chat_state.search,
        )
        .render(
            horizontal[0],
            frame.buffer_mut(),
            &mut self.state.chat_state.chat_list_state,
        );

        if !self.state.chats_loaded && self.state.chat_state.chats.is_empty() {
            LoadingWidget::new(
                " Sender ",
                Some("Fetching chats...".to_string()),
                &mut self.loading_spinner,
            )
            .render(horizontal[0], frame.buffer_mut());
        }

        let (message_search_bar, message_needle) = {
            let search = &self.state.chat_state.message_search;
            (
                search.title_line(),
                search
                    .is_active()
                    .then(|| search.query_text().trim().to_string()),
            )
        };

        ChatWidget::new(
            self.state.focus,
            &mut self.state.write,
            self.state.config.max_write_lines,
            message_search_bar,
            message_needle,
        )
        .render(
            horizontal[1],
            frame.buffer_mut(),
            &mut self.state.chat_state,
        );

        let name = self
            .state
            .chat_state
            .selected_chat()
            .map(|c| c.contact_name.clone());

        StatusBarWidget::new(name, self.state.focus, self.state.status_hint())
            .render(vertical[1], frame.buffer_mut());

        if let Some(open) = &self.state.chat_state.open_chat {
            debug!(
                chat = %open.chat.contact_name,
                history_len = open.history.len(),
                focus = ?self.state.focus,
                "TUI render complete"
            );
        }
    }
}
