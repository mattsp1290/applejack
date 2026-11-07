//! The Steam client runtime and the client launch path over the stub bundle:
//! install with its Authenticode gate, the login state, a session that
//! starts at the client's marker and ends when it clears while the client
//! stays up, the A2 refusal and the routine patch it is not, cancel, stop,
//! repair, a dead owner's row, a stale install marker, a stale running
//! marker, and a library folder added under a running client.

mod common;

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use applejack_core::config::{ConfigDb, Drm, NewSession, SessionRow, SessionState};
use applejack_core::host::{ControllerFact, ControllerKind, Transport};
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
    first_run_timeout: Duration::from_secs(600),
};

/// [`FAST`] with a first run that gives up on the window in seconds.
const SHORT_WINDOW: RunTuning = RunTuning {
    first_run_timeout: Duration::from_secs(3),
    ..FAST
};

/// How long a run against the stub may take to end: the longest legitimate
/// one is a cancel (`cancel_wait`) followed by the drain.
const RUN_WAIT: Duration = Duration::from_secs(30);

/// A remembered account, as the client writes one after a sign-in.
const LOGINUSERS: &str = "\"users\"\n{\n\t\"76561190000000000\"\n\t{\n\t\t\"AccountName\"\t\t\"fixture\"\n\t\t\"RememberPassword\"\t\t\"1\"\n\t\t\"MostRecent\"\t\t\"1\"\n\t\t\"AllowAutoLogin\"\t\t\"1\"\n\t}\n}\n";

struct Fixture {
    dir: TempDir,
    tuning: RunTuning,
    /// The stub servers [`Fixture::server_at`] started, reaped at the end.
    servers: RefCell<Vec<std::process::Child>>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Each server left with the stop it served; a test that failed
        // first may leave one up, and the kill keeps the wait bounded.
        for mut server in self.servers.take() {
            let _ = server.kill();
            let _ = server.wait();
        }
    }
}

impl Fixture {
    /// An installed engine and a registered Steam-DRM title, no client.
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let fixture = Self {
            dir,
            tuning: FAST,
            servers: RefCell::new(Vec::new()),
        };
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
            .with_controllers(Vec::new())
            .with_run_tuning(self.tuning)
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
        self.install_hooked(content, &mut |_| {})
    }

    /// [`Fixture::install_reporting`], with `hook` seeing each line as it is
    /// reported, so a test can act between the install's steps.
    fn install_hooked(
        &self,
        content: &str,
        hook: &mut dyn FnMut(&str),
    ) -> (Result<ClientRuntimeState, CoreError>, Vec<String>) {
        Self::install_on(self.core(), content, hook)
    }

    /// [`Fixture::install_reporting`] with `pads` as the inventory the
    /// install reads.
    fn install_with_pads(
        &self,
        pads: Vec<ControllerFact>,
    ) -> (Result<ClientRuntimeState, CoreError>, Vec<String>) {
        Self::install_on(
            self.core().with_controllers(pads),
            "VALVE-SIGNED",
            &mut |_| {},
        )
    }

    fn install_on(
        mut core: Core,
        content: &str,
        hook: &mut dyn FnMut(&str),
    ) -> (Result<ClientRuntimeState, CoreError>, Vec<String>) {
        let fetch = |_url: &str, dest: &Path, progress: &mut dyn FnMut(u64, Option<u64>)| {
            fs::write(dest, content).map_err(|error| CoreError::Message(error.to_string()))?;
            progress(content.len() as u64, Some(content.len() as u64));
            Ok(())
        };
        let mut lines = Vec::new();
        let outcome = core.plan_client_op(ClientOp::Install).and_then(|plan| {
            plan.run_with(&fetch, &mut |phase, message| {
                let line = format!("{phase}: {message}");
                hook(&line);
                lines.push(line);
            })
        });
        (outcome.map(|status| status.state), lines)
    }

    /// The stub client's one CEF directory.
    fn cef_dir(&self) -> PathBuf {
        self.steam_dir().join("bin/cef/cef.win7x64")
    }

    /// Another `cef.win*` directory under the stub client's `bin/cef`.
    fn cef_dir_named(&self, name: &str) -> PathBuf {
        self.steam_dir().join("bin/cef").join(name)
    }

    /// Every `bin/cef/cef.win*` directory with a `steamwebhelper.exe` whose
    /// helper is the shim with a stash beside it.
    fn every_populated_cef_dir_is_shimmed(&self) -> bool {
        fs::read_dir(self.steam_dir().join("bin/cef"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|dir| dir.join(WEBHELPER_EXE).is_file())
            .all(|dir| {
                fs::read(dir.join(WEBHELPER_EXE)).unwrap() == self.shim()
                    && dir.join(WEBHELPER_REAL).is_file()
            })
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

    /// `repair --reinstall`, with every `phase: message` line reported.
    fn reinstall_reporting(&self) -> (Result<ClientRuntimeState, CoreError>, Vec<String>) {
        self.reinstall_hooked(&mut |_| {})
    }

    /// [`Fixture::reinstall_reporting`] with `hook` seeing each line as it
    /// is reported.
    fn reinstall_hooked(
        &self,
        hook: &mut dyn FnMut(&str),
    ) -> (Result<ClientRuntimeState, CoreError>, Vec<String>) {
        let fetch = |_: &str, dest: &Path, _: &mut dyn FnMut(u64, Option<u64>)| {
            fs::write(dest, "VALVE-SIGNED").map_err(|error| CoreError::Message(error.to_string()))
        };
        let mut lines = Vec::new();
        let outcome = self
            .core()
            .plan_client_op(ClientOp::Repair { reinstall: true })
            .and_then(|plan| {
                plan.run_with(&fetch, &mut |phase, message| {
                    let line = format!("{phase}: {message}");
                    hook(&line);
                    lines.push(line);
                })
            });
        (outcome.map(|status| status.state), lines)
    }

    /// A stub server for the product's own `stop` to end, spawned when a
    /// reported line starts with `at`: a server up before `install` or
    /// `first_run` look would read as a running client. Each `stop` ends
    /// one server, so each stop a test wants exercised needs its own.
    fn server_at(&self, at: &'static str) -> impl FnMut(&str) + '_ {
        let prefix = self.prefix_dir();
        let mut spawned = false;
        move |line: &str| {
            if !spawned && line.starts_with(at) {
                spawned = true;
                self.servers
                    .borrow_mut()
                    .push(spawn_stub_server("server", &prefix));
            }
        }
    }

    /// `runtime steam stop` through the product, with a stub server up for
    /// it to end.
    fn stop_through_product(&self) {
        let server = spawn_stub_server("server", &self.prefix_dir());
        let status = self
            .core()
            .plan_client_op(ClientOp::Stop)
            .unwrap()
            .run(&mut |_, _| {})
            .unwrap();
        wait_gone("the client", || self.client_up());
        drop(server);
        let _ = fs::remove_file(self.prefix_dir().join("client-quit"));
        let _ = fs::remove_file(self.prefix_dir().join("server-quit"));
        assert_ne!(status.state, ClientRuntimeState::Absent);
    }

    /// The stub client's argument lines, one per start, oldest first.
    fn client_starts(&self) -> Vec<String> {
        fs::read_to_string(self.prefix_dir().join("client-args"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn clear_client_starts(&self) {
        let _ = fs::remove_file(self.prefix_dir().join("client-args"));
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
        self.start_collecting(|phase, _| phase.to_owned())
    }

    /// [`Fixture::start`] keeping every `phase: message` line, not the
    /// phases alone.
    fn start_reporting(&self) -> Running {
        self.start_collecting(|phase, message| format!("{phase}: {message}"))
    }

    fn start_collecting(&self, keep: fn(&str, &str) -> String) -> Running {
        let plan = self.core().plan_run(APP, false).unwrap();
        let id = plan.session_id();
        let thread = thread::spawn(move || {
            let mut lines = Vec::new();
            let outcome = plan.run(&mut |phase, message| lines.push(keep(phase, &message)));
            (outcome, lines)
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

/// Asserts `lines` carries each of `wanted`, in that order, each matched by
/// a line that starts with it.
fn assert_in_order(lines: &[String], wanted: &[&str]) {
    let mut from = 0;
    for want in wanted {
        let found = lines[from..]
            .iter()
            .position(|line| line.starts_with(want))
            .unwrap_or_else(|| {
                panic!("no line starting with {want:?} after line {from} in {lines:#?}")
            });
        from += found + 1;
    }
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
    let starts = fixture.client_starts();
    assert_eq!(
        starts.len(),
        2,
        "the self-update's helper got a restart: {starts:?}"
    );
    assert!(
        starts
            .iter()
            .all(|args| args.ends_with("steam.exe -noverifyfiles")),
        "every start skips the client's file check, which would undo the shim: {starts:?}"
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
    assert_eq!(
        phases,
        ["prepare", "prepare", "starting", "running", "exited"]
    );
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
    assert_eq!(
        phases,
        ["prepare", "prepare", "starting", "running", "exited"]
    );
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

    // A stub server up for the reinstall's own stop to end the client.
    let server = spawn_stub_server("server", &fixture.prefix_dir());
    fs::remove_file(fixture.steam_dir().join("bootstrapped")).unwrap();
    let (state, lines) = fixture.reinstall_reporting();
    assert_eq!(state.unwrap(), ClientRuntimeState::NeedsLogin, "{lines:?}");
    assert_in_order(
        &lines,
        &["stop: stopping the Steam client", "repair: removing "],
    );
    assert!(
        fixture.steam_dir().join("bootstrapped").is_file(),
        "the bootstrapper ran again"
    );
    drop(server);
    fixture.stop_client();
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

/// The hidraw mode of the client prefix's record.
fn client_prefix_hidraw(fixture: &Fixture) -> Option<HidrawMode> {
    fixture
        .core()
        .prefix_list()
        .unwrap()
        .into_iter()
        .find(|p| p.name == STEAM_CLIENT_PREFIX)
        .unwrap()
        .hidraw
}

fn dualsense() -> ControllerFact {
    ControllerFact {
        name: "DualSense Wireless Controller".to_owned(),
        transport: Transport::Usb,
        vendor_id: "0x054c".to_owned(),
        product_id: "0x0ce6".to_owned(),
        connected: true,
        kind: ControllerKind::Dualsense,
    }
}

#[test]
fn install_decides_the_client_prefixs_hidraw_by_the_pads_connected_then() {
    let fixture = Fixture::new();
    let (state, lines) = fixture.install_with_pads(vec![dualsense()]);
    assert_eq!(state.unwrap(), ClientRuntimeState::NeedsLogin);
    assert_eq!(
        lines[..2],
        [
            "prepare: hidraw disabled (DualSense connected)",
            "prefix: creating prefix steam-client",
        ],
        "{lines:?}"
    );
    assert_eq!(client_prefix_hidraw(&fixture), Some(HidrawMode::Disabled));

    let fixture = Fixture::new();
    let (state, lines) = fixture.install_with_pads(Vec::new());
    assert_eq!(state.unwrap(), ClientRuntimeState::NeedsLogin);
    assert_eq!(
        lines[..2],
        [
            "prepare: hidraw enabled (no Sony pad connected)",
            "prefix: creating prefix steam-client",
        ],
        "{lines:?}"
    );
    assert_eq!(client_prefix_hidraw(&fixture), Some(HidrawMode::Enabled));
    // The first title run finds the rule's mode already in place.
    fixture.sign_in();
    let running = fixture.start();
    fixture.quit_game(0);
    let (outcome, phases) = running.finish();
    assert_eq!(outcome.unwrap().state, SessionState::Exited);
    assert!(
        !phases.iter().any(|phase| phase == "prefix"),
        "nothing to flip after the install: {phases:?}"
    );
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
    fixture
        .core()
        .config_set(APP, "hidraw_enabled", "false")
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
    assert_eq!(
        prefix.hidraw,
        Some(HidrawMode::Enabled),
        "the install's mode stays while the client owns the prefix"
    );
    drop(server);
}

/// A fresh install in the live shape: the bootstrapper lays no CEF tree, so
/// the first start has nothing to shim; the self-update lays the client's
/// helper down (a window that would be black), the client is restarted
/// with the helper stashed and the shim in front, and a later start that
/// finds the shim there leaves both alone.
#[test]
fn install_puts_the_shim_in_front_of_the_web_helper_and_a_restart_leaves_it() {
    let fixture = Fixture::new();
    let (state, lines) = fixture.install_reporting("VALVE-SIGNED");
    assert_eq!(state.unwrap(), ClientRuntimeState::NeedsLogin, "{lines:?}");
    assert_eq!(fixture.helper(), fixture.shim());
    assert_eq!(
        fixture.real_helper().as_deref(),
        Some(&b"REAL-HELPER-1"[..])
    );
    assert_in_order(
        &lines,
        &[
            "bootstrap: running SteamSetup.exe /S",
            "library: listing S:\\ in libraryfolders.vdf before the client starts",
            "shim: no bin/cef/cef.win* directory yet; the client's self-update brings it",
            "start: macOS may ask",
            "start: starting steam.exe for its self-update",
            "client: window: the login window exists (logs/webhelper.txt)",
            "client: the client laid its web helper down without the shim in front of it",
            "stop: stopping the Steam client",
            "shim: ",
            "start: macOS may ask",
            "start: starting steam.exe for its self-update",
            "client: window: the login window exists (logs/webhelper.txt)",
            "build: client build 1761720601",
            "needs_login: ",
        ],
    );
    let shim_lines: Vec<&String> = lines
        .iter()
        .filter(|line| line.starts_with("shim: "))
        .collect();
    assert_eq!(shim_lines.len(), 2, "{lines:?}");
    assert!(
        shim_lines[1].ends_with(
            "stashed the web helper as steamwebhelper_real.exe and put the shim in front of it"
        ),
        "applied, not reapplied: {lines:?}"
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
    let (state, detail) = fixture.core().client_runtime_state().unwrap();
    assert_eq!(state, ClientRuntimeState::Broken, "{detail}");
    assert!(
        detail.contains("steamwebhelper_real.exe is missing"),
        "an old shim without the real helper is an orphan: {detail}"
    );
    let (state, lines) = fixture.install_reporting("VALVE-SIGNED");
    assert_eq!(state.unwrap(), ClientRuntimeState::NeedsLogin, "{lines:?}");
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("shim: ") && line.contains("removed an orphan shim")),
        "{lines:?}"
    );
    assert_eq!(fixture.helper(), fixture.shim());
    assert_eq!(
        fixture.real_helper().as_deref(),
        Some(&b"REAL-HELPER-1"[..])
    );
}

/// The shim with nothing to run beside it is `broken` for `status`, `repair`
/// and `install` alike. `repair` with no snapshot names the way out;
/// `install` is it: the orphan shim is removed, the client started once
/// without `-noverifyfiles` so its file check lays the helper down, and the
/// client restarted with the helper stashed and the shim in front.
#[test]
fn a_shim_without_the_real_helper_is_broken_and_install_lays_the_helper_down_again() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.stop_client();
    fs::remove_file(fixture.cef_dir().join(WEBHELPER_REAL)).unwrap();
    let (state, detail) = fixture.core().client_runtime_state().unwrap();
    assert_eq!(state, ClientRuntimeState::Broken, "{detail}");
    assert!(
        detail.contains("steamwebhelper_real.exe is missing beside it"),
        "{detail}"
    );
    assert!(
        detail.contains("`applejack runtime steam install` removes the orphan shim"),
        "{detail}"
    );
    let status = fixture.core().client_runtime_status().unwrap();
    assert_eq!(status.state, ClientRuntimeState::Broken);
    assert_eq!(status.detail, detail);
    let error = fixture.core().plan_run(APP, false).unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientRuntimeBroken, "{error}");
    assert!(
        error
            .to_string()
            .contains("`applejack runtime steam status` shows it"),
        "{error}"
    );

    let (outcome, lines) = repair_with_lines(&fixture);
    let error = outcome.unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientRuntimeBroken, "{error}");
    let message = error.to_string();
    assert!(
        message.contains("no snapshot of steam-client exists"),
        "{message}"
    );
    assert!(message.contains("repair --reinstall"), "{message}");
    assert!(
        !lines.iter().any(|line| line.contains("restoring")),
        "nothing restored: {lines:?}"
    );
    assert!(
        fixture.cef_dir().join(WEBHELPER_EXE).is_file(),
        "repair touched nothing"
    );
    fixture.assert_lock_free();

    fixture.clear_client_starts();
    let (state, lines) = fixture.install_reporting("VALVE-SIGNED");
    assert_eq!(state.unwrap(), ClientRuntimeState::NeedsLogin, "{lines:?}");
    let cef = fixture.cef_dir().display().to_string();
    assert_in_order(
        &lines,
        &[
            &format!(
                "shim: {cef}: removed an orphan shim; the client's file check lays its helper \
                 down at this start"
            ),
            "start: starting steam.exe for its self-update, without -noverifyfiles: a web \
             helper is missing and the client's file check lays it down",
            "client: window: ",
            "client: the client laid its web helper down without the shim in front of it",
            "stop: stopping the Steam client",
            &format!(
                "shim: {cef}: stashed the web helper as steamwebhelper_real.exe and put the shim \
                 in front of it"
            ),
            "start: starting steam.exe for its self-update",
            "client: window: ",
            "needs_login: ",
        ],
    );
    let starts = fixture.client_starts();
    assert_eq!(starts.len(), 2, "{starts:?}");
    assert!(
        starts[0].ends_with("steam.exe"),
        "the first start runs the client's file check: {starts:?}"
    );
    assert!(
        starts[1].ends_with("steam.exe -noverifyfiles"),
        "the restart skips it again: {starts:?}"
    );
    assert_eq!(fixture.helper(), fixture.shim());
    assert_eq!(
        fixture.real_helper().as_deref(),
        Some(&b"REAL-HELPER-1"[..])
    );
    assert_eq!(
        fixture.core().client_runtime_state().unwrap().0,
        ClientRuntimeState::NeedsLogin
    );
    assert!(fixture.client_up());
    fixture.stop_client();
}

/// The orphan recovery when the client's file check does not lay the
/// helper down (the live client has not been seen to; the stub's
/// `no-file-check` knob is that shape): the start without `-noverifyfiles`
/// runs to the window timeout, the error names `repair --reinstall` rather
/// than a self-update to wait for, the directory is left with neither
/// helper file and reads `broken` naming both ways out, a run is refused,
/// and `repair --reinstall` recovers through the self-update, whose first
/// start lays the helper regardless of the flag.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one recovery from start to end, in the order the user sees it"
)]
fn a_file_check_that_lays_no_helper_times_out_naming_reinstall_and_reinstall_recovers() {
    let mut fixture = Fixture::new();
    fixture.install();
    fixture.stop_through_product();
    fs::remove_file(fixture.cef_dir().join(WEBHELPER_REAL)).unwrap();
    fs::write(fixture.prefix_dir().join("no-file-check"), "").unwrap();
    assert_eq!(
        fixture.core().client_runtime_state().unwrap().0,
        ClientRuntimeState::Broken
    );
    fixture.tuning = SHORT_WINDOW;
    fixture.clear_client_starts();
    let cef = fixture.cef_dir().display().to_string();
    // The server for the timeout's stop goes up once the client is being
    // started; up before, `install` would read it as a running client.
    let (outcome, lines) = fixture.install_hooked(
        "VALVE-SIGNED",
        &mut fixture.server_at("start: starting steam.exe for its self-update, without"),
    );
    let error = outcome.unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientRuntimeBroken, "{error}");
    assert!(
        matches!(
            &error,
            CoreError::ClientRuntimeBroken {
                state: ClientRuntimeState::Broken,
                ..
            }
        ),
        "{error}"
    );
    let message = error.to_string();
    assert!(
        message.contains(&format!(
            "{cef} still has no steamwebhelper.exe: the directory held no web helper (an \
             orphan shim was removed, or none was there) and the client's file check did not \
             lay one down within 3 seconds"
        )),
        "{message}"
    );
    assert!(
        message.contains("`applejack runtime steam repair --reinstall`"),
        "{message}"
    );
    assert!(
        !message.contains("wait for the client's self-update"),
        "nothing is updating: {message}"
    );
    assert!(
        message.contains("`applejack runtime steam status` shows it"),
        "{message}"
    );
    assert_in_order(
        &lines,
        &[
            &format!(
                "shim: {cef}: removed an orphan shim; the client's file check lays its helper \
                 down at this start"
            ),
            "start: starting steam.exe for its self-update, without -noverifyfiles: a web \
             helper is missing and the client's file check lays it down",
            "stop: stopping the Steam client",
        ],
    );
    let starts = fixture.client_starts();
    assert_eq!(starts.len(), 1, "{starts:?}");
    assert!(
        starts[0].ends_with("steam.exe"),
        "one start, with the file check on: {starts:?}"
    );
    assert!(!fixture.cef_dir().join(WEBHELPER_EXE).exists());
    assert!(fixture.real_helper().is_none());
    wait_gone("the client", || fixture.client_up());

    let (state, detail) = fixture.core().client_runtime_state().unwrap();
    assert_eq!(state, ClientRuntimeState::Broken, "{detail}");
    assert!(
        detail.starts_with(&format!(
            "{cef} has neither steamwebhelper.exe nor steamwebhelper_real.exe"
        )),
        "{detail}"
    );
    assert!(
        detail
            .contains("`applejack runtime steam install` starts the client with its file check on"),
        "{detail}"
    );
    assert!(
        detail.contains("`applejack runtime steam repair --reinstall` removes Steam/package/"),
        "{detail}"
    );
    let error = fixture.core().plan_run(APP, false).unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientRuntimeBroken, "{error}");
    assert!(error.to_string().contains("has neither"), "{error}");

    fixture.clear_client_starts();
    let (state, lines) =
        fixture.reinstall_hooked(&mut fixture.server_at("client: the client laid its web helper"));
    assert_eq!(state.unwrap(), ClientRuntimeState::NeedsLogin, "{lines:?}");
    assert_in_order(
        &lines,
        &[
            "stop: the client is not running",
            "repair: removing ",
            "bootstrap: running SteamSetup.exe /S",
            &format!("shim: {cef}: no steamwebhelper.exe; the client's self-update lays it down"),
            "start: starting steam.exe for its self-update, without -noverifyfiles: a web \
             helper is missing and the client's self-update lays it down",
            "client: window: ",
            "client: the client laid its web helper down without the shim in front of it",
            "stop: stopping the Steam client",
            &format!(
                "shim: {cef}: stashed the web helper as steamwebhelper_real.exe and put the shim \
                 in front of it"
            ),
            "start: starting steam.exe for its self-update",
            "client: window: ",
            "needs_login: ",
        ],
    );
    assert_eq!(fixture.helper(), fixture.shim());
    assert_eq!(
        fixture.real_helper().as_deref(),
        Some(&b"REAL-HELPER-1"[..])
    );
    assert_eq!(
        fixture.core().client_runtime_state().unwrap().0,
        ClientRuntimeState::NeedsLogin
    );
    assert!(fixture.client_up());
    fixture.stop_client();
}

/// `repair --reinstall`, the way out `NoWebHelper` names, starts the
/// client's self-update with no manifest; when that does not reach the
/// window within the timeout (the stub's `no-self-update` knob: the
/// self-update lays nothing), the error is the plain `updating` timeout,
/// not the file-check one (nothing was removed, nothing checked, and
/// `repair --reinstall` again would restart the whole download), `status`
/// reads `updating`, and `install` resumes the self-update to the window.
#[test]
fn a_reinstall_whose_self_update_times_out_stays_updating_and_install_resumes_it() {
    let mut fixture = Fixture::new();
    fixture.install();
    fixture.stop_through_product();
    fs::remove_file(fixture.cef_dir().join(WEBHELPER_EXE)).unwrap();
    fs::remove_file(fixture.cef_dir().join(WEBHELPER_REAL)).unwrap();
    fs::write(fixture.prefix_dir().join("no-file-check"), "").unwrap();
    fs::write(fixture.prefix_dir().join("no-self-update"), "").unwrap();
    assert_eq!(
        fixture.core().client_runtime_state().unwrap().0,
        ClientRuntimeState::Broken
    );
    fixture.tuning = SHORT_WINDOW;
    fixture.clear_client_starts();
    let cef = fixture.cef_dir().display().to_string();
    let (outcome, lines) = fixture.reinstall_hooked(
        &mut fixture.server_at("start: starting steam.exe for its self-update, without"),
    );
    let error = outcome.unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientRuntimeBroken, "{error}");
    assert!(
        matches!(
            &error,
            CoreError::ClientRuntimeBroken {
                state: ClientRuntimeState::Updating,
                ..
            }
        ),
        "the self-update's timeout is not the file check's: {error}"
    );
    let message = error.to_string();
    assert!(
        message.contains("the client did not reach the login window within 3 seconds"),
        "{message}"
    );
    assert!(
        message.contains("wait for the client's self-update"),
        "{message}"
    );
    assert!(
        !message.contains("still has no steamwebhelper.exe") && !message.contains("removed"),
        "{message}"
    );
    assert!(!message.contains("repair --reinstall"), "{message}");
    assert_in_order(
        &lines,
        &[
            "repair: removing ",
            "bootstrap: running SteamSetup.exe /S",
            &format!("shim: {cef}: no steamwebhelper.exe; the client's self-update lays it down"),
            "start: starting steam.exe for its self-update, without -noverifyfiles: a web \
             helper is missing and the client's self-update lays it down",
            "stop: stopping the Steam client",
        ],
    );
    assert_eq!(fixture.client_starts().len(), 1);
    assert!(!fixture.steam_dir().join("package").exists());
    wait_gone("the client", || fixture.client_up());
    let (state, detail) = fixture.core().client_runtime_state().unwrap();
    assert_eq!(state, ClientRuntimeState::Updating, "{detail}");
    assert_eq!(detail, "the client has not finished its first self-update");

    fs::remove_file(fixture.prefix_dir().join("no-self-update")).unwrap();
    fixture.clear_client_starts();
    let (state, lines) = fixture.install_hooked(
        "VALVE-SIGNED",
        &mut fixture.server_at("client: the client laid its web helper"),
    );
    assert_eq!(state.unwrap(), ClientRuntimeState::NeedsLogin, "{lines:?}");
    assert_in_order(
        &lines,
        &[
            &format!("shim: {cef}: no steamwebhelper.exe; the client's self-update lays it down"),
            "start: starting steam.exe for its self-update, without -noverifyfiles: a web \
             helper is missing and the client's self-update lays it down",
            "client: window: ",
            "client: the client laid its web helper down without the shim in front of it",
            "stop: stopping the Steam client",
            &format!(
                "shim: {cef}: stashed the web helper as steamwebhelper_real.exe and put the shim \
                 in front of it"
            ),
            "start: starting steam.exe for its self-update",
            "client: window: ",
            "needs_login: ",
        ],
    );
    assert!(
        !lines.iter().any(|line| line.starts_with("bootstrap:")),
        "{lines:?}"
    );
    assert_eq!(fixture.helper(), fixture.shim());
    assert_eq!(
        fixture.real_helper().as_deref(),
        Some(&b"REAL-HELPER-1"[..])
    );
    assert_eq!(
        fixture.core().client_runtime_state().unwrap().0,
        ClientRuntimeState::NeedsLogin
    );
    assert!(fixture.client_up());
    fixture.stop_client();
}

/// A `cef.win*` directory with no entries beside the one that holds the
/// helper is a leftover, not a client without its helper: `status` does not
/// read it as `broken`, and `install` skips it (saying so), starts the
/// client with `-noverifyfiles`, and reaches the window.
#[test]
fn an_empty_cef_directory_beside_the_helper_is_skipped_by_status_and_install() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.stop_through_product();
    let empty = fixture.steam_dir().join("bin/cef/cef.win10x64");
    fs::create_dir(&empty).unwrap();
    let (state, detail) = fixture.core().client_runtime_state().unwrap();
    assert_eq!(state, ClientRuntimeState::NeedsLogin, "{detail}");
    fixture.clear_client_starts();
    let (state, lines) = fixture.install_hooked("VALVE-SIGNED", &mut |_| {});
    assert_eq!(state.unwrap(), ClientRuntimeState::NeedsLogin, "{lines:?}");
    assert_in_order(
        &lines,
        &[
            &format!(
                "shim: {}: an empty directory beside one that holds the web helper; skipped",
                empty.display()
            ),
            &format!(
                "shim: {}: the shim is in front of the web helper",
                fixture.cef_dir().display()
            ),
            "start: starting steam.exe for its self-update",
            "needs_login: ",
        ],
    );
    assert!(
        !lines
            .iter()
            .any(|line| line.contains("without -noverifyfiles")),
        "{lines:?}"
    );
    let starts = fixture.client_starts();
    assert_eq!(starts.len(), 1, "{starts:?}");
    assert!(starts[0].ends_with("-noverifyfiles"), "{starts:?}");
    assert!(empty.is_dir());
    assert_eq!(
        fixture.core().client_runtime_state().unwrap().0,
        ClientRuntimeState::NeedsLogin
    );
    assert!(fixture.client_up());
    fixture.stop_client();
}

/// A run planned on a `ready` client whose web helper went missing before
/// the shim pass (the race `plan_run` cannot see) is refused at the pass
/// with the same reason `status` reads, and starts no client.
#[test]
fn a_run_whose_shim_pass_finds_no_web_helper_refuses_instead_of_starting_the_client() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.sign_in();
    fixture.stop_through_product();
    let plan = fixture.core().plan_run(APP, false).unwrap();
    fs::remove_file(fixture.cef_dir().join(WEBHELPER_EXE)).unwrap();
    fs::remove_file(fixture.cef_dir().join(WEBHELPER_REAL)).unwrap();
    fixture.clear_client_starts();
    let error = run_plan(plan).unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientRuntimeBroken, "{error}");
    let text = error.to_string();
    assert!(
        text.contains(&format!(
            "{} has neither steamwebhelper.exe nor steamwebhelper_real.exe",
            fixture.cef_dir().display()
        )),
        "{text}"
    );
    assert!(fixture.client_starts().is_empty(), "no client was started");
    assert_eq!(fixture.rows().len(), 1);
    assert_eq!(fixture.rows()[0].state, SessionState::Failed, "{text}");
}

/// `repair` skips a snapshot holding a `cef.win*` directory with neither
/// helper file, as it skips one holding the orphan, and restores the one
/// that holds helper and stash.
#[test]
fn repair_skips_a_snapshot_with_no_web_helper() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.core().prefix_snapshot(STEAM_CLIENT_PREFIX).unwrap();
    fixture.stop_through_product();
    fs::remove_file(fixture.cef_dir().join(WEBHELPER_EXE)).unwrap();
    fs::remove_file(fixture.cef_dir().join(WEBHELPER_REAL)).unwrap();
    let (state, detail) = fixture.core().client_runtime_state().unwrap();
    assert_eq!(state, ClientRuntimeState::Broken, "{detail}");
    let bare = fixture.core().prefix_snapshot(STEAM_CLIENT_PREFIX).unwrap();
    let (outcome, lines) = repair_with_lines(&fixture);
    let status = outcome.unwrap();
    assert_eq!(status.state, ClientRuntimeState::NeedsLogin, "{lines:?}");
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("repair: skipping snapshot ")
                && line.contains(&bare.timestamp)
                && line.contains("holds no web helper")),
        "{lines:?}"
    );
    assert_eq!(fixture.helper(), fixture.shim());
    assert_eq!(
        fixture.real_helper().as_deref(),
        Some(&b"REAL-HELPER-1"[..])
    );
}

/// `repair --reinstall` on the same state, against the live shape (the
/// bootstrapper lays no helper): the orphan shim is removed at the start,
/// the self-update lays the helper, and the restart stashes it.
#[test]
fn repair_reinstall_recovers_a_shim_without_the_real_helper() {
    let fixture = Fixture::new();
    fixture.install();
    fs::remove_file(fixture.cef_dir().join(WEBHELPER_REAL)).unwrap();
    fs::remove_file(fixture.steam_dir().join("bootstrapped")).unwrap();
    // One server for the reinstall's stop of the client left at the window,
    // one for the restart's stop after the self-update laid the helper.
    let server = spawn_stub_server("server", &fixture.prefix_dir());
    let (state, lines) =
        fixture.reinstall_hooked(&mut fixture.server_at("client: the client laid its web helper"));
    assert_eq!(state.unwrap(), ClientRuntimeState::NeedsLogin, "{lines:?}");
    assert!(
        fixture.steam_dir().join("bootstrapped").is_file(),
        "the bootstrapper ran again"
    );
    let cef = fixture.cef_dir().display().to_string();
    assert_in_order(
        &lines,
        &[
            "stop: stopping the Steam client",
            "repair: removing ",
            "bootstrap: running SteamSetup.exe /S",
            &format!(
                "shim: {cef}: removed an orphan shim; the client's self-update lays its helper \
                 down"
            ),
            "start: starting steam.exe for its self-update, without -noverifyfiles: a web \
             helper is missing and the client's self-update lays it down",
            "client: the client laid its web helper down without the shim in front of it",
            "stop: stopping the Steam client",
            "shim: ",
            "needs_login: ",
        ],
    );
    drop(server);
    assert_eq!(fixture.helper(), fixture.shim());
    assert_eq!(
        fixture.real_helper().as_deref(),
        Some(&b"REAL-HELPER-1"[..])
    );
    assert!(fixture.client_up());
    fixture.stop_client();
}

/// `repair` restores a snapshot that holds the helper and its stash over a
/// shim without the real helper, and skips one that holds the orphan.
#[test]
fn repair_restores_a_snapshot_over_a_shim_without_the_real_helper() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.core().prefix_snapshot(STEAM_CLIENT_PREFIX).unwrap();
    fixture.stop_client();
    fs::remove_file(fixture.cef_dir().join(WEBHELPER_REAL)).unwrap();
    let orphaned = fixture.core().prefix_snapshot(STEAM_CLIENT_PREFIX).unwrap();
    let (outcome, lines) = repair_with_lines(&fixture);
    let status = outcome.unwrap();
    assert_eq!(status.state, ClientRuntimeState::NeedsLogin, "{lines:?}");
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("repair: skipping snapshot ")
                && line.contains(&orphaned.timestamp)
                && line.contains("with no steamwebhelper_real.exe beside it")),
        "{lines:?}"
    );
    assert_eq!(fixture.helper(), fixture.shim());
    assert_eq!(
        fixture.real_helper().as_deref(),
        Some(&b"REAL-HELPER-1"[..])
    );
}

/// A stash that is itself a shim (a pre-marker shim stashed by an older
/// build's hash rule) would be run by the shim in a loop; it is removed and
/// the helper laid down again.
#[test]
fn a_stash_that_is_a_shim_is_removed_and_the_helper_laid_down_again() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.stop_client();
    fs::write(
        fixture.cef_dir().join(WEBHELPER_REAL),
        b"OLD BUILD applejack steamwebhelper shim OLD BUILD",
    )
    .unwrap();
    assert_eq!(
        fixture.core().client_runtime_state().unwrap().0,
        ClientRuntimeState::Broken
    );
    let (state, lines) = fixture.install_reporting("VALVE-SIGNED");
    assert_eq!(state.unwrap(), ClientRuntimeState::NeedsLogin, "{lines:?}");
    assert_in_order(
        &lines,
        &[
            "shim: ",
            "shim: ",
            "start: starting steam.exe for its self-update, without -noverifyfiles",
        ],
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("removed a stashed steamwebhelper_real.exe")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("removed an orphan shim")),
        "{lines:?}"
    );
    assert_eq!(fixture.helper(), fixture.shim());
    assert_eq!(
        fixture.real_helper().as_deref(),
        Some(&b"REAL-HELPER-1"[..])
    );
    fixture.stop_client();
}

/// A stash with nothing in front of it gets the shim back, with no file
/// check and nothing restashed.
#[test]
fn a_missing_helper_in_front_of_the_stash_gets_the_shim_back() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.stop_client();
    fs::remove_file(fixture.cef_dir().join(WEBHELPER_EXE)).unwrap();
    assert_eq!(
        fixture.core().client_runtime_state().unwrap().0,
        ClientRuntimeState::NeedsLogin,
        "a missing shim is not an orphan"
    );
    fixture.clear_client_starts();
    let (state, lines) = fixture.install_reporting("VALVE-SIGNED");
    assert_eq!(state.unwrap(), ClientRuntimeState::NeedsLogin, "{lines:?}");
    assert!(
        lines.iter().any(|line| line.starts_with("shim: ")
            && line.ends_with("put the shim back in front of the stashed web helper")),
        "{lines:?}"
    );
    assert_eq!(fixture.client_starts().len(), 1, "no restart: {lines:?}");
    assert_eq!(fixture.helper(), fixture.shim());
    assert_eq!(
        fixture.real_helper().as_deref(),
        Some(&b"REAL-HELPER-1"[..])
    );
    fixture.stop_client();
}

/// `install` with the client up is the login step; with a helper that is
/// not the shim (a client started outside applejack wrote it back), the
/// window is black and the output says what to do.
#[test]
fn install_with_the_client_up_refuses_when_the_helper_was_written_back_over_the_shim() {
    let fixture = Fixture::new();
    fixture.install();
    fs::write(fixture.cef_dir().join(WEBHELPER_EXE), b"REAL-HELPER-2").unwrap();
    let cef = fixture.cef_dir().display().to_string();
    let (state, detail) = fixture.core().client_runtime_state().unwrap();
    assert_eq!(state, ClientRuntimeState::Broken, "{detail}");
    assert!(
        detail.starts_with(&format!(
            "{cef}/steamwebhelper.exe is the client's own web helper with steamwebhelper_real.exe \
             stashed beside it"
        )),
        "{detail}"
    );
    let (state, lines) = fixture.install_reporting("VALVE-SIGNED");
    let error = state.unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientRuntimeBroken, "{error}");
    assert!(error.to_string().contains(&detail), "{error}\n{detail}");
    assert!(
        lines.iter().any(|line| line.starts_with(&format!(
            "shim: {cef}: the web helper running is the client's own (the client wrote its web \
             helper back over the shim; the stash is beside it), so the window cannot draw; \
             `applejack runtime steam stop`, then `install` or `repair`"
        ))),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.starts_with("client: window")),
        "no wait for a window that cannot draw: {lines:?}"
    );
    assert_eq!(
        fixture.helper(),
        b"REAL-HELPER-2",
        "nothing is written under a running client"
    );
    fixture.stop_client();
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
    let cef = fixture.cef_dir().display().to_string();
    assert!(
        lines.contains(&format!(
            "client: the client laid its web helper down without the shim in front of it \
             ({cef}); restarting the client with the shim in front of it (restart 1 of 2)"
        )),
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

/// The live client made `bin/cef/cef.win64` at its second start, after the
/// shim pass had covered the `cef.win7*` directories, and ran that helper
/// unshimmed. The scan after every start catches it: the first restart
/// stashes the helper the self-update wrote back, the second the helper in
/// the new directory, and the install ends with every populated `cef.win*`
/// shimmed.
#[test]
fn a_cef_directory_the_client_makes_at_a_later_start_gets_the_shim_by_another_restart() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.stop_client();
    fixture.clear_client_starts();
    fs::write(fixture.prefix_dir().join("self-update"), "").unwrap();
    fs::write(fixture.prefix_dir().join("late-cef-win64"), "").unwrap();
    let cef = fixture.cef_dir().display().to_string();
    let win64 = fixture.cef_dir_named("cef.win64");
    let (state, lines) = fixture.install_reporting("VALVE-SIGNED");
    assert_eq!(state.unwrap(), ClientRuntimeState::NeedsLogin, "{lines:?}");
    assert_in_order(
        &lines,
        &[
            "start: starting steam.exe for its self-update",
            "client: window: ",
            &format!(
                "client: the client laid its web helper down without the shim in front of it \
                 ({cef}); restarting the client with the shim in front of it (restart 1 of 2)"
            ),
            "stop: stopping the Steam client",
            &format!(
                "shim: {cef}: a client update replaced the web helper; stashed the new one and \
                 put the shim back"
            ),
            "start: starting steam.exe for its self-update",
            "client: window: ",
            &format!(
                "client: the client laid its web helper down without the shim in front of it \
                 ({}); restarting the client with the shim in front of it (restart 2 of 2)",
                win64.display()
            ),
            "stop: stopping the Steam client",
            &format!(
                "shim: {}: stashed the web helper as steamwebhelper_real.exe and put the shim in \
                 front of it",
                win64.display()
            ),
            &format!("shim: {cef}: the shim is in front of the web helper"),
            "start: starting steam.exe for its self-update",
            "client: window: ",
            "needs_login: ",
        ],
    );
    assert_eq!(fixture.client_starts().len(), 3, "{lines:?}");
    assert!(fixture.every_populated_cef_dir_is_shimmed());
    assert_eq!(
        fs::read(win64.join(WEBHELPER_REAL)).unwrap(),
        b"REAL-HELPER-64"
    );
    assert_eq!(
        fixture.real_helper().as_deref(),
        Some(&b"REAL-HELPER-2"[..])
    );
    assert_eq!(
        fixture.core().client_runtime_state().unwrap().0,
        ClientRuntimeState::NeedsLogin
    );
    assert!(fixture.client_up());
    fixture.stop_client();
}

/// A client that lays a new unshimmed directory down at every start is
/// restarted twice and no more: the install ends `broken` naming the
/// directory that kept appearing, `status` reads the same, and once the
/// client stops doing that, `install` shims every directory it left.
#[test]
fn a_directory_the_client_makes_at_every_start_ends_the_install_broken_after_two_restarts() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.stop_client();
    fixture.clear_client_starts();
    let knob = fixture.prefix_dir().join("new-cef-dir-every-start");
    fs::write(&knob, "").unwrap();
    let third = fixture.cef_dir_named("cef.win-extra-3");
    let (state, lines) = fixture.install_reporting("VALVE-SIGNED");
    let error = state.unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientRuntimeBroken, "{error}");
    let message = error.to_string();
    assert!(
        message.contains(&format!(
            "{}/steamwebhelper.exe is the client's own web helper with no steamwebhelper_real.exe \
             beside it",
            third.display()
        )),
        "{message}"
    );
    assert!(
        message.contains("`applejack runtime steam stop` if the client is running, then `applejack runtime steam install`"),
        "{message}"
    );
    assert!(
        message.contains("(the client made the directory after the shim pass; no stash beside it)"),
        "{message}"
    );
    assert!(
        message.contains("`applejack runtime steam status` shows it"),
        "{message}"
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with("client: the client laid its web helper down"))
            .count(),
        2,
        "{lines:?}"
    );
    assert!(
        lines.contains(&format!(
            "client: {}/steamwebhelper.exe is still not the shim after 2 restarts; giving up",
            third.display()
        )),
        "{lines:?}"
    );
    assert_eq!(fixture.client_starts().len(), 3, "{lines:?}");
    assert!(fixture.client_up(), "the client is left running: {lines:?}");
    let (state, detail) = fixture.core().client_runtime_state().unwrap();
    assert_eq!(state, ClientRuntimeState::Broken, "{detail}");
    assert!(detail.starts_with(&third.display().to_string()), "{detail}");
    fixture.assert_lock_free();

    fs::remove_file(&knob).unwrap();
    fixture.stop_client();
    fixture.clear_client_starts();
    let (state, lines) = fixture.install_reporting("VALVE-SIGNED");
    assert_eq!(state.unwrap(), ClientRuntimeState::NeedsLogin, "{lines:?}");
    assert_eq!(fixture.client_starts().len(), 1, "{lines:?}");
    assert!(fixture.every_populated_cef_dir_is_shimmed());
    assert!(
        !lines.iter().any(|line| line.starts_with("bootstrap:")),
        "nothing re-downloaded: {lines:?}"
    );
    fixture.stop_client();
}

/// A populated `cef.win*` directory with the client's own helper in front
/// and nothing stashed beside it reads `broken`, running client or not,
/// since that helper draws no window; `install` under the running client
/// says so and refuses, and `install` after a stop shims it without a
/// download.
#[test]
fn an_unshimmed_helper_in_a_populated_cef_directory_reads_broken_and_install_shims_it() {
    let fixture = Fixture::new();
    fixture.install();
    let win64 = fixture.cef_dir_named("cef.win64");
    fs::create_dir_all(&win64).unwrap();
    fs::write(win64.join(WEBHELPER_EXE), b"REAL-HELPER-64").unwrap();
    let mut server = spawn_stub_server("server", &fixture.prefix_dir());
    let status = fixture.core().client_runtime_status().unwrap();
    assert_eq!(
        status.state,
        ClientRuntimeState::Broken,
        "{}",
        status.detail
    );
    assert!(status.running, "{status:?}");
    assert!(
        status.detail.starts_with(&format!(
            "{}/steamwebhelper.exe is the client's own web helper with no \
             steamwebhelper_real.exe beside it",
            win64.display()
        )),
        "{}",
        status.detail
    );
    assert!(
        status.detail.contains("`applejack runtime steam stop`")
            && status.detail.contains("`applejack runtime steam install`"),
        "{}",
        status.detail
    );
    let (state, lines) = fixture.install_reporting("VALVE-SIGNED");
    let error = state.unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientRuntimeBroken, "{error}");
    assert!(
        error
            .to_string()
            .contains("steamwebhelper.exe is the client's own web helper"),
        "{error}"
    );
    assert!(
        lines.iter().any(|line| line.starts_with(&format!(
            "shim: {}: the web helper running is the client's own (the client made the \
             directory after the shim pass; no stash beside it), so the window cannot draw; \
             `applejack runtime steam stop`, then `install` or `repair`",
            win64.display()
        ))),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.starts_with("client: window")),
        "no wait for a window that cannot draw: {lines:?}"
    );
    assert_eq!(
        fs::read(win64.join(WEBHELPER_EXE)).unwrap(),
        b"REAL-HELPER-64",
        "nothing is written under a running client"
    );
    // One stub server per prefix: this one leaves before the product's
    // `stop` gets its own.
    fs::write(fixture.prefix_dir().join("server-quit"), "").unwrap();
    server.wait().unwrap();
    let _ = fs::remove_file(fixture.prefix_dir().join("server-quit"));
    fixture.stop_through_product();
    let (state, detail) = fixture.core().client_runtime_state().unwrap();
    assert_eq!(state, ClientRuntimeState::Broken, "{detail}");

    let (state, lines) = fixture.install_reporting("VALVE-SIGNED");
    assert_eq!(state.unwrap(), ClientRuntimeState::NeedsLogin, "{lines:?}");
    assert!(
        !lines
            .iter()
            .any(|line| line.starts_with("bootstrap:") || line.starts_with("download:")),
        "nothing re-downloaded: {lines:?}"
    );
    assert!(
        lines.contains(&format!(
            "shim: {}: stashed the web helper as steamwebhelper_real.exe and put the shim in \
             front of it",
            win64.display()
        )),
        "{lines:?}"
    );
    assert!(fixture.every_populated_cef_dir_is_shimmed());
    assert_eq!(
        fs::read(win64.join(WEBHELPER_REAL)).unwrap(),
        b"REAL-HELPER-64"
    );
    assert_eq!(
        fixture.core().client_runtime_state().unwrap().0,
        ClientRuntimeState::NeedsLogin
    );
    fixture.stop_client();
}

/// The other unshimmed shape: the client writes its helper back over the
/// shim at every start, so the stash is beside it each time. After the two
/// restarts the error names that shape (never a missing stash) and `status`
/// reads the same `broken` reason; `install` once the client writes nothing
/// back restashes it (`Reapplied`).
#[test]
fn a_helper_the_client_writes_back_at_every_start_ends_the_install_broken_naming_the_stash() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.stop_client();
    fixture.clear_client_starts();
    let knob = fixture.prefix_dir().join("self-update-always");
    fs::write(&knob, "").unwrap();
    let cef = fixture.cef_dir().display().to_string();
    let (state, lines) = fixture.install_reporting("VALVE-SIGNED");
    let error = state.unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientRuntimeBroken, "{error}");
    let message = error.to_string();
    assert!(
        message.contains(&format!(
            "{cef}/steamwebhelper.exe is the client's own web helper with steamwebhelper_real.exe \
             stashed beside it"
        )),
        "{message}"
    );
    assert!(
        message.contains(
            "(the client wrote its web helper back over the shim; the stash is beside it)"
        ),
        "{message}"
    );
    assert!(
        !message.contains("no steamwebhelper_real.exe"),
        "the stash is there: {message}"
    );
    assert!(
        message.contains("`applejack runtime steam status` shows it"),
        "{message}"
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with("client: the client laid its web helper down"))
            .count(),
        2,
        "{lines:?}"
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("a client update replaced the web helper"))
            .count(),
        2,
        "restashed at each restart: {lines:?}"
    );
    assert!(
        lines.contains(&format!(
            "client: {cef}/steamwebhelper.exe is still not the shim after 2 restarts; giving up"
        )),
        "{lines:?}"
    );
    assert_eq!(fixture.client_starts().len(), 3, "{lines:?}");
    assert!(fixture.client_up(), "the client is left running: {lines:?}");
    assert_eq!(fixture.helper(), b"REAL-HELPER-2");
    assert_eq!(
        fixture.real_helper().as_deref(),
        Some(&b"REAL-HELPER-2"[..])
    );
    let (state, detail) = fixture.core().client_runtime_state().unwrap();
    assert_eq!(state, ClientRuntimeState::Broken, "{detail}");
    assert!(
        message.contains(&detail),
        "the error and `status` name the same shape:\n{message}\n{detail}"
    );
    fixture.assert_lock_free();

    fs::remove_file(&knob).unwrap();
    fixture.stop_client();
    fixture.clear_client_starts();
    let (state, lines) = fixture.install_reporting("VALVE-SIGNED");
    assert_eq!(state.unwrap(), ClientRuntimeState::NeedsLogin, "{lines:?}");
    assert_eq!(fixture.client_starts().len(), 1, "{lines:?}");
    assert!(
        lines.contains(&format!(
            "shim: {cef}: a client update replaced the web helper; stashed the new one and put \
             the shim back"
        )),
        "{lines:?}"
    );
    assert_eq!(fixture.helper(), fixture.shim());
    assert_eq!(
        fixture.real_helper().as_deref(),
        Some(&b"REAL-HELPER-2"[..])
    );
    assert!(
        !lines.iter().any(|line| line.starts_with("bootstrap:")),
        "nothing re-downloaded: {lines:?}"
    );
    fixture.stop_client();
}

/// `repair` on either unshimmed shape does what `install` does in place
/// (stop if running, stash, shim, start to the window) and restores no
/// snapshot: a restore would roll the client back for a state one shim pass
/// fixes.
#[test]
fn repair_puts_the_shim_in_front_of_an_unshimmed_helper_instead_of_restoring() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.core().prefix_snapshot(STEAM_CLIENT_PREFIX).unwrap();
    fixture.stop_client();
    let win64 = fixture.cef_dir_named("cef.win64");
    fs::create_dir_all(&win64).unwrap();
    fs::write(win64.join(WEBHELPER_EXE), b"REAL-HELPER-64").unwrap();
    let (outcome, lines) = repair_with_lines(&fixture);
    let status = outcome.unwrap();
    assert_eq!(status.state, ClientRuntimeState::NeedsLogin, "{lines:?}");
    assert_in_order(
        &lines,
        &[
            &format!(
                "repair: {}: the client's own web helper is in front (the client made the \
                 directory after the shim pass; no stash beside it); nothing to restore",
                win64.display()
            ),
            "stop: the client is not running",
            &format!(
                "shim: {}: stashed the web helper as steamwebhelper_real.exe and put the shim in \
                 front of it",
                win64.display()
            ),
            "start: starting steam.exe",
            "client: window: ",
            "needs_login: ",
        ],
    );
    assert!(
        !lines
            .iter()
            .any(|line| line.starts_with("repair: restoring") || line.starts_with("bootstrap:")),
        "no restore, no download: {lines:?}"
    );
    assert!(fixture.every_populated_cef_dir_is_shimmed());
    assert_eq!(
        fs::read(win64.join(WEBHELPER_REAL)).unwrap(),
        b"REAL-HELPER-64"
    );
    assert_eq!(status.snapshots, 1, "the snapshot is untouched");
    assert!(fixture.client_up(), "{lines:?}");

    // The write-back shape, under a running client: `repair` stops it first.
    fs::write(fixture.cef_dir().join(WEBHELPER_EXE), b"REAL-HELPER-2").unwrap();
    let server = spawn_stub_server("server", &fixture.prefix_dir());
    fixture.servers.borrow_mut().push(server);
    let cef = fixture.cef_dir().display().to_string();
    let (outcome, lines) = repair_with_lines(&fixture);
    let status = outcome.unwrap();
    assert_eq!(status.state, ClientRuntimeState::NeedsLogin, "{lines:?}");
    assert_in_order(
        &lines,
        &[
            &format!(
                "repair: {cef}: the client's own web helper is in front (the client wrote its \
                 web helper back over the shim; the stash is beside it); nothing to restore"
            ),
            "stop: stopping the Steam client",
            &format!(
                "shim: {cef}: a client update replaced the web helper; stashed the new one and \
                 put the shim back"
            ),
            "start: starting steam.exe",
            "client: window: ",
            "needs_login: ",
        ],
    );
    assert!(
        !lines
            .iter()
            .any(|line| line.starts_with("repair: restoring")),
        "{lines:?}"
    );
    assert_eq!(fixture.helper(), fixture.shim());
    assert_eq!(
        fixture.real_helper().as_deref(),
        Some(&b"REAL-HELPER-2"[..])
    );
    assert_eq!(status.snapshots, 1);
    fixture.stop_client();
}

/// A run's client start is never restarted (that would end the game); a
/// directory the client makes at it gets one `client:` line once the title
/// is up, saying what the next `status` reads and what to do, and `repair`
/// after the game puts the shim in front.
#[test]
fn a_run_whose_client_start_lays_a_new_directory_says_so_and_status_reads_broken_after() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.sign_in();
    fixture.stop_client();
    fixture.clear_client_starts();
    let knob = fixture.prefix_dir().join("new-cef-dir-every-start");
    fs::write(&knob, "").unwrap();
    let running = fixture.start_reporting();
    fixture.quit_game(0);
    let (outcome, lines) = running.finish();
    assert_eq!(outcome.unwrap().state, SessionState::Exited, "{lines:?}");
    let extra = fixture.cef_dir_named("cef.win-extra-1");
    assert!(
        lines.contains(&format!(
            "client: the client laid its web helper down without the shim in front of it at \
             this start ({}); the game runs on, but the client's window cannot draw and \
             `status` reads broken: after the game, `applejack runtime steam stop`, then \
             `applejack runtime steam install` or `applejack runtime steam repair`, put the \
             shim in front of it",
            extra.display()
        )),
        "{lines:?}"
    );
    assert_eq!(fixture.client_starts().len(), 1, "no restart: {lines:?}");
    let (state, detail) = fixture.core().client_runtime_state().unwrap();
    assert_eq!(state, ClientRuntimeState::Broken, "{detail}");
    assert!(detail.starts_with(&extra.display().to_string()), "{detail}");
    let error = fixture.core().plan_run(APP, false).unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientRuntimeBroken, "{error}");

    fs::remove_file(&knob).unwrap();
    fixture.stop_client();
    let (outcome, lines) = repair_with_lines(&fixture);
    assert_eq!(
        outcome.unwrap().state,
        ClientRuntimeState::Ready,
        "{lines:?}"
    );
    assert!(fixture.every_populated_cef_dir_is_shimmed());
    fixture.stop_client();
}

/// A start that shuts down on its own, with nothing in the bootstrap log
/// since the start but `Verification skipped` and `Shutdown` (seen twice
/// live), is started once more, once per op; a client that does it again
/// is the early exit it always was.
#[test]
fn a_client_that_shuts_down_cleanly_at_its_start_is_started_once_more() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.stop_client();
    fixture.clear_client_starts();
    fs::write(fixture.prefix_dir().join("early-shutdown"), "").unwrap();
    let (state, lines) = fixture.install_reporting("VALVE-SIGNED");
    assert_eq!(state.unwrap(), ClientRuntimeState::NeedsLogin, "{lines:?}");
    assert_in_order(
        &lines,
        &[
            "shim: ",
            "start: starting steam.exe for its self-update",
            "client: the client shut down on its own with nothing but the shutdown in \
             logs/bootstrap_log.txt (no download line); starting it once more",
            "shim: ",
            "start: starting steam.exe for its self-update",
            "client: window: ",
            "needs_login: ",
        ],
    );
    assert_eq!(fixture.client_starts().len(), 2, "{lines:?}");
    assert!(
        !lines.iter().any(|line| line.starts_with("stop:")),
        "nothing to stop: {lines:?}"
    );
    assert!(fixture.client_up());
    fixture.stop_client();

    fixture.clear_client_starts();
    let knob = fixture.prefix_dir().join("early-shutdown-always");
    fs::write(&knob, "").unwrap();
    let (state, lines) = fixture.install_reporting("VALVE-SIGNED");
    let error = state.unwrap_err();
    assert_eq!(error.code(), ErrorCode::ClientRuntimeBroken, "{error}");
    let message = error.to_string();
    assert!(
        message.contains("the client exited before its window came up"),
        "{message}"
    );
    assert!(message.contains("logs/bootstrap_log.txt"), "{message}");
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with("client: the client shut down on its own"))
            .count(),
        1,
        "one retry: {lines:?}"
    );
    assert_eq!(fixture.client_starts().len(), 2, "{lines:?}");
    fs::remove_file(&knob).unwrap();
    fixture.assert_lock_free();
}
