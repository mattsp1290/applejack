//! The [`SteamConnection`] and [`SteamroomSession`] that talk to Steam through
//! `steamroom` 0.3.0 and `steamroom-client` 0.3.0. Compiled always; only the
//! spikes and live tests open a socket. `cargo test` drives
//! [`SteamroomSession`] over an in-memory transport.
//!
//! Every call below is named in `docs/steam-layer.md` under "API grounding"
//! with the registry file and line it was confirmed against.

mod cdn;
mod session;

use std::time::Duration;

use steamroom::client::{Ready, SteamClient};
use steamroom::connection::{CmServer, Protocol};
use steamroom::enums::EResultError;
use steamroom::error::ConnectionError;
use steamroom::transport::tcp::TcpTransport;
use steamroom_client::login::{
    ApprovedAuth, CredentialsLoginFlow, GuardChallenge, GuardType, LoginBuilder, LoginError,
    MobileChallenge, QrLoginFlow,
};
use tokio::time::timeout;

pub use cdn::LiveChunks;
pub use session::SteamroomSession;

use crate::auth::{AccessToken, AuthError, Credentials, GuardFactor, SteamConnection, Tokens};
use crate::keychain::RefreshToken;

/// Pinned crate versions, reported by the spike. Mirror `[workspace.dependencies]`.
pub const STEAMROOM_VERSION: &str = "0.3.0";
pub const STEAMROOM_CLIENT_VERSION: &str = "0.3.0";

/// `device_friendly_name` in auth sessions.
pub const DEVICE_NAME: &str = "applejack";

/// `CMsgClientLogon.obfuscated_private_ip.v4`, which Steam uses as the logon
/// id: two sessions of one account with different ids coexist, so this fixed
/// value never displaces the Windows client's session (it sends its obfuscated
/// LAN address). ASCII "APJK".
pub const LOGIN_ID: u32 = 0x4150_4A4B;

/// `EOSType` Windows 11, matching `steamroom_client::login::ClientOs::WINDOWS_11`;
/// the session asks for Windows depots.
const CLIENT_OS_WINDOWS_11: u32 = 20;

/// The CM does not report a retry-after; this is a conservative floor.
const RATE_LIMIT_RETRY_AFTER: Duration = Duration::from_secs(60);

/// `TcpTransport::connect` and the encryption handshake set no bound.
const CM_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// CM servers to try before giving up.
const CM_ATTEMPTS: usize = 3;

enum Stage {
    Idle,
    Qr(QrLoginFlow),
    GuardCode(GuardChallenge),
    MobileConfirm(MobileChallenge),
    Approved(ApprovedAuth),
}

/// The auth flow, one connection at a time. `logon` opens the session on its
/// own socket, as decision 5 in the plan requires.
pub struct SteamroomConnection {
    stage: Stage,
}

impl Default for SteamroomConnection {
    fn default() -> Self {
        Self::new()
    }
}

impl SteamroomConnection {
    #[must_use]
    pub fn new() -> Self {
        Self { stage: Stage::Idle }
    }

    fn take_stage(&mut self) -> Stage {
        std::mem::replace(&mut self.stage, Stage::Idle)
    }
}

impl std::fmt::Debug for SteamroomConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let stage = match self.stage {
            Stage::Idle => "Idle",
            Stage::Qr(_) => "Qr",
            Stage::GuardCode(_) => "GuardCode",
            Stage::MobileConfirm(_) => "MobileConfirm",
            Stage::Approved(_) => "Approved",
        };
        f.debug_struct("SteamroomConnection")
            .field("stage", &stage)
            .finish()
    }
}

impl SteamConnection for SteamroomConnection {
    type Session = SteamroomSession;

    async fn begin_qr(&mut self) -> Result<String, AuthError> {
        let flow = LoginBuilder::new()
            .device_name(DEVICE_NAME)
            .with_qr()
            .begin()
            .await
            .map_err(map_login_error)?;
        let payload = flow.challenge_url().to_owned();
        self.stage = Stage::Qr(flow);
        Ok(payload)
    }

    async fn begin_credentials(
        &mut self,
        credentials: &Credentials,
    ) -> Result<Option<GuardFactor>, AuthError> {
        let flow = LoginBuilder::new()
            .device_name(DEVICE_NAME)
            .with_credentials(
                credentials.account_name.clone(),
                credentials.password.expose().to_owned(),
            )
            .begin()
            .await
            .map_err(map_login_error)?;
        match flow {
            CredentialsLoginFlow::Approved(approved) => {
                self.stage = Stage::Approved(approved);
                Ok(None)
            }
            CredentialsLoginFlow::NeedsGuardCode(challenge) => {
                let factor = guard_factor(challenge.allowed_kinds());
                self.stage = Stage::GuardCode(challenge);
                Ok(Some(factor))
            }
            CredentialsLoginFlow::NeedsMobileConfirm(challenge) => {
                self.stage = Stage::MobileConfirm(challenge);
                Ok(Some(GuardFactor::DeviceConfirmation))
            }
            _ => Err(AuthError::Connection(
                "steamroom-client returned an unknown login flow".to_owned(),
            )),
        }
    }

    async fn submit_guard_code(&mut self, code: &str) -> Result<(), AuthError> {
        // Check before taking: a wrong-stage call must leave the stage
        // untouched, as `OutOfOrder` promises.
        if !matches!(self.stage, Stage::GuardCode(_)) {
            return Err(AuthError::OutOfOrder("no Steam Guard code is pending"));
        }
        let Stage::GuardCode(challenge) = self.take_stage() else {
            unreachable!("stage checked above")
        };
        let kind = if challenge.allowed_kinds().contains(&GuardType::DeviceCode) {
            GuardType::DeviceCode
        } else {
            GuardType::EmailCode
        };
        match challenge.submit_code(code, kind).await {
            Ok(approved) => {
                self.stage = Stage::Approved(approved);
                Ok(())
            }
            Err((challenge, LoginError::InvalidGuardCode)) => {
                self.stage = Stage::GuardCode(challenge);
                Err(AuthError::InvalidGuardCode)
            }
            // The challenge is gone with the flow. A code mismatch reported
            // from the token poll rather than the submit itself would read
            // as "try another code", which nothing can accept any more.
            Err((_, error)) => Err(match map_login_error(error) {
                AuthError::InvalidGuardCode => AuthError::OutOfOrder(
                    "the Steam Guard challenge was consumed; begin the login again",
                ),
                other => other,
            }),
        }
    }

    async fn wait_for_tokens(&mut self) -> Result<Tokens, AuthError> {
        match &self.stage {
            Stage::GuardCode(challenge) => {
                return Err(AuthError::GuardRequired {
                    factor: guard_factor(challenge.allowed_kinds()),
                });
            }
            Stage::Idle => {
                return Err(AuthError::OutOfOrder("no auth session is pending"));
            }
            Stage::Qr(_) | Stage::MobileConfirm(_) | Stage::Approved(_) => {}
        }
        let approved = match self.take_stage() {
            Stage::Qr(flow) => flow.wait_for_scan().await.map_err(map_login_error)?,
            Stage::MobileConfirm(challenge) => challenge
                .wait_for_confirmation()
                .await
                .map_err(map_login_error)?,
            Stage::Approved(approved) => approved,
            Stage::Idle | Stage::GuardCode(_) => unreachable!("stage checked above"),
        };
        let tokens = approved.tokens();
        let account_name = tokens
            .account_name
            .clone()
            .ok_or(AuthError::MissingField("account_name"))?;
        // The auth connection is dropped here; `logon` opens the session with
        // the refresh token and the fixed logon id.
        Ok(Tokens {
            account_name,
            refresh_token: RefreshToken::new(tokens.refresh_token.clone()),
            access_token: AccessToken::new(tokens.access_token.clone()),
        })
    }

    async fn logon(
        &mut self,
        account_name: &str,
        refresh_token: &RefreshToken,
    ) -> Result<SteamroomSession, AuthError> {
        self.stage = Stage::Idle;
        let servers = CmServer::fetch()
            .await
            .unwrap_or_else(|_| CmServer::defaults());
        let tcp = servers
            .iter()
            .filter(|server| server.protocol == Protocol::Tcp);
        let mut failure = AuthError::Connection("no TCP CM server available".to_owned());
        for server in tcp.take(CM_ATTEMPTS) {
            match timeout(CM_CONNECT_TIMEOUT, connect_ready(server)).await {
                Ok(Ok(client)) => {
                    return SteamroomSession::start(client, account_name, refresh_token).await;
                }
                Ok(Err(error)) => failure = error,
                Err(_elapsed) => {
                    failure = AuthError::Connection("timed out connecting to a CM".to_owned());
                }
            }
        }
        Err(failure)
    }
}

async fn connect_ready(server: &CmServer) -> Result<SteamClient<Ready>, AuthError> {
    let transport = TcpTransport::connect(server)
        .await
        .map_err(map_steamroom_error)?;
    let (client, _rx) = SteamClient::connect(transport)
        .await
        .map_err(map_steamroom_error)?;
    client
        .encrypt()
        .await
        .map_err(map_steamroom_error)?
        .prepare()
        .await
        .map_err(map_steamroom_error)
}

fn guard_factor(kinds: &[GuardType]) -> GuardFactor {
    if kinds.contains(&GuardType::DeviceCode) {
        GuardFactor::DeviceCode
    } else if kinds.contains(&GuardType::EmailCode) {
        GuardFactor::EmailCode
    } else {
        GuardFactor::DeviceConfirmation
    }
}

fn map_login_error(error: LoginError) -> AuthError {
    match error {
        LoginError::LogonFailed(EResultError::DuplicateRequest)
        | LoginError::Transport(steamroom::Error::Connection(
            ConnectionError::ServiceMethodFailed(EResultError::DuplicateRequest),
        )) => AuthError::DuplicateGuardApproval,
        LoginError::InvalidPassword => AuthError::Refused,
        LoginError::InvalidGuardCode => AuthError::InvalidGuardCode,
        LoginError::LogonFailed(result) => map_eresult(result),
        LoginError::MissingField(field) => AuthError::MissingField(field),
        LoginError::Transport(error) => map_steamroom_error(error),
        other => AuthError::Connection(other.to_string()),
    }
}

fn map_steamroom_error(error: steamroom::Error) -> AuthError {
    match error {
        steamroom::Error::Connection(
            ConnectionError::LogonFailed(result) | ConnectionError::ServiceMethodFailed(result),
        ) => map_eresult(result),
        other => AuthError::Connection(other.to_string()),
    }
}

fn map_eresult(result: EResultError) -> AuthError {
    match result {
        EResultError::RateLimitExceeded | EResultError::LoginDeniedThrottle => {
            AuthError::RateLimited {
                retry_after: RATE_LIMIT_RETRY_AFTER,
            }
        }
        EResultError::TwoFactorRequired => AuthError::GuardRequired {
            factor: GuardFactor::DeviceCode,
        },
        EResultError::TwoFactorCodeMismatch => AuthError::InvalidGuardCode,
        EResultError::InvalidPassword
        | EResultError::AccessDenied
        | EResultError::Expired
        | EResultError::Revoked
        | EResultError::Banned => AuthError::Refused,
        other => AuthError::Connection(format!("Steam EResult {other}")),
    }
}

/// [`map_eresult`] for a call on a logged-in session. The login stands, so an
/// answer that means "refused" there means "denied this call" here.
fn map_call_result(call: &str, result: EResultError) -> AuthError {
    match map_eresult(result) {
        AuthError::Refused => AuthError::CallDenied {
            call: call.to_owned(),
            result: result.to_string(),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_request_tells_the_user_to_use_one_factor() {
        let error = map_login_error(LoginError::Transport(steamroom::Error::Connection(
            ConnectionError::ServiceMethodFailed(EResultError::DuplicateRequest),
        )));
        assert!(matches!(error, AuthError::DuplicateGuardApproval));
        assert!(error.to_string().contains("not both"));
        assert!(matches!(
            map_eresult(EResultError::DuplicateRequest),
            AuthError::Connection(_)
        ));
    }

    #[test]
    fn a_throttled_service_call_is_a_typed_rate_limit() {
        let error = map_eresult(EResultError::RateLimitExceeded);
        assert!(matches!(error, AuthError::RateLimited { .. }));
    }
}
