//! The operations the CLI and the RPC handler share, over a stub bundle.

mod common;

use std::ffi::OsString;
use std::fs;
use std::path::Path;

use applejack_core::{Core, CoreError};
use applejack_engine::{Error as EngineError, GraphicsBackend, HidrawMode, Severity, SyncMode};
use common::write_stub_bundle;
use serde_json::json;
use tempfile::TempDir;

fn core(dir: &TempDir) -> Core {
    Core::new(dir.path().join("data")).with_unsigned_installs(true)
}

/// A core with the stub bundle installed and active.
fn installed(dir: &TempDir, version: &str) -> Core {
    let tree = dir.path().join(format!("tree-{version}"));
    write_stub_bundle(&tree, version);
    let core = core(dir);
    core.plan_install("rosetta-wine", Some(&tree))
        .unwrap()
        .run(&mut |_| {})
        .unwrap();
    core
}

fn create(core: &mut Core, name: &str) {
    core.prefix_create(name, GraphicsBackend::Dxmt, None, HidrawMode::Disabled)
        .unwrap();
}

#[test]
fn install_activates_the_first_version_and_leaves_no_staging_behind() {
    let dir = TempDir::new().unwrap();
    let mut core = installed(&dir, "10.0.0");
    let list = core.engine_list().unwrap().engines;
    assert_eq!(list.len(), 1);
    assert!(list[0].installed && list[0].active, "{list:?}");
    let report = core.engine_doctor().unwrap();
    assert!(report.runnable, "{report:?}");

    let root = core
        .layout()
        .engine_root(applejack_engine::EngineId::RosettaWine);
    let mut names: Vec<String> = fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, [".install.lock", "10.0.0"]);
}

#[test]
fn a_second_version_stays_inactive_until_used() {
    let dir = TempDir::new().unwrap();
    let mut core = installed(&dir, "10.0.0");
    let tree = dir.path().join("tree-10.1.0");
    write_stub_bundle(&tree, "10.1.0");
    let second = core
        .plan_install("rosetta-wine", Some(&tree))
        .unwrap()
        .run(&mut |_| {})
        .unwrap();
    assert!(!second.active);
    core.engine_use("rosetta-wine", "10.1.0").unwrap();
    let list = core.engine_list().unwrap().engines;
    assert_eq!(
        list.iter()
            .map(|entry| (entry.version.as_str(), entry.active))
            .collect::<Vec<_>>(),
        [("10.0.0", false), ("10.1.0", true)]
    );
    let missing = core.engine_use("rosetta-wine", "9.0.0").unwrap_err();
    assert!(
        matches!(missing, CoreError::Engine(EngineError::NotInstalled { .. })),
        "{missing}"
    );
}

#[test]
fn doctor_with_no_engine_reports_the_fix_and_touches_nothing() {
    let dir = TempDir::new().unwrap();
    let mut core = core(&dir);
    let report = core.engine_doctor().unwrap();
    assert!(!report.runnable);
    let fix = report.findings.last().unwrap();
    assert_eq!(fix.severity, Severity::Info);
    assert!(fix.message.contains("--from"), "{}", fix.message);
    assert!(fix.message.contains("APPLEJACK_ALLOW_UNSIGNED_ENGINE=1"));
    assert_eq!(core.engine_list().unwrap().engines, Vec::new());
    assert!(!dir.path().join("data").exists(), "a read wrote to disk");
}

#[test]
fn doctor_names_engine_use_when_versions_exist_but_none_is_active() {
    let dir = TempDir::new().unwrap();
    let tree = dir.path().join("tree");
    write_stub_bundle(&tree, "10.0.0");
    let mut core = core(&dir);
    let target = core.layout().engine_dir(
        applejack_engine::EngineId::RosettaWine,
        &"10.0.0".parse().unwrap(),
    );
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::rename(&tree, &target).unwrap();
    let report = core.engine_doctor().unwrap();
    assert!(!report.runnable);
    assert!(
        report
            .findings
            .last()
            .unwrap()
            .message
            .contains("engine use rosetta-wine 10.0.0"),
        "{report:?}"
    );
}

#[test]
fn install_refusals_name_the_way_out() {
    let dir = TempDir::new().unwrap();
    let core = core(&dir);
    let error = core.plan_install("rosetta-wine", None).unwrap_err();
    assert!(matches!(error, CoreError::NoPublishedVersion { .. }));
    let message = error.to_string();
    assert!(message.contains("no published version"), "{message}");
    assert!(message.contains("--from"), "{message}");

    let refused = Core::new(dir.path())
        .plan_install("rosetta-wine", Some(Path::new("/nowhere")))
        .unwrap_err();
    assert!(matches!(refused, CoreError::UnsignedRefused));

    let unknown = core.plan_install("wine", None).unwrap_err();
    assert!(matches!(
        unknown,
        CoreError::Engine(EngineError::UnknownEngineId(_))
    ));
    let native = core.plan_install("native-arm64", None).unwrap_err();
    assert!(matches!(native, CoreError::EngineUnavailable { .. }));
}

#[test]
fn prefix_needs_an_active_engine_and_a_usable_name() {
    let dir = TempDir::new().unwrap();
    let mut core = core(&dir);
    let error = core
        .prefix_create("test", GraphicsBackend::Dxmt, None, HidrawMode::Disabled)
        .unwrap_err();
    assert!(matches!(error, CoreError::NoActiveEngine { .. }), "{error}");
    assert!(error.to_string().contains("--from"), "{error}");
    for name in ["", "..", ".snapshots", "a/b"] {
        let error = core
            .prefix_create(name, GraphicsBackend::Dxmt, None, HidrawMode::Disabled)
            .unwrap_err();
        assert!(
            matches!(
                error,
                CoreError::Engine(EngineError::InvalidSnapshotName { .. })
            ),
            "{name}: {error}"
        );
    }
}

#[test]
fn prefix_create_list_and_sync_default() {
    let dir = TempDir::new().unwrap();
    let mut core = installed(&dir, "10.0.0");
    assert_eq!(core.prefix_list().unwrap(), Vec::new());
    let entry = core
        .prefix_create("b", GraphicsBackend::Dxmt, None, HidrawMode::Disabled)
        .unwrap();
    assert_eq!(entry.sync_mode, Some(SyncMode::Msync));
    create(&mut core, "a");
    let unsupported = core
        .prefix_create("c", GraphicsBackend::D3DMetal, None, HidrawMode::Disabled)
        .unwrap_err();
    assert!(
        matches!(
            unsupported,
            CoreError::Engine(EngineError::BackendUnavailable { .. })
        ),
        "{unsupported}"
    );
    fs::create_dir(core.data_dir().join("prefixes/not-a-prefix")).unwrap();
    let list = core.prefix_list().unwrap();
    let names: Vec<&str> = list.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names, ["a", "b", "not-a-prefix"], ".snapshots never lists");
    assert!(list[2].problem.is_some());
    assert_eq!(
        fs::read_link(core.data_dir().join("prefixes/a/dosdevices/s:")).unwrap(),
        core.data_dir().join("library")
    );
}

#[test]
fn snapshot_restore_reset_and_remove() {
    let dir = TempDir::new().unwrap();
    let mut core = installed(&dir, "10.0.0");
    create(&mut core, "p");
    let prefix = core.data_dir().join("prefixes/p");
    let marker = prefix.join("drive_c/users/me/save.txt");
    fs::write(&marker, "first").unwrap();

    let snapshot = core.prefix_snapshot("p").unwrap();
    fs::write(&marker, "second").unwrap();
    let safety = core.prefix_restore("p", &snapshot.timestamp).unwrap();
    assert_ne!(safety.timestamp, snapshot.timestamp);
    assert_eq!(fs::read_to_string(&marker).unwrap(), "first");

    let missing = core.prefix_restore("p", "19700101T000000Z").unwrap_err();
    assert!(matches!(
        missing,
        CoreError::Engine(EngineError::SnapshotNotFound { .. })
    ));

    core.prefix_reset("p", true).unwrap();
    assert_eq!(fs::read_to_string(&marker).unwrap(), "first", "kept");
    core.prefix_reset("p", false).unwrap();
    assert!(!marker.exists(), "a plain reset drops user data");
    assert!(prefix.join("dosdevices/s:").symlink_metadata().is_ok());

    core.prefix_remove("p").unwrap();
    assert!(!prefix.exists());
    assert!(
        core.data_dir().join("prefixes/.snapshots/p").is_dir(),
        "snapshots outlive the prefix"
    );
    let gone = core.prefix_remove("p").unwrap_err();
    assert!(matches!(gone, CoreError::UnknownPrefix { .. }), "{gone}");
    assert!(matches!(
        core.prefix_snapshot("p").unwrap_err(),
        CoreError::UnknownPrefix { .. }
    ));
}

#[test]
fn prefix_exec_returns_the_exit_code_and_the_output() {
    let dir = TempDir::new().unwrap();
    let mut core = installed(&dir, "10.0.0");
    create(&mut core, "p");
    let mut out = Vec::new();
    let exit = core
        .prefix_exec(
            "p",
            Path::new("exit7.exe"),
            &[OsString::from("argument")],
            &mut out,
        )
        .unwrap();
    assert_eq!(exit, 7);
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("hello from the stub: argument"), "{text}");
}

#[test]
fn prefix_exec_refuses_a_unix_path_outside_the_data_directory() {
    let dir = TempDir::new().unwrap();
    let mut core = installed(&dir, "10.0.0");
    create(&mut core, "p");
    let outside = dir.path().join("elsewhere.exe");
    fs::write(&outside, "x").unwrap();
    let error = core
        .prefix_exec("p", &outside, &[], &mut Vec::new())
        .unwrap_err();
    assert!(
        matches!(error, CoreError::PathOutsideData { .. }),
        "{error}"
    );
}

#[test]
fn probe_cpuid_stores_both_runs() {
    let dir = TempDir::new().unwrap();
    let mut core = installed(&dir, "10.0.0");
    let result = core.engine_probe_cpuid().unwrap();
    assert!(result.changes_bits);
    assert!(result.probe.advertised.avx);
    assert!(!result.probe.plain.avx);
    let db = applejack_core::config::open_db(core.data_dir()).unwrap();
    let stored = db
        .meta_get(applejack_core::engines::META_CPUID_PROBE)
        .unwrap()
        .unwrap();
    let stored: serde_json::Value = serde_json::from_str(&stored).unwrap();
    assert_eq!(stored["advertised"]["avx"], json!(true));
}

fn gptk_volume(root: &Path) {
    let payload = root.join("redist/lib/external");
    fs::create_dir_all(payload.join("D3DMetal.framework")).unwrap();
    fs::write(payload.join("libd3dshared.dylib"), "x").unwrap();
    fs::write(root.join("README.md"), "Game Porting Toolkit 3.0\n").unwrap();
}

#[test]
fn d3dmetal_import_needs_the_license_and_remove_clears_the_record() {
    let dir = TempDir::new().unwrap();
    let mut core = installed(&dir, "10.0.0");
    let volume = dir.path().join("volume");
    gptk_volume(&volume);
    let refused = core.engine_d3dmetal_import(&volume, false).unwrap_err();
    assert!(
        matches!(
            refused,
            CoreError::Engine(EngineError::LicenseNotAccepted { .. })
        ),
        "{refused}"
    );
    let overlay = core.engine_d3dmetal_import(&volume, true).unwrap();
    assert_eq!(overlay.gptk_version, "3.0");
    assert!(overlay.overlay_dir.join("libd3dshared.dylib").is_file());
    let db = applejack_core::config::open_db(core.data_dir()).unwrap();
    assert!(db.d3dmetal_accepted("10.0.0").unwrap());
    core.engine_d3dmetal_remove().unwrap();
    assert!(!overlay.overlay_dir.exists());
    assert!(!db.d3dmetal_accepted("10.0.0").unwrap());
}
