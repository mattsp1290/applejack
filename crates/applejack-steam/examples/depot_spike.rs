//! Live check of the session, library, and depot code. Runs only with
//! `APPLEJACK_LIVE_STEAM=1`; see `docs/steam-layer.md` under "Running the
//! depot spike".

use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use applejack_steam::auth::{
    AuthError, AuthMode, Authenticator, Credentials, GuardFactor, License, LoginStep, Password,
};
use applejack_steam::depot::{DepotInstaller, InstallPlan, LibraryPaths, verify_install};
use applejack_steam::keychain::InMemoryStore;
use applejack_steam::library::{DEFAULT_BRANCH, Library};
use applejack_steam::live::{LiveChunks, SteamroomConnection, SteamroomSession};
use applejack_steam::session::{AppInfo, PackageGrants, SteamSession};
use steamroom::depot::DepotKey;
use steamroom::depot::manifest::DepotManifest;

const USAGE: &str = "\
depot_spike: live check of the Steam library and depot code (not run; nothing was contacted).

Run with your own Steam account:

  APPLEJACK_LIVE_STEAM=1 cargo run -p applejack-steam --example depot_spike
  APPLEJACK_LIVE_STEAM=1 cargo run -p applejack-steam --example depot_spike -- \\
      --install 480 --dir /tmp/applejack-spike

The first form logs in and prints how many owned apps have a Windows depot. The
second also installs that app into <dir>/steamapps/common and verifies it. Use a
small title your account owns; Spacewar (480) is the usual one once it is on the
account. A <dir> that holds anything else and no applejack record for that app
is refused before login; --force installs over it. Each Steam call is printed
before it is made, so the last line before a failure names the call that failed.

The spike logs in with your account name and password, read from the terminal
without echo. When Steam asks for a Steam Guard code, enter the code here and do
not also approve the sign-in in the mobile app: doing both fails with
DuplicateRequest. It holds the refresh token in memory only and never prints it.
Quit any Steam client that maps <dir> first.";

struct Args {
    install: Option<(u32, PathBuf)>,
    force: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut appid = None;
    let mut dir = None;
    let mut force = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--install" => {
                let value = args.next().ok_or("--install needs an appid")?;
                appid = Some(value.parse::<u32>().map_err(|_| "appid is not a number")?);
            }
            "--dir" => dir = Some(PathBuf::from(args.next().ok_or("--dir needs a path")?)),
            "--force" => force = true,
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    match (appid, dir) {
        (Some(appid), Some(dir)) => Ok(Args {
            install: Some((appid, dir)),
            force,
        }),
        (None, None) => Ok(Args {
            install: None,
            force,
        }),
        _ => Err("--install and --dir go together".to_owned()),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    if std::env::var("APPLEJACK_LIVE_STEAM").as_deref() != Ok("1") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("depot_spike: {message}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("depot_spike: {error}");
            ExitCode::FAILURE
        }
    }
}

/// A mistyped `--dir` must not land on someone's files. A record means this
/// directory holds a partial install of this app.
fn check_target(dir: &Path, record: &Path, force: bool) -> Result<(), String> {
    let occupied = fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some());
    if occupied && !force && !record.exists() {
        return Err(format!(
            "{} is not empty and holds no applejack record for this app; use a new --dir, or --force to install over it",
            dir.display()
        ));
    }
    Ok(())
}

fn step(call: &str) {
    println!("call: {call}");
}

/// Names each Steam call before making it. No argument is printed that a
/// session would treat as secret: only ids.
struct Traced<'a>(&'a SteamroomSession);

impl SteamSession for Traced<'_> {
    type Chunks = LiveChunks;

    fn heartbeat_interval(&self) -> Option<Duration> {
        self.0.heartbeat_interval()
    }

    async fn license_list(&self) -> Result<Vec<License>, AuthError> {
        step("license list");
        self.0.license_list().await
    }

    async fn package_grants(&self, licenses: &[License]) -> Result<PackageGrants, AuthError> {
        step(&format!("package info for {} licenses", licenses.len()));
        self.0.package_grants(licenses).await
    }

    async fn app_info(&self, appids: &[u32]) -> Result<Vec<AppInfo>, AuthError> {
        step(&format!("product info for {} apps", appids.len()));
        self.0.app_info(appids).await
    }

    async fn depot_key(&self, appid: u32, depot_id: u32) -> Result<DepotKey, AuthError> {
        step(&format!("decryption key for app {appid} depot {depot_id}"));
        self.0.depot_key(appid, depot_id).await
    }

    async fn manifest(
        &self,
        appid: u32,
        depot_id: u32,
        manifest_id: u64,
        branch: &str,
    ) -> Result<Option<DepotManifest>, AuthError> {
        step(&format!(
            "manifest {manifest_id} of app {appid} depot {depot_id} on {branch}"
        ));
        self.0.manifest(appid, depot_id, manifest_id, branch).await
    }

    async fn chunks(&self, appid: u32, depot_id: u32) -> Result<Arc<LiveChunks>, AuthError> {
        step(&format!("chunk source for app {appid} depot {depot_id}"));
        self.0.chunks(appid, depot_id).await
    }
}

async fn login() -> Result<SteamroomSession, Box<dyn std::error::Error>> {
    let credentials = Credentials {
        account_name: prompt_line("Steam account name: ")?,
        password: Password::new(rpassword::prompt_password("Steam password: ")?),
    };
    let store = InMemoryStore::default();
    let mut connection = SteamroomConnection::new();
    let mut auth = Authenticator::new(&mut connection, &store);
    let mut step = auth
        .begin(AuthMode::Credentials, Some(&credentials))
        .await?;
    let account_name = loop {
        step = match step {
            LoginStep::QrReady { .. } => return Err("unexpected QR step".into()),
            LoginStep::GuardRequired {
                factor: GuardFactor::DeviceConfirmation,
            } => {
                println!("Approve the sign-in in the Steam mobile app.");
                auth.finish().await?
            }
            LoginStep::GuardRequired { factor } => {
                let code = prompt_line(&format!("Steam Guard code ({factor:?}): "))?;
                auth.submit_guard_code(code.trim()).await?
            }
            LoginStep::Done { account_name } => break account_name,
        };
    };
    Ok(auth.refresh(&account_name).await?)
}

async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    if let Some((appid, dir)) = &args.install {
        check_target(dir, &LibraryPaths::new(dir).record(*appid), args.force)?;
    }
    step("auth login and CM logon");
    let session = login().await?;
    println!("logon ok");
    let session = Traced(&session);

    let library = Library::fetch(&session).await?;
    println!(
        "owned apps with a Windows depot: {} ({} described, skipped: {:?})",
        library.windows_apps().count(),
        library.apps.len(),
        library.skipped
    );

    let Some((appid, dir)) = args.install else {
        return Ok(());
    };
    let app = library.windows_app(appid)?;
    let plan = InstallPlan::new(app, DEFAULT_BRANCH, &library.filter())?;
    println!(
        "installing {} ({} depots, {} bytes, not licensed: {:?})",
        plan.name,
        plan.depots.len(),
        plan.size,
        plan.not_licensed
    );

    let paths = LibraryPaths::new(&dir);
    let client_running = || false;
    let progress = |report: &applejack_steam::InstallProgress| {
        println!(
            "depot {}: {}/{} files, {} bytes",
            report.depot_id, report.completed_files, report.total_files, report.completed_bytes
        );
    };
    let outcome = DepotInstaller::new(&session, paths.clone(), &dir, &client_running)
        .with_progress(&progress)
        .install(&plan)
        .await?;
    println!(
        "installed: {} files written, resumed: {}",
        outcome.files_written, outcome.resumed
    );

    let verify_dir = dir.clone();
    let report =
        tokio::task::spawn_blocking(move || verify_install(&paths, &verify_dir, appid)).await??;
    println!(
        "verify: {} files, {} mismatched, {} missing",
        report.files_checked,
        report.mismatched.len(),
        report.missing.len()
    );
    if !report.is_clean() {
        return Err("verify found differences".into());
    }
    println!("depot spike: green");
    Ok(())
}

fn prompt_line(prompt: &str) -> io::Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_with_other_files_is_refused_unless_forced_or_resuming() {
        let dir = tempfile::tempdir().unwrap();
        let paths = LibraryPaths::new(dir.path());
        let record = paths.record(480);

        assert!(check_target(dir.path(), &record, false).is_ok());
        assert!(check_target(&dir.path().join("new"), &record, false).is_ok());

        fs::write(dir.path().join("thesis.doc"), b"mine").unwrap();
        let refused = check_target(dir.path(), &record, false).unwrap_err();
        assert!(refused.contains("--force"), "{refused}");
        assert!(check_target(dir.path(), &record, true).is_ok());

        fs::create_dir_all(record.parent().unwrap()).unwrap();
        fs::write(&record, b"{}").unwrap();
        assert!(check_target(dir.path(), &record, false).is_ok());
    }
}
