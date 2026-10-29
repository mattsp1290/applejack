//! Data directory resolution, the database handle, and the default
//! environment policy that presets import through. Re-exports the
//! `applejack-config` types the CLI and the RPC layer use.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

pub use applejack_config::presets::{
    self, Accepted, Confirmation, DATA_DIR_PLACEHOLDER, HOME_PLACEHOLDER, META_HOST_CHIP,
    META_MACOS_VERSION, Rejected, SCHEMA_VERSION, Warning,
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
    env("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
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

/// The engine environment allowlist from plan 02 as an [`EnvPolicy`]. The
/// engine crate supplies its own table once it lands; this one exists so
/// preset import works before then.
#[derive(Debug, Clone)]
pub struct DefaultEnvPolicy<'db> {
    /// Root for ICD, plugin, and DXMT config paths: the engine payloads and
    /// their `<version>.user` overlays both live under `engines/`.
    engines: Vec<PathBuf>,
    /// Root for `WINEPREFIX`.
    prefixes: Vec<PathBuf>,
    overlay: OverlayFact<'db>,
}

/// Where the policy learns whether a `D3DMetal` overlay is accepted.
#[derive(Debug, Clone, Copy)]
enum OverlayFact<'db> {
    /// The `d3dmetal` table of this database.
    Db(&'db ConfigDb),
    /// A fixed answer for every engine version, for tests and hosts that
    /// already know it.
    Fixed(bool),
}

const ALLOWED_NAMES: [&str; 16] = [
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

impl<'db> DefaultEnvPolicy<'db> {
    /// Policy rooted at `data_dir`'s `engines/` and `prefixes/` directories,
    /// with the `D3DMetal` overlay fact read from `db`'s `d3dmetal` table.
    #[must_use]
    pub fn new(data_dir: &Path, db: &'db ConfigDb) -> Self {
        Self::with_fact(data_dir, OverlayFact::Db(db))
    }

    /// Policy with the overlay fact supplied by the caller, for hosts that
    /// already know it.
    #[must_use]
    pub fn with_overlay(data_dir: &Path, overlay_present: bool) -> DefaultEnvPolicy<'static> {
        DefaultEnvPolicy::with_fact(data_dir, OverlayFact::Fixed(overlay_present))
    }

    fn with_fact(data_dir: &Path, overlay: OverlayFact<'db>) -> Self {
        Self {
            engines: vec![data_dir.join("engines")],
            prefixes: vec![data_dir.join("prefixes")],
            overlay,
        }
    }
}

impl EnvPolicy for DefaultEnvPolicy<'_> {
    fn allows(&self, name: &str) -> bool {
        ALLOWED_NAMES.contains(&name)
    }

    fn roots_for(&self, name: &str) -> &[PathBuf] {
        match name {
            "WINEPREFIX" => &self.prefixes,
            "VK_ICD_FILENAMES" | "GST_PLUGIN_PATH" | "DXMT_CONFIG_FILE" => &self.engines,
            _ => &[],
        }
    }

    fn d3dmetal_overlay_present(&self, engine_version: Option<&str>) -> bool {
        let Some(version) = engine_version else {
            return false;
        };
        match self.overlay {
            // A failed read is "not accepted": the gate fails closed.
            OverlayFact::Db(db) => db.d3dmetal_accepted(version).unwrap_or(false),
            OverlayFact::Fixed(present) => present,
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

    fn preset_with_env(entries: &[(&str, &str)]) -> Preset {
        let mut config = GameConfig::default();
        for (name, value) in entries {
            config
                .extra_env
                .insert((*name).to_owned(), (*value).to_owned());
        }
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
    fn default_policy_matches_the_plan_allowlist() {
        let policy = DefaultEnvPolicy::with_overlay(Path::new("/data"), false);
        assert!(policy.allows("WINEDEBUG"));
        assert!(policy.allows("VK_ICD_FILENAMES"));
        assert!(policy.allows("DXMT_CONFIG"));
        assert!(policy.allows("DXMT_CONFIG_FILE"));
        assert!(!policy.allows("DYLD_INSERT_LIBRARIES"));
        assert!(!policy.allows("LD_PRELOAD"));
        assert_eq!(
            policy.roots_for("WINEPREFIX"),
            [PathBuf::from("/data/prefixes")]
        );
        for name in ["VK_ICD_FILENAMES", "GST_PLUGIN_PATH", "DXMT_CONFIG_FILE"] {
            assert_eq!(
                policy.roots_for(name),
                [PathBuf::from("/data/engines")],
                "{name}"
            );
        }
        assert_eq!(policy.roots_for("WINEDEBUG"), &[] as &[PathBuf]);
        assert!(
            policy.roots_for("DXMT_CONFIG").is_empty(),
            "DXMT_CONFIG is an option string; DXMT_CONFIG_FILE is the path"
        );
        assert!(!policy.d3dmetal_overlay_present(Some("1.0")));
        assert!(!policy.d3dmetal_overlay_present(None));
    }

    #[test]
    fn default_policy_import_rejects_host_owned_and_escaping_paths() {
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
            ("WINEDEBUG", "-all"),
            ("DXMT_CONFIG", "d3d11.preferredMaxFrameRate=60"),
        ]);
        let policy = DefaultEnvPolicy::with_overlay(data, false);
        let plan = presets::import(&preset, &policy, data, None, ImportOptions::default()).unwrap();
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
                "dyld_insert_libraries",
            ]
        );
        assert_eq!(
            plan.config.extra_env.keys().collect::<Vec<_>>(),
            ["DXMT_CONFIG", "WINEDEBUG", "WINEPREFIX"]
        );
    }

    #[test]
    fn path_roots_depend_on_the_variable_name() {
        let data = Path::new("/data");
        let policy = DefaultEnvPolicy::with_overlay(data, false);
        let crossed = preset_with_env(&[
            ("WINEPREFIX", "/data/engines/rosetta-wine/1.0"),
            ("VK_ICD_FILENAMES", "/data/prefixes/g/drive_c/evil.json"),
            ("GST_PLUGIN_PATH", "/data/prefixes/g/drive_c/evil"),
            ("DXMT_CONFIG_FILE", "/data/prefixes/g/dxmt.conf"),
        ]);
        let plan =
            presets::import(&crossed, &policy, data, None, ImportOptions::default()).unwrap();
        assert_eq!(
            rejected_names(&plan),
            [
                "DXMT_CONFIG_FILE",
                "GST_PLUGIN_PATH",
                "VK_ICD_FILENAMES",
                "WINEPREFIX"
            ]
        );
        for item in &plan.rejected {
            assert!(item.reason.contains("outside"), "{item:?}");
        }
        assert_eq!(plan.config.extra_env, BTreeMap::new());

        let own = preset_with_env(&[
            ("WINEPREFIX", "/data/prefixes/game"),
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
        let plan = presets::import(&own, &policy, data, None, ImportOptions::default()).unwrap();
        assert!(plan.rejected.is_empty(), "{:?}", plan.rejected);
        assert_eq!(plan.config.extra_env.len(), 4);
    }

    #[test]
    fn d3dmetal_gate_reads_the_database_and_fails_closed_without_a_version() {
        let db = ConfigDb::open_in_memory().unwrap();
        let policy = DefaultEnvPolicy::new(Path::new("/data"), &db);
        assert!(!policy.d3dmetal_overlay_present(Some("1.0.0")));
        assert!(!policy.d3dmetal_overlay_present(None));
        let fixed = DefaultEnvPolicy::with_overlay(Path::new("/data"), true);
        assert!(fixed.d3dmetal_overlay_present(Some("1.0.0")));
        assert!(!fixed.d3dmetal_overlay_present(None));
    }
}
