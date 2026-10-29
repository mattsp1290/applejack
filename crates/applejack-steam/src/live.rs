//! The [`SteamConnection`] that talks to Steam through `steamroom` 0.3.0 and
//! `steamroom-client` 0.3.0. Compiled always, run only by the G2 spike and by
//! live tests; nothing here executes under `cargo test` without
//! `APPLEJACK_LIVE_STEAM=1`.
//!
//! Every call below is named in `docs/steam-layer.md` under "API grounding"
//! with the registry file and line it was confirmed against.

use std::time::{Duration, Instant};

use prost::Message;
use steamroom::client::msg::ClientMsg;
use steamroom::client::multi::unpack_multi;
use steamroom::client::{LoggedIn, PROTOCOL_VERSION, SteamClient};
use steamroom::connection::{CmServer, Protocol};
use steamroom::enums::EResultError;
use steamroom::error::ConnectionError;
use steamroom::generated::{
    CMsgClientLicenseList, CMsgClientLogon, CMsgClientLogonResponse, CMsgIpAddress,
    c_msg_ip_address,
};
use steamroom::messages::EMsg;
use steamroom::messages::header::PacketHeader;
use steamroom::transport::tcp::TcpTransport;
use steamroom::types::SteamId;
use steamroom_client::login::{
    ApprovedAuth, CredentialsLoginFlow, GuardChallenge, GuardType, LoginBuilder, LoginError,
    MobileChallenge, QrLoginFlow,
};

use crate::auth::{
    AccessToken, AuthError, Credentials, GuardFactor, License, SteamConnection, Tokens,
};
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

/// How long `license_list` waits for `CLIENT_LICENSE_LIST` after logon.
const LICENSE_LIST_TIMEOUT: Duration = Duration::from_secs(30);

/// The CM does not report a retry-after; this is a conservative floor.
const RATE_LIMIT_RETRY_AFTER: Duration = Duration::from_secs(60);

enum Stage {
    Idle,
    Qr(QrLoginFlow),
    GuardCode(GuardChallenge),
    MobileConfirm(MobileChallenge),
    Approved(ApprovedAuth),
    LoggedIn(Session),
}

/// A logged-on client plus what the logon response said about keeping it.
struct Session {
    client: SteamClient<LoggedIn>,
    /// `CMsgClientLogonResponse.heartbeat_seconds`: how often the CM expects
    /// `SteamClient::send_heartbeat`. `None` when the CM did not say.
    heartbeat: Option<Duration>,
}

/// One Steam connection at a time, as decision 5 in the plan requires.
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

    /// The heartbeat interval the CM asked for in the logon response, once
    /// `logon` has succeeded. The unit that keeps a session open drives
    /// `SteamClient<LoggedIn>::send_heartbeat` at this interval.
    #[must_use]
    pub fn heartbeat_interval(&self) -> Option<Duration> {
        match &self.stage {
            Stage::LoggedIn(session) => session.heartbeat,
            _ => None,
        }
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
            Stage::LoggedIn(_) => "LoggedIn",
        };
        f.debug_struct("SteamroomConnection")
            .field("stage", &stage)
            .finish()
    }
}

impl SteamConnection for SteamroomConnection {
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
        let Stage::GuardCode(challenge) = self.take_stage() else {
            return Err(AuthError::OutOfOrder("no Steam Guard code is pending"));
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
            Err((_, error)) => Err(map_login_error(error)),
        }
    }

    async fn wait_for_tokens(&mut self) -> Result<Tokens, AuthError> {
        let approved = match self.take_stage() {
            Stage::Qr(flow) => flow.wait_for_scan().await.map_err(map_login_error)?,
            Stage::MobileConfirm(challenge) => challenge
                .wait_for_confirmation()
                .await
                .map_err(map_login_error)?,
            Stage::Approved(approved) => approved,
            Stage::GuardCode(challenge) => {
                let factor = guard_factor(challenge.allowed_kinds());
                self.stage = Stage::GuardCode(challenge);
                return Err(AuthError::GuardRequired { factor });
            }
            Stage::Idle | Stage::LoggedIn(_) => {
                return Err(AuthError::OutOfOrder("no auth session is pending"));
            }
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
    ) -> Result<(), AuthError> {
        self.stage = Stage::Idle;
        let servers = CmServer::fetch()
            .await
            .unwrap_or_else(|_| CmServer::defaults());
        let server = servers
            .iter()
            .find(|server| server.protocol == Protocol::Tcp)
            .ok_or_else(|| AuthError::Connection("no TCP CM server available".to_owned()))?;
        let transport = TcpTransport::connect(server)
            .await
            .map_err(map_steamroom_error)?;
        let (client, _rx) = SteamClient::connect(transport)
            .await
            .map_err(map_steamroom_error)?;
        let client = client
            .encrypt()
            .await
            .map_err(map_steamroom_error)?
            .prepare()
            .await
            .map_err(map_steamroom_error)?;

        let logon = CMsgClientLogon {
            protocol_version: Some(PROTOCOL_VERSION),
            cell_id: Some(0),
            client_os_type: Some(CLIENT_OS_WINDOWS_11),
            account_name: Some(account_name.to_owned()),
            // Steam's wire name for the refresh-token slot. This struct now
            // holds the secret: never log it or format it with `Debug`.
            access_token: Some(refresh_token.expose().to_owned()),
            obfuscated_private_ip: Some(CMsgIpAddress {
                ip: Some(c_msg_ip_address::Ip::V4(LOGIN_ID)),
            }),
            supports_rate_limit_response: Some(true),
            ..Default::default()
        };
        let body = logon.encode_to_vec();
        let mut msg = ClientMsg::with_body(EMsg::CLIENT_LOGON, &body);
        msg.header.steamid = Some(SteamId::from_parts(1, 1, 1, 0).raw());
        msg.header.client_sessionid = Some(0);
        // `login` has already checked the response's eresult; its body is
        // the `CMsgClientLogonResponse` whether it arrived alone or in a MULTI.
        let (client, response) = client.login(msg).await.map_err(map_steamroom_error)?;
        let response = CMsgClientLogonResponse::decode(&*response.body)
            .map_err(|error| AuthError::Connection(format!("CMsgClientLogonResponse: {error}")))?;
        let heartbeat = response
            .heartbeat_seconds
            .and_then(|seconds| u64::try_from(seconds).ok())
            .filter(|seconds| *seconds > 0)
            .map(Duration::from_secs);
        self.stage = Stage::LoggedIn(Session { client, heartbeat });
        Ok(())
    }

    async fn license_list(&mut self) -> Result<Vec<License>, AuthError> {
        let Stage::LoggedIn(Session { client, .. }) = &self.stage else {
            return Err(AuthError::OutOfOrder(
                "license_list needs a logged-in session",
            ));
        };
        let deadline = Instant::now() + LICENSE_LIST_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(AuthError::Connection(
                    "timed out waiting for CLIENT_LICENSE_LIST".to_owned(),
                ));
            }
            let incoming = tokio::time::timeout(remaining, client.recv_msg())
                .await
                .map_err(|_| {
                    AuthError::Connection("timed out waiting for CLIENT_LICENSE_LIST".to_owned())
                })?
                .map_err(map_steamroom_error)?;
            if incoming.emsg == EMsg::CLIENT_LICENSE_LIST {
                return decode_license_list(&incoming.body);
            }
            if incoming.emsg == EMsg::MULTI {
                for sub in unpack_multi(&incoming.body).map_err(map_steamroom_error)? {
                    if let PacketHeader::Protobuf { header, body } = PacketHeader::parse(&sub)
                        .map_err(|error| map_steamroom_error(error.into()))?
                        && header.emsg == EMsg::CLIENT_LICENSE_LIST
                    {
                        return decode_license_list(&body);
                    }
                }
            }
        }
    }
}

fn decode_license_list(body: &[u8]) -> Result<Vec<License>, AuthError> {
    let list = CMsgClientLicenseList::decode(body)
        .map_err(|error| AuthError::Connection(format!("CMsgClientLicenseList: {error}")))?;
    Ok(list
        .licenses
        .iter()
        .map(|license| License {
            package_id: license.package_id.unwrap_or(0),
            license_type: license.license_type.unwrap_or(0),
            flags: license.flags.unwrap_or(0),
            owner_id: license.owner_id.unwrap_or(0),
            access_token: license.access_token.unwrap_or(0),
        })
        .collect())
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
