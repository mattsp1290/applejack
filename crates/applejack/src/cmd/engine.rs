//! `applejack engine install|use|list|doctor|probe-cpuid|d3dmetal`.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use applejack_core::engines::describe_progress;
use applejack_core::{Core, CoreError};
use applejack_engine::Severity;
use clap::{Args, Subcommand};

#[derive(Args)]
pub struct EngineArgs {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Install an engine; `--from` takes a local unsigned bundle
    Install {
        /// Engine id, for example rosetta-wine
        id: String,
        /// Bundle directory or `.tar.zst`; needs
        /// `APPLEJACK_ALLOW_UNSIGNED_ENGINE=1`
        #[arg(long)]
        from: Option<PathBuf>,
    },
    /// Select the active version among installed ones
    Use {
        /// Engine id
        id: String,
        /// Installed version
        version: String,
    },
    /// Show installed and published versions
    List {
        /// Print as JSON
        #[arg(long)]
        json: bool,
    },
    /// Check whether the active engine can run on this host
    Doctor {
        /// Print as JSON
        #[arg(long)]
        json: bool,
    },
    /// Run the CPUID probe with and without `ROSETTA_ADVERTISE_AVX` and store
    /// the result
    ProbeCpuid,
    /// Manage the user-supplied `D3DMetal` overlay
    #[command(subcommand)]
    D3dmetal(D3dmetalAction),
}

#[derive(Subcommand)]
enum D3dmetalAction {
    /// Copy `D3DMetal` from a mounted Game Porting Toolkit volume
    Import {
        /// The mounted volume
        path: PathBuf,
        /// Accept Apple's license on the volume
        #[arg(long)]
        accept_apple_license: bool,
    },
    /// Delete the overlay
    Remove,
}

pub fn run(args: EngineArgs) -> ExitCode {
    match execute(args) {
        Ok(text) => super::emit("engine", &text),
        Err(error) => {
            eprintln!("applejack engine: {error}");
            ExitCode::FAILURE
        }
    }
}

fn execute(args: EngineArgs) -> Result<String, CoreError> {
    let mut core = Core::from_env()?;
    match args.action {
        Action::Install { id, from } => {
            let install = core.plan_install(&id, from.as_deref())?;
            let mut last_phase = "";
            let installed = install.run(&mut |progress| {
                let (phase, _, _, message) = describe_progress(progress);
                // Repeats of a phase are byte counts, which a terminal does
                // not need.
                if phase != last_phase {
                    last_phase = phase;
                    let _ = writeln!(io::stderr(), "{message}");
                }
            })?;
            let state = if installed.active {
                "active"
            } else {
                "not active"
            };
            Ok(format!(
                "installed {} {} at {} ({state})",
                installed.id,
                installed.version,
                installed.root.display()
            ))
        }
        Action::Use { id, version } => {
            core.engine_use(&id, &version)?;
            Ok(format!("{id} {version} is active"))
        }
        Action::List { json } => {
            let list = core.engine_list()?;
            if json {
                return Ok(serde_json::to_string_pretty(&list)?);
            }
            if list.engines.is_empty() {
                return Ok("no engines installed or published".to_owned());
            }
            let rows: Vec<String> = list
                .engines
                .iter()
                .map(|entry| {
                    let installed = if entry.installed {
                        "installed"
                    } else {
                        "published"
                    };
                    let active = if entry.active { " active" } else { "" };
                    format!("{} {} {installed}{active}", entry.engine_id, entry.version)
                })
                .collect();
            Ok(rows.join("\n"))
        }
        Action::Doctor { json } => {
            let report = core.engine_doctor()?;
            if json {
                return Ok(serde_json::to_string_pretty(&report)?);
            }
            let mut rows = vec![format!(
                "runnable: {}",
                if report.runnable { "yes" } else { "no" }
            )];
            rows.extend(report.findings.iter().map(|finding| {
                let severity = match finding.severity {
                    Severity::Info => "info",
                    Severity::Warning => "warning",
                    Severity::Error => "error",
                };
                format!("{severity:<7}  {}", finding.message)
            }));
            Ok(rows.join("\n"))
        }
        Action::ProbeCpuid => Ok(serde_json::to_string_pretty(&core.engine_probe_cpuid()?)?),
        Action::D3dmetal(D3dmetalAction::Import {
            path,
            accept_apple_license,
        }) => {
            let overlay = core.engine_d3dmetal_import(&path, accept_apple_license)?;
            Ok(format!(
                "imported D3DMetal {} into {}",
                overlay.gptk_version,
                overlay.overlay_dir.display()
            ))
        }
        Action::D3dmetal(D3dmetalAction::Remove) => {
            core.engine_d3dmetal_remove()?;
            Ok("removed the D3DMetal overlay".to_owned())
        }
    }
}
