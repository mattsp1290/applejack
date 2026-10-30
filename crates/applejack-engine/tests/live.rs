//! Live engine tests against a real payload. Opt in with
//! `APPLEJACK_LIVE_ENGINE=1`; the payload comes from `APPLEJACK_PAYLOAD_DIR`
//! or `fixtures/payloads/rosetta-wine-11.0` in the repository. Every test
//! kills the prefix's `wineserver` before it returns.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use applejack_engine::rosetta_wine::launch::child_path;
use applejack_engine::{
    BundleToml, Engine, EngineId, GraphicsBackend, HidrawMode, InstalledEngine, LaunchSpec, Prefix,
    PrefixSpec, RosettaWine, SyncMode,
};

fn enabled() -> bool {
    std::env::var_os("APPLEJACK_LIVE_ENGINE").is_some_and(|value| value == "1")
}

const FIXTURE_PAYLOAD: &str = "fixtures/payloads/rosetta-wine-11.0";

/// `APPLEJACK_PAYLOAD_DIR`, else the fixture payload of this checkout,
/// else (from a linked worktree, where `fixtures/payloads/` is ignored and
/// absent) the fixture payload of the main checkout.
fn payload_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("APPLEJACK_PAYLOAD_DIR") {
        return PathBuf::from(dir);
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut candidates = vec![repo.join(FIXTURE_PAYLOAD)];
    if let Ok(output) = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(&repo)
        .output()
        && output.status.success()
    {
        let common = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        candidates.push(repo.join(common).join("..").join(FIXTURE_PAYLOAD));
    }
    candidates
        .iter()
        .find_map(|candidate| candidate.canonicalize().ok())
        .unwrap_or_else(|| panic!("no payload: set APPLEJACK_PAYLOAD_DIR; tried {candidates:?}"))
}

/// Another unit may be reassembling the payload; wait for its `.tmp`
/// sibling to disappear before running anything from it.
fn wait_for_payload(payload: &Path) {
    let mut tmp = payload.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let started = Instant::now();
    while tmp.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(600),
            "{} still exists after 10 minutes",
            tmp.display()
        );
        std::thread::sleep(Duration::from_secs(5));
    }
}

struct Live {
    engine: InstalledEngine,
    _tmp: tempfile::TempDir,
    logs: PathBuf,
    prefix: Prefix,
}

impl Live {
    fn server_kill(&self) {
        let _ = Command::new(self.engine.root.join(&self.engine.bundle.wine.server))
            .arg("-k")
            .env_clear()
            .env("PATH", child_path(&self.engine))
            .env("WINEPREFIX", &self.prefix.dir)
            .status();
        let _ = Command::new(self.engine.root.join(&self.engine.bundle.wine.server))
            .arg("-w")
            .env_clear()
            .env("PATH", child_path(&self.engine))
            .env("WINEPREFIX", &self.prefix.dir)
            .status();
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.server_kill();
    }
}

fn setup(backend: GraphicsBackend) -> Live {
    let payload = payload_dir();
    wait_for_payload(&payload);
    let bundle = BundleToml::load(&payload).expect("payload BUNDLE.toml validates");
    let tmp = tempfile::tempdir().unwrap();
    let engine = InstalledEngine {
        id: EngineId::RosettaWine,
        version: bundle.version.clone(),
        root: payload,
        overlay_root: tmp.path().join("overlay"),
        bundle,
    };
    let logs = tmp.path().join("logs");
    let spec = PrefixSpec {
        backend,
        sync_mode: engine.bundle.default_sync_mode().unwrap(),
        hidraw: HidrawMode::Enabled,
        library_dir: Some(tmp.path().join("library")),
        log_dir: logs.clone(),
    };
    fs::create_dir_all(tmp.path().join("library")).unwrap();
    let prefix_dir = tmp.path().join("prefixes/test");
    let started = Instant::now();
    let prefix = RosettaWine
        .create_prefix(&engine, &prefix_dir, &spec)
        .expect("prefix creation");
    eprintln!(
        "[live] created prefix {} with backend {backend} in {:.1}s",
        prefix_dir.display(),
        started.elapsed().as_secs_f64()
    );
    Live {
        engine,
        _tmp: tmp,
        logs,
        prefix,
    }
}

fn launch_spec(live: &Live, exe: PathBuf, args: &[&str]) -> LaunchSpec {
    LaunchSpec {
        exe,
        args: args.iter().map(OsString::from).collect(),
        env_extras: vec![(OsString::from("WINEDEBUG"), OsString::from("-all"))],
        cwd: live.prefix.dir.clone(),
        log_dir: live.logs.clone(),
        advertise_avx: false,
        cpuid_probe: None,
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one sequence over one prefix; each prefix creation costs about a minute of wineboot"
)]
fn prefix_hello32_dx11_sample_and_cpuid() {
    if !enabled() {
        eprintln!("[live] skipped: set APPLEJACK_LIVE_ENGINE=1");
        return;
    }
    let live = setup(GraphicsBackend::Dxmt);
    let engine = &live.engine;
    let prefix = &live.prefix;

    // The prefix: wineboot ran, DXMT DLLs are in place, the record is there.
    assert!(
        prefix
            .dir
            .join("drive_c/windows/system32/d3d11.dll")
            .is_file()
    );
    assert!(
        prefix
            .dir
            .join("drive_c/windows/syswow64/d3d11.dll")
            .is_file()
    );
    assert!(prefix.dir.join("system.reg").is_file());
    assert!(
        fs::read_link(prefix.dir.join("dosdevices/s:")).is_ok(),
        "library drive linked"
    );
    assert_eq!(RosettaWine.open_prefix(&prefix.dir).unwrap(), *prefix);
    let system_reg = fs::read_to_string(prefix.dir.join("system.reg")).unwrap();
    assert!(
        system_reg.contains("DisableHidraw"),
        "hidraw seed reached HKLM"
    );
    let user_reg = fs::read_to_string(prefix.dir.join("user.reg")).unwrap();
    assert!(user_reg.contains("ShowCrashDialog") && user_reg.contains("DllOverrides"));

    // `<loader> --version` prints the BUNDLE.toml Wine version.
    let output = Command::new(engine.root.join(&engine.bundle.wine.loader))
        .arg("--version")
        .env_clear()
        .env("PATH", child_path(engine))
        .env("WINEPREFIX", &prefix.dir)
        .output()
        .unwrap();
    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    eprintln!("[live] loader --version: {version}");
    assert!(output.status.success());
    assert!(version.contains(&engine.bundle.wine.version), "{version}");

    // hello32: a 32-bit PE under the new WoW64.
    let hello = engine.root.join("probes/hello32.exe");
    let mut session = RosettaWine
        .launch(engine, prefix, &launch_spec(&live, hello, &[]))
        .expect("launch hello32");
    let status = session.wait().unwrap();
    let log = fs::read_to_string(&session.log_path).unwrap();
    eprintln!(
        "[live] hello32 exited {status}; log {}:\n{log}",
        session.log_path.display()
    );
    assert!(status.success());
    assert!(log.contains("hello32 ok"));

    // dx11-sample: found by session id, cancelled, server gone.
    let sample = engine
        .root
        .join(&engine.bundle.probes.as_ref().unwrap().dx11_sample);
    let mut session = RosettaWine
        .launch(
            engine,
            prefix,
            &launch_spec(&live, sample, &["--frames", "100000"]),
        )
        .expect("launch dx11-sample");
    eprintln!(
        "[live] dx11-sample session {} pid {}",
        session.id, session.pid
    );
    let started = Instant::now();
    let found = loop {
        let processes = session.processes();
        if processes
            .iter()
            .any(|p| p.exe.eq_ignore_ascii_case("dx11-sample.exe"))
        {
            break processes;
        }
        if let Some(status) = session.try_wait().unwrap() {
            let log = fs::read_to_string(&session.log_path).unwrap_or_default();
            panic!("dx11-sample exited early with {status}:\n{log}");
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "dx11-sample not found by session id within 10 s; saw {processes:?}"
        );
        std::thread::sleep(Duration::from_millis(250));
    };
    eprintln!(
        "[live] processes after {:.1}s: {found:?}",
        started.elapsed().as_secs_f64()
    );
    assert!(session.wineserver_alive().unwrap());
    session.cancel().expect("cancel");
    assert!(
        !session.wineserver_alive().unwrap(),
        "wineserver still alive after cancel"
    );
    // `wineserver -k` lets the sample close its window and return, so the
    // status is whatever it chose; the point is that it has exited.
    let status = session.wait().unwrap();
    eprintln!("[live] dx11-sample after cancel: {status}");
    let log = fs::read_to_string(&session.log_path).unwrap();
    eprintln!("[live] dx11-sample log:\n{log}");
    let leftovers = session.processes();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // The CPUID probe, both ways.
    let probe = RosettaWine.probe_cpuid(engine).expect("cpuid probe");
    eprintln!(
        "[live] cpuid plain {:?} advertised {:?} changes_bits {}",
        probe.plain,
        probe.advertised,
        probe.changes_bits()
    );

    // apply_overrides switches to vkd3d and back without touching wineboot.
    let switched = RosettaWine
        .apply_overrides(
            engine,
            prefix,
            GraphicsBackend::Vkd3dProton,
            SyncMode::None,
            HidrawMode::Disabled,
        )
        .expect("apply vkd3d");
    assert_eq!(switched.backend, GraphicsBackend::Vkd3dProton);
    assert!(
        prefix
            .dir
            .join("drive_c/windows/system32/d3d12.dll")
            .is_file()
    );
    assert!(
        !prefix
            .dir
            .join("drive_c/windows/system32/winemetal.dll")
            .is_file()
    );
    let restored = RosettaWine
        .apply_overrides(
            engine,
            &switched,
            GraphicsBackend::Dxmt,
            SyncMode::Esync,
            HidrawMode::Enabled,
        )
        .expect("apply dxmt");
    assert!(
        prefix
            .dir
            .join("drive_c/windows/system32/winemetal.dll")
            .is_file()
    );
    assert!(
        !prefix
            .dir
            .join("drive_c/windows/system32/d3d12.dll")
            .is_file()
    );
    assert_eq!(RosettaWine.open_prefix(&prefix.dir).unwrap(), restored);
    live.server_kill();
    assert!(!applejack_engine::rosetta_wine::process::wineserver_alive(&prefix.dir).unwrap());
}
