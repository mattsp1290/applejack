//! Login, refresh, and logout, driven over a [`SteamConnection`] so unit tests
//! run against a fake while [`crate::live::SteamroomConnection`] talks to
//! Steam. A successful logon yields a [`SteamSession`], which owns everything
//! that happens after the auth flow.

use std::fmt;
use std::future::Future;
use std::time::Duration;

use crate::Error;
use crate::keychain::{RefreshToken, RefreshTokenStore};
use crate::session::SteamSession;

/// How a login starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    /// Print a challenge for the Steam mobile app to scan.
    Qr,
    /// Account name and password, then Steam Guard.
    Credentials,
}

/// The second factor Steam asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardFactor {
    /// A code sent by email.
    EmailCode,
    /// A code from the Steam mobile app.
    DeviceCode,
    /// Approval in the Steam mobile app, with no code to type.
    DeviceConfirmation,
}

/// What the caller must do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginStep {
    /// Render `payload` as a QR code, then call [`Authenticator::finish`].
    QrReady { payload: String },
    /// Collect a code and call [`Authenticator::submit_guard_code`], or for
    /// [`GuardFactor::DeviceConfirmation`] call [`Authenticator::finish`].
    GuardRequired { factor: GuardFactor },
    /// Steam has the answer and has not issued tokens yet; poll again.
    Waiting,
    /// The refresh token is stored.
    Done { account_name: String },
}

/// One look at a pending auth session.
#[derive(Debug, Clone)]
pub enum AuthPoll {
    /// Not approved yet. `challenge` is a rotated QR payload to render.
    Pending {
        challenge: Option<String>,
    },
    Approved(Tokens),
}

/// Failures while talking to Steam. Never carries a token or password.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum AuthError {
    /// Steam throttled the request; do not retry before `retry_after`.
    #[error("rate limited by Steam; retry after {retry_after:?}")]
    RateLimited { retry_after: Duration },
    /// A second factor is needed before the step can complete.
    #[error("Steam Guard {factor:?} required")]
    GuardRequired { factor: GuardFactor },
    /// Steam rejected the credentials or the token. Only a login answers
    /// this; a CLI sends the user to log in again.
    #[error("Steam refused the login")]
    Refused,
    /// Steam denied one call on a healthy session, such as a depot this
    /// account cannot reach. Logging in again will not help.
    #[error("Steam denied {call}: {result}")]
    CallDenied { call: String, result: String },
    /// The Steam Guard code was wrong; the session is still open.
    #[error("Steam Guard code rejected")]
    InvalidGuardCode,
    /// No refresh token is stored; the user must log in.
    #[error("no stored Steam refresh token")]
    NoStoredToken,
    /// A step was called before the one it needs.
    #[error("login step out of order: {0}")]
    OutOfOrder(&'static str),
    /// Steam answered without a field the flow needs.
    #[error("Steam response missing field `{0}`")]
    MissingField(&'static str),
    /// Transport, protocol, or decode failure.
    #[error("connection: {0}")]
    Connection(String),
    /// This CM could not take the logon: it would not connect, answered
    /// `TryAnotherCM`, hung up before answering, or never answered. Another
    /// may, so `logon` moves on.
    #[error("{0}")]
    CmUnavailable(String),
}

/// A password typed by the user. `Debug` never reveals the value, and the
/// type is not comparable: a derived comparison is not constant time.
#[derive(Clone)]
pub struct Password(String);

impl Password {
    pub fn new(password: impl Into<String>) -> Self {
        Self(password.into())
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(<redacted>)")
    }
}

/// A short-lived Steam access token, kept in memory only. Not comparable,
/// for the same reason as [`Password`].
#[derive(Clone)]
pub struct AccessToken(String);

impl AccessToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AccessToken(<redacted>)")
    }
}

/// Account name and password for [`AuthMode::Credentials`].
#[derive(Debug, Clone)]
pub struct Credentials {
    pub account_name: String,
    pub password: Password,
}

/// What a completed auth session yields. Not comparable: the tokens are
/// secrets, and nothing needs to compare them.
#[derive(Debug, Clone)]
pub struct Tokens {
    pub account_name: String,
    pub refresh_token: RefreshToken,
    pub access_token: AccessToken,
}

/// One entry of `CMsgClientLicenseList`. `Debug` omits `access_token`, the
/// per-package PICS access token.
#[derive(Clone, PartialEq, Eq)]
pub struct License {
    pub package_id: u32,
    pub license_type: u32,
    pub flags: u32,
    pub owner_id: u32,
    pub access_token: u64,
}

impl fmt::Debug for License {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("License")
            .field("package_id", &self.package_id)
            .field("license_type", &self.license_type)
            .field("flags", &self.flags)
            .field("owner_id", &self.owner_id)
            .finish_non_exhaustive()
    }
}

/// The auth flow, which needs no logged-in session. Each method advances one
/// connection's state; the order is `begin_qr` or `begin_credentials`, then
/// `poll_tokens` every [`poll_interval`](Self::poll_interval), with
/// `submit_guard_code` in between when a code arrives. Polling during a code
/// prompt is what lets an approval in the Steam mobile app finish the login.
/// `logon` turns a refresh token into a [`SteamSession`]. A call made in the
/// wrong state is [`AuthError::OutOfOrder`] and leaves the state unchanged.
pub trait SteamConnection {
    /// What a successful `logon` yields.
    type Session: SteamSession;

    /// Starts a QR session and returns the challenge payload to render.
    fn begin_qr(&mut self) -> impl Future<Output = Result<String, AuthError>> + Send;

    /// Starts a credentials session. `Some(factor)` means Steam Guard is
    /// needed before tokens are issued.
    fn begin_credentials(
        &mut self,
        credentials: &Credentials,
    ) -> impl Future<Output = Result<Option<GuardFactor>, AuthError>> + Send;

    /// Submits a Steam Guard code for the pending credentials session. A
    /// session approved in the mobile app while the code was on its way
    /// succeeds here: the next poll has the tokens.
    fn submit_guard_code(
        &mut self,
        code: &str,
    ) -> impl Future<Output = Result<(), AuthError>> + Send;

    /// One poll of the pending session. Valid while a code is awaited too.
    /// Any error ends the session.
    fn poll_tokens(&mut self) -> impl Future<Output = Result<AuthPoll, AuthError>> + Send;

    /// How often Steam asked to be polled.
    fn poll_interval(&self) -> Duration;

    /// Opens a logged-in session from a refresh token.
    fn logon(
        &mut self,
        account_name: &str,
        refresh_token: &RefreshToken,
    ) -> impl Future<Output = Result<Self::Session, AuthError>> + Send;
}

/// Where a login stands. Copy it out with [`Authenticator::state`] to keep a
/// login across calls, and hand it to [`Authenticator::resume`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginState {
    Idle,
    AwaitingTokens,
    AwaitingGuardCode(GuardFactor),
}

/// Drives login, refresh, and logout over a connection and a token store.
#[derive(Debug)]
pub struct Authenticator<'a, C, S: ?Sized> {
    connection: &'a mut C,
    store: &'a S,
    state: LoginState,
}

impl<'a, C: SteamConnection, S: RefreshTokenStore + ?Sized> Authenticator<'a, C, S> {
    pub fn new(connection: &'a mut C, store: &'a S) -> Self {
        Self::resume(connection, store, LoginState::Idle)
    }

    /// Picks up a login at `state`, over the connection that started it.
    pub fn resume(connection: &'a mut C, store: &'a S, state: LoginState) -> Self {
        Self {
            connection,
            store,
            state,
        }
    }

    #[must_use]
    pub const fn state(&self) -> LoginState {
        self.state
    }

    /// Starts a login. [`AuthMode::Credentials`] needs `credentials`.
    ///
    /// # Errors
    ///
    /// [`AuthError`] from the connection, or [`Error::Keychain`] when the
    /// token cannot be stored.
    pub async fn begin(
        &mut self,
        mode: AuthMode,
        credentials: Option<&Credentials>,
    ) -> Result<LoginStep, Error> {
        match mode {
            AuthMode::Qr => {
                let payload = self.connection.begin_qr().await?;
                self.state = LoginState::AwaitingTokens;
                Ok(LoginStep::QrReady { payload })
            }
            AuthMode::Credentials => {
                let credentials = credentials.ok_or(AuthError::OutOfOrder(
                    "credentials mode needs an account name and password",
                ))?;
                match self.connection.begin_credentials(credentials).await? {
                    Some(GuardFactor::DeviceConfirmation) => {
                        self.state = LoginState::AwaitingTokens;
                        Ok(LoginStep::GuardRequired {
                            factor: GuardFactor::DeviceConfirmation,
                        })
                    }
                    Some(factor) => {
                        self.state = LoginState::AwaitingGuardCode(factor);
                        Ok(LoginStep::GuardRequired { factor })
                    }
                    None => {
                        self.state = LoginState::AwaitingTokens;
                        self.finish().await
                    }
                }
            }
        }
    }

    /// Submits a Steam Guard code and polls once. [`LoginStep::Waiting`]
    /// means the code was taken and the tokens are still on their way.
    ///
    /// # Errors
    ///
    /// [`AuthError::OutOfOrder`] when no code is pending.
    /// [`AuthError::InvalidGuardCode`] leaves the session open: call again
    /// with another code. Any other error ends the login, because the
    /// connection has dropped its session; the next call must be
    /// [`Authenticator::begin`], and `finish` or `submit_guard_code` before
    /// that is [`AuthError::OutOfOrder`].
    pub async fn submit_guard_code(&mut self, code: &str) -> Result<LoginStep, Error> {
        let LoginState::AwaitingGuardCode(_) = self.state else {
            return Err(AuthError::OutOfOrder("no Steam Guard code is pending").into());
        };
        match self.connection.submit_guard_code(code).await {
            Ok(()) => {}
            // The challenge survives a wrong code; anything else ends the
            // session, and a driver that kept asking for codes would loop.
            Err(AuthError::InvalidGuardCode) => return Err(AuthError::InvalidGuardCode.into()),
            Err(error) => {
                self.state = LoginState::Idle;
                return Err(error.into());
            }
        }
        self.state = LoginState::AwaitingTokens;
        self.poll().await
    }

    /// One look at the pending login, valid while a code is awaited: an
    /// approval in the Steam mobile app finishes the login from here.
    /// Stores the refresh token when Steam issues it.
    ///
    /// # Errors
    ///
    /// [`AuthError::OutOfOrder`] when no login is in progress. Any other
    /// error ends the login, as the auth flow is consumed on failure, so the
    /// next call must be [`Authenticator::begin`].
    pub async fn poll(&mut self) -> Result<LoginStep, Error> {
        if self.state == LoginState::Idle {
            return Err(AuthError::OutOfOrder("no login in progress").into());
        }
        match self.connection.poll_tokens().await {
            Ok(AuthPoll::Pending { challenge }) => Ok(match (challenge, self.state) {
                (Some(payload), _) => LoginStep::QrReady { payload },
                (None, LoginState::AwaitingGuardCode(factor)) => {
                    LoginStep::GuardRequired { factor }
                }
                (None, _) => LoginStep::Waiting,
            }),
            Ok(AuthPoll::Approved(tokens)) => {
                self.state = LoginState::Idle;
                self.store.put(&tokens.refresh_token)?;
                Ok(LoginStep::Done {
                    account_name: tokens.account_name,
                })
            }
            Err(error) => {
                self.state = LoginState::Idle;
                Err(error.into())
            }
        }
    }

    /// Polls at Steam's interval until the login ends, for a driver with no
    /// code to wait for.
    ///
    /// # Errors
    ///
    /// [`AuthError::GuardRequired`] from the driver's own state (a code is
    /// still pending), which leaves the login where it was, and what
    /// [`poll`](Self::poll) reports.
    pub async fn finish(&mut self) -> Result<LoginStep, Error> {
        if let LoginState::AwaitingGuardCode(factor) = self.state {
            return Err(AuthError::GuardRequired { factor }.into());
        }
        loop {
            let step = self.poll().await?;
            if matches!(step, LoginStep::Done { .. }) {
                return Ok(step);
            }
            tokio::time::sleep(self.connection.poll_interval()).await;
        }
    }

    /// Opens a session from the stored refresh token. The account name is not
    /// secret; the host keeps it next to its other metadata.
    ///
    /// # Errors
    ///
    /// [`AuthError::NoStoredToken`] when the store is empty;
    /// [`AuthError::Refused`] when Steam rejects the token.
    pub async fn refresh(&mut self, account_name: &str) -> Result<C::Session, Error> {
        let token = self.store.get()?.ok_or(AuthError::NoStoredToken)?;
        Ok(self.connection.logon(account_name, &token).await?)
    }

    /// Deletes the stored refresh token.
    ///
    /// # Errors
    ///
    /// [`Error::Keychain`] when the store rejects the delete.
    pub fn logout(&self) -> Result<(), Error> {
        self.store.delete()
    }
}
