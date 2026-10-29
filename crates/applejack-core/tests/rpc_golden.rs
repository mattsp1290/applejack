//! Goldens under `tests/rpc_golden/` and the method table in
//! `docs/rpc-protocol.md` are generated from the Rust types and must match
//! what is committed. `APPLEJACK_UPDATE_GOLDENS=1` rewrites both.

use applejack_core::host::{HostFacts, ScreenRecordingState};
use applejack_core::rpc::PROTOCOL_VERSION;
use applejack_core::rpc::methods::{HelloResult, Method, MethodStatus};
use applejack_core::rpc::wire::{Id, Request, Response};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

const UPDATE_ENV: &str = "APPLEJACK_UPDATE_GOLDENS";

/// `docs/rpc-protocol.md`, relative to this crate.
const DOC_PATH: &str = "../../docs/rpc-protocol.md";
const TABLE_BEGIN: &str = "<!-- method-table:begin -->";
const TABLE_END: &str = "<!-- method-table:end -->";

fn update_requested() -> bool {
    std::env::var_os(UPDATE_ENV).is_some_and(|value| value == "1")
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("rpc_golden")
}

fn pretty<T: serde::Serialize>(value: &T) -> String {
    let mut text = serde_json::to_string_pretty(value).expect("wire types encode");
    text.push('\n');
    text
}

/// File name to content, generated from the types.
fn expected() -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    for (index, method) in Method::examples().iter().enumerate() {
        let id = Id::Number(i64::try_from(index + 1).expect("small"));
        let request = Request::new(id, method.name(), method.params());
        files.insert(format!("{}.request.json", method.name()), pretty(&request));
    }
    let hello = Response::result(
        Id::Number(1),
        serde_json::to_value(HelloResult {
            protocol_version: PROTOCOL_VERSION,
            core_version: "0.1.0".to_owned(),
        })
        .expect("encodes"),
    );
    files.insert("hello.result.json".to_owned(), pretty(&hello));
    let facts = Response::result(
        Id::Number(2),
        serde_json::to_value(HostFacts {
            macos_version: "26.0.1".to_owned(),
            arch: Some("arm64".to_owned()),
            rosetta_present: Some(true),
            free_disk_bytes: Some(1_168_608_788_480),
            screen_recording: ScreenRecordingState::Unknown,
            developer_id_identities: Some(1),
        })
        .expect("encodes"),
    );
    files.insert("host.doctor.result.json".to_owned(), pretty(&facts));
    files
}

fn committed(dir: &Path) -> BTreeMap<String, String> {
    fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|entry| entry.expect("readable entry"))
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let text = fs::read_to_string(entry.path()).expect("readable golden");
            (name, text)
        })
        .collect()
}

/// First differing line, for the failure message.
fn diff(expected: &str, actual: &str) -> String {
    let mut lines = expected.lines().zip(actual.lines()).enumerate();
    match lines.find(|(_, (left, right))| left != right) {
        Some((index, (left, right))) => {
            format!(
                "line {}:\n  expected: {left}\n  actual:   {right}",
                index + 1
            )
        }
        None => format!(
            "line counts differ: expected {}, actual {}",
            expected.lines().count(),
            actual.lines().count()
        ),
    }
}

/// The method table as `docs/rpc-protocol.md` carries it between the markers.
fn render_method_table() -> String {
    let mut table = String::from("| Method | Status |\n|---|---|\n");
    for method in Method::examples() {
        let status = match method.status() {
            MethodStatus::Implemented => "implemented".to_owned(),
            MethodStatus::Pending { work_package } => {
                format!("pending (work package {work_package})")
            }
        };
        writeln!(table, "| `{}` | {status} |", method.name()).expect("string write");
    }
    table
}

#[test]
fn docs_method_table_matches_the_types() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(DOC_PATH);
    let doc = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let start = doc.find(TABLE_BEGIN).expect("begin marker in docs") + TABLE_BEGIN.len();
    let end = doc.find(TABLE_END).expect("end marker in docs");
    assert!(start <= end, "method table markers are out of order");
    let expected = render_method_table();
    let actual = doc[start..end].trim();
    if actual == expected.trim() {
        return;
    }
    if update_requested() {
        let rewritten = format!("{}\n{expected}{}", &doc[..start], &doc[end..]);
        fs::write(&path, rewritten).expect("docs written");
        return;
    }
    panic!(
        "{} method table drifted from methods.rs (set {UPDATE_ENV}=1 to rewrite):\n{}",
        path.display(),
        diff(expected.trim(), actual)
    );
}

#[test]
fn committed_goldens_match_the_types() {
    let dir = golden_dir();
    let update = update_requested();
    let expected = expected();
    let committed = committed(&dir);
    let mut drift = Vec::new();

    for (name, text) in &expected {
        match committed.get(name) {
            Some(actual) if actual == text => {}
            Some(actual) => drift.push(format!("{name} differs: {}", diff(text, actual))),
            None => drift.push(format!("{name} is missing")),
        }
        if update {
            fs::write(dir.join(name), text).expect("golden written");
        }
    }
    for name in committed
        .keys()
        .filter(|name| !expected.contains_key(*name))
    {
        drift.push(format!("{name} has no method in the table"));
        if update {
            fs::remove_file(dir.join(name)).expect("stray golden removed");
        }
    }

    assert!(
        drift.is_empty() || update,
        "goldens drifted from the Rust types (set {UPDATE_ENV}=1 to rewrite):\n{}",
        drift.join("\n")
    );
}

#[test]
fn every_committed_request_parses_into_the_table() {
    let committed = committed(&golden_dir());
    let mut parsed = Vec::new();
    for (name, text) in committed
        .iter()
        .filter(|(name, _)| name.ends_with(".request.json"))
    {
        let request: Request = serde_json::from_str(text).unwrap_or_else(|e| panic!("{name}: {e}"));
        let method = Method::from_request(&request.method, request.params)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            format!("{}.request.json", method.name()),
            *name,
            "golden file name does not match its method"
        );
        parsed.push(method.name());
    }
    let mut names: Vec<&str> = Method::NAMES.to_vec();
    names.sort_unstable();
    parsed.sort_unstable();
    assert_eq!(parsed, names, "one request golden per table method");
}

#[test]
fn result_goldens_decode_into_their_types() {
    let dir = golden_dir();
    let hello: Response =
        serde_json::from_str(&fs::read_to_string(dir.join("hello.result.json")).unwrap()).unwrap();
    let applejack_core::rpc::wire::Body::Result(value) = hello.body else {
        panic!("hello.result.json is an error");
    };
    let hello: HelloResult = serde_json::from_value(value).unwrap();
    assert_eq!(hello.protocol_version, PROTOCOL_VERSION);

    let doctor: Response =
        serde_json::from_str(&fs::read_to_string(dir.join("host.doctor.result.json")).unwrap())
            .unwrap();
    let applejack_core::rpc::wire::Body::Result(value) = doctor.body else {
        panic!("host.doctor.result.json is an error");
    };
    let facts: HostFacts = serde_json::from_value(value).unwrap();
    assert_eq!(facts.screen_recording, ScreenRecordingState::Unknown);
}
