//! Live engine tests against a real payload. Opt in with
//! `APPLEJACK_LIVE_ENGINE=1`; the payload comes from `APPLEJACK_PAYLOAD_DIR`
//! or `fixtures/payloads/rosetta-wine-11.0` in the repository. Every test
//! kills the prefix's `wineserver` before it returns.
//!
//! `dx11-sample.exe` creates a Metal device and a window, which needs a
//! logged-in GUI session: from a context without display access it exits
//! with `dx11-sample: D3D11CreateDeviceAndSwapChain failed: 0x80004005`.
//! Set `APPLEJACK_LIVE_HEADLESS=1` to have exactly that early exit (the
//! session log carries [`HEADLESS_EXCUSE`]) reported rather than fail the
//! test; process discovery then runs over `cmd /c ping` instead. Any other
//! early exit (a missing executable, a DLL that does not load, a crash)
//! fails the test with or without the flag.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output};
use std::time::{Duration, Instant};

use applejack_engine::rosetta_wine::launch::{MONO_GECKO_OVERRIDE, child_path};
use applejack_engine::rosetta_wine::prefix::reg_error_line;
use applejack_engine::rosetta_wine::{Process, snapshot};
use applejack_engine::{
    BundleToml, Engine, EngineId, Error, GraphicsBackend, HidrawMode, InstalledEngine, LaunchSpec,
    Prefix, PrefixSpec, RosettaWine, Session, SyncMode,
};

fn enabled() -> bool {
    std::env::var_os("APPLEJACK_LIVE_ENGINE").is_some_and(|value| value == "1")
}

fn headless() -> bool {
    std::env::var_os("APPLEJACK_LIVE_HEADLESS").is_some_and(|value| value == "1")
}

/// What `dx11-sample` prints (`dx11-sample: %s failed: 0x%08lx`) when it
/// cannot create a device; the one early exit the headless flag excuses.
const HEADLESS_EXCUSE: &str = "D3D11CreateDeviceAndSwapChain failed";

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
    snapshots: PathBuf,
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
    let tmp_snapshots = tmp.path().join("snapshots");
    let spec = PrefixSpec {
        backend,
        sync_mode: engine.bundle.default_sync_mode().unwrap(),
        hidraw: HidrawMode::Enabled,
        library_dir: Some(tmp.path().join("library")),
        log_dir: logs.clone(),
        boot_timeout: None,
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
        snapshots: tmp_snapshots,
        prefix,
    }
}

/// `<loader> args` in `cwd` with the engine's `PATH`, the prefix, and the
/// Mono/Gecko override, as a plain run outside `launch`.
fn loader_output(live: &Live, args: &[&str], cwd: &Path) -> Output {
    Command::new(live.engine.root.join(&live.engine.bundle.wine.loader))
        .args(args)
        .env_clear()
        .env("PATH", child_path(&live.engine))
        .env("WINEPREFIX", &live.prefix.dir)
        .env("WINEDLLOVERRIDES", MONO_GECKO_OVERRIDE)
        .env("WINEDEBUG", "-all")
        .current_dir(cwd)
        .output()
        .unwrap()
}

fn loader_status(live: &Live, args: &[&str], cwd: &Path) -> ExitStatus {
    loader_output(live, args, cwd).status
}

/// Launches `spec` and polls `Session::processes()` until, at least 2 s
/// after launch (Wine has rewritten argv by then, so the session entry is
/// only reachable through the folded last `cmd` entry), the title `title`
/// and a Windows process other than the loader (pid != `session.pid`, `exe`
/// ending in `.exe`) both carry the session id, or the loader exits first.
/// Returns the session and what was seen, or the loader's early exit status
/// and log.
fn launch_and_discover(
    live: &Live,
    spec: &LaunchSpec,
    title: &str,
) -> Result<(Session, Vec<Process>), (ExitStatus, String)> {
    let mut session = RosettaWine
        .launch(&live.engine, &live.prefix, spec)
        .expect("launch");
    eprintln!(
        "[live] session {} loader pid {} for {}",
        session.id,
        session.pid,
        spec.exe.display()
    );
    let started = Instant::now();
    loop {
        let polled = Instant::now();
        let processes = session.processes();
        let cost = polled.elapsed();
        let has_title = processes.iter().any(|p| p.exe.eq_ignore_ascii_case(title));
        let has_other = processes
            .iter()
            .any(|p| p.pid != session.pid && p.exe.to_ascii_lowercase().ends_with(".exe"));
        if has_title && has_other && started.elapsed() >= Duration::from_secs(2) {
            eprintln!(
                "[live] processes after {:.1}s (one processes() call took {cost:?}): {processes:#?}",
                started.elapsed().as_secs_f64()
            );
            return Ok((session, processes));
        }
        if let Some(status) = session.try_wait().unwrap() {
            let log = fs::read_to_string(&session.log_path).unwrap_or_default();
            let _ = session.cancel();
            return Err((status, log));
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{title} and another Windows process of the session not both found by id within 10 s; saw {processes:?}"
        );
        std::thread::sleep(Duration::from_millis(250));
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

/// Shows where the session entry sits for each found process: as `sysinfo`
/// reports it, the last `cmd` entry for a Wine-rewritten argv.
fn print_argv_tails(processes: &[Process]) {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let pids: Vec<Pid> = processes.iter().map(|p| Pid::from_u32(p.pid)).collect();
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&pids),
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_environ(UpdateKind::Always),
    );
    for pid in pids {
        if let Some(process) = system.process(pid) {
            let in_environ = process
                .environ()
                .iter()
                .any(|e| e.to_string_lossy().starts_with("APPLEJACK_SESSION="));
            eprintln!(
                "[live]   pid {pid}: cmd tail {:?}; APPLEJACK_SESSION in environ: {in_environ}",
                process.cmd().iter().rev().take(2).rev().collect::<Vec<_>>()
            );
        }
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

    // The seed import is `reg import` because it reports a bad file;
    // `regedit /S` exits 0 for the same file.
    let scratch = live.logs.join("scratch");
    fs::create_dir_all(&scratch).unwrap();
    fs::write(
        scratch.join("malformed.reg"),
        "this is not a registry file\n",
    )
    .unwrap();
    let import = loader_status(&live, &["reg", "import", "malformed.reg"], &scratch);
    let regedit = loader_status(&live, &["regedit", "/S", "malformed.reg"], &scratch);
    eprintln!("[live] malformed.reg: reg import {import}; regedit /S {regedit}");
    assert!(
        !import.success(),
        "reg import must fail on a malformed file"
    );
    // A well-formed file with a key under an unknown root: `reg import`
    // exits 0, imports the rest, and prints the line the engine scans for
    // (review of 8b5bfd9), which is why `hidraw_key`'s root is validated
    // with the bundle and the seed log is checked after every import.
    fs::write(
        scratch.join("mixed.reg"),
        "REGEDIT4\n\n[HKEY_CURRENT_USER\\Software\\ApplejackLiveProbe]\n\"ok\"=dword:00000001\n\n[HKXX\\Bad]\n\"x\"=dword:00000001\n",
    )
    .unwrap();
    let mixed = loader_output(&live, &["reg", "import", "mixed.reg"], &scratch);
    let mixed_text = format!(
        "{}{}",
        String::from_utf8_lossy(&mixed.stdout),
        String::from_utf8_lossy(&mixed.stderr)
    );
    let reg_line = reg_error_line(&mixed_text);
    eprintln!(
        "[live] mixed.reg: reg import {}; reg line {reg_line:?}",
        mixed.status
    );
    assert!(
        mixed.status.success(),
        "reg import exits 0 for a mixed file; the engine scans its output for the reason"
    );
    assert!(
        reg_line.is_some_and(|line| line.contains("Unable to import the registry key 'HKXX\\Bad'")),
        "{mixed_text}"
    );

    // Make the liveness check about the next session: stop the server the
    // earlier sessions started (it lingers a few seconds on its own).
    live.server_kill();
    assert!(!applejack_engine::rosetta_wine::process::wineserver_alive(&prefix.dir).unwrap());
    assert!(
        fs::read_to_string(prefix.dir.join("user.reg"))
            .unwrap()
            .contains("ApplejackLiveProbe"),
        "the importable key of the mixed file landed"
    );

    // dx11-sample: a Windows process found by session id, this session's
    // server up, restore refused meanwhile, cancel, server gone, loader
    // reaped, restore allowed.
    let sample = engine
        .root
        .join(&engine.bundle.probes.as_ref().unwrap().dx11_sample);
    let sample_spec = launch_spec(&live, sample, &["--frames", "100000"]);
    let (mut session, found) = match launch_and_discover(&live, &sample_spec, "dx11-sample.exe") {
        Ok(found) => found,
        Err((status, log)) if headless() && log.contains(HEADLESS_EXCUSE) => {
            eprintln!(
                "[live] dx11-sample could not create a device, status {status} (APPLEJACK_LIVE_HEADLESS=1 and the log says `{HEADLESS_EXCUSE}`: reported, not a failure):\n{log}"
            );
            live.server_kill();
            let ping = launch_spec(&live, PathBuf::from("cmd"), &["/c", "ping -n 60 127.0.0.1"]);
            launch_and_discover(&live, &ping, "cmd.exe")
                .unwrap_or_else(|(status, log)| panic!("cmd exited early with {status}:\n{log}"))
        }
        Err((status, log)) => panic!("dx11-sample exited early with {status}:\n{log}"),
    };
    let windows: Vec<&Process> = found
        .iter()
        .filter(|p| p.pid != session.pid && p.exe.to_ascii_lowercase().ends_with(".exe"))
        .collect();
    eprintln!("[live] Windows processes found by session id: {windows:?}");
    let first = windows
        .first()
        .expect("a Windows process other than the loader");
    assert_ne!(first.pid, session.pid);
    print_argv_tails(&found);
    assert!(
        session.wineserver_alive().unwrap(),
        "this session's server is up"
    );
    let snap = snapshot::create(&prefix.dir, &live.snapshots, "test").expect("snapshot");
    let refused =
        snapshot::restore(&prefix.dir, &live.snapshots, "test", &snap.timestamp).unwrap_err();
    eprintln!("[live] restore while the server runs: {refused}");
    assert!(matches!(refused, Error::PrefixUnusable { .. }));
    let refused = RosettaWine
        .apply_overrides(
            engine,
            prefix,
            GraphicsBackend::Vkd3dProton,
            SyncMode::None,
            HidrawMode::Disabled,
        )
        .unwrap_err();
    eprintln!("[live] apply while the server runs: {refused}");
    assert!(matches!(refused, Error::PrefixUnusable { .. }));
    assert!(
        prefix
            .dir
            .join("drive_c/windows/system32/winemetal.dll")
            .is_file(),
        "the DXMT DLLs stayed in place"
    );
    assert!(session.wineserver_alive().unwrap(), "the session survived");
    session.cancel().expect("cancel");
    let alive_after = session.wineserver_alive().unwrap();
    eprintln!("[live] wineserver_alive after cancel: {alive_after}");
    assert!(!alive_after, "wineserver still alive after cancel");
    // `wineserver -k` lets the process close and return, so the status is
    // whatever it chose; the point is that cancel reaped it.
    let status = session
        .try_wait()
        .unwrap()
        .expect("loader reaped by cancel");
    eprintln!("[live] loader after cancel: {status}");
    let log = fs::read_to_string(&session.log_path).unwrap();
    eprintln!("[live] session log:\n{log}");
    let leftovers = session.processes();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    drop(session);
    snapshot::restore(&prefix.dir, &live.snapshots, "test", &snap.timestamp)
        .expect("restore once the server is gone");
    assert_eq!(RosettaWine.open_prefix(&prefix.dir).unwrap(), *prefix);

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
