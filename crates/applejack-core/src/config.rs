//! Data directory resolution, the database handle, and the default
//! environment policy that presets import through. Re-exports the
//! `applejack-config` types the CLI and the RPC layer use.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use applejack_engine::launch::env::{ALLOWLIST, Entry, Kind, Owner};

pub use applejack_config::presets::{
    self, Accepted, Confirmation, DATA_DIR_PLACEHOLDER, ENGINE_OWNED_NAMES, HOME_PLACEHOLDER,
    HOST_OWNED_NAMES, LOADER_PREFIXES, META_HOST_CHIP, META_MACOS_VERSION, Rejected,
    SCHEMA_VERSION, SECRET_NAME_FRAGMENTS, ValueCheck,
};
pub use applejack_config::{
    AppRow, ConfigDb, Drm, EnvPolicy, Error, Field, GameConfig, GraphicsBackend, ImportOptions,
    ImportPlan, LaunchMode, Preset, Resolved, SyncMode, resolve,
};

/// Environment variable that overrides the data directory.
pub const DATA_DIR_ENV: &str = "APPLEJACK_DATA_DIR";

const DATA_DIR_RELATIVE: &str = "Library/Application Support/applejack";

/// File name of the configuration database inside the data directory.
pub const DB_FILE: &str = "db.sqlite";

/// Why the data directory cannot be determined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataDirError {
    /// `HOME` is unset, empty, or relative and no override is given.
    HomeUnset,
    /// `APPLEJACK_DATA_DIR` is set but not an absolute path.
    RelativeOverride(PathBuf),
}

impl fmt::Display for DataDirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HomeUnset => write!(
                f,
                "HOME is unset, empty, or relative and {DATA_DIR_ENV} is not set"
            ),
            Self::RelativeOverride(path) => write!(
                f,
                "{DATA_DIR_ENV} must be an absolute path, got {}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for DataDirError {}

/// The data directory: `APPLEJACK_DATA_DIR` when set and absolute, else
/// `$HOME/Library/Application Support/applejack`.
///
/// # Errors
///
/// Returns [`DataDirError`] when neither source yields an absolute path.
pub fn data_dir() -> Result<PathBuf, DataDirError> {
    data_dir_from(|name| std::env::var_os(name))
}

/// [`data_dir`] over an explicit environment lookup, so tests never touch the
/// process environment.
///
/// # Errors
///
/// Same as [`data_dir`].
pub fn data_dir_from(env: impl Fn(&str) -> Option<OsString>) -> Result<PathBuf, DataDirError> {
    if let Some(value) = env(DATA_DIR_ENV).filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        return if path.is_absolute() {
            Ok(path)
        } else {
            Err(DataDirError::RelativeOverride(path))
        };
    }
    home_dir_from(env)
        .map(|home| home.join(DATA_DIR_RELATIVE))
        .ok_or(DataDirError::HomeUnset)
}

/// The home directory from an explicit environment lookup: `HOME` when set
/// and absolute. Presets rewrite paths under it to `$HOME` on export and
/// expand them on import.
pub fn home_dir_from(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    env("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
}

/// Opens the configuration database under `data_dir`, creating it on first use.
///
/// # Errors
///
/// Returns the errors of [`ConfigDb::open`].
pub fn open_db(data_dir: &Path) -> Result<ConfigDb, Error> {
    ConfigDb::open(&data_dir.join(DB_FILE))
}

/// Set to `1` to allow `engine install --from`: an unsigned local bundle.
/// Debug builds only; a release build refuses the variable.
pub const ALLOW_UNSIGNED_ENV: &str = "APPLEJACK_ALLOW_UNSIGNED_ENGINE";

/// Whether this process may install an unsigned local bundle.
pub fn allow_unsigned_from(env: impl Fn(&str) -> Option<OsString>) -> bool {
    cfg!(debug_assertions) && env(ALLOW_UNSIGNED_ENV).is_some_and(|value| value == "1")
}

const LOG_DIR_RELATIVE: &str = "Library/Logs/applejack";

/// The log root. A data directory override moves it to `<data_dir>/logs`,
/// so a throwaway data directory never writes to the user's real logs.
#[must_use]
pub fn log_dir_from(data_dir: &Path, env: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    let overridden = env(DATA_DIR_ENV).is_some_and(|value| !value.is_empty());
    if overridden {
        return data_dir.join("logs");
    }
    home_dir_from(env).map_or_else(|| data_dir.join("logs"), |home| home.join(LOG_DIR_RELATIVE))
}

/// The engine's environment allowlist as an [`EnvPolicy`]: a preset may set
/// exactly the game-owned rows of [`ALLOWLIST`], with the same value kinds,
/// so the engine's table is the one list of names.
#[derive(Debug, Clone)]
pub struct DefaultEnvPolicy {
    /// Root for ICD, plugin, and DXMT config paths: the engine payloads and
    /// their `<version>.user` overlays both live under `engines/`.
    engines: Vec<PathBuf>,
}

impl DefaultEnvPolicy {
    /// Policy rooted at `data_dir`'s `engines/` directory.
    #[must_use]
    pub fn new(data_dir: &Path) -> Self {
        Self {
            engines: vec![data_dir.join("engines")],
        }
    }
}

fn allowlist_row(name: &str) -> Option<&'static Entry> {
    ALLOWLIST.iter().find(|entry| entry.name == name)
}

impl EnvPolicy for DefaultEnvPolicy {
    fn allows(&self, name: &str) -> bool {
        allowlist_row(name).is_some_and(|entry| entry.owner == Owner::Game)
    }

    fn value_check(&self, name: &str) -> ValueCheck<'_> {
        match allowlist_row(name).map(|entry| entry.kind) {
            Some(Kind::Path) => ValueCheck::Path(&self.engines),
            Some(Kind::PathList) => ValueCheck::PathList(&self.engines),
            _ => ValueCheck::Plain,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        }
    }

    fn preset_with(config: GameConfig) -> Preset {
        Preset {
            schema: SCHEMA_VERSION,
            appid: 1,
            engine_id: None,
            engine_version: None,
            host_chip: None,
            macos_version: None,
            config,
        }
    }

    fn preset_with_env(entries: &[(&str, &str)]) -> Preset {
        let mut config = GameConfig::default();
        for (name, value) in entries {
            config
                .extra_env
                .insert((*name).to_owned(), (*value).to_owned());
        }
        preset_with(config)
    }

    fn rejected_names(plan: &ImportPlan) -> Vec<&str> {
        let mut names: Vec<&str> = plan
            .rejected
            .iter()
            .map(|item| item.field.trim_start_matches("extra_env."))
            .collect();
        names.sort_unstable();
        names
    }

    #[test]
    fn absolute_override_is_used_verbatim() {
        let dir = data_dir_from(env_of(&[
            ("APPLEJACK_DATA_DIR", "/tmp/aj"),
            ("HOME", "/Users/x"),
        ]));
        assert_eq!(dir, Ok(PathBuf::from("/tmp/aj")));
    }

    #[test]
    fn relative_override_is_rejected() {
        let dir = data_dir_from(env_of(&[
            ("APPLEJACK_DATA_DIR", "rel/dir"),
            ("HOME", "/Users/x"),
        ]));
        assert_eq!(
            dir,
            Err(DataDirError::RelativeOverride(PathBuf::from("rel/dir")))
        );
        assert!(dir.unwrap_err().to_string().contains("absolute"));
    }

    #[test]
    fn empty_override_falls_back_to_home() {
        let dir = data_dir_from(env_of(&[("APPLEJACK_DATA_DIR", ""), ("HOME", "/Users/x")]));
        assert_eq!(
            dir,
            Ok(PathBuf::from(
                "/Users/x/Library/Application Support/applejack"
            ))
        );
    }

    #[test]
    fn missing_or_empty_home_is_an_error_not_a_panic() {
        assert_eq!(data_dir_from(env_of(&[])), Err(DataDirError::HomeUnset));
        assert_eq!(
            data_dir_from(env_of(&[("HOME", "")])),
            Err(DataDirError::HomeUnset)
        );
        assert_eq!(
            data_dir_from(env_of(&[("HOME", "relative")])),
            Err(DataDirError::HomeUnset)
        );
    }

    #[test]
    fn home_dir_requires_an_absolute_home() {
        assert_eq!(
            home_dir_from(env_of(&[("HOME", "/Users/x")])),
            Some(PathBuf::from("/Users/x"))
        );
        assert_eq!(home_dir_from(env_of(&[("HOME", "relative")])), None);
        assert_eq!(home_dir_from(env_of(&[])), None);
    }

    #[test]
    fn unsigned_installs_need_the_variable_set_to_1() {
        assert_eq!(
            allow_unsigned_from(env_of(&[(ALLOW_UNSIGNED_ENV, "1")])),
            cfg!(debug_assertions),
            "a release build refuses the variable"
        );
        assert!(!allow_unsigned_from(env_of(&[(ALLOW_UNSIGNED_ENV, "0")])));
        assert!(!allow_unsigned_from(env_of(&[])));
    }

    #[test]
    fn a_data_dir_override_keeps_logs_out_of_the_real_log_root() {
        let data = Path::new("/tmp/aj");
        let overridden = env_of(&[("APPLEJACK_DATA_DIR", "/tmp/aj"), ("HOME", "/Users/x")]);
        assert_eq!(log_dir_from(data, overridden), data.join("logs"));
        assert_eq!(
            log_dir_from(data, env_of(&[("HOME", "/Users/x")])),
            Path::new("/Users/x/Library/Logs/applejack")
        );
    }

    /// The engine's table is the source: every game row is importable and
    /// nothing else is, so a new row cannot miss the policy.
    #[test]
    fn the_policy_is_the_game_tier_of_the_engine_allowlist() {
        let policy = DefaultEnvPolicy::new(Path::new("/data"));
        for entry in ALLOWLIST {
            assert_eq!(
                policy.allows(entry.name),
                entry.owner == Owner::Game,
                "{}",
                entry.name
            );
            if entry.owner == Owner::Game {
                assert!(!ENGINE_OWNED_NAMES.contains(&entry.name), "{}", entry.name);
                assert!(!HOST_OWNED_NAMES.contains(&entry.name), "{}", entry.name);
            }
        }
    }

    #[test]
    fn default_policy_matches_the_plan_allowlist() {
        let policy = DefaultEnvPolicy::new(Path::new("/data"));
        for name in [
            "WINEDEBUG",
            "VK_ICD_FILENAMES",
            "DXMT_CONFIG",
            "DXMT_CONFIG_FILE",
            "GST_PLUGIN_PATH",
            "MTL_HUD_ENABLED",
        ] {
            assert!(policy.allows(name), "{name}");
        }
        for name in [
            "DYLD_INSERT_LIBRARIES",
            "LD_PRELOAD",
            "WINEPREFIX",
            "WINEDLLOVERRIDES",
            "WINEMSYNC",
            "WINEESYNC",
            "PATH",
            "HOME",
            "TMPDIR",
            "APPLEJACK_SESSION",
        ] {
            assert!(!policy.allows(name), "{name}");
        }
        let engines = [PathBuf::from("/data/engines")];
        assert_eq!(
            policy.value_check("DXMT_CONFIG_FILE"),
            ValueCheck::Path(&engines)
        );
        for name in ["VK_ICD_FILENAMES", "GST_PLUGIN_PATH"] {
            assert_eq!(
                policy.value_check(name),
                ValueCheck::PathList(&engines),
                "{name}"
            );
        }
        assert_eq!(policy.value_check("WINEDEBUG"), ValueCheck::Plain);
        assert_eq!(
            policy.value_check("DXMT_CONFIG"),
            ValueCheck::Plain,
            "DXMT_CONFIG is an option string; DXMT_CONFIG_FILE is the path"
        );
    }

    #[test]
    fn default_policy_import_rejects_host_owned_engine_owned_and_escaping_paths() {
        let data = Path::new("/data");
        let preset = preset_with_env(&[
            ("PATH", "/tmp/evil:/usr/bin"),
            ("HOME", "/tmp"),
            ("TMPDIR", "/tmp"),
            ("APPLEJACK_SESSION", "x"),
            ("dyld_insert_libraries", "/x.dylib"),
            ("LD_PRELOAD", "/x.so"),
            (
                "VK_ICD_FILENAMES",
                "/data/engines/rosetta-wine/1.0/icd.json:/tmp/evil.json",
            ),
            ("GST_PLUGIN_PATH", "/data/engines-evil/plugins"),
            ("WINEPREFIX", "/data/prefixes/game"),
            ("WINEDLLOVERRIDES", "ntdll=n"),
            ("WINEDEBUG", "-all"),
            ("DXMT_CONFIG", "d3d11.preferredMaxFrameRate=60"),
        ]);
        let policy = DefaultEnvPolicy::new(data);
        let db = ConfigDb::open_in_memory().unwrap();
        let plan =
            presets::import(&preset, &policy, &db, data, None, ImportOptions::default()).unwrap();
        assert_eq!(
            rejected_names(&plan),
            [
                "APPLEJACK_SESSION",
                "GST_PLUGIN_PATH",
                "HOME",
                "LD_PRELOAD",
                "PATH",
                "TMPDIR",
                "VK_ICD_FILENAMES",
                "WINEDLLOVERRIDES",
                "WINEPREFIX",
                "dyld_insert_libraries",
            ]
        );
        for name in ["WINEPREFIX", "WINEDLLOVERRIDES"] {
            let item = plan
                .rejected
                .iter()
                .find(|item| item.field == format!("extra_env.{name}"))
                .unwrap();
            assert!(item.reason.contains("set by the engine"), "{item:?}");
        }
        assert_eq!(
            plan.config.extra_env.keys().collect::<Vec<_>>(),
            ["DXMT_CONFIG", "WINEDEBUG"]
        );
    }

    #[test]
    fn path_checks_depend_on_the_variable_name() {
        let data = Path::new("/data");
        let policy = DefaultEnvPolicy::new(data);
        let db = ConfigDb::open_in_memory().unwrap();
        let crossed = preset_with_env(&[
            ("VK_ICD_FILENAMES", "/data/prefixes/g/drive_c/evil.json"),
            ("GST_PLUGIN_PATH", "/data/prefixes/g/drive_c/evil"),
            ("DXMT_CONFIG_FILE", "/data/prefixes/g/dxmt.conf"),
        ]);
        let plan =
            presets::import(&crossed, &policy, &db, data, None, ImportOptions::default()).unwrap();
        assert_eq!(
            rejected_names(&plan),
            ["DXMT_CONFIG_FILE", "GST_PLUGIN_PATH", "VK_ICD_FILENAMES"]
        );
        for item in &plan.rejected {
            assert!(item.reason.contains("outside"), "{item:?}");
        }
        assert_eq!(plan.config.extra_env, BTreeMap::new());

        let listed = preset_with_env(&[(
            "DXMT_CONFIG_FILE",
            "/data/engines/rosetta-wine/1.0.user/a.conf:/data/engines/rosetta-wine/1.0.user/b.conf",
        )]);
        let plan =
            presets::import(&listed, &policy, &db, data, None, ImportOptions::default()).unwrap();
        assert_eq!(rejected_names(&plan), ["DXMT_CONFIG_FILE"]);
        assert!(
            plan.rejected[0].reason.contains("one path"),
            "{:?}",
            plan.rejected
        );

        let own = preset_with_env(&[
            (
                "VK_ICD_FILENAMES",
                "/data/engines/rosetta-wine/1.0/icd.json",
            ),
            (
                "GST_PLUGIN_PATH",
                "/data/engines/rosetta-wine/1.0/plugins:/data/engines/rosetta-wine/1.0.user/plugins",
            ),
            (
                "DXMT_CONFIG_FILE",
                "/data/engines/rosetta-wine/1.0.user/dxmt.conf",
            ),
        ]);
        let plan =
            presets::import(&own, &policy, &db, data, None, ImportOptions::default()).unwrap();
        assert!(plan.rejected.is_empty(), "{:?}", plan.rejected);
        assert_eq!(plan.config.extra_env.len(), 3);
    }

    #[test]
    fn d3dmetal_gate_reads_the_database() {
        let data = Path::new("/data");
        let policy = DefaultEnvPolicy::new(data);
        let db = ConfigDb::open_in_memory().unwrap();
        let preset = preset_with(GameConfig {
            engine_version: Some("1.0.0".to_owned()),
            graphics_backend: Some(GraphicsBackend::D3dmetal),
            ..GameConfig::default()
        });
        let plan =
            presets::import(&preset, &policy, &db, data, None, ImportOptions::default()).unwrap();
        assert_eq!(rejected_names(&plan), ["graphics_backend"]);
        assert!(
            plan.rejected[0].reason.contains("1.0.0"),
            "{:?}",
            plan.rejected
        );
        assert_eq!(plan.config.graphics_backend, None);

        db.d3dmetal_accept("1.0.0", "/tmp/gptk.dmg", "2.1", "2026-10-29T00:00:00Z")
            .unwrap();
        let plan =
            presets::import(&preset, &policy, &db, data, None, ImportOptions::default()).unwrap();
        assert!(plan.rejected.is_empty(), "{:?}", plan.rejected);
        assert_eq!(
            plan.config.graphics_backend,
            Some(GraphicsBackend::D3dmetal)
        );
    }
}
