//! The Windows Steam client in the shared `steam-client` prefix: install
//! from Valve's bootstrapper, its first run, login-state detection, update,
//! snapshot, repair, and stop (plan 05).
//!
//! The runtime is in one [`ClientRuntimeState`], read from disk each time:
//!
//! | State | Read from |
//! |---|---|
//! | `absent` | no `drive_c/Program Files (x86)/Steam/steam.exe` in the prefix |
//! | `installing` | `applejack/client-install.json` in the prefix while the prefix lock is held |
//! | `broken` | the prefix record does not open, a `Steam/` directory without `steam.exe`, or an install marker nobody holds the lock for |
//! | `updating` | `Steam/package/steam_client_win32.manifest` is missing, or `Steam/package/tmp/` exists while the client runs |
//! | `needs_login` | `Steam/config/loginusers.vdf` names no remembered account |
//! | `ready` | it names one |
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
use applejack_engine::rosetta_wine::process::{find_session, wineserver_running};
use applejack_engine::rosetta_wine::snapshot::{self, PrefixLock};
use applejack_engine::rosetta_wine::utc_now;
use applejack_engine::{Engine, GraphicsBackend, HidrawMode, InstalledEngine, LaunchSpec, Prefix};
use applejack_steam::acf::app_state;
use applejack_steam::{AppManifest, Document, LibraryPaths, Node, Value};
use serde::{Deserialize, Serialize};

use crate::error::CoreError;
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

/// The client manifest the build id comes from.
pub const CLIENT_MANIFEST: &str = "package/steam_client_win32.manifest";

/// The host-arch Authenticode tool inside the engine payload.
pub const OSSLSIGNCODE: &str = "probes/osslsigncode";

/// The trust store the signature chain is checked against: macOS's own.
pub const CA_FILE: &str = "/etc/ssl/cert.pem";

/// How long the bootstrapper and the first run may each take.
pub const FIRST_RUN_TIMEOUT: Duration = Duration::from_secs(600);

/// Growth under a title's install directory that counts as a re-download.
pub const REDOWNLOAD_GROWTH: u64 = 64 << 20;

const INSTALL_MARKER: &str = "applejack/client-install.json";

const BOOTSTRAPPER_FILE: &str = "applejack/SteamSetup.exe";

const STEAM_EXE: &str = "steam.exe";

const LOGINUSERS: &str = "config/loginusers.vdf";

const LIBRARYFOLDERS: &str = "config/libraryfolders.vdf";

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

    #[must_use]
    pub fn manifest(&self) -> PathBuf {
        self.steam_dir().join(CLIENT_MANIFEST)
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

/// The client build id from `steam_client_win32.manifest`.
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

/// Whether a subject's organisation is Valve Corp.
#[must_use]
pub fn signer_is_valve(subject: &str) -> bool {
    subject.split(',').map(str::trim).any(|rdn| {
        rdn.strip_prefix("O=")
            .is_some_and(|org| org.trim().starts_with("Valve Corp"))
    })
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
            reason: "its signer is not Valve Corp".to_owned(),
        });
    }
    Ok(signer)
}

/// Adds `windows_path` to a `libraryfolders.vdf` document as the client
/// writes one, or finds it there, and lists `appid` under it with its size.
/// Returns whether the text changed. Everything else in the file is kept.
pub fn add_library_folder(
    document: &mut Document,
    windows_path: &str,
    appid: u32,
    size_on_disk: u64,
) -> bool {
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
    let Value::Map(folders) = &mut root.value else {
        return false;
    };
    let same = |path: &str| {
        path.trim_end_matches('\\')
            .eq_ignore_ascii_case(windows_path.trim_end_matches('\\'))
    };
    let size = size_on_disk.to_string();
    let key = appid.to_string();
    let found = folders
        .iter()
        .position(|folder| folder.value.get_str("path").is_some_and(same));
    let Some(found) = found else {
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
                Node::map("apps", vec![Node::str(key, size)]),
            ],
        ));
        return true;
    };
    let Value::Map(fields) = &mut folders[found].value else {
        return false;
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
        return false;
    };
    match entries.iter_mut().find(|entry| entry.key == key) {
        Some(entry) if entry.value == Value::Str(size.clone()) => false,
        Some(entry) => {
            entry.value = Value::Str(size);
            true
        }
        None => {
            entries.push(Node::str(key, size));
            true
        }
    }
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

/// Watches a title for the client re-downloading it, from the `.acf` and
/// the install directory. Plan 00's assumption A2 says the client adopts
/// the install with a verification pass only; this is where it is refuted.
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
    pub fn redownload(&mut self) -> Option<String> {
        if let Ok(text) = fs::read_to_string(&self.acf)
            && let Ok(now) = AppManifest::parse(&text)
        {
            if now.buildid != self.before.buildid {
                return Some(format!(
                    "buildid changed from {} to {}",
                    self.before.buildid, now.buildid
                ));
            }
            if now.state_flags & UPDATE_BITS != 0 {
                return Some(format!("StateFlags became {}", now.state_flags));
            }
        }
        if self
            .scanned_at
            .is_none_or(|at| at.elapsed() >= SIZE_SCAN_EVERY)
        {
            self.scanned_at = Some(Instant::now());
            let size = dir_size(&self.install_dir);
            if size.saturating_sub(self.baseline) >= REDOWNLOAD_GROWTH {
                return Some(format!(
                    "the install directory grew by {} bytes",
                    size - self.baseline
                ));
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

    /// The runtime's state and why, from disk.
    ///
    /// # Errors
    ///
    /// The prefix path failure.
    pub fn client_runtime_state(&self) -> Result<(ClientRuntimeState, String), CoreError> {
        let paths = self.client_paths()?;
        let dir = &paths.prefix_dir;
        if !dir.is_dir() {
            return Ok((
                ClientRuntimeState::Absent,
                "no steam-client prefix".to_owned(),
            ));
        }
        let marker = paths.install_marker();
        if marker.exists() {
            let held = matches!(self.lock_prefix(STEAM_CLIENT_PREFIX), Err(ref e) if crate::runtime::session::busy(e));
            return Ok(if held {
                (
                    ClientRuntimeState::Installing,
                    "an install is running".to_owned(),
                )
            } else {
                (
                    ClientRuntimeState::Broken,
                    "an install stopped before it finished; run `applejack runtime steam install` again"
                        .to_owned(),
                )
            });
        }
        if let Err(error) = self.engine.open_prefix(dir) {
            return Ok((ClientRuntimeState::Broken, error.to_string()));
        }
        if !paths.steam_exe().is_file() {
            return Ok(if paths.steam_dir().is_dir() {
                (
                    ClientRuntimeState::Broken,
                    format!("{} has no steam.exe", paths.steam_dir().display()),
                )
            } else {
                (
                    ClientRuntimeState::Absent,
                    "the client is not installed in the prefix".to_owned(),
                )
            });
        }
        if !paths.manifest().is_file() {
            return Ok((
                ClientRuntimeState::Updating,
                "the client has not finished its first self-update".to_owned(),
            ));
        }
        if paths.package_tmp().is_dir() && wineserver_running(dir).unwrap_or(true) {
            return Ok((
                ClientRuntimeState::Updating,
                "the client is updating itself".to_owned(),
            ));
        }
        let login = fs::read_to_string(paths.loginusers())
            .ok()
            .and_then(|text| remembered_login(&text));
        Ok(match login {
            Some(true) => (ClientRuntimeState::Ready, "an account is remembered".to_owned()),
            _ => (
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
        let (state, detail) = self.client_runtime_state()?;
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
        Ok(self.client_session_key()?.is_some_and(|key| {
            find_session(&key)
                .iter()
                .any(|process| process.exe.eq_ignore_ascii_case(STEAM_EXE))
        }))
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
    /// [`CoreError::ClientRuntimeBroken`] when the client never comes up, or
    /// the engine failure.
    pub fn run_with(
        mut self,
        fetch: Fetcher<'_>,
        report: &mut dyn FnMut(&str, String),
    ) -> Result<ClientRuntimeStatus, CoreError> {
        match self.op {
            ClientOp::Install => self.install(fetch, false, report)?,
            ClientOp::Repair { reinstall: true } => self.install(fetch, true, report)?,
            ClientOp::Repair { reinstall: false } => self.repair(report)?,
            ClientOp::Update => self.update(report)?,
            ClientOp::Stop => self.stop(report)?,
        }
        self.core.client_runtime_status()
    }

    fn log_path(&self) -> PathBuf {
        self.core.log_dir().join("steam-client").join("runtime.log")
    }

    fn client_running(&mut self) -> bool {
        self.core.client_is_up().unwrap_or(true)
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
        report("prefix", format!("creating prefix {STEAM_CLIENT_PREFIX}"));
        let library = self.core.library_dir()?;
        self.core.build_prefix(
            STEAM_CLIENT_PREFIX,
            &self.installed,
            GraphicsBackend::Dxmt,
            None,
            HidrawMode::Disabled,
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
        if self.client_running() && !reinstall {
            // The window the user signs in to is up; a second steam.exe
            // would only hand off to it.
            return self.wait_ready(None, report);
        }
        if reinstall {
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
        self.first_run(&prefix, report)
    }

    fn bootstrap(
        &mut self,
        fetch: Fetcher<'_>,
        prefix: &Prefix,
        report: &mut dyn FnMut(&str, String),
    ) -> Result<(), CoreError> {
        let tool = self.installed.root.join(OSSLSIGNCODE);
        if !tool.is_file() {
            return Err(CoreError::ClientRuntimeBroken {
                state: ClientRuntimeState::Absent,
                detail: format!(
                    "the engine payload ships no `{OSSLSIGNCODE}`, so the bootstrapper cannot be verified"
                ),
            });
        }
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
        let deadline = Instant::now() + FIRST_RUN_TIMEOUT;
        let status = loop {
            if let Some(status) = session.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = session.cancel();
                return Err(CoreError::ClientRuntimeBroken {
                    state: ClientRuntimeState::Broken,
                    detail: format!(
                        "SteamSetup.exe ran past {} seconds; log {}",
                        FIRST_RUN_TIMEOUT.as_secs(),
                        session.log_path.display()
                    ),
                });
            }
            std::thread::sleep(CLIENT_POLL);
        };
        if !status.success() {
            return Err(CoreError::ClientRuntimeBroken {
                state: ClientRuntimeState::Broken,
                detail: format!(
                    "SteamSetup.exe exited {status}; log {}",
                    session.log_path.display()
                ),
            });
        }
        Core::settle(&prefix.dir)?;
        if !self.paths.steam_exe().is_file() {
            return Err(CoreError::ClientRuntimeBroken {
                state: ClientRuntimeState::Broken,
                detail: format!(
                    "the bootstrapper left no {}",
                    self.paths.steam_exe().display()
                ),
            });
        }
        Ok(())
    }

    /// Starts `steam.exe` and waits for its self-update and the login
    /// window. The client stays up for the user.
    fn first_run(
        &mut self,
        prefix: &Prefix,
        report: &mut dyn FnMut(&str, String),
    ) -> Result<(), CoreError> {
        if self.client_running() {
            return self.wait_ready(None, report);
        }
        report("start", "starting steam.exe for its self-update".to_owned());
        let key = new_session_id("steam-client");
        let spec = LaunchSpec {
            session_id: Some(key.clone()),
            exe: self.paths.steam_exe(),
            args: Vec::new(),
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
        // The loader is the client; it is not reaped here, since the client
        // outlives this call. The kernel reaps it with this process.
        std::mem::forget(session);
        self.wait_ready(Some(&key), report)
    }

    /// Waits for the self-update to finish: the manifest exists, nothing is
    /// left under `package/tmp`, and the web helper that draws the window is
    /// up. A `loginusers.vdf` write counts too.
    fn wait_ready(
        &mut self,
        key: Option<&str>,
        report: &mut dyn FnMut(&str, String),
    ) -> Result<(), CoreError> {
        let key = match key {
            Some(key) => Some(key.to_owned()),
            None => self.core.client_session_key()?,
        };
        let login_before = fs::metadata(self.paths.loginusers())
            .and_then(|meta| meta.modified())
            .ok();
        let deadline = Instant::now() + FIRST_RUN_TIMEOUT;
        let mut settled = 0;
        let mut last_phase = String::new();
        loop {
            let manifest = self.paths.manifest().is_file();
            let tmp = self.paths.package_tmp().is_dir();
            let helper = key.as_deref().is_some_and(|key| {
                find_session(key)
                    .iter()
                    .any(|process| process.exe.eq_ignore_ascii_case("steamwebhelper.exe"))
            });
            let login_changed = fs::metadata(self.paths.loginusers())
                .and_then(|meta| meta.modified())
                .ok()
                != login_before;
            let phase = if !manifest || tmp {
                "updating"
            } else if helper || login_changed {
                "window"
            } else {
                "starting"
            };
            if phase != last_phase {
                phase.clone_into(&mut last_phase);
                report("client", phase.to_owned());
            }
            settled = if manifest && !tmp { settled + 1 } else { 0 };
            if manifest && !tmp && (helper || login_changed) && settled >= SETTLED_LOOKS {
                break;
            }
            if !self.client_running() && settled >= SETTLED_LOOKS {
                return Err(CoreError::ClientRuntimeBroken {
                    state: ClientRuntimeState::Broken,
                    detail: "the client exited before its window came up; see the session log"
                        .to_owned(),
                });
            }
            if Instant::now() >= deadline {
                let _ = self.stop(report);
                return Err(CoreError::ClientRuntimeBroken {
                    state: ClientRuntimeState::Updating,
                    detail: format!(
                        "the client did not reach the login window within {} seconds; it was stopped and its files kept",
                        FIRST_RUN_TIMEOUT.as_secs()
                    ),
                });
            }
            std::thread::sleep(
                self.core
                    .tuning
                    .tick
                    .max(CLIENT_POLL.min(self.core.tuning.tick * 4)),
            );
        }
        self.record_build(report)?;
        let (state, detail) = self.core.client_runtime_state()?;
        report(state.as_str(), detail);
        Ok(())
    }

    fn record_build(&mut self, report: &mut dyn FnMut(&str, String)) -> Result<(), CoreError> {
        let Some(build) = fs::read_to_string(self.paths.manifest())
            .ok()
            .and_then(|text| client_build_id(&text))
        else {
            return Ok(());
        };
        self.core.db()?.meta_set(META_CLIENT_BUILD, &build)?;
        report("build", format!("client build {build}"));
        Ok(())
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
        self.first_run(&prefix, report)
    }

    /// Restores the newest snapshot.
    fn repair(&mut self, report: &mut dyn FnMut(&str, String)) -> Result<(), CoreError> {
        let Some(newest) = self.core.prefix_snapshots(STEAM_CLIENT_PREFIX)?.pop() else {
            return Err(CoreError::ClientRuntimeBroken {
                state: ClientRuntimeState::Broken,
                detail: format!(
                    "no snapshot of {STEAM_CLIENT_PREFIX} exists; `applejack runtime steam repair --reinstall` reruns the bootstrapper"
                ),
            });
        };
        self.stop(report)?;
        if self.paths.prefix_dir.is_dir() {
            Core::settle(&self.paths.prefix_dir)?;
        }
        report(
            "repair",
            format!(
                "restoring {} from {}",
                STEAM_CLIENT_PREFIX, newest.timestamp
            ),
        );
        snapshot::restore(
            &self.paths.prefix_dir,
            &self.core.snapshots_root(),
            STEAM_CLIENT_PREFIX,
            &newest.timestamp,
        )?;
        let _ = fs::remove_file(self.paths.install_marker());
        Ok(())
    }
}

/// Backs the title's `.acf` up and lists its library for the client, before
/// a client launch.
///
/// # Errors
///
/// The read, parse, or write failure.
pub(crate) fn prepare_client_launch(
    paths: &ClientPaths,
    library: &Path,
    appid: u32,
    installdir: &str,
) -> Result<RedownloadWatch, CoreError> {
    let watch = RedownloadWatch::start(library, appid, installdir)?;
    let windows_path = library_windows_path(&paths.prefix_dir, library)?;
    let file = paths.libraryfolders();
    let mut document = match fs::read_to_string(&file) {
        Ok(text) => Document::parse(&text)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Document::default(),
        Err(error) => return Err(CoreError::io(&file, error)),
    };
    let size = watch.before.size_on_disk;
    if add_library_folder(&mut document, &windows_path, appid, size) {
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent).map_err(|error| CoreError::io(parent, error))?;
        }
        fs::write(&file, document.to_text()).map_err(|error| CoreError::io(&file, error))?;
    }
    Ok(watch)
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
        assert!(signer_is_valve("O=Valve Corporation,C=US"));
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
        assert!(add_library_folder(&mut document, "S:\\", 242_920, 4096));
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
        assert!(
            !add_library_folder(&mut again, "s:\\", 242_920, 4096),
            "same path, same app, same size"
        );
        assert!(
            add_library_folder(&mut again, "S:", 242_921, 1),
            "a second title joins the folder"
        );
        assert_eq!(again.get_str("libraryfolders/1/apps/242921"), Some("1"));
        assert_eq!(again.get_str("libraryfolders/1/path"), Some("S:\\"));
        assert_eq!(again.get_map("libraryfolders").unwrap().len(), 2);
    }

    #[test]
    fn an_empty_file_gets_the_root_and_the_folder() {
        let mut document = Document::default();
        assert!(add_library_folder(
            &mut document,
            "Z:\\Users\\me\\library",
            10,
            1
        ));
        let parsed = Document::parse(&document.to_text()).unwrap();
        assert_eq!(
            parsed.get_str("libraryfolders/0/path"),
            Some("Z:\\Users\\me\\library")
        );
        assert_eq!(parsed.get_str("libraryfolders/0/apps/10"), Some("1"));
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
