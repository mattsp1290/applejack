//! The Windows Steam client in the shared `steam-client` prefix: install
//! from Valve's bootstrapper, its first run, login-state detection, update,
//! snapshot, repair, and stop (plan 05).
//!
//! The runtime is in one [`ClientRuntimeState`], read from disk each time:
//!
//! | State | Read from |
//! |---|---|
//! | `absent` | no `drive_c/Program Files (x86)/Steam/steam.exe` in the prefix |
//! | `installing` | `applejack/client-install.json` in the prefix while another process holds the prefix lock |
//! | `broken` | the prefix record does not open, a `Steam/` directory without `steam.exe`, an install marker nobody holds the lock for, or a `cef.win*/steamwebhelper.exe` that is the web helper shim with no `steamwebhelper_real.exe` beside it |
//! | `updating` | `Steam/package/steam_client_win64.manifest` (or the older `win32` one) is missing, or `Steam/package/tmp/` exists while the client runs |
//! | `needs_login` | `Steam/config/loginusers.vdf` names no remembered account |
//! | `ready` | it names one |
//!
//! An op's `ClientRuntimeBroken` carries the state read when the op
//! returns, so it is the state `status` then shows.
//!
//! `loginusers.vdf` holds the account name and id. They are read for the
//! state alone and go nowhere: not into a row, a log, or an error.
//!
//! `meta` keeps `steam_client_buildid` (from the client manifest after each
//! successful start), `steam_client_session` (the `APPLEJACK_SESSION` the
//! running client was started with, which every process it starts carries),
//! and `steam_client_redownload` (the A2 finding, when it happened).

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use applejack_engine::rosetta_wine::launch::new_session_id;
use applejack_engine::rosetta_wine::process::{find_in_prefix, find_session, wineserver_running};
use applejack_engine::rosetta_wine::snapshot::{self, PrefixLock};
use applejack_engine::rosetta_wine::utc_now;
use applejack_engine::{Engine, GraphicsBackend, InstalledEngine, LaunchSpec, Prefix};
use applejack_steam::acf::app_state;
use applejack_steam::{AppManifest, Document, LibraryPaths, Node, Value};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::CoreError;
use crate::host::HidrawDecision;
use crate::prefixes::SnapshotResult;
use crate::runtime::session::log_line;
use crate::state::Core;
use crate::steam::steam_runtime;

/// The prefix plan 05 keeps for the Windows Steam client.
pub const STEAM_CLIENT_PREFIX: &str = "steam-client";

/// Valve's bootstrapper. Unversioned, so nothing pins its hash: the
/// Authenticode signature is what is checked (decision D4).
pub const BOOTSTRAPPER_URL: &str =
    "https://cdn.fastly.steamstatic.com/client/installer/SteamSetup.exe";

/// `meta` key of the client build id.
pub const META_CLIENT_BUILD: &str = "steam_client_buildid";

/// `meta` key of the session the running client was started with.
pub const META_CLIENT_SESSION: &str = "steam_client_session";

/// `meta` key of the A2 finding: when the client re-downloaded a title.
pub const META_REDOWNLOAD: &str = "steam_client_redownload";

/// Where the bootstrapper installs the client, inside the prefix.
pub const STEAM_DIR: &str = "drive_c/Program Files (x86)/Steam";

/// The client manifests the build id comes from, newest client first: the
/// 2024 bootstrapper installs the 64-bit client and writes the first; an
/// older one wrote the second (plan 05 named it).
pub const CLIENT_MANIFESTS: [&str; 2] = [
    "package/steam_client_win64.manifest",
    "package/steam_client_win32.manifest",
];

/// The host-arch Authenticode tool inside the engine payload.
pub const OSSLSIGNCODE: &str = "probes/osslsigncode";

/// The trust store the signature chain is checked against: macOS's own.
pub const CA_FILE: &str = "/etc/ssl/cert.pem";

/// The web helper shim inside the engine payload. Written over the
/// client's `steamwebhelper.exe`, it runs the real helper with
/// `--in-process-gpu`: CEF then draws the login page in the process that
/// owns the window, the only process DXMT will give a swap chain on it to
/// (DXMT issue 141; unit U15b).
pub const WEBHELPER_SHIM: &str = "probes/steamwebhelper-shim.exe";

/// The helper the client starts to draw its windows, under each
/// `bin/cef/cef.win*/` directory of the client.
pub const WEBHELPER_EXE: &str = "steamwebhelper.exe";

/// Where the client's own helper is stashed, beside the shim.
pub const WEBHELPER_REAL: &str = "steamwebhelper_real.exe";

/// Bytes every build of the shim carries (`MARKER` in its source): a helper
/// that is not this payload's shim by hash but carries them is an older
/// shim, to be replaced, never stashed as the client's helper.
pub const SHIM_MARKER: &[u8] = b"applejack steamwebhelper shim";

/// The helper's log name for the login window; with `Created window` or
/// `WasHidden 0` on the line, the window exists.
pub const LOGIN_WINDOW_MARK: &str = "DesktopLoginWindow_uid0";

/// The client's assertion when something (the macOS microphone prompt)
/// holds its main loop; unanswered, the client dies about 3 minutes in.
pub const STALL_ASSERTION: &str = "BMainLoop appears to have stalled";

/// Every client start but one: the client's own check of its files would
/// write its helper back over the shim. The one start without it is the
/// start after an orphan shim was removed, so that check lays the helper
/// down again.
pub const NOVERIFYFILES: &str = "-noverifyfiles";

/// The sentence a `ClientRuntimeBroken` carries when `runtime_state`
/// reads the same finding from disk, so `status` shows it.
const STATUS_SHOWS_IT: &str = "`applejack runtime steam status` shows it";

/// How long the bootstrapper and the first run may each take, unless
/// `RunTuning::first_run_timeout` says otherwise.
pub const FIRST_RUN_TIMEOUT: Duration = Duration::from_secs(600);

/// The longest file that can be a shim: a build is about 68 KB, the
/// client's helper several MB, so a file over this is the helper and a
/// `status` read need not hash it.
const SHIM_MAX_LEN: u64 = 1 << 20;

/// Growth under a title's install directory that counts as a re-download.
pub const REDOWNLOAD_GROWTH: u64 = 64 << 20;

/// How many times one `install`, `update` or `repair` stops and restarts
/// the client because a start left a `cef.win*` helper in front without
/// the shim (the first self-update, a client update, or a directory the
/// client made at that start, as `cef.win64` beside the two `cef.win7*`
/// live). Past it the op ends `broken` naming the directory instead of
/// looping.
pub const RESHIM_ROUNDS: u32 = 2;

/// The bootstrap log's line for the client ending on its own. A start that
/// logs it with no download line since the start (seen twice live:
/// `Verification skipped`, `Shutdown` 16 s later, nothing else) is started
/// once more, once per `install`, `update` or `repair`, before the exit is an
/// error.
const SHUTDOWN_MARK: &str = "Shutdown";

const INSTALL_MARKER: &str = "applejack/client-install.json";

const BOOTSTRAPPER_FILE: &str = "applejack/SteamSetup.exe";

const STEAM_EXE: &str = "steam.exe";

const LOGINUSERS: &str = "config/loginusers.vdf";

const LIBRARYFOLDERS: &str = "config/libraryfolders.vdf";

const CEF_DIR: &str = "bin/cef";

const WEBHELPER_LOG: &str = "logs/webhelper.txt";

/// Where the stall assertion lands besides the session's own `wine.log`
/// (the client's stderr), in the order it is looked for.
const STALL_LOGS: [&str; 2] = ["logs/console_log.txt", "logs/bootstrap_log.txt"];

const DUMPS_DIR: &str = "dumps";

/// The `StateFlags` bits that mean the client is fetching the title.
const UPDATE_BITS: u32 = app_state::UPDATE_REQUIRED
    | app_state::UPDATE_RUNNING
    | app_state::UPDATE_STARTED
    | app_state::PREALLOCATING
    | app_state::DOWNLOADING
    | app_state::STAGING
    | app_state::COMMITTING
    | app_state::ADDING_FILES;

/// Between looks at the install directory's size.
const SIZE_SCAN_EVERY: Duration = Duration::from_secs(5);

/// Between looks while waiting for the client to come up.
const CLIENT_POLL: Duration = Duration::from_secs(1);

/// Looks in a row without `package/tmp` before the self-update counts as done.
const SETTLED_LOOKS: u32 = 5;

/// How long a `steamwebhelper.exe` process is given to log the login
/// window before the process alone counts as the window. On the one host
/// tried the line comes 17 to 22 s after the start.
const WINDOW_FALLBACK_GRACE: Duration = Duration::from_secs(90);

/// The client's own dump for an assertion, under `Steam/dumps/`; one written
/// since the start is the stall when no log carries its text yet.
const ASSERT_DUMP_PREFIX: &str = "assert_steam.exe_";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientRuntimeState {
    Absent,
    Installing,
    NeedsLogin,
    Ready,
    Updating,
    Broken,
}

impl ClientRuntimeState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Installing => "installing",
            Self::NeedsLogin => "needs_login",
            Self::Ready => "ready",
            Self::Updating => "updating",
            Self::Broken => "broken",
        }
    }
}

impl ClientRuntimeState {
    /// The tail of the `ClientRuntimeBroken` message: what gets the runtime
    /// out of this state. A `broken` detail carries its own remedy (see
    /// `BrokenReason`), since no one command fits every sub-state: `repair`
    /// restores a snapshot only when a usable one exists, and refuses any
    /// state but `broken`, where a restore would roll back a client worth
    /// keeping.
    #[must_use]
    pub const fn advice(self) -> &'static str {
        match self {
            Self::Installing => "; wait for the install to finish",
            Self::Updating => {
                "; wait for the client's self-update to finish, or run `applejack runtime steam \
                 install` if the client was stopped"
            }
            Self::Absent | Self::NeedsLogin | Self::Ready | Self::Broken => "",
        }
    }
}

/// What [`Core::runtime_state`] reads: a `broken` runtime's reason, or
/// another state with its detail.
pub(crate) enum RuntimeReading {
    Broken(BrokenReason),
    Other(ClientRuntimeState, String),
}

/// Why the runtime is `broken`, with the way out of that sub-state. Every
/// `broken` detail the code produces is one of these, so the composed
/// message never promises a restore that `repair` cannot make.
#[derive(Debug)]
pub(crate) enum BrokenReason {
    /// The marker of an install that died, without a lock holder.
    DeadInstall,
    /// `applejack/prefix.toml` does not read or parse; the engine's error.
    RecordDoesNotOpen(String),
    /// A `Steam/` directory without `steam.exe`.
    NoSteamExe(PathBuf),
    /// `SteamSetup.exe` ran past the first-run timeout (the seconds); its log.
    BootstrapperTimedOut { secs: u64, log: PathBuf },
    /// `SteamSetup.exe` failed; its exit status and log.
    BootstrapperExited(String, PathBuf),
    /// `SteamSetup.exe` succeeded and left no `steam.exe` at the path.
    BootstrapperLeftNoExe(PathBuf),
    /// The client exited before its window came up; its bootstrap log.
    ClientExitedEarly(PathBuf),
    /// The client asserted a stalled main loop and died: a macOS prompt
    /// was left unanswered. The log the assertion is in, or the assert
    /// dump the client wrote when no log carries the text.
    ClientStalledOnPrompt(PathBuf),
    /// The engine payload has no web helper shim at the path.
    NoShim(PathBuf),
    /// A `cef.win*` directory holds the shim as `steamwebhelper.exe` and no
    /// real helper beside it (or a stash that is itself a shim).
    ShimWithoutReal(PathBuf),
    /// A `cef.win*` directory holds neither `steamwebhelper.exe` nor
    /// `steamwebhelper_real.exe` (or an installed client has no such
    /// directory at all): nothing there can draw the window.
    NoWebHelper(PathBuf),
    /// A populated `cef.win*` directory holds the client's own helper as
    /// `steamwebhelper.exe`, so it runs without the shim and draws no
    /// window. `stashed` says which shape: the client wrote its helper back
    /// over the shim with `steamwebhelper_real.exe` still beside it, or it
    /// made the directory after the shim pass and nothing is stashed.
    HelperUnshimmed { dir: PathBuf, stashed: bool },
    /// The start without `-noverifyfiles`, with the client's manifest in
    /// place, ended with the client exiting and the directory still without
    /// a helper. The directory held no helper going in, whether an orphan
    /// shim was removed or nothing was there.
    HelperNotLaidDown(PathBuf),
    /// A web helper file under `bin/cef` does not read; the io error.
    HelperUnreadable(String),
    /// `repair` found no snapshot at all.
    NoSnapshot,
    /// `repair` found snapshots and skipped every one (each `repair:
    /// skipping snapshot` line says why).
    NoUsableSnapshot(usize),
    /// `repair` restored the snapshot and the runtime still reads `broken`.
    StillBroken { timestamp: String, detail: String },
}

impl BrokenReason {
    /// The `broken` detail: the break, then its way out.
    pub(crate) fn detail(&self) -> String {
        let (cause, way_out) = self.parts();
        format!("{cause}; {way_out}")
    }

    /// The break alone, for a line whose own way out follows (`repair`'s
    /// restore), where the detail's advice would repeat it or name another
    /// command.
    pub(crate) fn cause(&self) -> String {
        self.parts().0
    }

    fn parts(&self) -> (String, String) {
        const REPAIR_RESTORES: &str =
            "`applejack runtime steam repair` restores the newest usable snapshot";
        const REINSTALL_RERUNS: &str =
            "`applejack runtime steam repair --reinstall` reruns the bootstrapper";
        let (cause, way_out) = match self {
            Self::DeadInstall => (
                "an install stopped before it finished".to_owned(),
                "run `applejack runtime steam install` again",
            ),
            Self::RecordDoesNotOpen(error) => (error.clone(), REPAIR_RESTORES),
            Self::NoSteamExe(dir) => (
                format!("{} has no steam.exe", dir.display()),
                REPAIR_RESTORES,
            ),
            Self::BootstrapperTimedOut { secs, log } => (
                format!(
                    "SteamSetup.exe ran past {secs} seconds; log {}",
                    log.display()
                ),
                REINSTALL_RERUNS,
            ),
            Self::BootstrapperExited(status, log) => (
                format!("SteamSetup.exe exited {status}; log {}", log.display()),
                REINSTALL_RERUNS,
            ),
            Self::BootstrapperLeftNoExe(exe) => (
                format!("the bootstrapper left no {}", exe.display()),
                REINSTALL_RERUNS,
            ),
            Self::ClientExitedEarly(log) => (
                format!(
                    "the client exited before its window came up; see {} and the session log",
                    log.display()
                ),
                "`applejack runtime steam install` starts it again",
            ),
            Self::ClientStalledOnPrompt(evidence) => client_stalled_on_prompt(evidence),
            Self::NoShim(path) => (
                format!(
                    "the engine payload ships no web helper shim at {}, and without it the \
                     client's login window does not draw on this engine",
                    path.display()
                ),
                "install a payload that ships it",
            ),
            Self::ShimWithoutReal(dir) => (
                format!(
                    "{}/{WEBHELPER_EXE} is the shim and {WEBHELPER_REAL} is missing beside it",
                    dir.display()
                ),
                "`applejack runtime steam install` removes the orphan shim and starts the \
                 client once with its file check on, which lays the client's own helper down \
                 again, then stashes it and puts the shim back",
            ),
            Self::NoWebHelper(dir) => (
                format!(
                    "{} has neither {WEBHELPER_EXE} nor {WEBHELPER_REAL}, so the client has no \
                     web helper to draw its window",
                    dir.display()
                ),
                "`applejack runtime steam install` starts the client with its file check on, \
                 which may lay the helper down, and `applejack runtime steam repair \
                 --reinstall` removes Steam/package/ so the client's self-update fetches its \
                 web helper again",
            ),
            Self::HelperUnshimmed { dir, stashed } => (
                helper_unshimmed(dir, *stashed),
                "`applejack runtime steam stop` if the client is running, then `applejack \
                 runtime steam install`, or `applejack runtime steam repair` alone, put the \
                 shim in front of it without downloading anything",
            ),
            Self::HelperNotLaidDown(dir) => helper_not_laid_down(dir),
            Self::HelperUnreadable(error) => (
                format!("a web helper file does not read: {error}"),
                REPAIR_RESTORES,
            ),
            Self::NoSnapshot => (
                format!("no snapshot of {STEAM_CLIENT_PREFIX} exists"),
                REINSTALL_RERUNS,
            ),
            Self::NoUsableSnapshot(listed) => (
                format!(
                    "no usable snapshot of {STEAM_CLIENT_PREFIX} exists ({listed} listed, none \
                     usable; see the `repair: skipping snapshot` lines)"
                ),
                REINSTALL_RERUNS,
            ),
            Self::StillBroken { timestamp, detail } => {
                return (
                    format!("restoring {timestamp} left the runtime broken ({detail})"),
                    format!(
                        "`applejack prefix restore {STEAM_CLIENT_PREFIX} <timestamp>` picks \
                         another snapshot, and {REINSTALL_RERUNS}"
                    ),
                );
            }
        };
        (cause, way_out.to_owned())
    }
}

/// [`BrokenReason::ClientStalledOnPrompt`]'s cause and way out: an
/// assertion dump alone names the prompt as the usual cause, a stall line
/// names it outright.
fn client_stalled_on_prompt(evidence: &Path) -> (String, &'static str) {
    if evidence.extension().is_some_and(|ext| ext == "dmp") {
        return (
            format!(
                "the client died after writing an assertion dump since the start ({})",
                evidence.display()
            ),
            "the usual cause is a macOS permission prompt for \"applejack\" (the microphone) \
             left unanswered: answer it (Don't Allow is fine) and run `applejack runtime steam \
             install` again",
        );
    }
    (
        format!(
            "the client's main loop stalled and the client died (`{STALL_ASSERTION}`, see {}): \
             a macOS permission prompt for \"applejack\" (the microphone) was left unanswered",
            evidence.display()
        ),
        "answer it (Don't Allow is fine) and run `applejack runtime steam install` again",
    )
}

/// [`BrokenReason::HelperUnshimmed`]'s cause: the shape by the stash, the
/// fix (the shim pass, by `install` or `repair`) the same for both.
fn helper_unshimmed(dir: &Path, stashed: bool) -> String {
    let (beside, shape) = if stashed {
        (
            format!("with {WEBHELPER_REAL} stashed beside it"),
            "the client wrote its web helper back over the shim; the stash is beside it",
        )
    } else {
        (
            format!("with no {WEBHELPER_REAL} beside it"),
            "the client made the directory after the shim pass; no stash beside it",
        )
    };
    format!(
        "{}/{WEBHELPER_EXE} is the client's own web helper {beside}, so it runs without the shim \
         and its window does not draw on this engine ({shape})",
        dir.display()
    )
}

/// [`BrokenReason::HelperNotLaidDown`]'s cause and way out: the client
/// exited, so its file check may not have run to its end, and `install`
/// again comes first.
fn helper_not_laid_down(dir: &Path) -> (String, &'static str) {
    (
        format!(
            "{} still has no {WEBHELPER_EXE}: the directory held no web helper (an orphan \
             shim was removed, or none was there) and the client's file check did not lay one \
             down before the client exited; see the session log",
            dir.display()
        ),
        "run `applejack runtime steam install` again; if the directory is still empty, \
         `applejack runtime steam repair --reinstall` removes Steam/package/ so the client's \
         self-update fetches its web helper again",
    )
}

impl BrokenReason {
    /// Whether `runtime_state` reads this same finding from disk, so that
    /// `applejack runtime steam status` shows it. A bootstrapper or client
    /// that died, or a payload fault, is an event `status` cannot see, and
    /// `status` counts snapshots without opening one.
    pub(crate) const fn shown_by_status(&self) -> bool {
        match self {
            Self::DeadInstall
            | Self::RecordDoesNotOpen(_)
            | Self::NoSteamExe(_)
            | Self::ShimWithoutReal(_)
            | Self::NoWebHelper(_)
            | Self::HelperUnshimmed { .. }
            | Self::HelperNotLaidDown(_)
            | Self::HelperUnreadable(_)
            | Self::NoSnapshot
            | Self::StillBroken { .. } => true,
            Self::BootstrapperTimedOut { .. }
            | Self::BootstrapperExited(..)
            | Self::BootstrapperLeftNoExe(_)
            | Self::ClientExitedEarly(_)
            | Self::ClientStalledOnPrompt(_)
            | Self::NoShim(_)
            | Self::NoUsableSnapshot(_) => false,
        }
    }

    /// The `ClientRuntimeBroken` for this reason, with the `status` sentence
    /// only where it is true.
    pub(crate) fn into_error(self) -> CoreError {
        CoreError::ClientRuntimeBroken {
            state: ClientRuntimeState::Broken,
            detail: shown_detail(self.detail(), self.shown_by_status()),
        }
    }
}

/// `detail`, with [`STATUS_SHOWS_IT`] appended when `shown`.
fn shown_detail(detail: String, shown: bool) -> String {
    if shown {
        format!("{detail}. {STATUS_SHOWS_IT}")
    } else {
        detail
    }
}

impl std::fmt::Display for ClientRuntimeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `runtime.steam.status` and `applejack runtime steam status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientRuntimeStatus {
    pub state: ClientRuntimeState,
    pub prefix: PathBuf,
    pub build_id: Option<String>,
    /// A `wineserver` runs for the prefix: the client is up.
    pub running: bool,
    pub snapshots: usize,
    pub detail: String,
}

/// The client's files inside the prefix.
#[derive(Debug, Clone)]
pub struct ClientPaths {
    pub prefix_dir: PathBuf,
}

impl ClientPaths {
    #[must_use]
    pub fn new(prefix_dir: impl Into<PathBuf>) -> Self {
        Self {
            prefix_dir: prefix_dir.into(),
        }
    }

    #[must_use]
    pub fn steam_dir(&self) -> PathBuf {
        self.prefix_dir.join(STEAM_DIR)
    }

    #[must_use]
    pub fn steam_exe(&self) -> PathBuf {
        self.steam_dir().join(STEAM_EXE)
    }

    /// The client manifest on disk, or the one a 64-bit client would
    /// write when neither is there yet.
    #[must_use]
    pub fn manifest(&self) -> PathBuf {
        let steam = self.steam_dir();
        CLIENT_MANIFESTS
            .iter()
            .map(|name| steam.join(name))
            .find(|path| path.is_file())
            .unwrap_or_else(|| steam.join(CLIENT_MANIFESTS[0]))
    }

    #[must_use]
    pub fn package_tmp(&self) -> PathBuf {
        self.steam_dir().join("package/tmp")
    }

    #[must_use]
    pub fn loginusers(&self) -> PathBuf {
        self.steam_dir().join(LOGINUSERS)
    }

    #[must_use]
    pub fn libraryfolders(&self) -> PathBuf {
        self.steam_dir().join(LIBRARYFOLDERS)
    }

    #[must_use]
    pub fn install_marker(&self) -> PathBuf {
        self.prefix_dir.join(INSTALL_MARKER)
    }

    #[must_use]
    pub fn bootstrapper(&self) -> PathBuf {
        self.prefix_dir.join(BOOTSTRAPPER_FILE)
    }

    /// `bin/cef/`, which holds one `cef.win*` directory per CEF build.
    #[must_use]
    pub fn cef_dir(&self) -> PathBuf {
        self.steam_dir().join(CEF_DIR)
    }

    /// The helper's log, where the login window announces itself.
    #[must_use]
    pub fn webhelper_log(&self) -> PathBuf {
        self.steam_dir().join(WEBHELPER_LOG)
    }
}

/// What `ensure_webhelper_shim` did to one `cef.win*` directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShimEdit {
    /// The shim was the helper already, with the real one beside it.
    InPlace,
    /// The client's helper was stashed and the shim written for the first time.
    Applied,
    /// A client update had written its helper over the shim; the new helper
    /// is stashed and the shim is back.
    Reapplied,
    /// An older build of the shim was the helper; this payload's is now.
    Upgraded,
    /// The stash was there and nothing in front of it; the shim is back.
    Rewritten,
    /// The helper was a shim with no real helper beside it, so nothing it
    /// could run: it is gone, and the next start lays the helper down: the
    /// client's self-update when its manifest is gone too (`repair
    /// --reinstall`), else its file check.
    OrphanRemoved { self_update: bool },
}

impl ShimEdit {
    fn describe(self) -> &'static str {
        match self {
            Self::InPlace => "the shim is in front of the web helper",
            Self::Applied => {
                "stashed the web helper as steamwebhelper_real.exe and put the shim in front of it"
            }
            Self::Reapplied => {
                "a client update replaced the web helper; stashed the new one and put the shim back"
            }
            Self::Upgraded => "replaced an older build of the shim with this payload's",
            Self::Rewritten => "put the shim back in front of the stashed web helper",
            Self::OrphanRemoved { self_update: true } => {
                "removed an orphan shim; the client's self-update lays its helper down"
            }
            Self::OrphanRemoved { self_update: false } => {
                "removed an orphan shim; the client's file check lays its helper down at this \
                 start"
            }
        }
    }
}

/// The `cef.win*` directories under the client's `bin/cef/`, sorted; none
/// when the directory is not there.
fn cef_dirs(paths: &ClientPaths) -> Result<Vec<PathBuf>, CoreError> {
    let cef = paths.cef_dir();
    if !cef.is_dir() {
        return Ok(Vec::new());
    }
    let mut dirs = Vec::new();
    for entry in fs::read_dir(&cef).map_err(|error| CoreError::io(&cef, error))? {
        let entry = entry.map_err(|error| CoreError::io(&cef, error))?;
        let path = entry.path();
        if path.is_dir()
            && entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("cef.win"))
        {
            dirs.push(path);
        }
    }
    dirs.sort();
    Ok(dirs)
}

/// The file's SHA-256 and whether it carries [`SHIM_MARKER`].
fn hash_and_marker(path: &Path) -> Result<([u8; 32], bool), CoreError> {
    let bytes = fs::read(path).map_err(|error| CoreError::io(path, error))?;
    let marked = bytes
        .windows(SHIM_MARKER.len())
        .any(|window| window == SHIM_MARKER);
    Ok((Sha256::digest(&bytes).into(), marked))
}

/// The payload's shim, or `NoShim` naming the path.
fn shim_path(engine_root: &Path) -> Result<PathBuf, CoreError> {
    let path = engine_root.join(WEBHELPER_SHIM);
    if path.is_file() {
        Ok(path)
    } else {
        Err(BrokenReason::NoShim(path).into_error())
    }
}

fn read_shim(engine_root: &Path) -> Result<(Vec<u8>, [u8; 32]), CoreError> {
    let path = shim_path(engine_root)?;
    let bytes = fs::read(&path).map_err(|error| CoreError::io(&path, error))?;
    let hash = Sha256::digest(&bytes).into();
    Ok((bytes, hash))
}

/// Whether the file is a shim: this payload's by hash, or any build's by
/// its marker; a file over [`SHIM_MAX_LEN`] is the helper, opened but not
/// read, so one that does not open fails here as a small one would. `None`
/// when nothing is at the path; an error when something other than a file
/// is, since the pass would write the shim through a rename over it.
fn shim_file(path: &Path, shim_hash: Option<&[u8; 32]>) -> Result<Option<bool>, CoreError> {
    let Ok(meta) = fs::metadata(path) else {
        return Ok(None);
    };
    if !meta.is_file() {
        return Err(CoreError::io(
            path,
            std::io::Error::other("is not a regular file"),
        ));
    }
    if meta.len() > SHIM_MAX_LEN {
        fs::File::open(path).map_err(|error| CoreError::io(path, error))?;
        return Ok(Some(false));
    }
    let (hash, marked) = hash_and_marker(path)?;
    Ok(Some(marked || shim_hash.is_some_and(|shim| *shim == hash)))
}

/// The first `cef.win*` directory whose `steamwebhelper.exe` is a shim by
/// its marker with no client helper beside it: the shim has nothing to run,
/// and the runtime is `broken` until the helper is laid down again. Needs
/// no payload, so `status`, `repair`, and `install` read the same thing.
fn orphan_shim(paths: &ClientPaths) -> Result<Option<PathBuf>, CoreError> {
    for dir in cef_dirs(paths)? {
        if shim_file(&dir.join(WEBHELPER_EXE), None)? == Some(true)
            && shim_file(&dir.join(WEBHELPER_REAL), None)? != Some(false)
        {
            return Ok(Some(dir));
        }
    }
    Ok(None)
}

/// [`cef_dirs`] split into the directories that hold anything and those
/// with no entries at all. A directory with no entries beside one that
/// holds something is a leftover the client's file check never fills, not
/// a client missing its helper, so the helper reads skip it; with every
/// directory empty they are all kept, since nothing then draws the window.
fn populated_cef_dirs(paths: &ClientPaths) -> Result<(Vec<PathBuf>, Vec<PathBuf>), CoreError> {
    let dirs = cef_dirs(paths)?;
    let mut kept = Vec::new();
    let mut skipped = Vec::new();
    for dir in dirs {
        let mut entries = fs::read_dir(&dir).map_err(|error| CoreError::io(&dir, error))?;
        if entries.next().is_some() {
            kept.push(dir);
        } else {
            skipped.push(dir);
        }
    }
    if kept.is_empty() {
        return Ok((skipped, Vec::new()));
    }
    Ok((kept, skipped))
}

/// The first `cef.win*` directory with neither `steamwebhelper.exe` nor
/// `steamwebhelper_real.exe`, or `bin/cef` itself when no such directory
/// exists: nothing there can draw the window. A directory with no entries
/// beside a populated one is skipped ([`populated_cef_dirs`]). Read after
/// the `updating` reads, since a self-update still filling the tree looks
/// the same.
pub(crate) fn missing_web_helper(paths: &ClientPaths) -> Result<Option<PathBuf>, CoreError> {
    let (dirs, _) = populated_cef_dirs(paths)?;
    if dirs.is_empty() {
        return Ok(Some(paths.cef_dir()));
    }
    Ok(dirs
        .into_iter()
        .find(|dir| !dir.join(WEBHELPER_EXE).is_file() && !dir.join(WEBHELPER_REAL).is_file()))
}

/// Writes `shim` over `helper` through a sibling and a rename, with the
/// permissions of `mode_of`.
fn write_shim(helper: &Path, shim: &[u8], mode_of: &Path) -> Result<(), CoreError> {
    let staged = helper.with_extension("exe.applejack");
    fs::write(&staged, shim).map_err(|error| CoreError::io(&staged, error))?;
    let mode = fs::metadata(mode_of)
        .map_err(|error| CoreError::io(mode_of, error))?
        .permissions();
    fs::set_permissions(&staged, mode).map_err(|error| CoreError::io(&staged, error))?;
    fs::rename(&staged, helper).map_err(|error| CoreError::io(helper, error))
}

/// What one pass of [`ensure_webhelper_shim`] left behind.
#[derive(Debug, Default)]
pub(crate) struct ShimPass {
    pub edits: Vec<(PathBuf, ShimEdit)>,
    /// A `cef.win*` directory has no `steamwebhelper.exe` (an orphan shim
    /// was removed, or the client's tree is missing it): the next start must
    /// run without `-noverifyfiles`, so the client's file check lays the
    /// helper down.
    pub helper_missing: bool,
}

/// Puts the payload's shim in front of the client's web helper under every
/// `bin/cef/cef.win*/` directory. A helper that is not a shim (by SHA-256
/// against the payload's and by the marker, never by size) is copied to
/// `steamwebhelper_real.exe`, over any older stash, and the shim written in
/// its place, so after a client self-update, which writes the new helper
/// over the shim, the next start stashes the new helper: the real one is
/// never older than the client. A shim with no client helper beside it (or
/// a stash that is itself a shim, which it would run in a loop) is removed,
/// and the pass says so in `helper_missing`; a stash with nothing in front
/// of it gets the shim back. Every start of the client goes through here.
/// One edit per directory, in order, each reported under `shim`.
///
/// # Errors
///
/// [`CoreError::ClientRuntimeBroken`] when the payload has no shim; the
/// read, copy, or write failure.
pub(crate) fn ensure_webhelper_shim(
    engine_root: &Path,
    paths: &ClientPaths,
    report: &mut dyn FnMut(&str, String),
) -> Result<ShimPass, CoreError> {
    let (shim, shim_hash) = read_shim(engine_root)?;
    let mut pass = ShimPass::default();
    let (dirs, skipped) = populated_cef_dirs(paths)?;
    for dir in skipped {
        report(
            "shim",
            format!(
                "{}: an empty directory beside one that holds the web helper; skipped",
                dir.display()
            ),
        );
    }
    // With no manifest the client's first self-update lays the whole tree
    // down; with one, only its file check does.
    let self_update = !paths.manifest().is_file();
    let lays_it = if self_update {
        "self-update"
    } else {
        "file check"
    };
    if dirs.is_empty() {
        if self_update {
            report(
                "shim",
                "no bin/cef/cef.win* directory yet; the client's self-update brings it".to_owned(),
            );
        } else {
            pass.helper_missing = true;
            report(
                "shim",
                "no bin/cef/cef.win* directory; the client's file check lays it down".to_owned(),
            );
        }
    }
    for dir in dirs {
        let helper = dir.join(WEBHELPER_EXE);
        let real = dir.join(WEBHELPER_REAL);
        let has_real = match shim_file(&real, Some(&shim_hash))? {
            Some(false) => true,
            Some(true) => {
                fs::remove_file(&real).map_err(|error| CoreError::io(&real, error))?;
                report(
                    "shim",
                    format!(
                        "{}: removed a stashed {WEBHELPER_REAL} that was itself a shim",
                        dir.display()
                    ),
                );
                false
            }
            None => false,
        };
        let edit = if !helper.is_file() && !has_real {
            pass.helper_missing = true;
            report(
                "shim",
                format!(
                    "{}: no {WEBHELPER_EXE}; the client's {lays_it} lays it down",
                    dir.display()
                ),
            );
            continue;
        } else if !helper.is_file() {
            write_shim(&helper, &shim, &real)?;
            ShimEdit::Rewritten
        } else {
            let (helper_hash, marked) = hash_and_marker(&helper)?;
            let is_this_shim = helper_hash == shim_hash;
            if (is_this_shim || marked) && !has_real {
                fs::remove_file(&helper).map_err(|error| CoreError::io(&helper, error))?;
                pass.helper_missing = true;
                ShimEdit::OrphanRemoved { self_update }
            } else if is_this_shim {
                ShimEdit::InPlace
            } else if marked {
                write_shim(&helper, &shim, &helper)?;
                ShimEdit::Upgraded
            } else {
                fs::copy(&helper, &real).map_err(|error| CoreError::io(&real, error))?;
                write_shim(&helper, &shim, &helper)?;
                if has_real {
                    ShimEdit::Reapplied
                } else {
                    ShimEdit::Applied
                }
            }
        };
        report("shim", format!("{}: {}", dir.display(), edit.describe()));
        pass.edits.push((dir, edit));
    }
    Ok(pass)
}

/// A populated `cef.win*` directory with the client's own helper in front
/// of nothing: it runs unshimmed and draws no window, running client or
/// not. The two shapes read the same way and are told apart only in the
/// words, since the fix (the shim pass) is the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExposedHelper {
    pub(crate) dir: PathBuf,
    /// `steamwebhelper_real.exe` is beside the helper: the client wrote its
    /// helper back over the shim. Without it the client made the directory
    /// after the shim pass.
    pub(crate) stashed: bool,
}

impl ExposedHelper {
    pub(crate) fn reason(self) -> BrokenReason {
        BrokenReason::HelperUnshimmed {
            dir: self.dir,
            stashed: self.stashed,
        }
    }

    /// The shape, as the `shim:` and `client:` lines name it.
    fn shape(&self) -> &'static str {
        if self.stashed {
            "the client wrote its web helper back over the shim; the stash is beside it"
        } else {
            "the client made the directory after the shim pass; no stash beside it"
        }
    }
}

/// The first populated `cef.win*` directory whose `steamwebhelper.exe` is
/// not a shim by its marker (every build of the shim carries it): a client
/// start wrote its helper back over the shim, or made the directory, since
/// the last shim pass. Needs no payload, so `status`, `install`, `repair`
/// and the scan after every start read the same thing; the scan runs after
/// every start since the client lays directories down as it pleases
/// (`cef.win64` appeared at a second start live, after the pass had
/// covered `cef.win7` and `cef.win7x64`). It stops at the first exposed
/// directory and reads none after it, so callers treat its error as `None`
/// or read it after [`orphan_shim`] and [`missing_web_helper`], which read
/// every helper first; `repair` reads them all ([`cef_helpers_read`]).
pub(crate) fn exposed_helper(paths: &ClientPaths) -> Result<Option<ExposedHelper>, CoreError> {
    let (dirs, _) = populated_cef_dirs(paths)?;
    for dir in dirs {
        if shim_file(&dir.join(WEBHELPER_EXE), None)? == Some(false) {
            let stashed = dir.join(WEBHELPER_REAL).is_file();
            return Ok(Some(ExposedHelper { dir, stashed }));
        }
    }
    Ok(None)
}

/// The first read failure of a `steamwebhelper.exe` or
/// `steamwebhelper_real.exe` under any `cef.win*` directory, read as the
/// shim pass reads them (a regular file or nothing): the pass writes
/// through each, so with one that does not read it would fail midway.
fn cef_helpers_read(paths: &ClientPaths) -> Result<(), CoreError> {
    for dir in cef_dirs(paths)? {
        shim_file(&dir.join(WEBHELPER_EXE), None)?;
        shim_file(&dir.join(WEBHELPER_REAL), None)?;
    }
    Ok(())
}

/// Whether the lines `path` gained past `offset` are the client ending on
/// its own with nothing done: a [`SHUTDOWN_MARK`] line and no download
/// line (a normal start logs `Download skipped` or the update it fetches).
fn clean_shutdown(path: &Path, offset: u64) -> bool {
    log_has_since(path, offset, |line| line.contains(SHUTDOWN_MARK))
        && !log_has_since(path, offset, |line| {
            line.to_ascii_lowercase().contains("download")
        })
}

/// The file's length, as the offset later reads start from; 0 when absent.
fn log_offset(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |meta| meta.len())
}

/// Whether a line written to `path` past `offset` satisfies `wanted`. The
/// client appends to its logs across runs, so only this run's lines count.
fn log_has_since(path: &Path, offset: u64, wanted: impl Fn(&str) -> bool) -> bool {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    // A log rotated or truncated during the wait is shorter than it was:
    // everything in it is this run's.
    let offset = match file.metadata() {
        Ok(meta) if meta.len() < offset => 0,
        _ => offset,
    };
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return false;
    }
    let mut text = String::new();
    if file.read_to_string(&mut text).is_err() {
        return false;
    }
    text.lines().any(wanted)
}

/// What says the login window is there, strongest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowSignal {
    /// The helper logged the window (`logs/webhelper.txt`).
    Logged,
    /// `loginusers.vdf` was written: someone signed in.
    LoginChanged,
    /// A `steamwebhelper.exe` is up, after the grace without a line.
    Helper,
}

impl WindowSignal {
    fn detail(self) -> &'static str {
        match self {
            Self::Logged => ": the login window exists (logs/webhelper.txt)",
            Self::LoginChanged => ": loginusers.vdf changed",
            Self::Helper => ": a steamwebhelper.exe is up; no window line in logs/webhelper.txt",
        }
    }
}

/// How a window wait ended without an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowWait {
    /// The window (or its fallback signal) is there.
    Window,
    /// The client this call started is gone with a clean shutdown in
    /// `logs/bootstrap_log.txt` and no download line, no stall, and no
    /// helper left missing.
    ShutDown,
}

/// What one look at a starting client found.
struct WindowSignals {
    /// The manifest exists and nothing is under `package/tmp`.
    updated: bool,
    signal: Option<WindowSignal>,
}

impl WindowSignals {
    fn phase(&self) -> (&'static str, &'static str) {
        match (self.updated, self.signal) {
            (false, _) => ("updating", ""),
            (true, Some(signal)) => ("window", signal.detail()),
            (true, None) => ("starting", ""),
        }
    }

    fn ready(&self) -> bool {
        self.updated && self.signal.is_some()
    }
}

/// The stall, if any: the assertion written to one of `logs` past its
/// offset, or an assert dump since `since`.
fn stall_evidence(
    logs: &[(PathBuf, u64)],
    paths: &ClientPaths,
    since: std::time::SystemTime,
) -> Option<PathBuf> {
    logs.iter()
        .find(|(path, offset)| log_has_since(path, *offset, |line| line.contains(STALL_ASSERTION)))
        .map(|(path, _)| path.clone())
        .or_else(|| assert_dump_since(paths, since))
}

/// An `assert_steam.exe_*.dmp` the client wrote since `since`, if any.
fn assert_dump_since(paths: &ClientPaths, since: std::time::SystemTime) -> Option<PathBuf> {
    let dumps = paths.steam_dir().join(DUMPS_DIR);
    let entries = fs::read_dir(dumps).ok()?;
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(ASSERT_DUMP_PREFIX))
        })
        .filter(|entry| {
            entry
                .metadata()
                .and_then(|meta| meta.modified())
                .is_ok_and(|modified| modified >= since)
        })
        .map(|entry| entry.path())
        .next()
}

/// The helper's line that says the login window exists.
fn is_login_window_line(line: &str) -> bool {
    line.contains(LOGIN_WINDOW_MARK)
        && (line.contains("Created window") || line.contains("WasHidden 0"))
}

/// Whether `loginusers.vdf` names a remembered account; `None` when the file
/// is absent or does not parse. Nothing of its contents leaves this function.
#[must_use]
pub fn remembered_login(text: &str) -> Option<bool> {
    let document = Document::parse(text).ok()?;
    let users = document.get_map("users")?;
    Some(users.iter().any(|user| {
        let remembered = user.value.get_str("RememberPassword") == Some("1");
        let auto = user
            .value
            .get_str("AllowAutoLogin")
            .is_none_or(|flag| flag == "1");
        remembered && auto
    }))
}

/// The client build id from `steam_client_win64.manifest` (or `win32`): the
/// root key's `version`.
#[must_use]
pub fn client_build_id(text: &str) -> Option<String> {
    let document = Document::parse(text).ok()?;
    let root = document.nodes.first()?;
    root.value.get_str("version").map(str::to_owned)
}

/// What `osslsigncode verify` said of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureReport {
    /// The chain verified and the digest matched.
    pub verified: bool,
    /// The leaf signer's subject, when a signature was found.
    pub subject: Option<String>,
}

/// Reads `osslsigncode verify` output. `success` is the tool's exit status;
/// the text decides nothing on its own, since a failed chain still prints
/// the signer.
#[must_use]
pub fn parse_signature_report(output: &str, success: bool) -> SignatureReport {
    let subject = output.lines().map(str::trim).find_map(|line| {
        line.strip_prefix("Subject:")
            .map(|rest| rest.trim().to_owned())
    });
    let verified = success
        && output
            .lines()
            .any(|line| line.trim() == "Signature verification: ok")
        && output.lines().any(|line| line.trim() == "Succeeded");
    SignatureReport { verified, subject }
}

/// The organisation Valve signs the bootstrapper as, exactly: the live
/// signer is `CN=Valve Corp.,O=Valve Corp.,L=Bellevue,ST=Washington,C=US`.
const VALVE_ORGS: [&str; 1] = ["Valve Corp."];

/// Whether a subject's organisation is Valve Corp., exactly, over RDNs
/// split on unescaped commas: `O=Valve Corp. Impersonators` and a `CN`
/// carrying `\, O=Valve Corp.` both fail.
#[must_use]
pub fn signer_is_valve(subject: &str) -> bool {
    split_rdns(subject).iter().any(|rdn| {
        rdn.strip_prefix("O=")
            .is_some_and(|org| VALVE_ORGS.contains(&org.trim()))
    })
}

/// RDNs split on unescaped commas; `\,` stays inside its value (RFC 2253).
fn split_rdns(subject: &str) -> Vec<String> {
    let mut rdns = Vec::new();
    let mut current = String::new();
    let mut chars = subject.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            ',' => rdns.push(std::mem::take(&mut current).trim().to_owned()),
            _ => current.push(c),
        }
    }
    rdns.push(current.trim().to_owned());
    rdns
}

/// Runs `osslsigncode verify` on `file` and refuses anything but a verified
/// chain whose leaf signer is Valve Corp. The file is kept either way.
///
/// # Errors
///
/// [`CoreError::BootstrapperUnverified`] naming the signer found, or the
/// spawn failure.
pub fn verify_bootstrapper(tool: &Path, file: &Path) -> Result<String, CoreError> {
    let output = Command::new(tool)
        .args(["verify", "-CAfile", CA_FILE, "-in"])
        .arg(file)
        .output()
        .map_err(|error| CoreError::io(tool, error))?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    let report = parse_signature_report(&text, output.status.success());
    let signer = report
        .subject
        .clone()
        .unwrap_or_else(|| "no signature found".to_owned());
    if !report.verified {
        return Err(CoreError::BootstrapperUnverified {
            path: file.to_path_buf(),
            signer,
            reason: "its Authenticode signature did not verify".to_owned(),
        });
    }
    if !signer_is_valve(&signer) {
        return Err(CoreError::BootstrapperUnverified {
            path: file.to_path_buf(),
            signer,
            reason: "its signer is not Valve Corp.".to_owned(),
        });
    }
    Ok(signer)
}

/// What an edit of `libraryfolders.vdf` did. A folder the client has not
/// seen needs a client restart (**unverified** live; the client reads the
/// file at startup); an app listed under a known folder does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryEdit {
    Unchanged,
    AppListed,
    FolderAdded,
}

/// The client's own install folder, index `0` of a file the client writes.
const CLIENT_LIBRARY: &str = "C:\\Program Files (x86)\\Steam";

fn same_library(a: &str, b: &str) -> bool {
    a.trim_end_matches('\\')
        .eq_ignore_ascii_case(b.trim_end_matches('\\'))
}

/// The `libraryfolders` map, made when the document has none.
fn library_folders(document: &mut Document) -> &mut Vec<Node> {
    let at = document
        .nodes
        .iter()
        .position(|node| node.key.eq_ignore_ascii_case("libraryfolders"))
        .unwrap_or_else(|| {
            document.nodes.push(Node::map("libraryfolders", Vec::new()));
            document.nodes.len() - 1
        });
    let root = &mut document.nodes[at];
    if !matches!(root.value, Value::Map(_)) {
        root.value = Value::Map(Vec::new());
    }
    match &mut root.value {
        Value::Map(folders) => folders,
        Value::Str(_) => unreachable!("made a map above"),
    }
}

/// Adds `windows_path` to a `libraryfolders.vdf` document as the client
/// writes one, with no apps, unless it is there. Everything else in the
/// file is kept.
pub fn ensure_library_folder(document: &mut Document, windows_path: &str) -> LibraryEdit {
    let folders = library_folders(document);
    let known = folders.iter().any(|folder| {
        folder
            .value
            .get_str("path")
            .is_some_and(|path| same_library(path, windows_path))
    });
    if known {
        return LibraryEdit::Unchanged;
    }
    let index = folders
        .iter()
        .filter_map(|folder| folder.key.parse::<u32>().ok())
        .max()
        .map_or(0, |max| max + 1);
    folders.push(Node::map(
        index.to_string(),
        vec![
            Node::str("path", windows_path),
            Node::str("label", ""),
            Node::str("contentid", content_id(windows_path).to_string()),
            Node::str("totalsize", "0"),
            Node::str("update_clean_bytes_tally", "0"),
            Node::str("time_last_update_verified", "0"),
            Node::map("apps", Vec::new()),
        ],
    ));
    LibraryEdit::FolderAdded
}

/// [`ensure_library_folder`], then lists `appid` under the folder with its
/// size.
pub fn add_library_folder(
    document: &mut Document,
    windows_path: &str,
    appid: u32,
    size_on_disk: u64,
) -> LibraryEdit {
    let added = ensure_library_folder(document, windows_path);
    let folders = library_folders(document);
    let size = size_on_disk.to_string();
    let key = appid.to_string();
    let Some(found) = folders.iter().position(|folder| {
        folder
            .value
            .get_str("path")
            .is_some_and(|path| same_library(path, windows_path))
    }) else {
        return added;
    };
    let Value::Map(fields) = &mut folders[found].value else {
        return added;
    };
    let apps = fields
        .iter()
        .position(|field| field.key.eq_ignore_ascii_case("apps"))
        .unwrap_or_else(|| {
            fields.push(Node::map("apps", Vec::new()));
            fields.len() - 1
        });
    let apps = &mut fields[apps];
    if !matches!(apps.value, Value::Map(_)) {
        apps.value = Value::Map(Vec::new());
    }
    let Value::Map(entries) = &mut apps.value else {
        return added;
    };
    let listed = match entries.iter_mut().find(|entry| entry.key == key) {
        Some(entry) if entry.value == Value::Str(size.clone()) => false,
        Some(entry) => {
            entry.value = Value::Str(size);
            true
        }
        None => {
            entries.push(Node::str(key, size));
            true
        }
    };
    match (added, listed) {
        (LibraryEdit::FolderAdded, _) => LibraryEdit::FolderAdded,
        (_, true) => LibraryEdit::AppListed,
        (_, false) => LibraryEdit::Unchanged,
    }
}

/// Edits the client's `libraryfolders.vdf` so it lists `windows_path`, and
/// `app` (appid and `SizeOnDisk`) under it. A file made from nothing gets
/// the client's own folder first, as the client writes it. The write goes
/// through a sibling and a rename, so the client never reads a partial
/// file.
///
/// # Errors
///
/// The read, parse, or write failure.
pub(crate) fn list_library(
    paths: &ClientPaths,
    windows_path: &str,
    app: Option<(u32, u64)>,
) -> Result<LibraryEdit, CoreError> {
    let file = paths.libraryfolders();
    let mut document = match fs::read_to_string(&file) {
        Ok(text) => Document::parse(&text)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut document = Document::default();
            ensure_library_folder(&mut document, CLIENT_LIBRARY);
            document
        }
        Err(error) => return Err(CoreError::io(&file, error)),
    };
    let edit = match app {
        Some((appid, size)) => add_library_folder(&mut document, windows_path, appid, size),
        None => ensure_library_folder(&mut document, windows_path),
    };
    if edit != LibraryEdit::Unchanged {
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent).map_err(|error| CoreError::io(parent, error))?;
        }
        let sibling = file.with_extension("vdf.applejack");
        fs::write(&sibling, document.to_text()).map_err(|error| CoreError::io(&sibling, error))?;
        fs::rename(&sibling, &file).map_err(|error| CoreError::io(&file, error))?;
    }
    Ok(edit)
}

/// A stable stand-in for the random id the client gives a library.
fn content_id(path: &str) -> u64 {
    path.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The Windows path the client sees `library` at inside `prefix_dir`:
/// `S:\` when that drive maps the library, else through `Z:`, which Wine
/// maps to `/`.
///
/// # Errors
///
/// [`CoreError::Io`] when the library cannot be resolved.
pub fn library_windows_path(prefix_dir: &Path, library: &Path) -> Result<String, CoreError> {
    let canonical = fs::canonicalize(library).map_err(|error| CoreError::io(library, error))?;
    let drive = prefix_dir.join("dosdevices").join("s:");
    if let Ok(target) = fs::read_link(&drive)
        && fs::canonicalize(&target).is_ok_and(|target| target == canonical)
    {
        return Ok("S:\\".to_owned());
    }
    let unix = canonical.to_string_lossy();
    Ok(format!("Z:{}", unix.replace('/', "\\")))
}

/// What the A2 watch saw of the title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Redownload {
    /// The client is fetching the depot over the adopted one: A2 refuted.
    Refuted(String),
    /// A routine patch of an install that is behind its branch: the build
    /// moves forward, by `buildid` or by `TargetBuildID`.
    Update { from: u64, to: u64 },
}

/// Watches a title for the client re-downloading it, from the `.acf` and
/// the install directory. Plan 00's assumption A2 says the client adopts
/// the install with a verification pass only; this is where it is refuted.
/// A build moving forward is the client patching an out-of-date install,
/// which A2 allows for and `install --update` is the answer to; a build
/// unchanged or going back under update bits, or the directory growing, is
/// the refutation.
#[derive(Debug)]
pub struct RedownloadWatch {
    acf: PathBuf,
    backup: PathBuf,
    before: AppManifest,
    install_dir: PathBuf,
    baseline: u64,
    scanned_at: Option<Instant>,
}

impl RedownloadWatch {
    /// Copies the `.acf` to its backup and measures the install directory.
    ///
    /// # Errors
    ///
    /// The read, parse, or copy failure.
    pub fn start(library: &Path, appid: u32, install_name: &str) -> Result<Self, CoreError> {
        let paths = LibraryPaths::new(library);
        let acf = paths.acf(appid);
        let text = fs::read_to_string(&acf).map_err(|error| CoreError::io(&acf, error))?;
        let before = AppManifest::parse(&text)?;
        let backup = acf_backup_path(library, appid);
        if let Some(parent) = backup.parent() {
            fs::create_dir_all(parent).map_err(|error| CoreError::io(parent, error))?;
        }
        fs::copy(&acf, &backup).map_err(|error| CoreError::io(&backup, error))?;
        let install_dir = paths.install_dir(install_name);
        Ok(Self {
            acf,
            backup,
            before,
            baseline: dir_size(&install_dir),
            install_dir,
            scanned_at: None,
        })
    }

    /// What changed, when the client is fetching the title.
    pub fn redownload(&mut self) -> Option<Redownload> {
        let from = self.before.buildid;
        if let Ok(text) = fs::read_to_string(&self.acf)
            && let Ok(now) = AppManifest::parse(&text)
        {
            if now.buildid > from {
                return Some(Redownload::Update {
                    from,
                    to: now.buildid,
                });
            }
            if now.buildid < from {
                return Some(Redownload::Refuted(format!(
                    "buildid changed from {from} to {}",
                    now.buildid
                )));
            }
            if let Some(to) = target_buildid(&text)
                && to > from
            {
                return Some(Redownload::Update { from, to });
            }
            if now.state_flags & UPDATE_BITS != 0 {
                return Some(Redownload::Refuted(format!(
                    "StateFlags became {}",
                    now.state_flags
                )));
            }
        }
        if self
            .scanned_at
            .is_none_or(|at| at.elapsed() >= SIZE_SCAN_EVERY)
        {
            self.scanned_at = Some(Instant::now());
            let size = dir_size(&self.install_dir);
            if size.saturating_sub(self.baseline) >= REDOWNLOAD_GROWTH {
                return Some(Redownload::Refuted(format!(
                    "the install directory grew by {} bytes",
                    size - self.baseline
                )));
            }
        }
        None
    }

    /// Puts the `.acf` back as it was before the launch.
    ///
    /// # Errors
    ///
    /// The copy failure.
    pub fn restore(&self) -> Result<(), CoreError> {
        fs::copy(&self.backup, &self.acf).map_err(|error| CoreError::io(&self.acf, error))?;
        Ok(())
    }
}

/// `TargetBuildID`, which the client writes beside `buildid` while an update
/// of the title is pending; `0` or absent otherwise.
fn target_buildid(acf: &str) -> Option<u64> {
    Document::parse(acf)
        .ok()?
        .get_str("AppState/TargetBuildID")?
        .parse()
        .ok()
}

/// `<library>/applejack/<appid>.acf.backup`, beside the download record.
#[must_use]
pub fn acf_backup_path(library: &Path, appid: u32) -> PathBuf {
    library
        .join("applejack")
        .join(format!("{appid}.acf.backup"))
}

fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                pending.push(entry.path());
            } else if meta.is_file() {
                total += meta.len();
            }
        }
    }
    total
}

/// Fetches `url` to `dest`, through a sibling file and a rename.
pub type Fetcher<'a> =
    &'a dyn Fn(&str, &Path, &mut dyn FnMut(u64, Option<u64>)) -> Result<(), CoreError>;

/// Downloads with the crate's HTTP client.
///
/// # Errors
///
/// [`CoreError::BootstrapperDownload`] for any HTTP failure, or the write.
pub fn fetch_url(
    url: &str,
    dest: &Path,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<(), CoreError> {
    let failed = |reason: String| CoreError::BootstrapperDownload {
        url: url.to_owned(),
        reason,
    };
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|error| CoreError::io(parent, error))?;
    }
    let partial = dest.with_extension("partial");
    let runtime = steam_runtime()?;
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .user_agent("applejack")
            .build()
            .map_err(|error| failed(error.to_string()))?;
        let mut response = client
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| failed(error.to_string()))?;
        let total = response.content_length();
        let mut file =
            fs::File::create(&partial).map_err(|error| CoreError::io(&partial, error))?;
        let mut done = 0;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| failed(error.to_string()))?
        {
            file.write_all(&chunk)
                .map_err(|error| CoreError::io(&partial, error))?;
            done += chunk.len() as u64;
            progress(done, total);
        }
        file.flush()
            .map_err(|error| CoreError::io(&partial, error))?;
        Ok::<(), CoreError>(())
    })?;
    fs::rename(&partial, dest).map_err(|error| CoreError::io(dest, error))
}

/// A claimed runtime operation: the prefix lock is held.
#[derive(Debug)]
pub struct PlannedClientOp {
    core: Core,
    _lock: PrefixLock,
    installed: InstalledEngine,
    paths: ClientPaths,
    op: ClientOp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientOp {
    Install,
    Update,
    Repair { reinstall: bool },
    Stop,
}

impl Core {
    pub(crate) fn client_paths(&self) -> Result<ClientPaths, CoreError> {
        Ok(ClientPaths::new(self.prefix_dir(STEAM_CLIENT_PREFIX)?))
    }

    /// The runtime's state and why, from disk. Nothing is written or locked.
    ///
    /// # Errors
    ///
    /// The prefix path failure.
    pub fn client_runtime_state(&self) -> Result<(ClientRuntimeState, String), CoreError> {
        self.runtime_state(false)
    }

    /// [`Core::client_runtime_state`]; `lock_held` from a caller that holds
    /// the prefix lock itself, for whom a marker is a dead install's and
    /// never its own.
    pub(crate) fn runtime_state(
        &self,
        lock_held: bool,
    ) -> Result<(ClientRuntimeState, String), CoreError> {
        Ok(match self.runtime_reading(lock_held)? {
            RuntimeReading::Broken(reason) => (ClientRuntimeState::Broken, reason.detail()),
            RuntimeReading::Other(state, detail) => (state, detail),
        })
    }

    /// [`Core::runtime_state`], with a `broken` runtime's reason kept whole.
    pub(crate) fn runtime_reading(&self, lock_held: bool) -> Result<RuntimeReading, CoreError> {
        let paths = self.client_paths()?;
        let dir = &paths.prefix_dir;
        if !dir.is_dir() {
            return Ok(RuntimeReading::Other(
                ClientRuntimeState::Absent,
                "no steam-client prefix".to_owned(),
            ));
        }
        let marker = paths.install_marker();
        if marker.exists() {
            let installing =
                !lock_held && PrefixLock::is_held(&self.snapshots_root(), STEAM_CLIENT_PREFIX)?;
            return Ok(if installing {
                RuntimeReading::Other(
                    ClientRuntimeState::Installing,
                    "an install is running".to_owned(),
                )
            } else {
                RuntimeReading::Broken(BrokenReason::DeadInstall)
            });
        }
        if let Err(error) = self.engine.open_prefix(dir) {
            return Ok(RuntimeReading::Broken(BrokenReason::RecordDoesNotOpen(
                error.to_string(),
            )));
        }
        if !paths.steam_exe().is_file() {
            return Ok(if paths.steam_dir().is_dir() {
                RuntimeReading::Broken(BrokenReason::NoSteamExe(paths.steam_dir()))
            } else {
                RuntimeReading::Other(
                    ClientRuntimeState::Absent,
                    "the client is not installed in the prefix".to_owned(),
                )
            });
        }
        let broken = |reason: BrokenReason| Ok(RuntimeReading::Broken(reason));
        match orphan_shim(&paths) {
            Ok(Some(dir)) => return broken(BrokenReason::ShimWithoutReal(dir)),
            Ok(None) => {}
            Err(error) => return broken(BrokenReason::HelperUnreadable(error.to_string())),
        }
        if !paths.manifest().is_file() {
            return Ok(RuntimeReading::Other(
                ClientRuntimeState::Updating,
                "the client has not finished its first self-update".to_owned(),
            ));
        }
        if paths.package_tmp().is_dir() && wineserver_running(dir).unwrap_or(true) {
            return Ok(RuntimeReading::Other(
                ClientRuntimeState::Updating,
                "the client is updating itself".to_owned(),
            ));
        }
        match missing_web_helper(&paths) {
            Ok(Some(dir)) => return broken(BrokenReason::NoWebHelper(dir)),
            Ok(None) => {}
            Err(error) => return broken(BrokenReason::HelperUnreadable(error.to_string())),
        }
        match exposed_helper(&paths) {
            Ok(Some(exposed)) => return broken(exposed.reason()),
            Ok(None) => {}
            Err(error) => return broken(BrokenReason::HelperUnreadable(error.to_string())),
        }
        let login = fs::read_to_string(paths.loginusers())
            .ok()
            .and_then(|text| remembered_login(&text));
        Ok(match login {
            Some(true) => RuntimeReading::Other(
                ClientRuntimeState::Ready,
                "an account is remembered".to_owned(),
            ),
            _ => RuntimeReading::Other(
                ClientRuntimeState::NeedsLogin,
                "no account is signed in; run `applejack runtime steam install` and sign in inside the Steam window"
                    .to_owned(),
            ),
        })
    }

    /// `runtime.steam.status`.
    ///
    /// # Errors
    ///
    /// The database or prefix failure.
    pub fn client_runtime_status(&mut self) -> Result<ClientRuntimeStatus, CoreError> {
        self.runtime_status(false)
    }

    pub(crate) fn runtime_status(
        &mut self,
        lock_held: bool,
    ) -> Result<ClientRuntimeStatus, CoreError> {
        let (state, detail) = self.runtime_state(lock_held)?;
        let paths = self.client_paths()?;
        let running =
            paths.prefix_dir.is_dir() && wineserver_running(&paths.prefix_dir).unwrap_or(false);
        let build_id = self.reading(|db| Ok(db.meta_get(META_CLIENT_BUILD)?))?;
        let snapshots = self.prefix_snapshots(STEAM_CLIENT_PREFIX)?.len();
        Ok(ClientRuntimeStatus {
            state,
            prefix: paths.prefix_dir,
            build_id,
            running,
            snapshots,
            detail,
        })
    }

    /// The session the running client was started with, if one is recorded.
    pub(crate) fn client_session_key(&mut self) -> Result<Option<String>, CoreError> {
        self.reading(|db| Ok(db.meta_get(META_CLIENT_SESSION)?))
    }

    /// Whether the client is up: its prefix has a server, or a `steam.exe`
    /// still carries the key it was started with.
    pub(crate) fn client_is_up(&mut self) -> Result<bool, CoreError> {
        let paths = self.client_paths()?;
        if paths.prefix_dir.is_dir() && wineserver_running(&paths.prefix_dir).unwrap_or(true) {
            return Ok(true);
        }
        if self.client_session_key()?.is_some_and(|key| {
            find_session(&key)
                .iter()
                .any(|process| process.exe.eq_ignore_ascii_case(STEAM_EXE))
        }) {
            return Ok(true);
        }
        Ok(find_in_prefix(&paths.prefix_dir)
            .iter()
            .any(|process| process.exe.eq_ignore_ascii_case(STEAM_EXE)))
    }

    /// Claims the client prefix for `op`. Refused while a title runs through
    /// the client.
    ///
    /// # Errors
    ///
    /// `PrefixBusy`, [`CoreError::NoActiveEngine`], or
    /// [`CoreError::ClientRuntimeBroken`] for a stop or update of a runtime
    /// that is absent.
    pub fn plan_client_op(&mut self, op: ClientOp) -> Result<PlannedClientOp, CoreError> {
        let lock = self.lock_idle_prefix(STEAM_CLIENT_PREFIX)?;
        let installed = self.require_active_engine()?;
        let paths = self.client_paths()?;
        if matches!(op, ClientOp::Update | ClientOp::Stop) && !paths.steam_exe().is_file() {
            return Err(CoreError::ClientRuntimeBroken {
                state: ClientRuntimeState::Absent,
                detail: "run `applejack runtime steam install` first".to_owned(),
            });
        }
        Ok(PlannedClientOp {
            core: self.detached(),
            _lock: lock,
            installed,
            paths,
            op,
        })
    }
}

impl PlannedClientOp {
    /// Runs the operation with Valve's URL as the bootstrapper source.
    ///
    /// # Errors
    ///
    /// As [`PlannedClientOp::run_with`].
    pub fn run(
        self,
        report: &mut dyn FnMut(&str, String),
    ) -> Result<ClientRuntimeStatus, CoreError> {
        self.run_with(&fetch_url, report)
    }

    /// Runs the operation; `fetch` is how the bootstrapper arrives.
    ///
    /// # Errors
    ///
    /// [`CoreError::BootstrapperDownload`], [`CoreError::BootstrapperUnverified`],
    /// [`CoreError::ClientRuntimeBroken`] when the client never comes up or
    /// a `repair` finds nothing broken, or the engine failure.
    pub fn run_with(
        mut self,
        fetch: Fetcher<'_>,
        report: &mut dyn FnMut(&str, String),
    ) -> Result<ClientRuntimeStatus, CoreError> {
        match self.dispatch(fetch, report) {
            Ok(()) => self.core.runtime_status(true),
            // Every op's runtime error goes out through here, after any stop
            // or restore the op made: the state is what `status` reads now,
            // the detail the op's own. A reading that fails keeps the op's
            // state, which is then the best there is.
            Err(CoreError::ClientRuntimeBroken { state, detail }) => {
                let state = self
                    .core
                    .runtime_state(true)
                    .map_or(state, |(reading, _)| reading);
                Err(CoreError::ClientRuntimeBroken { state, detail })
            }
            Err(error) => Err(error),
        }
    }

    fn dispatch(
        &mut self,
        fetch: Fetcher<'_>,
        report: &mut dyn FnMut(&str, String),
    ) -> Result<(), CoreError> {
        match self.op {
            ClientOp::Install => self.install(fetch, false, report),
            ClientOp::Repair { reinstall: true } => self.install(fetch, true, report),
            ClientOp::Repair { reinstall: false } => self.repair(report),
            ClientOp::Update => self.update(report),
            ClientOp::Stop => self.stop(report),
        }
    }

    fn log_path(&self) -> PathBuf {
        self.core.log_dir().join("steam-client").join("runtime.log")
    }

    fn client_running(&mut self) -> bool {
        self.core.client_is_up().unwrap_or(true)
    }

    /// Whether the client is up to wait on: a `wineserver` runs for the
    /// prefix. A `steam.exe` the record still names with no server behind
    /// it is a client that died (live, every Wine process goes with the
    /// server; the stub's linger), and a wait on it ends at a window that
    /// cannot come, so it is stopped and started instead.
    fn client_up_with_server(&self) -> bool {
        wineserver_running(&self.paths.prefix_dir).unwrap_or(true)
    }

    fn stop(&mut self, report: &mut dyn FnMut(&str, String)) -> Result<(), CoreError> {
        if !self.client_running() {
            report("stop", "the client is not running".to_owned());
            return Ok(());
        }
        report("stop", "stopping the Steam client".to_owned());
        self.core
            .engine
            .stop_server(&self.installed, &self.paths.prefix_dir, &self.log_path())?;
        Ok(())
    }

    fn ensure_prefix(&self, report: &mut dyn FnMut(&str, String)) -> Result<Prefix, CoreError> {
        if self.paths.prefix_dir.is_dir() {
            let (_, prefix) = self.core.open_prefix(STEAM_CLIENT_PREFIX)?;
            return Ok(prefix);
        }
        // The same rule a run applies, read once here, so the client starts
        // with the inventory's default and the first run has nothing to flip
        // while the client owns the prefix.
        let hidraw = HidrawDecision::from_inventory(&self.core.controllers(), None);
        report("prepare", hidraw.to_string());
        report("prefix", format!("creating prefix {STEAM_CLIENT_PREFIX}"));
        let library = self.core.library_dir()?;
        self.core.build_prefix(
            STEAM_CLIENT_PREFIX,
            &self.installed,
            GraphicsBackend::Dxmt,
            None,
            hidraw.mode,
            library,
        )
    }

    fn write_marker(&self) -> Result<(), CoreError> {
        let marker = self.paths.install_marker();
        if let Some(parent) = marker.parent() {
            fs::create_dir_all(parent).map_err(|error| CoreError::io(parent, error))?;
        }
        let text = serde_json::json!({"started_at": utc_now(), "op": format!("{:?}", self.op)});
        fs::write(&marker, text.to_string()).map_err(|error| CoreError::io(&marker, error))
    }

    fn install(
        &mut self,
        fetch: Fetcher<'_>,
        reinstall: bool,
        report: &mut dyn FnMut(&str, String),
    ) -> Result<(), CoreError> {
        let prefix = self.ensure_prefix(report)?;
        // The lock is held, so a marker on disk is a dead install's; left
        // there it reads `installing` under this lock and `broken` after it.
        let marker = self.paths.install_marker();
        if marker.exists() {
            report(
                "install",
                "clearing the marker of an install that died".to_owned(),
            );
            fs::remove_file(&marker).map_err(|error| CoreError::io(&marker, error))?;
        }
        if self.client_up_with_server() && !reinstall {
            // The window the user signs in to is up; a second steam.exe
            // would only hand off to it. A payload fault is reported when
            // the client next starts, not here.
            if let Some(exposed) = exposed_helper(&self.paths).ok().flatten() {
                // No window to wait for: the helper in front runs without
                // the shim, whichever way it got there.
                report(
                    "shim",
                    format!(
                        "{}: the web helper running is the client's own ({}), so the window \
                         cannot draw; `applejack runtime steam stop`, then `install` or \
                         `repair`, put the shim in front of it",
                        exposed.dir.display(),
                        exposed.shape()
                    ),
                );
                return Err(exposed.reason().into_error());
            }
            if let Some(dir) = orphan_shim(&self.paths).ok().flatten() {
                report(
                    "shim",
                    format!(
                        "{}: the web helper is the shim with no {WEBHELPER_REAL} beside it, so \
                         the window cannot draw; `applejack runtime steam stop`, then \
                         `install`, lay the helper down again",
                        dir.display()
                    ),
                );
            } else if let Some(dir) = self
                .paths
                .manifest()
                .is_file()
                .then(|| missing_web_helper(&self.paths).ok().flatten())
                .flatten()
            {
                report(
                    "shim",
                    format!(
                        "{}: no web helper, so the window cannot draw; `applejack runtime \
                         steam stop`, then `install` (its file check) or `applejack runtime \
                         steam repair --reinstall`, lay the helper down again",
                        dir.display()
                    ),
                );
            }
            self.wait_window(None, None, false, report)?;
            self.finish(report)?;
            return self.end_check();
        }
        if reinstall {
            // Refused here, not at the bootstrapper: past this point the
            // client's package is gone.
            self.payload_preflight()?;
            self.stop(report)?;
            let package = self.paths.steam_dir().join("package");
            if package.is_dir() {
                report("repair", format!("removing {}", package.display()));
                fs::remove_dir_all(&package).map_err(|error| CoreError::io(&package, error))?;
            }
        }
        if !self.paths.steam_exe().is_file() || reinstall {
            self.write_marker()?;
            // A refusal is not a half-done install: only a death between
            // here and the removal leaves the marker, and that reads as broken.
            let bootstrapped = self.bootstrap(fetch, &prefix, report);
            let _ = fs::remove_file(self.paths.install_marker());
            bootstrapped?;
        }
        self.first_run(&prefix, report)?;
        self.end_check()
    }

    /// The payload files the bootstrap path needs, checked before anything
    /// is fetched or removed: `osslsigncode` to verify the bootstrapper and
    /// the shim without which the window would not draw. The loader is
    /// checked when the engine loads, before any op is planned. Returns the
    /// verifier.
    fn payload_preflight(&self) -> Result<PathBuf, CoreError> {
        let tool = self.installed.root.join(OSSLSIGNCODE);
        if !tool.is_file() {
            // The state is `run_with`'s to set from the reading.
            return Err(CoreError::ClientRuntimeBroken {
                state: ClientRuntimeState::Absent,
                detail: format!(
                    "the engine payload ships no `{OSSLSIGNCODE}`, so the bootstrapper cannot be \
                     verified; install a payload that ships it"
                ),
            });
        }
        shim_path(&self.installed.root)?;
        Ok(tool)
    }

    fn bootstrap(
        &mut self,
        fetch: Fetcher<'_>,
        prefix: &Prefix,
        report: &mut dyn FnMut(&str, String),
    ) -> Result<(), CoreError> {
        let tool = self.payload_preflight()?;
        let file = self.paths.bootstrapper();
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent).map_err(|error| CoreError::io(parent, error))?;
        }
        report("download", format!("fetching {BOOTSTRAPPER_URL}"));
        fetch(BOOTSTRAPPER_URL, &file, &mut |done, total| {
            let total = total.map_or(String::new(), |total| format!("/{total}"));
            report("download", format!("{done}{total} bytes"));
        })?;
        let signer = verify_bootstrapper(&tool, &file)?;
        report(
            "verify",
            format!("Authenticode chain verified; signer {signer}"),
        );
        report("bootstrap", "running SteamSetup.exe /S".to_owned());
        let key = new_session_id("steam-bootstrap");
        let spec = LaunchSpec {
            session_id: Some(key),
            exe: file.clone(),
            args: vec!["/S".into()],
            env_extras: Vec::new(),
            cwd: prefix.dir.clone(),
            log_dir: self.core.log_dir().to_path_buf(),
            advertise_avx: false,
            cpuid_probe: None,
        };
        let mut session = self.core.engine.launch(&self.installed, prefix, &spec)?;
        let timeout = self.core.tuning.first_run_timeout;
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = session.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = session.cancel();
                return Err(BrokenReason::BootstrapperTimedOut {
                    secs: timeout.as_secs(),
                    log: session.log_path.clone(),
                }
                .into_error());
            }
            std::thread::sleep(CLIENT_POLL);
        };
        if !status.success() {
            return Err(BrokenReason::BootstrapperExited(
                status.to_string(),
                session.log_path.clone(),
            )
            .into_error());
        }
        Core::settle(&prefix.dir)?;
        if !self.paths.steam_exe().is_file() {
            return Err(BrokenReason::BootstrapperLeftNoExe(self.paths.steam_exe()).into_error());
        }
        Ok(())
    }

    /// Starts `steam.exe` and waits for its self-update and the login
    /// window. The client stays up for the user. After every start whose
    /// wait returns the window (or its fallback signal), the `cef.win*`
    /// directories are scanned again: a helper the client laid down without
    /// the shim in front (its first self-update, the file check after an
    /// orphan shim was removed, a client update, or a directory the client
    /// made at that start) stops the client, and the next start's shim pass
    /// stashes it, at most [`RESHIM_ROUNDS`] times; past that the op ends
    /// `broken` naming the directory and its shape. A start that shuts down
    /// on its own with nothing in its bootstrap log but the shutdown is
    /// started once more, once per op, before the exit is an error.
    fn first_run(
        &mut self,
        prefix: &Prefix,
        report: &mut dyn FnMut(&str, String),
    ) -> Result<(), CoreError> {
        if self.client_up_with_server() {
            self.wait_window(None, None, false, report)?;
            return self.finish(report);
        }
        // The client reads `libraryfolders.vdf` at startup, so the shared
        // library is listed before the client has ever started.
        let library = self.core.library_dir()?;
        let windows_path = library_windows_path(&self.paths.prefix_dir, &library)?;
        if list_library(&self.paths, &windows_path, None)? == LibraryEdit::FolderAdded {
            report(
                "library",
                format!("listing {windows_path} in libraryfolders.vdf before the client starts"),
            );
        }
        let mut restarts = 0;
        let mut retried = false;
        loop {
            match self.start_client(prefix, report)? {
                WindowWait::Window => {}
                WindowWait::ShutDown if !retried => {
                    retried = true;
                    report(
                        "client",
                        "the client shut down on its own with nothing but the shutdown in \
                         logs/bootstrap_log.txt (no download line); starting it once more"
                            .to_owned(),
                    );
                    Core::settle(&self.paths.prefix_dir)?;
                    continue;
                }
                WindowWait::ShutDown => {
                    return Err(BrokenReason::ClientExitedEarly(
                        self.paths.steam_dir().join(STALL_LOGS[1]),
                    )
                    .into_error());
                }
            }
            // A helper that does not read is `HelperUnreadable`, which
            // `finish` reports as `status` reads it; the io error alone
            // would end the op with no state line.
            let Some(exposed) = exposed_helper(&self.paths).ok().flatten() else {
                break;
            };
            if restarts == RESHIM_ROUNDS {
                // The same read `status` makes, so the error and `status`
                // name the same shape.
                report(
                    "client",
                    format!(
                        "{}/{WEBHELPER_EXE} is still not the shim after {RESHIM_ROUNDS} restarts; \
                         giving up",
                        exposed.dir.display()
                    ),
                );
                return Err(exposed.reason().into_error());
            }
            restarts += 1;
            report(
                "client",
                format!(
                    "the client laid its web helper down without the shim in front of it ({}); \
                     restarting the client with the shim in front of it (restart {restarts} of \
                     {RESHIM_ROUNDS})",
                    exposed.dir.display()
                ),
            );
            self.stop(report)?;
            Core::settle(&self.paths.prefix_dir)?;
        }
        self.finish(report)
    }

    /// One start of the client to its login window: the shim check, the
    /// prompt warning, the launch under a key of its own, the wait.
    fn start_client(
        &mut self,
        prefix: &Prefix,
        report: &mut dyn FnMut(&str, String),
    ) -> Result<WindowWait, CoreError> {
        let pass = ensure_webhelper_shim(&self.installed.root, &self.paths, report)?;
        report(
            "start",
            "macOS may ask whether \"applejack\" may use the microphone within about 15 s of \
             the client's start: answer it (Don't Allow is fine); left unanswered, the client \
             stalls and dies about 3 minutes in"
                .to_owned(),
        );
        let args = if pass.helper_missing {
            let lays_it = if self.paths.manifest().is_file() {
                "file check"
            } else {
                "self-update"
            };
            report(
                "start",
                format!(
                    "starting steam.exe for its self-update, without {NOVERIFYFILES}: a web \
                     helper is missing and the client's {lays_it} lays it down"
                ),
            );
            Vec::new()
        } else {
            report("start", "starting steam.exe for its self-update".to_owned());
            vec![NOVERIFYFILES.into()]
        };
        let key = new_session_id("steam-client");
        let spec = LaunchSpec {
            session_id: Some(key.clone()),
            exe: self.paths.steam_exe(),
            args,
            env_extras: Vec::new(),
            cwd: self.paths.steam_dir(),
            log_dir: self.core.log_dir().to_path_buf(),
            advertise_avx: false,
            cpuid_probe: None,
        };
        let session = self.core.engine.launch(&self.installed, prefix, &spec)?;
        self.core.db()?.meta_set(META_CLIENT_SESSION, &key)?;
        let _ = log_line(
            &self.log_path().to_string_lossy(),
            &format!(
                "client started under session {key}; log {}",
                session.log_path.display()
            ),
        );
        // The loader is the client and outlives this call; `Session`'s drop
        // is one non-blocking `try_wait`, so nothing here waits on it.
        let session_log = session.log_path.clone();
        drop(session);
        self.wait_window(Some(&key), Some(&session_log), pass.helper_missing, report)
    }

    /// The directory the start without `-noverifyfiles` was to fill and did
    /// not: the pass found one (`helper_missing`) and it is still empty.
    /// `None` with no manifest: that start was the client's self-update,
    /// which `status` reads as `updating` and the next `install` resumes,
    /// so the plain exit reasons hold.
    fn helper_still_missing(&self, helper_missing: bool) -> Option<PathBuf> {
        if !helper_missing || !self.paths.manifest().is_file() {
            return None;
        }
        missing_web_helper(&self.paths).ok().flatten()
    }

    /// The client is gone during the wait. One this call started
    /// (`own_start`), with no stall and no helper left missing, whose
    /// bootstrap log gained a clean shutdown and no download line since the
    /// start (`bootstrap` is the log and its offset then) is
    /// [`WindowWait::ShutDown`] for the caller to retry once; anything else
    /// is [`PlannedClientOp::client_gone`]'s error.
    fn client_gone_or_shut_down(
        &self,
        own_start: bool,
        stalled: Option<PathBuf>,
        helper_missing: bool,
        bootstrap: (&Path, u64),
    ) -> Result<WindowWait, CoreError> {
        if own_start
            && stalled.is_none()
            && self.helper_still_missing(helper_missing).is_none()
            && clean_shutdown(bootstrap.0, bootstrap.1)
        {
            return Ok(WindowWait::ShutDown);
        }
        Err(self.client_gone(stalled, helper_missing))
    }

    /// The client exited during the wait: the stall when one was seen, the
    /// helper it was to lay down and did not, else a bare exit pointing at
    /// the client's bootstrap log.
    fn client_gone(&self, stalled: Option<PathBuf>, helper_missing: bool) -> CoreError {
        match (stalled, self.helper_still_missing(helper_missing)) {
            (Some(evidence), _) => BrokenReason::ClientStalledOnPrompt(evidence),
            (None, Some(dir)) => BrokenReason::HelperNotLaidDown(dir),
            (None, None) => {
                BrokenReason::ClientExitedEarly(self.paths.steam_dir().join(STALL_LOGS[1]))
            }
        }
        .into_error()
    }

    /// The first-run timeout passed without the window: the client is
    /// stopped and its files kept, and the error carries what `status`
    /// reads then, under the timeout's own lead, so the two cannot differ.
    fn window_timed_out(&mut self, report: &mut dyn FnMut(&str, String)) -> CoreError {
        let _ = self.stop(report);
        let secs = self.core.tuning.first_run_timeout.as_secs();
        let lead = format!(
            "the client did not reach the login window within {secs} seconds; it was stopped \
             and its files kept"
        );
        match self.core.runtime_reading(true) {
            Ok(RuntimeReading::Broken(reason)) => CoreError::ClientRuntimeBroken {
                state: ClientRuntimeState::Broken,
                detail: format!(
                    "{lead}; {}",
                    shown_detail(reason.detail(), reason.shown_by_status())
                ),
            },
            Ok(RuntimeReading::Other(state, detail)) => {
                // `needs_login`'s detail and `updating`'s and `installing`'s
                // advice name a way out; these states name none, and a
                // failed op must.
                let again = match state {
                    ClientRuntimeState::Absent | ClientRuntimeState::Ready => {
                        "; `applejack runtime steam install` starts the client to its window again"
                    }
                    ClientRuntimeState::Installing
                    | ClientRuntimeState::Updating
                    | ClientRuntimeState::NeedsLogin
                    | ClientRuntimeState::Broken => "",
                };
                CoreError::ClientRuntimeBroken {
                    state,
                    detail: format!("{lead}; {detail}{again}"),
                }
            }
            Err(error) => error,
        }
    }

    /// Whether a `steamwebhelper.exe` is up: by the key when this core
    /// started the client, else by the prefix (a client `prefix exec` or an
    /// earlier core started).
    fn helper_up(&self, key: Option<&str>) -> bool {
        let is_helper = |process: &applejack_engine::rosetta_wine::Process| {
            process.exe.eq_ignore_ascii_case(WEBHELPER_EXE)
        };
        key.is_some_and(|key| find_session(key).iter().any(is_helper))
            || find_in_prefix(&self.paths.prefix_dir).iter().any(is_helper)
    }

    /// After the window: the build id into `meta`, the state as the last line.
    fn finish(&mut self, report: &mut dyn FnMut(&str, String)) -> Result<(), CoreError> {
        if let Some(build) = record_client_build(&mut self.core, &self.paths)? {
            report("build", format!("client build {build}"));
        }
        let (state, detail) = self.core.runtime_state(true)?;
        report(state.as_str(), detail);
        Ok(())
    }

    /// Waits for the self-update to finish and the login window to exist:
    /// the manifest exists, nothing is left under `package/tmp`, and the
    /// helper has logged the login window (`DesktopLoginWindow_uid0 ...
    /// Created window` or `WasHidden 0` in `logs/webhelper.txt`, written
    /// since this wait began). A `loginusers.vdf` write counts too, and a
    /// `steamwebhelper.exe` process counts after [`WINDOW_FALLBACK_GRACE`]
    /// without the line, at once for a client this call did not start
    /// (`session_log` is `None`), whose line predates the wait. A stall
    /// (the assertion in `session_log`, the client's logs, or an assert
    /// dump written since the start) is reported, and the process then no
    /// longer counts: the client dies about 3 minutes in unless the prompt
    /// is answered, and that death is reported as the prompt.
    /// `helper_missing` is the shim pass's: a client that exits during the
    /// wait names the helper the start did not lay down. A client this
    /// call started that is gone with a clean shutdown in its bootstrap log
    /// and nothing else to name is [`WindowWait::ShutDown`], the caller's to
    /// retry once; any other exit is the error.
    fn wait_window(
        &mut self,
        key: Option<&str>,
        session_log: Option<&Path>,
        helper_missing: bool,
        report: &mut dyn FnMut(&str, String),
    ) -> Result<WindowWait, CoreError> {
        let key = match key {
            Some(key) => Some(key.to_owned()),
            None => self.core.client_session_key()?,
        };
        let login_before = fs::metadata(self.paths.loginusers())
            .and_then(|meta| meta.modified())
            .ok();
        let steam = self.paths.steam_dir();
        let helper_log = self.paths.webhelper_log();
        let helper_log_before = log_offset(&helper_log);
        let bootstrap_log = steam.join(STALL_LOGS[1]);
        let bootstrap_before = log_offset(&bootstrap_log);
        let stall_logs: Vec<(PathBuf, u64)> = session_log
            .map(Path::to_path_buf)
            .into_iter()
            .chain(STALL_LOGS.iter().map(|name| steam.join(name)))
            .map(|path| {
                let offset = log_offset(&path);
                (path, offset)
            })
            .collect();
        let grace = session_log.map_or(Duration::ZERO, |_| WINDOW_FALLBACK_GRACE);
        let started_at = std::time::SystemTime::now();
        let started = Instant::now();
        let deadline = started + self.core.tuning.first_run_timeout;
        let mut settled = 0;
        let mut gone = 0;
        let mut stalled: Option<PathBuf> = None;
        let mut last_phase = String::new();
        loop {
            if stalled.is_none() {
                stalled = stall_evidence(&stall_logs, &self.paths, started_at);
                if let Some(evidence) = &stalled {
                    report(
                        "client",
                        format!(
                            "stalled: the client's main loop is stalled ({}); a macOS prompt is \
                             waiting, usually the microphone: answer it (Don't Allow is fine) \
                             within about 3 minutes of the start or the client dies",
                            evidence.display()
                        ),
                    );
                }
            }
            let login_changed = fs::metadata(self.paths.loginusers())
                .and_then(|meta| meta.modified())
                .ok()
                != login_before;
            let signal = if log_has_since(&helper_log, helper_log_before, is_login_window_line) {
                Some(WindowSignal::Logged)
            } else if login_changed {
                Some(WindowSignal::LoginChanged)
            } else if stalled.is_none()
                && started.elapsed() >= grace
                && self.helper_up(key.as_deref())
            {
                Some(WindowSignal::Helper)
            } else {
                None
            };
            let signals = WindowSignals {
                updated: self.paths.manifest().is_file() && !self.paths.package_tmp().is_dir(),
                signal,
            };
            let (phase, detail) = signals.phase();
            if phase != last_phase {
                phase.clone_into(&mut last_phase);
                report("client", format!("{phase}{detail}"));
            }
            settled = if signals.updated { settled + 1 } else { 0 };
            if signals.ready() && settled >= SETTLED_LOOKS {
                break;
            }
            // A client that is gone will not come up; its own logs and the
            // session log say why, and a stall assertion names the prompt.
            gone = if self.client_running() { 0 } else { gone + 1 };
            if gone >= SETTLED_LOOKS {
                let bootstrap = (bootstrap_log.as_path(), bootstrap_before);
                return self.client_gone_or_shut_down(
                    session_log.is_some(),
                    stalled,
                    helper_missing,
                    bootstrap,
                );
            }
            if Instant::now() >= deadline {
                return Err(self.window_timed_out(report));
            }
            std::thread::sleep(
                self.core
                    .tuning
                    .tick
                    .max(CLIENT_POLL.min(self.core.tuning.tick * 4)),
            );
        }
        Ok(WindowWait::Window)
    }

    /// Snapshots the prefix, then starts the client so it updates itself.
    fn update(&mut self, report: &mut dyn FnMut(&str, String)) -> Result<(), CoreError> {
        self.stop(report)?;
        Core::settle(&self.paths.prefix_dir)?;
        report(
            "snapshot",
            format!("snapshot of {STEAM_CLIENT_PREFIX} before the update"),
        );
        let taken = snapshot::create(
            &self.paths.prefix_dir,
            &self.core.snapshots_root(),
            STEAM_CLIENT_PREFIX,
        )?;
        report("snapshot", SnapshotResult::from(taken).timestamp);
        let (_, prefix) = self.core.open_prefix(STEAM_CLIENT_PREFIX)?;
        self.first_run(&prefix, report)?;
        self.end_check()
    }

    /// Repairs a `broken` runtime and refuses any other state, since a
    /// client worth keeping (its sign-in, its `userdata/`) must go through
    /// `prefix restore`, which keeps a safety copy. A dead install's marker
    /// is cleared first; with the client intact after that, nothing is
    /// restored. A client whose record opens and which has `steam.exe`,
    /// with its own web helper in front of the shim, is fixed in place:
    /// stopped, the helper stashed and the shim put in front by the next
    /// start's pass, and started to its window; nothing is restored, and
    /// whatever else `bin/cef` holds the pass handles or its end check
    /// reports. A helper or stash path that does not read, in any
    /// `cef.win*` directory, is the restore's. Otherwise a `repair:` line
    /// names the break and the newest usable snapshot is restored: one
    /// whose prefix record opens and which holds `steam.exe`. Each newer
    /// snapshot is reported as skipped with its reason, and with no usable
    /// one the op refuses with `ClientRuntimeBroken`. The safety copy the
    /// restore takes of the state repaired away is discarded when that
    /// state had no `steam.exe` (nothing of a client to keep), and kept as
    /// a regular snapshot when it had one (a prefix record that does not
    /// open may sit over a sign-in): the report names it for `prefix
    /// restore`, and `repair` never picks it, since its record does not
    /// open. The op succeeds only when the runtime no longer reads
    /// `broken`.
    fn repair(&mut self, report: &mut dyn FnMut(&str, String)) -> Result<(), CoreError> {
        let mut why = match self.core.runtime_reading(true)? {
            RuntimeReading::Broken(reason) => reason.cause(),
            RuntimeReading::Other(state, detail) => return Err(nothing_to_repair(state, &detail)),
        };
        let had_steam_exe = self.paths.steam_exe().is_file();
        let marker = self.paths.install_marker();
        if had_steam_exe && marker.exists() {
            fs::remove_file(&marker).map_err(|error| CoreError::io(&marker, error))?;
            let RuntimeReading::Broken(reason) = self.core.runtime_reading(true)? else {
                report(
                    "repair",
                    "cleared the marker of an install that died; the client's files are intact"
                        .to_owned(),
                );
                return Ok(());
            };
            why = reason.cause();
            report(
                "repair",
                format!(
                    "cleared the marker of an install that died; the runtime is still broken: \
                     {why}"
                ),
            );
        }
        // `runtime_reading` reads the record, `steam.exe` and the marker before
        // any `cef.win*` (the marker is gone by here: cleared above, or never
        // there with `steam.exe`), so with the first two intact and every
        // helper and stash path reading, an exposed helper is the pass's,
        // whatever else `bin/cef` holds beside it (an orphan shim or a
        // directory with no helper the pass handles or its end check
        // reports); a record that does not open, a missing `steam.exe`, or
        // any helper or stash path that does not read, in any directory
        // (`HelperUnreadable`, whose detail promises the restore), is the
        // restore's.
        let intact = had_steam_exe && self.core.engine.open_prefix(&self.paths.prefix_dir).is_ok();
        if intact {
            match cef_helpers_read(&self.paths) {
                Err(error) => why = BrokenReason::HelperUnreadable(error.to_string()).cause(),
                Ok(()) => {
                    if let Some(exposed) = exposed_helper(&self.paths).ok().flatten() {
                        return self.reshim_in_place(&exposed, report);
                    }
                }
            }
        }
        let newest = self.usable_snapshot(report)?;
        report(
            "repair",
            format!("{why}; restoring the newest usable snapshot"),
        );
        self.stop(report)?;
        let had_prefix = self.paths.prefix_dir.is_dir();
        if had_prefix {
            Core::settle(&self.paths.prefix_dir)?;
        }
        report(
            "repair",
            format!(
                "restoring {} from {}",
                STEAM_CLIENT_PREFIX, newest.timestamp
            ),
        );
        let safety = snapshot::restore(
            &self.paths.prefix_dir,
            &self.core.snapshots_root(),
            STEAM_CLIENT_PREFIX,
            &newest.timestamp,
        )?;
        // With no prefix to copy, `restore` returns the source, which stays.
        if had_prefix && safety.timestamp != newest.timestamp {
            if had_steam_exe {
                report(
                    "repair",
                    format!(
                        "keeping the safety snapshot {0} of the state repaired away; `applejack \
                         prefix restore {STEAM_CLIENT_PREFIX} {0}` brings it back",
                        safety.timestamp
                    ),
                );
            } else {
                report(
                    "repair",
                    format!(
                        "discarding the safety snapshot {} of the state repaired away",
                        safety.timestamp
                    ),
                );
                fs::remove_dir_all(&safety.dir)
                    .map_err(|error| CoreError::io(&safety.dir, error))?;
            }
        }
        let _ = fs::remove_file(self.paths.install_marker());
        let (state, detail) = self.core.runtime_state(true)?;
        if state == ClientRuntimeState::Broken {
            return Err(BrokenReason::StillBroken {
                timestamp: newest.timestamp,
                detail,
            }
            .into_error());
        }
        Ok(())
    }

    /// `repair`'s fix for a client whose one break is `exposed`: the shim
    /// pass puts the shim in front and downloads nothing, where a snapshot
    /// restore would roll the client back for no reason. `Ok` only when the
    /// runtime no longer reads `broken`, as the restore path promises.
    fn reshim_in_place(
        &mut self,
        exposed: &ExposedHelper,
        report: &mut dyn FnMut(&str, String),
    ) -> Result<(), CoreError> {
        report(
            "repair",
            format!(
                "{}: the client's own web helper is in front ({}); nothing to restore: \
                 stopping the client if it runs, stashing the helper, putting the shim in \
                 front of it and starting the client to its window",
                exposed.dir.display(),
                exposed.shape()
            ),
        );
        self.stop(report)?;
        Core::settle(&self.paths.prefix_dir)?;
        let prefix = self.ensure_prefix(report)?;
        self.first_run(&prefix, report)?;
        self.end_check()
    }

    /// `Ok` only when the runtime no longer reads `broken`: an op whose
    /// start left it `broken` fails with the reason `finish` reported,
    /// rather than succeeding on a runtime `status` reads `broken`.
    fn end_check(&self) -> Result<(), CoreError> {
        let (state, detail) = self.core.runtime_state(true)?;
        if state == ClientRuntimeState::Broken {
            return Err(CoreError::ClientRuntimeBroken {
                state,
                detail: shown_detail(detail, true),
            });
        }
        Ok(())
    }

    /// The newest snapshot `repair` can restore, after reporting each newer
    /// one it skips; `ClientRuntimeBroken` when none qualifies.
    fn usable_snapshot(
        &self,
        report: &mut dyn FnMut(&str, String),
    ) -> Result<SnapshotResult, CoreError> {
        let snapshots = self.core.prefix_snapshots(STEAM_CLIENT_PREFIX)?;
        for snapshot in snapshots.iter().rev() {
            match self.unusable_snapshot_reason(&snapshot.dir) {
                None => return Ok(snapshot.clone()),
                Some(why) => report(
                    "repair",
                    format!("skipping snapshot {}: {why}", snapshot.timestamp),
                ),
            }
        }
        Err(if snapshots.is_empty() {
            BrokenReason::NoSnapshot
        } else {
            BrokenReason::NoUsableSnapshot(snapshots.len())
        }
        .into_error())
    }

    /// Why `repair` cannot restore the snapshot at `dir`, or `None` when it
    /// can: its prefix record opens and it holds `steam.exe`.
    fn unusable_snapshot_reason(&self, dir: &Path) -> Option<String> {
        if let Err(error) = self.core.engine.open_prefix(dir) {
            return Some(format!("its prefix record does not open: {error}"));
        }
        let paths = ClientPaths::new(dir);
        if !paths.steam_exe().is_file() {
            return Some("it has no steam.exe".to_owned());
        }
        match orphan_shim(&paths) {
            Ok(Some(cef)) => {
                return Some(format!(
                    "{} holds the web helper shim with no {WEBHELPER_REAL} beside it",
                    cef.display()
                ));
            }
            Ok(None) => {}
            Err(error) => return Some(format!("its web helper does not read: {error}")),
        }
        match missing_web_helper(&paths) {
            Ok(Some(cef)) => {
                return Some(format!("{} holds no web helper", cef.display()));
            }
            Ok(None) => {}
            Err(error) => return Some(format!("its web helper does not read: {error}")),
        }
        match exposed_helper(&paths) {
            Ok(Some(exposed)) => Some(format!(
                "{} holds the client's own web helper without the shim in front ({})",
                exposed.dir.display(),
                exposed.shape()
            )),
            Ok(None) => None,
            Err(error) => Some(format!("its web helper does not read: {error}")),
        }
    }
}

/// `repair`'s refusal of a runtime that is not `broken`: the way out
/// depends on the state, and `prefix restore` fits only a client that is
/// there to copy.
fn nothing_to_repair(state: ClientRuntimeState, detail: &str) -> CoreError {
    let detail = match state {
        ClientRuntimeState::Absent => format!(
            "nothing to repair ({detail}); `applejack runtime steam install` installs the client"
        ),
        ClientRuntimeState::Updating => format!("nothing to repair ({detail})"),
        _ => format!(
            "nothing to repair ({detail}); `applejack prefix restore {STEAM_CLIENT_PREFIX} \
             <timestamp>` restores a chosen snapshot and keeps a safety copy of the current state"
        ),
    };
    CoreError::ClientRuntimeBroken {
        state,
        detail: shown_detail(detail, true),
    }
}

/// The refusal of a run or an operation for a state `runtime_state` read,
/// which `status` therefore shows.
pub(crate) fn runtime_refused(state: ClientRuntimeState, detail: String) -> CoreError {
    CoreError::ClientRuntimeBroken {
        state,
        detail: shown_detail(detail, true),
    }
}

/// Records the client build id from its manifest in `meta`, after a
/// successful start; `None` when the manifest is not there to read.
pub(crate) fn record_client_build(
    core: &mut Core,
    paths: &ClientPaths,
) -> Result<Option<String>, CoreError> {
    let Some(build) = fs::read_to_string(paths.manifest())
        .ok()
        .and_then(|text| client_build_id(&text))
    else {
        return Ok(None);
    };
    core.db()?.meta_set(META_CLIENT_BUILD, &build)?;
    Ok(Some(build))
}

/// Backs the title's `.acf` up and lists its library for the client, before
/// a client launch. A [`LibraryEdit::FolderAdded`] under a running client
/// is the caller's cue to restart it.
///
/// # Errors
///
/// The read, parse, or write failure.
pub(crate) fn prepare_client_launch(
    paths: &ClientPaths,
    library: &Path,
    appid: u32,
    installdir: &str,
) -> Result<(RedownloadWatch, LibraryEdit), CoreError> {
    let watch = RedownloadWatch::start(library, appid, installdir)?;
    let windows_path = library_windows_path(&paths.prefix_dir, library)?;
    let edit = list_library(
        paths,
        &windows_path,
        Some((appid, watch.before.size_on_disk)),
    )?;
    Ok((watch, edit))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALVE_OK: &str = "Signature Index: 0  (Primary Signature)\n\nSigner's certificate:\n\t------------------\n\tSigner #0:\n\t\tSubject: CN=Valve Corp.,O=Valve Corp.,L=Bellevue,ST=Washington,C=US\n\t\tIssuer : CN=DigiCert Trusted G4 Code Signing RSA4096 SHA384 2021 CA1,O=DigiCert\\, Inc.,C=US\n\nSignature verification: ok\n\nNumber of verified signatures: 1\nSucceeded\n";

    const OTHER_OK: &str = "Signer's certificate:\n\tSigner #0:\n\t\tSubject: CN=Fixture Signer,O=Not Valve\n\nSignature verification: ok\n\nNumber of verified signatures: 1\nSucceeded\n";

    const SELF_SIGNED: &str = "Signer's certificate:\n\tSigner #0:\n\t\tSubject: CN=Fixture Signer,O=Not Valve\n\n\tError: self-signed certificate\n\nSignature verification: failed\n\nNumber of verified signatures: 1\nFailed\n";

    const UNSIGNED: &str = "No signature found\nUnable to extract existing signature\nFailed\n";

    const TAMPERED: &str = "Signature verification: failed\n\nCalculated message digest : B9A0     MISMATCH!!!\n\nNumber of verified signatures: 1\nFailed\n";

    #[test]
    fn only_a_verified_valve_signature_passes() {
        let valve = parse_signature_report(VALVE_OK, true);
        assert!(valve.verified);
        assert!(signer_is_valve(valve.subject.as_deref().unwrap()));
        let other = parse_signature_report(OTHER_OK, true);
        assert!(other.verified);
        assert!(!signer_is_valve(other.subject.as_deref().unwrap()));
        for (text, subject) in [
            (SELF_SIGNED, Some("CN=Fixture Signer,O=Not Valve")),
            (UNSIGNED, None),
            (TAMPERED, None),
        ] {
            let report = parse_signature_report(text, false);
            assert!(!report.verified, "{text}");
            assert_eq!(report.subject.as_deref(), subject);
        }
        // A tool that printed success but exited non-zero is not trusted.
        assert!(!parse_signature_report(VALVE_OK, false).verified);
        assert!(
            !signer_is_valve("CN=Valve Corp."),
            "the organisation decides, not the common name"
        );
        assert!(signer_is_valve("CN=X,O=Valve Corp.,C=US"));
        for impostor in [
            "O=Valve Corporation,C=US",
            "O=Valve Corp,C=US",
            "CN=Valve Corp. Impersonators,O=Valve Corp. Impersonators,C=US",
            "O=Valve Corporate Holdings LLC,C=US",
            "CN=Foo\\, O=Valve Corp.,O=Evil Inc,C=US",
            "O=Evil Inc\\,O=Valve Corp.",
        ] {
            assert!(!signer_is_valve(impostor), "{impostor}");
        }
        assert_eq!(
            split_rdns("CN=Foo\\, O=Valve Corp.,O=Evil Inc"),
            ["CN=Foo, O=Valve Corp.", "O=Evil Inc"]
        );
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "one entry per reason and shape")]
    fn every_broken_message_names_its_own_way_out_and_no_restore_without_a_snapshot() {
        let log = PathBuf::from("/x/log.txt");
        let reasons = [
            BrokenReason::DeadInstall,
            BrokenReason::RecordDoesNotOpen("`/x` is not a usable prefix: no record".to_owned()),
            BrokenReason::NoSteamExe(PathBuf::from("/x/Steam")),
            BrokenReason::BootstrapperTimedOut {
                secs: 600,
                log: log.clone(),
            },
            BrokenReason::BootstrapperExited("exit status: 1".to_owned(), log.clone()),
            BrokenReason::BootstrapperLeftNoExe(PathBuf::from("/x/Steam/steam.exe")),
            BrokenReason::ClientExitedEarly(log.clone()),
            BrokenReason::ClientStalledOnPrompt(log),
            BrokenReason::ClientStalledOnPrompt(PathBuf::from(
                "/x/Steam/dumps/assert_steam.exe_20260101000000_1.dmp",
            )),
            BrokenReason::NoShim(PathBuf::from("/e/probes/steamwebhelper-shim.exe")),
            BrokenReason::ShimWithoutReal(PathBuf::from("/x/Steam/bin/cef/cef.win64")),
            BrokenReason::NoWebHelper(PathBuf::from("/x/Steam/bin/cef/cef.win64")),
            BrokenReason::HelperUnshimmed {
                dir: PathBuf::from("/x/Steam/bin/cef/cef.win64"),
                stashed: false,
            },
            BrokenReason::HelperUnshimmed {
                dir: PathBuf::from("/x/Steam/bin/cef/cef.win7x64"),
                stashed: true,
            },
            BrokenReason::HelperNotLaidDown(PathBuf::from("/x/Steam/bin/cef/cef.win64")),
            BrokenReason::HelperUnreadable(
                "/x/Steam/bin/cef/cef.win64/steamwebhelper.exe: permission denied".to_owned(),
            ),
            BrokenReason::NoSnapshot,
            BrokenReason::NoUsableSnapshot(2),
            BrokenReason::StillBroken {
                timestamp: "20260101T000000Z".to_owned(),
                detail: "d".to_owned(),
            },
        ];
        for reason in reasons {
            let cause = reason.cause();
            assert!(
                reason.detail().starts_with(&format!("{cause}; ")),
                "{reason:?}"
            );
            if !matches!(reason, BrokenReason::StillBroken { .. }) {
                assert!(!cause.contains("`applejack"), "the cause alone: {cause}");
            }
            // What `runtime_state` reads from disk, listed here on its own so
            // the method cannot agree with itself. A timeout or exit that
            // left no helper is read afterwards as `NoWebHelper`.
            let shown = matches!(
                reason,
                BrokenReason::DeadInstall
                    | BrokenReason::RecordDoesNotOpen(_)
                    | BrokenReason::NoSteamExe(_)
                    | BrokenReason::ShimWithoutReal(_)
                    | BrokenReason::NoWebHelper(_)
                    | BrokenReason::HelperUnshimmed { .. }
                    | BrokenReason::HelperNotLaidDown(_)
                    | BrokenReason::HelperUnreadable(_)
                    | BrokenReason::NoSnapshot
                    | BrokenReason::StillBroken { .. }
            );
            assert_eq!(reason.shown_by_status(), shown, "{reason:?}");
            let snapshot_may_be_missing = !matches!(
                reason,
                BrokenReason::RecordDoesNotOpen(_)
                    | BrokenReason::NoSteamExe(_)
                    | BrokenReason::HelperUnreadable(_)
                    | BrokenReason::StillBroken { .. }
            );
            let payload_fault = matches!(reason, BrokenReason::NoShim(_));
            let dump = matches!(&reason, BrokenReason::ClientStalledOnPrompt(path) if path.extension().is_some_and(|ext| ext == "dmp"));
            let text = reason.into_error().to_string();
            // `status` reads the finding from disk, or the sentence is a lie.
            assert_eq!(text.contains(STATUS_SHOWS_IT), shown, "{text}");
            if payload_fault {
                assert!(text.contains("install a payload"), "{text}");
            } else {
                assert!(text.contains("`applejack "), "no command in {text}");
            }
            if snapshot_may_be_missing {
                assert!(!text.contains("restore"), "{text}");
            } else {
                assert!(text.contains("restore"), "{text}");
            }
            if dump {
                assert!(text.contains("the usual cause"), "{text}");
                assert!(!text.contains("was left unanswered"), "{text}");
            }
            let reinstall = text.contains("repair --reinstall");
            assert!(
                payload_fault
                    || reinstall
                    || text.contains("runtime steam install")
                    || text.contains("restores"),
                "{text}"
            );
        }
        let text = |state| {
            CoreError::ClientRuntimeBroken {
                state,
                detail: "d".to_owned(),
            }
            .to_string()
        };
        for state in [ClientRuntimeState::Updating, ClientRuntimeState::Installing] {
            let text = text(state);
            assert!(!text.contains("repair"), "{text}");
            assert!(text.contains("wait"), "{text}");
        }
        assert!(!text(ClientRuntimeState::Absent).contains("repair"));
        let refused = runtime_refused(ClientRuntimeState::Updating, "d".to_owned()).to_string();
        assert!(refused.contains(STATUS_SHOWS_IT), "{refused}");
        let refused = nothing_to_repair(ClientRuntimeState::NeedsLogin, "d").to_string();
        assert!(refused.contains(STATUS_SHOWS_IT), "{refused}");
    }

    #[test]
    fn a_helper_less_directory_names_install_and_reinstall_as_the_ways_out() {
        let dir = PathBuf::from("/x/Steam/bin/cef/cef.win64");
        let text = BrokenReason::NoWebHelper(dir.clone()).detail();
        assert!(text.contains("`applejack runtime steam install`"), "{text}");
        assert!(
            text.contains("`applejack runtime steam repair --reinstall`"),
            "{text}"
        );
        let text = BrokenReason::HelperNotLaidDown(dir).detail();
        assert!(text.contains("did not lay one down"), "{text}");
        assert!(
            !text.contains("the orphan shim was removed"),
            "the cause is not known here: {text}"
        );
        assert!(
            text.contains("`applejack runtime steam repair --reinstall`"),
            "{text}"
        );
        // An early exit may not have run the file check to its end, so
        // `install` comes first.
        assert!(
            text.contains("run `applejack runtime steam install` again"),
            "{text}"
        );
    }

    /// A client tree for the helper reads: `Steam/package/` with the
    /// manifest and the `cef.win*` directories given, each with the files
    /// named (`REAL` text, so none is a shim).
    fn cef_tree(root: &Path, dirs: &[(&str, &[&str])]) -> ClientPaths {
        let paths = ClientPaths::new(root);
        fs::create_dir_all(paths.manifest().parent().unwrap()).unwrap();
        fs::write(paths.manifest(), "m").unwrap();
        for (dir, files) in dirs {
            let dir = paths.cef_dir().join(dir);
            fs::create_dir_all(&dir).unwrap();
            for file in *files {
                fs::write(dir.join(file), "REAL").unwrap();
            }
        }
        paths
    }

    #[test]
    fn an_empty_cef_directory_beside_a_populated_one_is_skipped_and_alone_is_a_missing_helper() {
        let dir = tempfile::tempdir().unwrap();
        let paths = cef_tree(
            dir.path(),
            &[
                ("cef.win10x64", &[]),
                ("cef.win7x64", &[WEBHELPER_EXE, WEBHELPER_REAL]),
            ],
        );
        assert_eq!(missing_web_helper(&paths).unwrap(), None);
        let (kept, skipped) = populated_cef_dirs(&paths).unwrap();
        assert_eq!(kept, vec![paths.cef_dir().join("cef.win7x64")]);
        assert_eq!(skipped, vec![paths.cef_dir().join("cef.win10x64")]);

        let dir = tempfile::tempdir().unwrap();
        let paths = cef_tree(dir.path(), &[("cef.win10x64", &[]), ("cef.win7x64", &[])]);
        assert_eq!(
            missing_web_helper(&paths).unwrap(),
            Some(paths.cef_dir().join("cef.win10x64"))
        );
        let (kept, skipped) = populated_cef_dirs(&paths).unwrap();
        assert_eq!(kept.len(), 2);
        assert_eq!(skipped, Vec::<PathBuf>::new());

        let dir = tempfile::tempdir().unwrap();
        let paths = cef_tree(
            dir.path(),
            &[
                ("cef.win10x64", &["other.dll"]),
                ("cef.win7x64", &[WEBHELPER_EXE]),
            ],
        );
        assert_eq!(
            missing_web_helper(&paths).unwrap(),
            Some(paths.cef_dir().join("cef.win10x64"))
        );
    }

    #[test]
    fn a_helper_that_is_not_a_regular_file_or_does_not_open_is_a_read_error() {
        let dir = tempfile::tempdir().unwrap();
        let paths = cef_tree(dir.path(), &[("cef.win7x64", &[WEBHELPER_REAL])]);
        let helper = paths.cef_dir().join("cef.win7x64").join(WEBHELPER_EXE);
        fs::create_dir(&helper).unwrap();
        let error = shim_file(&helper, None).unwrap_err().to_string();
        assert!(error.contains("is not a regular file"), "{error}");
        assert!(orphan_shim(&paths).is_err());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::remove_dir(&helper).unwrap();
            let big = fs::File::create(&helper).unwrap();
            big.set_len(SHIM_MAX_LEN + 1).unwrap();
            drop(big);
            fs::set_permissions(&helper, fs::Permissions::from_mode(0o000)).unwrap();
            // Root opens a mode-000 file; the read error is only there when
            // the open fails.
            if fs::File::open(&helper).is_err() {
                let error = shim_file(&helper, None).unwrap_err().to_string();
                assert!(error.contains("ermission denied"), "{error}");
            }
            fs::set_permissions(&helper, fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(shim_file(&helper, None).unwrap(), Some(false));
        }
    }

    #[test]
    fn a_log_shorter_than_its_recorded_offset_is_read_from_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("webhelper.txt");
        fs::write(&log, "old line\nold line\n").unwrap();
        let offset = log_offset(&log);
        fs::write(&log, "x\n").unwrap();
        assert!(log_has_since(&log, offset, |line| line == "x"));
        fs::write(&log, "old line\nold line\nnew\n").unwrap();
        assert!(log_has_since(&log, offset, |line| line == "new"));
        assert!(!log_has_since(&log, offset, |line| line == "old line"));
    }

    #[test]
    fn the_login_state_comes_from_a_remembered_user() {
        let remembered = "\"users\"\n{\n\t\"76561190000000000\"\n\t{\n\t\t\"AccountName\"\t\t\"someone\"\n\t\t\"RememberPassword\"\t\t\"1\"\n\t\t\"MostRecent\"\t\t\"1\"\n\t\t\"AllowAutoLogin\"\t\t\"1\"\n\t}\n}\n";
        let forgotten = remembered.replace(
            "\"RememberPassword\"\t\t\"1\"",
            "\"RememberPassword\"\t\t\"0\"",
        );
        let no_auto =
            remembered.replace("\"AllowAutoLogin\"\t\t\"1\"", "\"AllowAutoLogin\"\t\t\"0\"");
        assert_eq!(remembered_login(remembered), Some(true));
        assert_eq!(remembered_login(&forgotten), Some(false));
        assert_eq!(remembered_login(&no_auto), Some(false));
        assert_eq!(remembered_login("\"users\"\n{\n}\n"), Some(false));
        assert_eq!(remembered_login("not vdf {"), None);
    }

    #[test]
    fn the_build_id_is_the_manifests_version() {
        let text = "\"steam_client_win32\"\n{\n\t\"version\"\t\t\"1761720601\"\n\t\"bins_win32\"\n\t{\n\t\t\"file\"\t\t\"bins_win32.zip.abc\"\n\t}\n}\n";
        assert_eq!(client_build_id(text).as_deref(), Some("1761720601"));
        assert_eq!(client_build_id(""), None);
    }

    const FOLDERS: &str = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"C:\\\\Program Files (x86)\\\\Steam\"\n\t\t\"label\"\t\t\"\"\n\t\t\"contentid\"\t\t\"1\"\n\t\t\"totalsize\"\t\t\"0\"\n\t\t\"apps\"\n\t\t{\n\t\t\t\"228980\"\t\t\"100\"\n\t\t}\n\t}\n}\n";

    #[test]
    fn a_library_is_added_once_and_the_rest_of_the_file_round_trips() {
        let mut document = Document::parse(FOLDERS).unwrap();
        assert_eq!(
            add_library_folder(&mut document, "S:\\", 242_920, 4096),
            LibraryEdit::FolderAdded
        );
        let text = document.to_text();
        assert!(text.contains("\"path\"\t\t\"S:\\\\\""), "{text}");
        assert!(text.contains("\"242920\"\t\t\"4096\""), "{text}");
        assert!(
            text.contains("\"228980\"\t\t\"100\""),
            "the client's own folder is kept"
        );
        assert!(
            text.starts_with(&FOLDERS[..FOLDERS.len() - 2]),
            "the existing text is unchanged:\n{text}"
        );
        let mut again = Document::parse(&text).unwrap();
        assert_eq!(
            add_library_folder(&mut again, "s:\\", 242_920, 4096),
            LibraryEdit::Unchanged,
            "same path, same app, same size"
        );
        assert_eq!(
            add_library_folder(&mut again, "S:", 242_921, 1),
            LibraryEdit::AppListed,
            "a second title joins the folder without a restart"
        );
        assert_eq!(
            ensure_library_folder(&mut again, "S:\\"),
            LibraryEdit::Unchanged
        );
        assert_eq!(again.get_str("libraryfolders/1/apps/242921"), Some("1"));
        assert_eq!(again.get_str("libraryfolders/1/path"), Some("S:\\"));
        assert_eq!(again.get_map("libraryfolders").unwrap().len(), 2);
    }

    #[test]
    fn an_empty_file_gets_the_root_and_the_folder() {
        let mut document = Document::default();
        assert_eq!(
            add_library_folder(&mut document, "Z:\\Users\\me\\library", 10, 1),
            LibraryEdit::FolderAdded
        );
        let parsed = Document::parse(&document.to_text()).unwrap();
        assert_eq!(
            parsed.get_str("libraryfolders/0/path"),
            Some("Z:\\Users\\me\\library")
        );
        assert_eq!(parsed.get_str("libraryfolders/0/apps/10"), Some("1"));
    }

    #[test]
    fn a_folder_listed_before_the_first_start_follows_the_clients_own() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ClientPaths::new(dir.path());
        assert_eq!(
            list_library(&paths, "S:\\", None).unwrap(),
            LibraryEdit::FolderAdded
        );
        let text = fs::read_to_string(paths.libraryfolders()).unwrap();
        let document = Document::parse(&text).unwrap();
        assert_eq!(
            document.get_str("libraryfolders/0/path"),
            Some(CLIENT_LIBRARY)
        );
        assert_eq!(document.get_str("libraryfolders/1/path"), Some("S:\\"));
        assert!(
            !paths
                .libraryfolders()
                .with_extension("vdf.applejack")
                .exists(),
            "written through a sibling and a rename"
        );
        assert_eq!(
            list_library(&paths, "S:\\", Some((7, 9))).unwrap(),
            LibraryEdit::AppListed
        );
        assert_eq!(
            list_library(&paths, "Z:\\other", Some((8, 9))).unwrap(),
            LibraryEdit::FolderAdded
        );
    }

    fn acf(buildid: u64, state_flags: u32, target: Option<u64>) -> String {
        let target = target.map_or(String::new(), |target| {
            format!("\t\"TargetBuildID\"\t\t\"{target}\"\n")
        });
        format!(
            "\"AppState\"\n{{\n\t\"appid\"\t\t\"7\"\n\t\"Universe\"\t\t\"1\"\n\t\"name\"\t\t\"T\"\n\t\"StateFlags\"\t\t\"{state_flags}\"\n\t\"installdir\"\t\t\"T\"\n\t\"buildid\"\t\t\"{buildid}\"\n{target}\t\"LastUpdated\"\t\t\"0\"\n\t\"SizeOnDisk\"\t\t\"1\"\n}}\n"
        )
    }

    #[test]
    fn a_build_moving_forward_is_a_patch_and_anything_else_under_update_bits_a_refutation() {
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path();
        let paths = LibraryPaths::new(library);
        fs::create_dir_all(paths.install_dir("T")).unwrap();
        fs::write(paths.acf(7), acf(100, app_state::FULLY_INSTALLED, None)).unwrap();
        let mut watch = RedownloadWatch::start(library, 7, "T").unwrap();
        assert_eq!(watch.redownload(), None);
        let update = Some(Redownload::Update { from: 100, to: 101 });
        for (text, want) in [
            (acf(101, 0x0402, None), update.clone()),
            (acf(100, 0x0402, Some(101)), update.clone()),
            (acf(101, app_state::FULLY_INSTALLED, None), update),
            (
                acf(100, 0x0402, None),
                Some(Redownload::Refuted("StateFlags became 1026".to_owned())),
            ),
            (
                acf(100, 0x0402, Some(0)),
                Some(Redownload::Refuted("StateFlags became 1026".to_owned())),
            ),
            (
                acf(99, 0x0402, None),
                Some(Redownload::Refuted(
                    "buildid changed from 100 to 99".to_owned(),
                )),
            ),
            (
                acf(
                    100,
                    app_state::FULLY_INSTALLED | app_state::VALIDATING,
                    None,
                ),
                None,
            ),
        ] {
            fs::write(paths.acf(7), &text).unwrap();
            assert_eq!(watch.redownload(), want, "{text}");
        }
    }

    #[test]
    fn the_update_bits_are_the_client_fetching() {
        assert_eq!(app_state::FULLY_INSTALLED & UPDATE_BITS, 0);
        assert_eq!(
            (app_state::FULLY_INSTALLED | app_state::APP_RUNNING) & UPDATE_BITS,
            0
        );
        assert_eq!(
            (app_state::FULLY_INSTALLED | app_state::VALIDATING) & UPDATE_BITS,
            0,
            "a verification pass is A2 holding"
        );
        assert_ne!(
            0x0402 & UPDATE_BITS,
            0,
            "update required and update started"
        );
        assert_ne!(
            (app_state::FULLY_INSTALLED | app_state::DOWNLOADING) & UPDATE_BITS,
            0
        );
    }
}
