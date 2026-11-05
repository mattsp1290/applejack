//! `app.run`, `app.cancel`, and `session.list` over a stub loader that behaves
//! like a game: its argv names the exe, its environment carries the session,
//! and it stays until told to go. Each lock and refusal rule has a test here;
//! `crates/applejack/tests/app_session.rs` has the two-process half.

mod common;

use std::ffi::OsString;
use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use applejack_core::config::{ConfigDb, Drm, NewSession, SessionRow, SessionState};
use applejack_core::host::{ControllerFact, ControllerKind, Inventory, Transport};
use applejack_core::rpc::jobs::Notifier;
use applejack_core::rpc::methods::{
    AppCancelParams, AppRunParams, HelloParams, Method, SessionListParams,
};
use applejack_core::rpc::wire::{ErrorCode, RpcError};
use applejack_core::rpc::{CoreHandler, Handler, PROTOCOL_VERSION};
use applejack_core::run::{AppRegistration, NOTE_PROBE};
use applejack_core::runtime::session::{NOTE_ADOPTED, NOTE_LINGERED, NOTE_LOST, RunTuning};
use applejack_core::{Core, CoreError};
use applejack_engine::rosetta_wine::process::{find_session, signal};
use applejack_engine::rosetta_wine::utc_now;
use applejack_engine::{Error as EngineError, GraphicsBackend, HidrawMode};
use common::{script, spawn_stub_server, spawn_stub_title, write_stub_bundle, write_title};
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
    client_start_wait: Duration::from_secs(5),
    first_run_timeout: Duration::from_secs(600),
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
            &[
                "title.exe",
                "stubborn.exe",
                "quick.exe",
                "launcher.exe",
                "immortal.exe",
                "forking.exe",
            ],
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
            .with_controllers(Vec::new())
            .with_run_tuning(FAST)
    }

    fn register(&self, drm: Drm, exe: Option<&str>) {
        self.core()
            .app_register(&AppRegistration {
                appid: APP,
                drm: Some(drm),
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

/// Red if `launch_mode = client` gets the `drm` route: following it repeats
/// this refusal.
#[test]
fn a_title_held_to_the_client_is_told_about_launch_mode() {
    let fixture = Fixture::new();
    fixture.register(Drm::None, None);
    fixture
        .core()
        .config_set(APP, "launch_mode", "client")
        .unwrap();
    let text = fixture.core().plan_run(APP, false).unwrap_err().to_string();
    assert!(text.contains("set launch_mode"), "{text}");
    assert!(!text.contains("set drm none"), "{text}");

    fixture.register(Drm::Unknown, None);
    let text = fixture.core().plan_run(APP, false).unwrap_err().to_string();
    assert!(text.contains("set launch_mode"), "{text}");
    assert!(text.contains("set drm none"), "{text}");
}

/// The look in `plan_run` before the claim and the one after it. A FIFO in
/// place of the `.acf` holds the first look inside its read, past its record
/// check, so the record lands in exactly the gap the second look closes. A
/// real install that stopped between them leaves the same state. Red if the
/// second `installed_dir` goes.
#[test]
fn a_record_that_lands_after_the_first_look_refuses_the_run() {
    use std::io::Write;
    let fixture = Fixture::new();
    let acf = fixture
        .library()
        .join(format!("steamapps/appmanifest_{APP}.acf"));
    let text = fs::read(&acf).unwrap();
    fs::remove_file(&acf).unwrap();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&acf)
            .status()
            .unwrap()
            .success()
    );
    let mut core = fixture.core();
    let planner = thread::spawn(move || core.plan_run(APP, false).map(|_plan| ()));
    // This open returns once the first look has opened the FIFO to read.
    let mut writer = fs::OpenOptions::new().write(true).open(&acf).unwrap();
    let record = fixture
        .library()
        .join(format!("applejack/{APP}.download.json"));
    fs::create_dir_all(record.parent().unwrap()).unwrap();
    fs::write(&record, b"{}").unwrap();
    writer.write_all(&text).unwrap();
    drop(writer);
    let error = planner.join().unwrap().unwrap_err();
    assert_eq!(error.code(), ErrorCode::DownloadInProgress, "{error}");
    assert!(fixture.rows().is_empty(), "no session row");
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
fn a_registered_library_outside_the_data_directory_runs() {
    let fixture = Fixture::new();
    let outside = fixture.dir.path().join("elsewhere");
    write_title(&outside, APP, &["title.exe"]);
    let row = fixture
        .core()
        .app_register(&AppRegistration {
            appid: APP,
            drm: Some(Drm::None),
            exe: Some("title.exe".to_owned()),
            library: Some(outside.clone()),
        })
        .unwrap();
    let outside = fs::canonicalize(outside).unwrap();
    assert_eq!(row.library_dir.as_deref(), outside.to_str());
    let running = fixture.start();
    let link = fixture.prefix_dir().join("dosdevices/s:");
    assert_eq!(fs::read_link(link).unwrap(), outside);
    fixture.quit(0);
    assert_eq!(running.finish().0.exit_code, Some(0));
}

#[test]
fn the_exe_stays_inside_the_install_dir_of_a_library_outside_the_data_directory() {
    let fixture = Fixture::new();
    let outside = fixture.dir.path().join("elsewhere");
    write_title(&outside, APP, &["title.exe"]);
    fs::write(outside.join("steamapps/stray.exe"), common::pe_image()).unwrap();
    let register = |exe: &str| {
        fixture.core().app_register(&AppRegistration {
            appid: APP,
            drm: Some(Drm::None),
            exe: Some(exe.to_owned()),
            library: Some(outside.clone()),
        })
    };
    register("title.exe").unwrap();
    let error = register("../../stray.exe").unwrap_err();
    assert_eq!(error.code(), ErrorCode::NoLaunchExe, "{error}");
    fixture
        .core()
        .config_set(APP, "launch_exe_override", "../../stray.exe")
        .unwrap();
    assert_refused(&fixture, ErrorCode::NoLaunchExe);
}

#[test]
fn a_prefix_exec_outside_the_data_directory_is_still_refused() {
    let fixture = Fixture::new();
    let outside = fixture.dir.path().join("elsewhere");
    write_title(&outside, APP, &["title.exe"]);
    fixture
        .core()
        .prefix_create(PREFIX, GraphicsBackend::Dxmt, None, HidrawMode::Disabled)
        .unwrap();
    let exe = outside.join("steamapps/common/Title/title.exe");
    let mut sink = Vec::new();
    let error = fixture
        .core()
        .prefix_exec(PREFIX, &exe, &[] as &[OsString], &[], &mut sink)
        .unwrap_err();
    assert!(
        matches!(error, CoreError::PathOutsideData { .. }),
        "{error}"
    );
}

fn refused_registration(fixture: &Fixture, registration: &AppRegistration) -> CoreError {
    let before = fixture.db().app(registration.appid).unwrap();
    let error = fixture.core().app_register(registration).unwrap_err();
    let after = fixture.db().app(registration.appid).unwrap();
    assert_eq!(before, after, "a refusal writes nothing");
    error
}

fn manifest_with(fixture: &Fixture, from: &str, to: &str) {
    let acf = fixture
        .library()
        .join(format!("steamapps/appmanifest_{APP}.acf"));
    let text = fs::read_to_string(&acf).unwrap();
    assert!(text.contains(from));
    fs::write(&acf, text.replace(from, to)).unwrap();
}

fn registration(exe: Option<&str>, library: Option<PathBuf>) -> AppRegistration {
    AppRegistration {
        appid: APP,
        drm: Some(Drm::None),
        exe: exe.map(str::to_owned),
        library,
    }
}

#[test]
fn an_installdir_that_is_not_one_name_is_refused_at_register_and_at_run() {
    let fixture = Fixture::new();
    for bad in [
        "", "..", "../..", ".", "/etc", "a/b", "Title/", "a\\\\b", "a\u{1}b",
    ] {
        manifest_with(
            &fixture,
            "\"installdir\"\t\t\"Title\"",
            &format!("\"installdir\"\t\t\"{bad}\""),
        );
        let error = refused_registration(&fixture, &registration(None, None));
        assert_eq!(error.code(), ErrorCode::TitleNotInstalled, "{bad}: {error}");
        assert!(error.to_string().contains("install directory"), "{error}");
        manifest_with(
            &fixture,
            &format!("\"installdir\"\t\t\"{bad}\""),
            "\"installdir\"\t\t\"Title\"",
        );
    }
}

#[test]
fn a_stored_installdir_that_is_not_one_name_is_refused_at_run() {
    let fixture = Fixture::new();
    let mut row = fixture.db().app(APP).unwrap().unwrap();
    for bad in ["../..", "", "a\0b", "a\u{1}b"] {
        row.installdir = Some(bad.to_owned());
        fixture.db().upsert_app(&row).unwrap();
        assert_refused(&fixture, ErrorCode::TitleNotInstalled);
        assert!(
            plan_error(&fixture)
                .to_string()
                .contains("install directory"),
            "{bad:?}"
        );
    }
}

#[test]
fn an_install_dir_that_is_the_library_is_refused_at_run() {
    let fixture = Fixture::new();
    let title = fixture.library().join("steamapps/common/Title");
    fs::remove_dir_all(&title).unwrap();
    std::os::unix::fs::symlink(fixture.library(), &title).unwrap();
    assert_refused(&fixture, ErrorCode::TitleNotInstalled);
    assert!(
        plan_error(&fixture).to_string().contains("nothing usable"),
        "{}",
        plan_error(&fixture)
    );
}

#[test]
fn a_manifest_for_another_app_is_refused() {
    let fixture = Fixture::new();
    manifest_with(&fixture, "\"appid\"\t\t\"1234\"", "\"appid\"\t\t\"999\"");
    let error = refused_registration(&fixture, &registration(None, None));
    assert_eq!(error.code(), ErrorCode::TitleNotInstalled, "{error}");
    assert!(error.to_string().contains("names app 999"), "{error}");
    assert_refused(&fixture, ErrorCode::TitleNotInstalled);
}

#[test]
fn register_refuses_what_a_run_would_refuse_of_the_exe() {
    let fixture = Fixture::new();
    fs::write(fixture.library().join("outside.exe"), common::pe_image()).unwrap();
    let title = fixture.library().join("steamapps/common/Title");
    fs::write(title.join("notes.exe"), "plain text").unwrap();
    for exe in [
        "../../../outside.exe",
        "/etc/hosts",
        "missing.exe",
        "notes.exe",
    ] {
        let error = refused_registration(&fixture, &registration(Some(exe), None));
        assert_eq!(error.code(), ErrorCode::NoLaunchExe, "{exe}: {error}");
    }
}

#[test]
fn a_library_that_is_relative_or_missing_is_refused_at_register() {
    let fixture = Fixture::new();
    for library in ["library", "./library"] {
        let error = refused_registration(&fixture, &registration(None, Some(library.into())));
        assert_eq!(error.code(), ErrorCode::TitleNotInstalled, "{error}");
        assert!(error.to_string().contains("absolute"), "{error}");
    }
    let missing = fixture.dir.path().join("nowhere");
    let error = refused_registration(&fixture, &registration(None, Some(missing)));
    assert_eq!(error.code(), ErrorCode::TitleNotInstalled, "{error}");
}

#[test]
fn a_relative_library_in_the_record_is_refused_at_run() {
    let fixture = Fixture::new();
    let mut row = fixture.db().app(APP).unwrap().unwrap();
    row.library_dir = Some("library".to_owned());
    fixture.db().upsert_app(&row).unwrap();
    assert_refused(&fixture, ErrorCode::TitleNotInstalled);
    let error = plan_error(&fixture).to_string();
    assert!(error.contains("not an absolute path"), "{error}");
}

#[test]
fn control_characters_in_a_manifest_name_are_not_stored() {
    let fixture = Fixture::new();
    manifest_with(
        &fixture,
        "\"name\"\t\t\"Title\"",
        "\"name\"\t\t\"Ti\u{1b}[31m\u{202e}tle\u{7}\u{2066}\"",
    );
    let row = fixture
        .core()
        .app_register(&registration(None, None))
        .unwrap();
    assert_eq!(row.name.as_deref(), Some("Ti[31mtle"));
    let stored = fixture.db().app(APP).unwrap().unwrap();
    assert_eq!(stored.name, row.name);
}

#[test]
fn register_keeps_the_stored_drm_unless_it_is_given() {
    let fixture = Fixture::new();
    let mut again = registration(None, None);
    again.drm = None;
    let row = fixture.core().app_register(&again).unwrap();
    assert_eq!(row.drm, Drm::None);
    let mut steam = registration(None, None);
    steam.drm = Some(Drm::Steam);
    assert_eq!(fixture.core().app_register(&steam).unwrap().drm, Drm::Steam);

    let other = AppRegistration {
        appid: 77,
        drm: None,
        exe: None,
        library: None,
    };
    write_title(&fixture.library(), 77, &[]);
    assert_eq!(
        fixture.core().app_register(&other).unwrap().drm,
        Drm::Unknown
    );
}

#[test]
fn the_direct_verification_stamp_cannot_be_set_by_hand() {
    let fixture = Fixture::new();
    let error = fixture
        .core()
        .config_set(APP, "direct_verified_at", "2026-01-01T00:00:00Z")
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::InvalidParams, "{error}");
    assert_eq!(
        fixture.core().config_get(APP).unwrap().direct_verified_at,
        None
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
    assert_eq!(
        phases,
        ["prepare", "prefix", "starting", "running", "exited"]
    );
    assert_eq!(find_session(&end.session_key), []);
    fixture.assert_lock_free();
}

#[test]
fn the_log_lives_in_a_directory_of_the_session_under_the_data_dir() {
    let fixture = Fixture::new();
    let running = fixture.start();
    fixture.quit(0);
    let (end, _) = running.finish();
    let log = Path::new(&end.log_path);
    assert!(log.starts_with(fixture.data().join("logs").join(&end.session_key)));
    let text = fs::read_to_string(log).unwrap();
    assert!(text.contains("title up:"), "{text}");
}

#[test]
fn launch_arguments_reach_the_title_and_only_their_count_reaches_the_log() {
    let fixture = Fixture::new();
    fixture
        .core()
        .config_set(
            APP,
            "launch_args_override",
            "+connect srv +password hunter5",
        )
        .unwrap();
    let running = fixture.start();
    fixture.quit(0);
    let (end, _) = running.finish();
    let given = fs::read_to_string(fixture.prefix_dir().join("args")).unwrap();
    assert_eq!(given.trim(), "+connect srv +password hunter5");
    let text = fs::read_to_string(end.log_path).unwrap();
    assert!(!text.contains("hunter5"), "{text}");
    assert!(!text.contains("+connect"), "{text}");
    assert!(text.contains("title.exe (4 arguments)"), "{text}");
}

#[test]
fn a_new_prefix_maps_the_titles_library_and_is_reused() {
    let fixture = Fixture::new();
    let running = fixture.start();
    let link = fixture.prefix_dir().join("dosdevices/s:");
    let library = fs::canonicalize(fixture.library()).unwrap();
    assert_eq!(fs::read_link(link).unwrap(), library);
    fixture.quit(0);
    running.finish();
    fs::remove_file(fixture.prefix_dir().join("quit")).unwrap();
    let running = fixture.start();
    fixture.quit(0);
    let (_, phases) = running.finish();
    assert_eq!(phases, ["prepare", "starting", "running", "exited"]);
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

/// A run with `pads` as the inventory, to its end; the lines are
/// `phase: message`, as the CLI prints them.
fn run_with_pads(fixture: &Fixture, pads: Vec<ControllerFact>) -> Vec<String> {
    run_with_inventory(fixture, Inventory::available(pads))
}

fn run_with_inventory(fixture: &Fixture, inventory: Inventory) -> Vec<String> {
    let _ = fs::remove_file(fixture.prefix_dir().join("quit"));
    let plan = fixture
        .core()
        .with_controller_inventory(inventory)
        .plan_run(APP, false)
        .unwrap();
    let id = plan.session_id();
    let thread = thread::spawn(move || {
        let mut lines = Vec::new();
        let outcome = plan.run(&mut |phase, message| lines.push(format!("{phase}: {message}")));
        (outcome, lines)
    });
    fixture.wait_for(id, SessionState::Running);
    fixture.quit(0);
    let (outcome, lines) = thread.join().unwrap();
    outcome.unwrap();
    lines
}

/// The newest `seeds.reg` the engine imported for the title's prefix: a
/// create leaves it in the session's log directory, an apply in the prefix's
/// `applejack/` state directory.
fn seeds(fixture: &Fixture) -> String {
    fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, found);
            } else if path.file_name().is_some_and(|name| name == "seeds.reg") {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(&fixture.data(), &mut found);
    let newest = found
        .into_iter()
        .max_by_key(|path| fs::metadata(path).and_then(|meta| meta.modified()).ok())
        .expect("a seeds.reg was written");
    fs::read_to_string(newest).unwrap()
}

fn prefix_hidraw(fixture: &Fixture) -> Option<HidrawMode> {
    fixture.core().prefix_list().unwrap()[0].hidraw
}

fn dualsense(connected: bool) -> ControllerFact {
    ControllerFact {
        name: "DualSense Wireless Controller".to_owned(),
        transport: Transport::Bluetooth,
        vendor_id: "0x054c".to_owned(),
        product_id: "0x0ce6".to_owned(),
        connected,
        kind: ControllerKind::Dualsense,
    }
}

/// The value the fixture bundle's `hidraw_key` names, as the seed writes it.
const HIDRAW_DISABLED_SEED: &str = "\"DisableHidraw\"=dword:00000001";
const HIDRAW_ENABLED_SEED: &str = "\"DisableHidraw\"=dword:00000000";

#[test]
fn an_unset_hidraw_with_a_connected_dualsense_is_disabled_in_the_prefix() {
    let fixture = Fixture::new();
    let lines = run_with_pads(&fixture, vec![dualsense(true)]);
    assert!(
        lines.contains(&"prepare: hidraw disabled (DualSense connected)".to_owned()),
        "{lines:?}"
    );
    assert_eq!(prefix_hidraw(&fixture), Some(HidrawMode::Disabled));
    assert!(seeds(&fixture).contains(HIDRAW_DISABLED_SEED));
}

#[test]
fn an_unset_hidraw_with_no_connected_sony_pad_is_enabled() {
    let fixture = Fixture::new();
    let lines = run_with_pads(&fixture, vec![dualsense(false)]);
    assert!(
        lines.contains(&"prepare: hidraw enabled (no Sony pad connected)".to_owned()),
        "{lines:?}"
    );
    assert_eq!(prefix_hidraw(&fixture), Some(HidrawMode::Enabled));
    assert!(seeds(&fixture).contains(HIDRAW_ENABLED_SEED));
}

#[test]
fn an_explicit_hidraw_setting_wins_over_the_pads() {
    let fixture = Fixture::new();
    fixture
        .core()
        .config_set(APP, "hidraw_enabled", "false")
        .unwrap();
    let lines = run_with_pads(&fixture, Vec::new());
    assert!(
        lines.contains(&"prepare: hidraw disabled (config)".to_owned()),
        "{lines:?}"
    );
    assert_eq!(prefix_hidraw(&fixture), Some(HidrawMode::Disabled));
    fixture
        .core()
        .config_set(APP, "hidraw_enabled", "true")
        .unwrap();
    let lines = run_with_pads(&fixture, vec![dualsense(true)]);
    assert!(
        lines.contains(&"prepare: hidraw enabled (config)".to_owned()),
        "{lines:?}"
    );
    assert_eq!(prefix_hidraw(&fixture), Some(HidrawMode::Enabled));
    assert!(seeds(&fixture).contains(HIDRAW_ENABLED_SEED));
}

#[test]
fn a_pad_appearing_between_two_runs_flips_the_prefix() {
    let fixture = Fixture::new();
    run_with_pads(&fixture, Vec::new());
    assert_eq!(prefix_hidraw(&fixture), Some(HidrawMode::Enabled));
    let lines = run_with_pads(&fixture, vec![dualsense(true)]);
    assert_eq!(
        lines[..3],
        [
            "prepare: hidraw disabled (DualSense connected)",
            "prefix: applying settings to prefix 1234",
            "starting: title.exe",
        ],
        "{lines:?}"
    );
    assert_eq!(prefix_hidraw(&fixture), Some(HidrawMode::Disabled));
    assert!(seeds(&fixture).contains(HIDRAW_DISABLED_SEED));
    let lines = run_with_pads(&fixture, Vec::new());
    assert_eq!(lines[0], "prepare: hidraw enabled (no Sony pad connected)");
    assert_eq!(prefix_hidraw(&fixture), Some(HidrawMode::Enabled));
    assert!(seeds(&fixture).contains(HIDRAW_ENABLED_SEED));
}

#[test]
fn an_unavailable_inventory_disables_a_new_prefix_and_keeps_an_existing_one() {
    const WHY: &str = "controller inventory unavailable (SPBluetoothDataType timed out)";
    let fixture = Fixture::new();
    let unavailable = Inventory::unavailable("SPBluetoothDataType timed out");
    // Before the prefix exists, `config.resolve` says what the first run will do.
    let resolved = fixture
        .core()
        .with_controller_inventory(unavailable.clone())
        .config_resolve(APP)
        .unwrap();
    assert!(!resolved.hidraw_enabled);
    assert_eq!(resolved.hidraw_reason, WHY);
    let lines = run_with_inventory(&fixture, unavailable.clone());
    assert_eq!(
        lines[..3],
        [
            format!("prepare: hidraw disabled ({WHY})"),
            "prefix: creating prefix 1234".to_owned(),
            "starting: title.exe".to_owned(),
        ],
        "{lines:?}"
    );
    assert_eq!(prefix_hidraw(&fixture), Some(HidrawMode::Disabled));
    assert!(seeds(&fixture).contains(HIDRAW_DISABLED_SEED));

    run_with_pads(&fixture, Vec::new());
    assert_eq!(prefix_hidraw(&fixture), Some(HidrawMode::Enabled));
    let lines = run_with_inventory(&fixture, unavailable.clone());
    assert_eq!(
        lines[..2],
        [
            format!("prepare: hidraw enabled ({WHY})"),
            "starting: title.exe".to_owned(),
        ],
        "an existing prefix keeps its mode and nothing is applied: {lines:?}"
    );
    assert_eq!(prefix_hidraw(&fixture), Some(HidrawMode::Enabled));
    let resolved = fixture
        .core()
        .with_controller_inventory(unavailable)
        .config_resolve(APP)
        .unwrap();
    assert!(resolved.hidraw_enabled);
    assert_eq!(resolved.hidraw_reason, WHY);
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
    assert_busy(other.prefix_exec(
        PREFIX,
        Path::new("x.exe"),
        &[] as &[OsString],
        &[],
        &mut sink,
    ));
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
    let _other = applejack_engine::rosetta_wine::snapshot::PrefixLock::acquire(
        &fixture.prefix_dir(),
        &fixture.data().join("prefixes/.snapshots"),
        PREFIX,
    )
    .unwrap();
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
fn a_failure_with_a_live_process_of_the_session_leaves_the_row_open() {
    let fixture = Fixture::new();
    fixture
        .core()
        .prefix_create(PREFIX, GraphicsBackend::Dxmt, None, HidrawMode::Disabled)
        .unwrap();
    fixture
        .core()
        .config_set(APP, "graphics_backend", "d3dmetal")
        .unwrap();
    let plan = fixture.core().plan_run(APP, false).unwrap();
    let id = plan.session_id();
    let key = fixture.db().session(id).unwrap().unwrap().session_key;
    let _left = Orphan::start(&fixture, &key);
    plan.run(&mut |_, _| {}).unwrap_err();
    let row = fixture.db().session(id).unwrap().unwrap();
    assert_eq!(row.ended_at, None, "{:?}", row.note);
    assert_eq!(row.state, SessionState::Starting);
    assert!(
        fs::read_to_string(row.log_path)
            .unwrap()
            .contains("left running:")
    );

    let end = fixture
        .core()
        .plan_cancel(APP, false)
        .unwrap()
        .run(&mut |_, _| {})
        .unwrap();
    assert_eq!(end.state, SessionState::Cancelled);
    assert_eq!(find_session(&key), []);
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
    core.config_set(APP, "launch_args_override", "-token hunter4")
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
    let key = fixture
        .db()
        .session(running.id)
        .unwrap()
        .unwrap()
        .session_key;
    let mut server = spawn_stub_server(&key, &fixture.prefix_dir());
    let row = fixture
        .core()
        .plan_cancel(APP, true)
        .unwrap()
        .run(&mut |_, _| {})
        .unwrap();
    assert_eq!(row.state, SessionState::Cancelled);
    assert_eq!(row.exit_code, Some(99), "the stub server ended the title");
    assert_eq!(find_session(&row.session_key), []);
    assert!(server.wait().unwrap().success());
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

/// The run on its own thread, with `on_phase` called for each step, so a test
/// can land something in the window between two of them.
fn start_watching(
    fixture: &Fixture,
    tuning: RunTuning,
    mut on_phase: impl FnMut(&str, i64) + Send + 'static,
) -> Running {
    let plan = fixture
        .core()
        .with_run_tuning(tuning)
        .plan_run(APP, false)
        .unwrap();
    let id = plan.session_id();
    let thread = thread::spawn(move || {
        let mut phases = Vec::new();
        let outcome = plan.run(&mut |phase, _| {
            on_phase(phase, id);
            phases.push(phase.to_owned());
        });
        (outcome, phases)
    });
    Running { id, thread }
}

/// Fails the test rather than hanging it, and lets the title go if it did.
fn finish_or_fail(fixture: &Fixture, running: Running) -> (SessionRow, Vec<String>) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !running.thread.is_finished() {
        if Instant::now() > deadline {
            fixture.quit(0);
            panic!("the run did not end");
        }
        thread::sleep(Duration::from_millis(10));
    }
    running.finish()
}

#[test]
fn a_cancel_inside_the_launch_window_ends_the_title_it_just_started() {
    let fixture = Fixture::new();
    let data = fixture.data();
    let running = start_watching(&fixture, FAST, move |phase, id| {
        if phase == "starting" {
            let db = ConfigDb::open(&data.join("db.sqlite")).unwrap();
            db.request_session_cancel(id).unwrap();
        }
    });
    let (row, phases) = finish_or_fail(&fixture, running);
    assert_eq!(row.state, SessionState::Cancelled);
    assert_eq!(
        row.exit_code,
        Some(137),
        "no server is up yet, so the loader's kill ended it"
    );
    assert_eq!(phases.last().map(String::as_str), Some("cancelled"));
    assert_eq!(find_session(&row.session_key), []);
    fixture.assert_lock_free();
}

#[test]
fn a_cancel_ends_what_a_launcher_forks_while_it_waits_for_the_owner() {
    let fixture = Fixture::new();
    fixture.register(Drm::None, Some("forking.exe"));
    let data = fixture.data();
    let (sender, starting) = std::sync::mpsc::channel();
    let running = start_watching(&fixture, FAST, move |phase, id| {
        if phase == "starting" {
            let db = ConfigDb::open(&data.join("db.sqlite")).unwrap();
            db.request_session_cancel(id).unwrap();
            sender.send(()).unwrap();
        }
    });
    starting.recv_timeout(Duration::from_secs(10)).unwrap();
    let row = fixture
        .core()
        .with_run_tuning(FAST)
        .plan_cancel(APP, false)
        .unwrap()
        .run(&mut |_, _| {})
        .unwrap();
    assert_eq!(row.state, SessionState::Cancelled);
    assert_eq!(find_session(&row.session_key), []);
    fixture.assert_lock_free();
    finish_or_fail(&fixture, running);
}

#[test]
fn a_quick_cancel_is_not_an_early_exit() {
    let fixture = Fixture::new();
    let mut tuning = FAST;
    tuning.early_exit = Duration::from_secs(60);
    let running = start_watching(&fixture, tuning, |_, _| {});
    fixture.wait_for(running.id, SessionState::Running);
    let row = fixture
        .core()
        .plan_cancel(APP, false)
        .unwrap()
        .run(&mut |_, _| {})
        .unwrap();
    assert_eq!(row.state, SessionState::Cancelled);
    let (end, _) = running.finish();
    assert_eq!(end.note, None);
}

#[test]
fn a_cancel_that_fails_the_launch_closes_as_cancelled_not_failed() {
    let fixture = Fixture::new();
    fixture
        .core()
        .config_set(APP, "graphics_backend", "d3dmetal")
        .unwrap();
    let plan = fixture.core().plan_run(APP, false).unwrap();
    let id = plan.session_id();
    fixture.db().request_session_cancel(id).unwrap();
    let row = plan.run(&mut |_, _| {}).unwrap();
    assert_eq!(row.state, SessionState::Cancelled);
    assert_eq!(row.exit_code, None);
    fixture.assert_lock_free_without_prefix();
}

#[test]
fn a_cancel_leaves_the_server_of_a_prefix_still_being_built() {
    let fixture = Fixture::new();
    fixture
        .core()
        .prefix_create(PREFIX, GraphicsBackend::Dxmt, None, HidrawMode::Disabled)
        .unwrap();
    let plan = fixture.core().plan_run(APP, false).unwrap();
    let id = plan.session_id();
    // The build's server carries the build's own session id.
    let mut server = spawn_stub_server(&own_key("prefix-build"), &fixture.prefix_dir());
    let mut tuning = FAST;
    tuning.drain = Duration::from_millis(100);
    tuning.cancel_wait = Duration::from_millis(500);
    let cancel = fixture
        .core()
        .with_run_tuning(tuning)
        .plan_cancel(APP, false)
        .unwrap()
        .run(&mut |_, _| {})
        .unwrap_err();
    assert!(cancel.to_string().contains("did not let go"), "{cancel}");
    assert!(
        !fixture.prefix_dir().join("server-quit").exists(),
        "a cancel stopped the server of a row that is still starting"
    );
    fs::write(fixture.prefix_dir().join("server-quit"), "").unwrap();
    server.wait().unwrap();
    let row = plan.run(&mut |_, _| {}).unwrap();
    assert_eq!(row.id, id);
    assert_eq!(row.state, SessionState::Cancelled);
}

#[test]
fn a_cancel_of_a_row_with_no_process_leaves_the_server_alone() {
    let fixture = Fixture::new();
    let id = orphan_row(&fixture, "run-dead-row");
    fixture
        .db()
        .mark_session_running(id, "2026-11-01T00:00:01Z")
        .unwrap();
    let mut server = spawn_stub_server(&own_key("prefix-exec"), &fixture.prefix_dir());
    let row = fixture
        .core()
        .with_run_tuning(SHORT_DRAIN)
        .plan_cancel(APP, false)
        .unwrap()
        .run(&mut |_, _| {})
        .unwrap();
    assert_eq!(row.state, SessionState::Cancelled);
    assert!(
        !fixture.prefix_dir().join("server-quit").exists(),
        "a cancel stopped a server the session has no part in"
    );
    fs::write(fixture.prefix_dir().join("server-quit"), "").unwrap();
    server.wait().unwrap();
}

#[test]
fn a_cancel_stops_a_server_that_outstays_the_title() {
    let fixture = Fixture::new();
    let mut tuning = FAST;
    tuning.drain = Duration::from_millis(100);
    let running = start_watching(&fixture, tuning, |_, _| {});
    fixture.wait_for(running.id, SessionState::Running);
    let row = fixture.db().session(running.id).unwrap().unwrap();
    let mut server = spawn_stub_server(&row.session_key, &fixture.prefix_dir());
    let end = fixture
        .core()
        .with_run_tuning(tuning)
        .plan_cancel(APP, false)
        .unwrap()
        .run(&mut |_, _| {})
        .unwrap();
    assert_eq!(end.state, SessionState::Cancelled);
    assert_eq!(find_session(&end.session_key), []);
    assert!(
        server.wait().unwrap().success(),
        "the server left on `-k`, not as a straggler"
    );
    fixture.assert_lock_free();
    running.finish();
}

#[test]
fn cancelling_a_title_that_is_not_running_is_refused() {
    let fixture = Fixture::new();
    let error = fixture.core().plan_cancel(APP, false).unwrap_err();
    assert_eq!(error.code(), ErrorCode::NotRunning, "{error}");
}

// The end of a session.

const SHORT_DRAIN: RunTuning = RunTuning {
    drain: Duration::from_millis(100),
    ..FAST
};

#[test]
fn a_busy_database_at_the_marker_does_not_abandon_the_title() {
    let fixture = Fixture::new();
    let plan = fixture.core().plan_run(APP, false).unwrap();
    let id = plan.session_id();
    let log = plan.log_path().to_owned();
    let writer = rusqlite::Connection::open(fixture.data().join("db.sqlite")).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    let running = Running {
        id,
        thread: thread::spawn(move || {
            let mut phases = Vec::new();
            let outcome = plan.run(&mut |phase, _| phases.push(phase.to_owned()));
            (outcome, phases)
        }),
    };
    wait_until("the marker to fail", || {
        fs::read_to_string(&log)
            .is_ok_and(|text| text.contains("could not record running"))
            .then_some(())
    });
    let row = fixture.db().session(id).unwrap().unwrap();
    assert_eq!(row.ended_at, None, "{:?}", row.note);
    assert_ne!(find_session(&row.session_key), []);
    assert!(
        !running.thread.is_finished(),
        "the owner stays with the title"
    );
    assert_busy(fixture.core().prefix_snapshot(PREFIX));

    writer.execute_batch("ROLLBACK").unwrap();
    fixture.wait_for(id, SessionState::Running);
    fixture.quit(4);
    let (end, phases) = running.finish();
    assert_eq!(end.state, SessionState::Exited);
    assert_eq!(end.exit_code, Some(4));
    assert_eq!(
        phases,
        ["prepare", "prefix", "starting", "running", "exited"]
    );
    fixture.assert_lock_free();
}

#[test]
fn a_session_that_outlives_its_launcher_stays_open_and_locked_until_it_drains() {
    let fixture = Fixture::new();
    fixture.register(Drm::None, Some("launcher.exe"));
    let mut tuning = SHORT_DRAIN;
    tuning.early_exit = Duration::from_secs(60);
    let (sender, lingering) = std::sync::mpsc::channel();
    let running = start_watching(&fixture, tuning, move |phase, _| {
        if phase == "lingering" {
            sender.send(()).unwrap();
        }
    });
    lingering.recv_timeout(Duration::from_secs(10)).unwrap();
    let row = fixture.db().session(running.id).unwrap().unwrap();
    assert_eq!(row.ended_at, None);
    assert_eq!(row.state, SessionState::Running);
    assert_eq!(
        find_session(&row.session_key)[0].exe,
        "game.exe",
        "the launcher's game is what is left"
    );
    assert_busy(fixture.core().prefix_snapshot(PREFIX));
    assert_refused(&fixture, ErrorCode::AlreadyRunning);

    fixture.quit(0);
    let (end, _) = finish_or_fail(&fixture, running);
    assert_eq!(end.state, SessionState::Exited);
    assert_eq!(
        end.exit_code,
        Some(3),
        "the launch exe's status, not the game's"
    );
    assert_eq!(end.note.as_deref(), Some(NOTE_LINGERED));
    assert_eq!(find_session(&end.session_key), []);
    fixture.assert_lock_free();
}

#[test]
fn a_lingering_session_keeps_its_start_and_ends_when_it_drains() {
    // Stamps have one second of resolution.
    const PAST_A_STAMP: Duration = Duration::from_millis(1100);
    let fixture = Fixture::new();
    fixture.register(Drm::None, Some("immortal.exe"));
    let (sender, lingering) = std::sync::mpsc::channel();
    let running = start_watching(&fixture, SHORT_DRAIN, move |phase, _| {
        if phase == "lingering" {
            sender.send(utc_now()).unwrap();
        }
    });
    fixture.wait_for(running.id, SessionState::Running);
    let at_marker = fixture.db().session(running.id).unwrap().unwrap();
    thread::sleep(PAST_A_STAMP);
    fixture.quit(0);
    let lingered_at = lingering.recv_timeout(Duration::from_secs(10)).unwrap();
    let row = fixture.db().session(running.id).unwrap().unwrap();
    assert_eq!(row.state, SessionState::Running);
    assert_eq!(row.started_at, at_marker.started_at, "the marker's time");
    thread::sleep(PAST_A_STAMP);
    for process in find_session(&row.session_key) {
        signal(process.pid, true).unwrap();
    }
    let (end, _) = finish_or_fail(&fixture, running);
    assert_eq!(end.state, SessionState::Exited);
    assert_eq!(end.exit_code, Some(0));
    assert_eq!(end.note.as_deref(), Some(NOTE_LINGERED));
    assert_eq!(end.started_at, at_marker.started_at);
    let ended_at = end.ended_at.expect("the row closed");
    assert!(
        ended_at > lingered_at,
        "{ended_at} is when it drained, later than {lingered_at}"
    );
}

#[test]
fn cancel_reaches_the_game_a_launcher_left() {
    let fixture = Fixture::new();
    fixture.register(Drm::None, Some("launcher.exe"));
    let (sender, lingering) = std::sync::mpsc::channel();
    let running = start_watching(&fixture, SHORT_DRAIN, move |phase, _| {
        if phase == "lingering" {
            sender.send(()).unwrap();
        }
    });
    lingering.recv_timeout(Duration::from_secs(10)).unwrap();
    let row = fixture
        .core()
        .with_run_tuning(SHORT_DRAIN)
        .plan_cancel(APP, false)
        .unwrap()
        .run(&mut |_, _| {})
        .unwrap();
    assert_eq!(row.state, SessionState::Cancelled);
    assert_eq!(find_session(&row.session_key), []);
    fixture.assert_lock_free();
    assert_eq!(finish_or_fail(&fixture, running).0.exit_code, Some(3));
}

#[test]
fn cancel_ends_a_helper_nothing_else_will() {
    let fixture = Fixture::new();
    fixture.register(Drm::None, Some("immortal.exe"));
    let running = start_watching(&fixture, SHORT_DRAIN, |_, _| {});
    fixture.wait_for(running.id, SessionState::Running);
    let row = fixture
        .core()
        .with_run_tuning(SHORT_DRAIN)
        .plan_cancel(APP, false)
        .unwrap()
        .run(&mut |_, _| {})
        .unwrap();
    assert_eq!(row.state, SessionState::Cancelled);
    assert_eq!(find_session(&row.session_key), []);
    fixture.assert_lock_free();
    finish_or_fail(&fixture, running);
}

#[test]
fn the_row_closes_only_when_the_server_has_gone() {
    let fixture = Fixture::new();
    let running = start_watching(&fixture, SHORT_DRAIN, |_, _| {});
    fixture.wait_for(running.id, SessionState::Running);
    let row = fixture.db().session(running.id).unwrap().unwrap();
    let mut server = spawn_stub_server(&row.session_key, &fixture.prefix_dir());
    fixture.quit(0);
    thread::sleep(Duration::from_millis(400));
    assert_eq!(
        fixture.db().session(running.id).unwrap().unwrap().ended_at,
        None
    );
    assert!(!running.thread.is_finished());
    fs::write(fixture.prefix_dir().join("server-quit"), "").unwrap();
    server.wait().unwrap();
    let (end, phases) = running.finish();
    assert_eq!(end.exit_code, Some(0));
    assert_eq!(end.note.as_deref(), Some(NOTE_LINGERED));
    assert!(phases.contains(&"lingering".to_owned()), "{phases:?}");
}

#[test]
fn a_run_waits_out_the_server_the_last_one_left() {
    let fixture = Fixture::new();
    let running = fixture.start();
    fixture.quit(0);
    running.finish();
    fs::remove_file(fixture.prefix_dir().join("quit")).unwrap();
    let mut server = spawn_stub_server(&own_key("run-before"), &fixture.prefix_dir());
    let quit = fixture.prefix_dir().join("server-quit");
    let leaves = thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        fs::write(quit, "").unwrap();
        server.wait().unwrap();
    });
    let running = fixture.start();
    leaves.join().unwrap();
    fixture.quit(0);
    running.finish();
}

// A core that died.

/// A session key no other process on the host shares: `find_session` reads
/// the whole process table, so a second copy of a test running beside this
/// one (another worktree's suite) would otherwise see, adopt or cancel this
/// one's helpers.
fn own_key(name: &str) -> String {
    format!("{name}-{}", std::process::id())
}

/// An open row as a dead core leaves it: nobody holds the lock.
fn orphan_row(fixture: &Fixture, key: &str) -> i64 {
    fixture
        .core()
        .prefix_create(PREFIX, GraphicsBackend::Dxmt, None, HidrawMode::Disabled)
        .unwrap();
    let log = fixture.data().join(format!("{key}.log"));
    fixture
        .db()
        .insert_session(&NewSession {
            appid: APP,
            started_at: "2026-11-01T00:00:00Z",
            engine_id: "rosetta-wine",
            engine_version: "10.0.0",
            log_path: log.to_str().unwrap(),
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
        Self::named(fixture, key, "title.exe")
    }

    fn named(fixture: &Fixture, key: &str, exe: &str) -> Self {
        let child = spawn_stub_title(key, &fixture.prefix_dir(), exe);
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
    let key = own_key("run-live");
    let id = orphan_row(&fixture, &key);
    let _title = Orphan::start(&fixture, &key);
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
fn a_dead_owners_session_answers_busy_at_once_not_after_the_settle() {
    let fixture = Fixture::new();
    let key = own_key("run-gap");
    orphan_row(&fixture, &key);
    let _title = Orphan::start(&fixture, &key);
    let mut server = spawn_stub_server(&key, &fixture.prefix_dir());
    let started = Instant::now();
    let mut core = fixture.core();
    assert_busy(core.prefix_snapshot(PREFIX));
    assert_busy(core.prefix_reset(PREFIX, false));
    assert_busy(core.prefix_remove(PREFIX));
    assert_busy(core.prefix_restore(PREFIX, "20260101T000000Z"));
    assert_busy(core.prefix_exec(
        PREFIX,
        Path::new("x.exe"),
        &[] as &[OsString],
        &[],
        &mut Vec::new(),
    ));
    assert!(started.elapsed() < Duration::from_secs(5), "it stalled");
    fs::write(fixture.prefix_dir().join("server-quit"), "").unwrap();
    server.wait().unwrap();
}

#[test]
fn a_game_a_dead_owners_launcher_left_is_taken_over_not_lost() {
    let fixture = Fixture::new();
    let key = own_key("run-left");
    let id = orphan_row(&fixture, &key);
    let game = Orphan::named(&fixture, &key, "game.exe");
    let mut adopted = fixture.core().reconcile_sessions().unwrap();
    assert_eq!(adopted.len(), 1, "title.exe is gone and its game is not");
    assert_eq!(fixture.db().session(id).unwrap().unwrap().ended_at, None);
    let session = adopted.pop().unwrap();
    let thread = thread::spawn(move || session.watch(&mut |_, _| {}));
    assert_busy(fixture.core().prefix_snapshot(PREFIX));
    fixture.quit(0);
    drop(game);
    let row = thread.join().unwrap().unwrap();
    assert_eq!(row.state, SessionState::Exited);
}

#[test]
fn a_new_core_takes_over_a_live_title_and_holds_the_prefix() {
    let fixture = Fixture::new();
    let key = own_key("run-adopt");
    let id = orphan_row(&fixture, &key);
    let title = Orphan::start(&fixture, &key);
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
    let key = own_key("run-cancel-orphan");
    let id = orphan_row(&fixture, &key);
    let mut title = Orphan::start(&fixture, &key);
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
    assert_eq!(find_session(&key), []);
    drop(reaper.join().unwrap());
    fixture.assert_lock_free();
}

/// Keeps a helper of the session running on its own thread, the way a
/// watchdog no list has seen yet would: each time the helper dies the next
/// is started, `respawns` times (`respawns + 1` helpers in all), so with a
/// bounded `respawns` the last kill pass leaves nothing, and every spawn
/// follows a death by a few ms, never a clock. `start` returns with the
/// first helper spawned, not yet necessarily listed; `stop` says how each
/// helper that died before it died (the one it kills itself is not listed)
/// and `drop` kills whichever is current.
struct Respawner {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Vec<ExitStatus>>>,
}

impl Respawner {
    fn start(fixture: &Fixture, key: &str, respawns: usize) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let prefix = fixture.prefix_dir();
        let key = key.to_owned();
        let mut child = spawn_stub_title(&key, &prefix, "helper.exe");
        let thread = thread::spawn(move || {
            let mut deaths = Vec::new();
            let mut left = respawns;
            while !stopping.load(Ordering::Relaxed) {
                if let Ok(Some(status)) = child.try_wait() {
                    deaths.push(status);
                    if left == 0 {
                        return deaths;
                    }
                    left -= 1;
                    child = spawn_stub_title(&key, &prefix, "helper.exe");
                }
                thread::sleep(Duration::from_millis(2));
            }
            // A death the loop had not polled yet is the core's, not ours.
            if let Ok(Some(status)) = child.try_wait() {
                deaths.push(status);
            } else {
                let _ = child.kill();
                let _ = child.wait();
            }
            deaths
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }

    /// Stops the respawner; how each helper that died before the stop died.
    /// A respawner thread that panicked (a spawn that failed) panics here,
    /// where `Drop` would swallow it.
    fn stop(mut self) -> Vec<ExitStatus> {
        self.stop.store(true, Ordering::Relaxed);
        self.thread
            .take()
            .map_or_else(Vec::new, |thread| thread.join().unwrap())
    }
}

impl Drop for Respawner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[test]
fn cancel_keeps_killing_until_the_session_is_empty() {
    let fixture = Fixture::new();
    let key = own_key("run-respawn-ends");
    let id = orphan_row(&fixture, &key);
    let respawns = 2;
    // A look comes `tick` after each kill pass: wide enough for the next
    // helper, spawned a few ms after the kill, to precede the look's
    // snapshot, so every look until the last is non-empty (the respawner's
    // 2 ms poll plus a spawn must stay well under `tick`). The kill passes
    // must also fit the second `term_wait` `end_stragglers` allows: the
    // last kill lands at worst term_wait + (respawns + 1) * (tick + look),
    // about 0.4 s into the second 1 s with a 30 ms look, and the empty look
    // after it (which returns at any `waited`) lands near 1.5 s of the 2 s
    // bound.
    let mut tuning = SHORT_DRAIN;
    tuning.term_wait = Duration::from_secs(1);
    tuning.tick = Duration::from_millis(100);
    let respawner = Respawner::start(&fixture, &key, respawns);
    let row = fixture
        .core()
        .with_run_tuning(tuning)
        .plan_cancel(APP, false)
        .unwrap()
        .run(&mut |_, _| {})
        .unwrap();
    assert_eq!(row.id, id);
    assert_eq!(row.state, SessionState::Cancelled);
    // The look precedes the stop, which would kill the current helper.
    let left = find_session(&key);
    let deaths = respawner.stop();
    assert_eq!(
        deaths.len(),
        respawns + 1,
        "every helper was killed by the cancel: {deaths:?}; left {left:?}"
    );
    assert!(
        deaths.iter().all(|status| status.signal() == Some(9)),
        "each helper died of the cancel's SIGKILL: {deaths:?}"
    );
    assert_eq!(
        left,
        [],
        "nothing of the session outlived the row: {deaths:?}"
    );
    fixture.assert_lock_free();
}

#[test]
fn a_cancel_that_cannot_empty_the_session_leaves_the_row_open() {
    let fixture = Fixture::new();
    let key = own_key("run-respawn-stays");
    let id = orphan_row(&fixture, &key);
    let respawner = Respawner::start(&fixture, &key, usize::MAX);
    let error = fixture
        .core()
        .with_run_tuning(SHORT_DRAIN)
        .plan_cancel(APP, false)
        .unwrap()
        .run(&mut |_, _| {})
        .unwrap_err();
    assert!(error.to_string().contains("still has processes"), "{error}");
    let row = fixture.db().session(id).unwrap().unwrap();
    assert_eq!(row.ended_at, None);
    assert!(row.cancel_requested);
    let deaths = respawner.stop();
    assert!(
        !deaths.is_empty() && deaths.iter().all(|status| status.signal() == Some(9)),
        "the cancel kept killing helpers while the row stayed open: {deaths:?}"
    );
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
    assert_eq!(
        phases,
        ["prepare", "prefix", "starting", "running", "cancelled"]
    );
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

    let live = own_key("run-hello-live");
    let adopted = fixture
        .db()
        .insert_session(&NewSession {
            appid: APP,
            started_at: "2026-11-01T00:00:00Z",
            engine_id: "rosetta-wine",
            engine_version: "10.0.0",
            log_path: "/nonexistent/wine.log",
            session_key: &live,
            prefix: PREFIX,
            launch_exe: "title.exe",
        })
        .unwrap()
        .unwrap();
    let title = Orphan::start(&fixture, &live);
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
    let mut handler = CoreHandler::new(Core::new(&data).with_controllers(Vec::new()));
    let hello = Method::Hello(HelloParams {
        protocol_version: PROTOCOL_VERSION,
    });
    request(&mut handler, &notifier, &hello).unwrap();
    assert!(!data.exists());
}
