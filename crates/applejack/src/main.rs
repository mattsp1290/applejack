//! Command-line entry point for applejack.

use applejack_core::host::HostFacts;
use clap::{Args, Parser, Subcommand};
use std::ffi::OsString;
use std::io::{self, Write};
use std::process::ExitCode;

mod cmd;

/// `EX_UNAVAILABLE` from sysexits(3): the subcommand exists but is not implemented yet.
const EXIT_NOT_IMPLEMENTED: u8 = 69;

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

/// Arguments a stub accepts and ignores, so callers see exit 69 rather than a
/// usage error until the subcommand's work package lands.
#[derive(Args)]
struct Pending {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
    _args: Vec<OsString>,
}

#[derive(Subcommand)]
enum Command {
    /// Install, inspect, and roll back engines
    Engine(Pending),
    /// Sign in to Steam and list the owned library
    Steam(Pending),
    /// Download a title's Windows depot
    Install(Pending),
    /// Launch an installed title
    Run(Pending),
    /// Read and write per-game configuration
    Config(Pending),
    /// Create, snapshot, and reset Wine prefixes
    Prefix(Pending),
    /// Manage the shared Steam client prefix
    Runtime(Pending),
    /// Serve JSON-RPC over stdio for the app shell
    Serve(cmd::serve::ServeArgs),
    /// Report host readiness facts
    Doctor {
        /// Print the facts as JSON
        #[arg(long)]
        json: bool,
    },
}

impl Command {
    fn name(&self) -> &'static str {
        match self {
            Self::Engine(_) => "engine",
            Self::Steam(_) => "steam",
            Self::Install(_) => "install",
            Self::Run(_) => "run",
            Self::Config(_) => "config",
            Self::Prefix(_) => "prefix",
            Self::Runtime(_) => "runtime",
            Self::Serve(_) => "serve",
            Self::Doctor { .. } => "doctor",
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Doctor { json } => doctor(json),
        Command::Serve(args) => cmd::serve::run(args),
        stub => not_implemented(stub.name()),
    }
}

fn not_implemented(name: &str) -> ExitCode {
    eprintln!("applejack {name}: not implemented");
    ExitCode::from(EXIT_NOT_IMPLEMENTED)
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
