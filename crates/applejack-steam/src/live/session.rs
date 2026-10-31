//! The logged-in session. One task owns the receive side of the connection
//! and routes every message; requests register a waiter and only send, so
//! nothing competes for `recv_msg`. A second task sends heartbeats whether or
//! not a request is waiting.
//!
//! The session stays in `SteamClient<Ready>` on purpose. `SteamClient::login`
//! discards the sibling messages of the `MULTI` that carries the logon
//! response, and every call on `SteamClient<LoggedIn>` runs its own receive
//! loop, so neither can coexist with a dispatcher. `Ready` exposes `send_msg`
//! and `recv_msg`, which is all this needs; the requests below are the wire
//! forms of the `LoggedIn` calls, confirmed against
//! `steamroom-0.3.0/src/client/mod.rs`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use prost::Message;
use steamroom::client::msg::ClientMsg;
use steamroom::client::multi::unpack_multi;
use steamroom::client::{HasEResult, IncomingMsg, PROTOCOL_VERSION, Ready, SteamClient};
use steamroom::depot::DepotKey;
use steamroom::depot::manifest::DepotManifest;
use steamroom::error::ConnectionError;
use steamroom::generated::{
    CContentServerDirectoryGetCdnAuthTokenRequest, CContentServerDirectoryGetCdnAuthTokenResponse,
    CContentServerDirectoryGetManifestRequestCodeRequest,
    CContentServerDirectoryGetManifestRequestCodeResponse,
    CContentServerDirectoryGetServersForSteamPipeRequest,
    CContentServerDirectoryGetServersForSteamPipeResponse, CMsgClientGetDepotDecryptionKey,
    CMsgClientGetDepotDecryptionKeyResponse, CMsgClientLicenseList, CMsgClientLoggedOff,
    CMsgClientLogon, CMsgClientLogonResponse, CMsgClientPicsAccessTokenRequest,
    CMsgClientPicsAccessTokenResponse, CMsgClientPicsProductInfoRequest,
    CMsgClientPicsProductInfoResponse, CMsgIpAddress, CMsgProtoBufHeader,
    c_msg_client_pics_product_info_request, c_msg_ip_address,
};
use steamroom::messages::EMsg;
use steamroom::messages::header::PacketHeader;
use steamroom::types::SteamId;
use steamroom::types::key_value::{KeyValue, KvValue, parse_binary_kv};
use tokio::sync::{OnceCell, mpsc, oneshot, watch};
use tokio::task::AbortHandle;
use tokio::time::{MissedTickBehavior, interval, timeout};

use super::cdn::{LiveChunks, ServerEntry, cdn_server, token_query};
use super::{CLIENT_OS_WINDOWS_11, LOGIN_ID, map_call_result, map_eresult, map_steamroom_error};
use crate::auth::{AuthError, License};
use crate::keychain::RefreshToken;
use crate::library::DEFAULT_BRANCH;
use crate::session::{AppInfo, PackageGrants, SteamSession};

const LOGON_TIMEOUT: Duration = Duration::from_secs(30);
const LICENSE_LIST_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// The interval G2 observed, for a CM that does not state one.
const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(9);

/// Apps or packages per PICS request.
const PICS_BATCH: usize = 200;

/// `MULTI` inside `MULTI`, as deep as a message may go.
const MAX_MULTI_DEPTH: usize = 4;

const SERVERS_METHOD: &str = "ContentServerDirectory.GetServersForSteamPipe#1";
const REQUEST_CODE_METHOD: &str = "ContentServerDirectory.GetManifestRequestCode#1";
const CDN_TOKEN_METHOD: &str = "ContentServerDirectory.GetCDNAuthToken#1";

/// Names the job a CM could not deliver. `steamroom` names no constant for it.
const DEST_JOB_FAILED: EMsg = EMsg(113);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Waiter {
    reply: EMsg,
    sender: mpsc::UnboundedSender<IncomingMsg>,
}

#[derive(Default, Clone)]
struct Published {
    licenses: Option<Result<Vec<License>, AuthError>>,
    closed: Option<AuthError>,
}

struct LogonReply {
    heartbeat: Option<Duration>,
}

pub(super) struct Inner {
    client: SteamClient<Ready>,
    steam_id: AtomicU64,
    session_id: AtomicI32,
    cell_id: AtomicU32,
    next_job: AtomicU64,
    waiters: Mutex<BTreeMap<u64, Waiter>>,
    logon: Mutex<Option<oneshot::Sender<Result<LogonReply, AuthError>>>>,
    published: watch::Sender<Published>,
    servers: OnceCell<Vec<ServerEntry>>,
}

impl Inner {
    fn close(&self, error: AuthError) {
        self.published.send_modify(|published| {
            published.closed.get_or_insert(error.clone());
        });
        // Dropping the senders wakes every waiter.
        lock(&self.waiters).clear();
        if let Some(sender) = lock(&self.logon).take() {
            let _ = sender.send(Err(error));
        }
    }

    fn closed(&self) -> Option<AuthError> {
        self.published.borrow().closed.clone()
    }

    fn header(&self) -> CMsgProtoBufHeader {
        CMsgProtoBufHeader {
            steamid: Some(self.steam_id.load(Ordering::Relaxed)),
            client_sessionid: Some(self.session_id.load(Ordering::Relaxed)),
            ..CMsgProtoBufHeader::default()
        }
    }

    /// Sends a request and returns the waiter its replies arrive on.
    async fn request(
        &self,
        what: &'static str,
        emsg: EMsg,
        method: Option<&str>,
        body: &[u8],
        reply: EMsg,
    ) -> Result<Exchange<'_>, AuthError> {
        if let Some(error) = self.closed() {
            return Err(error);
        }
        let job = self.next_job.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::unbounded_channel();
        lock(&self.waiters).insert(job, Waiter { reply, sender });
        let exchange = Exchange {
            inner: self,
            what,
            job,
            receiver,
        };
        // `close` publishes before it clears the waiters, so a close that
        // missed this waiter is visible here.
        if let Some(error) = self.closed() {
            return Err(error);
        }
        let mut msg = ClientMsg::with_body(emsg, body);
        msg.header = self.header();
        msg.header.jobid_source = Some(job);
        msg.header.target_job_name = method.map(str::to_owned);
        self.client
            .send_msg(&msg)
            .await
            .map_err(map_steamroom_error)?;
        Ok(exchange)
    }

    async fn call(&self, method: &'static str, body: &[u8]) -> Result<Vec<u8>, AuthError> {
        let mut exchange = self
            .request(
                method,
                EMsg::SERVICE_METHOD_CALL_FROM_CLIENT,
                Some(method),
                body,
                EMsg::SERVICE_METHOD_RESPONSE,
            )
            .await?;
        let reply = exchange.next().await?;
        if let Some(code) = reply.header.eresult {
            steamroom::enums::eresult(code).map_err(|result| map_call_result(method, result))?;
        }
        Ok(reply.body.to_vec())
    }

    async fn product_info(
        &self,
        request: CMsgClientPicsProductInfoRequest,
    ) -> Result<Vec<CMsgClientPicsProductInfoResponse>, AuthError> {
        let body = request.encode_to_vec();
        let mut exchange = self
            .request(
                "PICS product info",
                EMsg::CLIENT_PICS_PRODUCT_INFO_REQUEST,
                None,
                &body,
                EMsg::CLIENT_PICS_PRODUCT_INFO_RESPONSE,
            )
            .await?;
        let mut parts = Vec::new();
        loop {
            let reply = exchange.next().await?;
            let part = decode::<CMsgClientPicsProductInfoResponse>(&reply.body)?;
            let pending = part.response_pending.unwrap_or(false);
            parts.push(part);
            if !pending {
                return Ok(parts);
            }
        }
    }

    pub(super) async fn cdn_auth_token(
        &self,
        appid: u32,
        depot_id: u32,
        host: &str,
    ) -> Result<String, AuthError> {
        let body = CContentServerDirectoryGetCdnAuthTokenRequest {
            depot_id: Some(depot_id),
            host_name: Some(host.to_owned()),
            app_id: Some(appid),
        }
        .encode_to_vec();
        let reply = self.call(CDN_TOKEN_METHOD, &body).await?;
        decode::<CContentServerDirectoryGetCdnAuthTokenResponse>(&reply)?
            .token
            .filter(|token| !token.is_empty())
            .map(token_query)
            .ok_or(AuthError::MissingField("token"))
    }

    async fn servers(&self) -> Result<&[ServerEntry], AuthError> {
        let servers = self
            .servers
            .get_or_try_init(|| async {
                let body = CContentServerDirectoryGetServersForSteamPipeRequest {
                    cell_id: Some(self.cell_id.load(Ordering::Relaxed)),
                    ..Default::default()
                }
                .encode_to_vec();
                let reply = self.call(SERVERS_METHOD, &body).await?;
                let mut listed =
                    decode::<CContentServerDirectoryGetServersForSteamPipeResponse>(&reply)?
                        .servers;
                listed.sort_by(|a, b| {
                    a.weighted_load
                        .unwrap_or(f32::MAX)
                        .total_cmp(&b.weighted_load.unwrap_or(f32::MAX))
                });
                Ok::<_, AuthError>(
                    listed
                        .iter()
                        .filter(|server| !server.steam_china_only.unwrap_or(false))
                        .filter_map(cdn_server)
                        .collect(),
                )
            })
            .await?;
        Ok(servers)
    }
}

/// A registered waiter. Dropping it unregisters the job, so a reply that
/// arrives after a timeout is ignored.
struct Exchange<'a> {
    inner: &'a Inner,
    what: &'static str,
    job: u64,
    receiver: mpsc::UnboundedReceiver<IncomingMsg>,
}

impl Exchange<'_> {
    async fn next(&mut self) -> Result<IncomingMsg, AuthError> {
        match timeout(REQUEST_TIMEOUT, self.receiver.recv()).await {
            Ok(Some(reply)) => Ok(reply),
            // Only a close or a failed job drops the sender.
            Ok(None) => Err(self.inner.closed().unwrap_or_else(|| {
                AuthError::Connection(format!("the CM could not deliver {}", self.what))
            })),
            Err(_elapsed) => Err(AuthError::Connection(format!(
                "timed out waiting for {} from Steam",
                self.what
            ))),
        }
    }
}

impl Drop for Exchange<'_> {
    fn drop(&mut self) {
        lock(&self.inner.waiters).remove(&self.job);
    }
}

async fn receive_loop(inner: Arc<Inner>) {
    loop {
        let routed = match inner.client.recv_msg().await {
            Ok(incoming) => route(&inner, incoming, 0),
            Err(error) => Err(map_steamroom_error(error)),
        };
        if let Err(error) = routed {
            inner.close(error);
            return;
        }
    }
}

fn route(inner: &Inner, incoming: IncomingMsg, depth: usize) -> Result<(), AuthError> {
    match incoming.emsg {
        EMsg::MULTI if depth >= MAX_MULTI_DEPTH => Err(AuthError::Connection(
            "MULTI messages nested too deeply".to_owned(),
        )),
        EMsg::MULTI => {
            for packet in unpack_multi(&incoming.body).map_err(map_steamroom_error)? {
                route(inner, parse_packet(&packet)?, depth + 1)?;
            }
            Ok(())
        }
        EMsg::CLIENT_LOG_ON_RESPONSE => {
            on_logon(inner, &incoming);
            Ok(())
        }
        EMsg::CLIENT_LICENSE_LIST => {
            let licenses = decode_license_list(&incoming.body);
            inner.published.send_modify(|published| {
                published.licenses = Some(licenses);
            });
            Ok(())
        }
        EMsg::CLIENT_LOGGED_OFF => {
            let code = CMsgClientLoggedOff::decode(&*incoming.body)
                .map_or(2, |message| message.eresult.unwrap_or(2));
            Err(AuthError::Connection(format!(
                "Steam logged the session off (EResult {code})"
            )))
        }
        DEST_JOB_FAILED => {
            if let Some(job) = incoming.header.jobid_target {
                lock(&inner.waiters).remove(&job);
            }
            Ok(())
        }
        emsg => {
            let waiters = lock(&inner.waiters);
            // A reply without a job id goes to the oldest request expecting
            // that message, except a service reply, which always echoes its
            // job. One whose job has gone timed out and is dropped.
            let waiter = match incoming.header.jobid_target {
                Some(job) if job != u64::MAX => waiters.get(&job),
                _ if emsg == EMsg::SERVICE_METHOD_RESPONSE => None,
                _ => waiters.values().find(|waiter| waiter.reply == emsg),
            };
            if let Some(waiter) = waiter.filter(|waiter| waiter.reply == emsg) {
                let _ = waiter.sender.send(incoming);
            }
            Ok(())
        }
    }
}

fn on_logon(inner: &Inner, incoming: &IncomingMsg) {
    let reply = decode_logon(&incoming.body);
    if reply.is_ok() {
        if let Some(steam_id) = incoming.header.steamid {
            inner.steam_id.store(steam_id, Ordering::Relaxed);
        }
        if let Some(session_id) = incoming.header.client_sessionid {
            inner.session_id.store(session_id, Ordering::Relaxed);
        }
    }
    let reply = reply.map(|response| {
        if let Some(cell_id) = response.cell_id {
            inner.cell_id.store(cell_id, Ordering::Relaxed);
        }
        LogonReply {
            heartbeat: response
                .heartbeat_seconds
                .and_then(|seconds| u64::try_from(seconds).ok())
                .filter(|seconds| *seconds > 0)
                .map(Duration::from_secs),
        }
    });
    if let Some(sender) = lock(&inner.logon).take() {
        let _ = sender.send(reply);
    }
}

fn decode_logon(body: &[u8]) -> Result<CMsgClientLogonResponse, AuthError> {
    let response = decode::<CMsgClientLogonResponse>(body)?;
    let code = response.eresult.ok_or(AuthError::MissingField("eresult"))?;
    steamroom::enums::eresult(code).map_err(map_eresult)?;
    Ok(response)
}

fn parse_packet(data: &[u8]) -> Result<IncomingMsg, AuthError> {
    let invalid = |error: steamroom::error::ParseError| map_steamroom_error(error.into());
    match PacketHeader::parse(data).map_err(invalid)? {
        PacketHeader::Protobuf { header, body } => Ok(IncomingMsg {
            emsg: header.emsg,
            is_protobuf: true,
            header: header.decode_header().unwrap_or_default(),
            body,
        }),
        PacketHeader::Simple { header, body } => Ok(IncomingMsg {
            emsg: header.emsg,
            is_protobuf: false,
            header: CMsgProtoBufHeader::default(),
            body,
        }),
        PacketHeader::Extended { header, body } => Ok(IncomingMsg {
            emsg: header.emsg,
            is_protobuf: false,
            header: CMsgProtoBufHeader {
                steamid: Some(header.steam_id),
                client_sessionid: Some(header.session_id),
                jobid_target: Some(header.target_job_id),
                ..CMsgProtoBufHeader::default()
            },
            body,
        }),
    }
}

fn decode<M: Message + Default>(body: &[u8]) -> Result<M, AuthError> {
    M::decode(body).map_err(|error| AuthError::Connection(format!("protobuf decode: {error}")))
}

fn decode_license_list(body: &[u8]) -> Result<Vec<License>, AuthError> {
    let list = decode::<CMsgClientLicenseList>(body)?;
    // The proto default for a missing eresult is 2 (Fail).
    steamroom::enums::eresult(list.eresult.unwrap_or(2))
        .map_err(|result| map_call_result("the license list", result))?;
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

/// A package buffer is binary `KeyValues` behind a 4-byte version prefix
/// (`SteamKit`, `PICSProductInfoCallback`); the bare form is accepted too.
fn add_package_grants(buffer: &[u8], grants: &mut PackageGrants) {
    let parsed = buffer
        .get(4..)
        .and_then(|rest| parse_binary_kv(rest).ok())
        .or_else(|| parse_binary_kv(buffer).ok());
    if let Some(root) = parsed {
        grants.appids.extend(id_list(&root, "appids"));
        grants.depot_ids.extend(id_list(&root, "depotids"));
    }
}

fn id_list(root: &KeyValue, key: &str) -> Vec<u32> {
    let Some(KeyValue {
        value: KvValue::Children(entries),
        ..
    }) = root.get(key)
    else {
        return Vec::new();
    };
    entries
        .values()
        .filter_map(|entry| match &entry.value {
            KvValue::Int32(id) => u32::try_from(*id).ok(),
            KvValue::String(id) => id.parse().ok(),
            _ => None,
        })
        .collect()
}

/// A logged-in Steam session. Dropping it stops both tasks and closes the
/// connection.
pub struct SteamroomSession {
    inner: Arc<Inner>,
    heartbeat: Option<Duration>,
    tasks: Vec<AbortHandle>,
}

impl std::fmt::Debug for SteamroomSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SteamroomSession")
            .field("heartbeat", &self.heartbeat)
            .finish_non_exhaustive()
    }
}

impl Drop for SteamroomSession {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        self.inner
            .close(AuthError::Connection("session closed".to_owned()));
    }
}

impl SteamroomSession {
    /// Logs on over a prepared client and starts the receive and heartbeat
    /// tasks on the current runtime. [`logon`](crate::auth::SteamConnection::logon)
    /// calls this; it is public so a test can supply an in-memory transport.
    ///
    /// # Errors
    ///
    /// The mapped `CMsgClientLogonResponse` result, or
    /// [`AuthError::Connection`] on a transport failure or timeout.
    pub async fn start(
        client: SteamClient<Ready>,
        account_name: &str,
        refresh_token: &RefreshToken,
    ) -> Result<Self, AuthError> {
        let (logon_sender, logon_receiver) = oneshot::channel();
        let inner = Arc::new(Inner {
            client,
            steam_id: AtomicU64::new(SteamId::from_parts(1, 1, 1, 0).raw()),
            session_id: AtomicI32::new(0),
            cell_id: AtomicU32::new(0),
            next_job: AtomicU64::new(1),
            waiters: Mutex::new(BTreeMap::new()),
            logon: Mutex::new(Some(logon_sender)),
            published: watch::Sender::new(Published::default()),
            servers: OnceCell::new(),
        });
        let mut session = Self {
            inner: Arc::clone(&inner),
            heartbeat: None,
            tasks: vec![tokio::spawn(receive_loop(Arc::clone(&inner))).abort_handle()],
        };

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
        msg.header = inner.header();
        inner
            .client
            .send_msg(&msg)
            .await
            .map_err(map_steamroom_error)?;

        let reply = timeout(LOGON_TIMEOUT, logon_receiver)
            .await
            .map_err(|_| AuthError::Connection("timed out waiting for the logon reply".to_owned()))?
            .map_err(|_| AuthError::Connection("session closed during logon".to_owned()))??;
        session.heartbeat = reply.heartbeat;
        let period = reply.heartbeat.unwrap_or(DEFAULT_HEARTBEAT);
        session
            .tasks
            .push(tokio::spawn(heartbeat_loop(inner, period)).abort_handle());
        Ok(session)
    }
}

async fn heartbeat_loop(inner: Arc<Inner>, period: Duration) {
    let mut ticks = interval(period);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ticks.tick().await;
    loop {
        ticks.tick().await;
        if inner.closed().is_some() {
            return;
        }
        let mut msg = ClientMsg::new(EMsg::CLIENT_HEART_BEAT);
        msg.header = inner.header();
        if let Err(error) = inner.client.send_msg(&msg).await {
            inner.close(map_steamroom_error(error));
            return;
        }
    }
}

impl SteamSession for SteamroomSession {
    type Chunks = LiveChunks;

    fn heartbeat_interval(&self) -> Option<Duration> {
        self.heartbeat
    }

    async fn license_list(&self) -> Result<Vec<License>, AuthError> {
        let mut receiver = self.inner.published.subscribe();
        let published = timeout(
            LICENSE_LIST_TIMEOUT,
            receiver
                .wait_for(|published| published.licenses.is_some() || published.closed.is_some()),
        )
        .await
        .map_err(|_| AuthError::Connection("timed out waiting for CLIENT_LICENSE_LIST".to_owned()))?
        .map_err(|_| AuthError::Connection("session closed".to_owned()))?
        .clone();
        match (published.licenses, published.closed) {
            (Some(licenses), _) => licenses,
            (None, Some(error)) => Err(error),
            (None, None) => unreachable!("wait_for checked one of them"),
        }
    }

    async fn package_grants(&self, licenses: &[License]) -> Result<PackageGrants, AuthError> {
        let mut grants = PackageGrants::default();
        for batch in licenses.chunks(PICS_BATCH) {
            let request = CMsgClientPicsProductInfoRequest {
                packages: batch
                    .iter()
                    .map(
                        |license| c_msg_client_pics_product_info_request::PackageInfo {
                            packageid: Some(license.package_id),
                            access_token: Some(license.access_token),
                        },
                    )
                    .collect(),
                meta_data_only: Some(false),
                ..Default::default()
            };
            for part in self.inner.product_info(request).await? {
                for package in &part.packages {
                    if let Some(buffer) = &package.buffer {
                        add_package_grants(buffer, &mut grants);
                    }
                }
            }
        }
        Ok(grants)
    }

    async fn app_info(&self, appids: &[u32]) -> Result<Vec<AppInfo>, AuthError> {
        let mut apps = Vec::with_capacity(appids.len());
        for batch in appids.chunks(PICS_BATCH) {
            let body = CMsgClientPicsAccessTokenRequest {
                appids: batch.to_vec(),
                ..Default::default()
            }
            .encode_to_vec();
            let mut exchange = self
                .inner
                .request(
                    "PICS access tokens",
                    EMsg::CLIENT_PICS_ACCESS_TOKEN_REQUEST,
                    None,
                    &body,
                    EMsg::CLIENT_PICS_ACCESS_TOKEN_RESPONSE,
                )
                .await?;
            let reply = exchange.next().await?;
            drop(exchange);
            let tokens = decode::<CMsgClientPicsAccessTokenResponse>(&reply.body)?;
            let request = CMsgClientPicsProductInfoRequest {
                apps: batch
                    .iter()
                    .map(|appid| c_msg_client_pics_product_info_request::AppInfo {
                        appid: Some(*appid),
                        access_token: tokens
                            .app_access_tokens
                            .iter()
                            .find(|token| token.appid == Some(*appid))
                            .and_then(|token| token.access_token)
                            .or(Some(0)),
                        ..Default::default()
                    })
                    .collect(),
                meta_data_only: Some(false),
                ..Default::default()
            };
            for part in self.inner.product_info(request).await? {
                for app in part.apps {
                    let Some(appid) = app.appid else { continue };
                    apps.push(AppInfo {
                        appid,
                        change_number: app.change_number.unwrap_or(0),
                        text: app.buffer.map(|buffer| {
                            String::from_utf8_lossy(&buffer)
                                .trim_end_matches('\0')
                                .to_owned()
                        }),
                    });
                }
            }
        }
        Ok(apps)
    }

    async fn depot_key(&self, appid: u32, depot_id: u32) -> Result<DepotKey, AuthError> {
        let body = CMsgClientGetDepotDecryptionKey {
            depot_id: Some(depot_id),
            app_id: Some(appid),
        }
        .encode_to_vec();
        let mut exchange = self
            .inner
            .request(
                "a depot key",
                EMsg::CLIENT_GET_DEPOT_DECRYPTION_KEY,
                None,
                &body,
                EMsg::CLIENT_GET_DEPOT_DECRYPTION_KEY_RESPONSE,
            )
            .await?;
        let reply = exchange.next().await?;
        let response = CMsgClientGetDepotDecryptionKeyResponse::decode_checked(&reply.body)
            .map_err(|error| match error {
                steamroom::Error::Connection(ConnectionError::DepotAccessDenied(_)) => {
                    AuthError::CallDenied {
                        call: format!("the key for depot {depot_id}"),
                        result: "access denied".to_owned(),
                    }
                }
                other => map_steamroom_error(other),
            })?;
        // A key for another depot would pass as a run of corrupt chunks.
        if response.depot_id.is_some_and(|id| id != depot_id) {
            return Err(AuthError::Connection(format!(
                "Steam answered a key request for depot {depot_id} with another depot's key"
            )));
        }
        let bytes = response
            .depot_encryption_key
            .ok_or(AuthError::MissingField("depot_encryption_key"))?;
        let key = <[u8; 32]>::try_from(bytes.as_slice())
            .map_err(|_| map_steamroom_error(ConnectionError::EncryptionFailed.into()))?;
        Ok(DepotKey(key))
    }

    async fn manifest(
        &self,
        appid: u32,
        depot_id: u32,
        manifest_id: u64,
        branch: &str,
    ) -> Result<Option<DepotManifest>, AuthError> {
        let body = CContentServerDirectoryGetManifestRequestCodeRequest {
            app_id: Some(appid),
            depot_id: Some(depot_id),
            manifest_id: Some(manifest_id),
            // SteamKit sends no branch for `public`.
            app_branch: (!branch.eq_ignore_ascii_case(DEFAULT_BRANCH)).then(|| branch.to_owned()),
            branch_password_hash: None,
        }
        .encode_to_vec();
        let reply = self.inner.call(REQUEST_CODE_METHOD, &body).await?;
        let code = decode::<CContentServerDirectoryGetManifestRequestCodeResponse>(&reply)?
            .manifest_request_code
            .filter(|code| *code != 0);
        let Some(code) = code else {
            return Ok(None);
        };
        self.chunks(appid, depot_id)
            .await?
            .manifest(depot_id, manifest_id, code)
            .await
    }

    async fn chunks(&self, appid: u32, _depot_id: u32) -> Result<Arc<LiveChunks>, AuthError> {
        let servers = self.inner.servers().await?;
        LiveChunks::new(Arc::clone(&self.inner), appid, servers).map(Arc::new)
    }
}
