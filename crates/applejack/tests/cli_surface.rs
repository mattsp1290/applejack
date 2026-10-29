use std::process::Command;

const SUBCOMMANDS: [&str; 9] = [
    "engine", "steam", "install", "run", "config", "prefix", "runtime", "serve", "doctor",
];

const STUBS: [&str; 7] = [
    "engine", "steam", "install", "run", "prefix", "runtime", "serve",
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
    assert_not_implemented("serve", &["--stdio"]);
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
fn config_rejects_usage_errors() {
    for args in [
        vec!["config"],
        vec!["config", "10"],
        vec!["config", "ten", "show"],
        vec!["config", "10", "bogus"],
        vec!["config", "10", "set", "hidraw_enabled"],
        vec!["config", "10", "show", "--bogus"],
    ] {
        let output = applejack().args(&args).output().expect("runs");
        assert_eq!(
            output.status.code(),
            Some(2),
            "`{}` exit code",
            args.join(" ")
        );
    }
}

#[test]
fn config_show_on_empty_data_dir_prints_empty_config() {
    let data_dir = TempDataDir::new("config-show");
    let output = applejack()
        .env("APPLEJACK_DATA_DIR", &data_dir.path)
        .args(["config", "0", "show"])
        .output()
        .expect("runs");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).expect("utf-8");
    assert!(text.contains("hidraw_enabled"), "{text}");
    assert!(
        !text.lines().any(|line| !line.ends_with(" = (unset)")),
        "{text}"
    );

    let output = applejack()
        .env("APPLEJACK_DATA_DIR", &data_dir.path)
        .args(["config", "0", "show", "--json"])
        .output()
        .expect("runs");
    assert!(output.status.success());
    let config: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert!(config["hidraw_enabled"].is_null(), "{config}");
    assert_eq!(config["extra_env"], serde_json::json!({}));
    assert!(
        !data_dir.path.join("db.sqlite").exists(),
        "a read created the database"
    );
}

#[test]
fn config_import_that_accepts_nothing_creates_no_database() {
    let data_dir = TempDataDir::new("config-import-nothing");
    std::fs::create_dir_all(&data_dir.path).expect("creates temp dir");
    let preset = data_dir.path.join("preset.json");
    std::fs::write(
        &preset,
        r#"{"schema": 1, "appid": 11,
            "config": {"extra_env": {"WINEPREFIX": "$APPLEJACK_DATA_DIR/prefixes/g"},
                       "direct_verified_at": "2026-10-29T00:00:00Z"}}"#,
    )
    .expect("writes preset");
    let output = config(
        &data_dir.path,
        &["11", "import", preset.to_str().expect("utf-8")],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = String::from_utf8(output.stdout).expect("utf-8");
    assert_eq!(
        report
            .lines()
            .filter(|line| line.starts_with("reject"))
            .count(),
        2,
        "{report}"
    );
    assert!(report.contains("nothing to apply to appid 11"), "{report}");
    assert!(
        !data_dir.path.join("db.sqlite").exists(),
        "an import that accepted nothing created the database"
    );
}

#[test]
fn config_rejects_relative_data_dir() {
    let output = applejack()
        .env("APPLEJACK_DATA_DIR", "relative/dir")
        .args(["config", "0", "show"])
        .output()
        .expect("runs");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("utf-8");
    assert!(stderr.contains("APPLEJACK_DATA_DIR"), "{stderr}");
}

fn config(data_dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    applejack()
        .env("APPLEJACK_DATA_DIR", data_dir)
        .arg("config")
        .args(args)
        .output()
        .expect("runs")
}

#[test]
fn config_set_errors_exit_1_without_creating_the_database() {
    let data_dir = TempDataDir::new("config-set-errors");
    for args in [
        ["10", "set", "colour", "blue"],
        ["10", "set", "hidraw_enabled", "maybe"],
    ] {
        let output = config(&data_dir.path, &args);
        assert_eq!(output.status.code(), Some(1), "{}", args.join(" "));
    }
    assert!(!data_dir.path.join("db.sqlite").exists());
}

#[test]
fn config_set_accepts_values_starting_with_a_hyphen() {
    let data_dir = TempDataDir::new("config-hyphen");
    let output = config(
        &data_dir.path,
        &["10", "set", "launch_args_override", "-windowed"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = config(&data_dir.path, &["10", "show", "--json"]);
    let shown: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert_eq!(shown["launch_args_override"], "-windowed");
}

#[test]
fn config_import_needing_confirmation_exits_65_and_writes_nothing() {
    let data_dir = TempDataDir::new("config-import");
    std::fs::create_dir_all(&data_dir.path).expect("creates temp dir");
    let preset = data_dir.path.join("preset.json");
    std::fs::write(
        &preset,
        r#"{"schema": 1, "appid": 99, "engine_id": null, "engine_version": null,
            "host_chip": null, "macos_version": null,
            "config": {"hidraw_enabled": true, "launch_exe_override": "/bin/sh"}}"#,
    )
    .expect("writes preset");
    let preset = preset.to_str().expect("utf-8 path");

    let output = config(&data_dir.path, &["10", "import", preset]);
    assert_eq!(output.status.code(), Some(65));
    assert!(
        !data_dir.path.join("db.sqlite").exists(),
        "an unconfirmed import created the database"
    );
    let report = String::from_utf8(output.stdout).expect("utf-8");
    assert!(report.contains("confirm  launch_exe_override"), "{report}");
    assert_eq!(
        report
            .lines()
            .filter(|line| line.contains("launch_exe_override"))
            .count(),
        1,
        "listed once, as confirm:\n{report}"
    );
    assert!(!report.starts_with('\n'), "{report:?}");
    let stderr = String::from_utf8(output.stderr).expect("utf-8");
    assert!(
        stderr.contains("warning: preset was exported for appid 99, applying to 10"),
        "{stderr}"
    );

    let newer = data_dir.path.join("newer.json");
    std::fs::write(&newer, r#"{"schema": 2, "appid": 10, "config": {}}"#).expect("writes preset");
    let output = config(
        &data_dir.path,
        &["10", "import", newer.to_str().expect("utf-8")],
    );
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("utf-8");
    assert!(stderr.contains("schema version 2"), "{stderr}");
    assert!(
        !data_dir.path.join("db.sqlite").exists(),
        "a refused import created the database"
    );

    let shown = config(&data_dir.path, &["10", "show", "--json"]);
    let shown: serde_json::Value = serde_json::from_slice(&shown.stdout).expect("valid JSON");
    assert!(
        shown["hidraw_enabled"].is_null(),
        "written without confirmation: {shown}"
    );

    let output = config(
        &data_dir.path,
        &["10", "import", preset, "--accept-launch-overrides"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let shown = config(&data_dir.path, &["10", "show", "--json"]);
    let shown: serde_json::Value = serde_json::from_slice(&shown.stdout).expect("valid JSON");
    assert_eq!(shown["hidraw_enabled"], true);
}

#[test]
fn config_import_rejects_forged_report_lines_and_oversized_files() {
    let data_dir = TempDataDir::new("config-import-hygiene");
    std::fs::create_dir_all(&data_dir.path).expect("creates temp dir");
    let forged = data_dir.path.join("forged.json");
    std::fs::write(
        &forged,
        r#"{"schema": 1, "appid": 10,
            "config": {"launch_exe_override": "/bin/sh\nreject   launch_exe_override = forged",
                       "notes": "\u001b[2J"}}"#,
    )
    .expect("writes preset");
    let output = config(
        &data_dir.path,
        &["10", "import", forged.to_str().expect("utf-8")],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = String::from_utf8(output.stdout).expect("utf-8");
    let rejects: Vec<&str> = report
        .lines()
        .filter(|line| line.starts_with("reject"))
        .collect();
    assert_eq!(rejects.len(), 2, "{report}");
    assert!(
        rejects.iter().all(|line| line.contains("control")),
        "{report}"
    );
    assert!(
        !report
            .lines()
            .any(|line| line.starts_with("reject   launch_exe_override = forged")),
        "a preset value forged a report line:\n{report}"
    );
    assert!(report.contains("nothing to apply"), "{report}");

    // One byte over the cap, and over the cap with the cut inside a
    // multibyte character: the size message wins over a UTF-8 error.
    let mut cut = vec![b' '; 1 << 20];
    cut.extend_from_slice("\u{e9}".as_bytes());
    for body in [vec![b' '; (1 << 20) + 1], cut] {
        let huge = data_dir.path.join("huge.json");
        std::fs::write(&huge, body).expect("writes preset");
        let output = config(
            &data_dir.path,
            &["11", "import", huge.to_str().expect("utf-8")],
        );
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8(output.stderr).expect("utf-8");
        assert!(stderr.contains("preset limit"), "{stderr}");
    }
    let shown = config(&data_dir.path, &["11", "show", "--json"]);
    let shown: serde_json::Value = serde_json::from_slice(&shown.stdout).expect("valid JSON");
    assert!(shown["launch_exe_override"].is_null(), "{shown}");
}

#[cfg(unix)]
#[test]
fn config_import_of_an_unbounded_device_is_refused_at_the_cap() {
    let data_dir = TempDataDir::new("config-import-dev-zero");
    let started = std::time::Instant::now();
    let output = config(&data_dir.path, &["10", "import", "/dev/zero"]);
    let elapsed = started.elapsed();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).expect("utf-8");
    assert!(stderr.contains("preset limit"), "{stderr}");
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "an unbounded read would never return: took {elapsed:?}"
    );
    assert!(!data_dir.path.join("db.sqlite").exists());
}

#[test]
fn config_show_escapes_control_characters_in_values() {
    let data_dir = TempDataDir::new("config-show-escape");
    let output = config(
        &data_dir.path,
        &[
            "10",
            "set",
            "notes",
            "\u{1b}[1A\u{1b}[2K launch_exe_override (unset)\n",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = config(&data_dir.path, &["10", "show"]);
    let text = String::from_utf8(output.stdout).expect("utf-8");
    assert!(!text.contains('\u{1b}'), "{text:?}");
    assert!(text.contains("\\u{1b}[1A"), "{text}");
    assert_eq!(
        text.lines()
            .filter(|line| line.starts_with("launch_exe_override"))
            .count(),
        1,
        "{text}"
    );
}

#[test]
fn config_export_then_import_round_trips() {
    let data_dir = TempDataDir::new("config-round-trip");
    let preset = data_dir.path.join("p.json");
    let preset = preset.to_str().expect("utf-8 path");
    for args in [
        &["10", "set", "hidraw_enabled", "true"][..],
        &["10", "set", "extra_env", "WINEDEBUG=-all"],
        &["10", "set", "extra_env", "MY_TOKEN=hunter2"],
        &["10", "export", preset],
        &["11", "import", preset],
    ] {
        let output = config(&data_dir.path, args);
        assert!(
            output.status.success(),
            "`{}`: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let exported = std::fs::read_to_string(preset).expect("reads preset");
    assert!(!exported.contains("hunter2"), "{exported}");
    assert!(!exported.contains("MY_TOKEN"), "{exported}");
    let output = config(&data_dir.path, &["11", "import", preset]);
    let stderr = String::from_utf8(output.stderr).expect("utf-8");
    assert!(
        stderr.contains("warning: preset was exported for appid 10, applying to 11"),
        "{stderr}"
    );
    let shown = config(&data_dir.path, &["11", "show", "--json"]);
    let shown: serde_json::Value = serde_json::from_slice(&shown.stdout).expect("valid JSON");
    assert_eq!(shown["hidraw_enabled"], true);
    assert_eq!(shown["extra_env"], serde_json::json!({"WINEDEBUG": "-all"}));
}

struct TempDataDir {
    path: std::path::PathBuf,
}

impl TempDataDir {
    fn new(label: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        Self {
            path: std::env::temp_dir().join(format!(
                "applejack-cli-{label}-{}-{nanos}",
                std::process::id()
            )),
        }
    }
}

impl Drop for TempDataDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
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
