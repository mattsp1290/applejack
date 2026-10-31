//! Windows depot install, update, and verify.
//!
//! `steamroom-client`'s `DepotJob` does the file work: it verifies every
//! chunk against its Adler-32 and SHA-1 ids, assembles each file in a staging
//! directory, and renames it into place only after a whole-file SHA-1. This
//! module decides what `DepotJob` sees, by handing it a manifest trimmed to
//! the files that still need writing, and owns everything around it: locks,
//! the ownership rule, the resume record, stored manifests, stale-file
//! removal, and the `.acf`.
//!
//! Layout under the library root:
//!
//! ```text
//! steamapps/common/<installdir>/                 the game
//! steamapps/appmanifest_<appid>.acf              written last
//! applejack/<appid>.download.json                exists only mid-download
//! applejack/manifests/<depot>_<manifest>.json    decrypted file lists
//! ```

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::error::Error as StdError;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use steamroom::depot::manifest::{DepotManifest, ManifestFile};
use steamroom::depot::{ChunkId, DepotId, DepotKey};
use steamroom::enums::DepotFileFlags;
use steamroom::error::Error as SteamError;
use steamroom_client::download::{
    BoxError, ChunkFetcher, DepotJob, DownloadError, DownloadReport, OldChunkLoc, RetryConfig,
};
use steamroom_client::event::DownloadEvent;
use tokio::sync::{mpsc, watch};

use crate::Error;
use crate::acf::{AcfWriter, AppManifest, InstalledDepot, app_state};
use crate::auth::AuthError;
use crate::library::{DepotFilter, OwnedApp, PlannedDepot};
use crate::lock::{self, HeldLocks, LockDir};
use crate::session::SteamSession;

/// `DepotJob`'s working directory inside an install; removed on success.
const STAGING_DIR: &str = ".DepotDownloader";

const RECORD_VERSION: u32 = 1;

/// Completed files between two saves of the resume record.
const RECORD_SAVE_EVERY: usize = 256;

/// How often a running download re-reads the ownership predicate.
const CLIENT_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// HTTP 429 and 503 without a `Retry-After`.
const RATE_LIMIT_RETRY_AFTER: Duration = Duration::from_secs(60);

const TOO_MANY_REQUESTS: u16 = 429;
const SERVICE_UNAVAILABLE: u16 = 503;

const HASH_BUFFER: usize = 1 << 20;

/// Where one library keeps things.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryPaths {
    root: PathBuf,
}

impl LibraryPaths {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `steamapps/common/<installdir>`
    #[must_use]
    pub fn install_dir(&self, installdir: &str) -> PathBuf {
        self.root.join("steamapps/common").join(installdir)
    }

    /// `steamapps/appmanifest_<appid>.acf`
    #[must_use]
    pub fn acf(&self, appid: u32) -> PathBuf {
        self.root
            .join("steamapps")
            .join(format!("appmanifest_{appid}.acf"))
    }

    /// `applejack/<appid>.download.json`
    #[must_use]
    pub fn record(&self, appid: u32) -> PathBuf {
        self.root
            .join("applejack")
            .join(format!("{appid}.download.json"))
    }

    /// `applejack/manifests/<depot>_<manifest>.json`
    #[must_use]
    pub fn manifest(&self, depot_id: u32, manifest_id: u64) -> PathBuf {
        self.root
            .join("applejack/manifests")
            .join(format!("{depot_id}_{manifest_id}.json"))
    }
}

/// What an install would fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    pub appid: u32,
    pub name: String,
    pub installdir: String,
    pub branch: String,
    pub buildid: u64,
    pub depots: Vec<PlannedDepot>,
    /// Bytes across `depots`.
    pub size: u64,
    pub filter: DepotFilter,
}

impl InstallPlan {
    /// Chooses the depots for one branch. Build the plan from product info
    /// read just before installing: Steam keeps serving old manifests, so a
    /// stale list installs a stale build rather than failing.
    ///
    /// # Errors
    ///
    /// [`Error::NoWindowsDepot`], [`Error::NoManifestForBranch`], or
    /// [`Error::UnsafePath`] when `installdir` is not one plain directory
    /// name.
    pub fn new(app: &OwnedApp, branch: &str, filter: &DepotFilter) -> Result<Self, Error> {
        let depots = app.windows_depots(branch, filter)?;
        check_installdir(&app.installdir)?;
        let buildid = branch_build(app, branch)?;
        Ok(Self {
            appid: app.appid,
            name: app.name.clone(),
            installdir: app.installdir.clone(),
            branch: branch.to_owned(),
            buildid,
            size: depots.iter().map(|depot| depot.size).sum(),
            depots,
            filter: filter.clone(),
        })
    }
}

/// A branch without a build id cannot be installed: the `.acf` needs one, and
/// the client reads anything else as an update pending.
fn branch_build(app: &OwnedApp, branch: &str) -> Result<u64, Error> {
    app.builds
        .get(branch)
        .copied()
        .ok_or_else(|| Error::NoManifestForBranch {
            appid: app.appid,
            branch: branch.to_owned(),
        })
}

fn check_installdir(installdir: &str) -> Result<(), Error> {
    let mut components = Path::new(installdir).components();
    if !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
        || installdir.contains('\\')
    {
        return Err(Error::UnsafePath {
            path: installdir.to_owned(),
        });
    }
    Ok(())
}

/// One progress report. Files already verified count towards `completed_files`
/// and not towards `completed_bytes`, so bytes may finish short of the total
/// on a resumed run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallProgress {
    pub appid: u32,
    pub depot_id: u32,
    pub completed_files: u64,
    pub total_files: u64,
    pub completed_bytes: u64,
    pub total_bytes: u64,
}

/// What a finished run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOutcome {
    pub appid: u32,
    /// Depots whose manifest this run installed.
    pub changed_depots: Vec<u32>,
    pub files_written: u64,
    pub files_removed: u64,
    /// A matching resume record was picked up.
    pub resumed: bool,
}

/// Tuning for the download itself.
#[derive(Debug, Clone)]
pub struct InstallOptions {
    pub max_downloads: usize,
    pub retry: RetryConfig,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            max_downloads: 16,
            retry: RetryConfig::default(),
        }
    }
}

/// True while a Steam client runs in a prefix that maps this library. The
/// caller supplies `wineserver_alive` here, because it lives in
/// `applejack-engine` and this crate does not depend on it.
pub type ClientRunning<'a> = &'a (dyn Fn() -> bool + Send + Sync);

/// Receives [`InstallProgress`].
pub type ProgressSink<'a> = &'a (dyn Fn(&InstallProgress) + Send + Sync);

/// `library/applejack/<appid>.download.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadRecord {
    pub version: u32,
    pub appid: u32,
    pub branch: String,
    /// The run found another target's partial state, so every file of the
    /// manifest is checked, not only those that differ from the `.acf`.
    #[serde(default)]
    pub rescan: bool,
    pub depots: Vec<RecordDepot>,
}

/// One depot of a [`DownloadRecord`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordDepot {
    pub depot_id: u32,
    pub manifest_id: u64,
    pub previous_manifest_id: Option<u64>,
    /// Every file is in place.
    pub complete: bool,
    /// Manifest file names already in place.
    pub files: BTreeSet<String>,
    /// Targets of earlier stopped runs, whose files may still be on disk.
    #[serde(default)]
    pub abandoned: BTreeSet<u64>,
}

impl DownloadRecord {
    fn load(path: &Path) -> Option<Self> {
        let record: Self = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
        (record.version == RECORD_VERSION).then_some(record)
    }

    fn save(&self, path: &Path, appid: u32) -> Result<(), Error> {
        let text = serde_json::to_vec_pretty(self).map_err(|error| Error::Io {
            path: path.to_owned(),
            source: io::Error::other(error),
        })?;
        write_atomic(path, &text).map_err(|source| io_error(appid, path, source))
    }

    fn matches(&self, appid: u32, branch: &str, work: &[WorkDepot]) -> bool {
        self.appid == appid
            && self.branch == branch
            && self.depots.len() == work.len()
            && self.depots.iter().zip(work).all(|(recorded, depot)| {
                recorded.depot_id == depot.depot_id
                    && recorded.manifest_id == depot.manifest_id
                    && recorded.previous_manifest_id == depot.previous_id
            })
    }

    /// Forgets a completed file that no longer has its recorded size, so a
    /// file deleted between runs is fetched again.
    fn prune(&mut self, install_dir: &Path, work: &[WorkDepot]) {
        for (recorded, depot) in self.depots.iter_mut().zip(work) {
            let sizes: HashMap<&str, u64> = depot
                .manifest
                .files
                .iter()
                .map(|file| (file.filename.as_str(), file.size))
                .collect();
            let before = recorded.files.len();
            recorded.files.retain(|name| {
                let path = install_dir.join(name.replace('\\', "/"));
                sizes.get(name.as_str()).is_some_and(|size| {
                    fs::symlink_metadata(path).is_ok_and(|meta| meta.len() == *size)
                })
            });
            if recorded.files.len() != before {
                recorded.complete = false;
            }
        }
    }
}

/// A record left by a run that stopped. The `.acf` is exactly what that run
/// did not move, so a title with one is not known to be good whatever the
/// `.acf` says. `record` is `None` when the file cannot be read, and then
/// every depot counts as touched.
struct Unfinished {
    record: Option<DownloadRecord>,
}

impl Unfinished {
    fn read(path: &Path, appid: u32) -> Option<Self> {
        path.exists().then(|| Self {
            record: DownloadRecord::load(path)
                .filter(|record| record.appid == appid && !record.depots.is_empty()),
        })
    }

    fn touched(&self, depot_id: u32) -> bool {
        self.record
            .as_ref()
            .is_none_or(|record| record.depots.iter().any(|depot| depot.depot_id == depot_id))
    }

    /// Every earlier target of a depot other than the two this run deals in.
    fn abandoned(&self, depot_id: u32, target: u64, previous: Option<u64>) -> BTreeSet<u64> {
        self.record
            .iter()
            .flat_map(|record| &record.depots)
            .filter(|depot| depot.depot_id == depot_id)
            .flat_map(|depot| depot.abandoned.iter().chain([&depot.manifest_id]))
            .copied()
            .filter(|id| *id != target && Some(*id) != previous)
            .collect()
    }

    /// Recorded depots the plan does not select.
    fn unplanned<'u>(&'u self, planned: &'u [PlannedDepot]) -> impl Iterator<Item = u32> + use<'u> {
        self.record
            .iter()
            .flat_map(|record| &record.depots)
            .map(|depot| depot.depot_id)
            .filter(|id| planned.iter().all(|depot| depot.depot_id != *id))
    }
}

/// One depot this run has to bring up to date. `previous_id` equals
/// `manifest_id` for a depot back at its `.acf` manifest after a stopped run.
struct WorkDepot {
    depot_id: u32,
    manifest_id: u64,
    manifest: DepotManifest,
    previous_id: Option<u64>,
    previous: Option<DepotManifest>,
    abandoned_ids: BTreeSet<u64>,
    /// The stored manifests of `abandoned_ids`.
    abandoned: Vec<DepotManifest>,
}

impl WorkDepot {
    fn size(&self) -> u64 {
        self.manifest.files.iter().map(|file| file.size).sum()
    }

    fn old_layouts(&self) -> Option<HashMap<String, Vec<OldChunkLoc>>> {
        let previous = self.previous.as_ref()?;
        Some(
            previous
                .files
                .iter()
                .map(|file| {
                    let chunks = file
                        .chunks
                        .iter()
                        .zip(chunk_offsets(file))
                        .map(|(chunk, offset)| OldChunkLoc {
                            id: chunk.id.clone(),
                            offset,
                            size: chunk.uncompressed_size,
                        })
                        .collect();
                    (file.normalized_path(), chunks)
                })
                .collect(),
        )
    }
}

/// An installed depot this run has no reason to write.
struct KeptDepot {
    depot_id: u32,
    manifest_id: u64,
    manifest: DepotManifest,
}

impl From<KeptDepot> for WorkDepot {
    /// A kept depot that came to own a path, with nothing of its own changed.
    fn from(depot: KeptDepot) -> Self {
        Self {
            depot_id: depot.depot_id,
            manifest_id: depot.manifest_id,
            previous_id: Some(depot.manifest_id),
            previous: Some(depot.manifest.clone()),
            manifest: depot.manifest,
            abandoned_ids: BTreeSet::new(),
            abandoned: Vec::new(),
        }
    }
}

/// Which depot's entry applies to each path, keyed as [`path_owners`] keys
/// them: `current` after this run and `before` as the `.acf` left it.
struct Layers<'m> {
    current: HashMap<String, (u32, &'m ManifestFile)>,
    before: HashMap<String, (u32, &'m ManifestFile)>,
}

impl<'m> Layers<'m> {
    fn new(work: &'m [WorkDepot], kept: &'m [KeptDepot]) -> Self {
        let kept_layers = || kept.iter().map(|depot| (depot.depot_id, &depot.manifest));
        Self {
            current: path_owners(
                work.iter()
                    .map(|depot| (depot.depot_id, &depot.manifest))
                    .chain(kept_layers()),
            ),
            before: path_owners(
                work.iter()
                    .filter_map(|depot| Some((depot.depot_id, depot.previous.as_ref()?)))
                    .chain(kept_layers()),
            ),
        }
    }

    fn owns(&self, depot_id: u32, file: &ManifestFile) -> bool {
        self.current
            .get(&path_key(file))
            .is_some_and(|(owner, _)| *owner == depot_id)
    }

    /// The name matters as well as the content: on a case-sensitive volume a
    /// respelled path is a new file.
    fn changed(&self, file: &ManifestFile) -> bool {
        self.before.get(&path_key(file)).is_none_or(|(_, before)| {
            before.normalized_path() != file.normalized_path() || !same_content(before, file)
        })
    }

    /// The entries of `depot` this run has to write.
    fn to_write(
        &self,
        depot: &'m WorkDepot,
        rescan: bool,
        install_dir: &Path,
    ) -> impl Iterator<Item = &'m ManifestFile> {
        depot
            .manifest
            .files
            .iter()
            .filter(|file| self.owns(depot.depot_id, file))
            .filter(move |file| {
                let changed = self.changed(file);
                (rescan || changed) && !keeps_user_copy(file, install_dir, changed)
            })
    }
}

/// `path_owners`' key. Depots are built on Windows, where these are one path.
fn path_key(file: &ManifestFile) -> String {
    file.normalized_path().to_lowercase()
}

/// The one rule for overlapping depots: a path is on disk once, and the depot
/// with the highest id wins.
fn path_owners<'m>(
    layers: impl Iterator<Item = (u32, &'m DepotManifest)>,
) -> HashMap<String, (u32, &'m ManifestFile)> {
    let mut layers: Vec<_> = layers.collect();
    layers.sort_by_key(|(depot_id, _)| *depot_id);
    let mut owners = HashMap::new();
    for (depot_id, manifest) in layers {
        for file in &manifest.files {
            owners.insert(path_key(file), (depot_id, file));
        }
    }
    owners
}

/// `userconfig` and `versionedconfig` in a depot build script: the game or
/// the user edits the file, so an update does not replace it and validation
/// does not flag it.
fn is_user_config(file: &ManifestFile) -> bool {
    let flags = DepotFileFlags::from_bits_retain(file.flags);
    flags.intersects(DepotFileFlags::USER_CONFIG | DepotFileFlags::VERSIONED_USER_CONFIG)
}

/// A `versionedconfig` file is still replaced when the depot's copy changes.
fn keeps_user_copy(file: &ManifestFile, install_dir: &Path, changed: bool) -> bool {
    let flags = DepotFileFlags::from_bits_retain(file.flags);
    (flags.contains(DepotFileFlags::USER_CONFIG) || (is_user_config(file) && !changed))
        && fs::symlink_metadata(install_dir.join(file.normalized_path())).is_ok()
}

/// Where each chunk sits: its recorded offset, or for a manifest that omits
/// them, after the chunk before it, which is `DepotJob`'s rule.
fn chunk_offsets(file: &ManifestFile) -> Vec<u64> {
    let mut next = 0;
    file.chunks
        .iter()
        .map(|chunk| {
            let offset = chunk.offset.unwrap_or(next);
            next = offset + u64::from(chunk.uncompressed_size);
            offset
        })
        .collect()
}

fn same_content(before: &ManifestFile, after: &ManifestFile) -> bool {
    before.size == after.size
        && before.flags == after.flags
        && before.link_target == after.link_target
        && before.sha_content == after.sha_content
        && before.chunks.len() == after.chunks.len()
        && before
            .chunks
            .iter()
            .zip(&after.chunks)
            .all(|(a, b)| a.id == b.id && a.offset == b.offset)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RunKind {
    Install,
    Update,
}

/// Installs and updates titles in one library over a logged-in session.
pub struct DepotInstaller<'a, S> {
    session: &'a S,
    paths: LibraryPaths,
    data_dir: PathBuf,
    client_running: ClientRunning<'a>,
    options: InstallOptions,
    progress: Option<ProgressSink<'a>>,
}

impl<S> std::fmt::Debug for DepotInstaller<'_, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DepotInstaller")
            .field("paths", &self.paths)
            .field("data_dir", &self.data_dir)
            .finish_non_exhaustive()
    }
}

impl<'a, S: SteamSession> DepotInstaller<'a, S> {
    /// `data_dir` holds `locks/`; `client_running` is the ownership rule's
    /// predicate, consulted before the first write, while downloading, and
    /// before the `.acf` is written.
    pub fn new(
        session: &'a S,
        paths: LibraryPaths,
        data_dir: impl Into<PathBuf>,
        client_running: ClientRunning<'a>,
    ) -> Self {
        Self {
            session,
            paths,
            data_dir: data_dir.into(),
            client_running,
            options: InstallOptions::default(),
            progress: None,
        }
    }

    #[must_use]
    pub fn with_options(mut self, options: InstallOptions) -> Self {
        self.options = options;
        self
    }

    #[must_use]
    pub fn with_progress(mut self, progress: ProgressSink<'a>) -> Self {
        self.progress = Some(progress);
        self
    }

    /// Installs a title, resuming a matching partial run.
    ///
    /// # Errors
    ///
    /// [`Error::LockHeld`] at once when another process holds a lock.
    /// [`Error::ClientRunning`] while the ownership predicate is true.
    /// [`Error::AlreadyInstalled`] for a fully installed title and
    /// [`Error::ClientOwned`] for an `.acf` in any other state.
    /// [`Error::ManifestChanged`] when a manifest cannot be fetched after one
    /// re-read of the product info. [`Error::LinkOutOfInstall`] before any
    /// write, when a path runs through a symlink that leaves the install.
    /// [`Error::CorruptChunk`],
    /// [`Error::DiskFull`], and [`Error::Auth`] (including a CDN rate limit)
    /// leave the partial state in place for the next run.
    pub async fn install(&self, plan: &InstallPlan) -> Result<InstallOutcome, Error> {
        self.run(plan, RunKind::Install).await
    }

    /// `--update`: re-reads the `.acf`, refuses unless it reports fully
    /// installed, and writes only what differs from the manifests it records.
    /// The Steam client may have updated the title, so the `.acf` is the
    /// source of the installed manifest ids and the caller re-syncs its
    /// `apps` row from it.
    ///
    /// # Errors
    ///
    /// [`Error::NotInstalled`] without an `.acf`, [`Error::ClientOwned`]
    /// when it reports anything but fully installed,
    /// [`Error::UnfinishedDepot`] when a stopped run touched a depot this plan
    /// does not select, and the errors of [`install`](Self::install).
    pub async fn update(&self, plan: &InstallPlan) -> Result<InstallOutcome, Error> {
        self.run(plan, RunKind::Update).await
    }

    async fn run(&self, plan: &InstallPlan, kind: RunKind) -> Result<InstallOutcome, Error> {
        let appid = plan.appid;
        let mut locks = LockDir::new(&self.data_dir, "install");
        let held = locks
            .app(appid, lock::Mode::NonBlocking)?
            .with_library(lock::Mode::NonBlocking)?;
        self.refuse_while_client_runs(appid)?;

        let existing = read_acf(&self.paths, appid)?;
        match (&existing, kind) {
            (Some((manifest, _)), _) if !manifest.is_fully_installed() => {
                return Err(Error::ClientOwned {
                    appid,
                    state_flags: manifest.state_flags,
                });
            }
            (Some(_), RunKind::Install) => return Err(Error::AlreadyInstalled { appid }),
            (None, RunKind::Update) => return Err(Error::NotInstalled { appid }),
            _ => {}
        }

        let mut run = Run {
            installer: self,
            plan: plan.clone(),
            keys: HashMap::new(),
            refreshed: false,
            unfinished: Unfinished::read(&self.paths.record(appid), appid),
            locks: Arc::new(held),
        };
        // The files are where the `.acf` says, whatever the product info
        // says now.
        if let Some((manifest, _)) = &existing {
            check_installdir(&manifest.installdir)?;
            run.plan.installdir.clone_from(&manifest.installdir);
        }
        let installed: HashMap<u32, u64> = existing
            .iter()
            .flat_map(|(manifest, _)| &manifest.installed_depots)
            .map(|depot| (depot.depot_id, depot.manifest_id))
            .collect();
        let work = run.resolve(&installed).await?;
        // Without an `.acf` every planned file is checked and nothing claims
        // the title is whole, so only an installed title can be left mixed.
        // A depot the `.acf` lists went back to its manifest in `resolve`.
        if let Some(depot_id) = run
            .unfinished
            .as_ref()
            .filter(|_| existing.is_some())
            .and_then(|unfinished| {
                unfinished
                    .unplanned(&run.plan.depots)
                    .find(|id| !installed.contains_key(id))
            })
        {
            return Err(Error::UnfinishedDepot { appid, depot_id });
        }
        if work.is_empty() {
            return Ok(InstallOutcome {
                appid,
                changed_depots: Vec::new(),
                files_written: 0,
                files_removed: 0,
                resumed: false,
            });
        }
        let kept = run.kept_depots(existing.as_ref(), &work).await?;
        let outcome = run.download(work, kept, existing).await?;
        Ok(outcome)
    }

    fn refuse_while_client_runs(&self, appid: u32) -> Result<(), Error> {
        if (self.client_running)() {
            return Err(Error::ClientRunning { appid });
        }
        Ok(())
    }
}

/// One install or update, under its locks.
struct Run<'r, 'a, S> {
    installer: &'r DepotInstaller<'a, S>,
    /// The caller's plan, with `manifest_id` and `buildid` replaced if the
    /// product info is re-read.
    plan: InstallPlan,
    keys: HashMap<u32, DepotKey>,
    refreshed: bool,
    unfinished: Option<Unfinished>,
    locks: Arc<HeldLocks>,
}

impl<S: SteamSession> Run<'_, '_, S> {
    fn paths(&self) -> &LibraryPaths {
        &self.installer.paths
    }

    async fn key(&mut self, depot_id: u32) -> Result<DepotKey, Error> {
        if let Some(key) = self.keys.get(&depot_id) {
            return Ok(key.clone());
        }
        let key = self
            .installer
            .session
            .depot_key(self.plan.appid, depot_id)
            .await?;
        self.keys.insert(depot_id, key.clone());
        Ok(key)
    }

    /// The stored manifest, or a fetched one that is decrypted, checked, and
    /// stored. `None` when Steam no longer serves this id.
    async fn manifest(
        &mut self,
        depot_id: u32,
        manifest_id: u64,
    ) -> Result<Option<DepotManifest>, Error> {
        if let Some(stored) = load_manifest(&self.paths().manifest(depot_id, manifest_id)) {
            return Ok(Some(stored));
        }
        let fetched = self
            .installer
            .session
            .manifest(self.plan.appid, depot_id, manifest_id, &self.plan.branch)
            .await?;
        let Some(mut manifest) = fetched else {
            return Ok(None);
        };
        if manifest.manifest_id.is_some_and(|id| id.0 != manifest_id) {
            return Ok(None);
        }
        let key = self.key(depot_id).await?;
        manifest
            .decrypt_filenames(&key)
            .map_err(|error| Error::Depot {
                depot_id,
                message: format!("manifest {manifest_id}: {error}"),
            })?;
        check_paths(&manifest)?;
        let path = self.paths().manifest(depot_id, manifest_id);
        let text = serde_json::to_vec(&manifest).map_err(|error| Error::Depot {
            depot_id,
            message: format!("manifest {manifest_id}: {error}"),
        })?;
        write_atomic(&path, &text).map_err(|source| io_error(self.plan.appid, &path, source))?;
        Ok(Some(manifest))
    }

    /// Re-reads the product info and moves the whole plan to it, so every
    /// depot and the build id stay on one build. Happens once per run.
    async fn refresh(&mut self) -> Result<(), Error> {
        let appid = self.plan.appid;
        let infos = self.installer.session.app_info(&[appid]).await?;
        let app = infos
            .iter()
            .filter(|info| info.appid == appid)
            .find_map(|info| OwnedApp::parse(info, &self.plan.filter))
            .ok_or(Error::AppNotOwned { appid })?;
        self.plan.depots = app.windows_depots(&self.plan.branch, &self.plan.filter)?;
        self.plan.buildid = branch_build(&app, &self.plan.branch)?;
        self.refreshed = true;
        Ok(())
    }

    /// Fetches the target manifest of every depot that is out of date.
    async fn resolve(&mut self, installed: &HashMap<u32, u64>) -> Result<Vec<WorkDepot>, Error> {
        let listed: HashMap<u32, u64> = self
            .plan
            .depots
            .iter()
            .map(|depot| (depot.depot_id, depot.manifest_id))
            .collect();
        loop {
            match self.resolve_pass(installed).await? {
                Ok(work) => return Ok(work),
                Err(depot) if self.refreshed => {
                    let appid = self.plan.appid;
                    let depot_id = depot.depot_id;
                    let current = depot.manifest_id;
                    let listed = listed.get(&depot_id).copied().unwrap_or(current);
                    return Err(if listed == current {
                        Error::ManifestNotServed {
                            appid,
                            depot_id,
                            manifest_id: current,
                        }
                    } else {
                        Error::ManifestChanged {
                            appid,
                            depot_id,
                            listed,
                            current,
                        }
                    });
                }
                Err(_) => self.refresh().await?,
            }
        }
    }

    /// One pass over the plan. The inner `Err` is the depot whose manifest
    /// Steam no longer serves.
    async fn resolve_pass(
        &mut self,
        installed: &HashMap<u32, u64>,
    ) -> Result<Result<Vec<WorkDepot>, PlannedDepot>, Error> {
        let mut work = Vec::new();
        // A stopped run's depot the plan dropped stays in the `.acf`, so it
        // goes back to the manifest the `.acf` claims.
        let carried: Vec<PlannedDepot> = self
            .unfinished
            .iter()
            .flat_map(|unfinished| unfinished.unplanned(&self.plan.depots))
            .filter_map(|depot_id| {
                Some(PlannedDepot {
                    depot_id,
                    manifest_id: *installed.get(&depot_id)?,
                    size: 0,
                })
            })
            .collect();
        for planned in self
            .plan
            .depots
            .iter()
            .chain(&carried)
            .copied()
            .collect::<Vec<_>>()
        {
            let depot_id = planned.depot_id;
            let manifest_id = planned.manifest_id;
            let previous_id = installed.get(&depot_id).copied();
            let forced = self
                .unfinished
                .as_ref()
                .is_some_and(|unfinished| unfinished.touched(depot_id));
            if previous_id == Some(manifest_id) && !forced {
                continue;
            }
            let Some(manifest) = self.manifest(depot_id, manifest_id).await? else {
                return Ok(Err(planned));
            };
            let previous = match previous_id {
                Some(id) => self.manifest(depot_id, id).await?,
                None => None,
            };
            let abandoned_ids = self
                .unfinished
                .as_ref()
                .map(|unfinished| unfinished.abandoned(depot_id, manifest_id, previous_id))
                .unwrap_or_default();
            let abandoned = abandoned_ids
                .iter()
                .filter_map(|id| load_manifest(&self.paths().manifest(depot_id, *id)))
                .collect();
            work.push(WorkDepot {
                depot_id,
                manifest_id,
                manifest,
                previous_id,
                previous,
                abandoned_ids,
                abandoned,
            });
        }
        Ok(Ok(work))
    }

    /// The record to run under: a matching one to resume, or a new one.
    fn open_record(&self, work: &[WorkDepot], install_dir: &Path) -> (DownloadRecord, bool) {
        let appid = self.plan.appid;
        let record_path = self.paths().record(appid);
        let interrupted = record_path.exists();
        let loaded = DownloadRecord::load(&record_path)
            .filter(|record| record.matches(appid, &self.plan.branch, work));
        let resumed = loaded.is_some();
        // A run for other manifests stopped here and may have left files of
        // its own target, including ones the `.acf` says are current. The
        // whole manifest goes through `DepotJob`, which hashes each file and
        // rewrites only what differs. The record carries this, so stopping
        // the rescan does not lose it.
        let mut record = loaded.unwrap_or_else(|| DownloadRecord {
            version: RECORD_VERSION,
            appid,
            branch: self.plan.branch.clone(),
            rescan: interrupted,
            depots: work
                .iter()
                .map(|depot| RecordDepot {
                    depot_id: depot.depot_id,
                    manifest_id: depot.manifest_id,
                    previous_manifest_id: depot.previous_id,
                    complete: false,
                    files: BTreeSet::new(),
                    abandoned: depot.abandoned_ids.clone(),
                })
                .collect(),
        });
        record.prune(install_dir, work);
        (record, resumed)
    }

    /// The depots the `.acf` lists that are not in `work`, with whatever
    /// manifests are to be had: a depot the plan does not select is not
    /// fetched, since the account may no longer reach it.
    async fn kept_depots(
        &mut self,
        existing: Option<&(AppManifest, String)>,
        work: &[WorkDepot],
    ) -> Result<Kept, Error> {
        let mut kept = Kept {
            depots: Vec::new(),
            complete: true,
        };
        for depot in existing.iter().flat_map(|(acf, _)| &acf.installed_depots) {
            let (depot_id, manifest_id) = (depot.depot_id, depot.manifest_id);
            if work.iter().any(|target| target.depot_id == depot_id) {
                continue;
            }
            let planned = self.plan.depots.iter().any(|d| d.depot_id == depot_id);
            let manifest = if planned {
                self.manifest(depot_id, manifest_id).await?
            } else {
                load_manifest(&self.paths().manifest(depot_id, manifest_id))
            };
            match manifest {
                Some(manifest) => kept.depots.push(KeptDepot {
                    depot_id,
                    manifest_id,
                    manifest,
                }),
                None => kept.complete = false,
            }
        }
        Ok(kept)
    }

    async fn download(
        &mut self,
        mut work: Vec<WorkDepot>,
        kept: Kept,
        existing: Option<(AppManifest, String)>,
    ) -> Result<InstallOutcome, Error> {
        let appid = self.plan.appid;
        let record_path = self.paths().record(appid);
        let install_dir = self.paths().install_dir(&self.plan.installdir);
        // Resolving can take a while on large manifests.
        self.installer.refuse_while_client_runs(appid)?;

        let Kept {
            depots: kept,
            complete: every_depot_known,
        } = kept;
        let kept = promote_heirs(&mut work, kept);
        work.sort_by_key(|depot| depot.depot_id);
        let layers = Layers::new(&work, &kept);
        // A symlink here on a title with no `.acf` may be anything. On an
        // installed one it is how a game is moved to another disk.
        if existing.is_none()
            && let Ok(target) = fs::read_link(&install_dir)
        {
            return Err(Error::LinkOutOfInstall {
                link: install_dir,
                target,
            });
        }

        let (mut record, resumed) = self.open_record(&work, &install_dir);
        let mut tally = Tally::default();
        let queued = queue_files(&record, &work, &layers, &install_dir, &mut tally);

        // Checked before the record exists on disk: a refused run wrote
        // nothing, and a record would hold the title back from launching.
        refuse_links_out(&install_dir, &queued)?;
        record.save(&record_path, appid)?;
        fs::create_dir_all(&install_dir).map_err(|source| io_error(appid, &install_dir, source))?;

        // Without every manifest, nothing can be proved stale.
        let current = current_entries(&work, &kept);
        let stale = if every_depot_known {
            stale_names(&work, &current)
        } else {
            BTreeSet::new()
        };
        let mut files_removed = remove_entries(&install_dir, &blockers(&stale, &queued), &current);

        for (index, (depot, remaining)) in work.iter().zip(queued).enumerate() {
            if !remaining.is_empty() {
                self.installer.refuse_while_client_runs(appid)?;
                self.fetch_depot(
                    depot,
                    remaining,
                    &install_dir,
                    (&mut record, index),
                    &mut tally,
                )
                .await?;
            }
            record.depots[index].complete = true;
            record.save(&record_path, appid)?;
        }

        self.installer.refuse_while_client_runs(appid)?;
        files_removed += remove_entries(&install_dir, &stale, &current);
        let staging = install_dir.join(STAGING_DIR);
        if staging.exists() {
            fs::remove_dir_all(&staging).map_err(|source| io_error(appid, &staging, source))?;
        }
        self.write_acf(&work, existing)?;
        for depot in &work {
            let superseded = depot
                .previous_id
                .iter()
                .chain(&depot.abandoned_ids)
                .filter(|id| **id != depot.manifest_id);
            for id in superseded {
                let _ = fs::remove_file(self.paths().manifest(depot.depot_id, *id));
            }
        }
        // The install is finished. A record left behind only costs the next
        // run a rescan.
        let _ = fs::remove_file(&record_path);
        Ok(InstallOutcome {
            appid,
            changed_depots: work.iter().map(|depot| depot.depot_id).collect(),
            files_written: tally.files_written,
            files_removed,
            resumed,
        })
    }

    async fn fetch_depot(
        &mut self,
        depot: &WorkDepot,
        remaining: Vec<ManifestFile>,
        install_dir: &Path,
        (record, index): (&mut DownloadRecord, usize),
        tally: &mut Tally,
    ) -> Result<(), Error> {
        let appid = self.plan.appid;
        let depot_id = depot.depot_id;
        let record_path = self.paths().record(appid);
        let key = self.key(depot_id).await?;
        let fetcher = self.installer.session.chunks(appid, depot_id).await?;
        let (stop, stopped) = watch::channel(false);
        let stop = StopOnDrop(stop);
        let fetcher = Arc::new(Stoppable {
            inner: fetcher,
            stopped,
        });

        let (sender, mut events) = mpsc::unbounded_channel();
        let mut builder = DepotJob::builder()
            .depot_id(DepotId(depot_id))
            .depot_key(key)
            .install_dir(install_dir.to_owned())
            // Costs nothing for a missing file, and a file an earlier run
            // finished after its last record save is hashed, not refetched.
            .verify(true)
            .max_downloads(self.installer.options.max_downloads)
            .retry(self.installer.options.retry.clone())
            .event_sender(sender);
        if let Some(layouts) = depot.old_layouts() {
            builder = builder.old_file_layouts(layouts);
        }
        let job = builder.build().map_err(|error| Error::Depot {
            depot_id,
            message: error.to_string(),
        })?;
        let trimmed = DepotManifest::new(remaining);
        // `DepotJob` runs each chunk under `tokio::spawn` and offers no way
        // to cancel them, and a dropped `download` leaves them writing. So it
        // is never dropped: a stop makes the fetcher fail, `download` joins
        // its tasks and returns, and only then do the locks go, even when the
        // caller drops this future.
        let locks = Arc::clone(&self.locks);
        let mut download = tokio::spawn(async move {
            let result = job.download(&trimmed, fetcher).await;
            drop(locks);
            result
        });

        let mut client_check = tokio::time::interval_at(
            tokio::time::Instant::now() + CLIENT_CHECK_INTERVAL,
            CLIENT_CHECK_INTERVAL,
        );
        let mut unsaved = 0;
        let mut halted = None;
        let joined = loop {
            tokio::select! {
                biased;
                Some(event) = events.recv() => {
                    if self.absorb(event, depot_id, &mut record.depots[index], tally) {
                        unsaved += 1;
                    }
                    if unsaved >= RECORD_SAVE_EVERY {
                        if let Err(error) = record.save(&record_path, appid) {
                            stop.stop();
                            halted.get_or_insert(error);
                        }
                        unsaved = 0;
                    }
                }
                _ = client_check.tick(), if halted.is_none() => {
                    if (self.installer.client_running)() {
                        stop.stop();
                        halted = Some(Error::ClientRunning { appid });
                    }
                }
                joined = &mut download => break joined,
            }
        };
        while let Ok(event) = events.try_recv() {
            self.absorb(event, depot_id, &mut record.depots[index], tally);
        }
        // Saved before classifying so a disk that is too full for this write
        // reports the download's failure, and the last good record stands.
        let saved = record.save(&record_path, appid);
        if let Some(error) = halted {
            return Err(error);
        }
        joined
            .map_err(|error| Error::Depot {
                depot_id,
                message: error.to_string(),
            })?
            .map_err(|report| classify(&report, appid, depot_id, &record_path))?;
        saved
    }

    /// Applies one event; true when a file finished.
    fn absorb(
        &self,
        event: DownloadEvent,
        depot_id: u32,
        recorded: &mut RecordDepot,
        tally: &mut Tally,
    ) -> bool {
        let finished = match event {
            DownloadEvent::FileCompleted { filename } => {
                tally.files_written += 1;
                recorded.files.insert(filename);
                true
            }
            DownloadEvent::FileSkipped { filename } => {
                recorded.files.insert(filename);
                true
            }
            DownloadEvent::ChunkCompleted { bytes } => {
                tally.completed_bytes += bytes;
                false
            }
            _ => return false,
        };
        tally.completed_files += u64::from(finished);
        if let Some(progress) = self.installer.progress {
            progress(&InstallProgress {
                appid: self.plan.appid,
                depot_id,
                completed_files: tally.completed_files.min(tally.total_files),
                total_files: tally.total_files,
                completed_bytes: tally.completed_bytes,
                total_bytes: tally.total_bytes,
            });
        }
        finished
    }

    fn write_acf(
        &self,
        work: &[WorkDepot],
        existing: Option<(AppManifest, String)>,
    ) -> Result<(), Error> {
        let appid = self.plan.appid;
        let (previous, previous_text) = existing.unzip();
        // Depots this run did not touch stay recorded: the client may have
        // installed a language or DLC depot the plan does not select.
        let mut depots: BTreeMap<u32, InstalledDepot> = previous
            .iter()
            .flat_map(|manifest| &manifest.installed_depots)
            .map(|depot| (depot.depot_id, depot.clone()))
            .collect();
        for depot in work {
            depots.insert(
                depot.depot_id,
                InstalledDepot {
                    depot_id: depot.depot_id,
                    manifest_id: depot.manifest_id,
                    size: depot.size(),
                },
            );
        }
        let manifest = AppManifest {
            appid,
            name: self.plan.name.clone(),
            installdir: self.plan.installdir.clone(),
            state_flags: app_state::FULLY_INSTALLED,
            buildid: self.plan.buildid,
            last_updated: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs()),
            size_on_disk: depots.values().map(|depot| depot.size).sum(),
            installed_depots: depots.into_values().collect(),
        };
        let writer = AcfWriter::new(&manifest);
        let text = match previous_text {
            Some(text) => writer.merge(&text)?,
            None => writer.write(),
        };
        let path = self.paths().acf(appid);
        write_atomic(&path, text.as_bytes()).map_err(|source| io_error(appid, &path, source))
    }
}

/// Ends a run's fetches when dropped, so an install future the caller drops
/// stops them too.
struct StopOnDrop(watch::Sender<bool>);

impl StopOnDrop {
    fn stop(&self) {
        self.0.send_replace(true);
    }
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A fetcher that can be told to stop mid-download.
struct Stoppable<F> {
    inner: Arc<F>,
    stopped: watch::Receiver<bool>,
}

impl<F: ChunkFetcher> ChunkFetcher for Stoppable<F> {
    async fn fetch_chunk(&self, depot_id: DepotId, chunk_id: &ChunkId) -> Result<Bytes, BoxError> {
        let mut stopped = self.stopped.clone();
        let stop = async move {
            let _ = stopped.wait_for(|stopped| *stopped).await;
        };
        tokio::select! {
            biased;
            // An `Err` would go through `DepotJob`'s retry backoff, once per
            // queued chunk. An empty chunk fails its decode at once and
            // writes nothing.
            () = stop => Ok(Bytes::new()),
            fetched = self.inner.fetch_chunk(depot_id, chunk_id) => fetched,
        }
    }
}

/// `DepotJob` reports no event for a directory or an empty file, so they
/// stay out of the progress totals.
fn reports_events(file: &ManifestFile) -> bool {
    let flags = DepotFileFlags::from_bits_retain(file.flags);
    !flags.is_directory()
        && (file.size > 0 || !file.chunks.is_empty() || file.link_target.is_some())
}

#[derive(Default)]
struct Tally {
    completed_files: u64,
    total_files: u64,
    completed_bytes: u64,
    total_bytes: u64,
    files_written: u64,
}

/// What each depot of `work` still has to write under `record`.
fn queue_files(
    record: &DownloadRecord,
    work: &[WorkDepot],
    layers: &Layers<'_>,
    install_dir: &Path,
    tally: &mut Tally,
) -> Vec<Vec<ManifestFile>> {
    let mut queued = Vec::with_capacity(work.len());
    for (depot, done) in work.iter().zip(&record.depots) {
        let remaining: Vec<ManifestFile> = if done.complete {
            Vec::new()
        } else {
            layers
                .to_write(depot, record.rescan, install_dir)
                .filter(|file| !done.files.contains(&file.filename))
                .cloned()
                .collect()
        };
        tally.total_files += remaining.iter().filter(|file| reports_events(file)).count() as u64;
        tally.total_bytes += remaining.iter().map(|file| file.size).sum::<u64>();
        queued.push(remaining);
    }
    queued
}

/// What [`Run::kept_depots`] found. `complete` is false when a depot's
/// manifest could not be had, so the rest does not describe the whole title.
struct Kept {
    depots: Vec<KeptDepot>,
    complete: bool,
}

/// Moves into `work` each kept depot that came to own a path, which happens
/// when a later depot drops an overlay, and returns the rest.
fn promote_heirs(work: &mut Vec<WorkDepot>, kept: Vec<KeptDepot>) -> Vec<KeptDepot> {
    let heirs: BTreeSet<u32> = {
        let layers = Layers::new(work, &kept);
        kept.iter()
            .filter(|depot| {
                depot
                    .manifest
                    .files
                    .iter()
                    .any(|file| layers.owns(depot.depot_id, file) && layers.changed(file))
            })
            .map(|depot| depot.depot_id)
            .collect()
    };
    let (promoted, kept): (Vec<_>, Vec<_>) = kept
        .into_iter()
        .partition(|depot| heirs.contains(&depot.depot_id));
    work.extend(promoted.into_iter().map(WorkDepot::from));
    kept
}

/// Every path the final manifests name; true for a directory entry.
fn current_entries(work: &[WorkDepot], kept: &[KeptDepot]) -> BTreeMap<String, bool> {
    work.iter()
        .map(|depot| &depot.manifest)
        .chain(kept.iter().map(|depot| &depot.manifest))
        .flat_map(|manifest| &manifest.files)
        .map(|file| {
            let flags = DepotFileFlags::from_bits_retain(file.flags);
            (file.normalized_path(), flags.is_directory())
        })
        .collect()
}

/// Paths the old manifests had and no current manifest names.
fn stale_names(work: &[WorkDepot], current: &BTreeMap<String, bool>) -> BTreeSet<String> {
    work.iter()
        .flat_map(|depot| depot.previous.iter().chain(&depot.abandoned))
        .flat_map(|previous| &previous.files)
        .filter(|file| !is_user_config(file))
        .map(ManifestFile::normalized_path)
        .filter(|path| !current.contains_key(path))
        .collect()
}

/// The stale paths in the way of a queued one, either side of a file that
/// became a directory or the reverse. `DepotJob` cannot land those.
fn blockers(stale: &BTreeSet<String>, queued: &[Vec<ManifestFile>]) -> BTreeSet<String> {
    let names: BTreeSet<String> = queued.iter().flatten().map(path_key).collect();
    let ancestors = |path: &str| -> Vec<String> {
        Path::new(path)
            .ancestors()
            .skip(1)
            .map(|parent| parent.to_string_lossy().into_owned())
            .collect()
    };
    let queued_parents: BTreeSet<String> = names.iter().flat_map(|name| ancestors(name)).collect();
    stale
        .iter()
        .filter(|path| {
            let key = path.to_lowercase();
            queued_parents.contains(&key)
                || ancestors(&key).iter().any(|parent| names.contains(parent))
        })
        .cloned()
        .collect()
}

fn file_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::symlink_metadata(path).ok()?;
    Some((meta.dev(), meta.ino()))
}

/// Deletes `names`, then any directory that leaves empty. A file that will
/// not go is left behind: it is harmless, and the depot is already correct.
fn remove_entries(
    install_dir: &Path,
    names: &BTreeSet<String>,
    current: &BTreeMap<String, bool>,
) -> u64 {
    let Ok(root) = fs::canonicalize(install_dir) else {
        return 0;
    };
    // The volume decides what counts as one name: APFS folds case and Unicode
    // form, more than any comparison here could, so a respelled file is
    // found by identity. A file entry with a directory in its place is on
    // its way out.
    let live: HashSet<(u64, u64)> = current
        .iter()
        .map(|(name, is_dir)| (install_dir.join(name), *is_dir))
        .filter(|(path, is_dir)| *is_dir || !path.is_dir() || path.is_symlink())
        .filter_map(|(path, _)| file_identity(&path))
        .collect();
    let is_live =
        |path: &Path| file_identity(path).is_some_and(|identity| live.contains(&identity));
    let mut names: Vec<&String> = names.iter().collect();
    // Longest first, so a directory entry follows the files inside it.
    names.sort_by_key(|name| std::cmp::Reverse(name.len()));
    let mut removed = 0;
    for name in names {
        let path = install_dir.join(name);
        if is_live(&path) || link_out_of(install_dir, &root, Path::new(name)).is_some() {
            continue;
        }
        let is_dir = fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir());
        let gone = if is_dir {
            fs::remove_dir(&path)
        } else {
            fs::remove_file(&path)
        };
        if gone.is_ok() {
            removed += 1;
        }
        for parent in Path::new(name).ancestors().skip(1) {
            // An explicit empty directory of the new manifest stays.
            let path = install_dir.join(parent);
            if parent.as_os_str().is_empty() || is_live(&path) || fs::remove_dir(path).is_err() {
                break;
            }
        }
    }
    removed
}

/// The first symlink on the way to `relative` that leads out of the install,
/// with its target. `root` is the install directory, canonicalized.
fn link_out_of(install_dir: &Path, root: &Path, relative: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut at = install_dir.to_owned();
    for part in relative.parent()?.components() {
        at.push(part);
        let meta = fs::symlink_metadata(&at).ok()?;
        if meta.is_symlink() && !fs::canonicalize(&at).is_ok_and(|real| real.starts_with(root)) {
            let target = fs::read_link(&at).unwrap_or_default();
            return Some((at, target));
        }
    }
    None
}

/// Refuses a run that would write through a symlink out of the install, which
/// a user or another tool made: `DepotJob` joins paths and follows them. A
/// link the manifest lists stays inside, by [`check_paths`].
fn refuse_links_out(install_dir: &Path, queued: &[Vec<ManifestFile>]) -> Result<(), Error> {
    let Ok(root) = fs::canonicalize(install_dir) else {
        return Ok(());
    };
    let staging = Path::new(STAGING_DIR).join("staging");
    let mut parents: HashSet<PathBuf> = queued
        .iter()
        .flatten()
        .filter_map(|file| Some(Path::new(&file.normalized_path()).parent()?.to_owned()))
        .collect();
    parents.insert(staging);
    for parent in parents {
        if let Some((link, target)) = link_out_of(install_dir, &root, &parent.join("file")) {
            return Err(Error::LinkOutOfInstall { link, target });
        }
    }
    Ok(())
}

/// Refuses a manifest whose paths would land outside the install directory.
/// `DepotJob` joins them as given, and the manifest's signature is not
/// checked by `steamroom`.
fn check_paths(manifest: &DepotManifest) -> Result<(), Error> {
    let contained = |path: &str| {
        !path.is_empty()
            && Path::new(path)
                .components()
                .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
    };
    for file in &manifest.files {
        let target_escapes = file
            .link_target
            .as_ref()
            .is_some_and(|target| !contained(&target.replace('\\', "/")));
        if !contained(&file.normalized_path()) || target_escapes {
            return Err(Error::UnsafePath {
                path: file.filename.clone(),
            });
        }
    }
    Ok(())
}

fn classify(report: &DownloadReport, appid: u32, depot_id: u32, record: &Path) -> Error {
    let context = report.current_context();
    if is_disk_full(context) {
        return Error::DiskFull {
            appid,
            record: record.to_owned(),
        };
    }
    match context {
        DownloadError::Chunk(_)
        | DownloadError::ChunkSha1Mismatch { .. }
        | DownloadError::Sha1Mismatch { .. } => Error::CorruptChunk {
            depot_id,
            detail: report.to_string(),
        },
        DownloadError::Fetch { source } => match source.downcast_ref::<SteamError>() {
            Some(SteamError::CdnStatus {
                status,
                retry_after,
            }) if matches!(status.as_u16(), TOO_MANY_REQUESTS | SERVICE_UNAVAILABLE) => {
                Error::Auth(AuthError::RateLimited {
                    retry_after: retry_after.map_or(RATE_LIMIT_RETRY_AFTER, Duration::from_secs),
                })
            }
            _ => Error::Depot {
                depot_id,
                message: report.to_string(),
            },
        },
        _ => Error::Depot {
            depot_id,
            message: report.to_string(),
        },
    }
}

fn is_disk_full(error: &(dyn StdError + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if error.downcast_ref::<io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
            )
        }) {
            return true;
        }
        current = error.source();
    }
    false
}

fn io_error(appid: u32, path: &Path, source: io::Error) -> Error {
    if is_disk_full(&source) {
        return Error::DiskFull {
            appid,
            record: path.to_owned(),
        };
    }
    Error::Io {
        path: path.to_owned(),
        source,
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    let temporary = PathBuf::from(name);
    let written = File::create(&temporary).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    match written.and_then(|()| fs::rename(&temporary, path)) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error)
        }
    }
}

/// A manifest stored earlier. A library can live on a shared or restored
/// volume, so its paths get the same check as a fetched one.
fn load_manifest(path: &Path) -> Option<DepotManifest> {
    let manifest: DepotManifest = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    check_paths(&manifest).ok()?;
    Some(manifest)
}

fn read_acf(paths: &LibraryPaths, appid: u32) -> Result<Option<(AppManifest, String)>, Error> {
    let path = paths.acf(appid);
    match fs::read_to_string(&path) {
        Ok(text) => Ok(Some((AppManifest::parse(&text)?, text))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::Io { path, source }),
    }
}

/// What [`verify_install`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyReport {
    pub appid: u32,
    pub files_checked: u64,
    /// Present with the wrong size or content.
    pub mismatched: Vec<String>,
    pub missing: Vec<String>,
}

impl VerifyReport {
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.mismatched.is_empty() && self.missing.is_empty()
    }
}

/// `--verify`: re-hashes every file against the stored manifests of the
/// depots the `.acf` records. Reads only, needs no session, and takes the
/// same locks as an install. Hashes on the calling thread; from async code,
/// call it inside `spawn_blocking`.
///
/// # Errors
///
/// [`Error::LockHeld`], [`Error::NotInstalled`] without an `.acf`, and
/// [`Error::ManifestNotStored`] for a depot installed by something other
/// than this crate.
pub fn verify_install(
    paths: &LibraryPaths,
    data_dir: &Path,
    appid: u32,
) -> Result<VerifyReport, Error> {
    let mut locks = LockDir::new(data_dir, "verify");
    let mut app_lock = locks.app(appid, lock::Mode::NonBlocking)?;
    let _library_lock = app_lock.library(lock::Mode::NonBlocking)?;
    let (acf, _) = read_acf(paths, appid)?.ok_or(Error::NotInstalled { appid })?;
    check_installdir(&acf.installdir)?;
    let install_dir = paths.install_dir(&acf.installdir);

    let mut manifests = Vec::new();
    for depot in &acf.installed_depots {
        let manifest = load_manifest(&paths.manifest(depot.depot_id, depot.manifest_id)).ok_or(
            Error::ManifestNotStored {
                appid,
                depot_id: depot.depot_id,
                manifest_id: depot.manifest_id,
            },
        )?;
        manifests.push((depot.depot_id, manifest));
    }
    let owners = path_owners(manifests.iter().map(|(id, manifest)| (*id, manifest)));
    let expected: BTreeMap<String, &ManifestFile> = owners
        .into_values()
        .map(|(_, file)| (file.normalized_path(), file))
        .collect();

    let mut report = VerifyReport {
        appid,
        ..VerifyReport::default()
    };
    for (name, file) in &expected {
        report.files_checked += 1;
        match check_file(&install_dir.join(name), file) {
            Ok(true) => {}
            Ok(false) => report.mismatched.push(name.clone()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                report.missing.push(name.clone());
            }
            Err(source) => {
                return Err(Error::Io {
                    path: install_dir.join(name),
                    source,
                });
            }
        }
    }
    Ok(report)
}

fn check_file(path: &Path, file: &ManifestFile) -> io::Result<bool> {
    let flags = DepotFileFlags::from_bits_retain(file.flags);
    let meta = fs::symlink_metadata(path)?;
    if flags.is_directory() {
        return Ok(meta.is_dir());
    }
    if is_user_config(file) {
        return Ok(true);
    }
    // `DepotJob` takes an empty file on its size, and what real manifests
    // carry as its hash is unconfirmed.
    if file.size == 0 && file.chunks.is_empty() && file.link_target.is_none() {
        return Ok(meta.is_file() && meta.len() == 0);
    }
    if flags.is_symlink() || file.link_target.is_some() {
        let target = fs::read_link(path).ok();
        return Ok(target.as_deref() == file.link_target.as_deref().map(Path::new));
    }
    if !meta.is_file() || meta.len() != file.size {
        return Ok(false);
    }
    if let Some(expected) = &file.sha_content {
        return Ok(sha1_of_file(path)? == *expected);
    }
    check_chunks(path, file)
}

/// For a manifest entry without a whole-file hash: each chunk id is the SHA-1
/// of its bytes.
fn check_chunks(path: &Path, file: &ManifestFile) -> io::Result<bool> {
    use std::os::unix::fs::FileExt;
    let handle = File::open(path)?;
    let mut buffer = Vec::new();
    for (chunk, offset) in file.chunks.iter().zip(chunk_offsets(file)) {
        if chunk.id == ChunkId([0; 20]) {
            continue;
        }
        buffer.resize(chunk.uncompressed_size as usize, 0);
        handle.read_exact_at(&mut buffer, offset)?;
        let actual: [u8; 20] = Sha1::digest(&buffer).into();
        if actual != chunk.id.0 {
            return Ok(false);
        }
    }
    Ok(true)
}

fn sha1_of_file(path: &Path) -> io::Result<[u8; 20]> {
    let mut file = File::open(path)?;
    let mut hasher = Sha1::new();
    let mut buffer = vec![0; HASH_BUFFER];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(hasher.finalize().into());
        }
        hasher.update(&buffer[..read]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_write_and_a_failed_fetch_both_read_as_disk_full() {
        let write = DownloadError::Io(io::Error::from(io::ErrorKind::StorageFull));
        assert!(is_disk_full(&write));
        let fetch = DownloadError::Fetch {
            source: Box::new(SteamError::Io(io::Error::from(
                io::ErrorKind::QuotaExceeded,
            ))),
        };
        assert!(is_disk_full(&fetch));
        let other = DownloadError::Io(io::Error::from(io::ErrorKind::PermissionDenied));
        assert!(!is_disk_full(&other));
    }

    #[test]
    fn a_chunk_without_an_offset_follows_the_one_before() {
        let mut file = ManifestFile::new("f".to_owned(), 8);
        for _ in 0..3 {
            file.chunks
                .push(steamroom::depot::manifest::ManifestChunk::new(
                    ChunkId([1; 20]),
                    0,
                    4,
                ));
        }
        file.chunks[1].offset = Some(10);
        assert_eq!(chunk_offsets(&file), [0, 10, 14]);
    }
}
