use crate::backend::AuthSteps;

use super::{BackendError, BackendEvent, Chat, ChatId, LoginStepState, Message, Messenger};
use anyhow::Result;
use grammers_client::sender::SenderPool;
use grammers_client::session::storages::SqliteSession;
use grammers_client::{Client, SignInError};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};

/// The active login step. Stored between `request_code` and `sign_in` calls
/// so the TUI can collect user input across multiple frames.
///
/// Added clippy::large_enum_variant because the mem-usage here is temporary
#[allow(clippy::large_enum_variant)]
pub enum LoginToken {
    CodeRequested {
        token: grammers_client::client::LoginToken,
    },
    PasswordRequired {
        token: grammers_client::client::PasswordToken,
    },
}

pub struct TelegramMessenger {
    client: Client,
    api_hash: String,
    tx: broadcast::Sender<BackendEvent>,
    login_token: Arc<RwLock<Option<LoginToken>>>,
}

impl TelegramMessenger {
    /// Open (or create) the session, build a [`SenderPool`] and [`Client`].
    /// Spawns the pool runner in a background task.
    /// Does **not** attempt to log in – call [`Messenger::is_authenticated`]
    /// or [`Messenger::login`] afterwards.
    pub async fn new(
        session_dir: PathBuf,
        api_id: u32,
        api_hash: &str,
    ) -> Result<Self, BackendError> {
        let session_path = session_dir.join("tg_session.sqlite");
        let session = Arc::new(
            SqliteSession::open(&session_path)
                .await
                .map_err(|e| BackendError::Other(format!("failed to open TG session: {e}")))?,
        );

        let pool = SenderPool::new(session, api_id as i32);

        // The runner must be spawned; it drives all network I/O.
        let runner = pool.runner;
        tokio::spawn(async move { runner.run().await });

        let client = Client::new(pool.handle);

        let (tx, _) = broadcast::channel(128);

        Ok(Self {
            client,
            api_hash: api_hash.to_string(),
            tx,
            login_token: Arc::new(RwLock::new(None)),
        })
    }
}

#[async_trait::async_trait]
impl Messenger for TelegramMessenger {
    fn platform(&self) -> &'static str {
        "Telegram"
    }

    async fn is_authenticated(&self) -> bool {
        self.client.is_authorized().await.unwrap_or(false)
    }

    async fn chats(&self) -> Result<Vec<Chat>, BackendError> {
        // Phase 2: will iterate dialogs and map to Chat
        Err(BackendError::Other("not implemented yet".into()))
    }

    async fn set_read(&mut self, _chat: &ChatId) -> Result<(), BackendError> {
        // Phase 3: will call client.mark_as_read
        Ok(())
    }

    async fn history(&self, _chat: &ChatId) -> Result<Vec<Message>, BackendError> {
        // Phase 2: will iterate messages and map to Message
        Err(BackendError::Other("not implemented yet".into()))
    }

    async fn send(&self, _chat: &ChatId, _text: &str) -> Result<(), BackendError> {
        // Phase 3: will call client.send_message
        Err(BackendError::Other("not implemented yet".into()))
    }

    fn subscribe(&self) -> broadcast::Receiver<BackendEvent> {
        self.tx.subscribe()
    }

    async fn disconnect(&mut self) -> Result<(), BackendError> {
        self.client.disconnect();
        Ok(())
    }

    async fn login(&mut self) -> Result<(), BackendError> {
        if self.is_authenticated().await {
            return Ok(());
        }
        Err(BackendError::NotAuthenticated)
    }

    async fn logout(&mut self) -> Result<(), BackendError> {
        self.client
            .sign_out()
            .await
            .map_err(|e| BackendError::Other(format!("sign-out failed: {e}")))?;
        Ok(())
    }

    async fn cancel_login(&mut self) {
        *self.login_token.write().await = None;
    }

    fn login_steps(&self) -> Vec<AuthSteps> {
        vec![AuthSteps::Phone, AuthSteps::Code, AuthSteps::Password]
    }

    fn login_placeholder(&self, step: usize) -> &'static str {
        match step {
            0 => "+1234567890",
            1 => "12345",
            2 => "your 2FA password",
            _ => "",
        }
    }

    async fn login_step(
        &mut self,
        step: usize,
        input: &str,
    ) -> Result<LoginStepState, BackendError> {
        match step {
            0 => {
                let token = self
                    .client
                    .request_login_code(input, &self.api_hash)
                    .await?;
                *self.login_token.write().await = Some(LoginToken::CodeRequested { token });
                Ok(LoginStepState::NextStep)
            }
            1 => {
                let token = self.login_token.write().await.take().ok_or_else(|| {
                    BackendError::Other("no login token – call request_code first".into())
                })?;

                match token {
                    LoginToken::CodeRequested { token } => {
                        match self.client.sign_in(&token, input).await {
                            Ok(_user) => Ok(LoginStepState::Done),
                            Err(SignInError::PasswordRequired(password_token)) => {
                                *self.login_token.write().await =
                                    Some(LoginToken::PasswordRequired {
                                        token: password_token,
                                    });
                                Ok(LoginStepState::NextStep)
                            }
                            Err(SignInError::InvalidCode) => {
                                Err(BackendError::Other("invalid code".into()))
                            }
                            Err(SignInError::SignUpRequired) => Err(BackendError::Other(
                                "sign-up required – create an account with an official Telegram client first"
                                    .into(),
                            )),
                            Err(other) => {
                                Err(BackendError::Other(format!("sign-in failed: {other}")))
                            }
                        }
                    }
                    LoginToken::PasswordRequired { .. } => Err(BackendError::Other(
                        "call check_password instead of sign_in".into(),
                    )),
                }
            }
            2 => {
                let token = self
                    .login_token
                    .write()
                    .await
                    .take()
                    .ok_or_else(|| BackendError::Other("no password token".into()))?;

                match token {
                    LoginToken::PasswordRequired { token } => {
                        match self.client.check_password(token, input.as_bytes()).await {
                            Ok(_user) => Ok(LoginStepState::Done),
                            Err(SignInError::InvalidPassword(_)) => {
                                Err(BackendError::Other("invalid password".into()))
                            }
                            Err(other) => Err(BackendError::Other(format!(
                                "password check failed: {other}"
                            ))),
                        }
                    }
                    LoginToken::CodeRequested { .. } => Err(BackendError::Other(
                        "call sign_in instead of check_password".into(),
                    )),
                }
            }
            _ => Err(BackendError::Other("unknown login step".into())),
        }
    }
}
