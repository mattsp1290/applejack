//! The [`SteamConnection`] and [`SteamroomSession`] that talk to Steam through
//! `steamroom` 0.3.0 and `steamroom-client` 0.3.0. Compiled always; only the
//! CLI, the core, and live tests open a socket. `cargo test` drives
//! [`SteamroomSession`] and the auth flow over an in-memory transport.
//!
//! Every call below is named in `docs/steam-layer.md` under "API grounding"
//! with the registry file and line it was confirmed against.

mod cdn;
mod session;

use std::time::Duration;

use base64::Engine as _;
use prost::Message as _;
use steamroom::auth::GuardType;
use steamroom::client::{Ready, SteamClient};
use steamroom::connection::{CmServer, Protocol};
use steamroom::crypto::rsa::encrypt_with_rsa_public_key;
use steamroom::enums::EResultError;
use steamroom::error::ConnectionError;
use steamroom::generated::{
    CAuthenticationBeginAuthSessionViaCredentialsRequest,
    CAuthenticationBeginAuthSessionViaQrRequest, CAuthenticationPollAuthSessionStatusRequest,
    CAuthenticationPollAuthSessionStatusResponse,
};
use steamroom::transport::tcp::TcpTransport;
use tokio::time::timeout;

pub use cdn::LiveChunks;
pub use session::SteamroomSession;

use crate::auth::{AuthError, AuthPoll, Credentials, GuardFactor, SteamConnection, Tokens};
use crate::keychain::RefreshToken;

/// Pinned crate versions. Mirror `[workspace.dependencies]`.
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

/// `EResult::TryAnotherCM`, which `steamroom` has no variant for.
const TRY_ANOTHER_CM: i32 = 48;

/// `EAuthTokenPlatformType::SteamClient`: other platforms get tokens a CM
/// refuses at logon.
const PLATFORM_STEAM_CLIENT: i32 = 1;

/// Seconds between polls when Steam does not say.
const DEFAULT_POLL_SECONDS: f32 = 5.0;

/// `steamroom`'s calls wait for their reply without a bound.
const AUTH_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// `EResult::InvalidLoginAuthCode` and `ExpiredLoginAuthCode`, a wrong or
/// stale email code, which `steamroom` has no variants for.
const WRONG_EMAIL_CODE: [i32; 2] = [65, 71];

/// One auth session on its own connection. `steamroom-client`'s
/// `GuardChallenge` keeps its poll loop private and treats `DuplicateRequest`
/// as fatal, so this holds what it holds and makes the same `steamroom`
/// calls: a poll can then run while a code is awaited, and a code that loses
/// the race with an approval is not a failure.
struct AuthFlow {
    client: SteamClient<Ready>,
    client_id: u64,
    steam_id: u64,
    request_id: Vec<u8>,
    interval: Duration,
    kinds: Vec<GuardType>,
    code_pending: bool,
}

impl AuthFlow {
    async fn poll(&mut self) -> Result<AuthPoll, AuthError> {
        let request = CAuthenticationPollAuthSessionStatusRequest {
            client_id: Some(self.client_id),
            request_id: Some(self.request_id.clone()),
            ..Default::default()
        };
        let body = request.encode_to_vec();
        let call = self
            .client
            .call_service_method_non_authed("Authentication.PollAuthSessionStatus#1", &body);
        let response = bounded(call).await?;
        let answer: CAuthenticationPollAuthSessionStatusResponse = response
            .decode()
            .map_err(|error| AuthError::Connection(error.to_string()))?;
        if let Some(client_id) = answer.new_client_id.filter(|id| *id != 0) {
            self.client_id = client_id;
        }
        if let Some(refresh) = answer.refresh_token.as_ref().filter(|t| !t.is_empty()) {
            let account_name = answer
                .account_name
                .clone()
                .ok_or(AuthError::MissingField("account_name"))?;
            // The auth connection is dropped with the flow; `logon` opens the
            // session with the refresh token and the fixed logon id.
            return Ok(AuthPoll::Approved(Tokens {
                account_name,
                refresh_token: RefreshToken::new(refresh.clone()),
            }));
        }
        Ok(AuthPoll::Pending {
            challenge: answer.new_challenge_url.filter(|url| !url.is_empty()),
        })
    }
}

async fn bounded<T>(
    call: impl Future<Output = Result<T, steamroom::Error>>,
) -> Result<T, AuthError> {
    match timeout(AUTH_CALL_TIMEOUT, call).await {
        Ok(result) => result.map_err(map_steamroom_error),
        Err(_elapsed) => Err(AuthError::Connection(
            "Steam did not answer an auth call".to_owned(),
        )),
    }
}

/// The auth flow, one connection at a time. `logon` opens the session on its
/// own socket, as decision 5 in the plan requires.
#[derive(Default)]
pub struct SteamroomConnection {
    flow: Option<AuthFlow>,
    /// Takes the place of connecting; see [`SteamroomConnection::over`].
    prepared: Option<SteamClient<Ready>>,
}

impl SteamroomConnection {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An auth flow over a client that is already prepared, the way
    /// [`SteamroomSession::start`] takes one, so `cargo test` can drive it
    /// over an in-memory transport.
    #[must_use]
    pub fn over(client: SteamClient<Ready>) -> Self {
        Self {
            flow: None,
            prepared: Some(client),
        }
    }

    async fn auth_client(&mut self) -> Result<SteamClient<Ready>, AuthError> {
        if let Some(client) = self.prepared.take() {
            return Ok(client);
        }
        let servers = CmServer::fetch()
            .await
            .unwrap_or_else(|_| CmServer::defaults());
        let tcp = servers
            .iter()
            .filter(|server| server.protocol == Protocol::Tcp);
        first_cm(tcp, |server| async move { reach_cm(server).await }).await
    }
}

impl std::fmt::Debug for SteamroomConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SteamroomConnection")
            .field("auth_session", &self.flow.is_some())
            .finish_non_exhaustive()
    }
}

impl SteamConnection for SteamroomConnection {
    type Session = SteamroomSession;

    async fn begin_qr(&mut self) -> Result<String, AuthError> {
        let client = self.auth_client().await?;
        let request = CAuthenticationBeginAuthSessionViaQrRequest {
            device_friendly_name: Some(DEVICE_NAME.to_owned()),
            platform_type: Some(PLATFORM_STEAM_CLIENT),
            ..Default::default()
        };
        let session = bounded(client.begin_auth_session_via_qr(request)).await?;
        let payload = session
            .challenge_url
            .ok_or(AuthError::MissingField("challenge_url"))?;
        self.flow = Some(AuthFlow {
            client,
            client_id: session
                .client_id
                .ok_or(AuthError::MissingField("client_id"))?,
            steam_id: 0,
            request_id: session
                .request_id
                .ok_or(AuthError::MissingField("request_id"))?,
            interval: poll_interval(session.poll_interval),
            kinds: session.allowed_confirmations,
            code_pending: false,
        });
        Ok(payload)
    }

    async fn begin_credentials(
        &mut self,
        credentials: &Credentials,
    ) -> Result<Option<GuardFactor>, AuthError> {
        let client = self.auth_client().await?;
        let account_name = credentials.account_name.clone();
        let key = bounded(client.get_password_rsa_public_key(&account_name)).await?;
        let modulus = key
            .publickey_mod
            .ok_or(AuthError::MissingField("publickey_mod"))?;
        let exponent = key
            .publickey_exp
            .ok_or(AuthError::MissingField("publickey_exp"))?;
        let sealed = encrypt_with_rsa_public_key(
            credentials.password.expose().as_bytes(),
            &modulus,
            &exponent,
        )
        .map_err(|error| AuthError::Connection(error.to_string()))?;
        let request = CAuthenticationBeginAuthSessionViaCredentialsRequest {
            account_name: Some(account_name),
            encrypted_password: Some(base64::engine::general_purpose::STANDARD.encode(sealed)),
            encryption_timestamp: Some(key.timestamp.unwrap_or(0)),
            remember_login: Some(true),
            persistence: Some(1),
            platform_type: Some(PLATFORM_STEAM_CLIENT),
            device_friendly_name: Some(DEVICE_NAME.to_owned()),
            ..Default::default()
        };
        let session = bounded(client.begin_auth_session_via_credentials(request)).await?;
        let kinds = session.allowed_confirmations;
        let no_factor = kinds.is_empty() || kinds.contains(&GuardType::None);
        let code_pending = !no_factor
            && kinds
                .iter()
                .any(|kind| matches!(kind, GuardType::DeviceCode | GuardType::EmailCode));
        let factor = (!no_factor).then(|| guard_factor(&kinds));
        self.flow = Some(AuthFlow {
            client,
            client_id: session
                .client_id
                .ok_or(AuthError::MissingField("client_id"))?,
            steam_id: session.steam_id.ok_or(AuthError::MissingField("steamid"))?,
            request_id: session
                .request_id
                .ok_or(AuthError::MissingField("request_id"))?,
            interval: poll_interval(session.poll_interval),
            kinds,
            code_pending,
        });
        Ok(factor)
    }

    async fn submit_guard_code(&mut self, code: &str) -> Result<(), AuthError> {
        let Some(flow) = self.flow.as_mut().filter(|flow| flow.code_pending) else {
            return Err(AuthError::OutOfOrder("no Steam Guard code is pending"));
        };
        let kind = if flow.kinds.contains(&GuardType::DeviceCode) {
            GuardType::DeviceCode
        } else {
            GuardType::EmailCode
        };
        let call = flow
            .client
            .submit_steam_guard_code(flow.client_id, flow.steam_id, code, kind);
        match timeout(AUTH_CALL_TIMEOUT, call).await {
            Ok(Ok(())) => {
                flow.code_pending = false;
                Ok(())
            }
            Ok(Err(error)) => match service_result(&error) {
                // SteamKit reads this as success too: the session was
                // approved in the mobile app, and the next poll has tokens.
                Some(EResultError::DuplicateRequest) => {
                    flow.code_pending = false;
                    Ok(())
                }
                Some(result) if is_wrong_code(result) => Err(AuthError::InvalidGuardCode),
                _ => {
                    self.flow = None;
                    Err(map_steamroom_error(error))
                }
            },
            Err(_elapsed) => {
                self.flow = None;
                Err(AuthError::Connection(
                    "Steam did not answer an auth call".to_owned(),
                ))
            }
        }
    }

    async fn poll_tokens(&mut self) -> Result<AuthPoll, AuthError> {
        let Some(flow) = self.flow.as_mut() else {
            return Err(AuthError::OutOfOrder("no auth session is pending"));
        };
        match flow.poll().await {
            Ok(AuthPoll::Pending { challenge }) => Ok(AuthPoll::Pending { challenge }),
            done => {
                self.flow = None;
                // The session is gone with the flow, so "try another code"
                // would be answered by nothing.
                done.map_err(|error| match error {
                    AuthError::InvalidGuardCode => AuthError::OutOfOrder(
                        "the Steam Guard challenge was consumed; begin the login again",
                    ),
                    other => other,
                })
            }
        }
    }

    fn poll_interval(&self) -> Duration {
        self.flow.as_ref().map_or_else(
            || Duration::from_secs_f32(DEFAULT_POLL_SECONDS),
            |flow| flow.interval,
        )
    }

    async fn logon(
        &mut self,
        account_name: &str,
        refresh_token: &RefreshToken,
    ) -> Result<SteamroomSession, AuthError> {
        self.flow = None;
        let servers = CmServer::fetch()
            .await
            .unwrap_or_else(|_| CmServer::defaults());
        let tcp = servers
            .iter()
            .filter(|server| server.protocol == Protocol::Tcp);
        first_cm(tcp, |server| async move {
            let client = reach_cm(server).await?;
            SteamroomSession::start(client, account_name, refresh_token).await
        })
        .await
    }
}

fn poll_interval(seconds: Option<f32>) -> Duration {
    let seconds = seconds
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
        .unwrap_or(DEFAULT_POLL_SECONDS);
    Duration::from_secs_f32(seconds)
}

/// A prepared client on this CM, or [`AuthError::CmUnavailable`].
async fn reach_cm(server: &CmServer) -> Result<SteamClient<Ready>, AuthError> {
    match timeout(CM_CONNECT_TIMEOUT, connect_ready(server)).await {
        Ok(Ok(client)) => Ok(client),
        Ok(Err(error)) => Err(AuthError::CmUnavailable(error.to_string())),
        Err(_elapsed) => Err(AuthError::CmUnavailable(
            "connection: timed out connecting to a CM".to_owned(),
        )),
    }
}

/// Runs `attempt` on one server after another, at most [`CM_ATTEMPTS`]. Only
/// [`AuthError::CmUnavailable`] moves on: any other answer holds on every CM,
/// and repeating a refused logon or a rate limit would make it worse.
async fn first_cm<S, T, F: Future<Output = Result<T, AuthError>>>(
    servers: impl Iterator<Item = S>,
    mut attempt: impl FnMut(S) -> F,
) -> Result<T, AuthError> {
    let mut failure = AuthError::Connection("no TCP CM server available".to_owned());
    for server in servers.take(CM_ATTEMPTS) {
        match attempt(server).await {
            Err(error @ AuthError::CmUnavailable(_)) => failure = error,
            other => return other,
        }
    }
    Err(failure)
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

fn service_result(error: &steamroom::Error) -> Option<EResultError> {
    match error {
        steamroom::Error::Connection(ConnectionError::ServiceMethodFailed(result)) => Some(*result),
        _ => None,
    }
}

fn is_wrong_code(result: EResultError) -> bool {
    match result {
        EResultError::TwoFactorCodeMismatch => true,
        EResultError::Unknown(code) => WRONG_EMAIL_CODE.contains(&code),
        _ => false,
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
        EResultError::Unknown(TRY_ANOTHER_CM) => {
            AuthError::CmUnavailable("this Steam CM asked the client to try another".to_owned())
        }
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

    /// Plays one answer per server and returns the result with the calls made.
    async fn run_first_cm(answers: Vec<Result<u32, AuthError>>) -> (Result<u32, AuthError>, usize) {
        let mut calls = 0;
        let result = first_cm(answers.into_iter(), |answer| {
            calls += 1;
            std::future::ready(answer)
        })
        .await;
        (result, calls)
    }

    fn unavailable() -> Result<u32, AuthError> {
        Err(AuthError::CmUnavailable("try another".to_owned()))
    }

    #[tokio::test]
    async fn a_cm_that_cannot_take_the_logon_moves_on_to_the_next() {
        let (result, calls) = run_first_cm(vec![unavailable(), unavailable(), Ok(7)]).await;
        assert!(matches!(result, Ok(7)));
        assert_eq!(calls, 3);
    }

    #[tokio::test]
    async fn failover_stops_at_three_cms_with_servers_left() {
        let answers = vec![unavailable(), unavailable(), unavailable(), Ok(7), Ok(8)];
        let (result, calls) = run_first_cm(answers).await;
        assert!(matches!(result, Err(AuthError::CmUnavailable(_))));
        assert_eq!(calls, 3);
    }

    #[tokio::test]
    async fn an_answer_about_the_account_ends_the_logon_on_the_first_cm() {
        let finals = [
            AuthError::Refused,
            AuthError::RateLimited {
                retry_after: RATE_LIMIT_RETRY_AFTER,
            },
            AuthError::GuardRequired {
                factor: GuardFactor::DeviceCode,
            },
            AuthError::InvalidGuardCode,
            AuthError::Connection("Steam logged the session off (EResult 5)".to_owned()),
        ];
        for error in finals {
            let expected = error.to_string();
            let (result, calls) = run_first_cm(vec![Err(error), Ok(7)]).await;
            assert_eq!(result.unwrap_err().to_string(), expected);
            assert_eq!(calls, 1, "{expected}");
        }
    }

    #[tokio::test]
    async fn no_server_at_all_is_a_connection_error() {
        let (result, calls) = run_first_cm(Vec::new()).await;
        assert!(matches!(result, Err(AuthError::Connection(_))));
        assert_eq!(calls, 0);
    }

    #[test]
    fn try_another_cm_is_typed_and_says_so() {
        let error = map_eresult(EResultError::from_code(TRY_ANOTHER_CM));
        assert!(matches!(error, AuthError::CmUnavailable(_)));
        assert!(error.to_string().contains("try another"));
    }

    #[test]
    fn wrong_email_and_device_codes_read_as_wrong_codes() {
        assert!(is_wrong_code(EResultError::TwoFactorCodeMismatch));
        assert!(is_wrong_code(EResultError::from_code(65)));
        assert!(is_wrong_code(EResultError::from_code(71)));
        assert!(!is_wrong_code(EResultError::DuplicateRequest));
    }

    #[test]
    fn a_throttled_service_call_is_a_typed_rate_limit() {
        let error = map_eresult(EResultError::RateLimitExceeded);
        assert!(matches!(error, AuthError::RateLimited { .. }));
    }
}
