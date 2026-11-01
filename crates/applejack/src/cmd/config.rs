//! `applejack config <appid> show|set|export|import`.
//!
//! Only a write creates `db.sqlite`: `show`, `export`, and the import plan
//! read a missing database as empty, and a `set` or `import` that has nothing
//! valid to write leaves the data directory as it found it.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use applejack_core::config::{DataDirError, Field, GameConfig};
use applejack_core::game_config::ConfigImportResult;
use applejack_core::{Core, CoreError};
use clap::{Args, Subcommand};

/// `EX_DATAERR` from sysexits(3): the preset holds items the user has not confirmed.
const EXIT_CONFIRMATION_REQUIRED: u8 = 65;

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

impl From<CoreError> for Failure {
    fn from(error: CoreError) -> Self {
        Self::Message(error.to_string())
    }
}

impl From<DataDirError> for Failure {
    fn from(error: DataDirError) -> Self {
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
    let mut core = Core::from_env()?;
    let appid = args.appid;
    match args.action {
        Action::Show { json } => {
            let config = core.config_get(appid)?;
            if json {
                return serde_json::to_string_pretty(&config)
                    .map_err(|error| Failure::Message(format!("cannot encode config: {error}")));
            }
            Ok(render(&config))
        }
        Action::Set { key, value } => {
            core.config_set(appid, &key, &value)?;
            Ok(format!("{key} = {value:?}"))
        }
        Action::Export { file } => {
            core.config_export(appid, &file)?;
            Ok(format!("exported appid {appid} to {}", file.display()))
        }
        Action::Import {
            file,
            accept_launch_overrides,
        } => import(&mut core, appid, &file, accept_launch_overrides),
    }
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

fn import(
    core: &mut Core,
    appid: u32,
    file: &Path,
    accept_launch_overrides: bool,
) -> Result<String, Failure> {
    let outcome = core.config_import(appid, file, accept_launch_overrides)?;
    // A preset for a different title is the easy way to slip a foreign
    // executable and arguments past the user, so say so.
    if outcome.preset_appid != appid {
        eprintln!(
            "applejack config: warning: preset was exported for appid {}, applying to {appid}",
            outcome.preset_appid
        );
    }
    let mut lines = render_plan(&outcome);
    if outcome.applied {
        lines.push(format!(
            "applied {} item(s) to appid {appid}",
            outcome.accepted.len()
        ));
    } else if outcome.confirmations.is_empty() {
        lines.push(format!("nothing to apply to appid {appid}"));
    } else {
        lines.push(
            "not applied: rerun with --accept-launch-overrides to accept the items above"
                .to_owned(),
        );
        // Print the report before signalling, so the user sees what needs confirming.
        super::emit("config", &lines.join("\n"));
        return Err(Failure::ConfirmationRequired);
    }
    Ok(lines.join("\n"))
}

/// One report line per item. A launch override is listed once, as `confirm`,
/// although it is also in `accepted` (it is written once confirmed).
fn render_plan(plan: &ConfigImportResult) -> Vec<String> {
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
