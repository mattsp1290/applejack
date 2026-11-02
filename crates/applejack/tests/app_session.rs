//! The session rules across real processes: `applejack app run` in one,
//! `serve --stdio` or another CLI in the next, over a stub bundle. Nothing
//! here touches a Wine payload, a window server, or `~/Library`.

#[path = "../../applejack-core/tests/common/mod.rs"]
mod common;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use applejack_engine::rosetta_wine::process::find_session;
use common::{write_stub_bundle, write_title};
use serde_json::{Value, json};
use tempfile::TempDir;

const APP: &str = "1234";

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let fixture = Self { dir };
        fs::create_dir_all(fixture.data()).unwrap();
        let tree = fixture.dir.path().join("tree");
        write_stub_bundle(&tree, "10.0.0");
        write_title(
            &fixture.data().join("library"),
            1234,
            &["title.exe", "quick.exe"],
        );
        fixture.ok(&[
            "engine",
            "install",
            "rosetta-wine",
            "--from",
            tree.to_str().unwrap(),
        ]);
        fixture.ok(&[
            "app",
            "register",
            APP,
            "--drm",
            "none",
            "--exe",
            "title.exe",
        ]);
        fixture
    }

    fn data(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_applejack"));
        command
            .env("APPLEJACK_DATA_DIR", self.data())
            .env("APPLEJACK_ALLOW_UNSIGNED_ENGINE", "1");
        command
    }

    fn output(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.output(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn refused(&self, args: &[&str]) -> String {
        let output = self.output(args);
        assert!(!output.status.success(), "{args:?} was not refused");
        String::from_utf8(output.stderr).unwrap()
    }

    fn sessions(&self) -> Vec<Value> {
        serde_json::from_str(&self.ok(&["session", "list", "--json"])).unwrap()
    }

    fn attached_run(&self) -> Child {
        let child = self
            .command()
            .args(["app", "run", APP])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.wait_for_state("running");
        child
    }

    fn wait_for_state(&self, state: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(row) = self.sessions().first()
                && row["state"] == json!(state)
            {
                return row.clone();
            }
            assert!(Instant::now() < deadline, "no session reached {state}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn prefix(&self) -> PathBuf {
        self.data().join("prefixes").join(APP)
    }
}

fn key(row: &Value) -> String {
    row["session_key"].as_str().unwrap().to_owned()
}

fn request(serve: &mut Child, line: &Value) -> Value {
    let stdin = serve.stdin.as_mut().unwrap();
    writeln!(stdin, "{line}").unwrap();
    stdin.flush().unwrap();
    Value::Null
}

fn responses(reader: &mut impl BufRead, id: i64) -> (Value, Vec<Value>) {
    let mut before = Vec::new();
    loop {
        let mut line = String::new();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0, "serve closed");
        let message: Value = serde_json::from_str(&line).unwrap();
        if message["id"] == json!(id) {
            return (message, before);
        }
        before.push(message);
    }
}

#[test]
fn an_attached_run_holds_the_prefix_against_other_processes_until_the_title_exits() {
    let fixture = Fixture::new();
    let mut run = fixture.attached_run();
    for args in [
        &["prefix", "reset", APP][..],
        &["prefix", "remove", APP],
        &["prefix", "restore", APP, "20260101T000000Z"],
        &["prefix", "snapshot", APP],
    ] {
        let text = fixture.refused(args);
        assert!(text.contains("is using prefix"), "{args:?}: {text}");
    }
    let text = fixture.refused(&["app", "run", APP]);
    assert!(text.contains("already running"), "{text}");
    assert_eq!(run.try_wait().unwrap(), None, "the run stays attached");

    let cancel = fixture.ok(&["app", "cancel", APP]);
    assert!(cancel.contains("cancelled"), "{cancel}");
    let status = run.wait().unwrap();
    assert_eq!(status.code(), Some(128 + 15), "the title's own status");
    let row = &fixture.sessions()[0];
    assert_eq!(row["state"], json!("cancelled"));
    assert_eq!(find_session(&key(row)), []);
    fixture.ok(&["prefix", "snapshot", APP]);
}

#[test]
fn serve_and_a_cli_run_see_each_other() {
    let fixture = Fixture::new();
    let mut run = fixture.attached_run();
    let mut serve = fixture
        .command()
        .args(["serve", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(serve.stdout.take().unwrap());
    let message = |id: i64, method: &str, params: Value| json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    request(
        &mut serve,
        &message(1, "hello", json!({"protocol_version": 1})),
    );
    responses(&mut reader, 1);

    request(&mut serve, &message(2, "app.run", json!({"appid": 1234})));
    let (answer, _) = responses(&mut reader, 2);
    assert_eq!(answer["error"]["code"], json!(1014), "{answer}");
    request(
        &mut serve,
        &message(3, "prefix.snapshot", json!({"prefix": APP})),
    );
    let (answer, _) = responses(&mut reader, 3);
    assert_eq!(answer["error"]["code"], json!(1011), "{answer}");
    request(
        &mut serve,
        &message(4, "session.list", json!({"appid": 1234})),
    );
    let (answer, _) = responses(&mut reader, 4);
    assert_eq!(answer["result"]["sessions"][0]["state"], json!("running"));

    request(
        &mut serve,
        &message(5, "app.cancel", json!({"appid": 1234})),
    );
    let (answer, _) = responses(&mut reader, 5);
    assert!(answer["result"]["job_id"].is_number(), "{answer}");
    let job = answer["result"]["job_id"].clone();
    loop {
        let mut line = String::new();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        let event: Value = serde_json::from_str(&line).unwrap();
        if event["params"]["job_id"] == job && event["method"] != json!("job.progress") {
            assert_eq!(event["method"], json!("job.done"), "{event}");
            break;
        }
    }
    assert_eq!(run.wait().unwrap().code(), Some(128 + 15));
    assert_eq!(fixture.sessions()[0]["state"], json!("cancelled"));
    drop(serve.stdin.take());
    assert_eq!(serve.wait().unwrap().code(), Some(0));
}

#[test]
fn a_killed_cli_leaves_a_title_that_the_next_process_can_see_and_cancel() {
    let fixture = Fixture::new();
    let mut run = fixture.attached_run();
    run.kill().unwrap();
    run.wait().unwrap();

    let row = fixture.sessions()[0].clone();
    assert_eq!(row["state"], json!("running"), "the title outlives its CLI");
    assert_ne!(find_session(&key(&row)), []);
    let text = fixture.refused(&["app", "run", APP]);
    assert!(text.contains("already running"), "{text}");

    let cancel = fixture.ok(&["app", "cancel", APP]);
    assert!(cancel.contains("cancelled"), "{cancel}");
    assert_eq!(find_session(&key(&row)), []);
    assert_eq!(fixture.sessions()[0]["state"], json!("cancelled"));
    fixture.ok(&["prefix", "snapshot", APP]);
}

#[test]
fn a_title_gone_with_its_cli_closes_as_lost_on_the_next_start() {
    let fixture = Fixture::new();
    let mut run = fixture.attached_run();
    run.kill().unwrap();
    run.wait().unwrap();
    quit(&fixture.prefix());
    let row = fixture.wait_for_state("lost");
    assert_eq!(row["exit_code"], Value::Null);
    assert!(row["ended_at"].is_string());
}

fn quit(prefix: &Path) {
    fs::write(prefix.join("quit"), "0").unwrap();
}

#[test]
fn a_run_exits_with_the_titles_code() {
    let fixture = Fixture::new();
    let mut run = fixture.attached_run();
    // Past the early-exit window, or the run reports that instead.
    std::thread::sleep(Duration::from_secs(5));
    fs::write(fixture.prefix().join("quit"), "9").unwrap();
    assert_eq!(run.wait().unwrap().code(), Some(9));
    let row = &fixture.sessions()[0];
    assert_eq!(row["state"], json!("exited"));
    assert_eq!(row["exit_code"], json!(9));
}

#[test]
fn a_quick_exit_is_reported_and_fails_the_run() {
    let fixture = Fixture::new();
    fixture.ok(&["config", APP, "set", "launch_exe_override", "quick.exe"]);
    let text = fixture.refused(&["app", "run", APP]);
    assert!(text.contains("may require the Steam client"), "{text}");
    assert_eq!(fixture.sessions()[0]["state"], json!("exited"));
}

#[test]
fn register_needs_a_manifest_and_session_list_starts_empty() {
    let fixture = Fixture::new();
    let text = fixture.refused(&["app", "register", "999"]);
    assert!(text.contains("not installed"), "{text}");
    assert_eq!(fixture.ok(&["session", "list"]).trim(), "no sessions");
    let text = fixture.refused(&["app", "cancel", APP]);
    assert!(text.contains("not running"), "{text}");
}
