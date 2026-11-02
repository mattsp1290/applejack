//! `applejack app run|cancel|register`, and `applejack run`, its plan name.
//! A run stays attached until the title exits and exits with its code.

use std::path::PathBuf;
use std::process::ExitCode;

use applejack_core::config::{Drm, SessionRow, SessionState};
use applejack_core::run::AppRegistration;
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
    /// Record a title already on disk from its `appmanifest_<appid>.acf`,
    /// until `applejack install` writes this itself
    Register {
        /// Steam app id
        appid: u32,
        /// unknown, none, or steam. Only `none` runs without a client
        #[arg(long, default_value = "unknown")]
        drm: Drm,
        /// The launch executable, relative to the install directory
        #[arg(long)]
        exe: Option<String>,
        /// The library root holding `steamapps/`; `<data>/library` by default
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

fn finish(outcome: Result<String, CoreError>) -> ExitCode {
    match outcome {
        Ok(text) => super::emit("app", &text),
        Err(error) => {
            eprintln!("applejack app: {error}");
            ExitCode::FAILURE
        }
    }
}

fn step(phase: &str, message: &str) {
    eprintln!("{phase}: {message}");
}

pub fn run_title(args: &RunArgs) -> ExitCode {
    match attached(args) {
        // The status of a child; 0 to 255 by construction of `& 0xff`.
        Ok(row) => ExitCode::from(u8::try_from(row.exit_code.unwrap_or(0) & 0xff).unwrap_or(1)),
        Err(error) => {
            eprintln!("applejack app: {error}");
            ExitCode::FAILURE
        }
    }
}

fn attached(args: &RunArgs) -> Result<SessionRow, CoreError> {
    let mut core = Core::from_env()?;
    drop(core.reconcile_sessions()?);
    let plan = core.plan_run(args.appid, args.probe_direct)?;
    eprintln!("session {} log {}", plan.session_id(), plan.log_path());
    let row = plan.run(&mut |phase, message| step(phase, &message))?;
    println!("{}", super::session::render(&row));
    Ok(row)
}

fn cancel(appid: u32, kill_prefix: bool) -> Result<String, CoreError> {
    let mut core = Core::from_env()?;
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
