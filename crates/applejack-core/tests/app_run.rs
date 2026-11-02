//! `app.run`, `app.cancel`, and `session.list` over a stub loader that behaves
//! like a game: its argv names the exe, its environment carries the session,
//! and it stays until told to go. Each lock and refusal rule has a test here;
//! `crates/applejack/tests/app_session.rs` has the two-process half.

mod common;

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::mpsc::Receiver;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use applejack_core::config::{ConfigDb, Drm, NewSession, SessionRow, SessionState};
use applejack_core::rpc::jobs::Notifier;
use applejack_core::rpc::methods::{
    AppCancelParams, AppRunParams, HelloParams, Method, SessionListParams,
};
use applejack_core::rpc::wire::{ErrorCode, RpcError};
use applejack_core::rpc::{CoreHandler, Handler, PROTOCOL_VERSION};
use applejack_core::run::{AppRegistration, NOTE_PROBE};
use applejack_core::runtime::session::{NOTE_ADOPTED, NOTE_LOST, RunTuning};
use applejack_core::{Core, CoreError};
use applejack_engine::rosetta_wine::process::find_session;
use applejack_engine::{Error as EngineError, GraphicsBackend, HidrawMode};
use common::{script, spawn_stub_title, write_stub_bundle, write_title};
use serde_json::{Value, json};
use tempfile::TempDir;

const APP: u32 = 1234;
const PREFIX: &str = "1234";

const FAST: RunTuning = RunTuning {
    tick: Duration::from_millis(20),
    marker_misses: 2,
    drain: Duration::from_secs(3),
    early_exit: Duration::ZERO,
    term_wait: Duration::from_millis(300),
    cancel_wait: Duration::from_secs(10),
};

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    /// An installed engine and a registered client-free title.
    fn new() -> Self {
        let fixture = Self::without_engine();
        let tree = fixture.dir.path().join("tree");
        write_stub_bundle(&tree, "10.0.0");
        fixture
            .core()
            .plan_install("rosetta-wine", Some(&tree))
            .unwrap()
            .run(&mut |_| {})
            .unwrap();
        fixture
    }

    fn without_engine() -> Self {
        let dir = TempDir::new().unwrap();
        let fixture = Self { dir };
        fs::create_dir_all(fixture.data()).unwrap();
        write_title(
            &fixture.library(),
            APP,
            &["title.exe", "stubborn.exe", "quick.exe"],
        );
        fixture.register(Drm::None, Some("title.exe"));
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

    fn register(&self, drm: Drm, exe: Option<&str>) {
        self.core()
            .app_register(&AppRegistration {
                appid: APP,
                drm,
                exe: exe.map(str::to_owned),
                library: None,
            })
            .unwrap();
    }

    fn prefix_dir(&self) -> PathBuf {
        self.data().join("prefixes").join(PREFIX)
    }

    fn quit(&self, code: i32) {
        fs::write(self.prefix_dir().join("quit"), code.to_string()).unwrap();
    }

    fn db(&self) -> ConfigDb {
        ConfigDb::open(&self.data().join("db.sqlite")).unwrap()
    }

    fn rows(&self) -> Vec<SessionRow> {
        self.db().sessions(Some(APP)).unwrap()
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
        let row = wait_until("the session's state", || {
            let row = self.db().session(id).unwrap().unwrap();
            (row.state == state).then_some(row)
        });
        assert_eq!(row.state, state);
    }

    /// Fails the test on a prefix the session left locked.
    fn assert_lock_free(&self) {
        self.core()
            .prefix_snapshot(PREFIX)
            .expect("the lock is free");
    }

    /// For a prefix a failed run may not have left behind.
    fn assert_lock_free_without_prefix(&self) {
        applejack_engine::rosetta_wine::snapshot::PrefixLock::acquire(
            &self.prefix_dir(),
            &self.data().join("prefixes/.snapshots"),
            PREFIX,
        )
        .expect("the lock is free");
    }
}

struct Running {
    id: i64,
    thread: JoinHandle<(Result<SessionRow, CoreError>, Vec<String>)>,
}

impl Running {
    fn finish(self) -> (SessionRow, Vec<String>) {
        let (outcome, phases) = self.thread.join().unwrap();
        (outcome.unwrap(), phases)
    }
}

fn wait_until<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn plan_error(fixture: &Fixture) -> CoreError {
    fixture.core().plan_run(APP, false).unwrap_err()
}

fn assert_refused(fixture: &Fixture, code: ErrorCode) {
    let before = fixture.rows().len();
    let error = plan_error(fixture);
    assert_eq!(error.code(), code, "{error}");
    assert_eq!(fixture.rows().len(), before, "a refusal writes no session");
}

fn assert_busy<T: std::fmt::Debug>(outcome: Result<T, CoreError>) {
    let error = outcome.unwrap_err();
    assert!(
        matches!(error, CoreError::Engine(EngineError::PrefixBusy { .. })),
        "{error}"
    );
}

// Refusals.

#[test]
fn a_title_with_no_record_is_not_installed() {
    let fixture = Fixture::new();
    let error = fixture.core().plan_run(999, false).unwrap_err();
    assert_eq!(error.code(), ErrorCode::TitleNotInstalled, "{error}");
}

#[test]
fn a_title_gone_from_the_library_is_not_installed() {
    let fixture = Fixture::new();
    let acf = fixture
        .library()
        .join(format!("steamapps/appmanifest_{APP}.acf"));
    let text = fs::read_to_string(&acf).unwrap();
    fs::remove_file(&acf).unwrap();
    assert_refused(&fixture, ErrorCode::TitleNotInstalled);

    fs::write(&acf, text.replace("\"4\"", "\"2\"")).unwrap();
    assert_refused(&fixture, ErrorCode::TitleNotInstalled);

    fs::write(&acf, &text).unwrap();
    fs::remove_dir_all(fixture.library().join("steamapps/common/Title")).unwrap();
    fs::create_dir(fixture.library().join("steamapps/common/Title")).unwrap();
    assert_refused(&fixture, ErrorCode::TitleNotInstalled);
}

#[test]
fn a_download_record_refuses_the_run() {
    let fixture = Fixture::new();
    let record = fixture
        .library()
        .join(format!("applejack/{APP}.download.json"));
    fs::create_dir_all(record.parent().unwrap()).unwrap();
    fs::write(&record, "{}").unwrap();
    assert_refused(&fixture, ErrorCode::DownloadInProgress);
    assert!(!fixture.prefix_dir().exists());
}

#[test]
fn a_title_that_needs_the_client_is_refused_until_the_client_runtime_exists() {
    let fixture = Fixture::new();
    for drm in [Drm::Unknown, Drm::Steam] {
        fixture.register(drm, None);
        assert_refused(&fixture, ErrorCode::ClientRuntimeUnavailable);
    }
    assert!(!fixture.prefix_dir().exists());
}

#[test]
fn a_title_with_no_launch_exe_is_refused() {
    let fixture = Fixture::new();
    fixture
        .core()
        .config_set(APP, "launch_exe_override", "")
        .unwrap();
    assert_refused(&fixture, ErrorCode::NoLaunchExe);
    fixture
        .core()
        .config_set(APP, "launch_exe_override", "../../../db.sqlite")
        .unwrap();
    assert_refused(&fixture, ErrorCode::NoLaunchExe);
}

#[test]
fn a_library_outside_the_data_directory_is_refused() {
    let fixture = Fixture::new();
    let outside = fixture.dir.path().join("elsewhere");
    write_title(&outside, APP, &["title.exe"]);
    fixture
        .core()
        .app_register(&AppRegistration {
            appid: APP,
            drm: Drm::None,
            exe: None,
            library: Some(outside),
        })
        .unwrap();
    let error = plan_error(&fixture);
    assert!(
        matches!(error, CoreError::PathOutsideData { .. }),
        "{error}"
    );
}

#[test]
fn no_active_engine_refuses_the_run() {
    let fixture = Fixture::without_engine();
    assert_refused(&fixture, ErrorCode::EngineNotInstalled);
}

#[test]
fn a_stale_prefix_is_refused_before_a_session_exists() {
    let fixture = Fixture::new();
    fixture
        .core()
        .prefix_create(PREFIX, GraphicsBackend::Dxmt, None, HidrawMode::Disabled)
        .unwrap();
    let mut stale = vec![0; 0x40];
    stale.extend_from_slice(b"Wine builtin DLL\0");
    fs::write(
        fixture
            .prefix_dir()
            .join("drive_c/windows/system32/d3d11.dll"),
        stale,
    )
    .unwrap();
    assert_refused(&fixture, ErrorCode::PrefixUnusable);
}

// The session and its lock.

#[test]
fn a_session_runs_in_order_and_records_its_end() {
    let fixture = Fixture::new();
    let running = fixture.start();
    let row = fixture.db().session(running.id).unwrap().unwrap();
    assert_eq!(row.state, SessionState::Running);
    assert_eq!(row.ended_at, None);
    assert_eq!(row.launch_exe, "title.exe");
    assert_eq!(row.prefix, PREFIX);
    assert_ne!(find_session(&row.session_key), []);

    fixture.quit(3);
    let (end, phases) = running.finish();
    assert_eq!(end.state, SessionState::Exited);
    assert_eq!(end.exit_code, Some(3));
    assert!(end.ended_at.is_some());
    assert_eq!(phases, ["prefix", "starting", "running", "exited"]);
    assert_eq!(find_session(&end.session_key), []);
    fixture.assert_lock_free();
}

#[test]
fn the_log_lives_in_a_directory_of_the_session_under_the_data_dir() {
    let fixture = Fixture::new();
    fixture
        .core()
        .config_set(APP, "launch_args_override", "--windowed")
        .unwrap();
    let running = fixture.start();
    fixture.quit(0);
    let (end, _) = running.finish();
    let log = Path::new(&end.log_path);
    assert!(log.starts_with(fixture.data().join("logs").join(&end.session_key)));
    let text = fs::read_to_string(log).unwrap();
    assert!(text.contains("title up:"), "{text}");
    assert!(text.contains("--windowed"), "{text}");
}

#[test]
fn a_new_prefix_maps_the_titles_library_and_is_reused() {
    let fixture = Fixture::new();
    let running = fixture.start();
    let link = fixture.prefix_dir().join("dosdevices/s:");
    assert_eq!(fs::read_link(link).unwrap(), fixture.library());
    fixture.quit(0);
    running.finish();
    fs::remove_file(fixture.prefix_dir().join("quit")).unwrap();
    let running = fixture.start();
    fixture.quit(0);
    let (_, phases) = running.finish();
    assert_eq!(phases, ["starting", "running", "exited"]);
}

#[test]
fn a_changed_setting_is_applied_before_the_launch() {
    let fixture = Fixture::new();
    let running = fixture.start();
    fixture.quit(0);
    running.finish();
    fs::remove_file(fixture.prefix_dir().join("quit")).unwrap();
    fixture
        .core()
        .config_set(APP, "hidraw_enabled", "true")
        .unwrap();
    let running = fixture.start();
    fixture.quit(0);
    running.finish();
    let list = fixture.core().prefix_list().unwrap();
    assert_eq!(list[0].hidraw, Some(HidrawMode::Enabled));
}

#[test]
fn an_engine_change_snapshots_the_prefix_first() {
    let fixture = Fixture::new();
    let running = fixture.start();
    fixture.quit(0);
    running.finish();
    fs::remove_file(fixture.prefix_dir().join("quit")).unwrap();
    let tree = fixture.dir.path().join("tree2");
    write_stub_bundle(&tree, "10.0.1");
    let mut core = fixture.core();
    core.plan_install("rosetta-wine", Some(&tree))
        .unwrap()
        .run(&mut |_| {})
        .unwrap();
    core.engine_use("rosetta-wine", "10.0.1").unwrap();

    let running = fixture.start();
    fixture.quit(0);
    let (end, _) = running.finish();
    assert_eq!(end.engine_version, "10.0.1");
    assert_eq!(fixture.core().prefix_snapshots(PREFIX).unwrap().len(), 1);
    let list = fixture.core().prefix_list().unwrap();
    assert_eq!(list[0].engine_version.as_deref(), Some("10.0.1"));
}

#[test]
fn a_running_title_holds_the_prefix_against_every_operation() {
    let fixture = Fixture::new();
    let running = fixture.start();
    let mut other = fixture.core();
    assert_busy(other.prefix_reset(PREFIX, false));
    assert_busy(other.prefix_restore(PREFIX, "20260101T000000Z"));
    assert_busy(other.prefix_remove(PREFIX));
    assert_busy(other.prefix_snapshot(PREFIX));
    let mut sink = Vec::new();
    assert_busy(other.prefix_exec(PREFIX, Path::new("x.exe"), &[] as &[OsString], &mut sink));
    fixture.quit(0);
    running.finish();
    fixture.assert_lock_free();
}

#[test]
fn a_double_launch_is_refused_and_the_first_is_untouched() {
    let fixture = Fixture::new();
    let running = fixture.start();
    assert_refused(&fixture, ErrorCode::AlreadyRunning);
    let row = fixture.db().session(running.id).unwrap().unwrap();
    assert_eq!(row.state, SessionState::Running);
    fixture.quit(0);
    running.finish();
}

#[test]
fn a_busy_prefix_with_no_session_is_busy_not_already_running() {
    let fixture = Fixture::new();
    let core = fixture.core();
    let _other = applejack_engine::rosetta_wine::snapshot::PrefixLock::acquire(
        &fixture.prefix_dir(),
        &fixture.data().join("prefixes/.snapshots"),
        PREFIX,
    )
    .unwrap();
    drop(core);
    assert_refused(&fixture, ErrorCode::PrefixBusy);
}

#[test]
fn a_launch_that_cannot_start_is_a_failed_session_and_lets_go() {
    let fixture = Fixture::new();
    fixture
        .core()
        .config_set(APP, "graphics_backend", "d3dmetal")
        .unwrap();
    let plan = fixture.core().plan_run(APP, false).unwrap();
    let id = plan.session_id();
    let error = plan.run(&mut |_, _| {}).unwrap_err();
    assert_eq!(error.code(), ErrorCode::Internal, "{error}");
    let row = fixture.db().session(id).unwrap().unwrap();
    assert_eq!(row.state, SessionState::Failed);
    assert_eq!(row.note, Some(error.to_string()));
    assert!(
        fs::read_to_string(row.log_path)
            .unwrap()
            .contains("failed:")
    );
    fixture.assert_lock_free_without_prefix();
}

#[test]
fn a_direct_title_that_exits_at_once_may_need_the_client() {
    let fixture = Fixture::new();
    fixture.register(Drm::None, Some("quick.exe"));
    let mut tuning = FAST;
    tuning.early_exit = Duration::from_secs(5);
    let plan = fixture
        .core()
        .with_run_tuning(tuning)
        .plan_run(APP, false)
        .unwrap();
    let id = plan.session_id();
    let error = plan.run(&mut |_, _| {}).unwrap_err();
    assert_eq!(error.code(), ErrorCode::TitleExitedEarly, "{error}");
    assert!(error.to_string().contains("switch launch_mode to client"));
    let row = fixture.db().session(id).unwrap().unwrap();
    assert_eq!(row.state, SessionState::Exited);
    assert_eq!(row.exit_code, Some(0));
    assert!(row.note.unwrap().contains("Steam client"));
}

#[test]
fn the_early_exit_clock_starts_at_the_spawn_not_the_prefix_build() {
    let fixture = Fixture::new();
    fixture.register(Drm::None, Some("quick.exe"));
    let wine = fixture
        .data()
        .join("engines/rosetta-wine/10.0.0/wine/bin/wine64");
    let body = fs::read_to_string(&wine).unwrap();
    script(
        &wine,
        &body.replacen(
            "name=$(basename \"$1\")",
            "name=$(basename \"$1\")\n[ \"$name\" = wineboot ] && sleep 1",
            1,
        ),
    );
    let mut tuning = FAST;
    tuning.early_exit = Duration::from_millis(500);
    let error = fixture
        .core()
        .with_run_tuning(tuning)
        .plan_run(APP, false)
        .unwrap()
        .run(&mut |_, _| {})
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::TitleExitedEarly, "{error}");
}

#[test]
fn a_probe_runs_a_title_of_unknown_drm_and_verifies_nothing() {
    let fixture = Fixture::new();
    fixture.register(Drm::Unknown, None);
    let plan = fixture.core().plan_run(APP, true).unwrap();
    let id = plan.session_id();
    let thread = thread::spawn(move || plan.run(&mut |_, _| {}));
    fixture.wait_for(id, SessionState::Running);
    fixture.quit(0);
    let row = thread.join().unwrap().unwrap();
    assert_eq!(row.note.as_deref(), Some(NOTE_PROBE));
    let config = fixture.core().config_get(APP).unwrap();
    assert_eq!(config.direct_verified_at, None);
}

#[test]
fn no_environment_value_reaches_the_log() {
    let fixture = Fixture::new();
    let mut core = fixture.core();
    core.config_set(APP, "extra_env", "STEAM_REFRESH_TOKEN=hunter2")
        .unwrap();
    core.config_set(APP, "extra_env", "VK_ICD_FILENAMES=/tmp/hunter3")
        .unwrap();
    let running = fixture.start();
    fixture.quit(0);
    let (end, _) = running.finish();
    let text = fs::read_to_string(end.log_path).unwrap();
    assert!(!text.contains("hunter"), "{text}");
    assert!(text.contains("dropped VK_ICD_FILENAMES"), "{text}");
}

// Cancel.

#[test]
fn cancel_ends_the_title_from_another_core_and_frees_the_lock() {
    let fixture = Fixture::new();
    let running = fixture.start();
    let row = fixture
        .core()
        .plan_cancel(APP, false)
        .unwrap()
        .run(&mut |_, _| {})
        .unwrap();
    assert_eq!(row.state, SessionState::Cancelled);
    assert_eq!(row.exit_code, Some(128 + 15), "the owner read the status");
    assert_eq!(find_session(&row.session_key), []);
    fixture.assert_lock_free();
    let (end, phases) = running.finish();
    assert_eq!(end.state, SessionState::Cancelled);
    assert_eq!(phases.last().map(String::as_str), Some("cancelled"));
}

#[test]
fn a_title_that_ignores_sigterm_is_killed() {
    let fixture = Fixture::new();
    fixture.register(Drm::None, Some("stubborn.exe"));
    let running = fixture.start();
    let row = fixture
        .core()
        .plan_cancel(APP, false)
        .unwrap()
        .run(&mut |_, _| {})
        .unwrap();
    assert_eq!(row.exit_code, Some(128 + 9));
    assert_eq!(find_session(&row.session_key), []);
    running.finish();
}

#[test]
fn kill_prefix_stops_the_server() {
    let fixture = Fixture::new();
    let running = fixture.start();
    let row = fixture
        .core()
        .plan_cancel(APP, true)
        .unwrap()
        .run(&mut |_, _| {})
        .unwrap();
    assert_eq!(row.state, SessionState::Cancelled);
    assert_eq!(row.exit_code, Some(99), "the stub server ended the title");
    assert_eq!(find_session(&row.session_key), []);
    fixture.assert_lock_free();
    running.finish();
}

#[test]
fn a_cancel_before_the_spawn_never_starts_the_title() {
    let fixture = Fixture::new();
    let plan = fixture.core().plan_run(APP, false).unwrap();
    let id = plan.session_id();
    fixture.db().request_session_cancel(id).unwrap();
    let row = plan.run(&mut |_, _| {}).unwrap();
    assert_eq!(row.state, SessionState::Cancelled);
    assert_eq!(row.exit_code, None);
    let log = Path::new(&row.log_path);
    assert!(!log.exists() || !fs::read_to_string(log).unwrap().contains("title up"));
}

#[test]
fn cancelling_a_title_that_is_not_running_is_refused() {
    let fixture = Fixture::new();
    let error = fixture.core().plan_cancel(APP, false).unwrap_err();
    assert_eq!(error.code(), ErrorCode::NotRunning, "{error}");
}

// A core that died.

/// An open row as a dead core leaves it: nobody holds the lock.
fn orphan_row(fixture: &Fixture, key: &str) -> i64 {
    fixture
        .core()
        .prefix_create(PREFIX, GraphicsBackend::Dxmt, None, HidrawMode::Disabled)
        .unwrap();
    fixture
        .db()
        .insert_session(&NewSession {
            appid: APP,
            started_at: "2026-11-01T00:00:00Z",
            engine_id: "rosetta-wine",
            engine_version: "10.0.0",
            log_path: "/nonexistent/wine.log",
            session_key: key,
            prefix: PREFIX,
            launch_exe: "title.exe",
        })
        .unwrap()
        .unwrap()
}

/// A title no core is the parent of. Reaped by the test, so a kill shows.
struct Orphan(Child);

impl Orphan {
    fn start(fixture: &Fixture, key: &str) -> Self {
        let child = spawn_stub_title(key, &fixture.prefix_dir());
        let key = key.to_owned();
        wait_until("the orphan to be listed", || {
            (!find_session(&key).is_empty()).then_some(())
        });
        Self(child)
    }
}

impl Drop for Orphan {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_session_whose_core_and_title_are_gone_closes_as_lost() {
    let fixture = Fixture::new();
    let id = orphan_row(&fixture, "run-dead");
    let adopted = fixture.core().reconcile_sessions().unwrap();
    assert_eq!(adopted.len(), 0);
    let row = fixture.db().session(id).unwrap().unwrap();
    assert_eq!(row.state, SessionState::Lost);
    assert_eq!(row.exit_code, None);
    assert_eq!(row.note.as_deref(), Some(NOTE_LOST));
    assert!(row.ended_at.is_some());
}

#[test]
fn a_new_run_closes_a_dead_session_and_proceeds() {
    let fixture = Fixture::new();
    let id = orphan_row(&fixture, "run-dead2");
    let running = fixture.start();
    assert_eq!(
        fixture.db().session(id).unwrap().unwrap().state,
        SessionState::Lost
    );
    fixture.quit(0);
    running.finish();
}

#[test]
fn a_live_title_with_no_core_is_refused_as_running_and_stays_open() {
    let fixture = Fixture::new();
    let id = orphan_row(&fixture, "run-live");
    let _title = Orphan::start(&fixture, "run-live");
    assert_refused(&fixture, ErrorCode::AlreadyRunning);
    drop(fixture.core().reconcile_sessions().unwrap());
    assert_eq!(fixture.db().session(id).unwrap().unwrap().ended_at, None);
}

#[test]
fn a_session_owned_by_a_live_core_is_not_touched() {
    let fixture = Fixture::new();
    let running = fixture.start();
    let adopted = fixture.core().reconcile_sessions().unwrap();
    assert_eq!(adopted.len(), 0);
    let row = fixture.db().session(running.id).unwrap().unwrap();
    assert_eq!(row.state, SessionState::Running);
    fixture.quit(0);
    running.finish();
}

#[test]
fn a_new_core_takes_over_a_live_title_and_holds_the_prefix() {
    let fixture = Fixture::new();
    let id = orphan_row(&fixture, "run-adopt");
    let title = Orphan::start(&fixture, "run-adopt");
    let mut adopted = fixture.core().reconcile_sessions().unwrap();
    assert_eq!(adopted.len(), 1);
    let session = adopted.pop().unwrap();
    assert_busy(fixture.core().prefix_snapshot(PREFIX));
    let thread = thread::spawn(move || session.watch(&mut |_, _| {}));
    drop(title);
    let row = thread.join().unwrap().unwrap();
    assert_eq!(row.id, id);
    assert_eq!(row.state, SessionState::Exited);
    assert_eq!(row.exit_code, None);
    assert_eq!(row.note.as_deref(), Some(NOTE_ADOPTED));
    fixture.assert_lock_free();
}

#[test]
fn cancel_ends_a_title_nobody_owns() {
    let fixture = Fixture::new();
    let id = orphan_row(&fixture, "run-cancel-orphan");
    let mut title = Orphan::start(&fixture, "run-cancel-orphan");
    // The test is the parent here: reap in the background as launchd would.
    let reaper = thread::spawn(move || {
        let _ = title.0.wait();
        title
    });
    let row = fixture
        .core()
        .plan_cancel(APP, false)
        .unwrap()
        .run(&mut |_, _| {})
        .unwrap();
    assert_eq!(row.id, id);
    assert_eq!(row.state, SessionState::Cancelled);
    assert_eq!(find_session("run-cancel-orphan"), []);
    drop(reaper.join().unwrap());
    fixture.assert_lock_free();
}

// Over RPC.

fn request(
    handler: &mut CoreHandler,
    notifier: &Notifier,
    method: &Method,
) -> Result<Value, RpcError> {
    handler.handle(method, notifier)
}

fn run_method() -> Method {
    Method::AppRun(AppRunParams {
        appid: APP,
        probe_direct: false,
    })
}

/// Reads lines until `job_id` finishes; every notification the job sent, in
/// order. The session's end is `job.done` or `job.failed`.
fn job_lines(lines: &Receiver<String>, job_id: &Value) -> Vec<Value> {
    let mut seen = Vec::new();
    loop {
        let line = lines.recv_timeout(Duration::from_secs(15)).unwrap();
        let message: Value = serde_json::from_str(&line).unwrap();
        if message["params"]["job_id"] != *job_id {
            continue;
        }
        let done = message["method"] != "job.progress";
        seen.push(message);
        if done {
            return seen;
        }
    }
}

#[test]
fn over_rpc_a_run_answers_at_once_and_reports_on_its_job() {
    let fixture = Fixture::new();
    let (notifier, lines) = Notifier::channel();
    let mut handler = CoreHandler::new(fixture.core());
    let started = request(&mut handler, &notifier, &run_method()).unwrap();
    let id = started["id"].as_i64().unwrap();
    assert!(started["log_path"].as_str().unwrap().ends_with("wine.log"));
    fixture.wait_for(id, SessionState::Running);

    let second = request(&mut handler, &notifier, &run_method()).unwrap_err();
    assert_eq!(second.code, ErrorCode::AlreadyRunning);
    let busy = request(
        &mut handler,
        &notifier,
        &Method::PrefixReset(applejack_core::rpc::methods::PrefixResetParams {
            prefix: PREFIX.to_owned(),
            keep_user_data: false,
        }),
    )
    .unwrap_err();
    assert_eq!(busy.code, ErrorCode::PrefixBusy);

    let list = request(
        &mut handler,
        &notifier,
        &Method::SessionList(SessionListParams { appid: Some(APP) }),
    )
    .unwrap();
    assert_eq!(list["sessions"][0]["state"], json!("running"));

    let cancel = request(
        &mut handler,
        &notifier,
        &Method::AppCancel(AppCancelParams {
            appid: APP,
            kill_prefix: false,
        }),
    )
    .unwrap();
    let run_job = started["job_id"].clone();
    let cancel_job = cancel["job_id"].clone();
    let thread = thread::spawn(move || {
        let run = job_lines(&lines, &run_job);
        (run, lines)
    });
    let (run, lines) = thread.join().unwrap();
    let phases: Vec<&str> = run
        .iter()
        .filter_map(|message| message["params"]["phase"].as_str())
        .collect();
    assert_eq!(phases, ["prefix", "starting", "running", "cancelled"]);
    assert_eq!(run.last().unwrap()["method"], json!("job.done"));
    drop(job_lines(&lines, &cancel_job));
    let row = fixture.db().session(id).unwrap().unwrap();
    assert_eq!(row.state, SessionState::Cancelled);
}

#[test]
fn over_rpc_a_refusal_is_an_error_response_with_its_code() {
    let fixture = Fixture::new();
    fixture.register(Drm::Steam, None);
    let (notifier, _lines) = Notifier::channel();
    let mut handler = CoreHandler::new(fixture.core());
    let error = request(&mut handler, &notifier, &run_method()).unwrap_err();
    assert_eq!(error.code, ErrorCode::ClientRuntimeUnavailable);
    let error = request(
        &mut handler,
        &notifier,
        &Method::AppCancel(AppCancelParams {
            appid: APP,
            kill_prefix: false,
        }),
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::NotRunning);
}

#[test]
fn hello_takes_over_what_a_dead_core_left() {
    let fixture = Fixture::new();
    let lost = orphan_row(&fixture, "run-hello");
    let (notifier, lines) = Notifier::channel();
    let mut handler = CoreHandler::new(fixture.core());
    let hello = Method::Hello(HelloParams {
        protocol_version: PROTOCOL_VERSION,
    });
    request(&mut handler, &notifier, &hello).unwrap();
    assert_eq!(
        fixture.db().session(lost).unwrap().unwrap().state,
        SessionState::Lost
    );

    let adopted = fixture
        .db()
        .insert_session(&NewSession {
            appid: APP,
            started_at: "2026-11-01T00:00:00Z",
            engine_id: "rosetta-wine",
            engine_version: "10.0.0",
            log_path: "/nonexistent/wine.log",
            session_key: "run-hello-live",
            prefix: PREFIX,
            launch_exe: "title.exe",
        })
        .unwrap()
        .unwrap();
    let title = Orphan::start(&fixture, "run-hello-live");
    request(&mut handler, &notifier, &hello).unwrap();
    let progress: Value =
        serde_json::from_str(&lines.recv_timeout(Duration::from_secs(10)).unwrap()).unwrap();
    assert_eq!(progress["params"]["phase"], json!("running"));
    assert_busy(fixture.core().prefix_snapshot(PREFIX));
    let job = progress["params"]["job_id"].clone();
    drop(title);
    drop(job_lines(&lines, &job));
    let row = fixture.db().session(adopted).unwrap().unwrap();
    assert_eq!(row.state, SessionState::Exited);
    assert_eq!(row.note.as_deref(), Some(NOTE_ADOPTED));
}

#[test]
fn hello_on_a_fresh_data_directory_creates_nothing() {
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("data");
    let (notifier, _lines) = Notifier::channel();
    let mut handler = CoreHandler::new(Core::new(&data));
    let hello = Method::Hello(HelloParams {
        protocol_version: PROTOCOL_VERSION,
    });
    request(&mut handler, &notifier, &hello).unwrap();
    assert!(!data.exists());
}
