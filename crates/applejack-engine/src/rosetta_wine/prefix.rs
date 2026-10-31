//! The prefix builder: `wineboot --init`, backend DLL overrides, registry
//! seeds, and the library drive, in the order [`PREFIX_STEPS`] lists. The
//! engine's own state for a prefix lives in `<prefix>/applejack/`:
//! `prefix.toml` (the [`PrefixRecord`]), the last `seeds.reg`, and its
//! `reg-import.log`.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::bundle::DllOverrides;
use crate::{
    BundleToml, EngineId, Error, GraphicsBackend, HidrawMode, InstalledEngine, Prefix, PrefixSpec,
    Result, SyncMode, Version,
};

use super::d3dmetal;
use super::launch::{
    assemble_env, engine_rows, kill_prefix_server, new_session_id, run_loader, wait_for_server,
};

/// One step of prefix creation, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrefixStep {
    /// `<loader> wineboot --init` with the Mono and Gecko installers
    /// disabled, then `<server> -w`.
    WineBoot,
    /// The backend's DLLs copied into `drive_c/windows/system32` (x86-64)
    /// and `syswow64` (i386).
    DllOverrides,
    /// `reg import` of the rendered seeds: crash dialog off, `DllOverrides`
    /// persisted, the HID toggle.
    RegistrySeeds,
    /// `dosdevices/s:` linked to the shared library directory.
    LibraryDrive,
}

impl PrefixStep {
    /// The step's name in logs and docs.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::WineBoot => "wineboot",
            Self::DllOverrides => "dll-overrides",
            Self::RegistrySeeds => "registry-seeds",
            Self::LibraryDrive => "library-drive",
        }
    }
}

/// Every step, in the order `create_prefix` runs them.
pub const PREFIX_STEPS: [PrefixStep; 4] = [
    PrefixStep::WineBoot,
    PrefixStep::DllOverrides,
    PrefixStep::RegistrySeeds,
    PrefixStep::LibraryDrive,
];

/// The engine's directory inside a prefix.
pub const STATE_DIR: &str = "applejack";
/// The record file inside [`STATE_DIR`].
pub const RECORD_FILE: &str = "prefix.toml";
/// The `dosdevices` entry for the shared library.
pub const LIBRARY_DRIVE: &str = "s:";
/// How long `<loader> wineboot --init` may run when
/// [`PrefixSpec::boot_timeout`] is `None`. On expiry the loader is killed,
/// the prefix's server stopped with `-k`, the prefix removed, and the error
/// names `wineboot.log`. The Mono and Gecko override removes the known
/// dialog; this bound covers any other modal `wineboot` could raise.
pub const WINEBOOT_TIMEOUT: Duration = Duration::from_secs(300);
/// How long `wineserver -w` may take after `wineboot` or `reg import`, and
/// how long `reg import` itself may run, before the server is killed and
/// the step reported failed.
pub const SERVER_WAIT: Duration = Duration::from_secs(180);

const WINEBOOT_LOG: &str = "wineboot.log";
const IMPORT_LOG: &str = "reg-import.log";
const SEEDS_FILE: &str = "seeds.reg";

/// What the engine last applied to a prefix, as `applejack/prefix.toml`.
/// Unknown fields are kept, not refused: an older engine binary must still
/// open a prefix a newer one annotated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrefixRecord {
    /// Engine that created the prefix.
    pub engine: EngineId,
    /// Engine version that last touched it.
    pub engine_version: Version,
    /// Backend whose DLLs are in the prefix.
    pub backend: GraphicsBackend,
    /// Sync mode applied with it.
    pub sync_mode: SyncMode,
    /// HID toggle applied with it.
    pub hidraw: HidrawMode,
    /// DLL files the engine copied in or was copying in when the record was
    /// written, relative to the prefix, so the next `apply_overrides`
    /// removes them before copying another backend's. The record is
    /// rewritten before and after every copy, so it never names fewer files
    /// than are on disk.
    #[serde(default)]
    pub copied_dlls: Vec<PathBuf>,
}

impl PrefixRecord {
    /// Reads `<prefix_dir>/applejack/prefix.toml`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidPrefix`] when the record is absent or does
    /// not parse.
    pub fn read(prefix_dir: &Path) -> Result<Self> {
        let path = prefix_dir.join(STATE_DIR).join(RECORD_FILE);
        let invalid = |reason: String| Error::InvalidPrefix {
            path: prefix_dir.to_path_buf(),
            reason,
        };
        let text = fs::read_to_string(&path)
            .map_err(|error| invalid(format!("cannot read `{}`: {error}", path.display())))?;
        toml::from_str(&text)
            .map_err(|error| invalid(format!("`{}` does not parse: {error}", path.display())))
    }

    /// Writes the record through `prefix.toml.tmp` and a rename, so a
    /// reader sees either the previous record or this one, never a
    /// truncated file.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`]; the previous record is then still in place.
    pub fn write(&self, prefix_dir: &Path) -> Result<()> {
        let dir = prefix_dir.join(STATE_DIR);
        fs::create_dir_all(&dir).map_err(|error| Error::io(&dir, error))?;
        let path = dir.join(RECORD_FILE);
        let staging = dir.join(format!("{RECORD_FILE}.tmp"));
        let text =
            toml::to_string(self).map_err(|error| Error::io(&path, io::Error::other(error)))?;
        fs::write(&staging, text).map_err(|error| Error::io(&staging, error))?;
        fs::rename(&staging, &path).map_err(|error| Error::io(&path, error))
    }

    /// The [`Prefix`] the record describes.
    #[must_use]
    pub fn to_prefix(&self, dir: &Path) -> Prefix {
        Prefix {
            dir: dir.to_path_buf(),
            engine: self.engine,
            engine_version: self.engine_version.clone(),
            backend: self.backend,
            sync_mode: self.sync_mode,
            hidraw: self.hidraw,
        }
    }
}

/// Checks that `backend` can be applied with `installed`: its override
/// table lists a DLL (`WineD3D` needs none), and for `D3DMetal` the overlay
/// holds the payload.
///
/// # Errors
///
/// Returns [`Error::BackendUnavailable`] with the reason.
pub fn backend_available(installed: &InstalledEngine, backend: GraphicsBackend) -> Result<()> {
    let unavailable = |reason: String| Error::BackendUnavailable { backend, reason };
    match backend {
        GraphicsBackend::WineD3D => Ok(()),
        GraphicsBackend::D3DMetal => {
            let Some(dir) = d3dmetal::overlay_dir(installed) else {
                return Err(unavailable(format!(
                    "engine version {} declares no [d3dmetal] overlay location",
                    installed.version
                )));
            };
            if d3dmetal::overlay_present(installed) {
                Ok(())
            } else {
                Err(unavailable(format!(
                    "no verified D3DMetal overlay at `{}`; run `applejack engine d3dmetal import`",
                    dir.display()
                )))
            }
        }
        GraphicsBackend::Dxmt | GraphicsBackend::Vkd3dProton => {
            if override_dlls(&installed.bundle, backend).is_empty() {
                Err(unavailable(format!(
                    "engine version {} lists no DLLs for it in BUNDLE.toml",
                    installed.version
                )))
            } else {
                Ok(())
            }
        }
    }
}

static NO_OVERRIDES: DllOverrides = DllOverrides {
    x86_64: Vec::new(),
    i386: Vec::new(),
};

/// The override table for `backend`; empty for `WineD3D`.
#[must_use]
pub fn override_dlls(bundle: &BundleToml, backend: GraphicsBackend) -> &DllOverrides {
    match backend {
        GraphicsBackend::Dxmt => &bundle.overrides.dxmt,
        GraphicsBackend::Vkd3dProton => &bundle.overrides.vkd3d,
        GraphicsBackend::D3DMetal => &bundle.overrides.d3dmetal,
        GraphicsBackend::WineD3D => &NO_OVERRIDES,
    }
}

/// The DLL names (file stems, lowercase, sorted, unique across both
/// architectures) `backend` overrides.
#[must_use]
pub fn override_stems(bundle: &BundleToml, backend: GraphicsBackend) -> Vec<String> {
    let table = override_dlls(bundle, backend);
    let mut stems: Vec<String> = table
        .x86_64
        .iter()
        .chain(&table.i386)
        .filter_map(|dll| dll.file_stem())
        .map(|stem| stem.to_string_lossy().to_ascii_lowercase())
        .collect();
    stems.sort();
    stems.dedup();
    stems
}

/// Every DLL name any backend of the bundle overrides.
fn all_stems(bundle: &BundleToml) -> Vec<String> {
    let mut stems: Vec<String> = [
        GraphicsBackend::Dxmt,
        GraphicsBackend::Vkd3dProton,
        GraphicsBackend::D3DMetal,
    ]
    .into_iter()
    .flat_map(|backend| override_stems(bundle, backend))
    .collect();
    stems.sort();
    stems.dedup();
    stems
}

/// Renders the `REGEDIT4` file `reg import` loads: the crash dialog off,
/// `HKCU\Software\Wine\DllOverrides` set to `native,builtin` for the
/// backend's DLLs and deleted for every other backend's, and the HID toggle
/// at the bundle's `hidraw_key` (its last component is the value name; the
/// key is validated with the bundle, see `Registry::hidraw_value`).
#[must_use]
pub fn render_registry_seeds(
    bundle: &BundleToml,
    backend: GraphicsBackend,
    hidraw: HidrawMode,
) -> String {
    let active = override_stems(bundle, backend);
    let mut out = String::from(
        "REGEDIT4\n\n[HKEY_CURRENT_USER\\Software\\Wine\\WineDbg]\n\"ShowCrashDialog\"=dword:00000000\n\n[HKEY_CURRENT_USER\\Software\\Wine\\DllOverrides]\n",
    );
    for stem in all_stems(bundle) {
        if active.contains(&stem) {
            let _ = writeln!(out, "\"{stem}\"=\"native,builtin\"");
        } else {
            let _ = writeln!(out, "\"{stem}\"=-");
        }
    }
    if let Some((key, value)) = bundle.registry.hidraw_value() {
        let disabled = match hidraw {
            HidrawMode::Enabled => 0,
            HidrawMode::Disabled => 1,
        };
        let _ = write!(out, "\n[{key}]\n\"{value}\"=dword:{disabled:08x}\n");
    }
    out
}

/// The backend's DLLs as `(source in the bundle, destination relative to
/// the prefix)`: `x86_64` into `system32`, `i386` into `syswow64`.
fn planned_overrides(
    installed: &InstalledEngine,
    backend: GraphicsBackend,
) -> Vec<(PathBuf, PathBuf)> {
    let table = override_dlls(&installed.bundle, backend);
    let mut planned = Vec::new();
    for (dlls, target) in [
        (&table.x86_64, "drive_c/windows/system32"),
        (&table.i386, "drive_c/windows/syswow64"),
    ] {
        for dll in dlls {
            if let Some(name) = dll.file_name() {
                planned.push((installed.root.join(dll), Path::new(target).join(name)));
            }
        }
    }
    planned
}

/// Replaces the DLLs an earlier apply copied (`previously`) with the ones
/// `record.backend` needs, keeping `record` on disk in step with the tree:
/// it is written first naming every file that may exist during the swap
/// (the old set and the new), and again naming the new set once the copy is
/// done. Whatever fails afterwards, the next apply knows what to remove.
fn swap_overrides(
    installed: &InstalledEngine,
    prefix_dir: &Path,
    record: &mut PrefixRecord,
    previously: &[PathBuf],
) -> Result<()> {
    let planned = planned_overrides(installed, record.backend);
    let mut during = previously.to_vec();
    for (_, destination) in &planned {
        if !during.contains(destination) {
            during.push(destination.clone());
        }
    }
    record.copied_dlls = during;
    record.write(prefix_dir)?;
    for relative in previously {
        let path = prefix_dir.join(relative);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io(path, error)),
        }
    }
    for (source, relative) in &planned {
        let destination = prefix_dir.join(relative);
        if let Some(dir) = destination.parent() {
            fs::create_dir_all(dir).map_err(|error| Error::io(dir, error))?;
        }
        fs::copy(source, &destination).map_err(|error| Error::io(&destination, error))?;
    }
    record.copied_dlls = planned.into_iter().map(|(_, relative)| relative).collect();
    record.write(prefix_dir)
}

/// Creates the prefix. See [`PREFIX_STEPS`] for the order. When any step
/// fails the prefix's server is stopped (once, here: the steps themselves
/// only kill their own loader process), the directory is removed (or, when
/// the caller supplied an existing empty directory, emptied), and
/// `wineboot`'s log under `<spec.log_dir>/<session>/` survives.
pub(crate) fn create(
    installed: &InstalledEngine,
    prefix_dir: &Path,
    spec: &PrefixSpec,
) -> Result<Prefix> {
    backend_available(installed, spec.backend)?;
    let existed = check_prefix_dir(prefix_dir)?;
    if let Some(library) = &spec.library_dir {
        check_library_dir(library)?;
    }
    let session = new_session_id("prefix");
    let log_dir = spec.log_dir.join(&session);
    fs::create_dir_all(&log_dir).map_err(|error| Error::io(&log_dir, error))?;
    fs::create_dir_all(prefix_dir).map_err(|error| Error::io(prefix_dir, error))?;
    let built = build(installed, prefix_dir, spec, &session, &log_dir);
    if built.is_err() {
        // `-k` needs the directory: Wine keys the server on its inode.
        let _ = kill_prefix_server(installed, prefix_dir, &log_dir.join(WINEBOOT_LOG));
        let _ = if existed {
            clear_dir(prefix_dir)
        } else {
            fs::remove_dir_all(prefix_dir)
        };
    }
    built
}

fn build(
    installed: &InstalledEngine,
    prefix_dir: &Path,
    spec: &PrefixSpec,
    session: &str,
    log_dir: &Path,
) -> Result<Prefix> {
    // No backend rows yet: the DLLs are copied after the boot.
    let rows = engine_rows(
        installed,
        prefix_dir,
        session,
        GraphicsBackend::WineD3D,
        spec.sync_mode,
        false,
    );
    let (env, _dropped) = assemble_env(installed, prefix_dir, &[], &rows)?;
    let log = log_dir.join(WINEBOOT_LOG);
    run_loader(
        installed,
        &["wineboot", "--init"],
        &env,
        prefix_dir,
        &log,
        spec.boot_timeout.unwrap_or(WINEBOOT_TIMEOUT),
    )?;
    wait_for_server(installed, prefix_dir, &log, SERVER_WAIT)?;

    let mut record = PrefixRecord {
        engine: installed.id,
        engine_version: installed.version.clone(),
        backend: spec.backend,
        sync_mode: spec.sync_mode,
        hidraw: spec.hidraw,
        copied_dlls: Vec::new(),
    };
    swap_overrides(installed, prefix_dir, &mut record, &[])?;
    seed_registry(
        installed,
        prefix_dir,
        spec.backend,
        spec.hidraw,
        spec.sync_mode,
        session,
        log_dir,
    )?;
    if let Some(library) = &spec.library_dir {
        link_library_drive(prefix_dir, library)?;
    }
    Ok(record.to_prefix(prefix_dir))
}

/// Re-applies overrides to an existing prefix of this engine. Refused while
/// a server runs for the prefix, before anything moves: the DLLs would
/// change under the session, and a failed seed stops that server.
///
/// The record is rewritten as the DLLs change and before the registry is
/// seeded, so a failed import leaves a record that still names every DLL on
/// disk. `hidraw` and `sync_mode` keep their previous values in it until a
/// seed has fully succeeded. `sync_mode` is not in the registry at all; it
/// selects the environment of the import and of later launches. `hidraw`
/// is, and `reg import` is not atomic, so after a failed import (or a
/// failed final write of the record) the registry can be ahead of the
/// record; the next apply makes them agree. A failed seed also stops the
/// prefix's server, so nothing of the apply outlives the call.
pub(crate) fn apply(
    installed: &InstalledEngine,
    prefix: &Prefix,
    backend: GraphicsBackend,
    sync_mode: SyncMode,
    hidraw: HidrawMode,
) -> Result<Prefix> {
    if prefix.engine != installed.id {
        return Err(Error::InvalidPrefix {
            path: prefix.dir.clone(),
            reason: format!(
                "it belongs to engine {}, not {}",
                prefix.engine, installed.id
            ),
        });
    }
    backend_available(installed, backend)?;
    if super::process::wineserver_alive(&prefix.dir)? {
        return Err(Error::PrefixUnusable {
            path: prefix.dir.clone(),
            reason: "a wineserver is running for it, and overrides are swapped and the registry \
                     re-seeded only in an idle prefix; cancel the session first",
        });
    }
    let previous = PrefixRecord::read(&prefix.dir)?;
    let mut record = PrefixRecord {
        engine: installed.id,
        engine_version: installed.version.clone(),
        backend,
        sync_mode: previous.sync_mode,
        hidraw: previous.hidraw,
        copied_dlls: Vec::new(),
    };
    swap_overrides(installed, &prefix.dir, &mut record, &previous.copied_dlls)?;
    let session = new_session_id("apply");
    let state_dir = prefix.dir.join(STATE_DIR);
    let seeded = seed_registry(
        installed,
        &prefix.dir,
        backend,
        hidraw,
        sync_mode,
        &session,
        &state_dir,
    );
    if let Err(error) = seeded {
        let _ = kill_prefix_server(installed, &prefix.dir, &state_dir.join(IMPORT_LOG));
        return Err(error);
    }
    record.sync_mode = sync_mode;
    record.hidraw = hidraw;
    record.write(&prefix.dir)?;
    Ok(record.to_prefix(&prefix.dir))
}

/// The first line of `reg import`'s output that reports a failure. `reg`
/// exits 1 for a missing or malformed file (where `regedit /S` exits 0),
/// but exits 0 for a well-formed file holding a key it cannot import and
/// only prints `reg: Unable to import the registry key '...'`, so the
/// output of each import is checked for such a line as well.
#[must_use]
pub fn reg_error_line(output: &str) -> Option<&str> {
    output
        .lines()
        .map(str::trim_end)
        .find(|line| line.starts_with("reg: "))
}

/// Writes `seeds.reg` into `work_dir` and imports it with `reg import`
/// from that directory (a relative file name needs no path translation),
/// logging to `work_dir/reg-import.log`. The import fails when `reg` exits
/// non-zero or when this run's part of the log holds a line
/// [`reg_error_line`] recognises; the error carries that line as the
/// status.
fn seed_registry(
    installed: &InstalledEngine,
    prefix_dir: &Path,
    backend: GraphicsBackend,
    hidraw: HidrawMode,
    sync_mode: SyncMode,
    session: &str,
    work_dir: &Path,
) -> Result<()> {
    fs::create_dir_all(work_dir).map_err(|error| Error::io(work_dir, error))?;
    let seeds = work_dir.join(SEEDS_FILE);
    fs::write(
        &seeds,
        render_registry_seeds(&installed.bundle, backend, hidraw),
    )
    .map_err(|error| Error::io(&seeds, error))?;
    let rows = engine_rows(installed, prefix_dir, session, backend, sync_mode, false);
    let (env, _dropped) = assemble_env(installed, prefix_dir, &[], &rows)?;
    let log = work_dir.join(IMPORT_LOG);
    // The log is appended to across applies; only this run's part counts.
    let start = fs::metadata(&log).map_or(0, |meta| meta.len());
    run_loader(
        installed,
        &["reg", "import", SEEDS_FILE],
        &env,
        work_dir,
        &log,
        SERVER_WAIT,
    )?;
    wait_for_server(installed, prefix_dir, &log, SERVER_WAIT)?;
    let output = read_from(&log, start)?;
    if let Some(line) = reg_error_line(&output) {
        return Err(Error::WineCommandFailed {
            command: format!(
                "{} reg import {SEEDS_FILE}",
                installed.root.join(&installed.bundle.wine.loader).display()
            ),
            status: line.to_owned(),
            log,
        });
    }
    Ok(())
}

/// `log` from byte `start` on, lossily decoded.
fn read_from(log: &Path, start: u64) -> Result<String> {
    let mut file = File::open(log).map_err(|error| Error::io(log, error))?;
    file.seek(SeekFrom::Start(start))
        .map_err(|error| Error::io(log, error))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| Error::io(log, error))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The library directory must exist and be absolute: a relative target
/// would make `dosdevices/s:` point inside `dosdevices/`, and a missing one
/// would fail inside Steam instead of here.
fn check_library_dir(library_dir: &Path) -> Result<()> {
    let unusable = |reason| Error::LibraryDirUnusable {
        path: library_dir.to_path_buf(),
        reason,
    };
    if !library_dir.is_absolute() {
        return Err(unusable("the path must be absolute"));
    }
    if !library_dir.is_dir() {
        return Err(unusable("the path is not an existing directory"));
    }
    Ok(())
}

fn link_library_drive(prefix_dir: &Path, library_dir: &Path) -> Result<()> {
    check_library_dir(library_dir)?;
    let link = prefix_dir.join("dosdevices").join(LIBRARY_DRIVE);
    crate::bundle::remove_stale(&link)?;
    symlink(library_dir, &link).map_err(|error| Error::io(&link, error))
}

/// Whether `prefix_dir` can take a new prefix; `Ok(true)` when it is an
/// existing empty directory the caller supplied (which a failed creation
/// empties rather than removes), `Ok(false)` when it does not exist yet.
fn check_prefix_dir(prefix_dir: &Path) -> Result<bool> {
    let unusable = |reason| Error::PrefixUnusable {
        path: prefix_dir.to_path_buf(),
        reason,
    };
    if !prefix_dir.is_absolute() {
        return Err(unusable("the path must be absolute"));
    }
    match fs::symlink_metadata(prefix_dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Error::io(prefix_dir, error)),
        // Wine would key the server on the target's inode, not the link's.
        Ok(meta) if meta.is_symlink() => Err(unusable("the path is a symlink")),
        Ok(meta) if !meta.is_dir() => Err(unusable("something that is not a directory is there")),
        Ok(_) => {
            let mut entries =
                fs::read_dir(prefix_dir).map_err(|error| Error::io(prefix_dir, error))?;
            if entries.next().is_some() {
                Err(unusable("the directory is not empty"))
            } else {
                Ok(true)
            }
        }
    }
}

/// Removes the contents of `dir`, not `dir` itself.
fn clear_dir(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}
