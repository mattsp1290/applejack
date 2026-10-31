//! Jobs and the wire: notifications from job threads share stdout with
//! responses, and a job never holds up the request loop.

mod common;

use std::collections::BTreeMap;
use std::io::{self, BufRead, Read, Write};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use applejack_core::Core;
use applejack_core::rpc::jobs::{JobError, Notifier};
use applejack_core::rpc::methods::Method;
use applejack_core::rpc::wire::{ErrorCode, RpcError};
use applejack_core::rpc::{self, CoreHandler, ExitStatus, Handler};
use common::write_stub_bundle;
use serde_json::{Value, json};
use tempfile::TempDir;

const HELLO: &str = r#"{"jsonrpc":"2.0","id":1,"method":"hello","params":{"protocol_version":1}}"#;
const DOCTOR: &str = r#"{"jsonrpc":"2.0","id":2,"method":"host.doctor","params":{}}"#;

/// One message per wire line, whatever its kind.
fn parse_wire(output: &[u8]) -> Vec<Value> {
    let text = std::str::from_utf8(output).expect("UTF-8");
    assert!(text.is_empty() || text.ends_with('\n'), "unterminated line");
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
        .collect()
}

fn method_of(message: &Value) -> Option<&str> {
    message["method"].as_str()
}

/// `engine.list` starts `JOBS` jobs of `NOTES` notifications each.
struct Chatty;

const JOBS: usize = 8;
const NOTES: usize = 400;

impl Handler for Chatty {
    fn handle(&mut self, method: &Method, notifier: &Notifier) -> Result<Value, RpcError> {
        if let Method::EngineList(_) = method {
            for _ in 0..JOBS {
                notifier.start_job(|job| {
                    for index in 0..NOTES {
                        job.progress("work", Some(index as u64), None, "x".repeat(4096));
                    }
                    Ok(())
                })?;
            }
        }
        Ok(json!({"method": method.name()}))
    }
}

/// The framing proof: job threads write while the loop answers. Every wire
/// line parses, each job's notes arrive complete and in order, and each job
/// ends once.
#[test]
fn notifications_and_responses_never_break_a_line() {
    let list = r#"{"jsonrpc":"2.0","id":3,"method":"engine.list","params":{}}"#;
    let requests: String =
        std::iter::repeat_n(DOCTOR, 500).fold(format!("{HELLO}\n{list}\n"), |mut input, line| {
            input.push_str(line);
            input.push('\n');
            input
        });
    let mut output = Vec::new();
    let status = rpc::serve(requests.as_bytes(), &mut output, &mut Chatty).unwrap();
    assert_eq!(status, ExitStatus::Eof);

    let wire = parse_wire(&output);
    let responses = wire.iter().filter(|m| m.get("id").is_some()).count();
    assert_eq!(responses, 502);
    let mut seen: BTreeMap<u64, u64> = BTreeMap::new();
    let mut done = 0;
    for message in &wire {
        match method_of(message) {
            Some("job.progress") => {
                let job = message["params"]["job_id"].as_u64().unwrap();
                let index = message["params"]["done_bytes"].as_u64().unwrap();
                let next = seen.entry(job).or_default();
                assert_eq!(index, *next, "job {job} out of order");
                *next += 1;
            }
            Some("job.done") => done += 1,
            _ => {}
        }
    }
    assert_eq!(seen.len(), JOBS);
    assert!(
        seen.values().all(|&count| count == NOTES as u64),
        "{seen:?}"
    );
    assert_eq!(done, JOBS);
}

/// `engine.list` starts a job that blocks until the test releases it.
struct Blocking {
    release: Option<Receiver<()>>,
}

impl Handler for Blocking {
    fn handle(&mut self, method: &Method, notifier: &Notifier) -> Result<Value, RpcError> {
        if let Method::EngineList(_) = method {
            let release = self.release.take().expect("one job");
            return rpc::to_result(&notifier.start_job(move |_| {
                let _ = release.recv();
                Ok(())
            })?);
        }
        Ok(json!({}))
    }
}

/// Stdin as a channel of lines, so a test feeds the loop one step at a time.
struct LineReader {
    lines: Receiver<String>,
    pending: Vec<u8>,
}

impl Read for LineReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let count = available.len().min(buffer.len());
        buffer[..count].copy_from_slice(&available[..count]);
        self.consume(count);
        Ok(count)
    }
}

impl BufRead for LineReader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.pending.is_empty()
            && let Ok(line) = self.lines.recv()
        {
            self.pending = line.into_bytes();
        }
        Ok(&self.pending)
    }

    fn consume(&mut self, count: usize) {
        self.pending.drain(..count);
    }
}

/// Stdout as a channel of chunks.
struct ChannelWriter(Sender<Vec<u8>>);

impl Write for ChannelWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let _ = self.0.send(bytes.to_vec());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn next_message(output: &Receiver<Vec<u8>>) -> Value {
    let chunk = output
        .recv_timeout(Duration::from_secs(30))
        .expect("the core answered in time");
    serde_json::from_slice(&chunk).expect("one whole message per write")
}

#[test]
fn a_running_job_does_not_hold_up_hello_and_eof_waits_for_it() {
    let (release, released) = mpsc::channel();
    let (send_line, lines) = mpsc::channel::<String>();
    let (written, output) = mpsc::channel();
    let server = thread::spawn(move || {
        let reader = LineReader {
            lines,
            pending: Vec::new(),
        };
        let mut handler = Blocking {
            release: Some(released),
        };
        rpc::serve(reader, ChannelWriter(written), &mut handler)
    });

    let say = |line: &str| send_line.send(format!("{line}\n")).unwrap();
    say(HELLO);
    assert_eq!(next_message(&output)["id"], json!(1));
    say(r#"{"jsonrpc":"2.0","id":3,"method":"engine.list","params":{}}"#);
    let started = next_message(&output);
    assert_eq!(started["result"], json!({"job_id": 1}));

    say(HELLO);
    assert_eq!(
        next_message(&output)["id"],
        json!(1),
        "hello while the job runs"
    );
    say(DOCTOR);
    assert_eq!(next_message(&output)["id"], json!(2));

    drop(send_line);
    assert!(
        output.recv_timeout(Duration::from_millis(200)).is_err(),
        "the job has not finished"
    );
    assert!(!server.is_finished(), "EOF waits for the running job");
    release.send(()).unwrap();
    let done = next_message(&output);
    assert_eq!(done["method"], json!("job.done"));
    assert_eq!(server.join().unwrap().unwrap(), ExitStatus::Eof);
}

struct Failing;

impl Handler for Failing {
    fn handle(&mut self, method: &Method, notifier: &Notifier) -> Result<Value, RpcError> {
        match method {
            Method::EngineList(_) => rpc::to_result(&notifier.start_job(|_| {
                Err(JobError {
                    code: ErrorCode::DiskFull,
                    message: "full".to_owned(),
                })
            })?),
            Method::EngineDoctor(_) => rpc::to_result(&notifier.start_job(|_| panic!("job bug"))?),
            _ => Ok(json!({})),
        }
    }
}

#[test]
fn a_job_ends_with_one_failure_even_when_it_panics() {
    let input = format!(
        "{HELLO}\n{}\n{}\n",
        r#"{"jsonrpc":"2.0","id":3,"method":"engine.list","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":4,"method":"engine.doctor","params":{}}"#,
    );
    let mut output = Vec::new();
    rpc::serve(input.as_bytes(), &mut output, &mut Failing).unwrap();
    let wire = parse_wire(&output);
    let mut failed: Vec<(u64, i64)> = wire
        .iter()
        .filter(|message| method_of(message) == Some("job.failed"))
        .map(|message| {
            (
                message["params"]["job_id"].as_u64().unwrap(),
                message["params"]["code"].as_i64().unwrap(),
            )
        })
        .collect();
    failed.sort_unstable();
    assert_eq!(
        failed,
        [
            (1, ErrorCode::DiskFull.code()),
            (2, ErrorCode::InternalError.code())
        ]
    );
}

struct BrokenPipe;

impl Write for BrokenPipe {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "closed"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_writer_failure_is_the_error_serve_returns() {
    let input = format!("{HELLO}\n{DOCTOR}\n{HELLO}\n{DOCTOR}\n");
    let error = rpc::serve(input.as_bytes(), BrokenPipe, &mut Chatty).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
}

fn install_line(from: Option<&Path>) -> String {
    let mut params = json!({"engine_id": "rosetta-wine"});
    if let Some(from) = from {
        params["from"] = json!(from);
    }
    json!({"jsonrpc": "2.0", "id": 5, "method": "engine.install", "params": params}).to_string()
}

const LIST: &str = r#"{"jsonrpc":"2.0","id":6,"method":"engine.list","params":{}}"#;

fn handler(dir: &TempDir, allow_unsigned: bool) -> CoreHandler {
    CoreHandler::new(Core::new(dir.path().join("data")).with_unsigned_installs(allow_unsigned))
}

#[test]
fn engine_install_streams_progress_then_done_then_lists_it() {
    let dir = TempDir::new().unwrap();
    let tree = dir.path().join("tree");
    write_stub_bundle(&tree, "10.0.0");
    let mut handler = handler(&dir, true);
    let input = format!("{HELLO}\n{}\n", install_line(Some(&tree)));
    let mut output = Vec::new();
    rpc::serve(input.as_bytes(), &mut output, &mut handler).unwrap();
    let wire = parse_wire(&output);

    let started = wire.iter().find(|m| m["id"] == json!(5)).unwrap();
    let job_id = started["result"]["job_id"].clone();
    assert!(job_id.is_u64(), "{started}");
    let phases: Vec<&str> = wire
        .iter()
        .filter(|message| method_of(message) == Some("job.progress"))
        .map(|message| {
            assert_eq!(message["params"]["job_id"], job_id);
            message["params"]["phase"].as_str().unwrap()
        })
        .collect();
    assert_eq!(
        phases,
        ["pack", "verify", "extract", "validate", "installed"]
    );
    let last = wire.last().unwrap();
    assert_eq!(method_of(last), Some("job.done"), "{last}");

    let input = format!("{HELLO}\n{LIST}\n");
    let mut output = Vec::new();
    rpc::serve(input.as_bytes(), &mut output, &mut handler).unwrap();
    let wire = parse_wire(&output);
    assert_eq!(
        wire[1]["result"],
        json!({"engines": [{
            "engine_id": "rosetta-wine", "version": "10.0.0", "installed": true, "active": true,
        }]})
    );
}

#[test]
fn engine_install_refusals_are_responses_and_failures_are_typed_jobs() {
    let dir = TempDir::new().unwrap();
    let tree = dir.path().join("tree");
    write_stub_bundle(&tree, "10.0.0");

    let input = format!("{HELLO}\n{}\n", install_line(None));
    let mut output = Vec::new();
    rpc::serve(input.as_bytes(), &mut output, &mut handler(&dir, true)).unwrap();
    let wire = parse_wire(&output);
    assert_eq!(
        wire[1]["error"]["code"],
        json!(ErrorCode::DownloadFailed.code())
    );
    assert!(
        wire[1]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--from")
    );

    let input = format!("{HELLO}\n{}\n", install_line(Some(&tree)));
    let mut output = Vec::new();
    rpc::serve(input.as_bytes(), &mut output, &mut handler(&dir, false)).unwrap();
    let wire = parse_wire(&output);
    assert_eq!(
        wire[1]["error"]["code"],
        json!(ErrorCode::UnsignedEngineRefused.code())
    );

    std::fs::remove_file(tree.join("wine/bin/wine64")).unwrap();
    let input = format!("{HELLO}\n{}\n", install_line(Some(&tree)));
    let mut output = Vec::new();
    rpc::serve(input.as_bytes(), &mut output, &mut handler(&dir, true)).unwrap();
    let wire = parse_wire(&output);
    let failed = wire.last().unwrap();
    assert_eq!(method_of(failed), Some("job.failed"), "{failed}");
    assert_eq!(
        failed["params"]["code"],
        json!(ErrorCode::InvalidBundle.code())
    );
}

#[test]
fn config_methods_round_trip_through_the_handler() {
    let dir = TempDir::new().unwrap();
    let preset = dir.path().join("preset.json");
    let requests = [
        json!({"jsonrpc": "2.0", "id": 2, "method": "config.set", "params":
            {"appid": 220, "key": "hidraw_enabled", "value": true}}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "config.set", "params":
            {"appid": 220, "key": "extra_env", "value": {"WINEDEBUG": "-all"}}}),
        json!({"jsonrpc": "2.0", "id": 4, "method": "config.set", "params":
            {"appid": 220, "key": "launch_exe_override", "value": "game.exe"}}),
        json!({"jsonrpc": "2.0", "id": 5, "method": "config.get", "params": {"appid": 220}}),
        json!({"jsonrpc": "2.0", "id": 6, "method": "config.export", "params":
            {"appid": 220, "path": preset}}),
        json!({"jsonrpc": "2.0", "id": 7, "method": "config.import", "params":
            {"appid": 221, "path": preset}}),
        json!({"jsonrpc": "2.0", "id": 8, "method": "config.import", "params":
            {"appid": 221, "path": preset, "accept_launch_overrides": true}}),
        json!({"jsonrpc": "2.0", "id": 9, "method": "config.set", "params":
            {"appid": 220, "key": "nope", "value": 1}}),
    ];
    let input = requests
        .iter()
        .fold(format!("{HELLO}\n"), |input, request| {
            format!("{input}{request}\n")
        });
    let mut output = Vec::new();
    rpc::serve(input.as_bytes(), &mut output, &mut handler(&dir, false)).unwrap();
    let wire = parse_wire(&output);
    assert_eq!(wire[1]["result"], json!({}));
    let config = &wire[4]["result"];
    assert_eq!(config["hidraw_enabled"], json!(true));
    assert_eq!(config["extra_env"], json!({"WINEDEBUG": "-all"}));
    assert_eq!(wire[5]["result"], json!({"path": preset}));

    let unconfirmed = &wire[6]["result"];
    assert_eq!(unconfirmed["applied"], json!(false));
    assert_eq!(unconfirmed["preset_appid"], json!(220));
    assert_eq!(
        unconfirmed["confirmations"],
        json!([{"field": "launch_exe_override", "value": "game.exe"}])
    );
    assert_eq!(wire[7]["result"]["applied"], json!(true));
    assert_eq!(
        wire[8]["error"]["code"],
        json!(ErrorCode::InvalidParams.code())
    );
}

#[test]
fn prefix_reset_runs_as_a_job() {
    let dir = TempDir::new().unwrap();
    let tree = dir.path().join("tree");
    write_stub_bundle(&tree, "10.0.0");
    let mut core = Core::new(dir.path().join("data")).with_unsigned_installs(true);
    core.plan_install("rosetta-wine", Some(&tree))
        .unwrap()
        .run(&mut |_| {})
        .unwrap();
    core.prefix_create(
        "p",
        applejack_engine::GraphicsBackend::Dxmt,
        None,
        applejack_engine::HidrawMode::Disabled,
    )
    .unwrap();
    let requests = [
        r#"{"jsonrpc":"2.0","id":2,"method":"prefix.snapshot","params":{"prefix":"p"}}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"prefix.reset","params":{"prefix":"p"}}"#,
    ];
    let input = format!("{HELLO}\n{}\n{}\n", requests[0], requests[1]);
    let mut output = Vec::new();
    rpc::serve(input.as_bytes(), &mut output, &mut CoreHandler::new(core)).unwrap();
    let wire = parse_wire(&output);
    let snapshot = wire.iter().find(|m| m["id"] == json!(2)).unwrap();
    assert_eq!(snapshot["result"]["prefix"], json!("p"));
    let reset = wire.iter().find(|m| m["id"] == json!(3)).unwrap();
    assert!(reset["result"]["job_id"].is_u64(), "{reset}");
    assert_eq!(method_of(wire.last().unwrap()), Some("job.done"));
}
