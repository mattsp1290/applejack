//! The Steam client runtime and the client launch path over the stub bundle:
//! install with its Authenticode gate, the login state, a session that
//! starts at the client's marker and ends when it clears while the client
//! stays up, the A2 refusal and the routine patch it is not, cancel, stop,
//! repair, a dead owner's row, a stale install marker, a stale running
//! marker, and a library folder added under a running client.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use applejack_core::config::{ConfigDb, Drm, NewSession, SessionRow, SessionState};
use applejack_core::rpc::wire::ErrorCode;
use applejack_core::run::{AppRegistration, PlannedRun};
use applejack_core::runtime::session::{NOTE_CLIENT, NOTE_LOST, RunTuning};
use applejack_core::runtime::steam_client::{
    ClientOp, ClientRuntimeState, ClientRuntimeStatus, META_CLIENT_BUILD, META_CLIENT_SESSION,
    META_REDOWNLOAD, STEAM_CLIENT_PREFIX, WEBHELPER_EXE, WEBHELPER_REAL, acf_backup_path,
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

/// How long a run against the stub may take to end: the longest legitimate
/// one is a cancel (`cancel_wait`) followed by the drain.
const RUN_WAIT: Duration = Duration::from_secs(30);

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
        self.install_reporting(content).0
    }

    /// [`Fixture::install_with`] with every `phase: message` line reported.
    fn install_reporting(
        &self,
        content: &str,
    ) -> (Result<ClientRuntimeState, CoreError>, Vec<String>) {
        let fetch = |_url: &str, dest: &Path, progress: &mut dyn FnMut(u64, Option<u64>)| {
            fs::write(dest, content).map_err(|error| CoreError::Message(error.to_string()))?;
            progress(content.len() as u64, Some(content.len() as u64));
            Ok(())
        };
        let mut lines = Vec::new();
        let outcome = self
            .core()
            .plan_client_op(ClientOp::Install)
            .and_then(|plan| {
                plan.run_with(&fetch, &mut |phase, message| {
                    lines.push(format!("{phase}: {message}"));
                })
            });
        (outcome.map(|status| status.state), lines)
    }

    /// The stub client's one CEF directory.
    fn cef_dir(&self) -> PathBuf {
        self.steam_dir().join("bin/cef/cef.win7x64")
    }

    fn helper(&self) -> Vec<u8> {
        fs::read(self.cef_dir().join(WEBHELPER_EXE)).unwrap()
    }

    fn real_helper(&self) -> Option<Vec<u8>> {
        fs::read(self.cef_dir().join(WEBHELPER_REAL)).ok()
    }

    /// The stub bundle's shim, as the payload's would be.
    fn shim(&self) -> Vec<u8> {
        fs::read(
            self.data()
                .join("engines/rosetta-wine/10.0.0/probes/steamwebhelper-shim.exe"),
        )
        .unwrap()
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

    /// Ends the stub client as a stop would, and clears what that leaves so
    /// the next start is a fresh client.
    fn stop_client(&self) {
        fs::write(self.prefix_dir().join("client-quit"), "").unwrap();
        wait_gone("the client", || self.client_up());
        let _ = fs::remove_file(self.prefix_dir().join("client-quit"));
        let _ = fs::remove_file(self.prefix_dir().join("client-up"));
    }

    fn running_appid(&self) -> String {
        fs::read_to_string(self.prefix_dir().join("running-appid"))
            .unwrap_or_default()
            .trim()
            .to_owned()
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
        join_bounded(&format!("the run of session {}", self.id), self.thread)
    }
}

/// Joins a run's thread under [`RUN_WAIT`]. A watch that never ends (a
/// marker that never clears, a stub that never starts) fails the test
/// naming the run instead of hanging the harness.
fn join_bounded<T: Send + 'static>(what: &str, thread: JoinHandle<T>) -> T {
    let deadline = Instant::now() + RUN_WAIT;
    while !thread.is_finished() {
        assert!(
            Instant::now() < deadline,
            "timed out after {RUN_WAIT:?} waiting for {what} to end"
        );
        thread::sleep(Duration::from_millis(20));
    }
    thread.join().unwrap()
}

/// Runs a plan to its end on a thread of its own, under [`RUN_WAIT`].
fn run_plan(plan: PlannedRun) -> Result<SessionRow, CoreError> {
    let what = format!("the run of session {}", plan.session_id());
    join_bounded(&what, thread::spawn(move || plan.run(&mut |_, _| {})))
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
    assert!(text.contains("not Valve Corp."), "{text}");
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
    let (state, lines) = fixture.install_reporting("VALVE-SIGNED");
    assert_eq!(state.unwrap(), ClientRuntimeState::NeedsLogin);
    let bootstrapped = fs::read_to_string(fixture.steam_dir().join("bootstrapped")).unwrap();
    assert!(
        bootstrapped.trim().ends_with("SteamSetup.exe /S"),
        "{bootstrapped}"
    );
    assert!(
        lines.contains(&"client: window: the login window exists (logs/webhelper.txt)".to_owned()),
        "readiness is keyed on the helper's log line: {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line
            .starts_with("start: macOS may ask whether \"applejack\" may use the microphone")),
        "the prompt is announced before the start: {lines:?}"
    );
    let client_args = fs::read_to_string(fixture.prefix_dir().join("client-args")).unwrap();
    assert!(
        client_args.trim().ends_with("steam.exe -noverifyfiles"),
        "the first run skips the client's file check, which would undo the shim: {client_args}"
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
    let folders =
        fs::read_to_string(fixture.steam_dir().join("config/libraryfolders.vdf")).unwrap();
    assert!(
        folders.contains("\"path\"\t\t\"S:\\\\\""),
        "the shared library is listed before the client first starts: {folders}"
    );
    assert!(
        folders.contains("C:\\\\Program Files (x86)\\\\Steam"),
        "after the client's own folder: {folders}"
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

/// An install that died after `steam.exe` landed leaves its marker. The
/// next `install` holds the lock, so the marker is a dead install's: it is
/// cleared, and the state read under the operation's own lock is never
/// `installing`.
#[test]
fn a_stale_install_marker_is_cleared_by_the_next_install() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.stop_client();
    let marker = fixture.prefix_dir().join("applejack/client-install.json");
    fs::write(&marker, "{}").unwrap();
    let (state, detail) = fixture.core().client_runtime_state().unwrap();
    assert_eq!(state, ClientRuntimeState::Broken);
    assert!(detail.contains("runtime steam install"), "{detail}");
    let error = fixture.core().plan_run(APP, false).unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientRuntimeBroken, "{error}");

    let mut phases = Vec::new();
    let status = fixture
        .core()
        .plan_client_op(ClientOp::Install)
        .unwrap()
        .run_with(
            &|_: &str, _: &Path, _: &mut dyn FnMut(u64, Option<u64>)| {
                Err(CoreError::Message(
                    "the bootstrapper is not fetched again".to_owned(),
                ))
            },
            &mut |phase, message| phases.push(format!("{phase}: {message}")),
        )
        .unwrap();
    assert_eq!(status.state, ClientRuntimeState::NeedsLogin, "{phases:?}");
    assert!(!marker.exists(), "{phases:?}");
    assert!(
        phases
            .iter()
            .any(|line| line.contains("clearing the marker")),
        "{phases:?}"
    );
    assert_eq!(
        fixture.core().client_runtime_state().unwrap().0,
        ClientRuntimeState::NeedsLogin
    );
    assert!(fixture.client_up());
}

/// The marker with the lock held elsewhere reads `installing`, and the read
/// neither takes the lock nor makes anything under `.snapshots`.
#[test]
fn a_marker_under_a_live_lock_reads_installing_without_taking_the_lock() {
    let fixture = Fixture::new();
    fixture.install();
    let marker = fixture.prefix_dir().join("applejack/client-install.json");
    fs::write(&marker, "{}").unwrap();
    let held = fixture.core().claim_prefix(STEAM_CLIENT_PREFIX).unwrap();
    let (state, _) = fixture.core().client_runtime_state().unwrap();
    assert_eq!(state, ClientRuntimeState::Installing);
    let status = fixture.core().client_runtime_status().unwrap();
    assert_eq!(status.state, ClientRuntimeState::Installing);
    drop(held);
    assert_eq!(
        fixture.core().client_runtime_state().unwrap().0,
        ClientRuntimeState::Broken
    );
    fs::remove_file(&marker).unwrap();
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
    assert!(
        !fixture.prefix_dir().join("reg-add").exists(),
        "a clear marker is not reset"
    );

    // A second run hands off to the running client under its own key.
    let running = fixture.start();
    fixture.quit_game(0);
    assert_eq!(running.finish().0.unwrap().state, SessionState::Exited);
    assert!(fixture.client_up());
}

/// A client stopped while the title ran leaves `RunningAppID` at the appid.
/// The run resets it before `-applaunch`, so a client that then launches
/// nothing times out instead of a session that runs with no process.
#[test]
fn a_stale_running_appid_is_reset_before_the_launch() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.sign_in();
    fs::write(fixture.prefix_dir().join("running-appid"), APP.to_string()).unwrap();
    fs::write(fixture.prefix_dir().join("never-start"), "").unwrap();
    let plan = fixture.core().plan_run(APP, false).unwrap();
    let id = plan.session_id();
    let log = plan.log_path().to_owned();
    let started = Instant::now();
    let error = run_plan(plan).unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientLaunchTimedOut, "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "ended by the start wait, not a cancel"
    );
    assert_eq!(fixture.running_appid(), "0", "reset through the loader");
    assert_eq!(
        fs::read_to_string(fixture.prefix_dir().join("reg-add"))
            .unwrap()
            .trim(),
        "0"
    );
    assert!(
        fs::read_to_string(&log)
            .unwrap()
            .contains("RunningAppID already names this title"),
        "the reset is logged"
    );
    let row = fixture.db().session(id).unwrap().unwrap();
    assert_eq!(row.state, SessionState::Failed);
    fixture.assert_lock_free();
}

/// A value naming another title says nothing about this one: the launch
/// exe under the keys is what counts then.
#[test]
fn another_titles_marker_falls_through_to_the_exe() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.sign_in();
    fs::write(fixture.prefix_dir().join("running-appid"), "999").unwrap();
    fs::write(fixture.prefix_dir().join("no-marker"), "").unwrap();
    let running = fixture.start();
    assert_eq!(
        fixture.running_appid(),
        "999",
        "another title's value is left alone"
    );
    assert!(!fixture.prefix_dir().join("reg-add").exists());
    fixture.quit_game(0);
    let (outcome, phases) = running.finish();
    assert_eq!(outcome.unwrap().state, SessionState::Exited);
    assert_eq!(phases, ["prepare", "starting", "running", "exited"]);
}

/// A title in a library the running client has never seen: the folder is
/// written through a rename and the client is bounced before `-applaunch`,
/// so the run's `steam.exe` becomes the client and carries the run's key.
#[test]
fn a_library_folder_added_under_a_running_client_restarts_it_first() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.sign_in();
    let other = fixture.dir.path().join("other-library");
    write_title(&other, APP, &["game.exe"]);
    fixture
        .core()
        .app_register(&AppRegistration {
            appid: APP,
            drm: Some(Drm::Steam),
            exe: Some("game.exe".to_owned()),
            library: Some(other.clone()),
        })
        .unwrap();
    let old_key = fixture.client_key();
    let server = spawn_stub_server("server", &fixture.prefix_dir());
    let plan = fixture.core().plan_run(APP, false).unwrap();
    let id = plan.session_id();
    let thread = thread::spawn(move || {
        let mut phases = Vec::new();
        let outcome = plan.run(&mut |phase, message| phases.push(format!("{phase}: {message}")));
        (outcome, phases)
    });
    fixture.wait_for(id, SessionState::Running);
    let row = fixture.db().session(id).unwrap().unwrap();
    assert_eq!(
        fixture.client_key(),
        row.session_key,
        "this run started the client"
    );
    assert_ne!(old_key, row.session_key);
    // The stub writes the marker before it becomes the client.
    wait_until("the client under the new key", || {
        fixture.client_up().then_some(())
    });
    assert!(
        find_session(&old_key).is_empty(),
        "nothing of the old client is left"
    );
    let folders =
        fs::read_to_string(fixture.steam_dir().join("config/libraryfolders.vdf")).unwrap();
    assert!(folders.contains("Z:\\\\"), "{folders}");
    assert!(
        !fixture
            .steam_dir()
            .join("config/libraryfolders.vdf.applejack")
            .exists()
    );
    fixture.quit_game(0);
    let (outcome, phases) = join_bounded(&format!("the run of session {id}"), thread);
    assert_eq!(outcome.unwrap().state, SessionState::Exited);
    assert!(
        phases
            .iter()
            .any(|line| line.contains("restarting the Steam client")),
        "{phases:?}"
    );
    drop(server);
}

/// Plan 05 records the build after every client start; a run that starts
/// the client is one.
#[test]
fn a_run_that_starts_the_client_records_its_build() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.sign_in();
    fixture.stop_client();
    fixture.db().meta_delete(META_CLIENT_BUILD).unwrap();
    let running = fixture.start();
    fixture.quit_game(0);
    assert_eq!(running.finish().0.unwrap().state, SessionState::Exited);
    assert_eq!(
        fixture.db().meta_get(META_CLIENT_BUILD).unwrap().as_deref(),
        Some("1761720601")
    );
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
    let error = run_plan(plan).unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientRedownloadRefused, "{error}");
    assert!(error.to_string().contains("A2 failed"), "{error}");
    assert!(
        error.to_string().contains("StateFlags became 1026"),
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
        record.contains(&format!("app {APP} StateFlags became")),
        "{record}"
    );
    let _ = fs::remove_file(fixture.prefix_dir().join("server-quit"));
    drop(server);
}

/// An install one build behind is patched by the client: the build moves
/// forward. That is not a refuted A2; the client is stopped and the `.acf`
/// put back all the same, and the error names `install --update`.
#[test]
fn a_routine_patch_is_not_a_refuted_a2() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.sign_in();
    let before = fs::read_to_string(fixture.acf()).unwrap();
    let server = spawn_stub_server("server", &fixture.prefix_dir());
    fs::write(fixture.prefix_dir().join("patch"), "").unwrap();
    let plan = fixture.core().plan_run(APP, false).unwrap();
    let id = plan.session_id();
    let error = run_plan(plan).unwrap_err();
    assert_eq!(error.code(), ErrorCode::TitleUpdatePending, "{error}");
    let text = error.to_string();
    assert!(text.contains("from build 100 to 101"), "{text}");
    assert!(text.contains(&format!("install {APP} --update")), "{text}");
    assert!(text.contains("runtime steam stop"), "{text}");
    assert!(!text.contains("A2"), "{text}");
    assert_eq!(fs::read_to_string(fixture.acf()).unwrap(), before);
    assert_eq!(
        fixture.db().session(id).unwrap().unwrap().state,
        SessionState::Failed
    );
    assert!(fixture.prefix_dir().join("client-quit").exists());
    assert_eq!(
        fixture.db().meta_get(META_REDOWNLOAD).unwrap(),
        None,
        "a patch is not the A2 finding"
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
    let error = run_plan(plan).unwrap_err();
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
    assert_eq!(
        status.snapshots, 1,
        "the safety copy of the broken state is not kept"
    );
    // Broken again, a second repair restores the same snapshot, not the
    // discarded broken state.
    fs::remove_file(fixture.steam_dir().join("steam.exe")).unwrap();
    let status = fixture
        .core()
        .plan_client_op(ClientOp::Repair { reinstall: false })
        .unwrap()
        .run(&mut |_, _| {})
        .unwrap();
    assert!(fixture.steam_dir().join("steam.exe").is_file());
    assert_eq!(status.state, ClientRuntimeState::NeedsLogin);
    assert_eq!(status.snapshots, 1);

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
fn repair_refuses_a_client_that_is_not_broken_and_keeps_the_sign_in() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.core().prefix_snapshot(STEAM_CLIENT_PREFIX).unwrap();
    fixture.sign_in();
    fixture.stop_client();
    assert_eq!(
        fixture.core().client_runtime_state().unwrap().0,
        ClientRuntimeState::Ready
    );
    let error = fixture
        .core()
        .plan_client_op(ClientOp::Repair { reinstall: false })
        .unwrap()
        .run(&mut |_, _| {})
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientRuntimeBroken, "{error}");
    let text = error.to_string();
    assert!(text.contains("is ready: nothing to repair"), "{text}");
    assert!(
        text.contains("`applejack prefix restore steam-client <timestamp>`"),
        "{text}"
    );
    assert!(
        fixture.steam_dir().join("config/loginusers.vdf").is_file(),
        "the sign-in is still there"
    );
    assert_eq!(
        fixture.core().client_runtime_state().unwrap().0,
        ClientRuntimeState::Ready
    );
    assert_eq!(
        fixture
            .core()
            .prefix_snapshots(STEAM_CLIENT_PREFIX)
            .unwrap()
            .len(),
        1,
        "nothing restored, nothing discarded"
    );
    fixture.assert_lock_free();
}

#[test]
fn repair_over_a_dead_installs_marker_clears_it_and_restores_nothing() {
    let fixture = Fixture::new();
    fixture.install();
    let before = fixture.core().prefix_snapshot(STEAM_CLIENT_PREFIX).unwrap();
    fixture.sign_in();
    fixture.stop_client();
    let marker = fixture.prefix_dir().join("applejack/client-install.json");
    fs::write(&marker, "{}").unwrap();
    let (state, detail) = fixture.core().client_runtime_state().unwrap();
    assert_eq!(state, ClientRuntimeState::Broken, "{detail}");
    assert!(
        detail.contains("an install stopped before it finished"),
        "{detail}"
    );
    let mut lines = Vec::new();
    let status = fixture
        .core()
        .plan_client_op(ClientOp::Repair { reinstall: false })
        .unwrap()
        .run(&mut |phase, line| lines.push(format!("{phase}: {line}")))
        .unwrap();
    assert!(
        lines.contains(
            &"repair: cleared the marker of an install that died; the client's files are intact"
                .to_owned()
        ),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("restoring")),
        "{lines:?}"
    );
    assert!(!marker.exists(), "the marker is cleared");
    assert_eq!(status.state, ClientRuntimeState::Ready, "{}", status.detail);
    assert!(
        fixture.steam_dir().join("config/loginusers.vdf").is_file(),
        "the sign-in, newer than the snapshot, is still there"
    );
    let snapshots = fixture
        .core()
        .prefix_snapshots(STEAM_CLIENT_PREFIX)
        .unwrap();
    assert_eq!(status.snapshots, 1);
    assert_eq!(snapshots.len(), 1);
    assert_eq!(
        snapshots[0].timestamp, before.timestamp,
        "the one snapshot is untouched"
    );
    fixture.assert_lock_free();
}

fn repair_with_lines(fixture: &Fixture) -> (Result<ClientRuntimeStatus, CoreError>, Vec<String>) {
    let mut lines = Vec::new();
    let outcome = fixture
        .core()
        .plan_client_op(ClientOp::Repair { reinstall: false })
        .and_then(|op| op.run(&mut |phase, line| lines.push(format!("{phase}: {line}"))));
    (outcome, lines)
}

fn snapshot_stamps(fixture: &Fixture) -> Vec<String> {
    fixture
        .core()
        .prefix_snapshots(STEAM_CLIENT_PREFIX)
        .unwrap()
        .into_iter()
        .map(|snapshot| snapshot.timestamp)
        .collect()
}

#[test]
fn repair_skips_a_snapshot_whose_record_does_not_open_and_keeps_its_safety_copy() {
    let fixture = Fixture::new();
    fixture.install();
    let good = fixture.core().prefix_snapshot(STEAM_CLIENT_PREFIX).unwrap();
    fixture.sign_in();
    fixture.stop_client();
    let record = fixture.prefix_dir().join("applejack/prefix.toml");
    fs::write(&record, "this is not toml = = =").unwrap();
    let (state, detail) = fixture.core().client_runtime_state().unwrap();
    assert_eq!(state, ClientRuntimeState::Broken, "{detail}");

    // The state repaired away had `steam.exe`, so its copy stays, named.
    let (outcome, lines) = repair_with_lines(&fixture);
    let status = outcome.unwrap();
    assert_eq!(
        status.state,
        ClientRuntimeState::NeedsLogin,
        "{}",
        status.detail
    );
    let stamps = snapshot_stamps(&fixture);
    assert_eq!(stamps.len(), 2, "{stamps:?}");
    assert_eq!(stamps[0], good.timestamp);
    let kept = stamps[1].clone();
    assert!(
        lines.contains(&format!(
            "repair: keeping the safety snapshot {kept} of the state repaired away; \
             `applejack prefix restore steam-client {kept}` brings it back"
        )),
        "{lines:?}"
    );
    let kept_dir = fixture
        .data()
        .join("prefixes/.snapshots")
        .join(STEAM_CLIENT_PREFIX)
        .join(&kept);
    assert!(
        kept_dir
            .join("drive_c/Program Files (x86)/Steam/config/loginusers.vdf")
            .is_file(),
        "the copy holds the sign-in"
    );

    // A later break: the kept copy is the newest snapshot, and is not a
    // client; `repair` must say so and restore the good one.
    for _ in 0..3 {
        fs::remove_file(fixture.steam_dir().join("steam.exe")).unwrap();
        let (outcome, lines) = repair_with_lines(&fixture);
        let status = outcome.unwrap();
        assert_ne!(
            status.state,
            ClientRuntimeState::Broken,
            "{}",
            status.detail
        );
        assert_eq!(status.state, ClientRuntimeState::NeedsLogin);
        assert!(
            lines.iter().any(|line| line
                .starts_with(&format!("repair: skipping snapshot {kept}: "))
                && line.contains("prefix record does not open")),
            "{lines:?}"
        );
        assert!(
            lines.contains(&format!(
                "repair: restoring steam-client from {}",
                good.timestamp
            )),
            "{lines:?}"
        );
        assert!(fixture.steam_dir().join("steam.exe").is_file());
        assert_eq!(
            snapshot_stamps(&fixture),
            [good.timestamp.clone(), kept.clone()],
            "the no-steam.exe state's copy is discarded, the kept one stays"
        );
    }
    fixture.assert_lock_free();
}

#[test]
fn repair_over_a_marker_and_a_record_that_does_not_open_restores_after_the_marker() {
    let fixture = Fixture::new();
    fixture.install();
    let good = fixture.core().prefix_snapshot(STEAM_CLIENT_PREFIX).unwrap();
    fixture.stop_client();
    fs::write(
        fixture.prefix_dir().join("applejack/prefix.toml"),
        "nope = = =",
    )
    .unwrap();
    let marker = fixture.prefix_dir().join("applejack/client-install.json");
    fs::write(&marker, "{}").unwrap();
    let (outcome, lines) = repair_with_lines(&fixture);
    let status = outcome.unwrap();
    assert_eq!(
        status.state,
        ClientRuntimeState::NeedsLogin,
        "{}",
        status.detail
    );
    assert!(!marker.exists());
    assert!(
        lines.iter().any(|line| line.starts_with(
            "repair: cleared the marker of an install that died; the runtime is still broken: "
        )),
        "{lines:?}"
    );
    assert!(
        lines.contains(&format!(
            "repair: restoring steam-client from {}",
            good.timestamp
        )),
        "{lines:?}"
    );
    assert_eq!(
        status.snapshots, 2,
        "the copy of the state with steam.exe is kept"
    );
    fixture.assert_lock_free();
}

#[test]
fn repair_on_an_absent_runtime_points_at_install() {
    let fixture = Fixture::new();
    let (outcome, lines) = repair_with_lines(&fixture);
    let error = outcome.unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientRuntimeBroken, "{error}");
    let text = error.to_string();
    assert!(text.contains("is absent: nothing to repair"), "{text}");
    assert!(text.contains("`applejack runtime steam install`"), "{text}");
    assert!(!text.contains("prefix restore"), "{text}");
    assert!(lines.is_empty(), "{lines:?}");
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

/// The payload's shim goes in front of the client's web helper at install
/// (stashing the client's own as `steamwebhelper_real.exe`), and a start
/// that finds it there leaves both alone.
#[test]
fn install_puts_the_shim_in_front_of_the_web_helper_and_a_restart_leaves_it() {
    let fixture = Fixture::new();
    let (state, lines) = fixture.install_reporting("VALVE-SIGNED");
    assert_eq!(state.unwrap(), ClientRuntimeState::NeedsLogin);
    assert_eq!(fixture.helper(), fixture.shim());
    assert_eq!(
        fixture.real_helper().as_deref(),
        Some(&b"REAL-HELPER-1"[..])
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("shim: ") && line.contains("stashed the web helper")),
        "{lines:?}"
    );

    fixture.stop_client();
    let (state, lines) = fixture.install_reporting("VALVE-SIGNED");
    assert_eq!(state.unwrap(), ClientRuntimeState::NeedsLogin);
    assert_eq!(fixture.helper(), fixture.shim());
    assert_eq!(
        fixture.real_helper().as_deref(),
        Some(&b"REAL-HELPER-1"[..])
    );
    assert!(
        lines.iter().any(|line| line.starts_with("shim: ")
            && line.ends_with("the shim is in front of the web helper")),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("stashed")),
        "nothing restashed: {lines:?}"
    );
}

/// A client update writes its helper over the shim while the client is
/// stopped; the next start (here `update`, which snapshots and starts)
/// stashes the new helper and puts the shim back, so the stashed helper is
/// never older than the client.
#[test]
fn a_client_update_that_replaced_the_helper_is_stashed_before_the_next_start() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.stop_client();
    fs::write(fixture.cef_dir().join(WEBHELPER_EXE), b"REAL-HELPER-2").unwrap();
    let mut lines = Vec::new();
    let status = fixture
        .core()
        .plan_client_op(ClientOp::Update)
        .unwrap()
        .run(&mut |phase, message| lines.push(format!("{phase}: {message}")))
        .unwrap();
    assert_eq!(status.state, ClientRuntimeState::NeedsLogin);
    assert_eq!(fixture.helper(), fixture.shim());
    assert_eq!(
        fixture.real_helper().as_deref(),
        Some(&b"REAL-HELPER-2"[..])
    );
    assert!(
        lines.iter().any(|line| line.starts_with("shim: ")
            && line.contains("a client update replaced the web helper")),
        "{lines:?}"
    );
    assert!(fixture.client_up());
}

/// A payload upgrade brings a new shim build; the old shim on disk is
/// replaced, and never stashed over the client's helper.
#[test]
fn an_older_shim_is_replaced_and_never_stashed_as_the_real_helper() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.stop_client();
    let old_shim = b"OLD BUILD applejack steamwebhelper shim OLD BUILD";
    fs::write(fixture.cef_dir().join(WEBHELPER_EXE), old_shim).unwrap();
    let (state, lines) = fixture.install_reporting("VALVE-SIGNED");
    assert_eq!(state.unwrap(), ClientRuntimeState::NeedsLogin);
    assert_eq!(fixture.helper(), fixture.shim());
    assert_eq!(
        fixture.real_helper().as_deref(),
        Some(&b"REAL-HELPER-1"[..]),
        "the client's helper, not the old shim"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("shim: ") && line.contains("older build of the shim")),
        "{lines:?}"
    );
    fixture.stop_client();
    fs::remove_file(fixture.cef_dir().join(WEBHELPER_REAL)).unwrap();
    fs::write(fixture.cef_dir().join(WEBHELPER_EXE), old_shim).unwrap();
    let error = fixture.install_with("VALVE-SIGNED").unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientRuntimeBroken, "{error}");
    assert!(
        error
            .to_string()
            .contains("steamwebhelper_real.exe is missing"),
        "an old shim without the real helper: {error}"
    );
}

/// The shim with nothing to run beside it is a broken runtime, and the
/// message says how the helper comes back.
#[test]
fn a_shim_without_the_real_helper_is_broken_with_a_repair_hint() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.stop_client();
    fs::remove_file(fixture.cef_dir().join(WEBHELPER_REAL)).unwrap();
    let error = fixture.install_with("VALVE-SIGNED").unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientRuntimeBroken, "{error}");
    let message = error.to_string();
    assert!(
        message.contains("steamwebhelper_real.exe is missing"),
        "{message}"
    );
    assert!(
        message.contains("applejack runtime steam repair --reinstall"),
        "{message}"
    );
    assert!(!fixture.client_up(), "nothing started");
    fixture.assert_lock_free();
}

/// A self-update during the first run writes the client's helper over the
/// shim; after the window the client is stopped, the helper stashed, and
/// the client started again with the shim, so the install ends with a
/// window that draws. The stub has no server, so the stop ends nothing and
/// the first stub client lingers under its key until the final stop; the
/// product's second start and its new key are what is checked.
#[test]
fn a_self_update_during_the_first_run_restarts_the_client_with_the_shim() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.stop_client();
    fs::write(fixture.prefix_dir().join("self-update"), "").unwrap();
    let first_key = fixture.client_key();
    let (state, lines) = fixture.install_reporting("VALVE-SIGNED");
    assert_eq!(state.unwrap(), ClientRuntimeState::NeedsLogin, "{lines:?}");
    assert!(
        lines.contains(
            &"client: the self-update replaced the web helper; restarting the client with the \
              shim in front of it"
                .to_owned()
        ),
        "{lines:?}"
    );
    assert_eq!(fixture.helper(), fixture.shim());
    assert_eq!(
        fixture.real_helper().as_deref(),
        Some(&b"REAL-HELPER-2"[..])
    );
    assert_ne!(
        fixture.client_key(),
        first_key,
        "the restart is a new client session"
    );
    assert!(
        fixture.client_up(),
        "the restarted client stays up: {lines:?}"
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with("needs_login: "))
            .count(),
        1,
        "the state is reported once: {lines:?}"
    );
    assert!(
        !fixture.prefix_dir().join("self-update").exists(),
        "the stub applied its update at the first start"
    );
    fixture.stop_client();
}

/// A client whose main loop stalled on the macOS prompt and died is
/// `broken`, and the detail names the prompt rather than a bare exit.
#[test]
fn a_client_that_stalls_on_the_prompt_and_dies_names_the_prompt() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.stop_client();
    fs::write(fixture.prefix_dir().join("stall"), "").unwrap();
    let error = fixture.install_with("VALVE-SIGNED").unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientRuntimeBroken, "{error}");
    let message = error.to_string();
    assert!(
        message.contains("BMainLoop appears to have stalled"),
        "{message}"
    );
    assert!(message.contains("the microphone"), "{message}");
    assert!(message.contains("Don't Allow is fine"), "{message}");
    assert!(message.contains("logs/console_log.txt"), "{message}");
    let _ = fs::remove_file(fixture.prefix_dir().join("stall"));
    fixture.assert_lock_free();

    // The text may be nowhere yet (the client writes it to its console log
    // late); an assert dump written since the start is the same finding.
    fs::write(fixture.prefix_dir().join("stall-dump"), "").unwrap();
    let (state, lines) = fixture.install_reporting("VALVE-SIGNED");
    let error = state.unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientRuntimeBroken, "{error}");
    let message = error.to_string();
    assert!(message.contains("dumps/assert_steam.exe_"), "{message}");
    assert!(message.contains("Don't Allow is fine"), "{message}");
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("client: stalled: ") && line.contains("Don't Allow")),
        "the stall is reported while the client is still up: {lines:?}"
    );
    let _ = fs::remove_file(fixture.prefix_dir().join("stall-dump"));
    fixture.assert_lock_free();
}
