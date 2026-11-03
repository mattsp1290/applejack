//! `app.run`: refuse what cannot run, claim the prefix and a session row,
//! then bring the prefix up, launch the title, and watch it to its end.
//!
//! [`Core::plan_run`] is quick and answers the request, so every refusal is
//! an error response; [`PlannedRun::run`] may take as long as the title does
//! and belongs on a job thread or the CLI's main one. The lock travels with
//! the plan.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use applejack_engine::rosetta_wine::launch::{
    check_dxmt_copies, new_session_id, parse_reg_dword, reg_query, reg_set_dword,
};
use applejack_engine::rosetta_wine::process::{find_session, wineserver_running};
use applejack_engine::rosetta_wine::snapshot::{self, PrefixLock};
use applejack_engine::rosetta_wine::{CpuidProbe, utc_now};
use applejack_engine::{
    Engine, GraphicsBackend, HidrawMode, InstalledEngine, LaunchSpec, Prefix, Session, SyncMode,
    Version,
};
use applejack_steam::{AppManifest, InstallPlan, LaunchEntry, LibraryPaths};

use crate::config::presets::is_unsafe_char;
use crate::config::{
    AppRow, Drm, GameConfig, LaunchMode, NewSession, SessionEnd, SessionRow, SessionState,
};
use crate::engines::META_CPUID_PROBE;
use crate::error::CoreError;
use crate::prefixes::pick_sync;
use crate::runtime::direct::{DirectLaunch, resolve_launch, takes_direct_path};
use crate::runtime::session::{
    NOTE_CLIENT, NOTE_EARLY, NOTE_LINGERED, OwnedProbe, RUNNING_APPID_KEY, RUNNING_APPID_VALUE,
    WatchEnd, WatchEvent, WatchStyle, log_line, watch,
};
use crate::runtime::steam_client::RedownloadWatch;
use crate::runtime::steam_client::{
    BrokenReason, ClientPaths, ClientRuntimeState, LibraryEdit, META_CLIENT_SESSION,
    STEAM_CLIENT_PREFIX, ensure_webhelper_shim, missing_web_helper, prepare_client_launch,
    record_client_build, runtime_refused,
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
    /// `None` keeps what the title has, or `unknown` for a new one.
    pub drm: Option<Drm>,
    /// Written to `launch_exe_override`, relative to the install directory.
    pub exe: Option<String>,
    /// The library root, absolute; `<data>/library` when `None`.
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

/// What a client launch carries beside the direct one.
#[derive(Debug)]
struct ClientRun {
    /// The title's own prefix lock, so an install cannot start under it.
    _title: PrefixLock,
    installdir: String,
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
    client: Option<ClientRun>,
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
    /// [`SessionState::Failed`] unless a cancel asked for it, or
    /// [`CoreError::TitleExitedEarly`]. A failure that left a process of the
    /// session running leaves the row open: the next core takes it over, and
    /// `app.cancel` reaches it.
    pub fn run(mut self, report: &mut dyn FnMut(&str, String)) -> Result<SessionRow, CoreError> {
        match self.launch_and_watch(report) {
            Ok(end) => self.close(end.as_ref(), report),
            Err(error) => self.close_failed(error, report),
        }
    }

    fn close_failed(
        &mut self,
        error: CoreError,
        report: &mut dyn FnMut(&str, String),
    ) -> Result<SessionRow, CoreError> {
        let text = error.to_string();
        let log = self.session.log_path.clone();
        if !find_session(&self.session.session_key).is_empty() {
            let _ = log_line(&log, &format!("left running: {text}"));
            return Err(error);
        }
        let _ = log_line(&log, &format!("failed: {text}"));
        let cancelled = self.cancel_requested().unwrap_or(false);
        let state = if cancelled {
            SessionState::Cancelled
        } else {
            SessionState::Failed
        };
        let id = self.session.id;
        let closed = self.core.db().and_then(|db| {
            Ok(db.end_session(
                id,
                &SessionEnd {
                    ended_at: &utc_now(),
                    state,
                    exit_code: None,
                    note: Some(&text),
                },
            )?)
        });
        // The launch failure is the story. A row this cannot close is a dead
        // owner's to the next core.
        if let Err(close) = closed {
            let _ = log_line(&log, &format!("could not close the session: {close}"));
        }
        if cancelled {
            report(state.as_str(), format!("session {id} ended"));
            return self.core.session_row(id);
        }
        Err(error)
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
        let (redownload, starts_client) = match &self.client {
            Some(client) => {
                let paths = ClientPaths::new(prefix.dir.clone());
                let appid = self.session.appid;
                report(
                    "prepare",
                    format!(
                        "backing up appmanifest_{appid}.acf and listing its library for the client"
                    ),
                );
                let (watch, edit) =
                    prepare_client_launch(&paths, &self.library, appid, &client.installdir)?;
                let mut up = self.core.client_is_up()?;
                if up && edit == LibraryEdit::FolderAdded {
                    // The client reads libraryfolders.vdf at startup; a
                    // folder added behind its back is not a library it
                    // knows, and -applaunch would find the title missing.
                    report(
                        "prepare",
                        "restarting the Steam client so it reads the new library folder".to_owned(),
                    );
                    self.core.engine.stop_server(
                        &self.installed,
                        &prefix.dir,
                        Path::new(&self.session.log_path),
                    )?;
                    Core::settle(&prefix.dir)?;
                    up = false;
                }
                self.reset_stale_marker(&prefix)?;
                if !up {
                    // This run starts the client, so its key is the one the
                    // client's processes, and the games it starts, carry.
                    let pass = ensure_webhelper_shim(&self.installed.root, &paths, report)?;
                    if pass.helper_missing {
                        // `plan_run` refused a helper-less prefix before the
                        // answer; one that lost its helper since is refused
                        // here too, since only `install` runs the client's
                        // file check and restarts it with the shim.
                        let dir = missing_web_helper(&paths)?.unwrap_or_else(|| paths.cef_dir());
                        return Err(BrokenReason::NoWebHelper(dir).into_error());
                    }
                    self.core
                        .db()?
                        .meta_set(META_CLIENT_SESSION, &self.session.session_key)?;
                }
                (Some(watch), !up)
            }
            None => (None, false),
        };
        report("starting", self.session.launch_exe.clone());
        let mut session = self
            .core
            .engine
            .launch(&self.installed, &prefix, &self.spec)?;
        let watched = match redownload {
            Some(watch) => self.watch_client(&mut session, watch, report),
            None => self.watch_spawned(&mut session, report),
        };
        if let Err(error) = &watched {
            let log = &self.session.log_path;
            let _ = log_line(log, &format!("ending the title: {error}"));
            if let Err(stop) = session.cancel() {
                let _ = log_line(log, &format!("the title may still run: {stop}"));
            }
        }
        if starts_client {
            // Plan 05: the build id is recorded after every client start,
            // and this run was one; the client may have updated itself.
            let paths = ClientPaths::new(prefix.dir.clone());
            match record_client_build(&mut self.core, &paths) {
                Ok(Some(build)) => {
                    let _ = log_line(&self.session.log_path, &format!("client build {build}"));
                }
                Ok(None) => {}
                Err(error) => {
                    let _ = log_line(
                        &self.session.log_path,
                        &format!("could not record the client build: {error}"),
                    );
                }
            }
        }
        watched.map(Some)
    }

    /// A client stopped while this title ran leaves `RunningAppID` at the
    /// appid, and the watch would take that for this run's title before
    /// `-applaunch` has done anything: the marker has to be seen to appear.
    fn reset_stale_marker(&self, prefix: &Prefix) -> Result<(), CoreError> {
        let key = &self.session.session_key;
        let stale = reg_query(
            &self.installed,
            prefix,
            key,
            RUNNING_APPID_KEY,
            RUNNING_APPID_VALUE,
        )
        .ok()
        .flatten()
        .and_then(|data| parse_reg_dword(&data))
            == Some(self.session.appid);
        if !stale {
            return Ok(());
        }
        let _ = log_line(
            &self.session.log_path,
            "RunningAppID already names this title; resetting it before -applaunch",
        );
        reg_set_dword(
            &self.installed,
            prefix,
            key,
            RUNNING_APPID_KEY,
            RUNNING_APPID_VALUE,
            0,
        )?;
        Ok(())
    }

    fn watch_spawned(
        &mut self,
        session: &mut Session,
        report: &mut dyn FnMut(&str, String),
    ) -> Result<WatchEnd, CoreError> {
        // A cancel that read no process before the spawn left this to us; one
        // written after this read finds the process. A failed read is no
        // reason to stop a game.
        if self.cancel_requested().unwrap_or(false) {
            session.cancel()?;
        }
        self.watch_title(session, report)
    }

    /// The client path: the marker is the client's, a cancel before the
    /// marker shows ends the title by name, and the loader is reaped when it
    /// hands off.
    fn watch_client(
        &mut self,
        session: &mut Session,
        redownload: RedownloadWatch,
        report: &mut dyn FnMut(&str, String),
    ) -> Result<WatchEnd, CoreError> {
        let row = self.session.clone();
        let tuning = self.core.tuning;
        let mut probe = self.core.client_probe(&row, Some(redownload))?;
        let style = WatchStyle::Client {
            appid: row.appid,
            start_wait: tuning.client_start_wait,
        };
        let id = row.id;
        let log = row.log_path.clone();
        let core = &mut self.core;
        let outcome = watch(&mut probe, &tuning, false, style, &mut |event| {
            if event == WatchEvent::Running {
                match core
                    .db()
                    .and_then(|db| Ok(db.mark_session_running(id, &utc_now())?))
                {
                    Ok(()) => {
                        report("running", format!("session {id}"));
                        return true;
                    }
                    Err(error) => {
                        let _ = log_line(&log, &format!("could not record running: {error}"));
                        return false;
                    }
                }
            }
            true
        });
        let _ = session.try_wait();
        outcome
    }

    fn watch_title(
        &mut self,
        session: &mut Session,
        report: &mut dyn FnMut(&str, String),
    ) -> Result<WatchEnd, CoreError> {
        let exe = self.session.launch_exe.clone();
        let log = self.session.log_path.clone();
        let id = self.session.id;
        let tuning = self.core.tuning;
        let core = &mut self.core;
        let mut probe = OwnedProbe { session, exe: &exe };
        let mut warned = false;
        watch(
            &mut probe,
            &tuning,
            false,
            WatchStyle::Direct,
            &mut |event| match event {
                WatchEvent::Running => {
                    let stamped = core
                        .db()
                        .and_then(|db| Ok(db.mark_session_running(id, &utc_now())?));
                    match stamped {
                        Ok(()) => {
                            report("running", format!("session {id}"));
                            true
                        }
                        // A missed stamp is not the end of a game.
                        Err(error) => {
                            if !warned {
                                warned = true;
                                let _ =
                                    log_line(&log, &format!("could not record running: {error}"));
                            }
                            false
                        }
                    }
                }
                WatchEvent::Lingering => {
                    // A launcher can be gone before the first look.
                    if let Ok(db) = core.db() {
                        let _ = db.mark_session_running(id, &utc_now());
                    }
                    let _ = log_line(&log, NOTE_LINGERED);
                    report("lingering", format!("session {id} outlived {exe}"));
                    true
                }
            },
        )
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
        let lingered = end.is_some_and(|end| end.lingered);
        let early_exit = self.core.tuning.early_exit;
        let client = self.client.is_some();
        // A launcher is quick and the game behind it is no sign of DRM.
        let early = !client
            && state == SessionState::Exited
            && !lingered
            && end.is_some_and(|e| e.lived < early_exit);
        let notes: Vec<&str> = [
            early.then_some(NOTE_EARLY),
            lingered.then_some(NOTE_LINGERED),
            self.probe_direct.then_some(NOTE_PROBE),
            client.then_some(NOTE_CLIENT),
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
        if self.client.is_some() && wineserver_running(&dir)? {
            // The client is up and owns the prefix; overrides wait for a
            // stop. Its settings were applied when it was last built.
            let (_, prefix) = self.core.open_prefix(name)?;
            return Ok(prefix);
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
    /// [`CoreError::ClientRuntimeUnavailable`] and
    /// [`CoreError::ClientRuntimeBroken`] on the client path, [`CoreError::NoLaunchExe`],
    /// [`CoreError::NoActiveEngine`], [`CoreError::AlreadyRunning`],
    /// `PrefixBusy`, `PrefixUnusable`.
    pub fn plan_run(&mut self, appid: u32, probe_direct: bool) -> Result<PlannedRun, CoreError> {
        let app = self
            .reading(|db| Ok(db.app(appid)?))?
            .ok_or_else(|| not_installed(appid, NOT_INSTALLED))?;
        let config = self.config_get(appid)?;
        let install_dir = installed_dir(&app)?;
        let client = !takes_direct_path(&app, &config, probe_direct);
        if client {
            self.require_client_ready(&app, &config)?;
        }
        // The exe stays inside the install directory and that inside the
        // title's library, which may be on another volume (plan 02).
        let launch = if client {
            None
        } else {
            Some(resolve_launch(
                appid,
                &install_dir,
                &launch_config(&app, &config),
            )?)
        };
        let installed = self.run_engine(&config)?;
        let wish = PrefixWish::of(&config)?;
        let title = appid.to_string();
        let (name, lock, title_lock) = if client {
            let title_lock = self.claim_idle_title(appid, &title, false)?;
            let lock = self.claim_session_prefix(appid, STEAM_CLIENT_PREFIX)?;
            (STEAM_CLIENT_PREFIX.to_owned(), lock, Some(title_lock))
        } else {
            (
                title.clone(),
                self.claim_session_prefix(appid, &title)?,
                None,
            )
        };
        // An install that took the lock after the look above has gone by
        // now; one that got there first left its record.
        installed_dir(&app)?;
        let key = new_session_id("run");
        let log_path = self.log_dir().join(&key).join("wine.log");
        let log_text = log_path.to_string_lossy().into_owned();
        let exe_name = launch.as_ref().map_or_else(
            || {
                launch_config(&app, &config)
                    .launch_exe_override
                    .map(|exe| exe_file_name(&exe))
                    .unwrap_or_default()
            },
            |launch| launch.exe_name.clone(),
        );
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
                launch_exe: &exe_name,
            })?
            .ok_or(CoreError::AlreadyRunning { appid })?;
        let session = self.session_row(id)?;
        let spec = match launch {
            Some(launch) => self.launch_spec(&config, &key, launch)?,
            None => self.client_launch_spec(&config, &key, appid)?,
        };
        Ok(PlannedRun {
            core: self.detached(),
            _lock: lock,
            session,
            installed,
            library: app.library_dir.map(PathBuf::from).unwrap_or_default(),
            wish,
            spec,
            probe_direct,
            client: title_lock.map(|title| ClientRun {
                _title: title,
                installdir: app.installdir.clone().unwrap_or_default(),
            }),
        })
    }

    /// The client path needs a signed-in client; anything else is a refusal
    /// that names the way there.
    fn require_client_ready(&mut self, app: &AppRow, config: &GameConfig) -> Result<(), CoreError> {
        let appid = app.appid;
        let (state, detail) = self.client_runtime_state()?;
        let fix = match state {
            ClientRuntimeState::Ready => return Ok(()),
            ClientRuntimeState::Absent => "run `applejack runtime steam install`",
            ClientRuntimeState::NeedsLogin => {
                "run `applejack runtime steam install` and sign in inside the Steam window"
            }
            ClientRuntimeState::Installing
            | ClientRuntimeState::Updating
            | ClientRuntimeState::Broken => {
                return Err(runtime_refused(state, detail));
            }
        };
        Err(CoreError::ClientRuntimeUnavailable {
            appid,
            drm: app.drm,
            state,
            fix: fix.to_owned(),
            route: client_free_route(app, config),
        })
    }

    /// `steam.exe -applaunch <appid> -silent -noverifyfiles [args]` in the
    /// client prefix. `-silent` keeps the client's window down, which a
    /// signed-in client does not need; `-noverifyfiles` skips the client's
    /// own check of its files, not the title's. `game_config.steam_overlay`
    /// and `steam_input` are not applied, nor read (docs/steam-runtime.md).
    fn client_launch_spec(
        &mut self,
        config: &GameConfig,
        key: &str,
        appid: u32,
    ) -> Result<LaunchSpec, CoreError> {
        let paths = self.client_paths()?;
        let mut args: Vec<OsString> = vec![
            "-applaunch".into(),
            appid.to_string().into(),
            "-silent".into(),
            "-noverifyfiles".into(),
        ];
        args.extend(
            config
                .launch_args_override
                .as_deref()
                .unwrap_or_default()
                .split_whitespace()
                .map(OsString::from),
        );
        let cpuid_probe = self
            .reading(|db| Ok(db.meta_get(META_CPUID_PROBE)?))?
            .and_then(|json| serde_json::from_str::<CpuidProbe>(&json).ok());
        Ok(LaunchSpec {
            session_id: Some(key.to_owned()),
            exe: paths.steam_exe(),
            args,
            env_extras: config
                .extra_env
                .iter()
                .map(|(name, value)| (OsString::from(name), OsString::from(value)))
                .collect(),
            cwd: paths.steam_dir(),
            log_dir: self.log_dir().to_path_buf(),
            advertise_avx: config.rosetta_advertise_avx.unwrap_or(false),
            cpuid_probe,
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

    /// The prefix lock, once nothing else can claim the title. A prefix a
    /// launch would refuse is refused here, while a request can still say so.
    fn claim_session_prefix(&mut self, appid: u32, name: &str) -> Result<PrefixLock, CoreError> {
        let lock = self.claim_idle_title(appid, name, false)?;
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
    /// its `appmanifest_<appid>.acf`. This is for what `app.install` did not
    /// put there: a library copied in, a title the Windows client installed,
    /// or one a `.acf` adopts; `record_title` is the one write both
    /// go through. `drm` is the caller's word: nothing here can probe it. The
    /// manifest is not trusted: an appid that is not this title's, an unusable `installdir`,
    /// and an `exe` a run would refuse are refused here, with nothing
    /// written. The rest of what a run checks of the disk, such as a complete
    /// manifest and no download record, is left to the run.
    ///
    /// # Errors
    ///
    /// [`CoreError::TitleNotInstalled`] for a library that is relative or
    /// missing, or a manifest that is unreadable, names another app, or has
    /// an unusable `installdir`; [`CoreError::NoLaunchExe`] for an `exe` a
    /// run would refuse; or the database failure.
    pub fn app_register(&mut self, request: &AppRegistration) -> Result<AppRow, CoreError> {
        let appid = request.appid;
        let library = self.register_library(request)?;
        let paths = LibraryPaths::new(&library);
        let acf = paths.acf(appid);
        let text = std::fs::read_to_string(&acf)
            .map_err(|error| not_installed(appid, &format!("{}: {error}", acf.display())))?;
        let manifest = AppManifest::parse(&text)
            .map_err(|error| not_installed(appid, &format!("{}: {error}", acf.display())))?;
        if manifest.appid != appid {
            return Err(not_installed(
                appid,
                &format!("{} names app {}", acf.display(), manifest.appid),
            ));
        }
        if !plain_installdir(&manifest.installdir) {
            return Err(not_installed(appid, UNUSABLE_INSTALLDIR));
        }
        if let Some(exe) = &request.exe {
            let config = GameConfig {
                launch_exe_override: Some(exe.clone()),
                ..GameConfig::default()
            };
            resolve_launch(appid, &paths.install_dir(&manifest.installdir), &config)?;
        }
        let row = self.record_title(appid, &manifest, &library, request.drm, None)?;
        if let Some(exe) = &request.exe {
            self.db()?.set_field(appid, "launch_exe_override", exe)?;
        }
        Ok(row)
    }

    /// Writes what `.acf` says into the `apps` row, keeping what the row
    /// knows and the manifest does not. `drm` of `None` and `launch` of
    /// `None` leave those as they are. The one write of an installed title,
    /// for `app install` and `app register` alike.
    pub(crate) fn record_title(
        &mut self,
        appid: u32,
        manifest: &AppManifest,
        library: &Path,
        drm: Option<Drm>,
        launch: Option<&LaunchEntry>,
    ) -> Result<AppRow, CoreError> {
        let existing = self.reading(|db| Ok(db.app(appid)?))?.unwrap_or_default();
        let row = AppRow {
            appid,
            name: Some(printable(&manifest.name)),
            installdir: Some(manifest.installdir.clone()),
            drm: drm.unwrap_or(existing.drm),
            library_dir: Some(library.to_string_lossy().into_owned()),
            last_installed_buildid: Some(manifest.buildid),
            last_manifest_ids: manifest
                .installed_depots
                .iter()
                .map(|depot| (depot.depot_id.to_string(), depot.manifest_id.to_string()))
                .collect(),
            launch_exe: launch
                .map(|entry| entry.executable.clone())
                .or(existing.launch_exe),
            launch_args: match launch {
                Some(entry) => entry.arguments.clone(),
                None => existing.launch_args,
            },
            ..existing
        };
        self.db()?.upsert_app(&row)?;
        Ok(row)
    }

    /// What a run needs to find the download record of a first install. The
    /// missing build id is what says the title is not whole. An update stays
    /// whole on paper, and keeps its branch for the run that finishes it.
    pub(crate) fn note_install_started(
        &mut self,
        plan: &InstallPlan,
        library: &Path,
        update: bool,
    ) -> Result<(), CoreError> {
        let existing = self
            .reading(|db| Ok(db.app(plan.appid)?))?
            .unwrap_or_default();
        let branch = Some(plan.branch.clone());
        let row = if update {
            AppRow {
                installed_branch: branch,
                ..existing
            }
        } else {
            AppRow {
                appid: plan.appid,
                name: Some(printable(&plan.name)),
                installdir: Some(plan.installdir.clone()),
                library_dir: Some(library.to_string_lossy().into_owned()),
                last_installed_buildid: None,
                last_manifest_ids: BTreeMap::new(),
                installed_branch: branch,
                ..existing
            }
        };
        self.db()?.upsert_app(&row)?;
        Ok(())
    }

    /// The branch on disk, and what the install just learned about the
    /// default one: a title that took its build from there is current, even
    /// when the cached library has not seen that build.
    pub(crate) fn note_install_finished(
        &mut self,
        appid: u32,
        branch: &str,
        default_branch: &str,
    ) -> Result<AppRow, CoreError> {
        let existing = self.reading(|db| Ok(db.app(appid)?))?.unwrap_or_default();
        let latest = if branch == default_branch {
            existing.last_installed_buildid
        } else {
            existing.latest_buildid
        };
        let row = AppRow {
            appid,
            installed_branch: Some(branch.to_owned()),
            default_branch: Some(default_branch.to_owned()),
            latest_buildid: latest,
            ..existing
        };
        self.db()?.upsert_app(&row)?;
        Ok(row)
    }

    /// The canonical library of a registration, which is what a run and the
    /// prefix's `S:` get whatever the working directory then is.
    fn register_library(&self, request: &AppRegistration) -> Result<PathBuf, CoreError> {
        let appid = request.appid;
        let library = match &request.library {
            Some(library) if library.is_absolute() => library.clone(),
            Some(library) => {
                return Err(not_installed(
                    appid,
                    &format!("the library {} is not an absolute path", library.display()),
                ));
            }
            None => self
                .detached()
                .reading(|db| Ok(db.app(appid)?))?
                .and_then(|row| row.library_dir)
                .map_or_else(|| self.data_dir().join("library"), PathBuf::from),
        };
        std::fs::canonicalize(&library)
            .map_err(|error| not_installed(appid, &format!("{}: {error}", library.display())))
    }
}

const NOT_INSTALLED: &str = "it has no record; run `applejack install` first";

const UNUSABLE_INSTALLDIR: &str = "its manifest names an unusable install directory";

/// A Steam `installdir` is one directory name. Anything else in the manifest
/// or the row could widen the install root the exe is confined to.
fn plain_installdir(text: &str) -> bool {
    !text.is_empty()
        && text != "."
        && text != ".."
        && !text.contains(['/', '\\'])
        && !text.chars().any(char::is_control)
}

/// How to get past the client path, which `launch_mode = client` holds a
/// title to whatever `drm` says.
fn client_free_route(app: &AppRow, config: &GameConfig) -> String {
    let appid = app.appid;
    let probe = format!("try it with `applejack run {appid} --probe-direct`");
    let drm = format!("`applejack config {appid} set drm none`");
    if config.launch_mode != Some(LaunchMode::Client) {
        return format!(
            "Nothing marks a title client-free for you: {probe}, and if it runs, record that \
             with {drm}"
        );
    }
    let mode = format!("`applejack config {appid} set launch_mode \"\"`");
    if app.drm == Drm::None {
        return format!(
            "`launch_mode` is `client`: {probe}, and if it runs, clear that with {mode}"
        );
    }
    format!(
        "`launch_mode` is `client`: {probe}, and if it runs, clear that with {mode} and record it with {drm}"
    )
}

/// What a run launches: the override when the configuration has one, else
/// what the app info chose at install.
pub(crate) fn launch_config(app: &AppRow, config: &GameConfig) -> GameConfig {
    let mut merged = config.clone();
    let has_override = config
        .launch_exe_override
        .as_deref()
        .is_some_and(|exe| !exe.trim().is_empty());
    if !has_override {
        merged.launch_exe_override.clone_from(&app.launch_exe);
        if merged.launch_args_override.is_none() {
            merged.launch_args_override.clone_from(&app.launch_args);
        }
    }
    merged
}

/// The file name of a launch exe setting, whichever separator it uses.
fn exe_file_name(exe: &str) -> String {
    exe.rsplit(['/', '\\'])
        .next()
        .unwrap_or(exe)
        .trim()
        .to_owned()
}

/// A manifest's `name` reaches terminals and logs.
pub(crate) fn printable(text: &str) -> String {
    text.chars().filter(|c| !is_unsafe_char(*c)).collect()
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
        return Err(not_installed(appid, NOT_INSTALLED));
    };
    if !plain_installdir(installdir) {
        return Err(not_installed(appid, UNUSABLE_INSTALLDIR));
    }
    if !Path::new(library).is_absolute() {
        return Err(not_installed(appid, "its library is not an absolute path"));
    }
    let paths = LibraryPaths::new(library);
    if paths.record(appid).exists() {
        return Err(CoreError::download_in_progress(
            appid,
            app.last_installed_buildid.is_some(),
        ));
    }
    let acf = paths.acf(appid);
    let manifest = std::fs::read_to_string(&acf)
        .ok()
        .and_then(|text| AppManifest::parse(&text).ok())
        .ok_or_else(|| not_installed(appid, &format!("no readable {}", acf.display())))?;
    if manifest.appid != appid {
        return Err(not_installed(
            appid,
            &format!("{} names app {}", acf.display(), manifest.appid),
        ));
    }
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

/// Strictly inside: an install dir linked to the library would make every
/// title's directory the exe root.
fn inside(path: &Path, root: &Path) -> bool {
    match (std::fs::canonicalize(path), std::fs::canonicalize(root)) {
        (Ok(path), Ok(root)) => path != root && path.starts_with(root),
        _ => false,
    }
}
