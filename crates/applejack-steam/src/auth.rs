//! Login, refresh, and logout, driven over a [`SteamConnection`] so unit tests
//! run against a fake while [`crate::live::SteamroomConnection`] talks to
//! Steam.

use std::fmt;
use std::future::Future;
use std::time::Duration;

use crate::Error;
use crate::keychain::{RefreshToken, RefreshTokenStore};

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
    /// The refresh token is stored.
    Done { account_name: String },
}

/// Failures while talking to Steam. Never carries a token or password.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AuthError {
    /// Steam throttled the request; do not retry before `retry_after`.
    #[error("rate limited by Steam; retry after {retry_after:?}")]
    RateLimited { retry_after: Duration },
    /// A second factor is needed before the step can complete.
    #[error("Steam Guard {factor:?} required")]
    GuardRequired { factor: GuardFactor },
    /// Steam rejected the credentials or the token.
    #[error("Steam refused the login")]
    Refused,
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
}

/// A password typed by the user. `Debug` never reveals the value.
#[derive(Clone, PartialEq, Eq)]
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

/// A short-lived Steam access token, kept in memory only.
#[derive(Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// The calls that reach Steam. Each method advances one connection's state;
/// the order is `begin_qr` or `begin_credentials`, optionally
/// `submit_guard_code`, then `wait_for_tokens`; `logon` opens a session from a
/// refresh token and `license_list` reads from that session.
pub trait SteamConnection {
    /// Starts a QR session and returns the challenge payload to render.
    fn begin_qr(&mut self) -> impl Future<Output = Result<String, AuthError>> + Send;

    /// Starts a credentials session. `Some(factor)` means Steam Guard is
    /// needed before tokens are issued.
    fn begin_credentials(
        &mut self,
        credentials: &Credentials,
    ) -> impl Future<Output = Result<Option<GuardFactor>, AuthError>> + Send;

    /// Submits a Steam Guard code for the pending credentials session.
    fn submit_guard_code(
        &mut self,
        code: &str,
    ) -> impl Future<Output = Result<(), AuthError>> + Send;

    /// Polls until Steam issues tokens for the pending session.
    fn wait_for_tokens(&mut self) -> impl Future<Output = Result<Tokens, AuthError>> + Send;

    /// Opens a logged-in session from a refresh token.
    fn logon(
        &mut self,
        account_name: &str,
        refresh_token: &RefreshToken,
    ) -> impl Future<Output = Result<(), AuthError>> + Send;

    /// Reads `CMsgClientLicenseList` from the logged-in session. Call it
    /// once, directly after `logon` and before any PICS or heartbeat call:
    /// the live implementation drops every other message that arrives while
    /// it waits, and Steam sends the list unprompted after logon (and again
    /// whenever the account's licenses change), so a later call can eat
    /// another reply or wait out its timeout.
    fn license_list(&mut self) -> impl Future<Output = Result<Vec<License>, AuthError>> + Send;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Idle,
    AwaitingTokens,
    AwaitingGuardCode(GuardFactor),
}

/// Drives login, refresh, and logout over a connection and a token store.
#[derive(Debug)]
pub struct Authenticator<'a, C, S: ?Sized> {
    connection: &'a mut C,
    store: &'a S,
    pending: Pending,
}

impl<'a, C: SteamConnection, S: RefreshTokenStore + ?Sized> Authenticator<'a, C, S> {
    pub fn new(connection: &'a mut C, store: &'a S) -> Self {
        Self {
            connection,
            store,
            pending: Pending::Idle,
        }
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
                self.pending = Pending::AwaitingTokens;
                Ok(LoginStep::QrReady { payload })
            }
            AuthMode::Credentials => {
                let credentials = credentials.ok_or(AuthError::OutOfOrder(
                    "credentials mode needs an account name and password",
                ))?;
                match self.connection.begin_credentials(credentials).await? {
                    Some(GuardFactor::DeviceConfirmation) => {
                        self.pending = Pending::AwaitingTokens;
                        Ok(LoginStep::GuardRequired {
                            factor: GuardFactor::DeviceConfirmation,
                        })
                    }
                    Some(factor) => {
                        self.pending = Pending::AwaitingGuardCode(factor);
                        Ok(LoginStep::GuardRequired { factor })
                    }
                    None => {
                        self.pending = Pending::AwaitingTokens;
                        self.finish().await
                    }
                }
            }
        }
    }

    /// Submits a Steam Guard code and completes the login.
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
        let Pending::AwaitingGuardCode(_) = self.pending else {
            return Err(AuthError::OutOfOrder("no Steam Guard code is pending").into());
        };
        match self.connection.submit_guard_code(code).await {
            Ok(()) => {}
            // The challenge survives a wrong code; anything else ends the
            // session, and a driver that kept asking for codes would loop.
            Err(AuthError::InvalidGuardCode) => return Err(AuthError::InvalidGuardCode.into()),
            Err(error) => {
                self.pending = Pending::Idle;
                return Err(error.into());
            }
        }
        self.pending = Pending::AwaitingTokens;
        self.finish().await
    }

    /// Waits for Steam to issue tokens, stores the refresh token, and returns
    /// [`LoginStep::Done`].
    ///
    /// # Errors
    ///
    /// [`AuthError::GuardRequired`] when a code is still pending and
    /// [`AuthError::OutOfOrder`] when no login is in progress; both leave the
    /// login where it was. Any other error, from the connection or from
    /// storing the token, ends the login: the auth flow is consumed on
    /// failure, so the next call must be [`Authenticator::begin`].
    pub async fn finish(&mut self) -> Result<LoginStep, Error> {
        match self.pending {
            Pending::AwaitingTokens => {}
            Pending::AwaitingGuardCode(factor) => {
                return Err(AuthError::GuardRequired { factor }.into());
            }
            Pending::Idle => return Err(AuthError::OutOfOrder("no login in progress").into()),
        }
        // The flow is consumed whether `wait_for_tokens` succeeds or fails.
        self.pending = Pending::Idle;
        let tokens = self.connection.wait_for_tokens().await?;
        self.store.put(&tokens.refresh_token)?;
        Ok(LoginStep::Done {
            account_name: tokens.account_name,
        })
    }

    /// Opens a session from the stored refresh token. The account name is not
    /// secret; the host keeps it next to its other metadata.
    ///
    /// # Errors
    ///
    /// [`AuthError::NoStoredToken`] when the store is empty;
    /// [`AuthError::Refused`] when Steam rejects the token.
    pub async fn refresh(&mut self, account_name: &str) -> Result<(), Error> {
        let token = self.store.get()?.ok_or(AuthError::NoStoredToken)?;
        self.connection.logon(account_name, &token).await?;
        Ok(())
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
