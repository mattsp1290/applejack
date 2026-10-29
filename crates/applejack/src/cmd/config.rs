//! `applejack config <appid> show|set|export|import`.

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
/// and echoed back, so it is refused before `read_to_string`.
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
        Action::Show { json } => show(&config::open_db(&data_dir)?, args.appid, json),
        Action::Set { key, value } => {
            // Validate before opening, so a bad key or value never creates the database.
            let field = Field::parse(&key).ok_or_else(|| Error::UnknownField(key.clone()))?;
            GameConfig::default().set(field, &value)?;
            config::open_db(&data_dir)?.set_field(args.appid, &key, &value)?;
            Ok(format!("{key} = {value:?}"))
        }
        Action::Export { file } => export(
            &config::open_db(&data_dir)?,
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

fn show(db: &ConfigDb, appid: u32, json: bool) -> Result<String, Failure> {
    let config = db.game_config(appid)?.unwrap_or_default();
    if json {
        return serde_json::to_string_pretty(&config)
            .map_err(|error| Failure::Message(format!("cannot encode config: {error}")));
    }
    Ok(render(&config))
}

fn render(config: &GameConfig) -> String {
    let width = Field::names()
        .iter()
        .map(|name| name.len())
        .max()
        .unwrap_or(0);
    Field::ALL
        .iter()
        .map(|field| {
            let value = config.get(*field).unwrap_or_else(|| "(unset)".to_owned());
            format!("{:<width$}  {}", field.name(), escape_control(&value))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Escapes control characters the way `{:?}` does, without quoting the rest:
/// a stored value cannot move the cursor or forge a `show` line, and JSON
/// `extra_env` stays readable.
fn escape_control(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_control() {
                c.escape_debug().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
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
    let size = std::fs::metadata(file)
        .map_err(|error| cannot_read(&error))?
        .len();
    if size > MAX_PRESET_BYTES {
        return Err(cannot_read(&format!(
            "{size} bytes is larger than the {MAX_PRESET_BYTES} byte preset limit"
        )));
    }
    let text = std::fs::read_to_string(file).map_err(|error| cannot_read(&error))?;
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
    let db = config::open_db(data_dir)?;
    let policy = DefaultEnvPolicy::new(data_dir);
    let options = ImportOptions {
        accept_launch_overrides,
    };
    let plan = config::presets::import(&preset, &policy, &db, data_dir, home, options)?;
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
    plan.apply(&db)?;
    lines.push(format!(
        "applied {} field(s) to appid {appid}",
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
    for item in &plan.warnings {
        out.push(format!(
            "warning  {} = {:?}: {}",
            item.field, item.value, item.reason
        ));
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
    fn show_escapes_control_characters_only() {
        assert_eq!(escape_control("plain"), "plain");
        assert_eq!(escape_control("{\"A\":\"1\"}"), "{\"A\":\"1\"}");
        assert_eq!(
            escape_control("\u{1b}[1A\u{1b}[2K launch_exe_override (unset)\n"),
            "\\u{1b}[1A\\u{1b}[2K launch_exe_override (unset)\\n"
        );
    }
}
