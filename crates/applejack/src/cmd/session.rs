//! `applejack session list`.

use std::process::ExitCode;

use applejack_core::config::SessionRow;
use applejack_core::{Core, CoreError};
use clap::{Args, Subcommand};

#[derive(Args)]
pub struct SessionArgs {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// List sessions, newest first
    List {
        /// Only this title
        #[arg(long)]
        appid: Option<u32>,
        /// Print as JSON
        #[arg(long)]
        json: bool,
    },
}

pub fn run(args: &SessionArgs) -> ExitCode {
    let Action::List { appid, json } = args.action;
    match list(appid, json) {
        Ok(text) => super::emit("session", &text),
        Err(error) => {
            eprintln!("applejack session: {error}");
            ExitCode::FAILURE
        }
    }
}

fn list(appid: Option<u32>, json: bool) -> Result<String, CoreError> {
    let mut core = Core::from_env()?;
    drop(core.reconcile_sessions()?);
    let rows = core.session_list(appid)?;
    if json {
        return Ok(serde_json::to_string_pretty(&rows)?);
    }
    if rows.is_empty() {
        return Ok("no sessions".to_owned());
    }
    Ok(rows.iter().map(render).collect::<Vec<_>>().join("\n"))
}

/// One line per session.
pub fn render(row: &SessionRow) -> String {
    let exit = row
        .exit_code
        .map_or_else(|| "-".to_owned(), |code| code.to_string());
    let mut line = format!(
        "{} app {} {} exit {} started {} ended {} log {}",
        row.id,
        row.appid,
        row.state,
        exit,
        row.started_at,
        row.ended_at.as_deref().unwrap_or("-"),
        row.log_path,
    );
    if let Some(note) = &row.note {
        line.push_str(" (");
        line.push_str(note);
        line.push(')');
    }
    line
}
