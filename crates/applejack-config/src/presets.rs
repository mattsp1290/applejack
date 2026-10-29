//! Shareable per-game presets: export strips host-specific and secret
//! material; import filters every item through an [`EnvPolicy`] and reports
//! what it accepted, rejected, or needs the user to confirm before anything
//! is written.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{ConfigDb, Error, Field, GameConfig, GraphicsBackend};

/// The preset payload version this build writes and reads. Bump it when
/// `config` gains a column: `GameConfig` denies unknown keys, so an older
/// build refuses a preset that sets the new column, and the version check is
/// where it should learn why. Unset columns are not written, so a preset that
/// never set the column stays readable.
pub const SCHEMA_VERSION: u64 = 1;

/// Placeholder that replaces the data directory in exported paths.
pub const DATA_DIR_PLACEHOLDER: &str = "$APPLEJACK_DATA_DIR";

/// Placeholder that replaces the home directory in exported paths, so a
/// preset never carries the account name that macOS puts under `/Users/`.
pub const HOME_PLACEHOLDER: &str = "$HOME";

/// `meta` key for the host chip name recorded in exports.
pub const META_HOST_CHIP: &str = "host_chip";

/// `meta` key for the macOS version recorded in exports.
pub const META_MACOS_VERSION: &str = "macos_version";

/// `extra_env` names containing any of these never leave the host and are
/// never imported. Names are compared upper-cased with `_` removed, so
/// `Pass_word` and `api_key` match.
const SECRET_NAME_FRAGMENTS: [&str; 7] = [
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASS",
    "KEY",
    "AUTH",
    "CREDENTIAL",
];

/// Names a preset may never set, whatever the policy allows: the host
/// environment the launcher supplies, and variables that choose which
/// binaries, loaders, or shell startup files run. This is the hard stop behind
/// a policy that the engine crate will replace.
const HOST_OWNED_NAMES: [&str; 15] = [
    "PATH",
    "HOME",
    "TMPDIR",
    "APPLEJACK_SESSION",
    "USER",
    "LOGNAME",
    "SHELL",
    "PWD",
    "WINELOADER",
    "WINESERVER",
    "WINEDLLPATH",
    "BASH_ENV",
    "ENV",
    "IFS",
    "SHELLOPTS",
];

/// Dynamic-loader variable prefixes; any name starting with one is rejected.
const LOADER_PREFIXES: [&str; 2] = ["DYLD_", "LD_"];

/// Names the engine sets itself from the chosen backend and sync mode. A
/// preset value would be dropped at launch, so import refuses it, whatever
/// the policy allows; a policy's [`EnvPolicy::allows`] answers `false` for
/// them too.
pub const ENGINE_OWNED_NAMES: [&str; 4] =
    ["WINEPREFIX", "WINEDLLOVERRIDES", "WINEMSYNC", "WINEESYNC"];

/// How a policy checks a variable's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueCheck<'a> {
    /// Plain text; not checked as a path.
    Plain,
    /// Exactly one absolute path, which must resolve inside one of the roots.
    Path(&'a [PathBuf]),
    /// A `:`-separated list of absolute paths, each inside one of the roots.
    PathList(&'a [PathBuf]),
}

/// The engine's environment allowlist, reached through a trait so this crate
/// never depends on `applejack-engine`.
pub trait EnvPolicy {
    /// Whether a game-tier preset may set a variable with this name: the
    /// game tier of the engine's allowlist. A name the engine passes through
    /// but sets itself (`WINEPREFIX`, `APPLEJACK_SESSION`) answers `false`.
    fn allows(&self, name: &str) -> bool;
    /// How the variable's value is checked. Each path-valued name has its
    /// own roots: an ICD or plugin path may not point into a Windows-writable
    /// prefix.
    fn value_check(&self, name: &str) -> ValueCheck<'_>;
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
    /// Returns [`Error::Json`] when `text` is not a preset document, including
    /// when `config` holds a key that is not a `game_config` column.
    pub fn from_json(text: &str) -> Result<Self, Error> {
        Ok(serde_json::from_str(text)?)
    }

    /// Renders the preset as indented JSON. Unset `config` columns are left
    /// out, so a build that predates a column still reads a preset that never
    /// set it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Json`] when encoding fails.
    pub fn to_json_pretty(&self) -> Result<String, Error> {
        let mut value = serde_json::to_value(self)?;
        if let Some(config) = value
            .get_mut("config")
            .and_then(serde_json::Value::as_object_mut)
        {
            config.retain(|_, field| !field.is_null());
        }
        Ok(serde_json::to_string_pretty(&value)?)
    }
}

/// Builds the shareable preset for `appid`. Absolute paths under `data_dir`
/// become `$APPLEJACK_DATA_DIR/...` and paths under `home` become
/// `$HOME/...`, the longer directory first; secret-looking `extra_env` names
/// are dropped; the host-local `direct_verified_at` stamp is cleared; host
/// facts come from `meta` when recorded. The export is refused when the
/// account name (the last component of `home`) survives the rewrite as a
/// path component in any text field, such as the Wine user directory
/// `drive_c/users/<name>` or a `Z:\Users\<name>` path.
///
/// `home` is the exporting host's `HOME`, read by the caller: this crate
/// never consults the process environment.
///
/// # Errors
///
/// Returns [`Error::AccountNameInExport`] naming the first field that still
/// holds the account name, and the read errors of [`ConfigDb::game_config`]
/// and [`ConfigDb::meta_get`].
pub fn export(
    db: &ConfigDb,
    appid: u32,
    data_dir: &Path,
    home: Option<&Path>,
) -> Result<Preset, Error> {
    let mut config = db.game_config(appid)?.unwrap_or_default();
    let mut rewrites = vec![(path_text(data_dir), DATA_DIR_PLACEHOLDER)];
    if let Some(home) = home {
        rewrites.push((path_text(home), HOME_PLACEHOLDER));
    }
    // Longest first, so whichever directory lies inside the other is
    // rewritten as a whole before its ancestor is.
    rewrites.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
    let scrub = |text: &str| {
        rewrites.iter().fold(text.to_owned(), |text, (from, to)| {
            replace_bounded(&text, from, to)
        })
    };
    config.extra_env = config
        .extra_env
        .into_iter()
        .filter(|(name, _)| !is_secret_name(name))
        .map(|(name, value)| (name, scrub(&value)))
        .collect();
    for text in [
        &mut config.launch_exe_override,
        &mut config.launch_args_override,
        &mut config.notes,
    ]
    .into_iter()
    .flatten()
    {
        *text = scrub(text);
    }
    config.direct_verified_at = None;
    if let Some(account) = home.and_then(account_name)
        && let Some((field, _)) =
            exported_texts(&config).find(|(_, text)| contains_account(text, &account))
    {
        return Err(Error::AccountNameInExport { field });
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

/// The account short name: the last component of the home directory.
fn account_name(home: &Path) -> Option<String> {
    home.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
}

/// Whether `text` holds `account` as a whole path component under either
/// separator: `/Users/x`, `drive_c/users/x`, `Z:\Users\x`, `file:///Users/x`.
/// Case does not matter, because the macOS file system ignores it.
fn contains_account(text: &str, account: &str) -> bool {
    text.split(['/', '\\'])
        .any(|part| part.eq_ignore_ascii_case(account))
}

/// Every text field an export carries, with its report name.
fn exported_texts(config: &GameConfig) -> impl Iterator<Item = (String, &str)> {
    let env = config
        .extra_env
        .iter()
        .map(|(name, value)| (format!("extra_env.{name}"), value.as_str()));
    let fields = [
        (Field::LaunchExeOverride, &config.launch_exe_override),
        (Field::LaunchArgsOverride, &config.launch_args_override),
        (Field::Notes, &config.notes),
    ]
    .into_iter()
    .filter_map(|(field, value)| {
        value
            .as_deref()
            .map(|value| (field.name().to_owned(), value))
    });
    env.chain(fields)
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
    /// Every item that `apply` writes, confirmation items included.
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
    /// appid, inside one write transaction so a concurrent writer cannot
    /// interleave between the read and the write. Fields the preset does not
    /// set keep their stored values.
    ///
    /// The plan is written as computed: the overlay and path gates are not
    /// re-run against the database or the file system at apply time. The
    /// launch step re-validates the overlay and every path before use.
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
        db.write_locked(|db| {
            let mut stored = db.game_config(self.appid)?.unwrap_or_default();
            stored.merge_from(&self.config);
            db.set_game_config(self.appid, &stored)
        })
    }
}

/// Filters `preset` through `policy` into a plan. `$APPLEJACK_DATA_DIR` and
/// `$HOME` are expanded to `data_dir` and `home` before path checks; with
/// `home` unset, `$HOME` stays literal: a path-valued variable then fails the
/// absolute-path check and a `launch_exe_override` is rejected outright. The
/// `d3dmetal` gate reads `db`'s `d3dmetal` table; nothing is written.
///
/// # Errors
///
/// Returns [`Error::UnsupportedPresetSchema`] when the file's schema version
/// is not [`SCHEMA_VERSION`], and [`Error::Sqlite`] when the overlay table
/// cannot be read.
pub fn import(
    preset: &Preset,
    policy: &dyn EnvPolicy,
    db: &ConfigDb,
    data_dir: &Path,
    home: Option<&Path>,
    options: ImportOptions,
) -> Result<ImportPlan, Error> {
    if preset.schema != SCHEMA_VERSION {
        return Err(Error::UnsupportedPresetSchema(preset.schema));
    }
    let placeholders = Placeholders {
        data_dir: path_text(data_dir),
        home: home.map(path_text),
    };
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
        if field != Field::ExtraEnv && value.chars().any(is_unsafe_char) {
            plan.rejected.push(Rejected {
                field: field.name().to_owned(),
                value,
                reason: UNSAFE_CHARS.to_owned(),
            });
            continue;
        }
        match field {
            Field::ExtraEnv => {
                for (name, value) in &preset.config.extra_env {
                    import_env_entry(&mut plan, policy, &placeholders, name, value);
                }
            }
            Field::LaunchExeOverride => match placeholders.expand_path(&value) {
                Ok(value) => confirm(&mut plan, field, &value),
                Err(reason) => plan.rejected.push(Rejected {
                    field: field.name().to_owned(),
                    value,
                    reason,
                }),
            },
            Field::LaunchArgsOverride => {
                confirm(&mut plan, field, &placeholders.expand(&value));
            }
            Field::Notes => accept(&mut plan, field, &placeholders.expand(&value)),
            Field::GraphicsBackend
                if preset.config.graphics_backend == Some(GraphicsBackend::D3dmetal) =>
            {
                // An engine version that fails the identifier check is not
                // imported, so it cannot vouch for an overlay either.
                let gated_version = preset.config.engine_version.as_deref().filter(|version| {
                    matches!(crate::game_config::identifier(version), Ok(Some(_)))
                });
                let reason = match gated_version {
                    None => Some("d3dmetal needs the preset to name an engine version".to_owned()),
                    Some(version) => (!db.d3dmetal_accepted(version)?).then(|| {
                        format!("no D3DMetal overlay is accepted for engine version {version}")
                    }),
                };
                match reason {
                    Some(reason) => plan.rejected.push(Rejected {
                        field: field.name().to_owned(),
                        value,
                        reason,
                    }),
                    None => accept(&mut plan, field, &value),
                }
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

const UNSAFE_CHARS: &str =
    "control and bidirectional formatting characters are not allowed in preset values";

/// Whether `c` is a control character or a bidirectional formatting
/// character (`U+200E`, `U+200F`, `U+202A` to `U+202E`, `U+2066` to
/// `U+2069`). Either can move the cursor, reverse a line, or forge a report
/// line, so import refuses them; `Debug` formatting escapes them all.
fn is_unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}

/// The host paths a preset's placeholders expand to.
struct Placeholders {
    data_dir: String,
    home: Option<String>,
}

impl Placeholders {
    fn expand(&self, value: &str) -> String {
        let value = replace_bounded(value, DATA_DIR_PLACEHOLDER, &self.data_dir);
        match &self.home {
            Some(home) => replace_bounded(&value, HOME_PLACEHOLDER, home),
            None => value,
        }
    }

    /// Expands a single path and folds `.` and `..` away, so the value the
    /// user confirms reads as the path that runs. A path that used a
    /// placeholder must stay under that placeholder's directory.
    fn expand_path(&self, value: &str) -> Result<String, String> {
        let root = if starts_with_placeholder(value, DATA_DIR_PLACEHOLDER) {
            Some(&self.data_dir)
        } else if starts_with_placeholder(value, HOME_PLACEHOLDER) {
            match &self.home {
                Some(home) => Some(home),
                // A literal `$HOME` would normalize to a relative path that
                // does not resemble the file the user is asked to confirm.
                None => return Err(format!("{HOME_PLACEHOLDER} is not set on this host")),
            }
        } else {
            None
        };
        let expanded = self.expand(value);
        let normalized = normalize_lexically(Path::new(&expanded));
        if let Some(root) = root
            && !normalized.starts_with(root)
        {
            return Err(format!(
                "{value:?} escapes the directory its placeholder names"
            ));
        }
        Ok(normalized.to_string_lossy().into_owned())
    }
}

fn starts_with_placeholder(value: &str, placeholder: &str) -> bool {
    value
        .strip_prefix(placeholder)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Resolves `.` and `..` components without touching the filesystem. A `..`
/// that would climb above the start of a relative path is kept.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // `/..` is `/`; a relative path keeps climbing.
                Some(Component::RootDir) => {}
                _ => out.push(component),
            },
            other => out.push(other),
        }
    }
    out
}

/// Records a launch override: written only once confirmed, listed once.
fn confirm(plan: &mut ImportPlan, field: Field, value: &str) {
    let before = plan.accepted.len();
    accept(plan, field, value);
    if plan.accepted.len() > before {
        plan.confirmations.push(Confirmation {
            field: field.name().to_owned(),
            value: value.to_owned(),
        });
    }
}

/// Applies `value` through the same field parser `config set` uses. Serde
/// accepts any string for text columns, so the parser is where checks such as
/// the engine id charset reject a preset item.
fn accept(plan: &mut ImportPlan, field: Field, value: &str) {
    match plan.config.set(field, value) {
        Ok(()) => plan.accepted.push(Accepted {
            field: field.name().to_owned(),
            value: value.to_owned(),
        }),
        Err(error) => plan.rejected.push(Rejected {
            field: field.name().to_owned(),
            value: value.to_owned(),
            reason: match error {
                Error::InvalidValue { reason, .. } => reason,
                other => other.to_string(),
            },
        }),
    }
}

fn import_env_entry(
    plan: &mut ImportPlan,
    policy: &dyn EnvPolicy,
    placeholders: &Placeholders,
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
    if value.chars().any(is_unsafe_char) {
        return reject(value, UNSAFE_CHARS.to_owned());
    }
    if let Some(prefix) = LOADER_PREFIXES.iter().find(|prefix| {
        name.get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    }) {
        return reject(
            value,
            format!("{prefix}* loader variables are never passed to the engine"),
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
    if ENGINE_OWNED_NAMES
        .iter()
        .any(|owned| owned.eq_ignore_ascii_case(name))
    {
        return reject(
            value,
            "set by the engine from the chosen backend and sync mode".to_owned(),
        );
    }
    if is_secret_name(name) {
        return reject(value, "secret-looking names are never imported".to_owned());
    }
    if !policy.allows(name) {
        return reject(value, "not in the engine environment allowlist".to_owned());
    }
    let value = placeholders.expand(value);
    let check = match policy.value_check(name) {
        ValueCheck::Plain => None,
        ValueCheck::Path(roots) => Some((roots, false)),
        ValueCheck::PathList(roots) => Some((roots, true)),
    };
    if let Some((roots, list)) = check
        && let Err(reason) = paths_inside_roots(&value, roots, list)
    {
        return reject(&value, reason);
    }
    plan.config.extra_env.insert(name.to_owned(), value.clone());
    plan.accepted.push(Accepted { field, value });
}

/// Every path in `value` (one path, or a `:`-separated list when `list`)
/// must be absolute, `.`- and `..`-free, and under one of `roots`, lexically
/// and after resolving symlinks in whatever part of it already exists. Roots
/// need not exist yet; their existing ancestors are resolved the same way.
///
/// This is an import-time check of the text: a symlink that appears later
/// under a component that does not exist yet is not seen here. The launch
/// step re-validates every path before it reaches the process.
fn paths_inside_roots(value: &str, roots: &[PathBuf], list: bool) -> Result<(), String> {
    let mut parts = value.split(':').filter(|part| !part.is_empty()).peekable();
    if parts.peek().is_none() {
        return Err("a path-valued variable needs at least one path".to_owned());
    }
    if !list && value.contains(':') {
        return Err("a single-path variable may not hold a ':'-separated list".to_owned());
    }
    for part in parts {
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
                "{part:?} is outside the directories {} may point into",
                roots_text(roots)
            ));
        }
        let resolved = canonical_prefix(path)?;
        let inside = roots
            .iter()
            .map(|root| canonical_prefix(root))
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|root| resolved.starts_with(root));
        if !inside {
            return Err(format!(
                "{part:?} resolves to {} through a symlink, outside the directories {} may point into",
                resolved.display(),
                roots_text(roots)
            ));
        }
    }
    Ok(())
}

fn roots_text(roots: &[PathBuf]) -> String {
    roots
        .iter()
        .map(|root| format!("{}", root.display()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `path` with its longest existing ancestor canonicalized and the rest
/// appended lexically. A dangling symlink in the ancestry is an error.
fn canonical_prefix(path: &Path) -> Result<PathBuf, String> {
    let mut existing = path;
    loop {
        if existing.symlink_metadata().is_ok() {
            break;
        }
        match existing.parent() {
            Some(parent) => existing = parent,
            None => return Ok(path.to_path_buf()),
        }
    }
    let canonical = std::fs::canonicalize(existing)
        .map_err(|error| format!("{} cannot be resolved: {error}", existing.display()))?;
    let rest = path
        .strip_prefix(existing)
        .expect("existing is an ancestor of path");
    Ok(canonical.join(rest))
}

fn is_secret_name(name: &str) -> bool {
    let folded: String = name
        .chars()
        .filter(|c| *c != '_')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    SECRET_NAME_FRAGMENTS
        .iter()
        .any(|fragment| folded.contains(fragment))
}

/// A directory as text without a trailing separator, so a match must end at
/// a path boundary.
fn path_text(path: &Path) -> String {
    let text = path.to_string_lossy();
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

/// Whether a path may start at `start`: at the text start, after a
/// delimiter, or glued to a flag such as `-I/dir` or `-L/dir`. The flag rule
/// accepts only a token of dashes and flag letters before the path, so the
/// `/dir` inside `-x/opt/dir` is not rewritten. A `-` after a path is part of
/// a sibling name (`/data-other`), so it never ends a match.
fn path_may_start_at(text: &str, start: usize) -> bool {
    let before = &text[..start];
    if before.chars().next_back().is_none_or(is_delimiter) {
        return true;
    }
    let token_start = before.rfind(is_delimiter).map_or(0, |i| {
        i + before[i..].chars().next().map_or(1, char::len_utf8)
    });
    let token = &before[token_start..];
    let flag = token.trim_start_matches('-');
    token.starts_with('-') && flag.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Replaces `from` with `to` only where `from` is a whole path prefix: it
/// may start there per [`path_may_start_at`], and is followed by `/`, a
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
        let left_ok = path_may_start_at(text, start);
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

    fn home() -> PathBuf {
        PathBuf::from("/Users/example")
    }

    fn data_dir() -> PathBuf {
        home().join("Library/Application Support/applejack")
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
                ("Pass_word".to_owned(), "abc".to_owned()),
                ("API_KEY".to_owned(), "abc".to_owned()),
                ("AUTH".to_owned(), "abc".to_owned()),
                ("x_credential".to_owned(), "abc".to_owned()),
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
                    format!(
                        "-I{}/engines:{}-other/plugins",
                        data.display(),
                        data.display()
                    ),
                ),
            ]),
            launch_exe_override: Some(format!("{}/prefixes/game/drive_c/game.exe", data.display())),
            launch_args_override: Some(format!("--root {}", data.display())),
            direct_verified_at: Some("2026-10-29T00:00:00Z".to_owned()),
            ..GameConfig::default()
        };
        db.set_game_config(10, &config).unwrap();
        db.meta_set(META_HOST_CHIP, "Apple M4").unwrap();
        let preset = export(&db, 10, &data, None).unwrap();
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
            format!(
                "-I$APPLEJACK_DATA_DIR/engines:{}-other/plugins",
                data.display()
            ),
            "a glued -I flag is rewritten; a sibling directory with the data dir as a prefix is not"
        );
        assert_eq!(
            preset.config.launch_exe_override.as_deref(),
            Some("$APPLEJACK_DATA_DIR/prefixes/game/drive_c/game.exe")
        );
        assert_eq!(
            preset.config.launch_args_override.as_deref(),
            Some("--root $APPLEJACK_DATA_DIR")
        );
        assert_eq!(preset.config.direct_verified_at, None);
        let json = preset.to_json_pretty().unwrap();
        assert!(!json.contains("abc"), "secret values leaked: {json}");
        assert_eq!(Preset::from_json(&json).unwrap(), preset);
    }

    #[test]
    fn export_rewrites_the_home_directory_and_drops_account_paths() {
        let db = ConfigDb::open_in_memory().unwrap();
        let data = data_dir();
        let home = home();
        let config = GameConfig {
            extra_env: BTreeMap::from([
                ("Pass_word".to_owned(), "pw".to_owned()),
                ("API_KEY".to_owned(), "k3y".to_owned()),
                (
                    "DXMT_CONFIG_FILE".to_owned(),
                    format!("{}/Games/dxmt.conf", home.display()),
                ),
                (
                    "GST_PLUGIN_PATH".to_owned(),
                    format!("-I{}/engines", data.display()),
                ),
            ]),
            launch_exe_override: Some(format!(
                "{}/Library/Application Support/Steam/steamapps/common/Game/game.exe",
                home.display()
            )),
            launch_args_override: Some(format!(
                "-I{}/include -L{}/lib",
                home.display(),
                data.display()
            )),
            notes: Some(format!("runs from {}/Games", home.display())),
            ..GameConfig::default()
        };
        db.set_game_config(30, &config).unwrap();
        let preset = export(&db, 30, &data, Some(&home)).unwrap();
        let json = preset.to_json_pretty().unwrap();
        assert!(!json.contains("/Users/"), "home directory leaked: {json}");
        assert!(!json.contains("example"), "account name leaked: {json}");
        assert!(!json.contains("Pass_word"), "{json}");
        assert!(!json.contains("API_KEY"), "{json}");
        assert_eq!(
            preset.config.extra_env["DXMT_CONFIG_FILE"],
            "$HOME/Games/dxmt.conf"
        );
        assert_eq!(
            preset.config.extra_env["GST_PLUGIN_PATH"], "-I$APPLEJACK_DATA_DIR/engines",
            "the data dir is rewritten before the home directory"
        );
        assert_eq!(
            preset.config.launch_exe_override.as_deref(),
            Some("$HOME/Library/Application Support/Steam/steamapps/common/Game/game.exe")
        );
        assert_eq!(
            preset.config.launch_args_override.as_deref(),
            Some("-I$HOME/include -L$APPLEJACK_DATA_DIR/lib")
        );
        assert_eq!(
            preset.config.notes.as_deref(),
            Some("runs from $HOME/Games")
        );
    }

    #[test]
    fn export_rewrites_the_longer_directory_first() {
        let db = ConfigDb::open_in_memory().unwrap();
        let data = PathBuf::from("/tmp/hp");
        let home = data.join("acct");
        let config = GameConfig {
            notes: Some(format!(
                "save in {}/Saves; engines in {}/engines",
                home.display(),
                data.display()
            )),
            ..GameConfig::default()
        };
        db.set_game_config(31, &config).unwrap();
        let preset = export(&db, 31, &data, Some(&home)).unwrap();
        assert_eq!(
            preset.config.notes.as_deref(),
            Some("save in $HOME/Saves; engines in $APPLEJACK_DATA_DIR/engines"),
            "a home directory below the data dir is still rewritten as a whole"
        );
    }

    #[test]
    fn export_refuses_an_account_name_it_cannot_rewrite() {
        let db = ConfigDb::open_in_memory().unwrap();
        let data = data_dir();
        let home = home();
        let cases = [
            (
                "extra_env.DXMT_CONFIG_FILE",
                GameConfig {
                    extra_env: BTreeMap::from([(
                        "DXMT_CONFIG_FILE".to_owned(),
                        format!(
                            "{}/prefixes/g/drive_c/users/example/dxmt.conf",
                            data.display()
                        ),
                    )]),
                    ..GameConfig::default()
                },
            ),
            (
                "launch_args_override",
                GameConfig {
                    launch_args_override: Some("-savedir Z:\\Users\\EXAMPLE\\Saves".to_owned()),
                    ..GameConfig::default()
                },
            ),
            (
                "notes",
                GameConfig {
                    notes: Some("see file:///Users/example/x".to_owned()),
                    ..GameConfig::default()
                },
            ),
            // The override rewrites; the home path glued to `foo-` in the
            // notes does not, and the guard names the field that kept it.
            (
                "notes",
                GameConfig {
                    launch_exe_override: Some(format!("{}/y", home.display())),
                    notes: Some(format!("foo-{}/z", home.display())),
                    ..GameConfig::default()
                },
            ),
        ];
        for (expected, config) in cases {
            db.set_game_config(32, &config).unwrap();
            let error = export(&db, 32, &data, Some(&home)).unwrap_err();
            assert!(
                matches!(&error, Error::AccountNameInExport { field } if field == expected),
                "{expected}: {error}"
            );
            assert!(!error.to_string().contains("example"), "{error}");
        }

        // A word that is not a whole path component never trips the guard,
        // and without a home directory there is no account name to guard.
        let config = GameConfig {
            notes: Some("an example of examples/x, not the account".to_owned()),
            ..GameConfig::default()
        };
        db.set_game_config(33, &config).unwrap();
        export(&db, 33, &data, Some(&home)).unwrap();
        let config = GameConfig {
            notes: Some("see file:///Users/example/x".to_owned()),
            ..GameConfig::default()
        };
        db.set_game_config(34, &config).unwrap();
        export(&db, 34, &data, None).unwrap();
    }

    #[test]
    fn export_of_missing_row_is_an_empty_preset() {
        let db = ConfigDb::open_in_memory().unwrap();
        let preset = export(&db, 7, &data_dir(), None).unwrap();
        assert_eq!(preset.config, GameConfig::default());
        assert_eq!(preset.appid, 7);
        let json: serde_json::Value =
            serde_json::from_str(&preset.to_json_pretty().unwrap()).unwrap();
        assert_eq!(
            json["config"],
            serde_json::json!({ "extra_env": {} }),
            "unset columns are not written"
        );
        assert_eq!(Preset::from_json(&json.to_string()).unwrap(), preset);
    }

    #[test]
    fn secret_names_match_with_underscores_removed_and_any_case() {
        for name in [
            "MY_TOKEN",
            "steam_secret",
            "PASSWORD_X",
            "Pass_word",
            "API_KEY",
            "AUTH",
            "x_credential",
            "STEAM_PASS",
        ] {
            assert!(is_secret_name(name), "{name}");
        }
        for name in [
            "WINEDEBUG",
            "WINEPREFIX",
            "VK_ICD_FILENAMES",
            "GST_PLUGIN_PATH",
            "DXMT_CONFIG",
            "DXMT_CONFIG_FILE",
            "MTL_HUD_ENABLED",
            "ROSETTA_ADVERTISE_AVX",
        ] {
            assert!(!is_secret_name(name), "{name}");
        }
    }

    #[test]
    fn bounded_replacement_respects_path_boundaries() {
        assert_eq!(replace_bounded("/d/x", "/d", "$D"), "$D/x");
        assert_eq!(replace_bounded("/d", "/d", "$D"), "$D");
        assert_eq!(replace_bounded("/dx/y", "/d", "$D"), "/dx/y");
        assert_eq!(replace_bounded("/d-x/y", "/d", "$D"), "/d-x/y");
        assert_eq!(replace_bounded("a /d/x b /d", "/d", "$D"), "a $D/x b $D");
        assert_eq!(replace_bounded("$D/x", "$D", "/d"), "/d/x");
        assert_eq!(replace_bounded("$DX", "$D", "/d"), "$DX");
        assert_eq!(replace_bounded("/opt/d/x", "/d", "$D"), "/opt/d/x");
        assert_eq!(replace_bounded("x$D/y", "$D", "/d"), "x$D/y");
        assert_eq!(replace_bounded("-I/d/x", "/d", "$D"), "-I$D/x");
        assert_eq!(
            replace_bounded("-I/d/x -L/d/y", "/d", "$D"),
            "-I$D/x -L$D/y"
        );
        assert_eq!(replace_bounded("-I/dx/y", "/d", "$D"), "-I/dx/y");
        assert_eq!(
            replace_bounded("-x/opt/d/y", "/d", "$D"),
            "-x/opt/d/y",
            "the flag rule covers the flag letters only, not the rest of the token"
        );
        assert_eq!(replace_bounded("--p/d", "/d", "$D"), "--p$D");
        assert_eq!(replace_bounded("/opt-/d/x", "/d", "$D"), "/opt-/d/x");
        assert_eq!(replace_bounded("a=-I/d", "/d", "$D"), "a=-I$D");
        assert_eq!(replace_bounded("--prefix=/d", "/d", "$D"), "--prefix=$D");
        assert_eq!(
            replace_bounded("--root \"/d\" a=/d/x:/d/y", "/d", "$D"),
            "--root \"$D\" a=$D/x:$D/y"
        );
        assert_eq!(replace_bounded("$D:/x", "$D", "/d"), "/d:/x");
        assert_eq!(replace_bounded("/x", "", "y"), "/x");
        assert_eq!(replace_bounded("/x", "/", "y"), "/x");
    }

    #[test]
    fn lexical_normalization_folds_dot_components() {
        let normalized = |text: &str| normalize_lexically(Path::new(text));
        assert_eq!(normalized("/a/./b/../c"), PathBuf::from("/a/c"));
        assert_eq!(normalized("/a/../../b"), PathBuf::from("/b"));
        assert_eq!(normalized("a/../../b"), PathBuf::from("../b"));
        assert_eq!(normalized("./a/b/"), PathBuf::from("a/b"));
        assert_eq!(normalized("/"), PathBuf::from("/"));
    }

    #[test]
    fn unsupported_schema_is_refused() {
        struct Deny;
        impl EnvPolicy for Deny {
            fn allows(&self, _: &str) -> bool {
                false
            }
            fn value_check(&self, _: &str) -> ValueCheck<'_> {
                ValueCheck::Plain
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
        let db = ConfigDb::open_in_memory().unwrap();
        let error = import(
            &preset,
            &Deny,
            &db,
            &data_dir(),
            None,
            ImportOptions::default(),
        )
        .unwrap_err();
        assert!(
            matches!(error, Error::UnsupportedPresetSchema(2)),
            "{error}"
        );
    }
}
