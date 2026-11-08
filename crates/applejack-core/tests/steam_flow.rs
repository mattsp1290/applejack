//! Login, the library cache, and the install job over a scripted connection
//! and session. Nothing opens a socket or touches the Keychain: the store is
//! `InMemoryStore`, and every title, name, and token here is synthetic.
//! `crates/applejack/tests/app_session.rs` has the refusals through the real
//! binary.

mod common;

#[allow(dead_code, reason = "this test binary uses part of the shared fake")]
#[path = "../../applejack-steam/tests/support/mod.rs"]
mod support;

use std::future::{Future, ready};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{env, fs};

use applejack_core::config::{ConfigDb, Drm, NewSession};
use applejack_core::install::TitleInstall;
use applejack_core::rpc::jobs::Notifier;
use applejack_core::rpc::methods::{
    AppCancelParams, AppInstallParams, LoginMode, LoginSession, LoginStepWire, Method,
    SteamLibraryParams, SteamLoginCompleteParams, SteamLoginStartParams, SteamLogoutParams,
};
use applejack_core::rpc::wire::{ErrorCode, RpcError};
use applejack_core::rpc::{CoreHandler, Handler, Logged, REDACTED};
use applejack_core::steam::{
    InstallState, LoginFlow, META_ACCOUNT, NoSteam, SteamHost, SteamLibraryResult,
};
use applejack_core::{Core, CoreError};
use applejack_engine::rosetta_wine::snapshot::PrefixLock;
use applejack_engine::rosetta_wine::utc_now;
use applejack_steam::auth::{AuthError, AuthMode, AuthPoll, GuardFactor};
use applejack_steam::session::{AppInfo, PackageGrants, SteamSession};
use applejack_steam::{
    Credentials, Error as SteamError, InMemoryStore, License, LoginStep, Password, RefreshToken,
    RefreshTokenStore, SteamConnection, Tokens,
};
use serde_json::{Value, json};
use steamroom::depot::DepotKey;
use steamroom::depot::manifest::DepotManifest;
use support::{FakeChunks, FakeSession, Fault, file, manifest};
use tempfile::TempDir;

const APP: u32 = 7000;
const DEPOT: u32 = 7001;
const MANIFEST: u64 = 900;
const ACCOUNT: &str = "fixture-user";
const PASSWORD: &str = "fixture-password-value";
const REFRESH: &str = "fixture-refresh-token-value";
const CODE: &str = "FXCDE";
const QR: &str = "https://s.team/q/fixture-challenge";

fn pe() -> Vec<u8> {
    common::pe_image()
}

fn product_info(launch: &str) -> AppInfo {
    let text = format!(
        r#""appinfo"
{{
    "appid" "{APP}"
    "common" {{ "name" "Fixture Title" "oslist" "windows" }}
    "config" {{ "installdir" "FixtureTitle" "launch" {{ {launch} }} }}
    "depots" {{
        "{DEPOT}" {{
            "config" {{ "oslist" "windows" "osarch" "64" }}
            "manifests" {{ "public" {{ "gid" "{MANIFEST}" "size" "4096" }} }}
        }}
        "branches" {{ "public" {{ "buildid" "55" }} }}
    }}
}}
"#
    );
    AppInfo {
        appid: APP,
        change_number: 1,
        text: Some(text),
    }
}

/// Beside `product_info`'s `public`: another manifest and build on `beta`.
const BETA_MANIFEST: u64 = 901;

fn with_beta(mut info: AppInfo) -> AppInfo {
    let text = info.text.take().unwrap();
    let text = text
        .replace(
            r#""manifests" { "public" { "gid" "900" "size" "4096" } }"#,
            r#""manifests" { "public" { "gid" "900" "size" "4096" }
                             "beta" { "gid" "901" "size" "4096" } }"#,
        )
        .replace(
            r#""branches" { "public" { "buildid" "55" } }"#,
            r#""branches" { "public" { "buildid" "55" } "beta" { "buildid" "66" } }"#,
        );
    info.text = Some(text);
    info
}

/// [`session`] with a `beta` branch, and every file present on both.
fn branched_session() -> Arc<FakeSession> {
    let session = FakeSession {
        grants: PackageGrants {
            appids: [APP].into(),
            depot_ids: [DEPOT].into(),
        },
        ..FakeSession::default()
    };
    session
        .app_info
        .lock()
        .unwrap()
        .push_back(vec![with_beta(product_info(ONE_ENTRY))]);
    let chunks = &session.chunks;
    let exe = file(chunks, "bin\\title.exe", &[&pe()]);
    let public = file(chunks, "data.bin", &[b"public data"]);
    let beta = file(chunks, "data.bin", &[b"beta data"]);
    session.add_manifest(DEPOT, manifest(MANIFEST, vec![exe.clone(), public]));
    session.add_manifest(DEPOT, manifest(BETA_MANIFEST, vec![exe, beta]));
    Arc::new(session)
}

const ONE_ENTRY: &str = r#""0" { "executable" "bin\\title.exe" "arguments" "-windowed"
                                  "type" "default" "config" { "oslist" "windows" } }"#;
const TWO_ENTRIES: &str = r#""0" { "executable" "a.exe" } "1" { "executable" "b.exe" }"#;

/// A session whose title has two files, the second behind `block`.
fn session(launch: &str, block: bool) -> Arc<FakeSession> {
    let session = FakeSession {
        grants: PackageGrants {
            appids: [APP].into(),
            depot_ids: [DEPOT].into(),
        },
        ..FakeSession::default()
    };
    session
        .app_info
        .lock()
        .unwrap()
        .push_back(vec![product_info(launch)]);
    let chunks = &session.chunks;
    let exe = file(chunks, "bin\\title.exe", &[&pe()]);
    let data = file(chunks, "data.bin", &[b"title data"]);
    if block {
        chunks.fault(&data.chunks[0].id, Fault::Slow(Duration::from_secs(600)));
    }
    session.add_manifest(DEPOT, manifest(MANIFEST, vec![exe, data]));
    Arc::new(session)
}

/// What the scripted Steam will say, shared by every connection of a host.
#[derive(Default)]
struct Script {
    guard: Option<GuardFactor>,
    pending_polls: usize,
    /// The next submit fails with this.
    submit_error: Option<AuthError>,
    /// The next poll fails with this.
    poll_error: Option<AuthError>,
    logon_error: Option<AuthError>,
    submitted: Vec<String>,
    polls: usize,
    gate: Option<Arc<Gate>>,
    logons: Vec<(String, String)>,
    connections: usize,
}

struct FakeHost {
    script: Arc<Mutex<Script>>,
    session: Arc<FakeSession>,
    store: InMemoryStore,
}

impl FakeHost {
    fn new(script: Script, session: Arc<FakeSession>) -> Self {
        Self {
            script: Arc::new(Mutex::new(script)),
            session,
            store: InMemoryStore::default(),
        }
    }

    fn signed_in(session: Arc<FakeSession>) -> Self {
        let host = Self::new(Script::default(), session);
        host.store.put(&RefreshToken::new(REFRESH)).unwrap();
        host
    }
}

struct FakeConnection {
    script: Arc<Mutex<Script>>,
    session: Arc<FakeSession>,
}

/// Stops `license_list` where the fetch is under way, for a test to act.
struct Gate {
    reached: Mutex<Sender<()>>,
    release: Mutex<Receiver<()>>,
}

/// `FakeSession` behind an `Arc`, so a test can look at it afterwards.
struct Shared(Arc<FakeSession>, Option<Arc<Gate>>);

impl SteamSession for Shared {
    type Chunks = FakeChunks;

    fn heartbeat_interval(&self) -> Option<Duration> {
        None
    }

    fn license_list(&self) -> impl Future<Output = Result<Vec<License>, AuthError>> + Send {
        if let Some(gate) = &self.1 {
            gate.reached.lock().unwrap().send(()).unwrap();
            gate.release.lock().unwrap().recv().unwrap();
        }
        self.0.license_list()
    }

    fn package_grants(
        &self,
        licenses: &[License],
    ) -> impl Future<Output = Result<PackageGrants, AuthError>> + Send {
        self.0.package_grants(licenses)
    }

    fn app_info(
        &self,
        appids: &[u32],
    ) -> impl Future<Output = Result<Vec<AppInfo>, AuthError>> + Send {
        self.0.app_info(appids)
    }

    fn depot_key(
        &self,
        appid: u32,
        depot_id: u32,
    ) -> impl Future<Output = Result<DepotKey, AuthError>> + Send {
        self.0.depot_key(appid, depot_id)
    }

    fn manifest(
        &self,
        appid: u32,
        depot_id: u32,
        manifest_id: u64,
        branch: &str,
    ) -> impl Future<Output = Result<Option<DepotManifest>, AuthError>> + Send {
        self.0.manifest(appid, depot_id, manifest_id, branch)
    }

    fn chunks(
        &self,
        appid: u32,
        depot_id: u32,
    ) -> impl Future<Output = Result<Arc<FakeChunks>, AuthError>> + Send {
        self.0.chunks(appid, depot_id)
    }
}

impl SteamConnection for FakeConnection {
    type Session = Shared;

    fn begin_qr(&mut self) -> impl Future<Output = Result<String, AuthError>> + Send {
        ready(Ok(QR.to_owned()))
    }

    fn begin_credentials(
        &mut self,
        credentials: &Credentials,
    ) -> impl Future<Output = Result<Option<GuardFactor>, AuthError>> + Send {
        ready(if credentials.password.expose() == PASSWORD {
            Ok(self.script.lock().unwrap().guard)
        } else {
            Err(AuthError::Refused)
        })
    }

    fn submit_guard_code(
        &mut self,
        code: &str,
    ) -> impl Future<Output = Result<(), AuthError>> + Send {
        let mut script = self.script.lock().unwrap();
        script.submitted.push(code.to_owned());
        ready(script.submit_error.take().map_or(Ok(()), Err))
    }

    fn poll_tokens(&mut self) -> impl Future<Output = Result<AuthPoll, AuthError>> + Send {
        let mut script = self.script.lock().unwrap();
        script.polls += 1;
        if let Some(error) = script.poll_error.take() {
            return ready(Err(error));
        }
        ready(Ok(if script.pending_polls > 0 {
            script.pending_polls -= 1;
            AuthPoll::Pending { challenge: None }
        } else {
            AuthPoll::Approved(Tokens {
                account_name: ACCOUNT.to_owned(),
                refresh_token: RefreshToken::new(REFRESH),
            })
        }))
    }

    fn poll_interval(&self) -> Duration {
        Duration::ZERO
    }

    fn logon(
        &mut self,
        account_name: &str,
        refresh_token: &RefreshToken,
    ) -> impl Future<Output = Result<Shared, AuthError>> + Send {
        let mut script = self.script.lock().unwrap();
        script
            .logons
            .push((account_name.to_owned(), refresh_token.expose().to_owned()));
        let gate = script.gate.clone();
        ready(
            script
                .logon_error
                .take()
                .map_or_else(|| Ok(Shared(Arc::clone(&self.session), gate)), Err),
        )
    }
}

impl SteamHost for FakeHost {
    type Connection = FakeConnection;
    type Store = InMemoryStore;

    fn connection(&self) -> FakeConnection {
        self.script.lock().unwrap().connections += 1;
        FakeConnection {
            script: Arc::clone(&self.script),
            session: Arc::clone(&self.session),
        }
    }

    fn store(&self) -> &InMemoryStore {
        &self.store
    }
}

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: TempDir::new().unwrap(),
        }
    }

    fn data(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    fn core(&self) -> Core {
        Core::new(self.data()).with_controllers(Vec::new())
    }

    fn signed_in(&self) {
        fs::create_dir_all(self.data()).unwrap();
        self.db().meta_set(META_ACCOUNT, ACCOUNT).unwrap();
    }

    fn db(&self) -> ConfigDb {
        applejack_core::config::open_db(&self.data()).unwrap()
    }

    fn library(&self) -> PathBuf {
        self.data().join("library")
    }

    fn record(&self) -> PathBuf {
        self.library()
            .join("applejack")
            .join(format!("{APP}.download.json"))
    }

    fn acf(&self) -> PathBuf {
        self.library()
            .join("steamapps")
            .join(format!("appmanifest_{APP}.acf"))
    }

    fn install(&self, host: &FakeHost) -> Result<applejack_core::config::AppRow, CoreError> {
        install_in(&self.data(), host)
    }
}

fn request() -> TitleInstall {
    TitleInstall {
        appid: APP,
        ..TitleInstall::default()
    }
}

fn install_in(data: &Path, host: &FakeHost) -> Result<applejack_core::config::AppRow, CoreError> {
    Core::new(data)
        .with_controllers(Vec::new())
        .plan_title_install(request())?
        .run(host, &|_, _, _, _| {})
}

fn credentials() -> Credentials {
    Credentials {
        account_name: ACCOUNT.to_owned(),
        password: Password::new(PASSWORD),
    }
}

fn start_login(fixture: &Fixture, host: FakeHost) -> (Core, Arc<FakeHost>, LoginFlow<FakeHost>) {
    let mut core = fixture.core();
    let host = Arc::new(host);
    let flow = LoginFlow::start(
        &mut core,
        Arc::clone(&host),
        AuthMode::Credentials,
        Some(&credentials()),
    )
    .unwrap();
    (core, host, flow)
}

/// Red if a blank name reaches Steam: a harness did that, in a loop, up to 158
/// times. The fake takes any name, so only the refusal stops this.
#[test]
fn a_blank_account_name_is_refused_before_any_connection() {
    let fixture = Fixture::new();
    for blank in ["", "   ", " \t\r\n"] {
        let host = Arc::new(guarded(0));
        let credentials = Credentials {
            account_name: blank.to_owned(),
            password: Password::new(PASSWORD),
        };
        let error = LoginFlow::start(
            &mut fixture.core(),
            Arc::clone(&host),
            AuthMode::Credentials,
            Some(&credentials),
        )
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidParams, "{error}");
        assert_eq!(host.script.lock().unwrap().connections, 0, "{blank:?}");
    }
}

fn guarded(pending_polls: usize) -> FakeHost {
    FakeHost::new(
        Script {
            guard: Some(GuardFactor::DeviceCode),
            pending_polls,
            ..Script::default()
        },
        session(ONE_ENTRY, false),
    )
}

fn stored_token(host: &FakeHost) -> Option<String> {
    host.store
        .get()
        .unwrap()
        .map(|token| token.expose().to_owned())
}

/// Path one of the race: the phone. Red if `advance` does not poll while a
/// code is awaited.
#[test]
fn approving_on_the_phone_finishes_a_login_waiting_for_a_code() {
    let fixture = Fixture::new();
    let (mut core, host, mut flow) = start_login(&fixture, guarded(1));
    let prompt = LoginStep::GuardRequired {
        factor: GuardFactor::DeviceCode,
    };
    assert_eq!(flow.step(), &prompt);
    assert_eq!(flow.advance(&mut core, None).unwrap(), prompt);
    assert!(matches!(
        flow.advance(&mut core, None).unwrap(),
        LoginStep::Done { .. }
    ));
    assert_eq!(stored_token(&host).as_deref(), Some(REFRESH));
    assert_eq!(
        fixture.db().meta_get(META_ACCOUNT).unwrap().as_deref(),
        Some(ACCOUNT)
    );
    assert_eq!(host.script.lock().unwrap().submitted.len(), 0);
}

/// Path two: the keyboard. And a code typed after the phone won goes nowhere,
/// the other half of the race; `live_session.rs` has the `DuplicateRequest`
/// half on the wire.
#[test]
fn typing_the_code_finishes_it_and_a_late_code_is_dropped() {
    let fixture = Fixture::new();
    let (mut core, host, mut flow) = start_login(&fixture, guarded(0));
    let step = flow
        .advance(&mut core, Some(&format!(" {CODE} \n")))
        .unwrap();
    assert!(matches!(step, LoginStep::Done { .. }));
    assert_eq!(host.script.lock().unwrap().submitted, [CODE]);

    let step = flow.advance(&mut core, Some("LATE1")).unwrap();
    assert!(matches!(step, LoginStep::Done { .. }));
    assert_eq!(host.script.lock().unwrap().submitted, [CODE]);
}

#[test]
fn a_wrong_code_leaves_the_prompt_up_and_polling_goes_on() {
    let fixture = Fixture::new();
    let host = guarded(0);
    host.script.lock().unwrap().submit_error = Some(AuthError::InvalidGuardCode);
    let (mut core, host, mut flow) = start_login(&fixture, host);
    let error = flow.advance(&mut core, Some("WRONG")).unwrap_err();
    assert_eq!(error.code(), ErrorCode::SteamGuardRejected);
    assert_eq!(stored_token(&host), None);
    assert!(matches!(
        flow.advance(&mut core, None).unwrap(),
        LoginStep::Done { .. }
    ));
}

#[test]
fn a_login_to_another_account_drops_the_old_library() {
    let fixture = Fixture::new();
    fixture.signed_in();
    fixture.db().meta_set(META_ACCOUNT, "someone-else").unwrap();
    let host = FakeHost::signed_in(session(ONE_ENTRY, false));
    fixture.core().steam_library_refresh(&host).unwrap();
    assert_eq!(fixture.db().cached_library().unwrap().len(), 1);

    let (mut core, _host, mut flow) = start_login(&fixture, guarded(0));
    flow.advance(&mut core, None).unwrap();
    assert_eq!(fixture.db().cached_library().unwrap().len(), 0);
}

#[test]
fn every_way_a_silent_refresh_fails_needs_or_reports_a_login() {
    let fixture = Fixture::new();
    let host = FakeHost::new(Script::default(), session(ONE_ENTRY, false));
    let error = fixture.core().steam_library_refresh(&host).unwrap_err();
    assert!(
        matches!(error, CoreError::SteamLoginRequired { .. }),
        "{error}"
    );

    fixture.signed_in();
    let error = fixture.core().steam_library_refresh(&host).unwrap_err();
    assert!(
        matches!(error, CoreError::SteamLoginRequired { .. }),
        "{error}"
    );

    host.store.put(&RefreshToken::new(REFRESH)).unwrap();
    host.script.lock().unwrap().logon_error = Some(AuthError::Refused);
    let error = fixture.core().steam_library_refresh(&host).unwrap_err();
    assert!(
        matches!(error, CoreError::SteamLoginRequired { .. }),
        "{error}"
    );
    assert_eq!(error.code(), ErrorCode::SteamLoginRequired);

    host.script.lock().unwrap().logon_error = Some(AuthError::Connection("gone".to_owned()));
    let error = fixture.core().steam_library_refresh(&host).unwrap_err();
    assert!(matches!(error, CoreError::SteamRefreshFailed(_)), "{error}");
    assert_eq!(error.code(), ErrorCode::SteamUnavailable);
    assert_eq!(fixture.db().cached_library().unwrap().len(), 0);
}

#[test]
fn the_library_is_cached_with_drm_unknown_and_refresh_names_the_account() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let host = FakeHost::signed_in(session(ONE_ENTRY, false));
    let listing = fixture.core().steam_library_refresh(&host).unwrap();
    assert_eq!(listing.apps.len(), 1);
    let app = &listing.apps[0];
    assert_eq!(app.appid, APP);
    assert_eq!(app.name, "Fixture Title");
    assert_eq!(app.installdir, "FixtureTitle");
    assert_eq!(app.size_bytes, 4096);
    assert_eq!(app.drm, Drm::Unknown);
    assert_eq!(app.default_branch, "public");
    assert_eq!(app.install_state, InstallState::NotInstalled);
    assert_eq!(
        host.script.lock().unwrap().logons,
        [(ACCOUNT.to_owned(), REFRESH.to_owned())]
    );
    let cached = fixture.core().steam_library_cached().unwrap();
    assert_eq!(cached.apps, listing.apps);
    assert!(cached.fetched_at.is_some());
}

/// Red if logout leaves rows, the account, or takes an installed title.
#[test]
fn logout_clears_the_token_the_cache_and_the_account_and_keeps_what_is_on_disk() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let host = FakeHost::signed_in(session(ONE_ENTRY, false));
    fixture.core().steam_library_refresh(&host).unwrap();
    fixture.install(&host).unwrap();
    fixture.core().config_set(APP, "drm", "none").unwrap();
    fixture
        .db()
        .upsert_app(&applejack_core::config::AppRow {
            appid: APP + 1,
            fetched_at: Some("then".to_owned()),
            ..applejack_core::config::AppRow::default()
        })
        .unwrap();

    fixture.core().steam_logout(&host).unwrap();
    assert_eq!(stored_token(&host), None);
    let db = fixture.db();
    assert_eq!(db.meta_get(META_ACCOUNT).unwrap(), None);
    assert_eq!(db.cached_library().unwrap().len(), 0);
    assert_eq!(db.app(APP + 1).unwrap(), None);
    let kept = db.app(APP).unwrap().unwrap();
    assert_eq!(kept.drm, Drm::None);
    assert_eq!(kept.launch_exe.as_deref(), Some("bin/title.exe"));
    assert!(fixture.acf().exists());
    let error = fixture.core().steam_library_refresh(&host).unwrap_err();
    assert!(matches!(error, CoreError::SteamLoginRequired { .. }));
}

#[test]
fn an_install_writes_the_manifest_the_row_and_the_launch_exe() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let host = FakeHost::signed_in(session(ONE_ENTRY, false));
    let row = fixture.install(&host).unwrap();
    assert!(fixture.acf().exists());
    assert!(!fixture.record().exists());
    assert_eq!(row.name.as_deref(), Some("Fixture Title"));
    assert_eq!(row.installdir.as_deref(), Some("FixtureTitle"));
    assert_eq!(row.drm, Drm::Unknown, "nothing here marks a title none");
    assert_eq!(row.last_installed_buildid, Some(55));
    assert_eq!(
        row.last_manifest_ids
            .get(&DEPOT.to_string())
            .map(String::as_str),
        Some(MANIFEST.to_string().as_str())
    );
    assert_eq!(row.launch_exe.as_deref(), Some("bin/title.exe"));
    assert_eq!(row.launch_args.as_deref(), Some("-windowed"));
    let library = fs::canonicalize(fixture.library()).unwrap();
    assert_eq!(row.library_dir.as_deref(), library.to_str());
    assert_eq!(fixture.db().app(APP).unwrap(), Some(row));
    assert!(
        fixture
            .library()
            .join("steamapps/common/FixtureTitle/bin/title.exe")
            .exists()
    );
}

/// A second library for a title on disk elsewhere: the row must stop calling
/// the title whole while that install is unfinished.
#[test]
fn an_install_into_a_new_library_is_unfinished_until_it_finishes() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let host = FakeHost::signed_in(session(ONE_ENTRY, false));
    fixture.install(&host).unwrap();
    let elsewhere = fixture.dir.path().join("elsewhere");
    let cancel = TitleInstall {
        library: Some(elsewhere.clone()),
        ..request()
    };
    let plan = fixture.core().plan_title_install(cancel).unwrap();
    let host = FakeHost::signed_in(session(ONE_ENTRY, true));
    let record = elsewhere.join(format!("applejack/{APP}.download.json"));
    let error = std::thread::scope(|scope| {
        let job = scope.spawn(|| plan.run(&host, &|_, _, _, _| {}).unwrap_err());
        wait_for("the download record", || record.exists());
        fixture.core().cancel_install(APP).unwrap();
        job.join().unwrap()
    });
    assert_eq!(error.code(), ErrorCode::InstallCancelled, "{error}");
    let row = fixture.db().app(APP).unwrap().unwrap();
    assert_eq!(row.last_installed_buildid, None);
    assert!(row.library_dir.unwrap().ends_with("elsewhere"));
}

/// The refusal names the route; the route works; the run then goes as far as
/// it can without an engine. Red if `set drm` or the message changes.
#[test]
fn a_new_title_is_refused_with_the_route_to_a_client_free_run() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let host = FakeHost::signed_in(session(ONE_ENTRY, false));
    fixture.install(&host).unwrap();
    let error = fixture.core().plan_run(APP, false).unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientRuntimeUnavailable);
    let text = error.to_string();
    assert!(
        text.contains(&format!("applejack run {APP} --probe-direct")),
        "{text}"
    );
    assert!(
        text.contains(&format!("applejack config {APP} set drm none")),
        "{text}"
    );

    fixture.core().config_set(APP, "drm", "none").unwrap();
    let error = fixture.core().plan_run(APP, false).unwrap_err();
    assert!(matches!(error, CoreError::NoActiveEngine { .. }), "{error}");
}

#[test]
fn drm_takes_only_known_values_and_only_on_a_known_title() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let error = fixture.core().config_set(APP, "drm", "none").unwrap_err();
    assert_eq!(error.code(), ErrorCode::TitleNotInstalled);
    let host = FakeHost::signed_in(session(ONE_ENTRY, false));
    fixture.install(&host).unwrap();
    let error = fixture.core().config_set(APP, "drm", "maybe").unwrap_err();
    assert_eq!(error.code(), ErrorCode::InvalidParams);
    assert_eq!(fixture.db().app(APP).unwrap().unwrap().drm, Drm::Unknown);
}

#[test]
fn several_launch_entries_refuse_before_a_byte_arrives_and_the_override_gets_past() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let session = session(TWO_ENTRIES, false);
    let host = FakeHost::signed_in(Arc::clone(&session));
    let error = fixture.install(&host).unwrap_err();
    assert_eq!(error.code(), ErrorCode::LaunchExeAmbiguous);
    let text = error.to_string();
    assert!(text.contains("a.exe") && text.contains("b.exe"), "{text}");
    assert!(text.contains("launch_exe_override"), "{text}");
    assert_eq!(session.chunks.fetches(), 0);
    assert!(!fixture.record().exists());
    assert_eq!(fixture.db().app(APP).unwrap(), None);

    fixture
        .core()
        .config_set(APP, "launch_exe_override", "a.exe")
        .unwrap();
    let row = fixture.install(&host).unwrap();
    assert_eq!(row.launch_exe, None);
    assert_eq!(row.last_installed_buildid, Some(55));
}

#[test]
fn no_windows_launch_entry_is_refused_naming_the_override() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let linux_only = r#""0" { "executable" "run.sh" "config" { "oslist" "linux" } }"#;
    let host = FakeHost::signed_in(session(linux_only, false));
    let error = fixture.install(&host).unwrap_err();
    assert_eq!(error.code(), ErrorCode::NoLaunchExe);
    assert!(error.to_string().contains("launch_exe_override"), "{error}");
}

#[test]
fn a_title_the_client_launches_needs_no_launch_entry() {
    let fixture = Fixture::new();
    fixture.signed_in();
    fixture
        .core()
        .config_set(APP, "launch_mode", "client")
        .unwrap();
    let host = FakeHost::signed_in(session(TWO_ENTRIES, false));
    assert!(fixture.install(&host).is_ok());
}

#[test]
fn a_title_on_disk_is_refused_before_any_login_and_update_and_verify_take_it() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let host = FakeHost::signed_in(session(ONE_ENTRY, false));
    fixture.install(&host).unwrap();
    let logons = host.script.lock().unwrap().logons.len();
    let error = fixture.install(&host).unwrap_err();
    assert_eq!(error.code(), ErrorCode::AlreadyInstalled);
    assert_eq!(host.script.lock().unwrap().logons.len(), logons);

    let update = TitleInstall {
        update: true,
        ..request()
    };
    let row = fixture
        .core()
        .plan_title_install(update)
        .unwrap()
        .run(&host, &|_, _, _, _| {})
        .unwrap();
    assert_eq!(row.last_installed_buildid, Some(55));

    let verify = TitleInstall {
        verify: true,
        ..request()
    };
    let plan = fixture.core().plan_title_install(verify.clone()).unwrap();
    plan.run(&host, &|_, _, _, _| {}).unwrap();
    assert_eq!(
        host.script.lock().unwrap().logons.len(),
        logons + 1,
        "verify is offline"
    );

    fs::write(
        fixture
            .library()
            .join("steamapps/common/FixtureTitle/data.bin"),
        b"edited",
    )
    .unwrap();
    let error = fixture
        .core()
        .plan_title_install(verify)
        .unwrap()
        .run(&host, &|_, _, _, _| {})
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::VerifyFailed);
    assert!(error.to_string().contains("data.bin"), "{error}");
}

fn run_install(
    fixture: &Fixture,
    host: &FakeHost,
    request: TitleInstall,
) -> Result<applejack_core::config::AppRow, CoreError> {
    fixture
        .core()
        .plan_title_install(request)?
        .run(host, &|_, _, _, _| {})
}

fn update() -> TitleInstall {
    TitleInstall {
        update: true,
        ..request()
    }
}

/// The lines of an install, as `phase: message`.
fn reported(
    fixture: &Fixture,
    host: &FakeHost,
    request: TitleInstall,
) -> (
    Vec<String>,
    Result<applejack_core::config::AppRow, CoreError>,
) {
    let lines = Mutex::new(Vec::new());
    let outcome = fixture.core().plan_title_install(request).and_then(|plan| {
        plan.run(host, &|phase, _, _, message| {
            lines.lock().unwrap().push(format!("{phase}: {message}"));
        })
    });
    (lines.into_inner().unwrap(), outcome)
}

/// Red without the line: a wrong pick shows only when a run launches it, and
/// the app info rule has never met real data.
#[test]
fn an_install_says_which_exe_it_chose() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let host = FakeHost::signed_in(session(ONE_ENTRY, false));
    let (lines, outcome) = reported(&fixture, &host, request());
    outcome.unwrap();
    assert!(
        lines.contains(&"register: launch exe: bin/title.exe -windowed".to_owned()),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("warning")),
        "{lines:?}"
    );
}

#[test]
fn an_install_names_an_override_and_a_title_the_client_launches() {
    let fixture = Fixture::new();
    fixture.signed_in();
    fixture
        .core()
        .config_set(APP, "launch_exe_override", "bin/title.exe")
        .unwrap();
    let host = FakeHost::signed_in(session(TWO_ENTRIES, false));
    let (lines, outcome) = reported(&fixture, &host, request());
    outcome.unwrap();
    assert!(
        lines.contains(&"register: launch exe: bin/title.exe (launch_exe_override)".to_owned()),
        "{lines:?}"
    );
}

/// The title is whole, so this is a warning and the way out, not a failure.
#[test]
fn a_chosen_exe_that_cannot_run_is_reported_at_register() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let missing = r#""0" { "executable" "gone.exe" "type" "default" }"#;
    let host = FakeHost::signed_in(session(missing, false));
    let (lines, outcome) = reported(&fixture, &host, request());
    let row = outcome.unwrap();
    assert_eq!(row.launch_exe.as_deref(), Some("gone.exe"));
    let warning = lines.iter().find(|line| line.contains("warning")).unwrap();
    assert!(warning.contains("set launch_exe_override"), "{warning}");
}

/// App info text reaches terminals; a name must not drive one.
#[test]
fn app_info_text_is_made_printable_on_the_install_path() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let hostile = session(ONE_ENTRY, false);
    {
        let mut answers = hostile.app_info.lock().unwrap();
        let info = &mut answers[0][0];
        let text = info.text.take().unwrap();
        info.text = Some(text.replace("Fixture Title", "Fixture\u{1b}[2J Title"));
    }
    let host = FakeHost::signed_in(hostile);
    let (lines, outcome) = reported(&fixture, &host, request());
    outcome.unwrap();
    assert!(lines.iter().any(|line| line.contains("Fixture[2J Title")));
    assert!(
        !lines.iter().any(|line| line.contains('\u{1b}')),
        "{lines:?}"
    );

    let fixture = Fixture::new();
    fixture.signed_in();
    let entries = "\"0\" { \"executable\" \"a\u{1b}[2J.exe\" } \"1\" { \"executable\" \"b.exe\" }";
    let host = FakeHost::signed_in(session(entries, false));
    let error = fixture.install(&host).unwrap_err();
    assert_eq!(error.code(), ErrorCode::LaunchExeAmbiguous);
    assert!(!error.to_string().contains('\u{1b}'), "{error}");
}

/// A lone entry the filter left is refused through the install, naming the
/// way out, before a byte arrives.
#[test]
fn a_lone_entry_behind_a_lost_default_refuses_the_install() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let entries = r#""0" { "executable" "beta.exe" "type" "default" "config" { "betakey" "beta" } }
                     "1" { "executable" "bench.exe" "type" "option1" }"#;
    let host = FakeHost::signed_in(session(entries, false));
    let error = fixture.install(&host).unwrap_err();
    assert_eq!(error.code(), ErrorCode::LaunchExeAmbiguous);
    assert!(error.to_string().contains("launch_exe_override"), "{error}");
    assert!(error.to_string().contains("bench.exe"), "{error}");
    assert!(
        error.to_string().contains("launch_args_override"),
        "{error}"
    );
    assert!(!fixture.acf().exists());
}

/// Red if `verify` reads the old `.acf` and says nothing: "2 files match" on
/// a title whose run is refused reads as whole.
#[test]
fn verify_over_a_stopped_update_says_so() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let host = FakeHost::signed_in(session(ONE_ENTRY, false));
    fixture.install(&host).unwrap();
    let verify = |fixture: &Fixture| {
        let lines = Mutex::new(Vec::new());
        let request = TitleInstall {
            verify: true,
            ..request()
        };
        fixture
            .core()
            .plan_title_install(request)
            .unwrap()
            .run(&host, &|_, _, _, line| lines.lock().unwrap().push(line))
            .unwrap();
        lines.into_inner().unwrap().join("\n")
    };
    let whole = verify(&fixture);
    assert!(!whole.contains("--update"), "{whole}");

    fs::create_dir_all(fixture.record().parent().unwrap()).unwrap();
    fs::write(fixture.record(), "{}").unwrap();
    let stopped = verify(&fixture);
    assert!(
        stopped.contains(&format!("applejack install {APP} --update")),
        "{stopped}"
    );
    assert!(stopped.contains("2 files match"), "{stopped}");
}

/// Red if a plain install reads the `.acf` alone: `already installed` is a
/// dead end when only `--update` finishes this.
#[test]
fn a_stopped_update_is_sent_to_update() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let host = FakeHost::signed_in(session(ONE_ENTRY, false));
    fixture.install(&host).unwrap();
    fs::create_dir_all(fixture.record().parent().unwrap()).unwrap();
    fs::write(fixture.record(), "{}").unwrap();

    let refused = fixture.install(&host).unwrap_err();
    assert_eq!(refused.code(), ErrorCode::DownloadInProgress, "{refused}");
    assert!(
        refused
            .to_string()
            .contains(&format!("applejack install {APP} --update")),
        "{refused}"
    );

    let run = fixture.core().plan_run(APP, true).map(|_| ()).unwrap_err();
    assert_eq!(run.code(), ErrorCode::DownloadInProgress, "{run}");
    assert!(run.to_string().contains("--update"), "{run}");
}

#[test]
fn verifying_a_manifest_with_no_depots_is_refused() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let host = FakeHost::signed_in(session(ONE_ENTRY, false));
    fixture.install(&host).unwrap();
    let text = fs::read_to_string(fixture.acf()).unwrap();
    let start = text.find("\"InstalledDepots\"").unwrap();
    let end = start + text[start..].find('}').unwrap();
    let end = end + 1 + text[end + 1..].find('}').unwrap();
    fs::write(
        fixture.acf(),
        format!(
            "{}\"InstalledDepots\" {{ }}{}",
            &text[..start],
            &text[end + 1..]
        ),
    )
    .unwrap();
    let verify = TitleInstall {
        verify: true,
        ..request()
    };
    let error = run_install(&fixture, &host, verify).unwrap_err();
    assert_eq!(error.code(), ErrorCode::VerifyFailed, "{error}");
    assert!(error.to_string().contains("no depots"), "{error}");
}

/// Red if the row moves before the installer's refusals: a working copy in
/// the old library would be unrunnable after an install that wrote nothing.
#[test]
fn a_refusal_inside_the_installer_leaves_the_row_alone() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let host = FakeHost::signed_in(session(ONE_ENTRY, false));
    let row = fixture.install(&host).unwrap();
    let elsewhere = TempDir::new().unwrap();
    let library = fs::canonicalize(elsewhere.path()).unwrap();
    let plan = fixture
        .core()
        .plan_title_install(TitleInstall {
            library: Some(library.clone()),
            ..request()
        })
        .unwrap();
    // Lands between the plan's look and the installer's, as a second copy
    // of the library or a client would.
    fs::create_dir_all(library.join("steamapps")).unwrap();
    fs::copy(
        fixture.acf(),
        library.join(format!("steamapps/appmanifest_{APP}.acf")),
    )
    .unwrap();
    let error = plan.run(&host, &|_, _, _, _| {}).unwrap_err();
    assert_eq!(error.code(), ErrorCode::AlreadyInstalled, "{error}");
    assert_eq!(fixture.db().app(APP).unwrap().unwrap(), row);
}

/// Red if `--update` falls back on the default branch: the title would move
/// without a word.
#[test]
fn an_update_keeps_the_branch_the_title_came_from() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let host = FakeHost::signed_in(branched_session());
    let beta = TitleInstall {
        branch: Some("beta".to_owned()),
        ..request()
    };
    let row = run_install(&fixture, &host, beta).unwrap();
    assert_eq!(row.installed_branch.as_deref(), Some("beta"));
    assert_eq!(row.last_installed_buildid, Some(66));

    let row = run_install(&fixture, &host, update()).unwrap();
    assert_eq!(row.installed_branch.as_deref(), Some("beta"));
    assert_eq!(row.last_installed_buildid, Some(66));
    let data = fixture
        .library()
        .join("steamapps/common/FixtureTitle/data.bin");
    assert_eq!(fs::read(&data).unwrap(), b"beta data");

    let public = TitleInstall {
        branch: Some("public".to_owned()),
        ..update()
    };
    let row = run_install(&fixture, &host, public).unwrap();
    assert_eq!(row.installed_branch.as_deref(), Some("public"));
    assert_eq!(row.last_installed_buildid, Some(55));
    assert_eq!(fs::read(&data).unwrap(), b"public data");
}

/// `latest_buildid` is the default branch's: another branch's build differs
/// from it for good.
#[test]
fn a_title_on_another_branch_does_not_read_as_out_of_date() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let host = FakeHost::signed_in(branched_session());
    fixture.core().steam_library_refresh(&host).unwrap();
    let beta = TitleInstall {
        branch: Some("beta".to_owned()),
        ..request()
    };
    run_install(&fixture, &host, beta).unwrap();
    let listed = fixture.core().steam_library_cached().unwrap();
    assert_eq!(listed.apps[0].install_state, InstallState::Installed);
}

/// Red if the row keeps the cached build: a title just installed from a
/// build newer than the cache read as `update_available`.
#[test]
fn a_fresh_install_does_not_read_as_out_of_date() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let host = FakeHost::signed_in(session(ONE_ENTRY, false));
    fixture.core().steam_library_refresh(&host).unwrap();
    let db = fixture.db();
    let mut row = db.app(APP).unwrap().unwrap();
    row.latest_buildid = Some(54);
    db.upsert_app(&row).unwrap();
    fixture.install(&host).unwrap();
    assert_eq!(db.app(APP).unwrap().unwrap().latest_buildid, Some(55));
    let listed = fixture.core().steam_library_cached().unwrap();
    assert_eq!(listed.apps[0].install_state, InstallState::Installed);
}

#[test]
fn update_and_verify_together_are_refused() {
    let fixture = Fixture::new();
    let error = fixture
        .core()
        .plan_title_install(TitleInstall {
            update: true,
            verify: true,
            ..request()
        })
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::InvalidParams);
}

#[test]
fn an_install_refuses_an_unsigned_in_host_with_the_login_code() {
    let fixture = Fixture::new();
    let host = FakeHost::new(Script::default(), session(ONE_ENTRY, false));
    let error = fixture.install(&host).unwrap_err();
    assert_eq!(error.code(), ErrorCode::SteamLoginRequired);
    assert!(!fixture.record().exists());
}

fn open_session(fixture: &Fixture) {
    fixture
        .db()
        .insert_session(&NewSession {
            appid: APP,
            started_at: &utc_now(),
            engine_id: "rosetta-wine",
            engine_version: "1",
            log_path: "/tmp/x",
            session_key: "session-with-nothing-running",
            prefix: "7000",
            launch_exe: "title.exe",
        })
        .unwrap();
}

/// An open row with a free lock is a dead owner's, and settles; a live one
/// is `app_session.rs`'s, through real processes.
#[test]
fn a_dead_sessions_row_does_not_hold_an_install_back() {
    let fixture = Fixture::new();
    fixture.signed_in();
    open_session(&fixture);
    let host = FakeHost::signed_in(session(ONE_ENTRY, false));
    fixture.install(&host).unwrap();
    assert_eq!(fixture.db().open_session(APP).unwrap(), None);
}

/// The other process, which a failed assertion must not leave behind.
struct Helper(Child);

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn helper(name: &str, data: &Path) -> Helper {
    Helper(
        Command::new(env::current_exe().unwrap())
            .args([name, "--exact", "--ignored", "--nocapture"])
            .env(HELPER_DATA, data)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

const HELPER_DATA: &str = "APPLEJACK_TEST_INSTALL_DATA";

/// Not a test: the other process of the tests below. It installs a title
/// whose second file never arrives, until it is killed or told to stop.
#[test]
#[ignore = "run by the tests that spawn it"]
fn helper_install_that_never_finishes() {
    let Some(data) = env::var_os(HELPER_DATA) else {
        return;
    };
    let host = FakeHost::signed_in(session(ONE_ENTRY, true));
    let error = install_in(Path::new(&data), &host).unwrap_err();
    assert_eq!(error.code(), ErrorCode::InstallCancelled, "{error}");
}

fn wait_for(what: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A SIGKILL mid-download: the record outlives the process, a run in another
/// process refuses on it, the locks are free, and the next install resumes
/// and finishes. Red if the run stops checking the record, or a new install
/// starts from nothing.
#[test]
fn a_killed_install_blocks_runs_and_resumes() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let mut child = helper("helper_install_that_never_finishes", &fixture.data());
    let exe = fixture
        .library()
        .join("steamapps/common/FixtureTitle/bin/title.exe");
    wait_for("the first file", || {
        fixture.record().exists() && exe.exists()
    });

    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let error = fixture.core().plan_run(APP, true).unwrap_err();
    assert_eq!(error.code(), ErrorCode::DownloadInProgress);
    assert!(error.to_string().contains("applejack install"), "{error}");

    let session = session(ONE_ENTRY, false);
    let host = FakeHost::signed_in(Arc::clone(&session));
    let row = fixture.install(&host).unwrap();
    assert_eq!(row.last_installed_buildid, Some(55));
    assert_eq!(
        session.chunks.fetches(),
        1,
        "the finished file is not fetched again"
    );
    assert!(!fixture.record().exists());
    assert!(fixture.acf().exists());
    let error = fixture.core().plan_run(APP, true).unwrap_err();
    assert!(matches!(error, CoreError::NoActiveEngine { .. }), "{error}");
}

/// Both directions of the job lock, from another process, while nothing but
/// the lock says so: a second install, a run, and a cancel. Red if the
/// install takes no prefix lock, or the cancel file is not read.
#[test]
fn a_running_install_holds_the_title_against_other_processes_until_cancelled() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let mut child = helper("helper_install_that_never_finishes", &fixture.data());
    wait_for("the download record", || fixture.record().exists());

    let error = fixture.core().plan_title_install(request()).unwrap_err();
    assert_eq!(error.code(), ErrorCode::LockHeld, "{error}");
    let error = fixture.core().plan_run(APP, true).unwrap_err();
    assert_eq!(error.code(), ErrorCode::DownloadInProgress, "{error}");

    assert!(fixture.core().cancel_install(APP).unwrap());
    let status = child.0.wait().unwrap();
    assert!(status.success(), "the helper did not see InstallCancelled");
    assert!(fixture.record().exists(), "a stop keeps the record");
    assert!(!fixture.core().cancel_install(APP).unwrap());
}

/// Two logged-in sessions of one account displace each other. Red without
/// the `steam-session` lock.
#[test]
fn a_second_session_is_refused_at_once_naming_the_lock() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let mut child = helper("helper_install_that_never_finishes", &fixture.data());
    wait_for("the download record", || fixture.record().exists());
    let host = FakeHost::signed_in(session(ONE_ENTRY, false));
    let error = fixture.core().steam_library_refresh(&host).unwrap_err();
    assert_eq!(error.code(), ErrorCode::LockHeld, "{error}");
    assert!(error.to_string().contains("steam-session"), "{error}");
    assert_eq!(host.script.lock().unwrap().logons.len(), 0);
    fixture.core().cancel_install(APP).unwrap();
    child.0.wait().unwrap();
    assert!(fixture.core().steam_library_refresh(&host).is_ok());
}

/// The ownership rule: a server up in the client's prefix makes the library
/// read-only. Red if the plan stops asking.
#[test]
fn an_install_refuses_while_a_windows_client_runs() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let prefix = fixture.data().join("prefixes/steam-client");
    fs::create_dir_all(&prefix).unwrap();
    let mut server = common::spawn_stub_server("client-under-test", &prefix);
    let error = fixture.core().plan_title_install(request()).unwrap_err();
    assert_eq!(error.code(), ErrorCode::SteamClientOwnsTitle, "{error}");
    fs::write(prefix.join("server-quit"), b"").unwrap();
    server.wait().unwrap();
    assert!(fixture.core().plan_title_install(request()).is_ok());
}

/// The prefix lock, held the way a run holds it: nothing here is an install,
/// so it must read as busy, and with an open row as already running.
#[test]
fn an_install_refuses_while_the_titles_prefix_is_held() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let prefixes = fixture.data().join("prefixes");
    let _held =
        PrefixLock::acquire(&prefixes.join("7000"), &prefixes.join(".snapshots"), "7000").unwrap();
    let error = fixture.core().plan_title_install(request()).unwrap_err();
    assert_eq!(error.code(), ErrorCode::PrefixBusy, "{error}");
    open_session(&fixture);
    let error = fixture.core().plan_title_install(request()).unwrap_err();
    assert_eq!(error.code(), ErrorCode::AlreadyRunning, "{error}");
}

fn notified(lines: &Receiver<String>, wanted: &str) -> Vec<Value> {
    let mut seen = Vec::new();
    loop {
        let line = lines
            .recv_timeout(Duration::from_secs(20))
            .expect("the job ended");
        let value: Value = serde_json::from_str(&line).unwrap();
        let done = value["method"] == json!(wanted);
        seen.push(value);
        if done {
            return seen;
        }
    }
}

fn result_of<T: serde::de::DeserializeOwned>(outcome: Result<Value, RpcError>) -> T {
    serde_json::from_value(outcome.unwrap()).unwrap()
}

fn handler(fixture: &Fixture, host: FakeHost) -> CoreHandler<FakeHost> {
    CoreHandler::with_steam(fixture.core(), host)
}

fn start(
    handler: &mut impl Handler,
    notifier: &Notifier,
    password: &str,
) -> Result<Value, RpcError> {
    handler.handle(
        &Method::SteamLoginStart(SteamLoginStartParams {
            mode: LoginMode::Credentials {
                account_name: ACCOUNT.to_owned(),
                password: password.to_owned(),
            },
        }),
        notifier,
    )
}

fn complete(
    handler: &mut impl Handler,
    notifier: &Notifier,
    session_id: &str,
    code: Option<&str>,
) -> Result<Value, RpcError> {
    handler.handle(
        &Method::SteamLoginComplete(SteamLoginCompleteParams {
            session_id: session_id.to_owned(),
            steam_guard_code: code.map(str::to_owned),
        }),
        notifier,
    )
}

#[test]
fn login_over_rpc_polls_between_requests_and_takes_either_path() {
    let fixture = Fixture::new();
    let (notifier, _lines) = Notifier::channel();
    let mut handler = handler(&fixture, guarded(1));

    let started: LoginSession = result_of(start(&mut handler, &notifier, PASSWORD));
    assert!(matches!(started.step, LoginStepWire::GuardRequired { .. }));
    let polled: LoginSession =
        result_of(complete(&mut handler, &notifier, &started.session_id, None));
    assert!(matches!(polled.step, LoginStepWire::GuardRequired { .. }));
    let done: LoginSession =
        result_of(complete(&mut handler, &notifier, &started.session_id, None));
    assert_eq!(
        done.step,
        LoginStepWire::Done {
            account_name: ACCOUNT.to_owned()
        }
    );
    let gone = complete(&mut handler, &notifier, &started.session_id, None).unwrap_err();
    assert_eq!(gone.code, ErrorCode::SteamLoginFailed);

    let (notifier, _lines) = Notifier::channel();
    let mut wrong = self::handler(&fixture, guarded(0));
    result_of::<LoginSession>(start(&mut wrong, &notifier, PASSWORD));
    let stray = complete(&mut wrong, &notifier, "login-someone-else", Some(CODE)).unwrap_err();
    assert_eq!(stray.code, ErrorCode::SteamLoginFailed);
}

#[test]
fn a_wrong_code_over_rpc_answers_its_code_and_the_login_goes_on() {
    let fixture = Fixture::new();
    let (notifier, _lines) = Notifier::channel();
    let host = guarded(0);
    host.script.lock().unwrap().submit_error = Some(AuthError::InvalidGuardCode);
    let mut handler = handler(&fixture, host);
    let started: LoginSession = result_of(start(&mut handler, &notifier, PASSWORD));
    let id = &started.session_id;
    let error = complete(&mut handler, &notifier, id, Some("WRONG")).unwrap_err();
    assert_eq!(error.code, ErrorCode::SteamGuardRejected);
    let done: LoginSession = result_of(complete(&mut handler, &notifier, id, Some(CODE)));
    assert!(matches!(done.step, LoginStepWire::Done { .. }));
}

/// Everything secret goes through a real handler and a real log. Red if a
/// `steam.login.*` line stops going through `redact`.
#[test]
fn nothing_secret_reaches_the_log() {
    let fixture = Fixture::new();
    let (notifier, _lines) = Notifier::channel();
    let host = guarded(0);
    let mut logged = Logged::new(handler(&fixture, host), Vec::new());

    let refused = start(&mut logged, &notifier, "fixture-wrong-password").unwrap_err();
    assert_eq!(refused.code, ErrorCode::SteamLoginFailed);
    let started: LoginSession = result_of(start(&mut logged, &notifier, PASSWORD));
    let done: LoginSession = result_of(complete(
        &mut logged,
        &notifier,
        &started.session_id,
        Some(CODE),
    ));
    assert!(matches!(done.step, LoginStepWire::Done { .. }));
    let qr = Method::SteamLoginStart(SteamLoginStartParams {
        mode: LoginMode::Qr,
    });
    let answer = logged.handle(&qr, &notifier).unwrap();
    assert_eq!(answer["step"]["payload"], json!(QR));

    let (_, sink) = logged.into_parts();
    let log = String::from_utf8(sink).unwrap();
    for secret in [
        PASSWORD,
        "fixture-wrong-password",
        CODE,
        REFRESH,
        ACCOUNT,
        QR,
    ] {
        assert!(!log.contains(secret), "`{secret}` reached the log:\n{log}");
    }
    assert!(log.contains("steam.login.start"), "{log}");
    assert!(log.contains(REDACTED), "{log}");
}

#[test]
fn a_store_holds_the_token_and_no_row_or_file_does() {
    let fixture = Fixture::new();
    let (mut core, host, mut flow) = start_login(&fixture, guarded(0));
    flow.advance(&mut core, None).unwrap();
    assert_eq!(stored_token(&host).as_deref(), Some(REFRESH));
    let db = fs::read(fixture.data().join("db.sqlite")).unwrap();
    let wal = fs::read(fixture.data().join("db.sqlite-wal")).unwrap_or_default();
    for bytes in [db, wal] {
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains(REFRESH));
        assert!(!text.contains(PASSWORD));
    }
}

#[test]
fn the_library_over_rpc_answers_the_cache_and_refreshes_as_a_job() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let (notifier, lines) = Notifier::channel();
    let host = FakeHost::signed_in(session(ONE_ENTRY, false));
    let mut handler = handler(&fixture, host);
    let library = |refresh| Method::SteamLibrary(SteamLibraryParams { refresh });

    let cached: SteamLibraryResult = result_of(handler.handle(&library(false), &notifier));
    assert_eq!(cached.apps.len(), 0);
    assert_eq!(cached.job_id, None);

    let started: SteamLibraryResult = result_of(handler.handle(&library(true), &notifier));
    assert!(started.job_id.is_some());
    assert_eq!(started.apps.len(), 0, "the answer is the cache as it stood");
    notified(&lines, "job.done");

    let cached: SteamLibraryResult = result_of(handler.handle(&library(false), &notifier));
    assert_eq!(cached.apps.len(), 1);
}

/// Red if the fetch writes its rows without looking at the account: logout
/// cleared the cache and the job then put it back.
#[test]
fn a_logout_during_a_refresh_is_not_undone() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let (notifier, lines) = Notifier::channel();
    let (reached, arrived) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let host = FakeHost::signed_in(session(ONE_ENTRY, false));
    host.script.lock().unwrap().gate = Some(Arc::new(Gate {
        reached: Mutex::new(reached),
        release: Mutex::new(released),
    }));
    let mut handler = handler(&fixture, host);
    let refresh = Method::SteamLibrary(SteamLibraryParams { refresh: true });
    handler.handle(&refresh, &notifier).unwrap();
    arrived.recv_timeout(Duration::from_secs(20)).unwrap();

    handler
        .handle(&Method::SteamLogout(SteamLogoutParams {}), &notifier)
        .unwrap();
    release.send(()).unwrap();

    let seen = notified(&lines, "job.failed");
    let failed = seen.last().unwrap();
    assert_eq!(
        failed["params"]["code"],
        json!(ErrorCode::SteamLoginRequired.code()),
        "{failed}"
    );
    let db = fixture.db();
    assert_eq!(db.cached_library().unwrap(), Vec::new());
    assert_eq!(db.meta_get(META_ACCOUNT).unwrap(), None);
}

/// The other account's rows would otherwise land in the new account's cache.
#[test]
fn a_refresh_that_outlasts_a_change_of_account_drops_its_rows() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let db = fixture.db();
    let rows = [applejack_core::config::LibraryRow {
        appid: APP,
        name: "Fixture Title".to_owned(),
        installdir: "FixtureTitle".to_owned(),
        size_bytes: 1,
        default_branch: "public".to_owned(),
        latest_buildid: Some(1),
    }];
    db.meta_set(META_ACCOUNT, "fixture-other-user").unwrap();
    assert!(
        !db.cache_library_for(META_ACCOUNT, ACCOUNT, &rows, "t")
            .unwrap()
    );
    assert_eq!(db.cached_library().unwrap(), Vec::new());
    assert!(
        db.cache_library_for(META_ACCOUNT, "fixture-other-user", &rows, "t")
            .unwrap()
    );
    assert_eq!(db.cached_library().unwrap().len(), 1);
}

/// Red if `steam.login.complete` takes the login before reading the id: the
/// stale request would end the one the user is looking at.
#[test]
fn a_stale_session_id_leaves_the_pending_login_alone() {
    let fixture = Fixture::new();
    let (notifier, _lines) = Notifier::channel();
    let mut handler = handler(&fixture, guarded(5));
    let old: LoginSession = result_of(start(&mut handler, &notifier, PASSWORD));
    let new: LoginSession = result_of(start(&mut handler, &notifier, PASSWORD));
    assert_ne!(old.session_id, new.session_id);

    let stale = complete(&mut handler, &notifier, &old.session_id, None).unwrap_err();
    assert_eq!(stale.code, ErrorCode::SteamLoginFailed);

    let done: LoginSession = result_of(complete(
        &mut handler,
        &notifier,
        &new.session_id,
        Some(CODE),
    ));
    assert!(matches!(done.step, LoginStepWire::Waiting), "{done:?}");
}

/// A code taken and no token: without the bound nothing ends this but Steam.
#[test]
fn a_login_stuck_after_its_code_ends_with_a_typed_failure() {
    let fixture = Fixture::new();
    let (mut core, _host, flow) = start_login(&fixture, guarded(usize::MAX));
    let mut flow = flow.with_wait_limit(Duration::from_millis(40));
    assert_eq!(
        flow.advance(&mut core, Some(CODE)).unwrap(),
        LoginStep::Waiting
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    let error = loop {
        match flow.advance(&mut core, None) {
            Ok(step) => assert_eq!(step, LoginStep::Waiting),
            Err(error) => break error,
        }
        assert!(Instant::now() < deadline, "the wait was not bounded");
    };
    assert!(
        matches!(error, CoreError::SteamLoginStalled { .. }),
        "{error}"
    );
    assert_eq!(error.code(), ErrorCode::SteamLoginFailed);
}

#[test]
fn a_blank_account_name_over_rpc_is_invalid_params() {
    let fixture = Fixture::new();
    let host = guarded(0);
    let script = Arc::clone(&host.script);
    let mut handler = handler(&fixture, host);
    let (notifier, _lines) = Notifier::channel();
    let error = handler
        .handle(
            &Method::SteamLoginStart(SteamLoginStartParams {
                mode: LoginMode::Credentials {
                    account_name: "  ".to_owned(),
                    password: PASSWORD.to_owned(),
                },
            }),
            &notifier,
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidParams);
    assert_eq!(script.lock().unwrap().connections, 0);
}

/// Red if the limit is tested before the poll: an approval in the last
/// interval would be thrown away unseen.
#[test]
fn a_login_past_its_limit_looks_once_more_before_it_gives_up() {
    let fixture = Fixture::new();
    let (mut core, host, flow) = start_login(&fixture, guarded(usize::MAX));
    let mut flow = flow.with_wait_limit(Duration::from_millis(40));
    assert_eq!(
        flow.advance(&mut core, Some(CODE)).unwrap(),
        LoginStep::Waiting
    );
    std::thread::sleep(Duration::from_millis(80));
    host.script.lock().unwrap().pending_polls = 0;
    assert_eq!(
        flow.advance(&mut core, None).unwrap(),
        LoginStep::Done {
            account_name: ACCOUNT.to_owned()
        }
    );
}

/// The method table derives `Debug`, and the log is not the only reader.
#[test]
fn debug_of_a_login_request_holds_no_secret() {
    let start = Method::SteamLoginStart(SteamLoginStartParams {
        mode: LoginMode::Credentials {
            account_name: ACCOUNT.to_owned(),
            password: PASSWORD.to_owned(),
        },
    });
    let complete = Method::SteamLoginComplete(SteamLoginCompleteParams {
        session_id: "login-1".to_owned(),
        steam_guard_code: Some(CODE.to_owned()),
    });
    let text = format!("{start:?} {complete:?}");
    for secret in [PASSWORD, CODE, ACCOUNT] {
        assert!(!text.contains(secret), "{text}");
    }
    assert!(text.contains("login-1"), "{text}");
}

#[test]
fn logout_over_rpc_ends_a_pending_login() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let (notifier, _lines) = Notifier::channel();
    let host = guarded(0);
    host.store.put(&RefreshToken::new(REFRESH)).unwrap();
    let mut handler = handler(&fixture, host);
    let started: LoginSession = result_of(start(&mut handler, &notifier, PASSWORD));
    handler
        .handle(&Method::SteamLogout(SteamLogoutParams {}), &notifier)
        .unwrap();
    let gone = complete(&mut handler, &notifier, &started.session_id, None).unwrap_err();
    assert_eq!(gone.code, ErrorCode::SteamLoginFailed);
}

fn app_install() -> Method {
    Method::AppInstall(AppInstallParams {
        appid: APP,
        branch: None,
        update: false,
        verify: false,
        library: None,
    })
}

#[test]
fn an_install_over_rpc_is_a_job_with_phases_and_byte_counts() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let (notifier, lines) = Notifier::channel();
    let mut handler = handler(&fixture, FakeHost::signed_in(session(ONE_ENTRY, false)));
    let started = handler.handle(&app_install(), &notifier).unwrap();
    assert!(started["job_id"].is_u64());
    let seen = notified(&lines, "job.done");
    let phases: Vec<&str> = seen
        .iter()
        .filter_map(|line| line["params"]["phase"].as_str())
        .collect();
    assert_eq!(phases[..3], ["login", "resolve", "download"], "{phases:?}");
    assert_eq!(phases.last(), Some(&"register"), "{phases:?}");
    assert!(
        seen.iter()
            .any(|line| line["params"]["done_bytes"].as_u64() > Some(0)),
        "{seen:?}"
    );
    assert!(fixture.acf().exists());
}

#[test]
fn a_refused_install_over_rpc_is_an_error_response_and_starts_no_job() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let (notifier, lines) = Notifier::channel();
    let host = FakeHost::signed_in(session(ONE_ENTRY, false));
    let mut handler = handler(&fixture, host);
    handler.handle(&app_install(), &notifier).unwrap();
    notified(&lines, "job.done");
    let error = handler.handle(&app_install(), &notifier).unwrap_err();
    assert_eq!(error.code, ErrorCode::AlreadyInstalled);
}

/// `app.cancel` for a title being installed. Red if it answers `NotRunning`.
#[test]
fn app_cancel_stops_an_install_and_a_later_install_finishes_it() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let (notifier, lines) = Notifier::channel();
    let mut slow = self::handler(&fixture, FakeHost::signed_in(session(ONE_ENTRY, true)));
    slow.handle(&app_install(), &notifier).unwrap();
    wait_for("the download record", || fixture.record().exists());

    let cancel = Method::AppCancel(AppCancelParams {
        appid: APP,
        kill_prefix: false,
    });
    slow.handle(&cancel, &notifier).unwrap();
    let mut failed = None;
    while failed.is_none() {
        let line = lines.recv_timeout(Duration::from_secs(20)).unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        if value["method"] == json!("job.failed") {
            failed = Some(value);
        }
    }
    assert_eq!(
        failed.unwrap()["params"]["code"],
        json!(ErrorCode::InstallCancelled.code())
    );
    let error = slow.handle(&cancel, &notifier).unwrap_err();
    assert_eq!(error.code, ErrorCode::NotRunning);

    let host = FakeHost::signed_in(session(ONE_ENTRY, false));
    assert!(fixture.install(&host).is_ok());
}

/// `CoreHandler::new` is what the other suites build. It must not be able to
/// reach Steam or the Keychain, whatever method a test walks through.
#[test]
fn a_plain_handler_has_no_steam() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let (notifier, _lines) = Notifier::channel();
    let mut handler = CoreHandler::<NoSteam>::new(fixture.core());
    let error = start(&mut handler, &notifier, PASSWORD).unwrap_err();
    assert_eq!(error.code, ErrorCode::SteamUnavailable);
}

/// The argv rule from plan 04 holds for what this unit added on top of the
/// Steam crate: neither module builds a process, and the `security` tool is
/// named nowhere.
#[test]
fn the_steam_paths_build_no_process() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for path in [
        "src/steam.rs",
        "src/install.rs",
        "../applejack/src/cmd/steam.rs",
        "../applejack/src/cmd/install.rs",
    ] {
        let text = fs::read_to_string(root.join(path)).unwrap();
        for forbidden in [
            "process::Command",
            "Command::new",
            "security find-",
            "security add-",
        ] {
            assert!(!text.contains(forbidden), "{path}: {forbidden}");
        }
    }
}

#[test]
fn a_steam_error_keeps_its_code_through_a_job() {
    let error = CoreError::Steam(SteamError::AppNotOwned { appid: 1 });
    assert_eq!(error.code(), ErrorCode::AppNotOwned);
    let error = CoreError::Steam(SteamError::Cancelled { appid: 1 });
    assert_eq!(error.code(), ErrorCode::InstallCancelled);
}

/// What `applejack steam login --qr` saw live on 2026-11-06: four challenges
/// about 30 s apart, then the CM closed the socket. The flow names the expiry,
/// not the socket, and no command: the CLI adds its own, and the shell shows
/// this text beside its Retry button. (`live.rs` pins that the closed socket
/// arrives here as `AuthError::Disconnected`; the stub injects it directly.)
#[test]
fn an_unscanned_qr_challenge_ends_as_expired_and_names_no_command() {
    let fixture = Fixture::new();
    let host = FakeHost::new(
        Script {
            poll_error: Some(AuthError::Disconnected),
            ..Script::default()
        },
        session(ONE_ENTRY, false),
    );
    let mut core = fixture.core();
    let host = Arc::new(host);
    let mut flow = LoginFlow::start(&mut core, Arc::clone(&host), AuthMode::Qr, None).unwrap();
    assert_eq!(
        flow.step(),
        &LoginStep::QrReady {
            payload: QR.to_owned()
        }
    );
    let error = flow.advance(&mut core, None).unwrap_err();
    assert!(matches!(error, CoreError::SteamQrExpired { .. }), "{error}");
    assert_eq!(error.code(), ErrorCode::SteamLoginFailed);
    let text = error.to_string();
    assert!(
        text.starts_with("the QR challenge expired after "),
        "{text}"
    );
    assert!(text.ends_with(" s without a scan"), "{text}");
    assert!(!text.contains("applejack"), "{text}");
    assert_eq!(stored_token(&host), None);
    assert_eq!(fixture.db().meta_get(META_ACCOUNT).unwrap(), None);
}

/// Only the QR wait reads a hang-up as an expiry; a code prompt and any other
/// connection failure keep their own words.
#[test]
fn a_closed_connection_elsewhere_keeps_its_own_text() {
    let fixture = Fixture::new();
    let host = guarded(0);
    host.script.lock().unwrap().poll_error = Some(AuthError::Disconnected);
    let (mut core, _host, mut flow) = start_login(&fixture, host);
    let error = flow.advance(&mut core, None).unwrap_err();
    assert_eq!(error.code(), ErrorCode::SteamUnavailable);
    assert_eq!(error.to_string(), "connection: Steam closed the connection");

    let host = FakeHost::new(
        Script {
            poll_error: Some(AuthError::Connection("Steam did not answer".to_owned())),
            ..Script::default()
        },
        session(ONE_ENTRY, false),
    );
    let mut core = fixture.core();
    let mut flow = LoginFlow::start(&mut core, Arc::new(host), AuthMode::Qr, None).unwrap();
    let error = flow.advance(&mut core, None).unwrap_err();
    assert_eq!(error.code(), ErrorCode::SteamUnavailable);
    assert_eq!(error.to_string(), "connection: Steam did not answer");
}
