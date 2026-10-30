//! The `rosetta-wine` engine, offline: prefix builder steps and environment,
//! registry seeds, snapshots, the `D3DMetal` import, process discovery, and
//! the refusals. Nothing here runs Wine.

mod common;

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use applejack_engine::rosetta_wine::launch::{
    HOST_PATH, MONO_GECKO_OVERRIDE, PeMachine, check_32bit, child_path, dll_overrides_value,
    engine_rows, pe_machine, resolve_exe,
};
use applejack_engine::rosetta_wine::prefix::{
    PREFIX_STEPS, PrefixRecord, PrefixStep, STATE_DIR, backend_available, override_stems,
    render_registry_seeds,
};
use applejack_engine::rosetta_wine::process::{
    Process, ProcessRecord, SESSION_VARIABLE, executable_name, match_record,
};
use applejack_engine::rosetta_wine::{CpuidFeatures, CpuidProbe, cpuid, d3dmetal, snapshot};
use applejack_engine::{
    Engine, EngineId, Error, GraphicsBackend, HidrawMode, InstalledEngine, LaunchSpec, Prefix,
    PrefixSpec, RosettaWine, SyncMode,
};
use common::{BUNDLE_TOML, fake_installed, layout, rosetta, version};

fn installed(dir: &Path) -> InstalledEngine {
    let layout = layout(dir);
    fake_installed(&layout, "10.0.0");
    InstalledEngine::open(&layout, rosetta(), &version("10.0.0")).unwrap()
}

/// The fixture bundle with `wow64 = "none"` and no i386 lists.
fn installed_64_only(dir: &Path) -> InstalledEngine {
    let layout = layout(dir);
    let root = layout.engine_dir(rosetta(), &version("10.0.0"));
    let text = BUNDLE_TOML
        .replace("wow64 = \"new\"", "wow64 = \"none\"")
        .replace("i386 = [\"dxmt/i386/d3d11.dll\"]\n", "");
    common::write_bundle_tree(&root, &text);
    InstalledEngine::open(&layout, rosetta(), &version("10.0.0")).unwrap()
}

fn value<'a>(env: &'a [(OsString, OsString)], name: &str) -> Option<&'a str> {
    env.iter()
        .find(|(key, _)| key == name)
        .and_then(|(_, value)| value.to_str())
}

fn prefix_for(engine: &InstalledEngine, dir: &Path) -> Prefix {
    Prefix {
        dir: dir.to_path_buf(),
        engine: EngineId::RosettaWine,
        engine_version: engine.version.clone(),
        backend: GraphicsBackend::Dxmt,
        sync_mode: SyncMode::Esync,
        hidraw: HidrawMode::Enabled,
    }
}

#[test]
fn prefix_builder_steps_are_the_documented_four_in_order() {
    let names: Vec<&str> = PREFIX_STEPS.iter().map(|step| step.name()).collect();
    assert_eq!(
        names,
        [
            "wineboot",
            "dll-overrides",
            "registry-seeds",
            "library-drive"
        ]
    );
    assert_eq!(PREFIX_STEPS[0], PrefixStep::WineBoot);
    assert_eq!(PREFIX_STEPS[3], PrefixStep::LibraryDrive);
}

#[test]
fn engine_rows_carry_the_mono_gecko_override_and_backend_dlls() {
    let dir = tempfile::tempdir().unwrap();
    let engine = installed(dir.path());
    let prefix = dir.path().join("prefix");
    for (backend, expected) in [
        (
            GraphicsBackend::Dxmt,
            format!("{MONO_GECKO_OVERRIDE};d3d10core,d3d11,dxgi=n,b"),
        ),
        (
            GraphicsBackend::Vkd3dProton,
            format!("{MONO_GECKO_OVERRIDE};d3d12,d3d12core=n,b"),
        ),
        (GraphicsBackend::WineD3D, MONO_GECKO_OVERRIDE.to_owned()),
        (GraphicsBackend::D3DMetal, MONO_GECKO_OVERRIDE.to_owned()),
    ] {
        assert_eq!(dll_overrides_value(&engine.bundle, backend), expected);
        for (mode, variable) in [
            (SyncMode::Msync, Some("WINEMSYNC")),
            (SyncMode::Esync, Some("WINEESYNC")),
            (SyncMode::None, None),
        ] {
            let rows = engine_rows(&engine, &prefix, "s-1", backend, mode, false);
            assert_eq!(value(&rows, "WINEDLLOVERRIDES"), Some(expected.as_str()));
            assert!(
                value(&rows, "WINEDLLOVERRIDES")
                    .unwrap()
                    .starts_with("mscoree=;mshtml=")
            );
            assert_eq!(value(&rows, SESSION_VARIABLE), Some("s-1"));
            assert_eq!(value(&rows, "WINEPREFIX"), prefix.to_str());
            assert_eq!(
                value(&rows, "WINEMSYNC"),
                (variable == Some("WINEMSYNC")).then_some("1")
            );
            assert_eq!(
                value(&rows, "WINEESYNC"),
                (variable == Some("WINEESYNC")).then_some("1")
            );
            assert_eq!(value(&rows, "ROSETTA_ADVERTISE_AVX"), None);
            assert!(
                rows.iter()
                    .all(|(name, _)| !name.to_string_lossy().starts_with("DYLD_"))
            );
        }
    }
    let rows = engine_rows(
        &engine,
        &prefix,
        "s-1",
        GraphicsBackend::Dxmt,
        SyncMode::None,
        true,
    );
    assert_eq!(value(&rows, "ROSETTA_ADVERTISE_AVX"), Some("1"));
}

#[test]
fn child_path_is_the_loader_directory_then_system_paths() {
    let dir = tempfile::tempdir().unwrap();
    let engine = installed(dir.path());
    let expected = format!("{}:{HOST_PATH}", engine.root.join("wine/bin").display());
    assert_eq!(child_path(&engine), OsString::from(expected));
    assert!(!HOST_PATH.contains("/usr/local") && !HOST_PATH.contains("/opt/homebrew"));
}

#[test]
fn vkd3d_backend_sets_the_icd_only_when_the_bundle_has_one() {
    let dir = tempfile::tempdir().unwrap();
    let engine = installed(dir.path());
    let prefix = dir.path().join("prefix");
    let rows = engine_rows(
        &engine,
        &prefix,
        "s",
        GraphicsBackend::Vkd3dProton,
        SyncMode::None,
        false,
    );
    assert_eq!(value(&rows, "VK_ICD_FILENAMES"), None);
    let icd = engine.root.join("vulkan/MoltenVK_icd.json");
    fs::create_dir_all(icd.parent().unwrap()).unwrap();
    fs::write(&icd, "{}").unwrap();
    let rows = engine_rows(
        &engine,
        &prefix,
        "s",
        GraphicsBackend::Vkd3dProton,
        SyncMode::None,
        false,
    );
    assert_eq!(value(&rows, "VK_ICD_FILENAMES"), icd.to_str());
    let rows = engine_rows(
        &engine,
        &prefix,
        "s",
        GraphicsBackend::Dxmt,
        SyncMode::None,
        false,
    );
    assert_eq!(value(&rows, "VK_ICD_FILENAMES"), None);
}

#[test]
fn registry_seeds_render_crash_dialog_overrides_and_hidraw() {
    let dir = tempfile::tempdir().unwrap();
    let engine = installed(dir.path());
    assert_eq!(
        override_stems(&engine.bundle, GraphicsBackend::Dxmt),
        ["d3d10core", "d3d11", "dxgi"]
    );
    let text = render_registry_seeds(&engine.bundle, GraphicsBackend::Dxmt, HidrawMode::Disabled);
    assert_eq!(
        text,
        "REGEDIT4\n\n\
         [HKEY_CURRENT_USER\\Software\\Wine\\WineDbg]\n\"ShowCrashDialog\"=dword:00000000\n\n\
         [HKEY_CURRENT_USER\\Software\\Wine\\DllOverrides]\n\
         \"d3d10core\"=\"native,builtin\"\n\"d3d11\"=\"native,builtin\"\n\"d3d12\"=-\n\"d3d12core\"=-\n\"dxgi\"=\"native,builtin\"\n\n\
         [HKEY_LOCAL_MACHINE\\System\\CurrentControlSet\\Services\\winebus\\Parameters]\n\"DisableHidraw\"=dword:00000001\n"
    );
    let text = render_registry_seeds(
        &engine.bundle,
        GraphicsBackend::Vkd3dProton,
        HidrawMode::Enabled,
    );
    assert!(text.contains("\"d3d12\"=\"native,builtin\"\n"));
    assert!(text.contains("\"d3d11\"=-\n"));
    assert!(text.ends_with("\"DisableHidraw\"=dword:00000000\n"));
    let text = render_registry_seeds(
        &engine.bundle,
        GraphicsBackend::WineD3D,
        HidrawMode::Enabled,
    );
    assert!(!text.contains("native,builtin"));
}

#[test]
fn prefix_record_round_trips_and_open_prefix_reads_it() {
    let dir = tempfile::tempdir().unwrap();
    let prefix_dir = dir.path().join("prefix");
    fs::create_dir_all(&prefix_dir).unwrap();
    let record = PrefixRecord {
        engine: EngineId::RosettaWine,
        engine_version: version("10.0.0"),
        backend: GraphicsBackend::Vkd3dProton,
        sync_mode: SyncMode::None,
        hidraw: HidrawMode::Disabled,
        copied_dlls: vec![PathBuf::from("drive_c/windows/system32/d3d12.dll")],
    };
    record.write(&prefix_dir).unwrap();
    assert_eq!(PrefixRecord::read(&prefix_dir).unwrap(), record);
    let prefix = RosettaWine.open_prefix(&prefix_dir).unwrap();
    assert_eq!(prefix, record.to_prefix(&prefix_dir));
    assert!(matches!(
        RosettaWine.open_prefix(dir.path()).unwrap_err(),
        Error::InvalidPrefix { .. }
    ));
    fs::write(
        prefix_dir.join(STATE_DIR).join("prefix.toml"),
        "engine = \"native-arm64\"\nengine_version = \"1\"\nbackend = \"dxmt\"\nsync_mode = \"none\"\nhidraw = \"enabled\"\n",
    )
    .unwrap();
    assert!(matches!(
        RosettaWine.open_prefix(&prefix_dir).unwrap_err(),
        Error::InvalidPrefix { reason, .. } if reason.contains("native-arm64")
    ));
}

#[test]
fn d3dmetal_backend_is_refused_without_a_verified_overlay() {
    let dir = tempfile::tempdir().unwrap();
    let engine = installed(dir.path());
    let refused = backend_available(&engine, GraphicsBackend::D3DMetal).unwrap_err();
    assert!(matches!(
        &refused,
        Error::BackendUnavailable { backend: GraphicsBackend::D3DMetal, reason } if reason.contains("no verified D3DMetal overlay")
    ));
    let spec = PrefixSpec {
        backend: GraphicsBackend::D3DMetal,
        sync_mode: SyncMode::None,
        hidraw: HidrawMode::Enabled,
        library_dir: None,
        log_dir: dir.path().join("logs"),
    };
    let prefix_dir = dir.path().join("prefix");
    assert!(matches!(
        RosettaWine
            .create_prefix(&engine, &prefix_dir, &spec)
            .unwrap_err(),
        Error::BackendUnavailable { .. }
    ));
    assert!(!prefix_dir.exists(), "refused before touching the prefix");
    let prefix = prefix_for(&engine, &prefix_dir);
    assert!(matches!(
        RosettaWine
            .apply_overrides(
                &engine,
                &prefix,
                GraphicsBackend::D3DMetal,
                SyncMode::None,
                HidrawMode::Enabled
            )
            .unwrap_err(),
        Error::BackendUnavailable { .. }
    ));
    // The overlay payload makes the backend available; nothing else does.
    let overlay = d3dmetal::overlay_dir(&engine).unwrap();
    fs::create_dir_all(overlay.join(d3dmetal::FRAMEWORK)).unwrap();
    assert!(backend_available(&engine, GraphicsBackend::D3DMetal).is_err());
    fs::write(overlay.join(d3dmetal::DYLIB), b"x").unwrap();
    backend_available(&engine, GraphicsBackend::D3DMetal).unwrap();
    for backend in [
        GraphicsBackend::Dxmt,
        GraphicsBackend::Vkd3dProton,
        GraphicsBackend::WineD3D,
    ] {
        backend_available(&engine, backend).unwrap();
    }
}

#[test]
fn prefix_creation_refuses_a_relative_or_occupied_directory() {
    let dir = tempfile::tempdir().unwrap();
    let engine = installed(dir.path());
    let spec = PrefixSpec {
        backend: GraphicsBackend::WineD3D,
        sync_mode: SyncMode::None,
        hidraw: HidrawMode::Enabled,
        library_dir: None,
        log_dir: dir.path().join("logs"),
    };
    assert!(matches!(
        RosettaWine.create_prefix(&engine, Path::new("relative/prefix"), &spec).unwrap_err(),
        Error::PrefixUnusable { reason, .. } if reason.contains("absolute")
    ));
    let occupied = dir.path().join("occupied");
    fs::create_dir_all(&occupied).unwrap();
    fs::write(occupied.join("system.reg"), "").unwrap();
    assert!(matches!(
        RosettaWine.create_prefix(&engine, &occupied, &spec).unwrap_err(),
        Error::PrefixUnusable { reason, .. } if reason.contains("not empty")
    ));
    assert!(occupied.join("system.reg").exists());
}

fn write_pe(path: &Path, machine: u16) {
    let mut bytes = vec![0u8; 0x80 + 24];
    bytes[..2].copy_from_slice(b"MZ");
    bytes[60..64].copy_from_slice(&0x80u32.to_le_bytes());
    bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
    bytes[0x84..0x86].copy_from_slice(&machine.to_le_bytes());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

#[test]
fn pe_machine_reads_the_header_and_resolves_windows_paths() {
    let dir = tempfile::tempdir().unwrap();
    let i386 = dir.path().join("hello32.exe");
    let amd64 = dir.path().join("sample.exe");
    write_pe(&i386, 0x014c);
    write_pe(&amd64, 0x8664);
    fs::write(dir.path().join("text.exe"), "not a PE").unwrap();
    assert_eq!(pe_machine(&i386).unwrap(), Some(PeMachine::I386));
    assert_eq!(pe_machine(&amd64).unwrap(), Some(PeMachine::Amd64));
    assert_eq!(pe_machine(&dir.path().join("text.exe")).unwrap(), None);
    let prefix = dir.path().join("prefix");
    assert_eq!(resolve_exe(&prefix, dir.path(), &amd64), amd64);
    assert_eq!(
        resolve_exe(&prefix, dir.path(), Path::new("C:\\Games\\a\\b.exe")),
        prefix.join("dosdevices/c:/Games/a/b.exe")
    );
    assert_eq!(
        resolve_exe(&prefix, dir.path(), Path::new("s:/lib/x.exe")),
        prefix.join("dosdevices/s:/lib/x.exe")
    );
    assert_eq!(
        resolve_exe(&prefix, dir.path(), Path::new("bin\\run.exe")),
        dir.path().join("bin/run.exe")
    );
}

#[test]
fn thirty_two_bit_title_is_refused_on_a_64_bit_only_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let engine = installed_64_only(dir.path());
    assert!(!engine.bundle.wine.wow64.supports_32bit());
    let prefix_dir = dir.path().join("prefix");
    let i386 = dir.path().join("hello32.exe");
    let amd64 = dir.path().join("sample.exe");
    write_pe(&i386, 0x014c);
    write_pe(&amd64, 0x8664);
    let refused = check_32bit(&engine, &prefix_dir, dir.path(), &i386).unwrap_err();
    assert!(matches!(&refused, Error::ThirtyTwoBitUnsupported { exe, .. } if *exe == i386));
    assert!(refused.to_string().contains("wow64 = \"none\""));
    check_32bit(&engine, &prefix_dir, dir.path(), &amd64).unwrap();
    check_32bit(
        &engine,
        &prefix_dir,
        dir.path(),
        Path::new("C:\\missing.exe"),
    )
    .unwrap();

    // `launch` refuses before spawning anything.
    let spec = LaunchSpec {
        exe: i386.clone(),
        args: Vec::new(),
        env_extras: Vec::new(),
        cwd: dir.path().to_path_buf(),
        log_dir: dir.path().join("logs"),
        advertise_avx: false,
        cpuid_probe: None,
    };
    let prefix = prefix_for(&engine, &prefix_dir);
    assert!(matches!(
        RosettaWine.launch(&engine, &prefix, &spec).unwrap_err(),
        Error::ThirtyTwoBitUnsupported { .. }
    ));
    assert!(!dir.path().join("logs").exists());

    // The wow64 = "new" fixture accepts the same file.
    let other = tempfile::tempdir().unwrap();
    let engine = installed(other.path());
    check_32bit(&engine, &prefix_dir, dir.path(), &i386).unwrap();
}

#[test]
fn launch_refuses_a_prefix_of_another_engine() {
    let dir = tempfile::tempdir().unwrap();
    let engine = installed(dir.path());
    let mut prefix = prefix_for(&engine, &dir.path().join("prefix"));
    prefix.engine = EngineId::NativeArm64;
    let spec = LaunchSpec {
        exe: PathBuf::from("C:\\x.exe"),
        args: Vec::new(),
        env_extras: Vec::new(),
        cwd: dir.path().to_path_buf(),
        log_dir: dir.path().join("logs"),
        advertise_avx: false,
        cpuid_probe: None,
    };
    assert!(matches!(
        RosettaWine.launch(&engine, &prefix, &spec).unwrap_err(),
        Error::InvalidPrefix { reason, .. } if reason.contains("native-arm64")
    ));
}

fn os(items: &[&str]) -> Vec<OsString> {
    items.iter().map(OsString::from).collect()
}

/// A record as `sysinfo` reports it from `KERN_PROCARGS2` for the
/// dx11-sample under Wine: the exec path is the loader, argv was rewritten
/// by Wine to start with the Windows executable, and the environment is
/// the allowlisted set.
fn captured_record<'a>(cmd: &'a [OsString], environ: &'a [OsString]) -> ProcessRecord<'a> {
    ProcessRecord {
        pid: 4242,
        uid: Some(501),
        exe: Some(Path::new("/data/engines/rosetta-wine/11.0-1/wine/bin/wine")),
        cmd,
        environ,
    }
}

#[test]
fn process_discovery_matches_uid_and_exact_session_id() {
    let cmd = os(&[
        "/data/engines/rosetta-wine/11.0-1/probes/dx11-sample.exe",
        "--frames",
        "60",
    ]);
    let environ = os(&[
        "PATH=/data/engines/rosetta-wine/11.0-1/wine/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        "HOME=/Users/me",
        "WINEPREFIX=/data/prefixes/test",
        "WINEDLLOVERRIDES=mscoree=;mshtml=;d3d10core,d3d11,dxgi,winemetal=n,b",
        "WINEESYNC=1",
        "APPLEJACK_SESSION=launch-18c2a1f0b3e4d5c6-1a2b-0",
    ]);
    let record = captured_record(&cmd, &environ);
    let id = "launch-18c2a1f0b3e4d5c6-1a2b-0";
    assert_eq!(
        match_record(&record, id, 501),
        Some(Process {
            pid: 4242,
            exe: "dx11-sample.exe".to_owned(),
            session_id: id.to_owned(),
        })
    );
    assert_eq!(match_record(&record, id, 502), None, "another uid");
    assert_eq!(
        match_record(&record, "launch-18c2a1f0b3e4d5c6-1a2b", 501),
        None,
        "a prefix of the id"
    );
    assert_eq!(
        match_record(&record, "launch-18c2a1f0b3e4d5c6-1a2b-01", 501),
        None,
        "a longer id"
    );
    let mut no_uid = record;
    no_uid.uid = None;
    assert_eq!(match_record(&no_uid, id, 501), None);
    let unrelated = os(&["APPLEJACK_SESSION_OLD=launch-18c2a1f0b3e4d5c6-1a2b-0"]);
    assert_eq!(
        match_record(&captured_record(&cmd, &unrelated), id, 501),
        None
    );
}

#[test]
fn executable_name_prefers_the_last_exe_in_argv() {
    assert_eq!(
        executable_name(
            &os(&["C:\\Program Files\\Steam\\steam.exe", "-applaunch", "10"]),
            None
        ),
        "steam.exe"
    );
    assert_eq!(
        executable_name(&os(&["steamwebhelper.exe", "--type=renderer"]), None),
        "steamwebhelper.exe"
    );
    assert_eq!(
        executable_name(&os(&["/payload/wine/bin/wine", "/games/Game.EXE"]), None),
        "Game.EXE"
    );
    assert_eq!(
        executable_name(&os(&["/payload/wine/bin/wineserver"]), None),
        "wineserver"
    );
    assert_eq!(
        executable_name(&[], Some(Path::new("/payload/wine/bin/wine"))),
        "wine"
    );
    assert_eq!(executable_name(&[], None), "");
}

#[test]
fn snapshots_create_restore_reset_and_prune() {
    let dir = tempfile::tempdir().unwrap();
    let prefix = dir.path().join("prefixes/steam");
    let root = dir.path().join("prefixes/.snapshots");
    let user_file = prefix.join("drive_c/users/me/save.dat");
    fs::create_dir_all(user_file.parent().unwrap()).unwrap();
    fs::create_dir_all(prefix.join("dosdevices")).unwrap();
    fs::write(&user_file, "v1").unwrap();
    fs::write(prefix.join("system.reg"), "reg-v1").unwrap();
    std::os::unix::fs::symlink("../drive_c", prefix.join("dosdevices/c:")).unwrap();
    std::os::unix::fs::symlink("/", prefix.join("dosdevices/z:")).unwrap();

    let first = snapshot::create(&prefix, &root, "steam").unwrap();
    assert_eq!(first.name, "steam");
    assert!(first.dir.starts_with(root.join("steam")));
    assert_eq!(
        fs::read_to_string(first.dir.join("drive_c/users/me/save.dat")).unwrap(),
        "v1"
    );
    assert!(
        fs::symlink_metadata(first.dir.join("dosdevices/z:"))
            .unwrap()
            .is_symlink(),
        "symlinks are copied as links, not followed"
    );
    assert_eq!(
        fs::read_link(first.dir.join("dosdevices/c:")).unwrap(),
        PathBuf::from("../drive_c")
    );

    fs::write(&user_file, "v2").unwrap();
    fs::write(prefix.join("system.reg"), "reg-v2").unwrap();
    let second = snapshot::create(&prefix, &root, "steam").unwrap();
    assert_ne!(
        first.timestamp, second.timestamp,
        "same-second snapshots get distinct names"
    );
    assert_eq!(
        snapshot::list(&root, "steam").unwrap(),
        vec![first.clone(), second.clone()]
    );
    assert_eq!(snapshot::list(&root, "other").unwrap(), Vec::new());

    let safety = snapshot::restore(&prefix, &root, "steam", &first.timestamp).unwrap();
    assert_eq!(fs::read_to_string(&user_file).unwrap(), "v1");
    assert_eq!(
        fs::read_to_string(prefix.join("system.reg")).unwrap(),
        "reg-v1"
    );
    assert_eq!(
        fs::read_to_string(safety.dir.join("system.reg")).unwrap(),
        "reg-v2"
    );
    assert!(!dir.path().join("prefixes/steam.restoring").exists());
    assert!(!dir.path().join("prefixes/steam.replaced").exists());
    assert!(matches!(
        snapshot::restore(&prefix, &root, "steam", "19700101T000000Z").unwrap_err(),
        Error::SnapshotNotFound { .. }
    ));

    let mut recreated = 0;
    let safety = snapshot::reset(&prefix, &root, "steam", true, &mut |dir| {
        recreated += 1;
        fs::create_dir_all(dir.join("drive_c/users/me")).unwrap();
        fs::write(dir.join("system.reg"), "fresh").unwrap();
        fs::write(dir.join("drive_c/users/me/save.dat"), "fresh").unwrap();
        Ok(())
    })
    .unwrap();
    assert_eq!(recreated, 1);
    assert_eq!(
        fs::read_to_string(prefix.join("system.reg")).unwrap(),
        "fresh"
    );
    assert_eq!(
        fs::read_to_string(&user_file).unwrap(),
        "v1",
        "user data copied back"
    );
    assert_eq!(
        fs::read_to_string(safety.dir.join("system.reg")).unwrap(),
        "reg-v1"
    );
    snapshot::reset(&prefix, &root, "steam", false, &mut |dir| {
        fs::create_dir_all(dir.join("drive_c/users/me")).unwrap();
        fs::write(dir.join("drive_c/users/me/save.dat"), "wiped").unwrap();
        Ok(())
    })
    .unwrap();
    assert_eq!(fs::read_to_string(&user_file).unwrap(), "wiped");

    let all = snapshot::list(&root, "steam").unwrap();
    assert_eq!(all.len(), 5);
    let removed = snapshot::prune(&root, "steam", snapshot::DEFAULT_KEEP).unwrap();
    assert_eq!(removed, all[..2].to_vec());
    let kept = snapshot::list(&root, "steam").unwrap();
    assert_eq!(kept, all[2..].to_vec());
    assert!(kept.iter().all(|snapshot| snapshot.dir.is_dir()));
    assert!(removed.iter().all(|snapshot| !snapshot.dir.exists()));
    assert_eq!(snapshot::prune(&root, "steam", 10).unwrap(), Vec::new());
    for bad in ["", ".", "..", ".hidden", "a/b"] {
        assert!(matches!(
            snapshot::create(&prefix, &root, bad).unwrap_err(),
            Error::InvalidSnapshotName { .. }
        ));
    }
}

/// A GPTK volume as the import expects it.
fn gptk_volume(dir: &Path) -> PathBuf {
    let volume = dir.join("Game Porting Toolkit");
    let payload = volume.join(d3dmetal::PAYLOAD_DIR);
    fs::create_dir_all(payload.join(d3dmetal::FRAMEWORK).join("Versions/A")).unwrap();
    fs::write(
        payload
            .join(d3dmetal::FRAMEWORK)
            .join("Versions/A/D3DMetal"),
        "framework",
    )
    .unwrap();
    std::os::unix::fs::symlink(
        "A",
        payload.join(d3dmetal::FRAMEWORK).join("Versions/Current"),
    )
    .unwrap();
    fs::write(payload.join(d3dmetal::DYLIB), "dylib").unwrap();
    fs::write(volume.join(d3dmetal::LICENSE), "license").unwrap();
    fs::write(
        volume.join(d3dmetal::README),
        "# Game Porting Toolkit 3.0\n\nSee License.pdf. Payload under redist/lib/external/.\n",
    )
    .unwrap();
    volume
}

#[test]
fn d3dmetal_import_copies_the_payload_into_the_overlay_once_the_license_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let engine = installed(dir.path());
    let volume = gptk_volume(dir.path());
    let refused = d3dmetal::import(&engine, &volume, false).unwrap_err();
    assert!(matches!(
        &refused,
        Error::LicenseNotAccepted { acknowledgement } if acknowledgement.contains(&volume.join("License.pdf").display().to_string())
    ));
    assert!(refused.to_string().contains("--accept-apple-license"));
    assert!(!d3dmetal::overlay_present(&engine));

    let overlay = d3dmetal::import(&engine, &volume, true).unwrap();
    assert_eq!(
        overlay.overlay_dir,
        engine.overlay_root.join("wine/lib/external")
    );
    assert_eq!(overlay.imported_from, volume);
    assert_eq!(overlay.gptk_version, "3.0");
    assert_eq!(overlay.accepted_at.len(), "2026-10-30T17:15:00Z".len());
    assert!(overlay.accepted_at.ends_with('Z'));
    assert!(d3dmetal::overlay_present(&engine));
    assert_eq!(
        fs::read_to_string(
            overlay
                .overlay_dir
                .join("D3DMetal.framework/Versions/A/D3DMetal")
        )
        .unwrap(),
        "framework"
    );
    assert!(
        fs::symlink_metadata(
            overlay
                .overlay_dir
                .join("D3DMetal.framework/Versions/Current")
        )
        .unwrap()
        .is_symlink()
    );
    assert_eq!(
        fs::read_to_string(overlay.overlay_dir.join("libd3dshared.dylib")).unwrap(),
        "dylib"
    );
    backend_available(&engine, GraphicsBackend::D3DMetal).unwrap();

    d3dmetal::remove(&engine).unwrap();
    assert!(!d3dmetal::overlay_present(&engine));
    assert!(!overlay.overlay_dir.exists());
    d3dmetal::remove(&engine).unwrap();
}

#[test]
fn d3dmetal_import_names_the_missing_readme_or_payload() {
    let dir = tempfile::tempdir().unwrap();
    let engine = installed(dir.path());
    let volume = gptk_volume(dir.path());
    fs::remove_file(volume.join(d3dmetal::README)).unwrap();
    let error = d3dmetal::import(&engine, &volume, true).unwrap_err();
    assert!(
        matches!(&error, Error::D3DMetalSource { path, .. } if *path == volume.join("README.md"))
    );
    assert!(
        error
            .to_string()
            .contains(&volume.join("README.md").display().to_string())
    );

    let volume = gptk_volume(&dir.path().join("second"));
    fs::remove_file(volume.join(d3dmetal::PAYLOAD_DIR).join(d3dmetal::DYLIB)).unwrap();
    let error = d3dmetal::import(&engine, &volume, true).unwrap_err();
    assert!(matches!(
        &error,
        Error::D3DMetalSource { path, reason }
            if *path == volume.join("redist/lib/external/libd3dshared.dylib") && reason.contains("README.md")
    ));
    assert!(!d3dmetal::overlay_present(&engine));

    // A bundle without `[d3dmetal]` cannot take an import at all.
    let other = tempfile::tempdir().unwrap();
    let layout = layout(other.path());
    let root = layout.engine_dir(rosetta(), &version("10.0.0"));
    let text = BUNDLE_TOML.replace(
        "[overrides.d3dmetal]\nx86_64 = []\n\n[d3dmetal]\noverlay_dir = \"wine/lib/external\"\n",
        "",
    );
    assert_ne!(text, BUNDLE_TOML);
    common::write_bundle_tree(&root, &text);
    let engine = InstalledEngine::open(&layout, rosetta(), &version("10.0.0")).unwrap();
    assert!(matches!(
        d3dmetal::import(&engine, &gptk_volume(other.path()), true).unwrap_err(),
        Error::D3DMetalUnsupported { .. }
    ));
    assert_eq!(d3dmetal::gptk_version("nothing here"), None);
    assert_eq!(
        d3dmetal::gptk_version("Apple Game Porting Toolkit v2.1 beta"),
        None,
        "a token that does not start with a digit is not a version"
    );
    assert_eq!(
        d3dmetal::gptk_version("game porting toolkit 2.1 (beta)"),
        Some("2.1".to_owned())
    );
}

#[test]
fn cpuid_probe_parses_the_json_line_and_reports_changes() {
    let probe = Path::new("probes/cpuid");
    let plain = cpuid::parse(
        probe,
        "\n{\"avx\":false,\"avx2\":false,\"osxsave\":true,\"xcr0_ymm\":true}\n",
    )
    .unwrap();
    assert_eq!(
        plain,
        CpuidFeatures {
            avx: false,
            avx2: false,
            osxsave: true,
            xcr0_ymm: true
        }
    );
    let advertised = cpuid::parse(
        probe,
        "{\"avx\":true,\"avx2\":true,\"osxsave\":true,\"xcr0_ymm\":true}",
    )
    .unwrap();
    assert!(CpuidProbe { plain, advertised }.changes_bits());
    assert!(
        !CpuidProbe {
            plain,
            advertised: plain
        }
        .changes_bits()
    );
    assert!(matches!(
        cpuid::parse(probe, "cpuid: built for a non-x86-64 target").unwrap_err(),
        Error::ProbeOutput { .. }
    ));
    assert!(matches!(
        cpuid::parse(probe, "{\"avx\":true}").unwrap_err(),
        Error::ProbeOutput { .. }
    ));
}

#[test]
fn doctor_reports_rosetta_bundle_and_version_facts() {
    let dir = tempfile::tempdir().unwrap();
    let engine = installed(dir.path());
    let host = |rosetta: Option<bool>, macos: &str| applejack_engine::Host {
        macos_version: macos.to_owned(),
        arch: applejack_engine::Arch::Arm64,
        rosetta_present: rosetta,
    };
    let report = RosettaWine.doctor(&host(Some(true), "26.6.2"), Some(&engine));
    assert!(report.runnable, "{report:?}");
    assert!(report.findings.iter().any(|f| {
        f.message
            .contains("backends: dxmt, wined3d, vkd3d-proton, d3dmetal")
    }));
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.message == "D3DMetal overlay not imported")
    );
    let report = RosettaWine.doctor(&host(Some(false), "26.6.2"), Some(&engine));
    assert!(!report.runnable);
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.message.contains("softwareupdate --install-rosetta"))
    );
    let report = RosettaWine.doctor(&host(Some(true), "13.6"), Some(&engine));
    assert!(!report.runnable);
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.message.contains("older than the bundle's minimum 14.0"))
    );
    let report = RosettaWine.doctor(&host(Some(true), "14.5"), Some(&engine));
    assert!(report.runnable);
    assert!(report.findings.iter().any(|f| f.message.contains("DXMT")));
    assert!(!RosettaWine.doctor(&host(None, "26.6.2"), None).runnable);
}
