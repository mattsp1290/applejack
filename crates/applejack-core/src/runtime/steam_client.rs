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
//! | `broken` | the prefix record does not open, a `Steam/` directory without `steam.exe`, or an install marker nobody holds the lock for |
//! | `updating` | `Steam/package/steam_client_win64.manifest` (or the older `win32` one) is missing, or `Steam/package/tmp/` exists while the client runs |
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
use applejack_engine::rosetta_wine::process::{find_in_prefix, find_session, wineserver_running};
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

impl ClientRuntimeState {
    /// The tail of the `ClientRuntimeBroken` message: what gets the runtime
    /// out of this state. `repair` is right for `broken` only; it refuses
    /// any other state, since a restore would roll back a client worth
    /// keeping, and on a `broken` client whose files are intact it only
    /// clears the marker of a dead install.
    #[must_use]
    pub const fn advice(self) -> &'static str {
        match self {
            Self::Installing => "; wait for the install to finish",
            Self::Updating => {
                "; wait for the client's self-update to finish, or run `applejack runtime steam \
                 install` if the client was stopped"
            }
            Self::Broken => {
                ", and `applejack runtime steam repair` restores the newest snapshot, or only \
                 clears the marker of a dead install when the client's files are intact"
            }
            Self::Absent | Self::NeedsLogin | Self::Ready => "",
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
            let installing =
                !lock_held && PrefixLock::is_held(&self.snapshots_root(), STEAM_CLIENT_PREFIX)?;
            return Ok(if installing {
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
        match self.op {
            ClientOp::Install => self.install(fetch, false, report)?,
            ClientOp::Repair { reinstall: true } => self.install(fetch, true, report)?,
            ClientOp::Repair { reinstall: false } => self.repair(report)?,
            ClientOp::Update => self.update(report)?,
            ClientOp::Stop => self.stop(report)?,
        }
        self.core.runtime_status(true)
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
        // The loader is the client and outlives this call; `Session`'s drop
        // is one non-blocking `try_wait`, so nothing here waits on it.
        drop(session);
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
        let mut gone = 0;
        let mut last_phase = String::new();
        loop {
            let manifest = self.paths.manifest().is_file();
            let tmp = self.paths.package_tmp().is_dir();
            // By the key when this core started the client, else by the
            // prefix: a client `prefix exec` or an earlier core started.
            let is_helper = |process: &applejack_engine::rosetta_wine::Process| {
                process.exe.eq_ignore_ascii_case("steamwebhelper.exe")
            };
            let helper = key
                .as_deref()
                .is_some_and(|key| find_session(key).iter().any(is_helper))
                || find_in_prefix(&self.paths.prefix_dir).iter().any(is_helper);
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
            // A client that is gone will not come up; its own
            // `logs/bootstrap_log.txt` and the session log say why.
            gone = if self.client_running() { 0 } else { gone + 1 };
            if gone >= SETTLED_LOOKS {
                return Err(CoreError::ClientRuntimeBroken {
                    state: ClientRuntimeState::Broken,
                    detail: format!(
                        "the client exited before its window came up; see {} and the session log",
                        self.paths
                            .steam_dir()
                            .join("logs/bootstrap_log.txt")
                            .display()
                    ),
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
        if let Some(build) = record_client_build(&mut self.core, &self.paths)? {
            report("build", format!("client build {build}"));
        }
        let (state, detail) = self.core.runtime_state(true)?;
        report(state.as_str(), detail);
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

    /// Repairs a `broken` runtime and refuses any other state: the restore
    /// below discards what it replaces, so a client worth keeping (its
    /// sign-in, its `userdata/`) must go through `prefix restore`, which
    /// keeps a safety copy. A dead install's marker over an intact client
    /// is cleared without restoring anything.
    fn repair(&mut self, report: &mut dyn FnMut(&str, String)) -> Result<(), CoreError> {
        let (state, detail) = self.core.runtime_state(true)?;
        if state != ClientRuntimeState::Broken {
            return Err(CoreError::ClientRuntimeBroken {
                state,
                detail: format!(
                    "nothing to repair ({detail}); `applejack prefix restore {STEAM_CLIENT_PREFIX} \
                     <timestamp>` restores a chosen snapshot and keeps a safety copy of the \
                     current state"
                ),
            });
        }
        let had_steam_exe = self.paths.steam_exe().is_file();
        let marker = self.paths.install_marker();
        if had_steam_exe && marker.exists() {
            fs::remove_file(&marker).map_err(|error| CoreError::io(marker, error))?;
            report(
                "repair",
                "cleared the marker of an install that died; the client's files are intact"
                    .to_owned(),
            );
            return Ok(());
        }
        let Some(newest) = self.core.prefix_snapshots(STEAM_CLIENT_PREFIX)?.pop() else {
            return Err(CoreError::ClientRuntimeBroken {
                state: ClientRuntimeState::Broken,
                detail: format!(
                    "no snapshot of {STEAM_CLIENT_PREFIX} exists; `applejack runtime steam repair --reinstall` reruns the bootstrapper"
                ),
            });
        };
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
        // The safety copy is the state just repaired away; kept, it would be
        // the newest snapshot and the next `repair` would put it back. With
        // no prefix to copy, `restore` returns the source, which stays. A
        // state that still had `steam.exe` (a prefix record that does not
        // open) may hold a sign-in, so its copy stays too.
        if had_prefix && !had_steam_exe && safety.timestamp != newest.timestamp {
            report(
                "repair",
                format!(
                    "discarding the safety snapshot {} of the state repaired away",
                    safety.timestamp
                ),
            );
            fs::remove_dir_all(&safety.dir).map_err(|error| CoreError::io(&safety.dir, error))?;
        }
        let _ = fs::remove_file(self.paths.install_marker());
        Ok(())
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
    fn the_broken_advice_names_repair_for_a_broken_runtime_only() {
        let text = |state| {
            CoreError::ClientRuntimeBroken {
                state,
                detail: "d".to_owned(),
            }
            .to_string()
        };
        assert!(text(ClientRuntimeState::Broken).contains("runtime steam repair"));
        for state in [ClientRuntimeState::Updating, ClientRuntimeState::Installing] {
            let text = text(state);
            assert!(!text.contains("repair"), "{text}");
            assert!(text.contains("wait"), "{text}");
        }
        assert!(!text(ClientRuntimeState::Absent).contains("repair"));
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
