use std::process::Command;

const SUBCOMMANDS: [&str; 9] = [
    "engine", "steam", "install", "run", "config", "prefix", "runtime", "serve", "doctor",
];

const STUBS: [&str; 8] = [
    "engine", "steam", "install", "run", "config", "prefix", "runtime", "serve",
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
            help.lines().any(|line| line.trim_start().starts_with(name)),
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
        let output = applejack().arg(name).output().expect("runs");
        assert_eq!(output.status.code(), Some(69), "`{name}` exit code");
        let stderr = String::from_utf8(output.stderr).expect("utf-8");
        assert_eq!(stderr.trim(), format!("applejack {name}: not implemented"));
    }
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
