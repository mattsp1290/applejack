//! What a prefix survives: overlapping operations, a reader that dies under
//! a job, a failed reset, and a removal. Over a stub bundle, through the
//! operations the CLI and the RPC handler share. Lifted from the lifecycle
//! review's probes.

mod common;

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use applejack_core::rpc::jobs::Notifier;
use applejack_core::rpc::methods::Method;
use applejack_core::rpc::wire::{ErrorCode, RpcError};
use applejack_core::rpc::{self, CoreHandler, Handler};
use applejack_core::{Core, CoreError};
use applejack_engine::rosetta_wine::snapshot::PrefixLock;
use applejack_engine::{Error as EngineError, GraphicsBackend, HidrawMode};
use common::{script, write_stub_bundle};
use serde_json::{Value, json};
use tempfile::TempDir;

const HELLO: &str = r#"{"jsonrpc":"2.0","id":1,"method":"hello","params":{"protocol_version":1}}"#;
const LIST: &str = r#"{"jsonrpc":"2.0","id":2,"method":"engine.list","params":{}}"#;

fn parse(output: &[u8]) -> Vec<Value> {
    std::str::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn installed(dir: &TempDir) -> Core {
    let tree = dir.path().join("tree");
    write_stub_bundle(&tree, "10.0.0");
    let mut core = Core::new(dir.path().join("data")).with_unsigned_installs(true);
    core.plan_install("rosetta-wine", Some(&tree))
        .unwrap()
        .run(&mut |_| {})
        .unwrap();
    core.prefix_create("p", GraphicsBackend::Dxmt, None, HidrawMode::Disabled)
        .unwrap();
    core
}

fn stub_wine(dir: &TempDir) -> PathBuf {
    dir.path()
        .join("data/engines/rosetta-wine/10.0.0/wine/bin/wine64")
}

fn save_file(data: &Path) -> PathBuf {
    data.join("prefixes/p/drive_c/users/me/save.txt")
}

fn prefix_ok(core: &Core) -> bool {
    core.prefix_list()
        .unwrap()
        .iter()
        .any(|entry| entry.name == "p" && entry.problem.is_none())
}

fn reset_line(id: u32) -> String {
    json!({"jsonrpc":"2.0","id":id,"method":"prefix.reset","params":{"prefix":"p"}}).to_string()
}

/// One double click on reset, against a boot long enough for the second to
/// land inside the first. Whatever the answers, the prefix must survive and
/// the user's file must be in a snapshot.
fn double_reset() {
    let dir = TempDir::new().unwrap();
    let core = installed(&dir);
    let wine = stub_wine(&dir);
    let body = fs::read_to_string(&wine).unwrap();
    let slow = body.replacen(
        "name=$(basename \"$1\")",
        "name=$(basename \"$1\")\n[ \"$name\" = wineboot ] && sleep 0.5",
        1,
    );
    script(&wine, &slow);
    let data = core.data_dir().to_path_buf();
    fs::write(save_file(&data), "only copy").unwrap();
    let check = Core::new(&data);

    let input = format!("{HELLO}\n{}\n{}\n", reset_line(2), reset_line(3));
    let mut output = Vec::new();
    rpc::serve(input.as_bytes(), &mut output, &mut CoreHandler::new(core)).unwrap();

    let messages = parse(&output);
    let second = messages.iter().find(|m| m["id"] == json!(3)).unwrap();
    assert_eq!(
        second["error"]["code"],
        json!(ErrorCode::PrefixBusy.code()),
        "{messages:?}"
    );
    assert!(prefix_ok(&check), "no usable prefix: {messages:?}");
    let snapshots = check.prefix_snapshots("p").unwrap();
    assert!(
        snapshots
            .iter()
            .any(|s| s.dir.join("drive_c/users/me/save.txt").is_file()),
        "the user's file is in no snapshot: {messages:?}"
    );
}

#[test]
fn twelve_double_resets_leave_the_prefix_and_its_data() {
    for _ in 0..12 {
        double_reset();
    }
}

fn busy<T: std::fmt::Debug>(outcome: Result<T, CoreError>) {
    let error = outcome.unwrap_err();
    assert!(
        matches!(error, CoreError::Engine(EngineError::PrefixBusy { .. })),
        "{error:?}"
    );
    assert_eq!(error.code(), ErrorCode::PrefixBusy);
}

#[test]
fn every_operation_refuses_a_prefix_someone_else_holds() {
    let dir = TempDir::new().unwrap();
    let mut core = installed(&dir);
    let snapshot = core.prefix_snapshot("p").unwrap();
    let held = PrefixLock::acquire(&core.data_dir().join("prefixes/.snapshots"), "p").unwrap();

    busy(core.prefix_snapshot("p"));
    busy(core.prefix_restore("p", &snapshot.timestamp));
    busy(core.prefix_reset("p", false));
    busy(core.prefix_remove("p"));
    busy(core.prefix_create("p", GraphicsBackend::Dxmt, None, HidrawMode::Disabled));
    busy(core.prefix_exec(
        "p",
        Path::new("exit7.exe"),
        &[OsString::from("x")],
        &mut io::sink(),
    ));
    assert!(core.data_dir().join("prefixes/p").is_dir());

    drop(held);
    core.prefix_snapshot("p").unwrap();
}

#[test]
fn a_reset_of_an_unknown_prefix_is_refused_before_a_job_starts() {
    let dir = TempDir::new().unwrap();
    let core = installed(&dir);
    let line = json!({"jsonrpc":"2.0","id":2,"method":"prefix.reset","params":{"prefix":"nope"}});
    let input = format!("{HELLO}\n{line}\n");
    let mut output = Vec::new();
    rpc::serve(input.as_bytes(), &mut output, &mut CoreHandler::new(core)).unwrap();
    let messages = parse(&output);
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert_eq!(
        messages[1]["error"]["code"],
        json!(ErrorCode::InvalidParams.code())
    );
}

/// A job that sets a flag when it completes, after 300 ms.
struct Slow(Arc<AtomicBool>);

impl Handler for Slow {
    fn handle(&mut self, method: &Method, notifier: &Notifier) -> Result<Value, RpcError> {
        if let Method::EngineList(_) = method {
            let finished = self.0.clone();
            return rpc::to_result(&notifier.start_job(move |job| {
                job.progress("work", None, None, "first");
                std::thread::sleep(Duration::from_millis(300));
                job.progress("work", None, None, "second");
                finished.store(true, Ordering::SeqCst);
                Ok(())
            })?);
        }
        Ok(json!({}))
    }
}

/// Fails every write after the first `left` lines, as a closed pipe does.
struct DiesAfter(usize);

impl Write for DiesAfter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0 == 0 {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        self.0 -= 1;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_dead_reader_does_not_abandon_a_running_job() {
    let finished = Arc::new(AtomicBool::new(false));
    let input = format!("{HELLO}\n{LIST}\n");
    // `hello` and the job's start land, then the pipe is gone.
    let error = rpc::serve(input.as_bytes(), DiesAfter(2), &mut Slow(finished.clone()));
    assert_eq!(error.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    assert!(
        finished.load(Ordering::SeqCst),
        "serve returned while the job still ran; the caller's exit kills it"
    );
}

fn packed_archives(data: &Path) -> Vec<String> {
    fs::read_dir(data.join("engines/rosetta-wine"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tar.zst"))
        .collect()
}

#[test]
fn an_install_finishes_under_a_dead_reader_and_leaves_no_archive() {
    let dir = TempDir::new().unwrap();
    let tree = dir.path().join("tree");
    write_stub_bundle(&tree, "10.0.0");
    let data = dir.path().join("data");
    let core = Core::new(&data).with_unsigned_installs(true);
    let line = json!({
        "jsonrpc": "2.0", "id": 2, "method": "engine.install",
        "params": {"engine_id": "rosetta-wine", "from": tree}
    });
    let input = format!("{HELLO}\n{line}\n");
    rpc::serve(input.as_bytes(), DiesAfter(1), &mut CoreHandler::new(core)).unwrap_err();
    assert!(
        data.join("engines/rosetta-wine/10.0.0/BUNDLE.toml")
            .is_file()
    );
    assert_eq!(packed_archives(&data), Vec::<String>::new());
}

#[test]
fn the_next_install_clears_the_archive_a_killed_one_left() {
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("data");
    let root = data.join("engines/rosetta-wine");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("9.0.0.local.tar.zst"), "left by a killed install").unwrap();
    let tree = dir.path().join("tree");
    write_stub_bundle(&tree, "10.0.0");
    let core = Core::new(&data).with_unsigned_installs(true);
    core.plan_install("rosetta-wine", Some(&tree))
        .unwrap()
        .run(&mut |_| {})
        .unwrap();
    assert_eq!(packed_archives(&data), Vec::<String>::new());
}

#[test]
fn a_removal_can_be_undone_from_its_snapshot() {
    let dir = TempDir::new().unwrap();
    let core = installed(&dir);
    fs::write(save_file(core.data_dir()), "kept").unwrap();
    let snapshot = core.prefix_snapshot("p").unwrap();
    core.prefix_remove("p").unwrap();

    let listed = core.prefix_snapshots("p").unwrap();
    assert_eq!(
        listed,
        std::slice::from_ref(&snapshot),
        "a removed prefix keeps its list"
    );
    let restored = core.prefix_restore("p", &snapshot.timestamp).unwrap();
    assert_eq!(restored.timestamp, snapshot.timestamp);
    assert!(prefix_ok(&core));
    assert_eq!(
        fs::read_to_string(save_file(core.data_dir())).unwrap(),
        "kept"
    );
    assert_eq!(
        core.prefix_snapshots("p").unwrap().len(),
        1,
        "no safety copy"
    );
}

#[test]
fn restoring_a_gone_prefix_from_a_snapshot_it_lacks_fails_cleanly() {
    let dir = TempDir::new().unwrap();
    let core = installed(&dir);
    core.prefix_remove("p").unwrap();
    let error = core.prefix_restore("p", "19700101T000000Z").unwrap_err();
    assert!(matches!(
        error,
        CoreError::Engine(EngineError::SnapshotNotFound { .. })
    ));
}

#[test]
fn a_failed_reset_names_its_snapshot_and_can_be_restored() {
    let dir = TempDir::new().unwrap();
    let mut core = installed(&dir);
    fs::write(save_file(core.data_dir()), "only copy").unwrap();
    script(&stub_wine(&dir), "#!/bin/sh\nexit 3\n");

    let error = core.prefix_reset("p", true).unwrap_err();
    assert!(core.prefix_list().unwrap().is_empty(), "the prefix is gone");
    let snapshots = core.prefix_snapshots("p").unwrap();
    assert_eq!(snapshots.len(), 1);
    let stamp = &snapshots[0].timestamp;
    let text = error.to_string();
    assert!(
        text.contains(&format!("applejack prefix restore p {stamp}")),
        "{text}"
    );

    core.prefix_restore("p", stamp).unwrap();
    assert_eq!(
        fs::read_to_string(save_file(core.data_dir())).unwrap(),
        "only copy"
    );
}

#[test]
fn a_failed_reset_over_rpc_names_its_snapshot_in_job_failed() {
    let dir = TempDir::new().unwrap();
    let core = installed(&dir);
    script(&stub_wine(&dir), "#!/bin/sh\nexit 3\n");
    let check = Core::new(core.data_dir());
    let input = format!("{HELLO}\n{}\n", reset_line(2));
    let mut output = Vec::new();
    rpc::serve(input.as_bytes(), &mut output, &mut CoreHandler::new(core)).unwrap();
    let messages = parse(&output);
    let failed = messages
        .iter()
        .find(|message| message["method"] == json!("job.failed"))
        .unwrap();
    let stamp = &check.prefix_snapshots("p").unwrap()[0].timestamp;
    let text = failed["params"]["message"].as_str().unwrap();
    assert!(text.contains(stamp.as_str()), "{text}");
}

#[test]
fn a_from_directory_and_an_archive_report_the_same_code_for_no_descriptor() {
    let dir = TempDir::new().unwrap();
    let empty = dir.path().join("empty");
    fs::create_dir(&empty).unwrap();
    let core = Core::new(dir.path().join("data")).with_unsigned_installs(true);
    let error = core
        .plan_install("rosetta-wine", Some(&empty))
        .unwrap()
        .run(&mut |_| {})
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::InvalidBundle, "{error}");
}
