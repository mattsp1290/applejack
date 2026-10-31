//! `applejack prefix create|list|snapshot|restore|reset|remove|exec`.

use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::ExitCode;

use applejack_core::prefixes::PrefixEntry;
use applejack_core::{Core, CoreError};
use applejack_engine::{GraphicsBackend, HidrawMode, SyncMode};
use clap::{Args, Subcommand};

#[derive(Args)]
pub struct PrefixArgs {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Create a prefix with the active engine
    Create {
        /// Prefix name
        name: String,
        /// dxmt, wined3d, vkd3d-proton, or d3dmetal
        #[arg(long, default_value = "dxmt")]
        backend: GraphicsBackend,
        /// msync, esync, or none; the bundle's first mode by default
        #[arg(long)]
        sync: Option<SyncMode>,
        /// enabled or disabled
        #[arg(long, default_value = "disabled")]
        hidraw: HidrawMode,
    },
    /// List prefixes
    List {
        /// Print as JSON
        #[arg(long)]
        json: bool,
    },
    /// Clone a prefix to a timestamped snapshot
    Snapshot {
        /// Prefix name
        name: String,
    },
    /// Swap a snapshot back in, after a safety snapshot
    Restore {
        /// Prefix name
        name: String,
        /// Snapshot timestamp
        timestamp: String,
    },
    /// Recreate a prefix with the active engine, after a safety snapshot
    Reset {
        /// Prefix name
        name: String,
        /// Copy `drive_c/users/` back from the safety snapshot
        #[arg(long)]
        keep_user_data: bool,
    },
    /// Delete a prefix; its snapshots stay
    Remove {
        /// Prefix name
        name: String,
    },
    /// Run a Windows executable in a prefix and return its exit code. For
    /// development; `applejack run` is the launch path
    Exec {
        /// Prefix name
        name: String,
        /// After `--`: the executable (a Windows path, a bare name, or an
        /// absolute path inside the data directory) and its arguments
        #[arg(last = true, required = true, value_name = "EXE [ARGS]")]
        child: Vec<OsString>,
    },
}

pub fn run(args: PrefixArgs) -> ExitCode {
    match execute(args) {
        Ok(Outcome::Text(text)) => super::emit("prefix", &text),
        // The status of a child; 0 to 255 by construction of `& 0xff`.
        Ok(Outcome::Exit(code)) => ExitCode::from(u8::try_from(code & 0xff).unwrap_or(1)),
        Err(error) => {
            eprintln!("applejack prefix: {error}");
            ExitCode::FAILURE
        }
    }
}

enum Outcome {
    Text(String),
    Exit(i32),
}

fn execute(args: PrefixArgs) -> Result<Outcome, CoreError> {
    let mut core = Core::from_env()?;
    let text = match args.action {
        Action::Create {
            name,
            backend,
            sync,
            hidraw,
        } => {
            let entry = core.prefix_create(&name, backend, sync, hidraw)?;
            format!("created {}", render(&entry))
        }
        Action::List { json } => {
            let list = core.prefix_list()?;
            if json {
                serde_json::to_string_pretty(&list)?
            } else if list.is_empty() {
                "no prefixes".to_owned()
            } else {
                let rows: Vec<String> = list.iter().map(render).collect();
                rows.join("\n")
            }
        }
        Action::Snapshot { name } => {
            let snapshot = core.prefix_snapshot(&name)?;
            format!("snapshot {} {}", snapshot.prefix, snapshot.timestamp)
        }
        Action::Restore { name, timestamp } => {
            let safety = core.prefix_restore(&name, &timestamp)?;
            format!(
                "restored {name} from {timestamp}; safety snapshot {}",
                safety.timestamp
            )
        }
        Action::Reset {
            name,
            keep_user_data,
        } => {
            let safety = core.prefix_reset(&name, keep_user_data)?;
            format!("reset {name}; safety snapshot {}", safety.timestamp)
        }
        Action::Remove { name } => {
            core.prefix_remove(&name)?;
            format!("removed {name}")
        }
        Action::Exec { name, child } => {
            let (exe, args) = child
                .split_first()
                .ok_or_else(|| CoreError::Message("exec needs an executable".to_owned()))?;
            let status = core.prefix_exec(&name, Path::new(exe), args, &mut io::stdout())?;
            return Ok(Outcome::Exit(status));
        }
    };
    Ok(Outcome::Text(text))
}

fn render(entry: &PrefixEntry) -> String {
    if let Some(problem) = &entry.problem {
        return format!("{} unusable: {problem}", entry.name);
    }
    format!(
        "{} engine {} backend {} sync {} hidraw {}",
        entry.name,
        entry.engine_version.as_deref().unwrap_or("?"),
        entry.backend.map_or("?", GraphicsBackend::as_str),
        entry.sync_mode.map_or("?", SyncMode::as_str),
        entry.hidraw.map_or("?", HidrawMode::as_str),
    )
}
