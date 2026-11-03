//! The Steam client runtime and the client launch path over the stub bundle:
//! install with its Authenticode gate, the login state, a session that
//! starts at the client's marker and ends when it clears while the client
//! stays up, the A2 refusal, cancel, stop, repair, and a dead owner's row.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use applejack_core::config::{ConfigDb, Drm, NewSession, SessionRow, SessionState};
use applejack_core::rpc::wire::ErrorCode;
use applejack_core::run::AppRegistration;
use applejack_core::runtime::session::{NOTE_CLIENT, NOTE_LOST, RunTuning};
use applejack_core::runtime::steam_client::{
    ClientOp, ClientRuntimeState, META_CLIENT_BUILD, META_CLIENT_SESSION, META_REDOWNLOAD,
    STEAM_CLIENT_PREFIX, acf_backup_path,
};
use applejack_core::{Core, CoreError};
use applejack_engine::rosetta_wine::process::find_session;
use applejack_engine::rosetta_wine::utc_now;
use applejack_engine::{Error as EngineError, GraphicsBackend, HidrawMode};
use common::{spawn_stub_server, write_stub_bundle, write_title};
use tempfile::TempDir;

const APP: u32 = 242_920;

const FAST: RunTuning = RunTuning {
    tick: Duration::from_millis(20),
    marker_misses: 2,
    drain: Duration::from_secs(3),
    early_exit: Duration::ZERO,
    term_wait: Duration::from_millis(300),
    cancel_wait: Duration::from_secs(10),
    client_start_wait: Duration::from_secs(2),
};

/// A remembered account, as the client writes one after a sign-in.
const LOGINUSERS: &str = "\"users\"\n{\n\t\"76561190000000000\"\n\t{\n\t\t\"AccountName\"\t\t\"fixture\"\n\t\t\"RememberPassword\"\t\t\"1\"\n\t\t\"MostRecent\"\t\t\"1\"\n\t\t\"AllowAutoLogin\"\t\t\"1\"\n\t}\n}\n";

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    /// An installed engine and a registered Steam-DRM title, no client.
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let fixture = Self { dir };
        fs::create_dir_all(fixture.data()).unwrap();
        write_title(&fixture.library(), APP, &["game.exe"]);
        let tree = fixture.dir.path().join("tree");
        write_stub_bundle(&tree, "10.0.0");
        fixture
            .core()
            .plan_install("rosetta-wine", Some(&tree))
            .unwrap()
            .run(&mut |_| {})
            .unwrap();
        fixture
            .core()
            .app_register(&AppRegistration {
                appid: APP,
                drm: Some(Drm::Steam),
                exe: Some("game.exe".to_owned()),
                library: None,
            })
            .unwrap();
        fixture
    }

    fn data(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    fn library(&self) -> PathBuf {
        self.data().join("library")
    }

    fn core(&self) -> Core {
        Core::new(self.data())
            .with_unsigned_installs(true)
            .with_run_tuning(FAST)
    }

    fn prefix_dir(&self) -> PathBuf {
        self.data().join("prefixes").join(STEAM_CLIENT_PREFIX)
    }

    fn steam_dir(&self) -> PathBuf {
        self.prefix_dir().join("drive_c/Program Files (x86)/Steam")
    }

    fn acf(&self) -> PathBuf {
        self.library()
            .join(format!("steamapps/appmanifest_{APP}.acf"))
    }

    fn db(&self) -> ConfigDb {
        ConfigDb::open(&self.data().join("db.sqlite")).unwrap()
    }

    fn rows(&self) -> Vec<SessionRow> {
        self.db().sessions(Some(APP)).unwrap()
    }

    /// `runtime steam install` with the bootstrapper arriving as `content`.
    fn install_with(&self, content: &str) -> Result<ClientRuntimeState, CoreError> {
        let fetch = |_url: &str, dest: &Path, progress: &mut dyn FnMut(u64, Option<u64>)| {
            fs::write(dest, content).map_err(|error| CoreError::Message(error.to_string()))?;
            progress(content.len() as u64, Some(content.len() as u64));
            Ok(())
        };
        let status = self
            .core()
            .plan_client_op(ClientOp::Install)?
            .run_with(&fetch, &mut |_, _| {})?;
        Ok(status.state)
    }

    fn install(&self) {
        assert_eq!(
            self.install_with("VALVE-SIGNED").unwrap(),
            ClientRuntimeState::NeedsLogin
        );
    }

    fn sign_in(&self) {
        fs::write(self.steam_dir().join("config/loginusers.vdf"), LOGINUSERS).unwrap();
    }

    fn client_key(&self) -> String {
        self.db().meta_get(META_CLIENT_SESSION).unwrap().unwrap()
    }

    fn client_up(&self) -> bool {
        find_session(&self.client_key())
            .iter()
            .any(|process| process.exe == "steam.exe")
    }

    fn quit_game(&self, code: i32) {
        fs::write(self.prefix_dir().join("quit"), code.to_string()).unwrap();
    }

    fn start(&self) -> Running {
        let plan = self.core().plan_run(APP, false).unwrap();
        let id = plan.session_id();
        let thread = thread::spawn(move || {
            let mut phases = Vec::new();
            let outcome = plan.run(&mut |phase, _| phases.push(phase.to_owned()));
            (outcome, phases)
        });
        let running = Running { id, thread };
        self.wait_for(id, SessionState::Running);
        running
    }

    fn wait_for(&self, id: i64, state: SessionState) {
        wait_until(&format!("session {id} {state}"), || {
            self.db()
                .session(id)
                .unwrap()
                .filter(|row| row.state == state)
        });
    }

    fn assert_lock_free(&self) {
        drop(self.core().claim_prefix(STEAM_CLIENT_PREFIX).unwrap());
    }
}

struct Running {
    id: i64,
    thread: JoinHandle<(Result<SessionRow, CoreError>, Vec<String>)>,
}

impl Running {
    fn finish(self) -> (Result<SessionRow, CoreError>, Vec<String>) {
        self.thread.join().unwrap()
    }
}

fn wait_until<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_gone(what: &str, mut present: impl FnMut() -> bool) {
    wait_until(what, || (!present()).then_some(()));
}

#[test]
fn a_client_title_is_refused_until_the_runtime_is_installed_and_signed_in() {
    let fixture = Fixture::new();
    let error = fixture.core().plan_run(APP, false).unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientRuntimeUnavailable);
    let text = error.to_string();
    assert!(text.contains("absent"), "{text}");
    assert!(text.contains("applejack runtime steam install"), "{text}");
    assert!(
        text.contains("--probe-direct"),
        "the client-free route stays: {text}"
    );

    fixture.install();
    let error = fixture.core().plan_run(APP, false).unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientRuntimeUnavailable);
    let text = error.to_string();
    assert!(text.contains("needs_login"), "{text}");
    assert!(text.contains("sign in inside the Steam window"), "{text}");
    assert!(
        fixture.rows().is_empty(),
        "no row before the client is ready"
    );

    fs::remove_file(fixture.steam_dir().join("steam.exe")).unwrap();
    let error = fixture.core().plan_run(APP, false).unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientRuntimeBroken, "{error}");
    assert!(
        error.to_string().contains("runtime steam repair"),
        "{error}"
    );
}

#[test]
fn install_refuses_a_bootstrapper_that_is_not_valves_and_keeps_it() {
    let fixture = Fixture::new();
    let error = fixture.install_with("OTHER-SIGNED").unwrap_err();
    assert_eq!(error.code(), ErrorCode::BootstrapperUnverified, "{error}");
    let text = error.to_string();
    assert!(text.contains("not Valve Corp"), "{text}");
    assert!(text.contains("O=Not Valve"), "names the signer: {text}");
    assert!(
        fixture
            .prefix_dir()
            .join("applejack/SteamSetup.exe")
            .is_file(),
        "kept for inspection"
    );
    assert!(!fixture.steam_dir().join("steam.exe").exists(), "never ran");

    let error = fixture.install_with("TAMPERED").unwrap_err();
    assert_eq!(error.code(), ErrorCode::BootstrapperUnverified, "{error}");
    let text = error.to_string();
    assert!(text.contains("did not verify"), "{text}");
    assert!(text.contains("no signature found"), "{text}");
    assert!(!fixture.steam_dir().join("steam.exe").exists());
    assert_eq!(
        fixture.core().client_runtime_state().unwrap().0,
        ClientRuntimeState::Absent
    );
}

#[test]
fn install_lays_the_client_down_records_its_build_and_leaves_it_at_the_window() {
    let fixture = Fixture::new();
    fixture.install();
    let bootstrapped = fs::read_to_string(fixture.steam_dir().join("bootstrapped")).unwrap();
    assert!(
        bootstrapped.trim().ends_with("SteamSetup.exe /S"),
        "{bootstrapped}"
    );
    let status = fixture.core().client_runtime_status().unwrap();
    assert_eq!(status.state, ClientRuntimeState::NeedsLogin);
    assert_eq!(status.build_id.as_deref(), Some("1761720601"));
    assert_eq!(
        fixture.db().meta_get(META_CLIENT_BUILD).unwrap().as_deref(),
        Some("1761720601")
    );
    assert!(
        !fixture
            .prefix_dir()
            .join("applejack/client-install.json")
            .exists(),
        "the marker is gone"
    );
    assert!(fixture.client_up(), "the client stays up for the sign-in");
    assert!(
        find_session(&fixture.client_key())
            .iter()
            .any(|process| process.exe == "steamwebhelper.exe")
    );
    fixture.sign_in();
    assert_eq!(
        fixture.core().client_runtime_state().unwrap().0,
        ClientRuntimeState::Ready
    );
    // A second install with the client up is the login step again, not a
    // download: the bootstrapper's record is untouched.
    assert_eq!(
        fixture.install_with("TAMPERED").unwrap(),
        ClientRuntimeState::Ready
    );
}

#[test]
fn a_client_session_runs_from_the_marker_to_its_clearing_and_the_client_stays() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.sign_in();
    let before = fs::read(fixture.acf()).unwrap();
    let running = fixture.start();
    let row = fixture.db().session(running.id).unwrap().unwrap();
    assert_eq!(row.prefix, STEAM_CLIENT_PREFIX);
    assert_eq!(row.launch_exe, "game.exe");
    assert_eq!(
        fs::read(acf_backup_path(&fixture.library(), APP)).unwrap(),
        before
    );
    let folders =
        fs::read_to_string(fixture.steam_dir().join("config/libraryfolders.vdf")).unwrap();
    assert!(folders.contains("\"path\"\t\t\"S:\\\\\""), "{folders}");
    assert!(folders.contains(&format!("\"{APP}\"")), "{folders}");
    let args = fs::read_to_string(fixture.prefix_dir().join("args")).unwrap();
    assert_eq!(args.trim(), "-silent -noverifyfiles");
    assert!(matches!(
        fixture.core().plan_run(APP, false),
        Err(CoreError::AlreadyRunning { .. })
    ));
    assert!(matches!(
        fixture.core().plan_client_op(ClientOp::Stop),
        Err(CoreError::Engine(EngineError::PrefixBusy { .. }))
    ));

    fixture.quit_game(0);
    let (outcome, phases) = running.finish();
    let row = outcome.unwrap();
    assert_eq!(row.state, SessionState::Exited);
    assert_eq!(row.exit_code, None);
    assert_eq!(row.note.as_deref(), Some(NOTE_CLIENT));
    assert!(row.ended_at.is_some());
    assert_eq!(phases, ["prepare", "starting", "running", "exited"]);
    assert!(fixture.client_up(), "the client outlives the title");
    assert_eq!(
        fs::read(fixture.acf()).unwrap(),
        before,
        "the .acf is as it was"
    );
    fixture.assert_lock_free();

    // A second run hands off to the running client under its own key.
    let running = fixture.start();
    fixture.quit_game(0);
    assert_eq!(running.finish().0.unwrap().state, SessionState::Exited);
    assert!(fixture.client_up());
}

#[test]
fn a_redownload_stops_the_client_restores_the_acf_and_fails_as_a2() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.sign_in();
    let before = fs::read_to_string(fixture.acf()).unwrap();
    let server = spawn_stub_server("server", &fixture.prefix_dir());
    fs::write(fixture.prefix_dir().join("redownload"), "").unwrap();
    let plan = fixture.core().plan_run(APP, false).unwrap();
    let id = plan.session_id();
    let error = plan.run(&mut |_, _| {}).unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientRedownloadRefused, "{error}");
    assert!(error.to_string().contains("A2 failed"), "{error}");
    assert!(
        error
            .to_string()
            .contains("buildid changed from 100 to 101"),
        "{error}"
    );
    assert_eq!(
        fs::read_to_string(fixture.acf()).unwrap(),
        before,
        "restored from the backup"
    );
    let row = fixture.db().session(id).unwrap().unwrap();
    assert_eq!(row.state, SessionState::Failed);
    assert!(row.note.unwrap().contains("A2 failed"));
    assert!(
        fixture.prefix_dir().join("client-quit").exists(),
        "the client was stopped"
    );
    let record = fixture.db().meta_get(META_REDOWNLOAD).unwrap().unwrap();
    assert!(
        record.contains(&format!("app {APP} buildid changed")),
        "{record}"
    );
    let _ = fs::remove_file(fixture.prefix_dir().join("server-quit"));
    drop(server);
}

#[test]
fn a_client_that_reports_no_title_times_out_and_is_stopped() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.sign_in();
    fs::write(fixture.prefix_dir().join("never-start"), "").unwrap();
    let plan = fixture.core().plan_run(APP, false).unwrap();
    let id = plan.session_id();
    let error = plan.run(&mut |_, _| {}).unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientLaunchTimedOut, "{error}");
    assert_eq!(
        fixture.db().session(id).unwrap().unwrap().state,
        SessionState::Failed
    );
    fixture.assert_lock_free();
}

#[test]
fn cancel_ends_the_title_and_leaves_the_client_up() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.sign_in();
    let running = fixture.start();
    let row = fixture
        .core()
        .plan_cancel(APP, false)
        .unwrap()
        .run(&mut |_, _| {})
        .unwrap();
    assert_eq!(row.state, SessionState::Cancelled);
    assert_eq!(running.finish().0.unwrap().state, SessionState::Cancelled);
    let key = fixture.client_key();
    assert!(
        !find_session(&key)
            .iter()
            .any(|process| process.exe == "game.exe"),
        "the title is gone"
    );
    assert!(fixture.client_up(), "the client stays");
    assert!(!fixture.prefix_dir().join("client-quit").exists());
    fixture.assert_lock_free();
}

#[test]
fn stop_ends_the_client_and_update_snapshots_it_first() {
    let fixture = Fixture::new();
    fixture.install();
    let server = spawn_stub_server("server", &fixture.prefix_dir());
    let status = fixture
        .core()
        .plan_client_op(ClientOp::Stop)
        .unwrap()
        .run(&mut |_, _| {})
        .unwrap();
    assert!(fixture.prefix_dir().join("client-quit").exists());
    wait_gone("the client", || fixture.client_up());
    assert_eq!(
        status.state,
        ClientRuntimeState::NeedsLogin,
        "stopped, still installed"
    );
    drop(server);
    let _ = fs::remove_file(fixture.prefix_dir().join("client-quit"));
    let _ = fs::remove_file(fixture.prefix_dir().join("server-quit"));

    let mut phases = Vec::new();
    let status = fixture
        .core()
        .plan_client_op(ClientOp::Update)
        .unwrap()
        .run(&mut |phase, _| phases.push(phase.to_owned()))
        .unwrap();
    assert_eq!(status.snapshots, 1, "{phases:?}");
    assert!(phases.contains(&"snapshot".to_owned()), "{phases:?}");
    assert!(fixture.client_up(), "the update started the client again");
}

#[test]
fn repair_restores_the_newest_snapshot_and_reinstall_reruns_the_bootstrapper() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.core().prefix_snapshot(STEAM_CLIENT_PREFIX).unwrap();
    fs::remove_file(fixture.steam_dir().join("steam.exe")).unwrap();
    assert_eq!(
        fixture.core().client_runtime_state().unwrap().0,
        ClientRuntimeState::Broken
    );
    let status = fixture
        .core()
        .plan_client_op(ClientOp::Repair { reinstall: false })
        .unwrap()
        .run(&mut |_, _| {})
        .unwrap();
    assert!(
        fixture.steam_dir().join("steam.exe").is_file(),
        "back from the snapshot"
    );
    assert_eq!(status.state, ClientRuntimeState::NeedsLogin);

    fs::remove_file(fixture.steam_dir().join("bootstrapped")).unwrap();
    let fetch = |_: &str, dest: &Path, _: &mut dyn FnMut(u64, Option<u64>)| {
        fs::write(dest, "VALVE-SIGNED").map_err(|error| CoreError::Message(error.to_string()))
    };
    fixture
        .core()
        .plan_client_op(ClientOp::Repair { reinstall: true })
        .unwrap()
        .run_with(&fetch, &mut |_, _| {})
        .unwrap();
    assert!(
        fixture.steam_dir().join("bootstrapped").is_file(),
        "the bootstrapper ran again"
    );
}

#[test]
fn a_dead_owners_client_row_closes_once_the_title_is_gone() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.sign_in();
    let key = fixture.client_key();
    let id = fixture
        .db()
        .insert_session(&NewSession {
            appid: APP,
            started_at: &utc_now(),
            engine_id: "rosetta-wine",
            engine_version: "10.0.0",
            log_path: "/dev/null",
            session_key: &key,
            prefix: STEAM_CLIENT_PREFIX,
            launch_exe: "game.exe",
        })
        .unwrap()
        .unwrap();
    assert!(
        fixture.client_up(),
        "the client carries the dead owner's key"
    );
    assert!(
        fixture.core().reconcile_sessions().unwrap().is_empty(),
        "nothing to take over"
    );
    let row = fixture.db().session(id).unwrap().unwrap();
    assert_eq!(row.state, SessionState::Lost);
    assert_eq!(row.note.as_deref(), Some(NOTE_LOST));
    let running = fixture.start();
    fixture.quit_game(0);
    assert_eq!(running.finish().0.unwrap().state, SessionState::Exited);
}

#[test]
fn the_client_prefix_keeps_its_settings_while_the_client_runs() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.sign_in();
    fixture
        .core()
        .config_set(APP, "graphics_backend", "wined3d")
        .unwrap();
    let server = spawn_stub_server("server", &fixture.prefix_dir());
    let running = fixture.start();
    fixture.quit_game(0);
    assert_eq!(running.finish().0.unwrap().state, SessionState::Exited);
    let (_, prefix) = fixture
        .core()
        .prefix_list()
        .unwrap()
        .into_iter()
        .find(|p| p.name == STEAM_CLIENT_PREFIX)
        .map(|p| (p.name.clone(), p))
        .unwrap();
    assert_eq!(
        prefix.backend,
        Some(GraphicsBackend::Dxmt),
        "not reapplied under a running client"
    );
    assert_eq!(prefix.hidraw, Some(HidrawMode::Disabled));
    drop(server);
}
