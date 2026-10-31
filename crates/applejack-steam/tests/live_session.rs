//! `SteamroomSession` over an in-memory transport: the real `SteamClient`,
//! the real dispatcher, and a scripted CM on the far side. Nothing opens a
//! socket. Message bodies are synthetic.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use applejack_steam::auth::AuthError;
use applejack_steam::keychain::RefreshToken;
use applejack_steam::live::SteamroomSession;
use applejack_steam::session::SteamSession;
use bytes::Bytes;
use prost::Message;
use steamroom::client::SteamClient;
use steamroom::client::msg::ClientMsg;
use steamroom::error::{ConnectionError, Error as SteamError};
use steamroom::generated::{
    CContentServerDirectoryGetManifestRequestCodeRequest,
    CContentServerDirectoryGetManifestRequestCodeResponse,
    CContentServerDirectoryGetServersForSteamPipeResponse, CContentServerDirectoryServerInfo,
    CMsgClientGetDepotDecryptionKeyResponse, CMsgClientLicenseList, CMsgClientLoggedOff,
    CMsgClientLogon, CMsgClientLogonResponse, CMsgClientPicsAccessTokenRequest,
    CMsgClientPicsAccessTokenResponse, CMsgClientPicsProductInfoRequest,
    CMsgClientPicsProductInfoResponse, CMsgMulti, CMsgProtoBufHeader, c_msg_client_license_list,
    c_msg_client_pics_access_token_response, c_msg_client_pics_product_info_response,
};
use steamroom::messages::EMsg;
use steamroom::messages::header::PacketHeader;
use steamroom::transport::Transport;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

const FIXTURE_TOKEN: &str = "fixture-refresh-token-not-a-real-value";
const STEAM_ID: u64 = 76_561_197_960_287_930;
const SESSION_ID: i32 = 4242;

struct MemoryTransport {
    to_client: Mutex<mpsc::UnboundedReceiver<Bytes>>,
    to_server: mpsc::UnboundedSender<Bytes>,
}

impl Transport for MemoryTransport {
    fn send(
        &self,
        payload: &[u8],
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), SteamError>> + Send + '_>> {
        let sent = self
            .to_server
            .send(Bytes::copy_from_slice(payload))
            .map_err(|_| ConnectionError::Disconnected.into());
        Box::pin(async move { sent })
    }

    fn recv(
        &self,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Bytes, SteamError>> + Send + '_>> {
        Box::pin(async move {
            self.to_client
                .lock()
                .await
                .recv()
                .await
                .ok_or_else(|| ConnectionError::Disconnected.into())
        })
    }
}

/// The far side of the transport.
struct Cm {
    to_client: mpsc::UnboundedSender<Bytes>,
    from_client: mpsc::UnboundedReceiver<Bytes>,
}

struct Received {
    emsg: EMsg,
    header: CMsgProtoBufHeader,
    body: Bytes,
}

impl Cm {
    async fn recv(&mut self) -> Received {
        let data = self.from_client.recv().await.expect("client hung up");
        let PacketHeader::Protobuf { header, body } = PacketHeader::parse(&data).unwrap() else {
            panic!("expected a protobuf message");
        };
        Received {
            emsg: header.emsg,
            header: header.decode_header().unwrap(),
            body,
        }
    }

    async fn recv_past_heartbeats(&mut self) -> Received {
        loop {
            let received = self.recv().await;
            if received.emsg != EMsg::CLIENT_HEART_BEAT {
                return received;
            }
        }
    }

    fn send(&self, packet: Vec<u8>) {
        self.to_client.send(Bytes::from(packet)).unwrap();
    }

    fn reply(&self, to: &Received, emsg: EMsg, body: &[u8]) {
        let mut msg = ClientMsg::with_body(emsg, body);
        msg.header.jobid_target = to.header.jobid_source;
        self.send(msg.to_bytes());
    }
}

fn logon_response(heartbeat_seconds: i32) -> Vec<u8> {
    let body = CMsgClientLogonResponse {
        eresult: Some(1),
        heartbeat_seconds: Some(heartbeat_seconds),
        cell_id: Some(7),
        ..Default::default()
    }
    .encode_to_vec();
    let mut msg = ClientMsg::with_body(EMsg::CLIENT_LOG_ON_RESPONSE, &body);
    msg.header.steamid = Some(STEAM_ID);
    msg.header.client_sessionid = Some(SESSION_ID);
    msg.to_bytes()
}

fn license_list(package_ids: &[u32]) -> Vec<u8> {
    let body = CMsgClientLicenseList {
        eresult: Some(1),
        licenses: package_ids
            .iter()
            .map(|package_id| c_msg_client_license_list::License {
                package_id: Some(*package_id),
                access_token: Some(u64::from(*package_id) + 1),
                ..Default::default()
            })
            .collect(),
    }
    .encode_to_vec();
    ClientMsg::with_body(EMsg::CLIENT_LICENSE_LIST, &body).to_bytes()
}

fn multi(packets: &[Vec<u8>]) -> Vec<u8> {
    let mut payload = Vec::new();
    for packet in packets {
        payload.extend_from_slice(&u32::try_from(packet.len()).unwrap().to_le_bytes());
        payload.extend_from_slice(packet);
    }
    let body = CMsgMulti {
        size_unzipped: None,
        message_body: Some(payload),
    }
    .encode_to_vec();
    ClientMsg::with_body(EMsg::MULTI, &body).to_bytes()
}

/// Starts a session and hands back the CM once `CLIENT_LOGON` has arrived.
async fn connect() -> (JoinHandle<Result<SteamroomSession, AuthError>>, Cm) {
    let (client_sender, client_receiver) = mpsc::unbounded_channel();
    let (server_sender, server_receiver) = mpsc::unbounded_channel();
    let transport = MemoryTransport {
        to_client: Mutex::new(client_receiver),
        to_server: server_sender,
    };
    let (client, _rx) = SteamClient::connect_ws(transport).await.unwrap();
    let client = client.prepare().await.unwrap();
    let mut cm = Cm {
        to_client: client_sender,
        from_client: server_receiver,
    };
    assert_eq!(cm.recv().await.emsg, EMsg::CLIENT_HELLO);
    let session = tokio::spawn(async move {
        SteamroomSession::start(client, "redacted", &RefreshToken::new(FIXTURE_TOKEN)).await
    });
    let logon = cm.recv().await;
    assert_eq!(logon.emsg, EMsg::CLIENT_LOGON);
    let message = CMsgClientLogon::decode(&*logon.body).unwrap();
    assert_eq!(message.access_token.as_deref(), Some(FIXTURE_TOKEN));
    assert_eq!(message.supports_rate_limit_response, Some(true));
    (session, cm)
}

async fn logged_in(heartbeat_seconds: i32) -> (SteamroomSession, Cm) {
    let (session, cm) = connect().await;
    cm.send(logon_response(heartbeat_seconds));
    (session.await.unwrap().unwrap(), cm)
}

#[tokio::test]
async fn a_license_list_in_the_logon_multi_is_not_lost() {
    let (session, cm) = connect().await;
    // `SteamClient::login` would return on the first packet and drop the
    // second.
    cm.send(multi(&[logon_response(9), license_list(&[10, 20])]));
    let session = session.await.unwrap().unwrap();

    assert_eq!(session.heartbeat_interval(), Some(Duration::from_secs(9)));
    let licenses = session.license_list().await.unwrap();
    assert_eq!(
        licenses.iter().map(|l| l.package_id).collect::<Vec<_>>(),
        [10, 20]
    );
    assert_eq!(licenses[0].access_token, 11);
}

#[tokio::test]
async fn a_refused_logon_is_typed() {
    let (session, cm) = connect().await;
    let body = CMsgClientLogonResponse {
        eresult: Some(84),
        ..Default::default()
    }
    .encode_to_vec();
    cm.send(ClientMsg::with_body(EMsg::CLIENT_LOG_ON_RESPONSE, &body).to_bytes());
    assert!(matches!(
        session.await.unwrap(),
        Err(AuthError::RateLimited { .. })
    ));
}

#[tokio::test]
async fn license_list_answers_late_and_repeatedly_and_keeps_the_newest() {
    let (session, cm) = logged_in(9).await;
    let waiter = tokio::spawn(async move {
        let first = session.license_list().await.unwrap();
        (session, first)
    });
    tokio::task::yield_now().await;
    cm.send(license_list(&[1]));
    let (session, first) = waiter.await.unwrap();
    assert_eq!(first.len(), 1);

    cm.send(license_list(&[1, 2]));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(session.license_list().await.unwrap().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn heartbeats_continue_during_a_slow_pics_call() {
    let (session, mut cm) = logged_in(9).await;
    let session = Arc::new(session);
    let caller = Arc::clone(&session);
    let call = tokio::spawn(async move { caller.depot_key(100, 101).await });

    let request = cm.recv().await;
    assert_eq!(request.emsg, EMsg::CLIENT_GET_DEPOT_DECRYPTION_KEY);
    assert_eq!(request.header.steamid, Some(STEAM_ID));
    assert_eq!(request.header.client_sessionid, Some(SESSION_ID));

    // The reply is withheld until three heartbeats have arrived: the call
    // could not finish if they waited on it.
    for _ in 0..3 {
        // Shorter than the 60 s request timeout, so the call is still waiting.
        let heartbeat = tokio::time::timeout(Duration::from_secs(10), cm.recv())
            .await
            .expect("no heartbeat while a call was waiting");
        assert_eq!(heartbeat.emsg, EMsg::CLIENT_HEART_BEAT);
        assert_eq!(heartbeat.header.client_sessionid, Some(SESSION_ID));
    }
    let body = CMsgClientGetDepotDecryptionKeyResponse {
        eresult: Some(1),
        depot_id: Some(101),
        depot_encryption_key: Some(vec![7; 32]),
    }
    .encode_to_vec();
    cm.reply(
        &request,
        EMsg::CLIENT_GET_DEPOT_DECRYPTION_KEY_RESPONSE,
        &body,
    );
    assert_eq!(call.await.unwrap().unwrap().0, [7; 32]);
}

#[tokio::test(start_paused = true)]
async fn a_request_that_is_never_answered_times_out_and_the_session_survives() {
    let (session, mut cm) = logged_in(9).await;
    let session = Arc::new(session);
    let caller = Arc::clone(&session);
    let call = tokio::spawn(async move { caller.depot_key(100, 101).await });
    let unanswered = cm.recv().await;
    assert!(matches!(
        call.await.unwrap(),
        Err(AuthError::Connection(message)) if message.contains("timed out")
            && message.contains("depot key")
    ));

    // A late reply is dropped, not handed to the next caller.
    let body = CMsgClientGetDepotDecryptionKeyResponse {
        eresult: Some(1),
        depot_id: Some(101),
        depot_encryption_key: Some(vec![1; 32]),
    }
    .encode_to_vec();
    cm.reply(
        &unanswered,
        EMsg::CLIENT_GET_DEPOT_DECRYPTION_KEY_RESPONSE,
        &body,
    );

    let caller = Arc::clone(&session);
    let second = tokio::spawn(async move { caller.depot_key(100, 102).await });
    let request = cm.recv_past_heartbeats().await;
    let body = CMsgClientGetDepotDecryptionKeyResponse {
        eresult: Some(1),
        depot_id: Some(102),
        depot_encryption_key: Some(vec![2; 32]),
    }
    .encode_to_vec();
    cm.reply(
        &request,
        EMsg::CLIENT_GET_DEPOT_DECRYPTION_KEY_RESPONSE,
        &body,
    );
    assert_eq!(second.await.unwrap().unwrap().0, [2; 32]);
}

fn product_info_part(appid: u32, text: &str, pending: bool) -> Vec<u8> {
    CMsgClientPicsProductInfoResponse {
        apps: vec![c_msg_client_pics_product_info_response::AppInfo {
            appid: Some(appid),
            change_number: Some(5),
            buffer: Some(format!("{text}\0").into_bytes()),
            ..Default::default()
        }],
        response_pending: Some(pending),
        ..Default::default()
    }
    .encode_to_vec()
}

#[tokio::test]
async fn app_info_sends_tokens_and_collects_every_response_part() {
    let (session, mut cm) = logged_in(9).await;
    let session = Arc::new(session);
    let caller = Arc::clone(&session);
    let call = tokio::spawn(async move { caller.app_info(&[1, 2]).await });

    let tokens = cm.recv_past_heartbeats().await;
    assert_eq!(tokens.emsg, EMsg::CLIENT_PICS_ACCESS_TOKEN_REQUEST);
    assert_eq!(
        CMsgClientPicsAccessTokenRequest::decode(&*tokens.body)
            .unwrap()
            .appids,
        [1, 2]
    );
    let body = CMsgClientPicsAccessTokenResponse {
        app_access_tokens: vec![c_msg_client_pics_access_token_response::AppToken {
            appid: Some(2),
            access_token: Some(222),
        }],
        ..Default::default()
    }
    .encode_to_vec();
    cm.reply(&tokens, EMsg::CLIENT_PICS_ACCESS_TOKEN_RESPONSE, &body);

    let request = cm.recv_past_heartbeats().await;
    assert_eq!(request.emsg, EMsg::CLIENT_PICS_PRODUCT_INFO_REQUEST);
    let decoded = CMsgClientPicsProductInfoRequest::decode(&*request.body).unwrap();
    assert_eq!(decoded.apps[0].access_token, Some(0));
    assert_eq!(decoded.apps[1].access_token, Some(222));

    let first = product_info_part(1, "\"appinfo\"\n{\n}\n", true);
    let second = product_info_part(2, "\"appinfo\"\n{\n}\n", false);
    cm.reply(&request, EMsg::CLIENT_PICS_PRODUCT_INFO_RESPONSE, &first);
    cm.reply(&request, EMsg::CLIENT_PICS_PRODUCT_INFO_RESPONSE, &second);

    let apps = call.await.unwrap().unwrap();
    assert_eq!(apps.iter().map(|app| app.appid).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(apps[0].change_number, 5);
    assert!(!apps[0].text.as_ref().unwrap().contains('\0'));
}

fn package_buffer(package_id: u32, appids: &[i32]) -> Vec<u8> {
    let mut kv = 1_u32.to_le_bytes().to_vec();
    kv.push(0);
    kv.extend_from_slice(package_id.to_string().as_bytes());
    kv.push(0);
    kv.push(0);
    kv.extend_from_slice(b"appids\0");
    for (index, appid) in appids.iter().enumerate() {
        kv.push(2);
        kv.extend_from_slice(index.to_string().as_bytes());
        kv.push(0);
        kv.extend_from_slice(&appid.to_le_bytes());
    }
    kv.extend_from_slice(&[8, 8, 8]);
    kv
}

#[tokio::test]
async fn package_apps_passes_license_tokens_and_reads_the_binary_buffer() {
    let (session, mut cm) = logged_in(9).await;
    cm.send(license_list(&[10]));
    let licenses = session.license_list().await.unwrap();
    let session = Arc::new(session);
    let caller = Arc::clone(&session);
    let call = tokio::spawn(async move { caller.package_apps(&licenses).await });

    let request = cm.recv_past_heartbeats().await;
    let decoded = CMsgClientPicsProductInfoRequest::decode(&*request.body).unwrap();
    assert_eq!(decoded.packages[0].packageid, Some(10));
    assert_eq!(decoded.packages[0].access_token, Some(11));

    let body = CMsgClientPicsProductInfoResponse {
        packages: vec![c_msg_client_pics_product_info_response::PackageInfo {
            packageid: Some(10),
            buffer: Some(package_buffer(10, &[30, 20, 30])),
            ..Default::default()
        }],
        ..Default::default()
    }
    .encode_to_vec();
    cm.reply(&request, EMsg::CLIENT_PICS_PRODUCT_INFO_RESPONSE, &body);
    assert_eq!(call.await.unwrap().unwrap(), [20, 30]);
}

#[tokio::test]
async fn concurrent_requests_get_their_own_replies_by_job() {
    let (session, mut cm) = logged_in(9).await;
    let session = Arc::new(session);
    let one = {
        let session = Arc::clone(&session);
        tokio::spawn(async move { session.depot_key(100, 1).await })
    };
    let first = cm.recv_past_heartbeats().await;
    let two = {
        let session = Arc::clone(&session);
        tokio::spawn(async move { session.depot_key(100, 2).await })
    };
    let second = cm.recv_past_heartbeats().await;
    assert_ne!(first.header.jobid_source, second.header.jobid_source);

    let reply = |byte: u8| {
        CMsgClientGetDepotDecryptionKeyResponse {
            eresult: Some(1),
            depot_id: Some(u32::from(byte)),
            depot_encryption_key: Some(vec![byte; 32]),
        }
        .encode_to_vec()
    };
    cm.reply(
        &second,
        EMsg::CLIENT_GET_DEPOT_DECRYPTION_KEY_RESPONSE,
        &reply(2),
    );
    cm.reply(
        &first,
        EMsg::CLIENT_GET_DEPOT_DECRYPTION_KEY_RESPONSE,
        &reply(1),
    );
    assert_eq!(one.await.unwrap().unwrap().0, [1; 32]);
    assert_eq!(two.await.unwrap().unwrap().0, [2; 32]);
}

#[tokio::test]
async fn a_reply_without_a_job_id_reaches_the_request_expecting_it() {
    let (session, mut cm) = logged_in(9).await;
    let session = Arc::new(session);
    let caller = Arc::clone(&session);
    let call = tokio::spawn(async move { caller.depot_key(100, 1).await });
    cm.recv_past_heartbeats().await;
    let body = CMsgClientGetDepotDecryptionKeyResponse {
        eresult: Some(1),
        depot_id: Some(1),
        depot_encryption_key: Some(vec![3; 32]),
    }
    .encode_to_vec();
    cm.send(ClientMsg::with_body(EMsg::CLIENT_GET_DEPOT_DECRYPTION_KEY_RESPONSE, &body).to_bytes());
    assert_eq!(call.await.unwrap().unwrap().0, [3; 32]);
}

#[tokio::test]
async fn a_key_for_another_depot_is_refused() {
    let (session, mut cm) = logged_in(9).await;
    let session = Arc::new(session);
    let caller = Arc::clone(&session);
    let call = tokio::spawn(async move { caller.depot_key(100, 1).await });
    let request = cm.recv_past_heartbeats().await;
    let body = CMsgClientGetDepotDecryptionKeyResponse {
        eresult: Some(1),
        depot_id: Some(2),
        depot_encryption_key: Some(vec![3; 32]),
    }
    .encode_to_vec();
    cm.reply(
        &request,
        EMsg::CLIENT_GET_DEPOT_DECRYPTION_KEY_RESPONSE,
        &body,
    );
    assert!(matches!(call.await.unwrap(), Err(AuthError::Connection(_))));
}

#[tokio::test]
async fn a_logged_off_message_fails_the_waiter_and_later_calls() {
    let (session, mut cm) = logged_in(9).await;
    let session = Arc::new(session);
    let caller = Arc::clone(&session);
    let call = tokio::spawn(async move { caller.depot_key(100, 1).await });
    cm.recv_past_heartbeats().await;
    let body = CMsgClientLoggedOff { eresult: Some(6) }.encode_to_vec();
    cm.send(ClientMsg::with_body(EMsg::CLIENT_LOGGED_OFF, &body).to_bytes());

    let error = call.await.unwrap().unwrap_err();
    assert!(
        error.to_string().contains("logged the session off"),
        "{error}"
    );
    assert!(session.depot_key(100, 1).await.is_err());
    assert!(session.license_list().await.is_err());
}

#[tokio::test]
async fn a_lost_connection_fails_the_waiter() {
    let (session, mut cm) = logged_in(9).await;
    let session = Arc::new(session);
    let caller = Arc::clone(&session);
    let call = tokio::spawn(async move { caller.depot_key(100, 1).await });
    cm.recv_past_heartbeats().await;
    drop(cm);
    assert!(matches!(call.await.unwrap(), Err(AuthError::Connection(_))));
}

#[tokio::test]
async fn a_session_prints_no_token() {
    let (session, _cm) = logged_in(9).await;
    assert!(!format!("{session:?}").contains(FIXTURE_TOKEN));
}

/// A loopback HTTP server that gives every request one status line. No packet
/// leaves the machine.
fn http(status: &'static str) -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut request = [0; 2048];
            let _ = stream.read(&mut request);
            counter.fetch_add(1, Ordering::SeqCst);
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
        }
    });
    (port, hits)
}

/// `vhost` carries the port, which is what the fixture connects to. `host`
/// does not resolve, and `optional` HTTPS would fail against a plain server,
/// so these only pass with `SteamKit`'s rules.
fn cdn_entry(port: u16, load: f32) -> CContentServerDirectoryServerInfo {
    CContentServerDirectoryServerInfo {
        r#type: Some("CDN".to_owned()),
        host: Some("unresolvable.invalid".to_owned()),
        vhost: Some(format!("127.0.0.1:{port}")),
        https_support: Some("optional".to_owned()),
        weighted_load: Some(load),
        ..Default::default()
    }
}

/// Runs `manifest` against two loopback CDN servers, lowest load first.
async fn manifest_from(
    first: &'static str,
    second: &'static str,
) -> (Result<bool, AuthError>, usize, usize) {
    let (session, mut cm) = logged_in(9).await;
    let session = Arc::new(session);
    let caller = Arc::clone(&session);
    let (one, one_hits) = http(first);
    let (two, two_hits) = http(second);
    let call = tokio::spawn(async move {
        caller
            .manifest(100, 1, 5, "public")
            .await
            .map(|manifest| manifest.is_some())
    });
    let code = cm.recv_past_heartbeats().await;
    let body = CContentServerDirectoryGetManifestRequestCodeResponse {
        manifest_request_code: Some(9),
    }
    .encode_to_vec();
    cm.reply(&code, EMsg::SERVICE_METHOD_RESPONSE, &body);
    let list = cm.recv_past_heartbeats().await;
    let body = CContentServerDirectoryGetServersForSteamPipeResponse {
        servers: vec![cdn_entry(one, 1.0), cdn_entry(two, 2.0)],
        ..Default::default()
    }
    .encode_to_vec();
    cm.reply(&list, EMsg::SERVICE_METHOD_RESPONSE, &body);
    let result = call.await.unwrap();
    (
        result,
        one_hits.load(Ordering::SeqCst),
        two_hits.load(Ordering::SeqCst),
    )
}

const UNAVAILABLE: &str = "503 Service Unavailable";
const NOT_FOUND: &str = "404 Not Found";
const BROKEN: &str = "500 Internal Server Error";

// From the protocol review's `proof_one_503_stops_the_rotation`.
#[tokio::test]
async fn one_busy_cdn_server_does_not_end_a_manifest_fetch() {
    let (result, first_hits, second_hits) = manifest_from(UNAVAILABLE, NOT_FOUND).await;
    assert_eq!(first_hits, 1);
    assert_eq!(second_hits, 1);
    assert!(matches!(result, Ok(false)), "{result:?}");
}

#[tokio::test]
async fn a_rate_limit_is_the_answer_only_when_no_server_answered() {
    let (result, first_hits, second_hits) = manifest_from(UNAVAILABLE, UNAVAILABLE).await;
    assert_eq!((first_hits, second_hits), (1, 1));
    assert!(
        matches!(result, Err(AuthError::RateLimited { .. })),
        "{result:?}"
    );
}

#[tokio::test]
async fn one_404_in_the_pool_reads_as_no_longer_served() {
    let (result, first_hits, second_hits) = manifest_from(BROKEN, NOT_FOUND).await;
    assert_eq!((first_hits, second_hits), (1, 1));
    assert!(matches!(result, Ok(false)), "{result:?}");
}

#[tokio::test]
async fn every_cdn_server_failing_ends_the_pass_and_names_the_manifest() {
    let (result, first_hits, second_hits) = manifest_from(BROKEN, BROKEN).await;
    assert_eq!((first_hits, second_hits), (1, 1));
    assert!(
        matches!(&result, Err(AuthError::Connection(message)) if message.contains("manifest 5")),
        "{result:?}"
    );
}

// From the protocol review's
// `proof_access_denied_on_a_service_call_reads_as_a_refused_login`.
#[tokio::test]
async fn a_denied_service_call_is_not_a_refused_login() {
    let (session, mut cm) = logged_in(9).await;
    let session = Arc::new(session);
    let caller = Arc::clone(&session);
    let call = tokio::spawn(async move { caller.manifest(100, 1, 5, "beta").await.map(drop) });
    let request = cm.recv_past_heartbeats().await;
    let sent =
        CContentServerDirectoryGetManifestRequestCodeRequest::decode(&*request.body).unwrap();
    assert_eq!(sent.app_branch.as_deref(), Some("beta"));
    let mut msg = ClientMsg::new(EMsg::SERVICE_METHOD_RESPONSE);
    msg.header.jobid_target = request.header.jobid_source;
    msg.header.eresult = Some(15);
    cm.send(msg.to_bytes());

    let error = call.await.unwrap().unwrap_err();
    let AuthError::CallDenied { call, .. } = &error else {
        panic!("{error}");
    };
    assert!(call.contains("GetManifestRequestCode"), "{error}");
    assert!(!error.to_string().contains("login"), "{error}");
}

#[tokio::test]
async fn a_denied_depot_key_is_not_a_refused_login() {
    let (session, mut cm) = logged_in(9).await;
    let session = Arc::new(session);
    let caller = Arc::clone(&session);
    let call = tokio::spawn(async move { caller.depot_key(100, 7).await });
    let request = cm.recv_past_heartbeats().await;
    let body = CMsgClientGetDepotDecryptionKeyResponse {
        eresult: Some(15),
        depot_id: Some(7),
        depot_encryption_key: None,
    }
    .encode_to_vec();
    cm.reply(
        &request,
        EMsg::CLIENT_GET_DEPOT_DECRYPTION_KEY_RESPONSE,
        &body,
    );
    assert!(matches!(
        call.await.unwrap(),
        Err(AuthError::CallDenied { .. })
    ));
}

#[tokio::test]
async fn a_denied_logon_is_still_a_refused_login() {
    let (session, cm) = connect().await;
    let body = CMsgClientLogonResponse {
        eresult: Some(15),
        ..Default::default()
    }
    .encode_to_vec();
    cm.send(ClientMsg::with_body(EMsg::CLIENT_LOG_ON_RESPONSE, &body).to_bytes());
    assert!(matches!(session.await.unwrap(), Err(AuthError::Refused)));
}

#[tokio::test]
async fn the_public_branch_goes_unnamed_in_a_request_code() {
    let (session, mut cm) = logged_in(9).await;
    let session = Arc::new(session);
    let caller = Arc::clone(&session);
    let call = tokio::spawn(async move { caller.manifest(100, 1, 5, "public").await.map(drop) });
    let request = cm.recv_past_heartbeats().await;
    let sent =
        CContentServerDirectoryGetManifestRequestCodeRequest::decode(&*request.body).unwrap();
    assert_eq!(sent.app_branch, None);
    let body = CContentServerDirectoryGetManifestRequestCodeResponse {
        manifest_request_code: Some(0),
    }
    .encode_to_vec();
    cm.reply(&request, EMsg::SERVICE_METHOD_RESPONSE, &body);
    assert!(call.await.unwrap().is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_failed_job_ends_its_call_at_once() {
    let (session, mut cm) = logged_in(9).await;
    let session = Arc::new(session);
    let caller = Arc::clone(&session);
    let call = tokio::spawn(async move { caller.depot_key(100, 7).await });
    let request = cm.recv_past_heartbeats().await;
    let started = tokio::time::Instant::now();
    cm.reply(&request, EMsg(113), &[]);
    let error = call.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("could not deliver"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(session.heartbeat_interval().is_some());
}
