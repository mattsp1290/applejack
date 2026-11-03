//! Command-line entry point for applejack.

mod cmd;

use applejack_core::host::HostFacts;
use clap::{Parser, Subcommand};
use std::io::{self, Write};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "applejack",
    version,
    about = "Windows Steam game runner for Apple Silicon"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install, inspect, and roll back engines
    Engine(cmd::engine::EngineArgs),
    /// Sign in to Steam and list the owned library
    Steam(cmd::steam::SteamArgs),
    /// Download a title's Windows depots, update them, or verify them
    Install(cmd::install::InstallArgs),
    /// Launch an installed title; `app run` under its plan name
    Run(cmd::app::RunArgs),
    /// Run, cancel, and register titles
    App(cmd::app::AppArgs),
    /// List past and running sessions
    Session(cmd::session::SessionArgs),
    /// Read and write per-game configuration
    Config(cmd::config::ConfigArgs),
    /// Create, snapshot, and reset Wine prefixes
    Prefix(cmd::prefix::PrefixArgs),
    /// Manage the shared Steam client prefix
    Runtime(cmd::runtime::RuntimeArgs),
    /// Serve JSON-RPC over stdio for the app shell
    Serve(cmd::serve::ServeArgs),
    /// Report host readiness facts
    Doctor {
        /// Print the facts as JSON
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Doctor { json } => doctor(json),
        Command::Config(args) => cmd::config::run(args),
        Command::Engine(args) => cmd::engine::run(args),
        Command::Prefix(args) => cmd::prefix::run(args),
        Command::Serve(args) => cmd::serve::run(args),
        Command::Run(args) => cmd::app::run_title(&args),
        Command::App(args) => cmd::app::run(args),
        Command::Session(args) => cmd::session::run(&args),
        Command::Steam(args) => cmd::steam::run(&args),
        Command::Install(args) => cmd::install::run(args),
        Command::Runtime(args) => cmd::runtime::run(&args),
    }
}

fn doctor(json: bool) -> ExitCode {
    let facts = HostFacts::gather();
    let text = if json {
        match serde_json::to_string_pretty(&facts) {
            Ok(text) => text,
            Err(error) => {
                eprintln!("applejack doctor: cannot encode facts: {error}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        render_facts(&facts)
    };
    emit(&text)
}

/// Writes `text` to stdout. A reader that closes the pipe early is not an
/// error for a report command, so `BrokenPipe` exits 0.
fn emit(text: &str) -> ExitCode {
    let mut out = io::stdout().lock();
    match writeln!(out, "{text}").and_then(|()| out.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("applejack doctor: {error}");
            ExitCode::FAILURE
        }
    }
}

fn render_facts(facts: &HostFacts) -> String {
    let free_disk = facts
        .free_disk_bytes
        .map_or_else(|| "unknown".to_owned(), |bytes| format!("{bytes} bytes"));
    let rosetta =
        facts.rosetta_present.map_or(
            "unknown",
            |present| {
                if present { "present" } else { "absent" }
            },
        );
    let identities = facts
        .developer_id_identities
        .map_or_else(|| "unknown".to_owned(), |count| count.to_string());
    let rows: [(&str, String); 6] = [
        ("macOS version", facts.macos_version.clone()),
        (
            "architecture",
            facts.arch.clone().unwrap_or_else(|| "unknown".to_owned()),
        ),
        ("Rosetta", rosetta.to_owned()),
        ("free disk (data dir)", free_disk),
        ("screen recording", facts.screen_recording.to_string()),
        ("Developer ID identities", identities),
    ];
    let width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    rows.iter()
        .map(|(label, value)| format!("{label:<width$}  {value}"))
        .collect::<Vec<_>>()
        .join("\n")
}
