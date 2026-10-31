//! The engine half end to end through the binary, against a real payload:
//! install, list, doctor, a prefix, a 32-bit console probe, the CPUID probe,
//! snapshots, reset, a double reset and an install under a dead reader over
//! `serve --stdio`, and the same install answering while it runs. Opt in with
//! `APPLEJACK_LIVE_ENGINE=1`. The payload comes from `APPLEJACK_PAYLOAD_DIR`
//! or `fixtures/payloads/rosetta-wine-11.0` in this checkout or the main
//! one. Every path lives in a throwaway data directory. No GUI session is
//! needed: `dx11-sample` is left to `crates/applejack-engine/tests/live.rs`.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

const FIXTURE_PAYLOAD: &str = "fixtures/payloads/rosetta-wine-11.0";

fn enabled() -> bool {
    std::env::var_os("APPLEJACK_LIVE_ENGINE").is_some_and(|value| value == "1")
}

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
        .into_iter()
        .find(|dir| dir.join("BUNDLE.toml").is_file())
        .expect("set APPLEJACK_PAYLOAD_DIR to a payload directory")
}

fn applejack(data: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_applejack"));
    command
        .env("APPLEJACK_DATA_DIR", data)
        .env("APPLEJACK_ALLOW_UNSIGNED_ENGINE", "1");
    command
}

fn run(data: &Path, args: &[&str]) -> Output {
    let output = applejack(data).args(args).output().expect("runs");
    eprintln!(
        "[live] applejack {}: {}\n{}{}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn ok(data: &Path, args: &[&str]) -> String {
    let output = run(data, args);
    assert!(output.status.success(), "applejack {}", args.join(" "));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stop_servers(data: &Path) {
    let server = data.join("engines/rosetta-wine");
    let Ok(versions) = std::fs::read_dir(&server) else {
        return;
    };
    for version in versions.flatten() {
        let wineserver = version.path().join("wine/bin/wineserver");
        if wineserver.is_file() {
            let _ = Command::new(&wineserver)
                .arg("-k")
                .env("WINEPREFIX", data.join("prefixes/test"))
                .status();
        }
    }
}

#[test]
fn install_doctor_prefix_exec_probe_snapshot_reset() {
    if !enabled() {
        eprintln!("[live] skipped: set APPLEJACK_LIVE_ENGINE=1");
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let data = scratch.path().join("data");
    let payload = payload_dir();

    let installed = ok(
        &data,
        &[
            "engine",
            "install",
            "rosetta-wine",
            "--from",
            payload.to_str().unwrap(),
        ],
    );
    assert!(installed.contains("(active)"), "{installed}");
    let list = ok(&data, &["engine", "list"]);
    assert!(list.contains("installed active"), "{list}");
    let doctor = ok(&data, &["engine", "doctor"]);
    assert!(doctor.contains("runnable: yes"), "{doctor}");

    ok(&data, &["prefix", "create", "test", "--backend", "dxmt"]);
    let prefixes = ok(&data, &["prefix", "list"]);
    assert!(prefixes.contains("backend dxmt"), "{prefixes}");

    let version = list.split_whitespace().nth(1).unwrap();
    let hello = data.join(format!("engines/rosetta-wine/{version}/probes/hello32.exe"));
    let exec = ok(
        &data,
        &["prefix", "exec", "test", "--", hello.to_str().unwrap()],
    );
    assert!(exec.contains("hello32 ok"), "{exec}");

    let exit = run(
        &data,
        &["prefix", "exec", "test", "--", "cmd", "/c", "exit", "5"],
    );
    assert_eq!(exit.status.code(), Some(5));

    let probe: Value = serde_json::from_str(&ok(&data, &["engine", "probe-cpuid"])).unwrap();
    assert!(probe["plain"]["avx"].is_boolean(), "{probe}");

    // Right after `exec`, so this also covers waiting out the server.
    let snapshot = ok(&data, &["prefix", "snapshot", "test"]);
    let timestamp = snapshot.split_whitespace().nth(2).unwrap().to_owned();
    assert!(
        data.join("prefixes/.snapshots/test")
            .join(&timestamp)
            .is_dir()
    );
    ok(&data, &["prefix", "restore", "test", &timestamp]);
    ok(&data, &["prefix", "reset", "test", "--keep-user-data"]);
    let exec = ok(
        &data,
        &["prefix", "exec", "test", "--", hello.to_str().unwrap()],
    );
    assert!(exec.contains("hello32 ok"), "after reset: {exec}");
    ok(&data, &["prefix", "remove", "test"]);
    assert_eq!(ok(&data, &["prefix", "list"]).trim(), "no prefixes");

    let listed = ok(&data, &["prefix", "list-snapshots", "test"]);
    assert!(listed.contains(&timestamp), "{listed}");
    ok(&data, &["prefix", "restore", "test", &timestamp]);
    let exec = ok(
        &data,
        &["prefix", "exec", "test", "--", hello.to_str().unwrap()],
    );
    assert!(exec.contains("hello32 ok"), "after a removal: {exec}");

    double_reset(&data);
    let exec = ok(
        &data,
        &["prefix", "exec", "test", "--", hello.to_str().unwrap()],
    );
    assert!(exec.contains("hello32 ok"), "after a double reset: {exec}");
    ok(&data, &["prefix", "remove", "test"]);
    stop_servers(&data);
}

/// Two `prefix.reset` lines, as a double click sends them, against real Wine:
/// the second is refused and the user's file lands in the safety snapshot.
fn double_reset(data: &Path) {
    let marker = "drive_c/users/save-marker.txt";
    std::fs::write(data.join("prefixes/test").join(marker), "only copy").unwrap();
    let mut core = applejack(data)
        .args(["serve", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = core.stdin.take().unwrap();
    let hello = json!({"jsonrpc": "2.0", "id": 1, "method": "hello",
                       "params": {"protocol_version": 1}});
    writeln!(stdin, "{hello}").unwrap();
    for id in [2, 3] {
        let reset = json!({"jsonrpc": "2.0", "id": id, "method": "prefix.reset",
                           "params": {"prefix": "test"}});
        writeln!(stdin, "{reset}").unwrap();
    }
    drop(stdin);
    let mut failures = Vec::new();
    let mut done = 0;
    let mut second = Value::Null;
    for line in BufReader::new(core.stdout.take().unwrap()).lines() {
        let line = line.unwrap();
        eprintln!("[live] {line}");
        let message: Value = serde_json::from_str(&line).unwrap();
        match message["method"].as_str() {
            Some("job.done") => done += 1,
            Some("job.failed") => failures.push(message),
            _ if message["id"] == json!(3) => second = message,
            _ => {}
        }
    }
    assert!(core.wait().unwrap().success());
    assert_eq!((done, failures.len()), (1, 0), "{failures:?}");
    assert_eq!(second["error"]["code"], json!(1011), "{second}");
    let snapshots = ok(data, &["prefix", "list-snapshots", "test"]);
    let safety = snapshots.lines().last().unwrap();
    assert!(
        data.join("prefixes/.snapshots/test")
            .join(safety)
            .join(marker)
            .is_file(),
        "{snapshots}"
    );
}

#[test]
fn an_install_finishes_after_both_pipes_close() {
    if !enabled() {
        eprintln!("[live] skipped: set APPLEJACK_LIVE_ENGINE=1");
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let data = scratch.path().join("data");
    let mut core = applejack(&data)
        .args(["serve", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = core.stdin.take().unwrap();
    let mut stdout = BufReader::new(core.stdout.take().unwrap());
    let hello = json!({"jsonrpc": "2.0", "id": 1, "method": "hello",
                       "params": {"protocol_version": 1}});
    let install = json!({"jsonrpc": "2.0", "id": 2, "method": "engine.install", "params":
                         {"engine_id": "rosetta-wine", "from": payload_dir()}});
    writeln!(stdin, "{hello}\n{install}").unwrap();
    let mut line = String::new();
    while !line.contains(r#""phase":"pack""#) {
        line.clear();
        assert_ne!(stdout.read_line(&mut line).unwrap(), 0, "core exited");
    }
    drop(stdout);
    drop(stdin);
    core.wait().unwrap();

    let root = data.join("engines/rosetta-wine");
    let names: Vec<String> = std::fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !names.iter().any(|name| name.ends_with(".tar.zst")),
        "{names:?}"
    );
    assert!(
        names
            .iter()
            .any(|name| root.join(name).join("BUNDLE.toml").is_file()),
        "the install did not finish: {names:?}"
    );
}

#[test]
fn serve_installs_with_progress_and_answers_while_it_runs() {
    if !enabled() {
        eprintln!("[live] skipped: set APPLEJACK_LIVE_ENGINE=1");
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let data = scratch.path().join("data");
    let mut core = applejack(&data)
        .args(["serve", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = core.stdin.take().unwrap();
    let mut stdout = BufReader::new(core.stdout.take().unwrap());
    // Owns stdin, so dropping it is the EOF that ends the core.
    let mut send = move |message: Value| writeln!(stdin, "{message}").unwrap();
    let mut read = || {
        let mut line = String::new();
        assert_ne!(stdout.read_line(&mut line).unwrap(), 0, "core exited");
        eprintln!("[live] {}", line.trim_end());
        serde_json::from_str::<Value>(&line).unwrap()
    };

    send(json!({"jsonrpc": "2.0", "id": 1, "method": "hello",
                "params": {"protocol_version": 1}}));
    assert!(read()["result"]["core_version"].is_string());
    send(
        json!({"jsonrpc": "2.0", "id": 2, "method": "engine.install", "params":
                {"engine_id": "rosetta-wine", "from": payload_dir()}}),
    );
    send(json!({"jsonrpc": "2.0", "id": 3, "method": "hello",
                "params": {"protocol_version": 1}}));

    let mut job_id = None;
    let mut hello_during_job = false;
    let mut phases = Vec::new();
    loop {
        let message = read();
        match message["method"].as_str() {
            Some("job.progress") => {
                phases.push(message["params"]["phase"].as_str().unwrap().to_owned());
            }
            Some("job.done") => {
                assert_eq!(Some(&message["params"]["job_id"]), job_id.as_ref());
                break;
            }
            Some(other) => panic!("{other}: {message}"),
            None if message["id"] == json!(2) => job_id = Some(message["result"]["job_id"].clone()),
            None => {
                assert_eq!(message["id"], json!(3));
                hello_during_job = !phases.iter().any(|phase| phase == "installed");
            }
        }
    }
    assert!(hello_during_job, "hello waited for the job");
    for phase in ["pack", "verify", "extract", "validate"] {
        assert!(phases.iter().any(|seen| seen == phase), "{phases:?}");
    }

    send(json!({"jsonrpc": "2.0", "id": 4, "method": "engine.list", "params": {}}));
    let list = read();
    assert_eq!(
        list["result"]["engines"][0]["active"],
        json!(true),
        "{list}"
    );
    drop(send);
    assert!(core.wait().unwrap().success());
}
