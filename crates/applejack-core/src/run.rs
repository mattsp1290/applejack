//! `app.run`: refuse what cannot run, claim the prefix and a session row,
//! then bring the prefix up, launch the title, and watch it to its end.
//!
//! [`Core::plan_run`] is quick and answers the request, so every refusal is
//! an error response; [`PlannedRun::run`] may take as long as the title does
//! and belongs on a job thread or the CLI's main one. The lock travels with
//! the plan.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use applejack_engine::rosetta_wine::launch::{check_dxmt_copies, new_session_id};
use applejack_engine::rosetta_wine::snapshot::{self, PrefixLock};
use applejack_engine::rosetta_wine::{CpuidProbe, utc_now};
use applejack_engine::{
    Engine, GraphicsBackend, HidrawMode, InstalledEngine, LaunchSpec, Prefix, Session, SyncMode,
    Version,
};
use applejack_steam::{AppManifest, LibraryPaths};

use crate::config::{AppRow, Drm, GameConfig, NewSession, SessionEnd, SessionRow, SessionState};
use crate::engines::META_CPUID_PROBE;
use crate::error::CoreError;
use crate::prefixes::pick_sync;
use crate::runtime::direct::{DirectLaunch, resolve_launch, takes_direct_path};
use crate::runtime::session::{
    NOTE_EARLY, NOTE_LINGERED, OwnedProbe, WatchEnd, busy, log_line, watch,
};
use crate::state::Core;

/// The note on a `--probe-direct` run. `direct_verified_at` needs the frame
/// probe's game-owned window, which does not exist yet, and a title that
/// stays up behind a "start Steam" dialog must not pass.
pub const NOTE_PROBE: &str =
    "direct probe: no frame probe yet, so direct_verified_at was not recorded";

const DEFAULT_BACKEND: GraphicsBackend = GraphicsBackend::Dxmt;

/// A title on disk, as `app register` finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppRegistration {
    pub appid: u32,
    pub drm: Drm,
    /// Written to `launch_exe_override`, relative to the install directory.
    pub exe: Option<String>,
    /// The library root; `<data>/library` when `None`.
    pub library: Option<PathBuf>,
}

/// What a run wants of the prefix; `None` keeps what it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PrefixWish {
    backend: Option<GraphicsBackend>,
    sync_mode: Option<SyncMode>,
    hidraw: Option<HidrawMode>,
}

impl PrefixWish {
    fn of(config: &GameConfig) -> Result<Self, CoreError> {
        Ok(Self {
            backend: config
                .graphics_backend
                .map(|backend| backend.as_str().parse::<GraphicsBackend>())
                .transpose()?,
            sync_mode: config
                .sync_mode
                .map(|mode| mode.as_str().parse::<SyncMode>())
                .transpose()?,
            hidraw: config.hidraw_enabled.map(|enabled| {
                if enabled {
                    HidrawMode::Enabled
                } else {
                    HidrawMode::Disabled
                }
            }),
        })
    }
}

/// A claimed session: the prefix lock is held and the row is open.
#[derive(Debug)]
pub struct PlannedRun {
    core: Core,
    _lock: PrefixLock,
    session: SessionRow,
    installed: InstalledEngine,
    library: PathBuf,
    wish: PrefixWish,
    spec: LaunchSpec,
    probe_direct: bool,
}

impl PlannedRun {
    /// The `sessions` row.
    #[must_use]
    pub const fn session_id(&self) -> i64 {
        self.session.id
    }

    /// Where the title's output goes.
    #[must_use]
    pub fn log_path(&self) -> &str {
        &self.session.log_path
    }

    /// Runs the title to its end and closes the row, then lets go of the
    /// prefix. `report` gets each step as a phase and a message.
    ///
    /// # Errors
    ///
    /// A failure before the title started, recorded as
    /// [`SessionState::Failed`], or [`CoreError::TitleExitedEarly`].
    pub fn run(mut self, report: &mut dyn FnMut(&str, String)) -> Result<SessionRow, CoreError> {
        match self.launch_and_watch(report) {
            Ok(end) => self.close(end.as_ref(), report),
            Err(error) => {
                let text = error.to_string();
                let _ = log_line(&self.session.log_path, &format!("failed: {text}"));
                self.core.db()?.end_session(
                    self.session.id,
                    &SessionEnd {
                        ended_at: &utc_now(),
                        state: SessionState::Failed,
                        exit_code: None,
                        note: Some(&text),
                    },
                )?;
                Err(error)
            }
        }
    }

    fn cancel_requested(&mut self) -> Result<bool, CoreError> {
        Ok(self.core.session_row(self.session.id)?.cancel_requested)
    }

    /// `None` when a cancel arrived before the title started.
    fn launch_and_watch(
        &mut self,
        report: &mut dyn FnMut(&str, String),
    ) -> Result<Option<WatchEnd>, CoreError> {
        let prefix = self.ensure_prefix(report)?;
        if self.cancel_requested()? {
            return Ok(None);
        }
        report("starting", self.session.launch_exe.clone());
        let mut session = self
            .core
            .engine
            .launch(&self.installed, &prefix, &self.spec)?;
        // A cancel that read no process before the spawn left this to us; one
        // written after this read finds the process.
        if self.cancel_requested()? {
            session.cancel()?;
        }
        self.watch_title(&mut session, report).map(Some)
    }

    fn watch_title(
        &mut self,
        session: &mut Session,
        report: &mut dyn FnMut(&str, String),
    ) -> Result<WatchEnd, CoreError> {
        let exe = self.session.launch_exe.clone();
        let id = self.session.id;
        let tuning = self.core.tuning;
        let core = &mut self.core;
        let mut probe = OwnedProbe { session, exe: &exe };
        watch(&mut probe, &tuning, false, &mut || {
            core.db()?.mark_session_running(id, &utc_now())?;
            report("running", format!("session {id}"));
            Ok(())
        })
    }

    fn close(
        &mut self,
        end: Option<&WatchEnd>,
        report: &mut dyn FnMut(&str, String),
    ) -> Result<SessionRow, CoreError> {
        let id = self.session.id;
        let cancelled = self.cancel_requested()?;
        let state = if cancelled {
            SessionState::Cancelled
        } else {
            SessionState::Exited
        };
        let exit_code = end.and_then(|end| end.exit_code);
        let ended_at = end.map_or_else(utc_now, |end| end.ended_at.clone());
        let lingered = end.is_some_and(|end| !end.drained);
        let early_exit = self.core.tuning.early_exit;
        let early = state == SessionState::Exited && end.is_some_and(|e| e.lived < early_exit);
        let notes: Vec<&str> = [
            early.then_some(NOTE_EARLY),
            lingered.then_some(NOTE_LINGERED),
            self.probe_direct.then_some(NOTE_PROBE),
        ]
        .into_iter()
        .flatten()
        .collect();
        let note = (!notes.is_empty()).then(|| notes.join("; "));
        self.core.db()?.end_session(
            id,
            &SessionEnd {
                ended_at: &ended_at,
                state,
                exit_code,
                note: note.as_deref(),
            },
        )?;
        report(state.as_str(), format!("session {id} ended"));
        if early {
            return Err(CoreError::TitleExitedEarly {
                appid: self.session.appid,
                seconds: early_exit.as_secs(),
            });
        }
        self.core.session_row(id)
    }

    /// Creates the prefix on first use, else waits for a server left by the
    /// last run and applies what the configuration changed. An engine change
    /// takes a snapshot first (decision 9).
    fn ensure_prefix(&self, report: &mut dyn FnMut(&str, String)) -> Result<Prefix, CoreError> {
        let name = &self.session.prefix;
        let dir = self.core.prefix_dir(name)?;
        let wish = self.wish;
        let installed = &self.installed;
        if !dir.is_dir() {
            report("prefix", format!("creating prefix {name}"));
            return self.core.build_prefix(
                name,
                installed,
                wish.backend.unwrap_or(DEFAULT_BACKEND),
                wish.sync_mode,
                wish.hidraw.unwrap_or(HidrawMode::Disabled),
                self.library.clone(),
            );
        }
        Core::settle(&dir)?;
        let (_, prefix) = self.core.open_prefix(name)?;
        let backend = wish.backend.unwrap_or(prefix.backend);
        let hidraw = wish.hidraw.unwrap_or(prefix.hidraw);
        let sync_mode = pick_sync(installed, Some(wish.sync_mode.unwrap_or(prefix.sync_mode)))?;
        let engine_changes = prefix.engine_version != installed.version;
        if !engine_changes
            && backend == prefix.backend
            && sync_mode == prefix.sync_mode
            && hidraw == prefix.hidraw
        {
            return Ok(prefix);
        }
        if engine_changes {
            report(
                "prefix",
                format!("snapshot of {name} before an engine change"),
            );
            snapshot::create(&dir, &self.core.snapshots_root(), name)?;
        }
        report("prefix", format!("applying settings to prefix {name}"));
        Ok(self
            .core
            .engine
            .apply_overrides(installed, &prefix, backend, sync_mode, hidraw)?)
    }
}

impl Core {
    /// Every refusal `app.run` has, then the claim. Nothing is started.
    ///
    /// # Errors
    ///
    /// [`CoreError::TitleNotInstalled`], [`CoreError::DownloadInProgress`],
    /// [`CoreError::ClientRuntimeUnavailable`], [`CoreError::NoLaunchExe`],
    /// [`CoreError::PathOutsideData`], [`CoreError::NoActiveEngine`],
    /// [`CoreError::AlreadyRunning`], `PrefixBusy`, `PrefixUnusable`.
    pub fn plan_run(&mut self, appid: u32, probe_direct: bool) -> Result<PlannedRun, CoreError> {
        let app = self.reading(|db| Ok(db.app(appid)?))?.ok_or_else(|| {
            not_installed(appid, "it has no record; run `applejack app register`")
        })?;
        let config = self.config_get(appid)?;
        let install_dir = installed_dir(&app)?;
        if !takes_direct_path(&app, &config, probe_direct) {
            return Err(CoreError::ClientRuntimeUnavailable {
                appid,
                drm: app.drm,
            });
        }
        let launch = resolve_launch(appid, &install_dir, &config)?;
        self.check_inside_data(&launch.exe)?;
        let installed = self.run_engine(&config)?;
        let wish = PrefixWish::of(&config)?;
        let name = appid.to_string();
        let lock = self.claim_session_prefix(appid, &name)?;
        let key = new_session_id("run");
        let log_path = self.log_dir().join(&key).join("wine.log");
        let log_text = log_path.to_string_lossy().into_owned();
        let id = self
            .db()?
            .insert_session(&NewSession {
                appid,
                started_at: &utc_now(),
                engine_id: installed.id.as_str(),
                engine_version: installed.version.as_str(),
                log_path: &log_text,
                session_key: &key,
                prefix: &name,
                launch_exe: &launch.exe_name,
            })?
            .ok_or(CoreError::AlreadyRunning { appid })?;
        let session = self.session_row(id)?;
        let spec = self.launch_spec(&config, &key, launch)?;
        Ok(PlannedRun {
            core: self.detached(),
            _lock: lock,
            session,
            installed,
            library: app.library_dir.map(PathBuf::from).unwrap_or_default(),
            wish,
            spec,
            probe_direct,
        })
    }

    /// The pinned engine when the game configuration names one.
    fn run_engine(&mut self, config: &GameConfig) -> Result<InstalledEngine, CoreError> {
        let Some(version) = &config.engine_version else {
            return self.require_active_engine();
        };
        let version = Version::new(version.clone())?;
        Ok(InstalledEngine::open(
            self.layout(),
            self.engine.id(),
            &version,
        )?)
    }

    /// The prefix lock, once nothing else can claim the title: a busy lock
    /// with an open row is this title running, and an open row behind a free
    /// lock is a dead owner's. A prefix a launch would refuse is refused
    /// here, while a request can still say so.
    fn claim_session_prefix(&mut self, appid: u32, name: &str) -> Result<PrefixLock, CoreError> {
        let lock = match self.lock_prefix(name) {
            Ok(lock) => lock,
            Err(error) if busy(&error) => {
                let open = self.reading(|db| Ok(db.open_session(appid)?))?;
                return Err(if open.is_some() {
                    CoreError::AlreadyRunning { appid }
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
        let dir = self.prefix_dir(name)?;
        if dir.is_dir() {
            let (_, prefix) = self.open_prefix(name)?;
            check_dxmt_copies(&dir, prefix.backend)?;
        }
        Ok(lock)
    }

    fn launch_spec(
        &mut self,
        config: &GameConfig,
        key: &str,
        launch: DirectLaunch,
    ) -> Result<LaunchSpec, CoreError> {
        let cpuid_probe = self
            .reading(|db| Ok(db.meta_get(META_CPUID_PROBE)?))?
            .and_then(|json| serde_json::from_str::<CpuidProbe>(&json).ok());
        Ok(LaunchSpec {
            session_id: Some(key.to_owned()),
            exe: launch.exe,
            args: launch.args,
            env_extras: config
                .extra_env
                .iter()
                .map(|(name, value)| (OsString::from(name), OsString::from(value)))
                .collect(),
            cwd: launch.cwd,
            log_dir: self.log_dir().to_path_buf(),
            advertise_avx: config.rosetta_advertise_avx.unwrap_or(false),
            cpuid_probe,
        })
    }

    /// Writes the `apps` row and launch exe of a title already on disk, from
    /// its `appmanifest_<appid>.acf`. This is the stand-in for `app.install`,
    /// which writes the same row and takes the exe from the app info. `drm`
    /// is the caller's word: nothing here can probe it.
    ///
    /// # Errors
    ///
    /// [`CoreError::TitleNotInstalled`] without a readable manifest, or the
    /// database failure.
    pub fn app_register(&mut self, request: &AppRegistration) -> Result<AppRow, CoreError> {
        let appid = request.appid;
        let library = request
            .library
            .clone()
            .unwrap_or_else(|| self.data_dir().join("library"));
        let acf = LibraryPaths::new(&library).acf(appid);
        let text = std::fs::read_to_string(&acf)
            .map_err(|error| not_installed(appid, &format!("{}: {error}", acf.display())))?;
        let manifest = AppManifest::parse(&text)
            .map_err(|error| not_installed(appid, &format!("{}: {error}", acf.display())))?;
        let existing = self.reading(|db| Ok(db.app(appid)?))?.unwrap_or_default();
        let row = AppRow {
            appid,
            name: Some(manifest.name),
            installdir: Some(manifest.installdir),
            drm: request.drm,
            library_dir: Some(library.to_string_lossy().into_owned()),
            last_installed_buildid: Some(manifest.buildid),
            ..existing
        };
        let db = self.db()?;
        db.upsert_app(&row)?;
        if let Some(exe) = &request.exe {
            db.set_field(appid, "launch_exe_override", exe)?;
        }
        Ok(row)
    }
}

fn not_installed(appid: u32, reason: &str) -> CoreError {
    CoreError::TitleNotInstalled {
        appid,
        reason: reason.to_owned(),
    }
}

/// The install directory of a title that is whole on disk. A download
/// record outranks everything: mid-write, the rest of the answers mislead.
fn installed_dir(app: &AppRow) -> Result<PathBuf, CoreError> {
    let appid = app.appid;
    let (Some(library), Some(installdir)) = (&app.library_dir, &app.installdir) else {
        return Err(not_installed(appid, "its record names no library location"));
    };
    let paths = LibraryPaths::new(library);
    if paths.record(appid).exists() {
        return Err(CoreError::DownloadInProgress { appid });
    }
    let acf = paths.acf(appid);
    let manifest = std::fs::read_to_string(&acf)
        .ok()
        .and_then(|text| AppManifest::parse(&text).ok())
        .ok_or_else(|| not_installed(appid, &format!("no readable {}", acf.display())))?;
    if !manifest.is_fully_installed() {
        return Err(not_installed(appid, "its manifest says it is not complete"));
    }
    let dir = paths.install_dir(installdir);
    let empty = std::fs::read_dir(&dir).map_or(true, |mut entries| entries.next().is_none());
    if empty || !inside(&dir, Path::new(library)) {
        return Err(not_installed(
            appid,
            &format!("nothing usable at {}", dir.display()),
        ));
    }
    Ok(dir)
}

fn inside(path: &Path, root: &Path) -> bool {
    match (std::fs::canonicalize(path), std::fs::canonicalize(root)) {
        (Ok(path), Ok(root)) => path.starts_with(root),
        _ => false,
    }
}
