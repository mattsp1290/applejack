//! Command-line entry point for applejack.

use applejack_core::host::HostFacts;
use clap::{Parser, Subcommand};
use std::process::ExitCode;

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

#[derive(Subcommand)]
enum Command {
    /// Install, inspect, and roll back engines
    Engine,
    /// Sign in to Steam and list the owned library
    Steam,
    /// Download a title's Windows depot
    Install,
    /// Launch an installed title
    Run,
    /// Read and write per-game configuration
    Config,
    /// Create, snapshot, and reset Wine prefixes
    Prefix,
    /// Manage the shared Steam client prefix
    Runtime,
    /// Serve JSON-RPC over stdio for the app shell
    Serve,
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
            Self::Engine => "engine",
            Self::Steam => "steam",
            Self::Install => "install",
            Self::Run => "run",
            Self::Config => "config",
            Self::Prefix => "prefix",
            Self::Runtime => "runtime",
            Self::Serve => "serve",
            Self::Doctor { .. } => "doctor",
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Doctor { json } => doctor(json),
        stub => not_implemented(stub.name()),
    }
}

fn not_implemented(name: &str) -> ExitCode {
    eprintln!("applejack {name}: not implemented");
    ExitCode::from(EXIT_NOT_IMPLEMENTED)
}

fn doctor(json: bool) -> ExitCode {
    let facts = HostFacts::gather();
    if json {
        match serde_json::to_string_pretty(&facts) {
            Ok(text) => println!("{text}"),
            Err(error) => {
                eprintln!("applejack doctor: cannot encode facts: {error}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        print_facts(&facts);
    }
    ExitCode::SUCCESS
}

fn print_facts(facts: &HostFacts) {
    let free_disk = facts
        .free_disk_bytes
        .map_or_else(|| "unknown".to_owned(), |bytes| format!("{bytes} bytes"));
    let rows: [(&str, String); 6] = [
        ("macOS version", facts.macos_version.clone()),
        ("architecture", facts.arch.clone()),
        (
            "Rosetta",
            if facts.rosetta_present {
                "present"
            } else {
                "absent"
            }
            .to_owned(),
        ),
        ("free disk (data dir)", free_disk),
        ("screen recording", facts.screen_recording.to_string()),
        (
            "Developer ID identities",
            facts.developer_id_identities.to_string(),
        ),
    ];
    let width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    for (label, value) in &rows {
        println!("{label:<width$}  {value}");
    }
}
