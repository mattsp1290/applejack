//! `applejack app run|cancel|register`, and `applejack run`, its plan name.
//! `app cancel` also stops a running `applejack install`, in any process.
//! A run stays attached until the session ends and exits with the launch
//! exe's status, or 1 when no status is known or the title exited early.
//! `SIGINT` and `SIGTERM` cancel it.
//!
//! The owner of a session must not die of a closed pipe, so nothing here
//! panics on a write: `println!` and `eprintln!` do.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use applejack_core::config::{Drm, SessionRow, SessionState};
use applejack_core::run::AppRegistration;
use applejack_core::runtime::session::is_client_session;
use applejack_core::{Core, CoreError};
use clap::{Args, Subcommand};

#[derive(Args)]
pub struct AppArgs {
    #[command(subcommand)]
    action: Action,
}

#[derive(Args)]
pub struct RunArgs {
    /// Steam app id
    appid: u32,
    /// Launch the exe with no client, whatever the title's drm says
    #[arg(long)]
    probe_direct: bool,
}

#[derive(Subcommand)]
enum Action {
    /// Run an installed title and stay until it exits
    Run(RunArgs),
    /// End a running title, wherever it was started from
    Cancel {
        /// Steam app id
        appid: u32,
        /// Stop the whole prefix (`wineserver -k`) right away
        #[arg(long)]
        kill_prefix: bool,
    },
    /// Record a title `applejack install` did not put there, from its
    /// `appmanifest_<appid>.acf`
    Register {
        /// Steam app id
        appid: u32,
        /// unknown, none, or steam. Only `none` runs without a client.
        /// Without this a registered title keeps what it has
        #[arg(long)]
        drm: Option<Drm>,
        /// The launch executable, relative to the install directory
        #[arg(long)]
        exe: Option<String>,
        /// The library root holding `steamapps/`, an absolute path; the
        /// title's own or `<data>/library` by default
        #[arg(long)]
        library: Option<PathBuf>,
    },
}

pub fn run(args: AppArgs) -> ExitCode {
    match args.action {
        Action::Run(args) => run_title(&args),
        Action::Cancel { appid, kill_prefix } => finish(cancel(appid, kill_prefix)),
        Action::Register {
            appid,
            drm,
            exe,
            library,
        } => finish(register(&AppRegistration {
            appid,
            drm,
            exe,
            library,
        })),
    }
}

fn say(line: &str) {
    let _ = writeln!(io::stderr(), "{line}");
}

fn print_row(row: &SessionRow) {
    let _ = writeln!(io::stdout(), "{}", super::session::render(row));
}

fn finish(outcome: Result<String, CoreError>) -> ExitCode {
    match outcome {
        Ok(text) => super::emit("app", &text),
        Err(error) => {
            say(&format!("applejack app: {error}"));
            ExitCode::FAILURE
        }
    }
}

fn step(phase: &str, message: &str) {
    say(&format!("{phase}: {message}"));
}

/// A session with no status to report is not a success, except one the
/// Steam client ran to its end: the title's status is the client's, and
/// the marker clearing is all a clean exit shows.
fn exit_status(row: &SessionRow) -> u8 {
    let unknown = u8::from(!(is_client_session(row) && row.state == SessionState::Exited));
    // The status of a child; 0 to 255 by construction of `& 0xff`.
    row.exit_code
        .map_or(unknown, |code| u8::try_from(code & 0xff).unwrap_or(1))
}

pub fn run_title(args: &RunArgs) -> ExitCode {
    match attached(args) {
        Ok(row) => ExitCode::from(exit_status(&row)),
        Err(error) => {
            say(&format!("applejack app: {error}"));
            ExitCode::FAILURE
        }
    }
}

fn attached(args: &RunArgs) -> Result<SessionRow, CoreError> {
    let mut core = Core::from_env()?;
    drop(core.reconcile_sessions()?);
    let plan = core.plan_run(args.appid, args.probe_direct)?;
    let id = plan.session_id();
    say(&format!("session {id} log {}", plan.log_path()));
    cancel_on_signal(args.appid);
    match plan.run(&mut |phase, message| step(phase, &message)) {
        Ok(row) => {
            print_row(&row);
            Ok(row)
        }
        Err(error @ CoreError::TitleExitedEarly { .. }) => {
            let rows = core.session_list(Some(args.appid))?;
            if let Some(row) = rows.iter().find(|row| row.id == id) {
                print_row(row);
            }
            Err(error)
        }
        Err(error) => Err(error),
    }
}

/// The first signal cancels the title and the owner goes on to close the row.
/// A second one stops waiting for that: the row stays open and `app cancel`
/// or the next core settles it.
fn cancel_on_signal(appid: u32) {
    super::on_signal(move || cancel_for_signal(appid));
}

fn cancel_for_signal(appid: u32) {
    match end_session(appid) {
        Ok(_) | Err(CoreError::NotRunning { .. }) => {}
        Err(error) => say(&format!("applejack app: {error}")),
    }
}

fn end_session(appid: u32) -> Result<SessionRow, CoreError> {
    Core::from_env()?
        .plan_cancel(appid, false)?
        .run(&mut |phase, message| step(phase, &message))
}

fn cancel(appid: u32, kill_prefix: bool) -> Result<String, CoreError> {
    let mut core = Core::from_env()?;
    if core.cancel_install(appid)? {
        return Ok(format!("asked the install of {appid} to stop"));
    }
    drop(core.reconcile_sessions()?);
    let row = core
        .plan_cancel(appid, kill_prefix)?
        .run(&mut |phase, message| step(phase, &message))?;
    if row.state == SessionState::Cancelled {
        Ok(super::session::render(&row))
    } else {
        Err(CoreError::Message(format!(
            "session {} ended on its own: {}",
            row.id,
            super::session::render(&row)
        )))
    }
}

fn register(request: &AppRegistration) -> Result<String, CoreError> {
    let row = Core::from_env()?.app_register(request)?;
    Ok(format!(
        "registered {} {} drm {} in {}",
        row.appid,
        row.name.as_deref().unwrap_or("?"),
        row.drm,
        row.library_dir.as_deref().unwrap_or("?"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use applejack_core::config::SessionState;

    fn row(exit_code: Option<i32>, state: SessionState) -> SessionRow {
        SessionRow {
            id: 1,
            appid: 10,
            started_at: String::new(),
            ended_at: None,
            engine_id: String::new(),
            engine_version: String::new(),
            exit_code,
            log_path: String::new(),
            state,
            session_key: String::new(),
            prefix: String::new(),
            launch_exe: String::new(),
            note: None,
            cancel_requested: false,
        }
    }

    #[test]
    fn a_run_exits_with_the_status_and_never_0_for_an_unknown_one() {
        assert_eq!(exit_status(&row(Some(0), SessionState::Exited)), 0);
        assert_eq!(exit_status(&row(Some(9), SessionState::Exited)), 9);
        assert_eq!(exit_status(&row(Some(143), SessionState::Cancelled)), 143);
        assert_eq!(exit_status(&row(None, SessionState::Cancelled)), 1);
        assert_eq!(exit_status(&row(None, SessionState::Exited)), 1);
    }

    #[test]
    fn a_client_session_that_exited_is_a_success_and_a_cancelled_one_is_not() {
        let mut exited = row(None, SessionState::Exited);
        exited.prefix = "steam-client".to_owned();
        assert_eq!(exit_status(&exited), 0);
        let mut cancelled = row(None, SessionState::Cancelled);
        cancelled.prefix = "steam-client".to_owned();
        assert_eq!(exit_status(&cancelled), 1);
        let mut failed = row(None, SessionState::Failed);
        failed.prefix = "steam-client".to_owned();
        assert_eq!(exit_status(&failed), 1);
    }
}
