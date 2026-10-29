//! Data directory resolution, the database handle, and the default
//! environment policy that presets import through. Re-exports the
//! `applejack-config` types the CLI and the RPC layer use.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};

pub use applejack_config::presets::{
    self, Accepted, Confirmation, DATA_DIR_PLACEHOLDER, META_HOST_CHIP, META_MACOS_VERSION,
    Rejected, SCHEMA_VERSION, Warning,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultEnvPolicy {
    roots: Vec<PathBuf>,
    overlay_present: bool,
}

const ALLOWED_NAMES: [&str; 15] = [
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

const PATH_VALUED_NAMES: [&str; 4] = [
    "VK_ICD_FILENAMES",
    "GST_PLUGIN_PATH",
    "DXMT_CONFIG",
    "WINEPREFIX",
];

impl DefaultEnvPolicy {
    /// Policy rooted at `data_dir`'s `engines/` and `prefixes/` directories.
    /// The `D3DMetal` overlay is considered present when any
    /// `engines/<id>/<version>.user/` directory exists.
    #[must_use]
    pub fn new(data_dir: &Path) -> Self {
        let engines = data_dir.join("engines");
        let overlay_present = overlay_present(&engines);
        Self {
            roots: vec![engines, data_dir.join("prefixes")],
            overlay_present,
        }
    }

    /// Policy with the overlay fact supplied by the caller, for hosts that
    /// already know it.
    #[must_use]
    pub fn with_overlay(data_dir: &Path, overlay_present: bool) -> Self {
        Self {
            roots: vec![data_dir.join("engines"), data_dir.join("prefixes")],
            overlay_present,
        }
    }
}

fn overlay_present(engines: &Path) -> bool {
    let Ok(engine_dirs) = std::fs::read_dir(engines) else {
        return false;
    };
    engine_dirs
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_dir(entry.path()).ok())
        .flatten()
        .filter_map(Result::ok)
        .any(|entry| {
            let path = entry.path();
            path.is_dir() && path.extension() == Some(OsStr::new("user"))
        })
}

impl EnvPolicy for DefaultEnvPolicy {
    fn allows(&self, name: &str) -> bool {
        ALLOWED_NAMES.contains(&name)
    }

    fn path_valued(&self, name: &str) -> bool {
        PATH_VALUED_NAMES.contains(&name)
    }

    fn allowed_roots(&self) -> &[PathBuf] {
        &self.roots
    }

    fn d3dmetal_overlay_present(&self) -> bool {
        self.overlay_present
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        }
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
    fn default_policy_matches_the_plan_allowlist() {
        let policy = DefaultEnvPolicy::with_overlay(Path::new("/data"), false);
        assert!(policy.allows("WINEDEBUG"));
        assert!(policy.allows("VK_ICD_FILENAMES"));
        assert!(!policy.allows("DYLD_INSERT_LIBRARIES"));
        assert!(!policy.allows("LD_PRELOAD"));
        assert!(policy.path_valued("WINEPREFIX"));
        assert!(!policy.path_valued("WINEDEBUG"));
        assert_eq!(
            policy.allowed_roots(),
            [
                PathBuf::from("/data/engines"),
                PathBuf::from("/data/prefixes")
            ]
        );
        assert!(!policy.d3dmetal_overlay_present());
    }

    #[test]
    fn default_policy_import_rejects_host_owned_and_escaping_paths() {
        let data = Path::new("/data");
        let mut config = GameConfig::default();
        for (name, value) in [
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
        ] {
            config.extra_env.insert(name.to_owned(), value.to_owned());
        }
        let preset = Preset {
            schema: SCHEMA_VERSION,
            appid: 1,
            engine_id: None,
            engine_version: None,
            host_chip: None,
            macos_version: None,
            config,
        };
        let policy = DefaultEnvPolicy::with_overlay(data, false);
        let plan = presets::import(&preset, &policy, data, ImportOptions::default()).unwrap();
        let mut rejected: Vec<&str> = plan
            .rejected
            .iter()
            .map(|item| item.field.trim_start_matches("extra_env."))
            .collect();
        rejected.sort_unstable();
        assert_eq!(
            rejected,
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
            ["WINEDEBUG", "WINEPREFIX"]
        );
    }

    #[test]
    fn overlay_detection_scans_user_directories() {
        let root = std::env::temp_dir().join(format!(
            "applejack-core-overlay-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        assert!(!DefaultEnvPolicy::new(&root).d3dmetal_overlay_present());
        std::fs::create_dir_all(root.join("engines/rosetta-wine/1.0")).unwrap();
        assert!(!DefaultEnvPolicy::new(&root).d3dmetal_overlay_present());
        std::fs::create_dir_all(root.join("engines/rosetta-wine/1.0.user")).unwrap();
        assert!(DefaultEnvPolicy::new(&root).d3dmetal_overlay_present());
        let _ = std::fs::remove_dir_all(&root);
    }
}
