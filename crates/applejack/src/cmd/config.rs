//! `applejack config <appid> show|set|export|import`.
//!
//! Only a write creates `db.sqlite`: `show`, `export`, and the import plan
//! read a missing database as empty, and a `set` or `import` that has nothing
//! valid to write leaves the data directory as it found it.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use applejack_core::config::{
    self, ConfigDb, DefaultEnvPolicy, Error, Field, GameConfig, ImportOptions, ImportPlan, Preset,
};
use clap::{Args, Subcommand};

/// `EX_DATAERR` from sysexits(3): the preset holds items the user has not confirmed.
const EXIT_CONFIRMATION_REQUIRED: u8 = 65;

/// Largest preset file `import` reads. A preset is a few hundred bytes of
/// settings; anything bigger is read into memory, written into the database,
/// and echoed back, so the read stops one byte past the cap and the file is
/// refused.
const MAX_PRESET_BYTES: u64 = 1 << 20;

#[derive(Args)]
pub struct ConfigArgs {
    /// Steam app id
    appid: u32,
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Print the stored configuration
    Show {
        /// Print as JSON
        #[arg(long)]
        json: bool,
    },
    /// Set one field; an empty value clears it. `extra_env` takes NAME=VALUE,
    /// a bare NAME to remove, or a JSON object to replace
    Set {
        /// Column name, as listed by `show`
        key: String,
        /// New value
        #[arg(allow_hyphen_values = true)]
        value: String,
    },
    /// Write a shareable preset file (secrets and host paths stripped)
    Export {
        /// Destination file
        file: PathBuf,
    },
    /// Apply a preset file after filtering it through the engine env policy
    Import {
        /// Preset file
        file: PathBuf,
        /// Accept the launch executable and argument overrides the preset
        /// carries
        #[arg(long)]
        accept_launch_overrides: bool,
    },
}

enum Failure {
    Message(String),
    ConfirmationRequired,
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        match error {
            Error::ConfirmationRequired(_) => Self::ConfirmationRequired,
            other => Self::Message(other.to_string()),
        }
    }
}

impl From<config::DataDirError> for Failure {
    fn from(error: config::DataDirError) -> Self {
        Self::Message(error.to_string())
    }
}

pub fn run(args: ConfigArgs) -> ExitCode {
    match execute(args) {
        Ok(text) => super::emit("config", &text),
        Err(Failure::ConfirmationRequired) => ExitCode::from(EXIT_CONFIRMATION_REQUIRED),
        Err(Failure::Message(message)) => {
            eprintln!("applejack config: {message}");
            ExitCode::FAILURE
        }
    }
}

fn execute(args: ConfigArgs) -> Result<String, Failure> {
    let data_dir = config::data_dir()?;
    // The home directory is read here and passed down: the config crate never
    // consults the process environment.
    let home = config::home_dir_from(|name| std::env::var_os(name));
    match args.action {
        Action::Show { json } => show(&open_existing_or_memory(&data_dir)?, args.appid, json),
        Action::Set { key, value } => {
            // Validate before opening, so a bad key or value never creates the database.
            let field = Field::parse(&key).ok_or_else(|| Error::UnknownField(key.clone()))?;
            GameConfig::default().set(field, &value)?;
            config::open_db(&data_dir)?.set_field(args.appid, &key, &value)?;
            Ok(format!("{key} = {value:?}"))
        }
        Action::Export { file } => export(
            &open_existing_or_memory(&data_dir)?,
            args.appid,
            &data_dir,
            home.as_deref(),
            &file,
        ),
        Action::Import {
            file,
            accept_launch_overrides,
        } => import(
            args.appid,
            &data_dir,
            home.as_deref(),
            &file,
            accept_launch_overrides,
        ),
    }
}

/// The database when it exists. A missing one holds no rows, so a reader
/// gets the same answer from an empty in-memory one without creating
/// `db.sqlite`.
fn open_existing(data_dir: &Path) -> Result<Option<ConfigDb>, Error> {
    data_dir
        .join(config::DB_FILE)
        .exists()
        .then(|| config::open_db(data_dir))
        .transpose()
}

fn open_existing_or_memory(data_dir: &Path) -> Result<ConfigDb, Error> {
    open_existing(data_dir)?.map_or_else(ConfigDb::open_in_memory, Ok)
}

fn show(db: &ConfigDb, appid: u32, json: bool) -> Result<String, Failure> {
    let config = db.game_config(appid)?.unwrap_or_default();
    if json {
        return serde_json::to_string_pretty(&config)
            .map_err(|error| Failure::Message(format!("cannot encode config: {error}")));
    }
    Ok(render(&config))
}

/// One `field = "value"` line per field, the form `set` echoes and the
/// import report uses, so a `show` line pastes into `set`. Values are
/// Debug-quoted: control and bidirectional formatting characters come out
/// escaped, so a stored value cannot move the cursor, reverse a line, or
/// forge a `show` line. `--json` gives the exact form.
fn render(config: &GameConfig) -> String {
    let width = Field::names()
        .iter()
        .map(|name| name.len())
        .max()
        .unwrap_or(0);
    Field::ALL
        .iter()
        .map(|field| {
            let value = config
                .get(*field)
                .map_or_else(|| "(unset)".to_owned(), |value| format!("{value:?}"));
            format!("{:<width$} = {value}", field.name())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn export(
    db: &ConfigDb,
    appid: u32,
    data_dir: &Path,
    home: Option<&Path>,
    file: &Path,
) -> Result<String, Failure> {
    let preset = config::presets::export(db, appid, data_dir, home)?;
    let json = preset.to_json_pretty()?;
    std::fs::write(file, format!("{json}\n"))
        .map_err(|error| Failure::Message(format!("cannot write {}: {error}", file.display())))?;
    Ok(format!("exported appid {appid} to {}", file.display()))
}

fn import(
    appid: u32,
    data_dir: &Path,
    home: Option<&Path>,
    file: &Path,
    accept_launch_overrides: bool,
) -> Result<String, Failure> {
    let cannot_read = |error: &dyn std::fmt::Display| {
        Failure::Message(format!("cannot read {}: {error}", file.display()))
    };
    // Bound the read itself: a FIFO or character device reports no size, and
    // a regular file can grow between a size check and the read. The size is
    // checked before decoding, so a cut inside a multibyte character still
    // reports the size, not a UTF-8 error.
    let mut bytes = Vec::new();
    std::fs::File::open(file)
        .map_err(|error| cannot_read(&error))?
        .take(MAX_PRESET_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| cannot_read(&error))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_PRESET_BYTES {
        return Err(cannot_read(&format!(
            "larger than the {MAX_PRESET_BYTES} byte preset limit"
        )));
    }
    let text = String::from_utf8(bytes).map_err(|error| cannot_read(&error))?;
    let mut preset = Preset::from_json(&text)?;
    // The CLI's appid wins: a preset exported for one title may be applied to
    // another entry (a different depot of the same game, for instance). Say
    // so, because a preset for a different title is the easy way to slip a
    // foreign executable and arguments past the user.
    if preset.appid != appid {
        eprintln!(
            "applejack config: warning: preset was exported for appid {}, applying to {appid}",
            preset.appid
        );
        preset.appid = appid;
    }
    let policy = DefaultEnvPolicy::new(data_dir);
    let options = ImportOptions {
        accept_launch_overrides,
    };
    // The plan only reads; `db.sqlite` is created below, once there is
    // something to write.
    let existing = open_existing(data_dir)?;
    let scratch;
    let plan_db = if let Some(db) = &existing {
        db
    } else {
        scratch = ConfigDb::open_in_memory()?;
        &scratch
    };
    let plan = config::presets::import(&preset, &policy, plan_db, data_dir, home, options)?;
    let mut lines = render_plan(&plan);
    if plan.needs_confirmation() {
        lines.push(
            "not applied: rerun with --accept-launch-overrides to accept the items above"
                .to_owned(),
        );
        // Print the report before signalling, so the user sees what needs confirming.
        super::emit("config", &lines.join("\n"));
        return Err(Failure::ConfirmationRequired);
    }
    if plan.accepted.is_empty() {
        lines.push(format!("nothing to apply to appid {appid}"));
        return Ok(lines.join("\n"));
    }
    let db = if let Some(db) = existing {
        db
    } else {
        config::open_db(data_dir)?
    };
    plan.apply(&db)?;
    lines.push(format!(
        "applied {} item(s) to appid {appid}",
        plan.accepted.len()
    ));
    Ok(lines.join("\n"))
}

/// One report line per item. A launch override is listed once, as `confirm`,
/// although it is also in `accepted` (it is written once confirmed).
fn render_plan(plan: &ImportPlan) -> Vec<String> {
    // Values come from an untrusted file; Debug formatting escapes newlines
    // and terminal control sequences so a preset cannot forge report lines.
    let mut out = Vec::new();
    for item in &plan.accepted {
        if plan
            .confirmations
            .iter()
            .any(|confirmation| confirmation.field == item.field)
        {
            continue;
        }
        out.push(format!("accept   {} = {:?}", item.field, item.value));
    }
    for item in &plan.rejected {
        out.push(format!(
            "reject   {} = {:?}: {}",
            item.field, item.value, item.reason
        ));
    }
    for item in &plan.confirmations {
        out.push(format!("confirm  {} = {:?}", item.field, item.value));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_quotes_values_and_escapes_control_and_bidi_characters() {
        let config = GameConfig {
            notes: Some("\u{1b}[1A\u{1b}[2K launch_exe_override (unset)\n".to_owned()),
            launch_args_override: Some("safe \u{202e} hs/nib/ \u{2066}x \u{200e}".to_owned()),
            extra_env: std::collections::BTreeMap::from([("A".to_owned(), "1".to_owned())]),
            ..GameConfig::default()
        };
        let text = render(&config);
        let line = |name: &str| {
            text.lines()
                .find(|line| line.starts_with(name))
                .unwrap_or_else(|| panic!("{name} missing in {text}"))
                .to_owned()
        };
        assert!(line("notes").ends_with("\"\\u{1b}[1A\\u{1b}[2K launch_exe_override (unset)\\n\""));
        assert!(
            line("launch_args_override")
                .ends_with("\"safe \\u{202e} hs/nib/ \\u{2066}x \\u{200e}\"")
        );
        assert!(line("extra_env").ends_with("\"{\\\"A\\\":\\\"1\\\"}\""));
        assert!(line("engine_id").ends_with("(unset)"));
        assert!(
            line("notes").contains(" = \""),
            "the `field = value` form set echoes: {text}"
        );
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("launch_exe_override"))
                .count(),
            1
        );
    }
}
