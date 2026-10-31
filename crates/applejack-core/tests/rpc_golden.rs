//! Goldens under `tests/rpc_golden/` and the method and error-code tables in
//! `docs/rpc-protocol.md` are generated from the Rust types and must match
//! what is committed. `APPLEJACK_UPDATE_GOLDENS=1` rewrites both.

use applejack_core::config::{GameConfig, GraphicsBackend, SyncMode};
use applejack_core::host::{HostFacts, ScreenRecordingState};
use applejack_core::rpc::PROTOCOL_VERSION;
use applejack_core::rpc::jobs::{JobDone, JobFailed, JobId, JobProgress, JobStarted};
use applejack_core::rpc::methods::{
    ConfigExportResult, ConfigImportResult, CpuidProbeResult, D3dmetalImportResult, EmptyResult,
    EngineEntry, EngineListResult, HelloResult, Method, MethodStatus, SnapshotResult,
};
use applejack_core::rpc::wire::{ErrorCode, Id, Notification, Request, Response};
use applejack_engine::rosetta_wine::{CpuidFeatures, CpuidProbe};
use applejack_engine::{DoctorReport, Finding, Severity};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const UPDATE_ENV: &str = "APPLEJACK_UPDATE_GOLDENS";

/// `docs/rpc-protocol.md`, relative to this crate.
const DOC_PATH: &str = "../../docs/rpc-protocol.md";
const TABLE_BEGIN: &str = "<!-- method-table:begin -->";
const TABLE_END: &str = "<!-- method-table:end -->";
const ERROR_TABLE_BEGIN: &str = "<!-- error-table:begin -->";
const ERROR_TABLE_END: &str = "<!-- error-table:end -->";

/// Both table tests may rewrite the docs file; one at a time.
static DOC_LOCK: Mutex<()> = Mutex::new(());

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
    let empty = Response::result(
        Id::Number(3),
        serde_json::to_value(EmptyResult {}).expect("encodes"),
    );
    files.insert("empty.result.json".to_owned(), pretty(&empty));
    results(&mut files);
    notifications(&mut files);
    let codes: BTreeMap<&str, i64> = ErrorCode::ALL
        .into_iter()
        .map(|code| (code.name(), code.code()))
        .collect();
    files.insert("error_codes.json".to_owned(), pretty(&codes));
    files
}

fn result_response<T: serde::Serialize>(value: &T) -> Response {
    Response::result(Id::Number(4), serde_json::to_value(value).expect("encodes"))
}

/// The results the shell decodes; `GoldenTests` reads the same files.
fn results(files: &mut BTreeMap<String, String>) {
    let mut add = |name: &str, response: &Response| {
        files.insert(format!("{name}.result.json"), pretty(response));
    };
    add(
        "engine.list",
        &result_response(&EngineListResult {
            engines: vec![EngineEntry {
                engine_id: "rosetta-wine".to_owned(),
                version: "11.0-1".to_owned(),
                installed: true,
                active: true,
            }],
        }),
    );
    add(
        "engine.install",
        &result_response(&JobStarted { job_id: JobId(7) }),
    );
    add(
        "engine.doctor",
        &result_response(&DoctorReport {
            runnable: false,
            findings: vec![Finding {
                severity: Severity::Error,
                message: "engine rosetta-wine is not installed".to_owned(),
            }],
        }),
    );
    let features = CpuidFeatures {
        avx: false,
        avx2: false,
        osxsave: true,
        xcr0_ymm: false,
    };
    add(
        "engine.probe_cpuid",
        &result_response(&CpuidProbeResult {
            probe: CpuidProbe {
                plain: features,
                advertised: CpuidFeatures {
                    avx: true,
                    ..features
                },
            },
            changes_bits: true,
        }),
    );
    add(
        "engine.d3dmetal.import",
        &result_response(&D3dmetalImportResult {
            engine_version: "11.0-1".to_owned(),
            overlay_dir: PathBuf::from("/data/engines/rosetta-wine/11.0-1.user/wine/lib/external"),
            imported_from: PathBuf::from("/Volumes/Game Porting Toolkit"),
            gptk_version: "3.0".to_owned(),
            accepted_at: "2026-10-31T12:00:00Z".to_owned(),
        }),
    );
    add(
        "prefix.snapshot",
        &result_response(&SnapshotResult {
            prefix: "220".to_owned(),
            timestamp: "20261031T120000Z".to_owned(),
            dir: PathBuf::from("/data/prefixes/.snapshots/220/20261031T120000Z"),
        }),
    );
    add(
        "config.get",
        &result_response(&GameConfig {
            graphics_backend: Some(GraphicsBackend::Dxmt),
            sync_mode: Some(SyncMode::Disabled),
            hidraw_enabled: Some(true),
            ..GameConfig::default()
        }),
    );
    add(
        "config.export",
        &result_response(&ConfigExportResult {
            path: PathBuf::from("/tmp/220.preset.json"),
        }),
    );
    add(
        "config.import",
        &result_response(&ConfigImportResult {
            applied: false,
            preset_appid: 220,
            accepted: vec![applejack_core::config::Accepted {
                field: "launch_exe_override".to_owned(),
                value: "game.exe".to_owned(),
            }],
            rejected: vec![applejack_core::config::Rejected {
                field: "extra_env.PATH".to_owned(),
                value: "/tmp".to_owned(),
                reason: "host-owned".to_owned(),
            }],
            confirmations: vec![applejack_core::config::Confirmation {
                field: "launch_exe_override".to_owned(),
                value: "game.exe".to_owned(),
            }],
        }),
    );
}

fn notifications(files: &mut BTreeMap<String, String>) {
    let mut add = |notification: &Notification| {
        files.insert(
            format!("{}.notification.json", notification.method),
            pretty(notification),
        );
    };
    add(&Notification::from(&JobProgress {
        job_id: JobId(7),
        phase: "pack".to_owned(),
        done_bytes: Some(1024),
        total_bytes: None,
        message: "packing the bundle directory".to_owned(),
    }));
    add(&Notification::from(&JobDone { job_id: JobId(7) }));
    add(&Notification::from(&JobFailed {
        job_id: JobId(7),
        code: ErrorCode::ChecksumMismatch,
        message: "archive SHA-256 mismatch".to_owned(),
    }));
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

/// The error-code table as `docs/rpc-protocol.md` carries it between the
/// markers.
fn render_error_table() -> String {
    let mut table = String::from("| Code | Name | Meaning |\n|---:|---|---|\n");
    for code in ErrorCode::ALL {
        writeln!(
            table,
            "| {} | `{}` | {} |",
            code.code(),
            code.name(),
            code.description()
        )
        .expect("string write");
    }
    table
}

/// Checks (or, under `APPLEJACK_UPDATE_GOLDENS=1`, rewrites) the table between
/// `begin` and `end` in the docs.
fn assert_doc_table(what: &str, begin: &str, end: &str, expected: &str) {
    let _guard = DOC_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(DOC_PATH);
    let doc = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let start = doc.find(begin).expect("begin marker in docs") + begin.len();
    let stop = doc.find(end).expect("end marker in docs");
    assert!(start <= stop, "{what} table markers are out of order");
    let actual = doc[start..stop].trim();
    if actual == expected.trim() {
        return;
    }
    if update_requested() {
        let rewritten = format!("{}\n{expected}{}", &doc[..start], &doc[stop..]);
        fs::write(&path, rewritten).expect("docs written");
        return;
    }
    panic!(
        "{} {what} table drifted from the types (set {UPDATE_ENV}=1 to rewrite):\n{}",
        path.display(),
        diff(expected.trim(), actual)
    );
}

#[test]
fn docs_method_table_matches_the_types() {
    assert_doc_table("method", TABLE_BEGIN, TABLE_END, &render_method_table());
}

#[test]
fn docs_error_table_matches_the_types() {
    assert_doc_table(
        "error-code",
        ERROR_TABLE_BEGIN,
        ERROR_TABLE_END,
        &render_error_table(),
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

    let empty: Response =
        serde_json::from_str(&fs::read_to_string(dir.join("empty.result.json")).unwrap()).unwrap();
    let applejack_core::rpc::wire::Body::Result(value) = empty.body else {
        panic!("empty.result.json is an error");
    };
    assert_eq!(value, serde_json::json!({}));
    let _: EmptyResult = serde_json::from_value(value).unwrap();
}

fn result_value(name: &str) -> serde_json::Value {
    let path = golden_dir().join(format!("{name}.result.json"));
    let response: Response = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    match response.body {
        applejack_core::rpc::wire::Body::Result(value) => value,
        applejack_core::rpc::wire::Body::Error(error) => panic!("{name}: {error}"),
    }
}

#[test]
fn result_goldens_for_engine_and_config_methods_decode() {
    let list: EngineListResult = serde_json::from_value(result_value("engine.list")).unwrap();
    assert!(list.engines[0].active);
    let _: JobStarted = serde_json::from_value(result_value("engine.install")).unwrap();
    let doctor: DoctorReport = serde_json::from_value(result_value("engine.doctor")).unwrap();
    assert!(!doctor.runnable);
    let cpuid: CpuidProbeResult =
        serde_json::from_value(result_value("engine.probe_cpuid")).unwrap();
    assert!(cpuid.changes_bits);
    let _: D3dmetalImportResult =
        serde_json::from_value(result_value("engine.d3dmetal.import")).unwrap();
    let _: SnapshotResult = serde_json::from_value(result_value("prefix.snapshot")).unwrap();
    let _: GameConfig = serde_json::from_value(result_value("config.get")).unwrap();
    let _: ConfigExportResult = serde_json::from_value(result_value("config.export")).unwrap();
    let import: ConfigImportResult = serde_json::from_value(result_value("config.import")).unwrap();
    assert!(!import.applied);
}

#[test]
fn error_codes_golden_lists_every_code() {
    let text = fs::read_to_string(golden_dir().join("error_codes.json")).unwrap();
    let codes: BTreeMap<String, i64> = serde_json::from_str(&text).unwrap();
    assert_eq!(codes.len(), ErrorCode::ALL.len());
    for code in ErrorCode::ALL {
        assert_eq!(
            codes.get(code.name()),
            Some(&code.code()),
            "{}",
            code.name()
        );
    }
}
