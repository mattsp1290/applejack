//! Prefix operations: create, list, snapshot, restore, reset, remove, and
//! the development `exec`. Prefixes live at `<data_dir>/prefixes/<name>/`
//! and snapshots at `<data_dir>/prefixes/.snapshots/<name>/<timestamp>/`.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use applejack_engine::rosetta_wine::prefix::{LIBRARY_DRIVE, backend_available};
use applejack_engine::rosetta_wine::process::{server_socket, wineserver_alive};
use applejack_engine::rosetta_wine::{Snapshot, snapshot};
use applejack_engine::{
    Engine, Error as EngineError, GraphicsBackend, HidrawMode, InstalledEngine, LaunchSpec, Prefix,
    PrefixSpec, SyncMode, Version,
};
use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::state::Core;

const PREFIXES_DIR: &str = "prefixes";
const SNAPSHOTS_DIR: &str = ".snapshots";
const LIBRARY_DIR: &str = "library";

/// How long an operation waits for a prefix's `wineserver` to leave. A
/// server outlives its last process by a few seconds and only then writes
/// the registry, so an operation right after a title exits waits it out; a
/// title that is still running is refused once this passes.
pub const SERVER_SETTLE: Duration = Duration::from_secs(10);

const EXEC_POLL: Duration = Duration::from_millis(100);

/// One prefix as `prefix list` shows it. `problem` is set, and the rest
/// empty, for a directory the engine cannot read as a prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrefixEntry {
    pub name: String,
    pub dir: PathBuf,
    pub engine_version: Option<String>,
    pub backend: Option<GraphicsBackend>,
    pub sync_mode: Option<SyncMode>,
    pub hidraw: Option<HidrawMode>,
    pub problem: Option<String>,
}

impl PrefixEntry {
    fn of(name: &str, dir: &Path, prefix: &Prefix) -> Self {
        Self {
            name: name.to_owned(),
            dir: dir.to_path_buf(),
            engine_version: Some(prefix.engine_version.to_string()),
            backend: Some(prefix.backend),
            sync_mode: Some(prefix.sync_mode),
            hidraw: Some(prefix.hidraw),
            problem: None,
        }
    }
}

/// The result of `prefix.snapshot` and `prefix.restore`: a snapshot, which
/// for a restore is the safety snapshot of the replaced state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotResult {
    pub prefix: String,
    pub timestamp: String,
    pub dir: PathBuf,
}

impl From<Snapshot> for SnapshotResult {
    fn from(snapshot: Snapshot) -> Self {
        Self {
            prefix: snapshot.name,
            timestamp: snapshot.timestamp,
            dir: snapshot.dir,
        }
    }
}

impl Core {
    fn prefixes_root(&self) -> PathBuf {
        self.data_dir().join(PREFIXES_DIR)
    }

    fn snapshots_root(&self) -> PathBuf {
        self.prefixes_root().join(SNAPSHOTS_DIR)
    }

    fn prefix_dir(&self, name: &str) -> Result<PathBuf, CoreError> {
        snapshot::check_name(name)?;
        Ok(self.prefixes_root().join(name))
    }

    fn open_prefix(&self, name: &str) -> Result<(PathBuf, Prefix), CoreError> {
        let dir = self.prefix_dir(name)?;
        if !dir.is_dir() {
            return Err(CoreError::UnknownPrefix {
                name: name.to_owned(),
            });
        }
        let prefix = self.engine.open_prefix(&dir)?;
        Ok((dir, prefix))
    }

    /// The shared library directory, created on first use.
    fn library_dir(&self) -> Result<PathBuf, CoreError> {
        let dir = self.data_dir().join(LIBRARY_DIR);
        fs::create_dir_all(&dir).map_err(|error| CoreError::io(&dir, error))?;
        Ok(dir)
    }

    /// Waits out a lingering `wineserver`; a server still there after
    /// [`SERVER_SETTLE`] belongs to something running.
    fn settle(dir: &Path) -> Result<(), CoreError> {
        let deadline = Instant::now() + SERVER_SETTLE;
        while wineserver_alive(dir)? {
            if Instant::now() >= deadline {
                return Err(EngineError::PrefixUnusable {
                    path: dir.to_path_buf(),
                    reason: "a wineserver is still running for it",
                }
                .into());
            }
            std::thread::sleep(EXEC_POLL);
        }
        Ok(())
    }

    /// Builds a prefix with the active engine. `sync_mode` defaults to the
    /// bundle's first.
    ///
    /// # Errors
    ///
    /// [`CoreError::NoActiveEngine`], [`CoreError::SyncUnavailable`], or
    /// the engine's creation failure; the directory is removed on failure.
    pub fn prefix_create(
        &mut self,
        name: &str,
        backend: GraphicsBackend,
        sync_mode: Option<SyncMode>,
        hidraw: HidrawMode,
    ) -> Result<PrefixEntry, CoreError> {
        let dir = self.prefix_dir(name)?;
        let installed = self.require_active_engine()?;
        let sync_mode = pick_sync(&installed, sync_mode)?;
        let library = self.library_dir()?;
        let root = self.prefixes_root();
        fs::create_dir_all(&root).map_err(|error| CoreError::io(&root, error))?;
        let spec = self.prefix_spec(backend, sync_mode, hidraw, library);
        let prefix = self.engine.create_prefix(&installed, &dir, &spec)?;
        Ok(PrefixEntry::of(name, &dir, &prefix))
    }

    fn prefix_spec(
        &self,
        backend: GraphicsBackend,
        sync_mode: SyncMode,
        hidraw: HidrawMode,
        library: PathBuf,
    ) -> PrefixSpec {
        PrefixSpec {
            backend,
            sync_mode,
            hidraw,
            library_dir: Some(library),
            log_dir: self.log_dir().to_path_buf(),
            boot_timeout: None,
        }
    }

    /// Every directory under `prefixes/`, by name.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Io`] when the directory cannot be read.
    pub fn prefix_list(&self) -> Result<Vec<PrefixEntry>, CoreError> {
        let root = self.prefixes_root();
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(CoreError::io(root, error)),
        };
        let mut list = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| CoreError::io(&root, error))?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let dir = entry.path();
            if name.starts_with('.') || !dir.is_dir() {
                continue;
            }
            list.push(match self.engine.open_prefix(&dir) {
                Ok(prefix) => PrefixEntry::of(&name, &dir, &prefix),
                Err(error) => PrefixEntry {
                    name,
                    dir,
                    engine_version: None,
                    backend: None,
                    sync_mode: None,
                    hidraw: None,
                    problem: Some(error.to_string()),
                },
            });
        }
        list.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(list)
    }

    /// Clones the prefix to a new snapshot once its server has left.
    ///
    /// # Errors
    ///
    /// [`CoreError::UnknownPrefix`], a server that does not leave, or the
    /// copy failure.
    pub fn prefix_snapshot(&self, name: &str) -> Result<SnapshotResult, CoreError> {
        let (dir, _) = self.open_prefix(name)?;
        Self::settle(&dir)?;
        Ok(snapshot::create(&dir, &self.snapshots_root(), name)?.into())
    }

    /// Swaps a snapshot in and returns the safety snapshot taken first.
    ///
    /// # Errors
    ///
    /// [`CoreError::UnknownPrefix`], [`EngineError::SnapshotNotFound`], a
    /// server that does not leave, or the copy or swap failure.
    pub fn prefix_restore(&self, name: &str, timestamp: &str) -> Result<SnapshotResult, CoreError> {
        let dir = self.prefix_dir(name)?;
        if !dir.is_dir() {
            return Err(CoreError::UnknownPrefix {
                name: name.to_owned(),
            });
        }
        Self::settle(&dir)?;
        Ok(snapshot::restore(&dir, &self.snapshots_root(), name, timestamp)?.into())
    }

    /// Recreates the prefix with the active engine and its recorded
    /// settings, after a safety snapshot, which it returns.
    ///
    /// # Errors
    ///
    /// Refused, with the prefix untouched, when the recorded backend is not
    /// available in the active engine; a failed rebuild leaves the safety
    /// snapshot as the only copy.
    pub fn prefix_reset(
        &mut self,
        name: &str,
        keep_user_data: bool,
    ) -> Result<SnapshotResult, CoreError> {
        let (dir, prefix) = self.open_prefix(name)?;
        let installed = self.require_active_engine()?;
        backend_available(&installed, prefix.backend)?;
        let sync_mode = installed
            .bundle
            .sync_modes
            .contains(&prefix.sync_mode)
            .then_some(prefix.sync_mode);
        let sync_mode = pick_sync(&installed, sync_mode)?;
        let library = fs::read_link(dir.join("dosdevices").join(LIBRARY_DRIVE))
            .ok()
            .filter(|path| path.is_dir());
        let library = match library {
            Some(library) => library,
            None => self.library_dir()?,
        };
        Self::settle(&dir)?;
        let spec = self.prefix_spec(prefix.backend, sync_mode, prefix.hidraw, library);
        let engine = self.engine;
        let safety = snapshot::reset(
            &dir,
            &self.snapshots_root(),
            name,
            keep_user_data,
            &mut |dir| engine.create_prefix(&installed, dir, &spec).map(|_| ()),
        )?;
        Ok(safety.into())
    }

    /// Deletes the prefix and the server directory Wine left for it.
    /// Snapshots stay, so a removal can be undone.
    ///
    /// # Errors
    ///
    /// [`CoreError::UnknownPrefix`], a server that does not leave, or the
    /// removal failure.
    pub fn prefix_remove(&self, name: &str) -> Result<(), CoreError> {
        let dir = self.prefix_dir(name)?;
        if !dir.is_dir() {
            return Err(CoreError::UnknownPrefix {
                name: name.to_owned(),
            });
        }
        Self::settle(&dir)?;
        let server_dir = server_socket(&dir)
            .ok()
            .and_then(|socket| socket.parent().map(Path::to_path_buf));
        fs::remove_dir_all(&dir).map_err(|error| CoreError::io(&dir, error))?;
        if let Some(server_dir) = server_dir {
            let _ = fs::remove_dir_all(server_dir);
        }
        Ok(())
    }

    /// Runs `exe` in the prefix through `Engine::launch`, with the bundle
    /// that last touched the prefix, copies the session log to `out` as it
    /// grows, and returns the exit code (128 plus the signal for a signal).
    ///
    /// # Errors
    ///
    /// [`CoreError::PathOutsideData`] for an absolute Unix path outside the
    /// data directory; the launch failure; a log read failure.
    pub fn prefix_exec(
        &mut self,
        name: &str,
        exe: &Path,
        args: &[OsString],
        out: &mut dyn Write,
    ) -> Result<i32, CoreError> {
        let (dir, prefix) = self.open_prefix(name)?;
        self.check_inside_data(exe)?;
        let installed = self.prefix_engine(&prefix)?;
        let spec = LaunchSpec {
            exe: exe.to_path_buf(),
            args: args.to_vec(),
            env_extras: Vec::new(),
            cwd: dir,
            log_dir: self.log_dir().to_path_buf(),
            advertise_avx: false,
            cpuid_probe: None,
        };
        let mut session = self.engine.launch(&installed, &prefix, &spec)?;
        let mut offset = 0;
        let status = loop {
            let status = session.try_wait()?;
            copy_new(&session.log_path, &mut offset, out)?;
            if let Some(status) = status {
                break status;
            }
            std::thread::sleep(EXEC_POLL);
        };
        Ok(exit_code(status))
    }

    fn prefix_engine(&self, prefix: &Prefix) -> Result<InstalledEngine, CoreError> {
        let version: &Version = &prefix.engine_version;
        Ok(InstalledEngine::open(
            self.layout(),
            prefix.engine,
            version,
        )?)
    }

    fn check_inside_data(&self, exe: &Path) -> Result<(), CoreError> {
        if !exe.is_absolute() {
            return Ok(());
        }
        let canonical = |path: &Path| fs::canonicalize(path).map_err(|e| CoreError::io(path, e));
        if canonical(exe)?.starts_with(canonical(self.data_dir())?) {
            Ok(())
        } else {
            Err(CoreError::PathOutsideData {
                exe: exe.to_path_buf(),
            })
        }
    }
}

fn pick_sync(
    installed: &InstalledEngine,
    requested: Option<SyncMode>,
) -> Result<SyncMode, CoreError> {
    let bundle = &installed.bundle;
    match requested {
        Some(mode) if bundle.sync_modes.contains(&mode) => Ok(mode),
        Some(mode) => Err(CoreError::SyncUnavailable {
            version: installed.version.to_string(),
            mode,
        }),
        None => Ok(bundle.default_sync_mode().unwrap_or(SyncMode::None)),
    }
}

/// Copies what the log gained to `out`. A reader that went away does not
/// stop the wait: the child still runs and its exit code still counts.
fn copy_new(log: &Path, offset: &mut u64, out: &mut dyn Write) -> Result<(), CoreError> {
    let mut file = match fs::File::open(log) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(CoreError::io(log, error)),
    };
    let mut fresh = Vec::new();
    file.seek(SeekFrom::Start(*offset))
        .and_then(|_| file.read_to_end(&mut fresh))
        .map_err(|error| CoreError::io(log, error))?;
    *offset += fresh.len() as u64;
    let _ = out.write_all(&fresh).and_then(|()| out.flush());
    Ok(())
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}
