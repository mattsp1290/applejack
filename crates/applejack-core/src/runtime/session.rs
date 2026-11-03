//! The session lifecycle: the game-running marker, end detection, cancel,
//! and what a core does about a session another core left behind.
//!
//! A row in `sessions` moves through [`SessionState`]:
//!
//! ```text
//! starting -> running -> exited | cancelled
//! starting -> exited | failed | cancelled
//! running -> failed, when the watch failed and nothing of the session is left
//! starting | running -> lost
//! ```
//!
//! The process that claims a session owns it. It holds the [`PrefixLock`]
//! from before the row exists until after the row is closed, on the thread
//! that watches the title, so `prefix.reset`, `restore`, `remove`, and a
//! second launch answer busy, also from another process. The lock is a
//! `flock`, so the kernel drops it when the owner dies, and "open row with a
//! free lock" is how a dead owner shows.
//!
//! The CLI (`applejack app run`) and `applejack serve` see each other only
//! through that lock and the row: either can list a session, refuse a second
//! launch, or cancel, and only the owner reaps the loader and reads the exit
//! code. A cancel from the other process signals the title and waits for the
//! lock; the owner closes the row.
//!
//! Marker (plan 05, direct mode): a process of the session whose executable
//! name is the launch exe. The prefix's `wineserver` and Wine's own services
//! carry the session id too, and are what [`SessionProbe::drained`] waits
//! for.
//!
//! The marker ends the title, not the session: the row stays open and the
//! lock stays held until the session drains, so a game a launcher left
//! behind can still be cancelled and its prefix cannot be pulled away. That
//! wait has no bound. A session that will not drain is ended by `app.cancel`,
//! or by `kill_prefix` when a server under another key holds it up.
//! `exit_code` stays what plan 05 says: the status of the launch exe, however
//! long its children ran on.
//!
//! After the spawn nothing writes [`SessionState::Failed`] over a session
//! with a process left: a failure that cannot end the title leaves the row
//! open for the next core, as a dead owner does.
//!
//! A core that starts and finds a live session with no owner takes it over
//! with [`Adopted`] (`serve` only; plan 05 asks for it and it costs no
//! client support). The exit code of such a session is unknown.

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use applejack_engine::rosetta_wine::launch::{parse_reg_dword, reg_query};
use applejack_engine::rosetta_wine::process::{self, Process, find_session, wineserver_running};
use applejack_engine::rosetta_wine::snapshot::PrefixLock;
use applejack_engine::rosetta_wine::utc_now;
use applejack_engine::{EngineId, Error as EngineError, InstalledEngine, Prefix, Session, Version};

use crate::config::{SessionEnd, SessionRow, SessionState};
use crate::error::CoreError;
use crate::prefixes::{SERVER_SETTLE, exit_code};
use crate::runtime::steam_client::{
    META_REDOWNLOAD, Redownload, RedownloadWatch, STEAM_CLIENT_PREFIX,
};
use crate::state::Core;

/// The registry value the Steam client keeps at the running title's appid
/// (plan 05); read live through `reg query`, never from `user.reg`.
pub const RUNNING_APPID_KEY: &str = "HKCU\\Software\\Valve\\Steam";
pub const RUNNING_APPID_VALUE: &str = "RunningAppID";

/// The note on a session the Steam client launched.
pub const NOTE_CLIENT: &str =
    "launched through the Steam client; the title's exit code is not visible";

/// The note on a session whose owner died with the title gone.
pub const NOTE_LOST: &str = "the core that owned this session died; exit code unknown";

/// The note on a session another core took over.
pub const NOTE_ADOPTED: &str = "taken over after the owning core died; exit code unknown";

/// The note on a direct launch that did not last.
pub const NOTE_EARLY: &str = "exited early: the title may require the Steam client";

/// The note on a session that went on after the wait for it passed.
pub const NOTE_LINGERED: &str = "processes of the session outlived the launch exe";

/// How a session is watched. [`Default`] is the shipping set; tests shorten
/// it with [`Core::with_run_tuning`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunTuning {
    /// Between looks at the process table, which costs about 30 ms.
    pub tick: Duration,
    /// Looks in a row without the marker before it counts as cleared: a
    /// process table read can miss a process that is mid-`exec`.
    pub marker_misses: u32,
    /// How long the rest of a session may take to leave before the session
    /// counts as lingering. A cancel stops the prefix's server at this bound.
    pub drain: Duration,
    /// A natural end sooner than this is plan 05's "may require the client".
    pub early_exit: Duration,
    /// Between `SIGTERM` and `SIGKILL` on a cancel.
    pub term_wait: Duration,
    /// How long a cancel waits for the owner to let go of the prefix.
    pub cancel_wait: Duration,
    /// How long the Steam client has to report the title running after
    /// `-applaunch`: a verification pass of a large title takes a while.
    pub client_start_wait: Duration,
}

impl Default for RunTuning {
    fn default() -> Self {
        Self {
            tick: Duration::from_millis(500),
            marker_misses: 2,
            drain: SERVER_SETTLE,
            early_exit: Duration::from_secs(5),
            term_wait: Duration::from_secs(3),
            cancel_wait: Duration::from_secs(60),
            client_start_wait: Duration::from_secs(600),
        }
    }
}

/// Which lifecycle a watch follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchStyle {
    /// The loader is the title: its exit ends a title that never showed,
    /// and the session drains after the title.
    Direct,
    /// The loader is `steam.exe -applaunch`, which hands off and exits when
    /// the client already runs; only the marker says anything, it must
    /// appear within `start_wait`, and the client stays up afterwards, so
    /// nothing drains.
    Client { appid: u32, start_wait: Duration },
}

/// What the watch loop can see of a session.
pub trait SessionProbe {
    /// The loader's exit code once it has exited; `None` while it runs, and
    /// always for a session this process is not the parent of.
    ///
    /// # Errors
    ///
    /// Returns the `waitpid` failure.
    fn loader_exit(&mut self) -> Result<Option<i32>, CoreError>;

    /// The game-running marker.
    fn title_present(&mut self) -> bool;

    /// No process of the session is left and the prefix's server has gone.
    ///
    /// # Errors
    ///
    /// Returns the prefix `stat` failure.
    fn drained(&mut self) -> Result<bool, CoreError>;

    /// Something that must end the watch now, checked at every look: the
    /// client re-downloading the title. The default sees nothing.
    ///
    /// # Errors
    ///
    /// The reason the watch ends, after whatever the probe did about it.
    fn trouble(&mut self) -> Result<(), CoreError> {
        Ok(())
    }
}

/// What a watch tells its owner as it goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchEvent {
    /// The marker is there. Answering `false` asks again at the next look.
    Running,
    /// The title has gone and the session went on past [`RunTuning::drain`].
    /// Once.
    Lingering,
}

/// How a watched session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchEnd {
    /// When the marker cleared, or the loader exited for a title that never
    /// showed one. For a lingering session, when it drained.
    pub ended_at: String,
    /// The launch exe's status. What it left running has none here.
    pub exit_code: Option<i32>,
    /// The marker was seen at all.
    pub seen: bool,
    /// The session outlasted [`RunTuning::drain`].
    pub lingered: bool,
    /// From the start of the watch, which follows the spawn, to the end of
    /// the title; neither the prefix build before it nor the drain after it
    /// counts toward an early exit.
    pub lived: Duration,
}

/// Runs until the title is gone, then until the session drains, however long
/// that takes. `seen` starts true for a title already running when the watch
/// began. A failed look at the prefix counts as "not drained": the title is
/// not the owner's to abandon over a `stat`.
///
/// # Errors
///
/// Returns the failure to read the loader's status.
pub fn watch(
    probe: &mut dyn SessionProbe,
    tuning: &RunTuning,
    seen: bool,
    style: WatchStyle,
    on_event: &mut dyn FnMut(WatchEvent) -> bool,
) -> Result<WatchEnd, CoreError> {
    let watching = Instant::now();
    let mut announced = seen;
    let mut seen = seen;
    let mut misses = 0;
    let mut code = None;
    loop {
        probe.trouble()?;
        if code.is_none() {
            code = probe.loader_exit()?;
        }
        let present = probe.title_present();
        if present {
            misses = 0;
            seen = true;
            if !announced {
                announced = on_event(WatchEvent::Running);
            }
        } else {
            misses += 1;
        }
        let cleared = seen && misses >= tuning.marker_misses;
        match style {
            WatchStyle::Direct if cleared || (code.is_some() && !present) => break,
            WatchStyle::Client { .. } if cleared => break,
            WatchStyle::Client { appid, start_wait }
                if !seen && watching.elapsed() >= start_wait =>
            {
                return Err(CoreError::ClientLaunchTimedOut {
                    appid,
                    seconds: start_wait.as_secs(),
                });
            }
            _ => {}
        }
        std::thread::sleep(tuning.tick);
    }
    let title_ended = utc_now();
    let lived = watching.elapsed();
    if let WatchStyle::Client { .. } = style {
        return Ok(WatchEnd {
            ended_at: title_ended,
            exit_code: None,
            seen,
            lingered: false,
            lived,
        });
    }
    let deadline = Instant::now() + tuning.drain;
    let mut lingered = false;
    loop {
        if code.is_none() {
            code = probe.loader_exit()?;
        }
        if probe.drained().unwrap_or(false) {
            break;
        }
        if !lingered && Instant::now() >= deadline {
            lingered = true;
            on_event(WatchEvent::Lingering);
        }
        std::thread::sleep(tuning.tick);
    }
    if code.is_none() {
        code = probe.loader_exit()?;
    }
    Ok(WatchEnd {
        ended_at: if lingered { utc_now() } else { title_ended },
        exit_code: code,
        seen,
        lingered,
        lived,
    })
}

fn is_title(process: &Process, exe: &str) -> bool {
    process.exe.eq_ignore_ascii_case(exe)
}

/// A session this process spawned.
pub(crate) struct OwnedProbe<'a> {
    pub session: &'a mut Session,
    pub exe: &'a str,
}

impl SessionProbe for OwnedProbe<'_> {
    fn loader_exit(&mut self) -> Result<Option<i32>, CoreError> {
        Ok(self.session.try_wait()?.map(exit_code))
    }

    fn title_present(&mut self) -> bool {
        self.session
            .processes()
            .iter()
            .any(|process| is_title(process, self.exe))
    }

    fn drained(&mut self) -> Result<bool, CoreError> {
        Ok(self.session.processes().is_empty() && !wineserver_running(&self.session.prefix_dir)?)
    }
}

/// A session some other process spawned.
struct ForeignProbe<'a> {
    key: &'a str,
    exe: &'a str,
    prefix_dir: &'a Path,
}

impl SessionProbe for ForeignProbe<'_> {
    fn loader_exit(&mut self) -> Result<Option<i32>, CoreError> {
        Ok(None)
    }

    fn title_present(&mut self) -> bool {
        find_session(self.key)
            .iter()
            .any(|process| is_title(process, self.exe))
    }

    fn drained(&mut self) -> Result<bool, CoreError> {
        Ok(find_session(self.key).is_empty() && !wineserver_running(self.prefix_dir)?)
    }
}

/// The Steam client's own processes and Wine's services: what a cancel of a
/// client session with no known exe must leave alone.
const CLIENT_PROCESSES: &[&str] = &[
    "steam.exe",
    "steamwebhelper.exe",
    "steamwebhelper_real.exe",
    "steamservice.exe",
    "steamerrorreporter.exe",
    "steamerrorreporter64.exe",
    "gameoverlayui.exe",
    "services.exe",
    "winedevice.exe",
    "plugplay.exe",
    "explorer.exe",
    "rpcss.exe",
    "svchost.exe",
    "conhost.exe",
    "start.exe",
    "reg.exe",
    "reg",
    "wineserver",
    "wine",
    "wine64",
    "wine-preloader",
    "wine64-preloader",
];

fn is_client_process(name: &str) -> bool {
    CLIENT_PROCESSES
        .iter()
        .any(|known| known.eq_ignore_ascii_case(name))
}

/// Whether `row` is a session the Steam client launched.
#[must_use]
pub fn is_client_session(row: &SessionRow) -> bool {
    row.prefix == STEAM_CLIENT_PREFIX
}

/// The processes of `row`'s title under any of `keys`: the launch exe by
/// name, or with no exe known, whatever is not the client or Wine itself.
fn title_processes(keys: &[String], exe: &str) -> Vec<Process> {
    let mut found: Vec<Process> = keys
        .iter()
        .flat_map(|key| find_session(key))
        .filter(|process| {
            if exe.is_empty() {
                !is_client_process(&process.exe)
            } else {
                is_title(process, exe)
            }
        })
        .collect();
    found.sort();
    found.dedup();
    found
}

/// A session the Steam client runs. The marker is `RunningAppID`, read
/// through the loader; the launch exe by name under the run's key and the
/// client's is the fallback when the value is absent. `trouble` is the A2
/// watch: a re-download stops the client, puts the `.acf` back, records the
/// finding, and ends the watch.
pub(crate) struct ClientProbe {
    pub core: Core,
    pub appid: u32,
    pub keys: Vec<String>,
    pub exe: String,
    pub installed: InstalledEngine,
    pub prefix: Prefix,
    pub redownload: Option<RedownloadWatch>,
    pub log: String,
}

impl ClientProbe {
    fn running_appid(&self) -> Option<u32> {
        let key = self.keys.first().map_or("", String::as_str);
        reg_query(
            &self.installed,
            &self.prefix,
            key,
            RUNNING_APPID_KEY,
            RUNNING_APPID_VALUE,
        )
        .ok()
        .flatten()
        .and_then(|data| parse_reg_dword(&data))
    }

    fn stop_client(&self) {
        if let Err(error) =
            self.core
                .engine
                .stop_server(&self.installed, &self.prefix.dir, Path::new(&self.log))
        {
            let _ = log_line(&self.log, &format!("the client stop failed: {error}"));
        }
    }
}

impl SessionProbe for ClientProbe {
    fn loader_exit(&mut self) -> Result<Option<i32>, CoreError> {
        Ok(None)
    }

    /// The value names this title, or the launch exe runs under one of the
    /// keys. A value naming another title says nothing about this one: the
    /// user may have started that title from the client's own window.
    fn title_present(&mut self) -> bool {
        match self.running_appid() {
            Some(running) if running == self.appid => true,
            _ => !self.exe.is_empty() && !title_processes(&self.keys, &self.exe).is_empty(),
        }
    }

    fn drained(&mut self) -> Result<bool, CoreError> {
        Ok(true)
    }

    /// Either way the client is stopped and the `.acf` put back; only a
    /// refuted A2 is recorded in `meta`, since a routine patch is not the
    /// finding plan 00 asks about.
    fn trouble(&mut self) -> Result<(), CoreError> {
        let Some(seen) = self
            .redownload
            .as_mut()
            .and_then(RedownloadWatch::redownload)
        else {
            return Ok(());
        };
        let appid = self.appid;
        let (what, error) = match seen {
            Redownload::Refuted(detail) => (
                format!("A2 refuted: {detail}"),
                CoreError::ClientRedownloadRefused { appid, detail },
            ),
            Redownload::Update { from, to } => (
                format!("the client began a routine update from build {from} to {to}"),
                CoreError::TitleUpdatePending { appid, from, to },
            ),
        };
        let _ = log_line(&self.log, &format!("{what}; stopping the client"));
        self.stop_client();
        if let Some(watch) = &self.redownload
            && let Err(error) = watch.restore()
        {
            let _ = log_line(&self.log, &format!("the .acf restore failed: {error}"));
        }
        if let CoreError::ClientRedownloadRefused { detail, .. } = &error
            && let Ok(db) = self.core.db()
        {
            let record = format!("{} app {appid} {detail}", utc_now());
            let _ = db.meta_set(META_REDOWNLOAD, &record);
        }
        Err(error)
    }
}

/// Appends a line to the session log; it may not exist yet, and a session
/// that failed before the spawn should still have one to read.
pub(crate) fn log_line(path: &str, line: &str) -> Result<(), CoreError> {
    let path = Path::new(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| CoreError::io(parent, error))?;
    }
    fs::File::options()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| writeln!(file, "[applejack] {line}"))
        .map_err(|error| CoreError::io(path, error))
}

/// A live title whose owner died, with its prefix lock in hand.
#[derive(Debug)]
pub struct Adopted {
    core: Core,
    _lock: PrefixLock,
    session: SessionRow,
}

impl Adopted {
    /// Watches the title to its end and closes the row, holding the lock.
    ///
    /// # Errors
    ///
    /// Returns a probe or database failure.
    pub fn watch(mut self, report: &mut dyn FnMut(&str, String)) -> Result<SessionRow, CoreError> {
        let row = self.session.clone();
        let dir = self.core.prefix_dir(&row.prefix)?;
        report("running", format!("took over session {}", row.id));
        let note_lingering = format!("session {} outlived {}", row.id, row.launch_exe);
        let tuning = self.core.tuning;
        let mut on_event = |event| {
            if event == WatchEvent::Lingering {
                report("lingering", note_lingering.clone());
            }
            true
        };
        let end = if is_client_session(&row) {
            let mut probe = self.core.client_probe(&row, None)?;
            let style = WatchStyle::Client {
                appid: row.appid,
                start_wait: tuning.client_start_wait,
            };
            watch(&mut probe, &tuning, true, style, &mut on_event)?
        } else {
            let mut probe = ForeignProbe {
                key: &row.session_key,
                exe: &row.launch_exe,
                prefix_dir: &dir,
            };
            watch(&mut probe, &tuning, true, WatchStyle::Direct, &mut on_event)?
        };
        let cancelled = self
            .core
            .reading(|db| Ok(db.session(row.id)?))?
            .is_some_and(|row| row.cancel_requested);
        let state = if cancelled {
            SessionState::Cancelled
        } else {
            SessionState::Exited
        };
        self.core.db()?.end_session(
            row.id,
            &SessionEnd {
                ended_at: &end.ended_at,
                state,
                exit_code: None,
                note: Some(NOTE_ADOPTED),
            },
        )?;
        report(state.as_str(), format!("session {} ended", row.id));
        self.core.session_row(row.id)
    }
}

/// A claimed cancel of one open session.
#[derive(Debug)]
pub struct PlannedCancel {
    core: Core,
    session: SessionRow,
    kill_prefix: bool,
}

pub(crate) fn busy(error: &CoreError) -> bool {
    matches!(error, CoreError::Engine(EngineError::PrefixBusy { .. }))
}

impl PlannedCancel {
    /// The session being cancelled.
    #[must_use]
    pub const fn session(&self) -> &SessionRow {
        &self.session
    }

    /// Ends the title, then the rest of the session, then waits for the
    /// owner to close the row and let go of the prefix, ending what it finds
    /// of the session as it waits. When there is no owner the row is closed
    /// here. On return no process found under the session's id is left and
    /// the lock is free. This is the way out of a session that will not drain: what
    /// the server stop leaves is killed by pid, and a server stop that fails
    /// does not stop that.
    ///
    /// A session with no process has no server of its own: the owner may be
    /// building the prefix, and sees the request before the spawn, or the
    /// server may be `prefix exec`'s. It is left alone unless `kill_prefix`
    /// asks for the whole prefix, which stops it either way.
    ///
    /// # Errors
    ///
    /// Returns a message when the owner holds on past
    /// [`RunTuning::cancel_wait`], or when a process survived, which leaves a
    /// row this call would have closed open for the next cancel.
    pub fn run(mut self, report: &mut dyn FnMut(&str, String)) -> Result<SessionRow, CoreError> {
        let row = self.session.clone();
        let tuning = self.core.tuning;
        let dir = self.core.prefix_dir(&row.prefix)?;
        if is_client_session(&row) && !self.kill_prefix {
            return self.run_client(&row, report);
        }
        if self.kill_prefix {
            report("cancel", "stopping the prefix's server".to_owned());
            self.stop_server_logged(&row);
        } else {
            report("cancel", format!("ending {}", row.launch_exe));
            end_title(
                std::slice::from_ref(&row.session_key),
                &row.launch_exe,
                &tuning,
            )?;
            let mut probe = ForeignProbe {
                key: &row.session_key,
                exe: &row.launch_exe,
                prefix_dir: &dir,
            };
            let deadline = Instant::now() + tuning.drain;
            while !probe.drained().unwrap_or(false) {
                if Instant::now() >= deadline {
                    if self.has_a_session_to_stop(&row)? {
                        self.stop_server_logged(&row);
                    }
                    break;
                }
                std::thread::sleep(tuning.tick);
            }
        }
        end_stragglers(&row, &tuning)?;
        let lock = self.wait_for_owner(&row)?;
        let left = find_session(&row.session_key);
        if !left.is_empty() {
            let mut names = String::new();
            for process in &left {
                let _ = write!(names, " {} ({})", process.exe, process.pid);
            }
            return Err(CoreError::Message(format!(
                "session {} still has processes after the cancel:{names}",
                row.id
            )));
        }
        self.core.db()?.end_session(
            row.id,
            &SessionEnd {
                ended_at: &utc_now(),
                state: SessionState::Cancelled,
                exit_code: None,
                note: Some(NOTE_LOST),
            },
        )?;
        drop(lock);
        report("cancelled", format!("session {} ended", row.id));
        self.core.session_row(row.id)
    }

    /// Ends the title the client runs and leaves the client up (plan 05:
    /// the whole prefix goes only with `kill_prefix`). The owner closes the
    /// row when the marker clears; with no owner it is closed here.
    fn run_client(
        mut self,
        row: &SessionRow,
        report: &mut dyn FnMut(&str, String),
    ) -> Result<SessionRow, CoreError> {
        let tuning = self.core.tuning;
        let keys = self.core.session_keys(row)?;
        let what = if row.launch_exe.is_empty() {
            "the title's processes".to_owned()
        } else {
            row.launch_exe.clone()
        };
        report(
            "cancel",
            format!("ending {what}; the Steam client stays up"),
        );
        end_title(&keys, &row.launch_exe, &tuning)?;
        let deadline = Instant::now() + tuning.cancel_wait;
        let lock = loop {
            match self.core.lock_prefix(&row.prefix) {
                Ok(lock) => break lock,
                Err(error) if busy(&error) && Instant::now() < deadline => {
                    std::thread::sleep(tuning.tick);
                }
                Err(error) if busy(&error) => {
                    return Err(CoreError::Message(format!(
                        "the owner of session {} did not let go of prefix {}; cancel again, or \
                         with kill_prefix to stop the Steam client too",
                        row.id, row.prefix
                    )));
                }
                Err(error) => return Err(error),
            }
        };
        let left = title_processes(&keys, &row.launch_exe);
        if !left.is_empty() {
            let mut names = String::new();
            for process in &left {
                let _ = write!(names, " {} ({})", process.exe, process.pid);
            }
            return Err(CoreError::Message(format!(
                "session {} still has processes after the cancel:{names}",
                row.id
            )));
        }
        if self.core.session_row(row.id)?.ended_at.is_none() {
            self.core.db()?.end_session(
                row.id,
                &SessionEnd {
                    ended_at: &utc_now(),
                    state: SessionState::Cancelled,
                    exit_code: None,
                    note: Some(NOTE_LOST),
                },
            )?;
        }
        drop(lock);
        report("cancelled", format!("session {} ended", row.id));
        self.core.session_row(row.id)
    }

    /// An open row with a process left. Once the row has closed the prefix
    /// may be another session's, and with no process the server is not this
    /// session's to stop.
    fn has_a_session_to_stop(&mut self, row: &SessionRow) -> Result<bool, CoreError> {
        let now = self.core.session_row(row.id)?;
        Ok(now.ended_at.is_none() && !find_session(&row.session_key).is_empty())
    }

    fn wait_for_owner(&self, row: &SessionRow) -> Result<PrefixLock, CoreError> {
        let tuning = self.core.tuning;
        let deadline = Instant::now() + tuning.cancel_wait;
        loop {
            match self.core.lock_prefix(&row.prefix) {
                Ok(lock) => return Ok(lock),
                Err(error) if busy(&error) && Instant::now() < deadline => {
                    // A launcher can still fork while its owner waits on it.
                    end_stragglers(row, &tuning)?;
                    std::thread::sleep(tuning.tick);
                }
                Err(error) if busy(&error) => {
                    return Err(CoreError::Message(format!(
                        "the owner of session {} did not let go of prefix {}; \
                         cancel again, or with kill_prefix to stop the whole prefix",
                        row.id, row.prefix
                    )));
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn stop_server_logged(&self, row: &SessionRow) {
        if let Err(error) = self.stop_server(row) {
            let _ = log_line(&row.log_path, &format!("the server stop failed: {error}"));
        }
    }

    fn stop_server(&self, row: &SessionRow) -> Result<(), CoreError> {
        let id: EngineId = row.engine_id.parse()?;
        let version = Version::new(row.engine_version.clone())?;
        let installed = InstalledEngine::open(self.core.layout(), id, &version)?;
        let dir = self.core.prefix_dir(&row.prefix)?;
        self.core
            .engine
            .stop_server(&installed, &dir, Path::new(&row.log_path))?;
        Ok(())
    }
}

/// `SIGTERM` to the title's processes, then `SIGKILL` to what is left.
fn end_title(keys: &[String], exe: &str, tuning: &RunTuning) -> Result<(), CoreError> {
    let titles = || title_processes(keys, exe);
    for process in titles() {
        process::signal(process.pid, false)?;
    }
    let deadline = Instant::now() + tuning.term_wait;
    loop {
        let left = titles();
        if left.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            for process in left {
                process::signal(process.pid, true)?;
            }
            return Ok(());
        }
        std::thread::sleep(tuning.tick);
    }
}

/// `SIGKILL` to whatever of the session is left once a server stop has had
/// [`RunTuning::term_wait`]: a helper that is no client of the server, or
/// the server itself when it would not stop. Each look kills what it finds,
/// so what was forked after the last one goes too, until the list is empty
/// or another [`RunTuning::term_wait`] has passed. What the final look found
/// is still running, for the caller to report.
fn end_stragglers(row: &SessionRow, tuning: &RunTuning) -> Result<(), CoreError> {
    let started = Instant::now();
    loop {
        let left = find_session(&row.session_key);
        let waited = started.elapsed();
        if left.is_empty() || waited >= tuning.term_wait * 2 {
            return Ok(());
        }
        if waited >= tuning.term_wait {
            for process in left {
                process::signal(process.pid, true)?;
            }
        }
        std::thread::sleep(tuning.tick);
    }
}

impl Core {
    /// The keys a client session's processes carry: the run's own, and the
    /// one the client was started with, since the client starts the game.
    pub(crate) fn session_keys(&mut self, row: &SessionRow) -> Result<Vec<String>, CoreError> {
        let mut keys = vec![row.session_key.clone()];
        if is_client_session(row)
            && let Some(client) = self.client_session_key()?
            && !keys.contains(&client)
        {
            keys.push(client);
        }
        Ok(keys)
    }

    /// A [`ClientProbe`] for `row`, over the engine the row names.
    pub(crate) fn client_probe(
        &mut self,
        row: &SessionRow,
        redownload: Option<RedownloadWatch>,
    ) -> Result<ClientProbe, CoreError> {
        let id: EngineId = row.engine_id.parse()?;
        let version = Version::new(row.engine_version.clone())?;
        let installed = InstalledEngine::open(self.layout(), id, &version)?;
        let (_, prefix) = self.open_prefix(&row.prefix)?;
        let keys = self.session_keys(row)?;
        Ok(ClientProbe {
            core: self.detached(),
            appid: row.appid,
            keys,
            exe: row.launch_exe.clone(),
            installed,
            prefix,
            redownload,
            log: row.log_path.clone(),
        })
    }

    pub(crate) fn session_row(&mut self, id: i64) -> Result<SessionRow, CoreError> {
        self.reading(|db| Ok(db.session(id)?))?
            .ok_or_else(|| CoreError::Message(format!("session {id} has no row")))
    }

    /// Past and open sessions, newest first.
    ///
    /// # Errors
    ///
    /// Returns the database failure.
    pub fn session_list(&mut self, appid: Option<u32>) -> Result<Vec<SessionRow>, CoreError> {
        self.reading(|db| Ok(db.sessions(appid)?))
    }

    /// Settles every open row nobody owns: a free prefix lock means the
    /// owner is gone. A session that has left is closed as
    /// [`SessionState::Lost`]; one still running comes back as [`Adopted`],
    /// lock held, for a caller that will stay to watch it. A caller that
    /// drops it leaves the row open for the next core, and the title is
    /// still refused as already running.
    ///
    /// # Errors
    ///
    /// Returns the database failure, or a lock failure other than busy.
    pub fn reconcile_sessions(&mut self) -> Result<Vec<Adopted>, CoreError> {
        let rows = self.reading(|db| Ok(db.open_sessions()?))?;
        let mut adopted = Vec::new();
        for row in rows {
            let lock = match self.lock_prefix(&row.prefix) {
                Ok(lock) => lock,
                Err(error) if busy(&error) => continue,
                Err(error) => return Err(error),
            };
            if self.settle_orphan(&row)? {
                adopted.push(Adopted {
                    core: self.detached(),
                    _lock: lock,
                    session: row,
                });
            }
        }
        Ok(adopted)
    }

    /// With `row`'s owner known to be gone: true for a session with a process
    /// left, whether or not that is the launch exe; a session that has left
    /// gets its row closed.
    pub(crate) fn settle_orphan(&mut self, row: &SessionRow) -> Result<bool, CoreError> {
        let alive = if is_client_session(row) {
            // The client's own processes carry the key for as long as it
            // runs; only the title's marker says the session is alive.
            self.client_probe(row, None).map_or_else(
                |_| {
                    !title_processes(std::slice::from_ref(&row.session_key), &row.launch_exe)
                        .is_empty()
                },
                |mut probe| probe.title_present(),
            )
        } else {
            !find_session(&row.session_key).is_empty()
        };
        if alive {
            return Ok(true);
        }
        let state = if row.cancel_requested {
            SessionState::Cancelled
        } else {
            SessionState::Lost
        };
        self.db()?.end_session(
            row.id,
            &SessionEnd {
                ended_at: &utc_now(),
                state,
                exit_code: None,
                note: Some(NOTE_LOST),
            },
        )?;
        Ok(false)
    }

    /// Claims a cancel of `appid`'s open session and records the request, so
    /// whoever closes the row writes it as cancelled.
    ///
    /// # Errors
    ///
    /// [`CoreError::NotRunning`] when the title has no open session.
    pub fn plan_cancel(
        &mut self,
        appid: u32,
        kill_prefix: bool,
    ) -> Result<PlannedCancel, CoreError> {
        let session = self
            .reading(|db| Ok(db.open_session(appid)?))?
            .ok_or(CoreError::NotRunning { appid })?;
        self.db()?.request_session_cancel(session.id)?;
        Ok(PlannedCancel {
            core: self.detached(),
            session,
            kill_prefix,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// One scripted look: the marker, and the loader's exit if it has one.
    #[derive(Clone, Copy)]
    struct Look(bool, Option<i32>);

    struct Scripted {
        looks: VecDeque<Look>,
        drained_after: u32,
        now: Look,
    }

    impl Scripted {
        fn new(looks: &[Look], drained_after: u32) -> Self {
            Self {
                looks: looks.iter().copied().collect(),
                drained_after,
                now: Look(false, None),
            }
        }
    }

    impl SessionProbe for Scripted {
        fn loader_exit(&mut self) -> Result<Option<i32>, CoreError> {
            Ok(self.looks.front().copied().unwrap_or(self.now).1)
        }

        fn title_present(&mut self) -> bool {
            self.now = self.looks.pop_front().unwrap_or(self.now);
            self.now.0
        }

        fn drained(&mut self) -> Result<bool, CoreError> {
            if self.drained_after == 0 {
                return Ok(true);
            }
            self.drained_after -= 1;
            Ok(false)
        }
    }

    const FAST: RunTuning = RunTuning {
        tick: Duration::ZERO,
        marker_misses: 2,
        drain: Duration::from_millis(50),
        early_exit: Duration::ZERO,
        term_wait: Duration::ZERO,
        cancel_wait: Duration::ZERO,
        client_start_wait: Duration::from_millis(200),
    };

    fn run(probe: &mut Scripted, seen: bool) -> (WatchEnd, u32) {
        let mut running = 0;
        let end = watch(probe, &FAST, seen, WatchStyle::Direct, &mut |event| {
            if event == WatchEvent::Running {
                running += 1;
            }
            true
        })
        .unwrap();
        (end, running)
    }

    #[test]
    fn a_title_runs_from_the_marker_appearing_to_it_clearing() {
        let mut probe = Scripted::new(
            &[
                Look(false, None),
                Look(true, None),
                Look(true, None),
                Look(false, None),
                Look(false, Some(3)),
            ],
            0,
        );
        let (end, running) = run(&mut probe, false);
        assert_eq!(running, 1);
        assert!(end.seen);
        assert!(!end.lingered);
        assert_eq!(end.exit_code, Some(3));
    }

    #[test]
    fn one_missed_look_is_not_an_end() {
        let mut probe = Scripted::new(
            &[
                Look(true, None),
                Look(false, None),
                Look(true, None),
                Look(false, None),
                Look(false, None),
            ],
            0,
        );
        let (end, running) = run(&mut probe, false);
        assert_eq!(running, 1, "the marker came back: still one session");
        assert!(probe.looks.is_empty(), "the watch read to the second miss");
        assert_eq!(end.exit_code, None);
    }

    #[test]
    fn a_title_that_never_shows_ends_with_its_loader() {
        let mut probe = Scripted::new(&[Look(false, None), Look(false, Some(1))], 0);
        let (end, running) = run(&mut probe, false);
        assert_eq!(running, 0);
        assert!(!end.seen);
        assert_eq!(end.exit_code, Some(1));
    }

    #[test]
    fn a_loader_gone_with_the_title_still_there_is_not_the_end() {
        let mut probe = Scripted::new(
            &[
                Look(true, Some(0)),
                Look(true, Some(0)),
                Look(false, Some(0)),
            ],
            0,
        );
        let (end, _) = run(&mut probe, false);
        assert!(probe.looks.is_empty());
        assert_eq!(end.exit_code, Some(0));
    }

    #[test]
    fn a_watch_that_starts_on_a_running_title_does_not_announce_it() {
        let mut probe = Scripted::new(&[Look(true, None), Look(false, None), Look(false, None)], 0);
        let (end, running) = run(&mut probe, true);
        assert_eq!(running, 0);
        assert!(end.seen);
    }

    #[test]
    fn the_end_waits_for_the_session_past_the_bound_and_says_so_once() {
        let looks = [Look(true, None), Look(false, None), Look(false, Some(0))];
        let mut settles = Scripted::new(&looks, 0);
        assert!(!run(&mut settles, false).0.lingered);

        let mut tuning = FAST;
        tuning.drain = Duration::ZERO;
        let mut probe = Scripted::new(&looks, 50);
        let mut events = Vec::new();
        let end = watch(
            &mut probe,
            &tuning,
            false,
            WatchStyle::Direct,
            &mut |event| {
                events.push(event);
                true
            },
        )
        .unwrap();
        assert_eq!(
            probe.drained_after, 0,
            "the wait ended only when it drained"
        );
        assert_eq!(events, [WatchEvent::Running, WatchEvent::Lingering]);
        assert!(end.lingered);
        assert_eq!(end.exit_code, Some(0), "the launch exe's own status");
    }

    #[test]
    fn a_running_stamp_that_failed_is_asked_for_again_and_the_watch_goes_on() {
        let looks = [
            Look(true, None),
            Look(true, None),
            Look(true, None),
            Look(false, None),
            Look(false, Some(0)),
        ];
        let mut probe = Scripted::new(&looks, 0);
        let mut asked = 0;
        let end = watch(&mut probe, &FAST, false, WatchStyle::Direct, &mut |_| {
            asked += 1;
            asked == 2
        })
        .unwrap();
        assert_eq!(asked, 2);
        assert_eq!(end.exit_code, Some(0));
    }

    struct BrokenPrefix(u32);

    impl SessionProbe for BrokenPrefix {
        fn loader_exit(&mut self) -> Result<Option<i32>, CoreError> {
            Ok(Some(0))
        }

        fn title_present(&mut self) -> bool {
            false
        }

        fn drained(&mut self) -> Result<bool, CoreError> {
            self.0 -= 1;
            if self.0 == 0 {
                Ok(true)
            } else {
                Err(CoreError::Message("no stat".to_owned()))
            }
        }
    }

    #[test]
    fn a_failed_look_at_the_prefix_is_not_drained_and_not_the_end() {
        let mut probe = BrokenPrefix(3);
        watch(&mut probe, &FAST, false, WatchStyle::Direct, &mut |_| true).unwrap();
        assert_eq!(probe.0, 0);
    }

    const CLIENT: WatchStyle = WatchStyle::Client {
        appid: 7,
        start_wait: Duration::from_millis(200),
    };

    #[test]
    fn a_client_session_ignores_the_loader_and_ends_at_the_marker_without_draining() {
        // The loader hands off and exits before the marker shows.
        let mut probe = Scripted::new(
            &[
                Look(false, Some(0)),
                Look(false, Some(0)),
                Look(true, Some(0)),
                Look(true, Some(0)),
                Look(false, Some(0)),
                Look(false, Some(0)),
            ],
            50,
        );
        let mut running = 0;
        let end = watch(&mut probe, &FAST, false, CLIENT, &mut |event| {
            if event == WatchEvent::Running {
                running += 1;
            }
            true
        })
        .unwrap();
        assert_eq!(running, 1);
        assert!(end.seen);
        assert_eq!(end.exit_code, None, "the title's status is the client's");
        assert!(!end.lingered);
        assert_eq!(
            probe.drained_after, 50,
            "nothing waited for the client to drain"
        );
    }

    #[test]
    fn a_client_session_whose_marker_never_shows_times_out() {
        let mut probe = Scripted::new(&[Look(false, Some(0))], 0);
        let mut tuning = FAST;
        tuning.tick = Duration::from_millis(5);
        let error = watch(&mut probe, &tuning, false, CLIENT, &mut |_| true).unwrap_err();
        assert!(
            matches!(error, CoreError::ClientLaunchTimedOut { appid: 7, .. }),
            "{error}"
        );
    }

    struct Troubled(u32);

    impl SessionProbe for Troubled {
        fn loader_exit(&mut self) -> Result<Option<i32>, CoreError> {
            Ok(None)
        }

        fn title_present(&mut self) -> bool {
            true
        }

        fn drained(&mut self) -> Result<bool, CoreError> {
            Ok(true)
        }

        fn trouble(&mut self) -> Result<(), CoreError> {
            self.0 += 1;
            if self.0 == 3 {
                return Err(CoreError::ClientRedownloadRefused {
                    appid: 7,
                    detail: "scripted".to_owned(),
                });
            }
            Ok(())
        }
    }

    #[test]
    fn trouble_ends_a_running_watch_with_its_error() {
        let mut probe = Troubled(0);
        let error = watch(&mut probe, &FAST, false, CLIENT, &mut |_| true).unwrap_err();
        assert!(
            matches!(error, CoreError::ClientRedownloadRefused { .. }),
            "{error}"
        );
        assert_eq!(probe.0, 3, "checked at every look, before the marker");
    }

    #[test]
    fn a_marker_that_clears_with_the_loader_still_alive_ends_the_session() {
        let mut probe = Scripted::new(
            &[
                Look(true, None),
                Look(false, None),
                Look(false, None),
                Look(false, None),
            ],
            1,
        );
        let (end, _) = run(&mut probe, false);
        assert!(end.seen);
        assert_eq!(end.exit_code, None, "nothing reaped: the code is unknown");
    }
}
