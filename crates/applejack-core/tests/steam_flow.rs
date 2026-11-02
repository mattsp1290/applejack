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
use std::sync::mpsc::Receiver;
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
use applejack_steam::auth::{AccessToken, AuthError, AuthMode, AuthPoll, GuardFactor};
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
const ACCESS: &str = "fixture-access-token-value";
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
    logon_error: Option<AuthError>,
    submitted: Vec<String>,
    polls: usize,
    logons: Vec<(String, String)>,
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

/// `FakeSession` behind an `Arc`, so a test can look at it afterwards.
struct Shared(Arc<FakeSession>);

impl SteamSession for Shared {
    type Chunks = FakeChunks;

    fn heartbeat_interval(&self) -> Option<Duration> {
        None
    }

    fn license_list(&self) -> impl Future<Output = Result<Vec<License>, AuthError>> + Send {
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
        ready(Ok(if script.pending_polls > 0 {
            script.pending_polls -= 1;
            AuthPoll::Pending { challenge: None }
        } else {
            AuthPoll::Approved(Tokens {
                account_name: ACCOUNT.to_owned(),
                refresh_token: RefreshToken::new(REFRESH),
                access_token: AccessToken::new(ACCESS),
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
        ready(
            script
                .logon_error
                .take()
                .map_or_else(|| Ok(Shared(Arc::clone(&self.session))), Err),
        )
    }
}

impl SteamHost for FakeHost {
    type Connection = FakeConnection;
    type Store = InMemoryStore;

    fn connection(&self) -> FakeConnection {
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
        Core::new(self.data())
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
    fixture.core().cancel_install(APP).unwrap();
    let error = plan.run(&host, &|_, _, _, _| {}).unwrap_err();
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
        ACCESS,
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
        assert!(!text.contains(ACCESS));
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
