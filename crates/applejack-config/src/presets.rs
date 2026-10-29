//! Shareable per-game presets: export strips host-specific and secret
//! material; import filters every item through an [`EnvPolicy`] and reports
//! what it accepted, rejected, or needs the user to confirm before anything
//! is written.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{ConfigDb, Error, Field, GameConfig, GraphicsBackend};

/// The preset payload version this build writes and reads.
pub const SCHEMA_VERSION: u64 = 1;

/// Placeholder that replaces the data directory in exported paths.
pub const DATA_DIR_PLACEHOLDER: &str = "$APPLEJACK_DATA_DIR";

/// `meta` key for the host chip name recorded in exports.
pub const META_HOST_CHIP: &str = "host_chip";

/// `meta` key for the macOS version recorded in exports.
pub const META_MACOS_VERSION: &str = "macos_version";

/// `extra_env` names containing any of these (case-insensitive) never leave
/// the host and are never imported.
const SECRET_NAME_FRAGMENTS: [&str; 3] = ["TOKEN", "SECRET", "PASSWORD"];

/// Allowlisted names whose values the launcher supplies from the host. A
/// preset that sets them could redirect binary lookup (`PATH`) or the
/// session's files, and the policy does not path-check them.
const HOST_OWNED_NAMES: [&str; 4] = ["PATH", "HOME", "TMPDIR", "APPLEJACK_SESSION"];

/// The engine's environment allowlist, reached through a trait so this crate
/// never depends on `applejack-engine`.
pub trait EnvPolicy {
    /// Whether the engine passes a variable with this name to the process.
    fn allows(&self, name: &str) -> bool;
    /// Whether the variable's value is a path (or `:`-separated paths) that
    /// must resolve inside [`EnvPolicy::allowed_roots`].
    fn path_valued(&self, name: &str) -> bool;
    /// Directories a path-valued variable may point into: the engine
    /// directory, its `.user` overlay, and the prefix directory.
    fn allowed_roots(&self) -> &[PathBuf];
    /// Whether a `D3DMetal` overlay is installed, which `graphics_backend =
    /// d3dmetal` requires.
    fn d3dmetal_overlay_present(&self) -> bool;
}

/// A preset file: one title's configuration plus the facts about the host
/// that produced it. Never carries account names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preset {
    pub schema: u64,
    pub appid: u32,
    pub engine_id: Option<String>,
    pub engine_version: Option<String>,
    pub host_chip: Option<String>,
    pub macos_version: Option<String>,
    pub config: GameConfig,
}

impl Preset {
    /// Parses a preset file. The schema version is checked by [`import`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Json`] when `text` is not a preset document.
    pub fn from_json(text: &str) -> Result<Self, Error> {
        Ok(serde_json::from_str(text)?)
    }

    /// Renders the preset as indented JSON.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Json`] when encoding fails.
    pub fn to_json_pretty(&self) -> Result<String, Error> {
        Ok(serde_json::to_string_pretty(self)?)
    }
}

/// Builds the shareable preset for `appid`. Absolute paths under `data_dir`
/// become `$APPLEJACK_DATA_DIR/...`; secret-looking `extra_env` names are
/// dropped; host facts come from `meta` when recorded.
///
/// # Errors
///
/// Returns the read errors of [`ConfigDb::game_config`] and
/// [`ConfigDb::meta_get`].
pub fn export(db: &ConfigDb, appid: u32, data_dir: &Path) -> Result<Preset, Error> {
    let mut config = db.game_config(appid)?.unwrap_or_default();
    let data_dir = data_dir_text(data_dir);
    config.extra_env = config
        .extra_env
        .into_iter()
        .filter(|(name, _)| !is_secret_name(name))
        .map(|(name, value)| {
            (
                name,
                replace_bounded(&value, &data_dir, DATA_DIR_PLACEHOLDER),
            )
        })
        .collect();
    for text in [
        &mut config.launch_exe_override,
        &mut config.launch_args_override,
    ]
    .into_iter()
    .flatten()
    {
        *text = replace_bounded(text, &data_dir, DATA_DIR_PLACEHOLDER);
    }
    Ok(Preset {
        schema: SCHEMA_VERSION,
        appid,
        engine_id: config.engine_id.clone(),
        engine_version: config.engine_version.clone(),
        host_chip: db.meta_get(META_HOST_CHIP)?,
        macos_version: db.meta_get(META_MACOS_VERSION)?,
        config,
    })
}

/// What the user has agreed to before an import writes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportOptions {
    /// Covers every `launch_exe_override` and `launch_args_override` item.
    pub accept_launch_overrides: bool,
}

/// An item the import will write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Accepted {
    pub field: String,
    pub value: String,
}

/// An item the import refuses, with the reason shown to the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rejected {
    pub field: String,
    pub value: String,
    pub reason: String,
}

/// An item that is written only once the user confirms it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Confirmation {
    pub field: String,
    pub value: String,
}

/// An accepted item the user should still know about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Warning {
    pub field: String,
    pub value: String,
    pub reason: String,
}

/// The outcome of filtering a preset. Nothing reaches the database until
/// [`ImportPlan::apply`] runs with every confirmation covered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportPlan {
    pub appid: u32,
    pub accepted: Vec<Accepted>,
    pub rejected: Vec<Rejected>,
    pub confirmations: Vec<Confirmation>,
    pub warnings: Vec<Warning>,
    /// The filtered configuration, confirmation items included.
    pub config: GameConfig,
    options: ImportOptions,
}

impl ImportPlan {
    /// Whether `apply` would refuse under the plan's options.
    #[must_use]
    pub fn needs_confirmation(&self) -> bool {
        !self.confirmations.is_empty() && !self.options.accept_launch_overrides
    }

    /// Merges the filtered configuration into the stored row for the preset's
    /// appid. Fields the preset does not set keep their stored values.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ConfirmationRequired`] without writing when a
    /// confirmation item is not covered by the options, and the errors of
    /// [`ConfigDb::game_config`] and [`ConfigDb::set_game_config`].
    pub fn apply(&self, db: &ConfigDb) -> Result<(), Error> {
        if self.needs_confirmation() {
            return Err(Error::ConfirmationRequired(self.confirmations.clone()));
        }
        let mut stored = db.game_config(self.appid)?.unwrap_or_default();
        stored.merge_from(&self.config);
        db.set_game_config(self.appid, &stored)
    }
}

/// Filters `preset` through `policy` into a plan. `$APPLEJACK_DATA_DIR` is
/// expanded to `data_dir` before path checks.
///
/// # Errors
///
/// Returns [`Error::UnsupportedPresetSchema`] when the file's schema version
/// is not [`SCHEMA_VERSION`].
pub fn import(
    preset: &Preset,
    policy: &dyn EnvPolicy,
    data_dir: &Path,
    options: ImportOptions,
) -> Result<ImportPlan, Error> {
    if preset.schema != SCHEMA_VERSION {
        return Err(Error::UnsupportedPresetSchema(preset.schema));
    }
    let data_dir = data_dir_text(data_dir);
    let mut plan = ImportPlan {
        appid: preset.appid,
        accepted: Vec::new(),
        rejected: Vec::new(),
        confirmations: Vec::new(),
        warnings: Vec::new(),
        config: GameConfig::default(),
        options,
    };
    for field in Field::ALL {
        let Some(value) = preset.config.get(field) else {
            continue;
        };
        match field {
            Field::ExtraEnv => {
                for (name, value) in &preset.config.extra_env {
                    import_env_entry(&mut plan, policy, &data_dir, name, value);
                }
            }
            Field::LaunchExeOverride | Field::LaunchArgsOverride => {
                let value = replace_bounded(&value, DATA_DIR_PLACEHOLDER, &data_dir);
                plan.confirmations.push(Confirmation {
                    field: field.name().to_owned(),
                    value: value.clone(),
                });
                accept(&mut plan, field, &value);
            }
            Field::GraphicsBackend
                if preset.config.graphics_backend == Some(GraphicsBackend::D3dmetal)
                    && !policy.d3dmetal_overlay_present() =>
            {
                plan.rejected.push(Rejected {
                    field: field.name().to_owned(),
                    value,
                    reason: "no D3DMetal overlay is installed for the engine".to_owned(),
                });
            }
            Field::DirectVerifiedAt => plan.rejected.push(Rejected {
                field: field.name().to_owned(),
                value,
                reason: "host-local verification stamp; run the direct probe here instead"
                    .to_owned(),
            }),
            _ => accept(&mut plan, field, &value),
        }
    }
    Ok(plan)
}

fn accept(plan: &mut ImportPlan, field: Field, value: &str) {
    // `value` came from `GameConfig::get` on a parsed preset, or is a path
    // rewrite of one, so it parses; a failure would be a table bug.
    plan.config
        .set(field, value)
        .expect("value read from a parsed preset parses again");
    plan.accepted.push(Accepted {
        field: field.name().to_owned(),
        value: value.to_owned(),
    });
}

fn import_env_entry(
    plan: &mut ImportPlan,
    policy: &dyn EnvPolicy,
    data_dir: &str,
    name: &str,
    value: &str,
) {
    let field = format!("extra_env.{name}");
    let mut reject = |value: &str, reason: String| {
        plan.rejected.push(Rejected {
            field: field.clone(),
            value: value.to_owned(),
            reason,
        });
    };
    if let Err(reason) = crate::game_config::check_env_name(name) {
        return reject(value, reason);
    }
    if name
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("DYLD_"))
    {
        return reject(
            value,
            "DYLD_* variables are never passed to the engine".to_owned(),
        );
    }
    if HOST_OWNED_NAMES
        .iter()
        .any(|owned| owned.eq_ignore_ascii_case(name))
    {
        return reject(
            value,
            "set by the launcher from the host, never by a preset".to_owned(),
        );
    }
    if is_secret_name(name) {
        return reject(value, "secret-looking names are never imported".to_owned());
    }
    if !policy.allows(name) {
        return reject(value, "not in the engine environment allowlist".to_owned());
    }
    let value = replace_bounded(value, DATA_DIR_PLACEHOLDER, data_dir);
    if policy.path_valued(name)
        && let Err(reason) = paths_inside_roots(&value, policy.allowed_roots())
    {
        return reject(&value, reason);
    }
    if name == "WINEDLLOVERRIDES" {
        plan.warnings.push(Warning {
            field: field.clone(),
            value: value.clone(),
            reason: "DLL overrides are applied as given; the engine refuses overrides of \
                     ntdll and other core DLLs at launch"
                .to_owned(),
        });
    }
    plan.config.extra_env.insert(name.to_owned(), value.clone());
    plan.accepted.push(Accepted { field, value });
}

/// Every `:`-separated element must be an absolute, `.`- and `..`-free path
/// under one of `roots`. Roots need not exist yet, so the check is lexical.
fn paths_inside_roots(value: &str, roots: &[PathBuf]) -> Result<(), String> {
    for part in value.split(':').filter(|part| !part.is_empty()) {
        let path = Path::new(part);
        if !path.is_absolute() {
            return Err(format!("{part:?} is not an absolute path"));
        }
        if path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
        {
            return Err(format!("{part:?} contains . or .. components"));
        }
        if !roots.iter().any(|root| path.starts_with(root)) {
            return Err(format!(
                "{part:?} is outside the engine, overlay, and prefix directories"
            ));
        }
    }
    Ok(())
}

fn is_secret_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_NAME_FRAGMENTS
        .iter()
        .any(|fragment| upper.contains(fragment))
}

/// The data directory as text without a trailing separator, so a match must
/// end at a path boundary.
fn data_dir_text(data_dir: &Path) -> String {
    let text = data_dir.to_string_lossy();
    let trimmed = text.trim_end_matches('/');
    if trimmed.is_empty() {
        text.into_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Characters that may sit on either side of a path inside a value: list
/// separators, assignments, quotes, and whitespace.
fn is_delimiter(c: char) -> bool {
    c.is_whitespace() || matches!(c, ':' | ';' | '=' | ',' | '"' | '\'')
}

/// Replaces `from` with `to` only where `from` is a whole path prefix: it
/// starts the text or follows a delimiter, and is followed by `/`, a
/// delimiter, or the end. So `/data/x` never matches inside `/data/xy` or
/// `/opt/data/x`.
fn replace_bounded(text: &str, from: &str, to: &str) -> String {
    if from.is_empty() || from == "/" {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut search = 0;
    while let Some(offset) = text[search..].find(from) {
        let start = search + offset;
        let end = start + from.len();
        let left_ok = text[..start].chars().next_back().is_none_or(is_delimiter);
        let right_ok = text[end..]
            .chars()
            .next()
            .is_none_or(|c| c == '/' || is_delimiter(c));
        if left_ok && right_ok {
            out.push_str(&text[copied..start]);
            out.push_str(to);
            copied = end;
            search = end;
        } else {
            search = start + text[start..].chars().next().map_or(1, char::len_utf8);
        }
    }
    out.push_str(&text[copied..]);
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn data_dir() -> PathBuf {
        PathBuf::from("/Users/example/Library/Application Support/applejack")
    }

    #[test]
    fn export_drops_secret_names_and_rewrites_data_dir_paths() {
        let db = ConfigDb::open_in_memory().unwrap();
        let data = data_dir();
        let config = GameConfig {
            extra_env: BTreeMap::from([
                ("MY_TOKEN".to_owned(), "abc".to_owned()),
                ("steam_secret".to_owned(), "abc".to_owned()),
                ("PASSWORD_X".to_owned(), "abc".to_owned()),
                ("WINEDEBUG".to_owned(), "-all".to_owned()),
                (
                    "VK_ICD_FILENAMES".to_owned(),
                    format!(
                        "{}/engines/rosetta-wine/1.0/icd.json:{}/engines/rosetta-wine/1.0.user/icd.json",
                        data.display(),
                        data.display()
                    ),
                ),
                (
                    "GST_PLUGIN_PATH".to_owned(),
                    format!("{}-other/plugins", data.display()),
                ),
            ]),
            launch_exe_override: Some(format!("{}/prefixes/game/drive_c/game.exe", data.display())),
            launch_args_override: Some(format!("--root {}", data.display())),
            ..GameConfig::default()
        };
        db.set_game_config(10, &config).unwrap();
        db.meta_set(META_HOST_CHIP, "Apple M4").unwrap();
        let preset = export(&db, 10, &data).unwrap();
        assert_eq!(preset.schema, SCHEMA_VERSION);
        assert_eq!(preset.appid, 10);
        assert_eq!(preset.host_chip.as_deref(), Some("Apple M4"));
        assert_eq!(preset.macos_version, None);
        let env = &preset.config.extra_env;
        assert_eq!(
            env.keys().collect::<Vec<_>>(),
            ["GST_PLUGIN_PATH", "VK_ICD_FILENAMES", "WINEDEBUG"]
        );
        assert_eq!(env["WINEDEBUG"], "-all");
        assert_eq!(
            env["VK_ICD_FILENAMES"],
            "$APPLEJACK_DATA_DIR/engines/rosetta-wine/1.0/icd.json:$APPLEJACK_DATA_DIR/engines/rosetta-wine/1.0.user/icd.json"
        );
        assert_eq!(
            env["GST_PLUGIN_PATH"],
            format!("{}-other/plugins", data.display()),
            "a sibling directory with the data dir as a prefix is not rewritten"
        );
        assert_eq!(
            preset.config.launch_exe_override.as_deref(),
            Some("$APPLEJACK_DATA_DIR/prefixes/game/drive_c/game.exe")
        );
        assert_eq!(
            preset.config.launch_args_override.as_deref(),
            Some("--root $APPLEJACK_DATA_DIR")
        );
        let json = preset.to_json_pretty().unwrap();
        assert!(!json.contains("abc"), "secret values leaked: {json}");
        assert_eq!(Preset::from_json(&json).unwrap(), preset);
    }

    #[test]
    fn export_of_missing_row_is_an_empty_preset() {
        let db = ConfigDb::open_in_memory().unwrap();
        let preset = export(&db, 7, &data_dir()).unwrap();
        assert_eq!(preset.config, GameConfig::default());
        assert_eq!(preset.appid, 7);
    }

    #[test]
    fn bounded_replacement_respects_path_boundaries() {
        assert_eq!(replace_bounded("/d/x", "/d", "$D"), "$D/x");
        assert_eq!(replace_bounded("/d", "/d", "$D"), "$D");
        assert_eq!(replace_bounded("/dx/y", "/d", "$D"), "/dx/y");
        assert_eq!(replace_bounded("a /d/x b /d", "/d", "$D"), "a $D/x b $D");
        assert_eq!(replace_bounded("$D/x", "$D", "/d"), "/d/x");
        assert_eq!(replace_bounded("$DX", "$D", "/d"), "$DX");
        assert_eq!(replace_bounded("/opt/d/x", "/d", "$D"), "/opt/d/x");
        assert_eq!(replace_bounded("x$D/y", "$D", "/d"), "x$D/y");
        assert_eq!(
            replace_bounded("--root \"/d\" a=/d/x:/d/y", "/d", "$D"),
            "--root \"$D\" a=$D/x:$D/y"
        );
        assert_eq!(replace_bounded("$D:/x", "$D", "/d"), "/d:/x");
        assert_eq!(replace_bounded("/x", "", "y"), "/x");
        assert_eq!(replace_bounded("/x", "/", "y"), "/x");
    }

    #[test]
    fn unsupported_schema_is_refused() {
        struct Deny;
        impl EnvPolicy for Deny {
            fn allows(&self, _: &str) -> bool {
                false
            }
            fn path_valued(&self, _: &str) -> bool {
                false
            }
            fn allowed_roots(&self) -> &[PathBuf] {
                &[]
            }
            fn d3dmetal_overlay_present(&self) -> bool {
                false
            }
        }
        let preset = Preset {
            schema: 2,
            appid: 1,
            engine_id: None,
            engine_version: None,
            host_chip: None,
            macos_version: None,
            config: GameConfig::default(),
        };
        let error = import(&preset, &Deny, &data_dir(), ImportOptions::default()).unwrap_err();
        assert!(
            matches!(error, Error::UnsupportedPresetSchema(2)),
            "{error}"
        );
    }
}
