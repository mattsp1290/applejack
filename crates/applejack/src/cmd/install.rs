//! `applejack install <appid> [--branch] [--update] [--verify] [--library]`.
//! The refusals come first and need no Steam. `SIGINT` and `SIGTERM` stop the
//! download with its partial state kept, and the next `install` resumes.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use applejack_core::config::{AppRow, Drm};
use applejack_core::install::TitleInstall;
use applejack_core::steam::LiveSteam;
use applejack_core::{Core, CoreError};
use clap::Args;

/// A terminal needs far fewer lines than an RPC client gets.
const PROGRESS_EVERY: Duration = Duration::from_secs(2);

#[derive(Args)]
pub struct InstallArgs {
    /// Steam app id
    appid: u32,
    /// The branch to install; the app's default when left out
    #[arg(long)]
    branch: Option<String>,
    /// Fetch what changed since the installed manifests
    #[arg(long, conflicts_with = "verify")]
    update: bool,
    /// Hash the files against the stored manifests, offline
    #[arg(long)]
    verify: bool,
    /// The library root, an absolute path; the title's own or
    /// `<data>/library` by default
    #[arg(long)]
    library: Option<PathBuf>,
}

pub fn run(args: InstallArgs) -> ExitCode {
    match install(args) {
        Ok(row) => super::emit("install", &summary(&row)),
        Err(error) => super::fail("install", &error),
    }
}

fn install(args: InstallArgs) -> Result<AppRow, CoreError> {
    let appid = args.appid;
    let mut core = Core::from_env()?;
    drop(core.reconcile_sessions()?);
    let plan = core.plan_title_install(TitleInstall {
        appid,
        branch: args.branch,
        update: args.update,
        verify: args.verify,
        library: args.library,
    })?;
    super::on_signal(move || {
        if let Err(error) = Core::from_env()
            .map_err(CoreError::from)
            .and_then(|core| core.cancel_install(appid))
        {
            say(&format!("applejack install: {error}"));
        }
    });
    let last = Mutex::new(None::<Instant>);
    plan.run(&LiveSteam::default(), &|phase, done, total, message| {
        if let (Some(done), Some(total)) = (done, total) {
            let mut last = last
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if last.is_some_and(|at| at.elapsed() < PROGRESS_EVERY) && done < total {
                return;
            }
            *last = Some(Instant::now());
            say(&format!("{phase}: {done}/{total} bytes, {message}"));
        } else {
            say(&format!("{phase}: {message}"));
        }
    })
}

/// A title is not client-free until someone says so; this is where the user
/// learns how.
fn summary(row: &AppRow) -> String {
    let appid = row.appid;
    let name = row.name.as_deref().unwrap_or("?");
    let library = row.library_dir.as_deref().unwrap_or("?");
    let line = format!("installed {appid} {name} in {library}");
    if row.drm == Drm::None {
        return format!("{line}\nrun it with `applejack run {appid}`");
    }
    format!(
        "{line}\ndrm is {}: try it with `applejack run {appid} --probe-direct`, and if it runs, \
         record that with `applejack config {appid} set drm none`",
        row.drm
    )
}

fn say(line: &str) {
    let _ = writeln!(io::stderr(), "{line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_summary_names_the_route_to_a_client_free_run() {
        let unknown = AppRow {
            appid: 10,
            ..AppRow::default()
        };
        assert!(summary(&unknown).contains("applejack config 10 set drm none"));
        let free = AppRow {
            appid: 10,
            drm: Drm::None,
            ..AppRow::default()
        };
        assert!(summary(&free).contains("applejack run 10"));
        assert!(!summary(&free).contains("set drm none"));
    }
}
