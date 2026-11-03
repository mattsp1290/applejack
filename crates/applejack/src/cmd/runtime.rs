//! `applejack runtime steam install|status|update|repair|stop`: the Windows
//! Steam client in the shared `steam-client` prefix. `install` is the one
//! command that reaches the network, for Valve's bootstrapper; it leaves the
//! client running at its login window for the user to sign in.

use std::io::{self, Write};
use std::process::ExitCode;

use applejack_core::runtime::steam_client::{ClientOp, ClientRuntimeStatus};
use applejack_core::{Core, CoreError};
use clap::{Args, Subcommand};

#[derive(Args)]
pub struct RuntimeArgs {
    #[command(subcommand)]
    runtime: Runtime,
}

#[derive(Subcommand)]
enum Runtime {
    /// The Windows Steam client in the shared `steam-client` prefix
    Steam {
        #[command(subcommand)]
        action: Action,
    },
}

#[derive(Subcommand, Clone, Copy)]
enum Action {
    /// Download Valve's bootstrapper, verify its signature, install the
    /// client, and start it to its login window. With the client installed,
    /// starts it to the window
    Install,
    /// The runtime's state, build, and snapshots
    Status {
        /// Print as JSON
        #[arg(long)]
        json: bool,
    },
    /// Snapshot the prefix, then start the client so it updates itself
    Update,
    /// Restore the newest snapshot of the prefix
    Repair {
        /// Remove the client's package directory and rerun the bootstrapper
        #[arg(long)]
        reinstall: bool,
    },
    /// Stop the client (`wineserver -k` on the prefix)
    Stop,
}

pub fn run(args: &RuntimeArgs) -> ExitCode {
    let Runtime::Steam { action } = args.runtime;
    match execute(action) {
        Ok(text) => super::emit("runtime", &text),
        Err(error) => super::fail("runtime", &error),
    }
}

fn say(line: &str) {
    let _ = writeln!(io::stderr(), "{line}");
}

fn execute(action: Action) -> Result<String, CoreError> {
    let mut core = Core::from_env()?;
    let op = match action {
        Action::Status { json } => return status(&mut core, json),
        Action::Install => ClientOp::Install,
        Action::Update => ClientOp::Update,
        Action::Repair { reinstall } => ClientOp::Repair { reinstall },
        Action::Stop => ClientOp::Stop,
    };
    drop(core.reconcile_sessions()?);
    let plan = core.plan_client_op(op)?;
    let status = plan.run(&mut |phase, message| say(&format!("{phase}: {message}")))?;
    Ok(render(&status))
}

fn status(core: &mut Core, json: bool) -> Result<String, CoreError> {
    let status = core.client_runtime_status()?;
    if json {
        return Ok(serde_json::to_string_pretty(&status)?);
    }
    Ok(render(&status))
}

fn render(status: &ClientRuntimeStatus) -> String {
    format!(
        "steam client {} ({})\nprefix {}\nbuild {}\nrunning {}\nsnapshots {}",
        status.state,
        status.detail,
        status.prefix.display(),
        status.build_id.as_deref().unwrap_or("-"),
        if status.running { "yes" } else { "no" },
        status.snapshots,
    )
}
