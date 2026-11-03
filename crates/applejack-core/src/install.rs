//! `app.install`: refuse what cannot install, claim the title, then sign in,
//! download, and write the `apps` row.
//!
//! [`Core::plan_title_install`] is quick and needs no Steam, so every refusal
//! is an error response; [`PlannedInstall::run`] may take hours and belongs on
//! a job thread or the CLI's main one.
//!
//! A title's prefix lock is what keeps an install and a run apart, across
//! processes: a run holds it for the life of its session, and so does this for
//! the life of the job. `install-<appid>.cancel` under `locks/` is how another
//! process asks a running install to stop: the job's installer reads it once a
//! second, whichever process wrote it.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use applejack_engine::rosetta_wine::process::wineserver_running;
use applejack_engine::rosetta_wine::snapshot::PrefixLock;
use applejack_steam::{
    AppManifest, DepotInstaller, Error as SteamError, InstallPlan, InstallProgress, LaunchChoice,
    LaunchEntry, LibraryPaths, Lock, LockDir, Mode, OwnedApp, fetch_owned_app, verify_install,
};

use crate::config::{AppRow, Drm, GameConfig, LaunchMode};
use crate::error::CoreError;
use crate::run::{launch_config, printable};
use crate::runtime::direct::resolve_launch;
use crate::runtime::session::busy;
use crate::state::Core;
use crate::steam::{SteamHost, silent_logon, steam_runtime};

/// The prefix plan 05 keeps for the Windows Steam client.
const STEAM_CLIENT_PREFIX: &str = "steam-client";

/// A busy download reports per chunk; a pipe or a terminal needs far less.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// What `applejack install` and `app.install` ask for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TitleInstall {
    pub appid: u32,
    /// The app's default branch when `None`.
    pub branch: Option<String>,
    pub update: bool,
    pub verify: bool,
    /// An absolute library root. The title's own, or `<data>/library`, when
    /// `None`.
    pub library: Option<PathBuf>,
}

/// One step of an install: phase, bytes done, bytes in all, message.
pub type InstallReporter<'a> = &'a (dyn Fn(&str, Option<u64>, Option<u64>, String) + Send + Sync);

/// A claimed install: the job and prefix locks are held.
#[derive(Debug)]
pub struct PlannedInstall {
    core: Core,
    _job: Lock,
    _prefix: PrefixLock,
    request: TitleInstall,
    library: PathBuf,
    cancel: PathBuf,
}

impl Core {
    fn cancel_file(&self, appid: u32) -> PathBuf {
        self.data_dir()
            .join("locks")
            .join(format!("install-{appid}.cancel"))
    }

    /// Whether an install job of this title holds its lock in any process.
    fn install_running(&self, appid: u32) -> Result<bool, CoreError> {
        match LockDir::new(self.data_dir(), "probe").install_job(appid, Mode::NonBlocking) {
            Ok(_) => Ok(false),
            Err(SteamError::LockHeld { .. }) => Ok(true),
            Err(error) => Err(error.into()),
        }
    }

    /// Asks a running install of `appid` to stop, wherever it runs. `false`
    /// when there is none.
    ///
    /// # Errors
    ///
    /// The lock or file failure.
    pub fn cancel_install(&self, appid: u32) -> Result<bool, CoreError> {
        if !self.install_running(appid)? {
            return Ok(false);
        }
        let file = self.cancel_file(appid);
        fs::write(&file, b"").map_err(|error| CoreError::io(&file, error))?;
        Ok(true)
    }

    /// The prefix lock of a title nothing is running in. A busy lock with an
    /// open row is this title running, one with an install job is that
    /// download, and an open row behind a free lock is a dead owner's.
    /// `installing` says the caller holds the job lock itself.
    pub(crate) fn claim_idle_title(
        &mut self,
        appid: u32,
        name: &str,
        installing: bool,
    ) -> Result<PrefixLock, CoreError> {
        let lock = match self.lock_prefix(name) {
            Ok(lock) => lock,
            Err(error) if busy(&error) => {
                let open = self.reading(|db| Ok(db.open_session(appid)?))?;
                return Err(if open.is_some() {
                    CoreError::AlreadyRunning { appid }
                } else if !installing && self.install_running(appid).unwrap_or(false) {
                    CoreError::download_in_progress(appid, false)
                } else {
                    error
                });
            }
            Err(error) => return Err(error),
        };
        if let Some(row) = self.reading(|db| Ok(db.open_session(appid)?))?
            && self.settle_orphan(&row)?
        {
            return Err(CoreError::AlreadyRunning { appid });
        }
        Ok(lock)
    }

    /// The library root of an install: the request's, the title's own, or
    /// `<data>/library`, created and canonical.
    fn install_library(
        &self,
        request: &TitleInstall,
        existing: Option<&AppRow>,
    ) -> Result<PathBuf, CoreError> {
        let library = match (
            &request.library,
            existing.and_then(|row| row.library_dir.as_ref()),
        ) {
            (Some(library), _) if library.is_absolute() => library.clone(),
            (Some(library), _) => {
                return Err(CoreError::InvalidRequest(format!(
                    "the library {} is not an absolute path",
                    library.display()
                )));
            }
            (None, Some(own)) => PathBuf::from(own),
            (None, None) => self.data_dir().join("library"),
        };
        fs::create_dir_all(&library).map_err(|error| CoreError::io(&library, error))?;
        fs::canonicalize(&library).map_err(|error| CoreError::io(&library, error))
    }

    /// Every refusal an install has, then the claim. Nothing reaches Steam
    /// and nothing is written.
    ///
    /// # Errors
    ///
    /// [`CoreError::InvalidRequest`]; [`SteamError::LockHeld`] when another
    /// install of this title runs; [`CoreError::AlreadyRunning`] for an open
    /// session; [`SteamError::AlreadyInstalled`] and
    /// [`SteamError::ClientOwned`] for a title on disk;
    /// [`SteamError::ClientRunning`] while a Windows client runs.
    pub fn plan_title_install(
        &mut self,
        request: TitleInstall,
    ) -> Result<PlannedInstall, CoreError> {
        let appid = request.appid;
        if request.update && request.verify {
            return Err(CoreError::InvalidRequest(
                "`update` and `verify` are separate runs".to_owned(),
            ));
        }
        let job = LockDir::new(self.data_dir(), "install").install_job(appid, Mode::NonBlocking)?;
        let prefix = self.claim_idle_title(appid, &appid.to_string(), true)?;
        let existing = self.reading(|db| Ok(db.app(appid)?))?;
        let library = self.install_library(&request, existing.as_ref())?;
        let paths = LibraryPaths::new(&library);
        if !request.update && !request.verify {
            refuse_title_on_disk(&paths, appid)?;
        }
        if client_running(&self.prefix_dir(STEAM_CLIENT_PREFIX)?) {
            return Err(SteamError::ClientRunning { appid }.into());
        }
        let cancel = self.cancel_file(appid);
        // A file left by a run that ended first would stop this one.
        if let Err(error) = fs::remove_file(&cancel)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(CoreError::io(&cancel, error));
        }
        Ok(PlannedInstall {
            core: self.detached(),
            _job: job,
            _prefix: prefix,
            request,
            library,
            cancel,
        })
    }
}

/// Before any login: a title on disk is `--update`'s or `--verify`'s, and a
/// stopped update is `--update`'s alone. A manifest that does not parse is
/// left for the installer to report.
fn refuse_title_on_disk(paths: &LibraryPaths, appid: u32) -> Result<(), CoreError> {
    let Some(manifest) = fs::read_to_string(paths.acf(appid))
        .ok()
        .and_then(|text| AppManifest::parse(&text).ok())
    else {
        return Ok(());
    };
    Err(if !manifest.is_fully_installed() {
        SteamError::ClientOwned {
            appid,
            state_flags: manifest.state_flags,
        }
        .into()
    } else if paths.record(appid).exists() {
        CoreError::download_in_progress(appid, true)
    } else {
        SteamError::AlreadyInstalled { appid }.into()
    })
}

/// The ownership rule in `docs/steam-layer.md`, with the server's last act
/// counted too: it saves the registry after it stops listening, and the
/// client may still be writing. A server that cannot be checked counts as
/// running: the library is read-only then, which is the side a wrong answer
/// can afford.
fn client_running(prefix: &Path) -> bool {
    prefix.is_dir() && wineserver_running(prefix).unwrap_or(true)
}

fn client_launches(row: Option<&AppRow>, config: &GameConfig) -> bool {
    config.launch_mode == Some(LaunchMode::Client) || row.is_some_and(|row| row.drm == Drm::Steam)
}

/// Whether a run of this title needs the exe at all. A title the client
/// launches does not, and one with an override has its answer.
fn needs_launch_entry(row: Option<&AppRow>, config: &GameConfig) -> bool {
    let client = client_launches(row, config);
    let answered = row.is_some_and(|row| row.launch_exe.is_some())
        || config
            .launch_exe_override
            .as_deref()
            .is_some_and(|exe| !exe.trim().is_empty());
    !client && !answered
}

fn describe(entry: &LaunchEntry) -> String {
    let command = match &entry.arguments {
        Some(arguments) => format!("{} {arguments}", entry.executable),
        None => entry.executable.clone(),
    };
    printable(&command)
}

/// The app info's launch entry, or the refusal when a run would need one and
/// no single choice exists. It comes before the first byte of a download: the
/// way out is a setting, and it is cheaper to set before the bytes arrive.
fn pick_launch(
    app: &OwnedApp,
    plan: &InstallPlan,
    row: Option<&AppRow>,
    config: &GameConfig,
) -> Result<Option<LaunchEntry>, CoreError> {
    let appid = app.appid;
    match app.launch_choice(&plan.branch, &plan.filter) {
        LaunchChoice::One(entry) => Ok(Some(entry)),
        _ if !needs_launch_entry(row, config) => Ok(None),
        LaunchChoice::None => Err(CoreError::NoLaunchExe {
            appid,
            reason: format!(
                "its app info lists no Windows launch entry; set one with \
                 `applejack config {appid} set launch_exe_override <file>`"
            ),
        }),
        LaunchChoice::Several(entries) => Err(CoreError::LaunchExeAmbiguous {
            appid,
            candidates: entries.iter().map(describe).collect(),
        }),
    }
}

impl PlannedInstall {
    /// Runs the install to its end, then lets go of the title.
    ///
    /// # Errors
    ///
    /// [`SteamError::Cancelled`] after a request to stop; a download failure,
    /// which keeps the partial state for the next run;
    /// [`CoreError::VerifyFailed`]; and what signing in reports.
    pub fn run<H: SteamHost>(
        mut self,
        host: &H,
        report: InstallReporter<'_>,
    ) -> Result<AppRow, CoreError> {
        let outcome = if self.request.verify {
            self.verify(report)
        } else {
            self.download(host, report)
        };
        let _ = fs::remove_file(&self.cancel);
        outcome
    }

    fn verify(&mut self, report: InstallReporter<'_>) -> Result<AppRow, CoreError> {
        let appid = self.request.appid;
        let paths = LibraryPaths::new(&self.library);
        if paths.record(appid).exists() {
            report(
                "verify",
                None,
                None,
                CoreError::download_in_progress(appid, true).to_string(),
            );
        }
        report(
            "verify",
            None,
            None,
            "hashing the installed files".to_owned(),
        );
        let result = verify_install(&paths, self.core.data_dir(), appid)?;
        let row = self.sync_row()?;
        if !result.is_clean() {
            return Err(CoreError::VerifyFailed {
                appid,
                mismatched: result.mismatched.len(),
                missing: result.missing.len(),
                first: result
                    .mismatched
                    .iter()
                    .chain(&result.missing)
                    .next()
                    .cloned()
                    .unwrap_or_default(),
            });
        }
        report(
            "verify",
            None,
            None,
            format!("{} files match", result.files_checked),
        );
        Ok(row)
    }

    /// The `.acf` into the `apps` row: the Steam client may have moved it.
    fn sync_row(&mut self) -> Result<AppRow, CoreError> {
        let appid = self.request.appid;
        let paths = LibraryPaths::new(&self.library);
        let manifest = read_manifest(&paths, appid)?;
        self.core
            .record_title(appid, &manifest, &self.library, None, None)
    }

    fn download<H: SteamHost>(
        &mut self,
        host: &H,
        report: InstallReporter<'_>,
    ) -> Result<AppRow, CoreError> {
        let appid = self.request.appid;
        let account = self.core.steam_account()?;
        if self.request.update {
            self.sync_row()?;
        }
        let runtime = steam_runtime()?;
        let library = self.library.clone();
        let paths = LibraryPaths::new(&library);
        let data_dir = self.core.data_dir().to_path_buf();
        let client_prefix = self.core.prefix_dir(STEAM_CLIENT_PREFIX)?;
        let running = move || client_running(&client_prefix);
        let cancel_file = self.cancel.clone();
        let cancel = move || cancel_file.exists();
        let last_report = Mutex::new(None::<Instant>);
        let progress = |step: &InstallProgress| {
            let mut last = last_report
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let finished = step.completed_files >= step.total_files;
            if finished || last.is_none_or(|at: Instant| at.elapsed() >= PROGRESS_EVERY) {
                *last = Some(Instant::now());
                report(
                    "download",
                    Some(step.completed_bytes),
                    Some(step.total_bytes),
                    format!(
                        "depot {}: {} of {} files",
                        step.depot_id, step.completed_files, step.total_files
                    ),
                );
            }
        };
        let update = self.request.update;
        let core = &mut self.core;
        let request = &self.request;
        let (launch, resumed, branch, default_branch) = runtime.block_on(async {
            report("login", None, None, "signing in to Steam".to_owned());
            let signed_in = silent_logon(host, &data_dir, &account).await?;
            let session = &signed_in.session;
            report("resolve", None, None, format!("reading app {appid}"));
            let (app, filter) = fetch_owned_app(session, appid).await?;
            let row = core.reading(|db| Ok(db.app(appid)?))?;
            let stopped = paths.record(appid).exists();
            let branch = request
                .branch
                .clone()
                .or_else(|| {
                    row.as_ref()
                        .and_then(|row| row.installed_branch.clone())
                        .filter(|_| update || stopped)
                })
                .unwrap_or_else(|| app.default_branch.clone());
            let plan = InstallPlan::new(&app, &branch, &filter)?;
            let config = core.config_get(appid)?;
            let launch = pick_launch(&app, &plan, row.as_ref(), &config)?;
            report(
                "download",
                Some(0),
                Some(plan.size),
                printable(&format!("{} on {branch}", plan.name)),
            );
            let owner = Mutex::new(&mut *core);
            let before_write = || {
                owner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .note_install_started(&plan, &library, update)
                    .map_err(|error| error.to_string())
            };
            let installer = DepotInstaller::new(session, paths.clone(), &data_dir, &running)
                .with_progress(&progress)
                .with_cancel(&cancel)
                .with_before_write(&before_write);
            let outcome = if update {
                installer.update(&plan).await?
            } else {
                installer.install(&plan).await?
            };
            Ok::<_, CoreError>((launch, outcome.resumed, branch, app.default_branch))
        })?;
        report("register", None, None, format!("recording app {appid}"));
        let manifest = read_manifest(&paths, appid)?;
        self.core
            .record_title(appid, &manifest, &self.library, None, launch.as_ref())?;
        let row = self
            .core
            .note_install_finished(appid, &branch, &default_branch)?;
        self.report_launch(&row, report)?;
        if resumed {
            report(
                "register",
                None,
                None,
                "resumed a stopped download".to_owned(),
            );
        }
        Ok(row)
    }

    /// What a run would launch, while it is cheap to set another, and the
    /// reason it could not. The install stands either way: the files are
    /// whole, and the way out is a setting.
    fn report_launch(
        &mut self,
        row: &AppRow,
        report: InstallReporter<'_>,
    ) -> Result<(), CoreError> {
        let appid = row.appid;
        let config = self.core.config_get(appid)?;
        let client = client_launches(Some(row), &config);
        let merged = launch_config(row, &config);
        let Some(exe) = merged
            .launch_exe_override
            .as_deref()
            .filter(|exe| !exe.trim().is_empty())
        else {
            let line = if client {
                "launch exe: none needed, the Steam client launches it"
            } else {
                "launch exe: none recorded"
            };
            report("register", None, None, line.to_owned());
            return Ok(());
        };
        let chosen = match merged.launch_args_override.as_deref() {
            Some(arguments) if !arguments.is_empty() => format!("{exe} {arguments}"),
            _ => exe.to_owned(),
        };
        let source = if row.launch_exe.as_deref() == Some(exe) {
            ""
        } else {
            " (launch_exe_override)"
        };
        report(
            "register",
            None,
            None,
            format!("launch exe: {}{source}", printable(&chosen)),
        );
        let install_dir = row
            .installdir
            .as_deref()
            .map(|dir| LibraryPaths::new(&self.library).install_dir(dir));
        if let (false, Some(install_dir)) = (client, install_dir)
            && let Err(error) = resolve_launch(appid, &install_dir, &merged)
        {
            report(
                "register",
                None,
                None,
                format!("warning: {}", printable(&error.to_string())),
            );
        }
        Ok(())
    }
}

fn read_manifest(paths: &LibraryPaths, appid: u32) -> Result<AppManifest, CoreError> {
    let acf = paths.acf(appid);
    let text = match fs::read_to_string(&acf) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(SteamError::NotInstalled { appid }.into());
        }
        Err(error) => return Err(CoreError::io(&acf, error)),
    };
    Ok(AppManifest::parse(&text)?)
}
