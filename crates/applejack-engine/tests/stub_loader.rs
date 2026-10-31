//! Prefix creation, `apply_overrides`, launch, and cancel driven by stub
//! `wine` and `wineserver` shell scripts over the fixture bundle: the error
//! paths the real payload cannot produce on demand (a failing or hanging
//! `wineboot`, a failing `reg import`), the order the record is written in,
//! and the environment the loader child sees. Nothing here runs Wine.

mod common;

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use applejack_engine::rosetta_wine::prefix::PrefixRecord;
use applejack_engine::rosetta_wine::{CpuidFeatures, CpuidProbe};
use applejack_engine::{
    Engine, Error, GraphicsBackend, HidrawMode, InstalledEngine, LaunchSpec, PrefixSpec,
    RosettaWine, SyncMode,
};
use common::{fake_installed, layout, rosetta, version};

/// The stub loader. `$1` is the Wine command (`wineboot`, `reg`, or the
/// executable). It appends to `<bundle>/calls.log`, dumps its environment
/// to `<bundle>/env-<name>.txt`, writes its pid to `<bundle>/pid-<name>`,
/// obeys `<bundle>/mode-<name>` (`fail`, `hang`, or `complain`: exit 0 with
/// the line `reg import` prints for a key it cannot import), and for
/// `wineboot` creates the directories a real boot would.
const WINE: &str = r#"#!/bin/sh
D=$(cd "$(dirname "$0")/../.." && pwd)
name=$(basename "$1")
echo "wine $*" >> "$D/calls.log"
env | sort > "$D/env-$name.txt"
echo $$ > "$D/pid-$name"
mode=$(cat "$D/mode-$name" 2>/dev/null)
case "$mode" in
  fail) echo "stub: $name failed" >&2; exit 3 ;;
  hang) exec sleep 100007 ;;
  complain) echo "reg: Unable to import the registry key 'HKXX\\Bad'"; exit 0 ;;
esac
if [ "$name" = wineboot ]; then
  mkdir -p "$WINEPREFIX/dosdevices" "$WINEPREFIX/drive_c/windows/system32" "$WINEPREFIX/drive_c/windows/syswow64"
fi
exit 0
"#;

const SERVER: &str = r#"#!/bin/sh
D=$(cd "$(dirname "$0")/../.." && pwd)
echo "wineserver $* WINEPREFIX=$WINEPREFIX" >> "$D/calls.log"
exit 0
"#;

fn script(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// The fixture bundle with the loader and server replaced by the stubs.
fn stub_engine(dir: &Path) -> InstalledEngine {
    let layout = layout(dir);
    fake_installed(&layout, "10.0.0");
    let engine = InstalledEngine::open(&layout, rosetta(), &version("10.0.0")).unwrap();
    script(&engine.root.join("wine/bin/wine64"), WINE);
    script(&engine.root.join("wine/bin/wineserver"), SERVER);
    engine
}

fn spec(dir: &Path, backend: GraphicsBackend) -> PrefixSpec {
    PrefixSpec {
        backend,
        sync_mode: SyncMode::Esync,
        hidraw: HidrawMode::Enabled,
        library_dir: None,
        log_dir: dir.join("logs"),
        boot_timeout: None,
    }
}

fn launch_spec(prefix_dir: &Path, logs: &Path, extras: &[(&str, &str)]) -> LaunchSpec {
    LaunchSpec {
        exe: PathBuf::from("game.exe"),
        args: Vec::new(),
        env_extras: extras
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect(),
        cwd: prefix_dir.to_path_buf(),
        log_dir: logs.to_path_buf(),
        advertise_avx: false,
        cpuid_probe: None,
    }
}

fn calls(engine: &InstalledEngine) -> String {
    fs::read_to_string(engine.root.join("calls.log")).unwrap_or_default()
}

fn env_seen(engine: &InstalledEngine, name: &str) -> String {
    fs::read_to_string(engine.root.join(format!("env-{name}.txt"))).unwrap()
}

fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn process_exists(pid: &str) -> bool {
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", pid.trim()])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    // A reaped process is gone; a zombie would show as `Z`.
    !text.trim().is_empty()
}

/// From the engine-contract review of 4719a27: the plan's "wineboot failure
/// removes the prefix and reports the log path", offline.
#[test]
fn wineboot_failure_removes_the_prefix_and_names_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let engine = stub_engine(dir.path());
    fs::write(engine.root.join("mode-wineboot"), "fail").unwrap();
    let prefix_dir = dir.path().join("prefix");
    let err = RosettaWine
        .create_prefix(
            &engine,
            &prefix_dir,
            &spec(dir.path(), GraphicsBackend::Dxmt),
        )
        .unwrap_err();
    let Error::WineCommandFailed { log, .. } = &err else {
        panic!("{err:?}")
    };
    assert!(
        log.starts_with(dir.path().join("logs")),
        "{}",
        log.display()
    );
    assert!(
        fs::read_to_string(log)
            .unwrap()
            .contains("stub: wineboot failed")
    );
    assert!(!prefix_dir.exists(), "prefix removed");
    assert!(err.to_string().contains("wineboot --init"));
    assert!(err.to_string().contains(&log.display().to_string()));
    let calls = calls(&engine);
    assert!(
        calls.contains("wine wineboot --init\n") && calls.contains("wineserver -k "),
        "the server is stopped before the directory goes:\n{calls}"
    );

    // A caller-supplied empty directory is emptied, not removed.
    let supplied = dir.path().join("supplied");
    fs::create_dir_all(&supplied).unwrap();
    RosettaWine
        .create_prefix(&engine, &supplied, &spec(dir.path(), GraphicsBackend::Dxmt))
        .unwrap_err();
    assert!(supplied.is_dir(), "the caller's directory stays");
    assert_eq!(listing(&supplied), Vec::<String>::new());
}

#[test]
fn wineboot_past_its_bound_is_killed_and_the_prefix_removed() {
    let dir = tempfile::tempdir().unwrap();
    let engine = stub_engine(dir.path());
    fs::write(engine.root.join("mode-wineboot"), "hang").unwrap();
    let prefix_dir = dir.path().join("prefix");
    let mut spec = spec(dir.path(), GraphicsBackend::WineD3D);
    spec.boot_timeout = Some(Duration::from_secs(1));
    let started = Instant::now();
    let err = RosettaWine
        .create_prefix(&engine, &prefix_dir, &spec)
        .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    let Error::WineCommandFailed { status, log, .. } = &err else {
        panic!("{err:?}")
    };
    assert!(status.starts_with("timed out after 1s (pid "), "{status}");
    assert!(log.is_file() && log.starts_with(dir.path().join("logs")));
    assert!(!prefix_dir.exists(), "prefix removed");
    assert_eq!(
        calls(&engine).matches("wineserver -k ").count(),
        1,
        "the server is killed once, by the failed create:\n{}",
        calls(&engine)
    );
    let pid = fs::read_to_string(engine.root.join("pid-wineboot")).unwrap();
    assert!(
        !process_exists(&pid),
        "the hung loader (pid {pid}) was killed and reaped"
    );
}

/// From both reviews of 4719a27: a failed seed import must leave a record
/// that still names every DLL on disk, so the next apply cleans up.
#[test]
fn failed_seed_import_keeps_the_record_naming_the_dlls_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let engine = stub_engine(dir.path());
    let prefix_dir = dir.path().join("prefix");
    let prefix = RosettaWine
        .create_prefix(
            &engine,
            &prefix_dir,
            &spec(dir.path(), GraphicsBackend::Dxmt),
        )
        .unwrap();
    let system32 = prefix_dir.join("drive_c/windows/system32");
    let syswow64 = prefix_dir.join("drive_c/windows/syswow64");
    assert_eq!(
        listing(&system32),
        ["d3d10core.dll", "d3d11.dll", "dxgi.dll"]
    );
    assert_eq!(listing(&syswow64), ["d3d11.dll"]);
    assert!(calls(&engine).contains("wine reg import seeds.reg\n"));

    fs::write(engine.root.join("mode-reg"), "fail").unwrap();
    let err = RosettaWine
        .apply_overrides(
            &engine,
            &prefix,
            GraphicsBackend::Vkd3dProton,
            SyncMode::None,
            HidrawMode::Disabled,
        )
        .unwrap_err();
    assert!(err.to_string().contains("reg import seeds.reg"), "{err}");
    assert_eq!(listing(&system32), ["d3d12.dll", "d3d12core.dll"]);
    assert_eq!(listing(&syswow64), Vec::<String>::new());
    let record = PrefixRecord::read(&prefix_dir).unwrap();
    assert_eq!(record.backend, GraphicsBackend::Vkd3dProton);
    assert_eq!(
        record.copied_dlls,
        [
            PathBuf::from("drive_c/windows/system32/d3d12.dll"),
            PathBuf::from("drive_c/windows/system32/d3d12core.dll"),
        ],
        "the record names what is on disk, not what the seed step reached"
    );
    // The registry-backed settings were never seeded, so the record (and
    // `open_prefix`) keep saying what the registry holds (review of
    // 8b5bfd9).
    assert_eq!(record.hidraw, HidrawMode::Enabled);
    assert_eq!(record.sync_mode, SyncMode::Esync);
    let reopened = RosettaWine.open_prefix(&prefix_dir).unwrap();
    assert_eq!(reopened.hidraw, HidrawMode::Enabled);
    assert_eq!(reopened.sync_mode, SyncMode::Esync);
    assert_eq!(reopened.backend, GraphicsBackend::Vkd3dProton);
    assert!(
        calls(&engine).contains("wineserver -k "),
        "a failed seed stops the prefix's server:\n{}",
        calls(&engine)
    );

    fs::remove_file(engine.root.join("mode-reg")).unwrap();
    let restored = RosettaWine
        .apply_overrides(
            &engine,
            &reopened,
            GraphicsBackend::Dxmt,
            SyncMode::None,
            HidrawMode::Disabled,
        )
        .unwrap();
    assert_eq!(restored.backend, GraphicsBackend::Dxmt);
    assert_eq!(restored.hidraw, HidrawMode::Disabled);
    assert_eq!(restored.sync_mode, SyncMode::None);
    assert_eq!(
        listing(&system32),
        ["d3d10core.dll", "d3d11.dll", "dxgi.dll"],
        "no stale d3d12.dll"
    );
    assert_eq!(listing(&syswow64), ["d3d11.dll"]);
    assert_eq!(RosettaWine.open_prefix(&prefix_dir).unwrap(), restored);
}

/// From the process-and-prefix review of 8b5bfd9: `reg import` exits 0 for
/// a well-formed file holding a key it cannot import and only prints
/// `reg: Unable to import the registry key '...'`; that line fails the
/// seed, and only the current run's part of the log is read.
#[test]
fn a_reg_import_that_reports_a_key_failure_fails_the_apply() {
    let dir = tempfile::tempdir().unwrap();
    let engine = stub_engine(dir.path());
    let prefix_dir = dir.path().join("prefix");
    let prefix = RosettaWine
        .create_prefix(
            &engine,
            &prefix_dir,
            &spec(dir.path(), GraphicsBackend::Dxmt),
        )
        .unwrap();

    fs::write(engine.root.join("mode-reg"), "complain").unwrap();
    let err = RosettaWine
        .apply_overrides(
            &engine,
            &prefix,
            GraphicsBackend::Vkd3dProton,
            SyncMode::None,
            HidrawMode::Disabled,
        )
        .unwrap_err();
    let Error::WineCommandFailed {
        command,
        status,
        log,
    } = &err
    else {
        panic!("{err:?}")
    };
    assert!(command.ends_with(" reg import seeds.reg"), "{command}");
    assert_eq!(status, "reg: Unable to import the registry key 'HKXX\\Bad'");
    assert_eq!(log, &prefix_dir.join("applejack/reg-import.log"));
    assert!(err.to_string().contains("Unable to import"), "{err}");
    let record = PrefixRecord::read(&prefix_dir).unwrap();
    assert_eq!(record.backend, GraphicsBackend::Vkd3dProton);
    assert_eq!(record.hidraw, HidrawMode::Enabled);
    assert_eq!(record.sync_mode, SyncMode::Esync);

    // The line stays in the log; the next apply reads only its own run.
    fs::remove_file(engine.root.join("mode-reg")).unwrap();
    let applied = RosettaWine
        .apply_overrides(
            &engine,
            &RosettaWine.open_prefix(&prefix_dir).unwrap(),
            GraphicsBackend::Vkd3dProton,
            SyncMode::None,
            HidrawMode::Disabled,
        )
        .unwrap();
    assert!(
        fs::read_to_string(log)
            .unwrap()
            .contains("Unable to import")
    );
    assert_eq!(applied.hidraw, HidrawMode::Disabled);
    assert_eq!(applied.sync_mode, SyncMode::None);
    assert_eq!(RosettaWine.open_prefix(&prefix_dir).unwrap(), applied);
}

/// From the process-and-prefix review of 4719a27: a game extra must not
/// advertise AVX; only the engine, from the recorded probe, may.
#[test]
fn game_extras_cannot_advertise_avx() {
    let dir = tempfile::tempdir().unwrap();
    let engine = stub_engine(dir.path());
    let prefix_dir = dir.path().join("prefix");
    let prefix = RosettaWine
        .create_prefix(
            &engine,
            &prefix_dir,
            &spec(dir.path(), GraphicsBackend::WineD3D),
        )
        .unwrap();
    let logs = dir.path().join("logs");
    let spec = launch_spec(&prefix_dir, &logs, &[("ROSETTA_ADVERTISE_AVX", "1")]);
    let mut session = RosettaWine.launch(&engine, &prefix, &spec).unwrap();
    assert!(session.wait().unwrap().success());
    let seen = env_seen(&engine, "game.exe");
    assert!(!seen.contains("ROSETTA_ADVERTISE_AVX"), "{seen}");
    let log = fs::read_to_string(&session.log_path).unwrap();
    assert!(
        log.contains("dropped ROSETTA_ADVERTISE_AVX from Game tier"),
        "{log}"
    );

    let changes = CpuidFeatures {
        avx: true,
        avx2: true,
        osxsave: true,
        xcr0_ymm: true,
    };
    let no_change = CpuidFeatures {
        avx: false,
        ..changes
    };
    for (advertise, probe, expected) in [
        (true, None, false),
        (
            true,
            Some(CpuidProbe {
                plain: no_change,
                advertised: no_change,
            }),
            false,
        ),
        (
            false,
            Some(CpuidProbe {
                plain: no_change,
                advertised: changes,
            }),
            false,
        ),
        (
            true,
            Some(CpuidProbe {
                plain: no_change,
                advertised: changes,
            }),
            true,
        ),
    ] {
        let mut spec = launch_spec(&prefix_dir, &logs, &[]);
        spec.advertise_avx = advertise;
        spec.cpuid_probe = probe;
        let mut session = RosettaWine.launch(&engine, &prefix, &spec).unwrap();
        assert!(session.wait().unwrap().success());
        assert_eq!(
            env_seen(&engine, "game.exe").contains("ROSETTA_ADVERTISE_AVX=1"),
            expected,
            "advertise_avx {advertise}, probe {probe:?}"
        );
    }
}

#[test]
fn cancel_reaps_a_loader_that_outlives_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let engine = stub_engine(dir.path());
    let prefix_dir = dir.path().join("prefix");
    let prefix = RosettaWine
        .create_prefix(
            &engine,
            &prefix_dir,
            &spec(dir.path(), GraphicsBackend::WineD3D),
        )
        .unwrap();
    fs::write(engine.root.join("mode-game.exe"), "hang").unwrap();
    let spec = launch_spec(&prefix_dir, &dir.path().join("logs"), &[]);
    let mut session = RosettaWine.launch(&engine, &prefix, &spec).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let pid = session.pid.to_string();
    assert!(process_exists(&pid));
    let started = Instant::now();
    session.cancel().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    assert!(
        session.try_wait().unwrap().is_some(),
        "the loader was reaped by cancel"
    );
    assert!(!process_exists(&pid), "no zombie for pid {pid}");
    let calls = calls(&engine);
    assert!(
        calls.contains("wineserver -k ") && calls.contains("wineserver -w "),
        "{calls}"
    );
}
