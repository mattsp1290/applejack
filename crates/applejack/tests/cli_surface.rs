use std::process::Command;

const SUBCOMMANDS: [&str; 9] = [
    "engine", "steam", "install", "run", "config", "prefix", "runtime", "serve", "doctor",
];

const STUBS: [&str; 7] = [
    "engine", "steam", "install", "run", "config", "prefix", "runtime",
];

const DOCTOR_KEYS: [&str; 6] = [
    "macos_version",
    "arch",
    "rosetta_present",
    "free_disk_bytes",
    "screen_recording",
    "developer_id_identities",
];

fn applejack() -> Command {
    Command::new(env!("CARGO_BIN_EXE_applejack"))
}

#[test]
fn help_lists_every_subcommand() {
    let output = applejack().arg("--help").output().expect("runs");
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).expect("utf-8");
    for name in SUBCOMMANDS {
        assert!(
            help.lines()
                .any(|line| line.split_whitespace().next() == Some(name)),
            "--help does not list `{name}`:\n{help}"
        );
    }
}

#[test]
fn version_prints_crate_version() {
    let output = applejack().arg("--version").output().expect("runs");
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).expect("utf-8");
    assert_eq!(
        text.trim(),
        format!("applejack {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn stubs_exit_with_ex_unavailable() {
    for name in STUBS {
        assert_not_implemented(name, &[]);
    }
}

#[test]
fn stubs_ignore_trailing_arguments() {
    assert_not_implemented("prefix", &["snapshot"]);
    assert_not_implemented("steam", &["login"]);
}

fn assert_not_implemented(name: &str, args: &[&str]) {
    let output = applejack().arg(name).args(args).output().expect("runs");
    assert_eq!(
        output.status.code(),
        Some(69),
        "`{name} {}` exit code",
        args.join(" ")
    );
    let stderr = String::from_utf8(output.stderr).expect("utf-8");
    assert_eq!(stderr.trim(), format!("applejack {name}: not implemented"));
}

#[test]
fn doctor_rejects_unknown_flags_with_usage_error() {
    let output = applejack()
        .args(["doctor", "--bogus"])
        .output()
        .expect("runs");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn doctor_json_has_expected_keys() {
    let output = applejack()
        .args(["doctor", "--json"])
        .output()
        .expect("runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let facts: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let object = facts.as_object().expect("JSON object");
    for key in DOCTOR_KEYS {
        assert!(object.contains_key(key), "missing key `{key}` in {facts}");
    }
    assert_eq!(
        object.len(),
        DOCTOR_KEYS.len(),
        "unexpected keys in {facts}"
    );
    assert_eq!(facts["screen_recording"], "unknown");
}

const HELLO: &str = r#"{"jsonrpc":"2.0","id":1,"method":"hello","params":{"protocol_version":1}}"#;
const DOCTOR: &str = r#"{"jsonrpc":"2.0","id":2,"method":"host.doctor","params":{}}"#;

/// Runs `serve --stdio` with `input` on stdin; returns (exit code, stdout lines, stderr).
fn serve(input: &str, extra: &[&str]) -> (Option<i32>, Vec<serde_json::Value>, String) {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = applejack()
        .args(["serve", "--stdio"])
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawns");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input.as_bytes())
        .expect("writes");
    let output = child.wait_with_output().expect("exits");
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    let responses = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
        .collect();
    (
        output.status.code(),
        responses,
        String::from_utf8(output.stderr).expect("utf-8"),
    )
}

#[test]
fn serve_without_stdio_exits_2_with_usage() {
    let output = applejack().arg("serve").output().expect("runs");
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(output.stdout, b"");
    let stderr = String::from_utf8(output.stderr).expect("utf-8");
    assert!(
        stderr.contains("usage: applejack serve --stdio"),
        "{stderr}"
    );
}

#[test]
fn serve_stdio_answers_hello_and_host_doctor_then_exits_0() {
    let (code, responses, stderr) = serve(&format!("{HELLO}\n{DOCTOR}\n"), &[]);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["id"], 1);
    assert_eq!(responses[0]["result"]["protocol_version"], 1);
    assert_eq!(responses[1]["id"], 2);
    assert!(responses[1]["result"]["macos_version"].is_string());
    assert!(
        stderr.is_empty(),
        "stderr is silent without --log: {stderr}"
    );
}

#[test]
fn serve_stdio_logs_to_stderr_only_with_log_flag() {
    let (code, responses, stderr) = serve(&format!("{HELLO}\n{DOCTOR}\n"), &["--log"]);
    assert_eq!(code, Some(0));
    assert_eq!(responses.len(), 2, "log lines must not reach stdout");
    assert!(stderr.contains("-> hello"), "{stderr}");
    assert!(stderr.contains("<- host.doctor ok"), "{stderr}");
}

#[test]
fn serve_stdio_exits_non_zero_on_protocol_mismatch() {
    let mismatch = r#"{"jsonrpc":"2.0","id":1,"method":"hello","params":{"protocol_version":99}}"#;
    let (code, responses, _) = serve(&format!("{mismatch}\n{DOCTOR}\n"), &[]);
    assert_eq!(code, Some(65));
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["error"]["code"], 1000);
}
