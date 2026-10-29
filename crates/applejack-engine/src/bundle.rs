//! `BUNDLE.toml` and bundle installation: SHA-256 verification, extraction
//! into `<version>.partial/`, validation, and the final rename.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read, Seek};
use std::path::{Component as PathComponent, Path, PathBuf};

use applejack_manifest::{Arch, HttpsUrl, Sha256};
use serde::Deserialize;
use sha2::Digest;

use crate::{EngineId, EngineLayout, Error, GraphicsBackend, Result, SyncMode, Version, Wow64};

/// File name of the bundle descriptor at the bundle root.
pub const BUNDLE_FILE: &str = "BUNDLE.toml";

/// The `[wine]` table: where the Wine build came from and how to run it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Wine {
    pub version: String,
    pub source_url: HttpsUrl,
    pub source_rev: String,
    /// Loader executable, relative to the bundle root (`wine/bin/wine64`).
    pub loader: PathBuf,
    /// `wineserver`, relative to the bundle root (`wine/bin/wineserver`).
    pub server: PathBuf,
    /// Preloader, relative to the bundle root, when the build has one.
    #[serde(default)]
    pub preloader: Option<PathBuf>,
    pub wow64: Wow64,
}

/// One `[components]` entry: a third-party part of the bundle.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Component {
    pub version: String,
    /// SPDX license id.
    pub license: String,
}

/// DLLs a graphics backend copies into the prefix and overrides.
///
/// Every entry is a path relative to the bundle root (`dxmt/d3d11.dll`), so
/// the file is found by one rule for all backends and the DLL name for
/// `WINEDLLOVERRIDES` is its file stem.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DllOverrides {
    #[serde(default)]
    pub dlls: Vec<PathBuf>,
}

/// The `[overrides]` tables, one per backend that needs DLL overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overrides {
    #[serde(default)]
    pub dxmt: DllOverrides,
    #[serde(default)]
    pub vkd3d: DllOverrides,
    #[serde(default)]
    pub d3dmetal: DllOverrides,
}

/// The `[d3dmetal]` table: where the user-supplied payload goes inside the
/// per-user overlay.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct D3DMetal {
    /// Relative to the `<version>.user/` overlay root.
    pub overlay_dir: PathBuf,
}

/// The `[probes]` table: diagnostic executables shipped in the bundle,
/// relative to the bundle root.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Probes {
    pub cpuid: PathBuf,
    pub dx11_sample: PathBuf,
    pub xinput_check: PathBuf,
}

/// The `[registry]` table: keys whose exact names depend on the pinned Wine.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    /// The `winebus.sys` key that toggles raw HID access.
    pub hidraw_key: String,
}

/// The parsed `BUNDLE.toml` of an engine bundle.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleToml {
    pub id: EngineId,
    pub version: Version,
    pub arch: Arch,
    pub min_macos: String,
    pub wine: Wine,
    /// Ordered; the first entry is the default. Non-empty, no duplicates.
    pub sync_modes: Vec<SyncMode>,
    #[serde(default)]
    pub components: BTreeMap<String, Component>,
    #[serde(default)]
    pub overrides: Overrides,
    #[serde(default)]
    pub d3dmetal: Option<D3DMetal>,
    #[serde(default)]
    pub probes: Option<Probes>,
    #[serde(default)]
    pub registry: Option<Registry>,
    /// Executable (relative path) to the code-signing entitlements it needs.
    #[serde(default)]
    pub entitlements: BTreeMap<String, Vec<String>>,
    /// Empty until the bundle has been notarized.
    #[serde(default)]
    pub notarization_submission_id: String,
}

impl BundleToml {
    /// Parses `BUNDLE.toml` text. Schema rules (unknown fields, enum names,
    /// version shape) are enforced here; [`BundleToml::validate`] checks the
    /// invariants that need the bundle tree.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Toml`].
    pub fn parse(text: &str) -> Result<Self> {
        Ok(toml::from_str(text)?)
    }

    /// Reads, parses, and validates `<bundle_root>/BUNDLE.toml`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the file is unreadable, else the parse or
    /// validation error.
    pub fn load(bundle_root: &Path) -> Result<Self> {
        let path = bundle_root.join(BUNDLE_FILE);
        let text = fs::read_to_string(&path).map_err(|error| Error::io(path, error))?;
        let bundle = Self::parse(&text)?;
        bundle.validate(bundle_root)?;
        Ok(bundle)
    }

    /// Checks the invariants against the bundle tree at `bundle_root`:
    ///
    /// - every path is relative with no `.`, `..`, or empty component;
    /// - `wine.loader` and `wine.server` (and `wine.preloader` when set)
    ///   resolve, through any symlink, to files inside the canonical root;
    /// - `sync_modes` is non-empty with no duplicates;
    /// - every override DLL resolves to a file inside the canonical root;
    /// - `entitlements` keys are relative paths.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MissingLoader`] for an absent loader and
    /// [`Error::InvalidBundle`] for every other failure.
    pub fn validate(&self, bundle_root: &Path) -> Result<()> {
        relative("wine.loader", &self.wine.loader)?;
        relative("wine.server", &self.wine.server)?;
        if let Some(preloader) = &self.wine.preloader {
            relative("wine.preloader", preloader)?;
        }
        if let Some(d3dmetal) = &self.d3dmetal {
            relative("d3dmetal.overlay_dir", &d3dmetal.overlay_dir)?;
        }
        if let Some(probes) = &self.probes {
            relative("probes.cpuid", &probes.cpuid)?;
            relative("probes.dx11_sample", &probes.dx11_sample)?;
            relative("probes.xinput_check", &probes.xinput_check)?;
        }
        for executable in self.entitlements.keys() {
            relative("entitlements", Path::new(executable))?;
        }

        if self.sync_modes.is_empty() {
            return Err(Error::InvalidBundle(
                "sync_modes must list at least one mode".to_owned(),
            ));
        }
        let mut seen = BTreeSet::new();
        for mode in &self.sync_modes {
            if !seen.insert(mode.as_str()) {
                return Err(Error::InvalidBundle(format!(
                    "sync_modes lists `{mode}` more than once"
                )));
            }
        }

        if !resolves_to_file_inside(bundle_root, &self.wine.loader) {
            return Err(Error::MissingLoader {
                path: self.wine.loader.clone(),
            });
        }
        file_present("wine.server", bundle_root, &self.wine.server)?;
        if let Some(preloader) = &self.wine.preloader {
            file_present("wine.preloader", bundle_root, preloader)?;
        }

        for (table, overrides) in [
            ("dxmt", &self.overrides.dxmt),
            ("vkd3d", &self.overrides.vkd3d),
            ("d3dmetal", &self.overrides.d3dmetal),
        ] {
            let field = format!("overrides.{table}.dlls");
            for dll in &overrides.dlls {
                relative(&field, dll)?;
                file_present(&field, bundle_root, dll)?;
            }
        }
        Ok(())
    }

    /// The default sync mode: the first entry of `sync_modes`.
    #[must_use]
    pub fn default_sync_mode(&self) -> Option<SyncMode> {
        self.sync_modes.first().copied()
    }
}

fn relative(field: &str, path: &Path) -> Result<()> {
    let ok = !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, PathComponent::Normal(_)));
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidBundle(format!(
            "{field}: `{}` must be a relative path without `.` or `..`",
            path.display()
        )))
    }
}

fn file_present(field: &str, root: &Path, path: &Path) -> Result<()> {
    if resolves_to_file_inside(root, path) {
        Ok(())
    } else {
        Err(Error::InvalidBundle(format!(
            "{field}: `{}` is not a file inside the bundle",
            path.display()
        )))
    }
}

// Canonical on both sides so a symlink that leaves the bundle is caught even
// though the lexical path is relative.
fn resolves_to_file_inside(root: &Path, path: &Path) -> bool {
    let (Ok(root), Ok(full)) = (fs::canonicalize(root), fs::canonicalize(root.join(path))) else {
        return false;
    };
    full.starts_with(&root) && full.is_file()
}

/// What a bundle can do, derived from its `BUNDLE.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub arch: Arch,
    pub min_macos: String,
    /// Backends the bundle carries overrides for, plus `WineD3D`, which needs
    /// none. `D3DMetal` is listed only when the bundle declares an overlay
    /// location; whether the overlay is populated is a runtime question.
    pub backends: Vec<GraphicsBackend>,
    pub supports_32bit: bool,
    pub sync_modes: Vec<SyncMode>,
}

impl Capabilities {
    #[must_use]
    pub fn from_bundle(bundle: &BundleToml) -> Self {
        let mut backends = vec![GraphicsBackend::WineD3D];
        if !bundle.overrides.dxmt.dlls.is_empty() {
            backends.insert(0, GraphicsBackend::Dxmt);
        }
        if !bundle.overrides.vkd3d.dlls.is_empty() {
            backends.push(GraphicsBackend::Vkd3dProton);
        }
        if bundle.d3dmetal.is_some() {
            backends.push(GraphicsBackend::D3DMetal);
        }
        Self {
            arch: bundle.arch,
            min_macos: bundle.min_macos.clone(),
            backends,
            supports_32bit: bundle.wine.wow64.supports_32bit(),
            sync_modes: bundle.sync_modes.clone(),
        }
    }
}

/// An engine bundle installed under `engines/<id>/<version>/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledEngine {
    pub id: EngineId,
    pub version: Version,
    pub root: PathBuf,
    pub bundle: BundleToml,
}

impl InstalledEngine {
    /// Reads an already installed bundle back from `layout`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotInstalled`] when the directory is absent, else the
    /// load error.
    pub fn open(layout: &EngineLayout, id: EngineId, version: &Version) -> Result<Self> {
        let root = layout.engine_dir(id, version);
        if !root.is_dir() {
            return Err(Error::NotInstalled {
                id,
                version: version.clone(),
            });
        }
        let bundle = BundleToml::load(&root)?;
        Ok(Self {
            id,
            version: version.clone(),
            root,
            bundle,
        })
    }
}

/// Where an engine archive comes from. Only local files today; a download
/// variant arrives with the network unit without changing [`install`]
/// callers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ArchiveSource {
    /// A `.tar.zst` already on disk.
    LocalFile(PathBuf),
}

/// Installs a local `.tar.zst` archive as `engines/<id>/<version>/`.
/// See [`install`].
///
/// # Errors
///
/// As [`install`].
pub fn install_from_archive(
    layout: &EngineLayout,
    id: EngineId,
    version: &Version,
    archive: &Path,
    expected: &Sha256,
) -> Result<InstalledEngine> {
    install(
        layout,
        id,
        version,
        &ArchiveSource::LocalFile(archive.to_path_buf()),
        expected,
    )
}

/// Installs an engine bundle:
///
/// 1. refuses when `engines/<id>/<version>/` already exists;
/// 2. opens the archive once, hashes it, and refuses on mismatch, leaving
///    the archive as is;
/// 3. replaces a stale `engines/<id>/<version>.partial` (file or directory)
///    and extracts the same open archive into it, refusing any entry that
///    escapes the root or links outside it;
/// 4. loads and validates `BUNDLE.toml`, checking it names `id` and
///    `version`; on failure removes `.partial`;
/// 5. renames `.partial` to `<version>/`, removing `.partial` if that fails.
///
/// The archive's entries must sit at its root: `BUNDLE.toml` is the first
/// path component, not `<name>/BUNDLE.toml`. No other version directory is
/// read or written.
///
/// # Errors
///
/// [`Error::AlreadyInstalled`], [`Error::Sha256Mismatch`],
/// [`Error::BundleIdentityMismatch`], the validation error, or
/// [`Error::Io`].
pub fn install(
    layout: &EngineLayout,
    id: EngineId,
    version: &Version,
    source: &ArchiveSource,
    expected: &Sha256,
) -> Result<InstalledEngine> {
    let final_dir = layout.engine_dir(id, version);
    if final_dir.exists() {
        return Err(Error::AlreadyInstalled {
            id,
            version: version.clone(),
        });
    }
    let ArchiveSource::LocalFile(archive) = source;
    // One handle for hashing and extraction so the bytes verified are the
    // bytes unpacked.
    let mut file = File::open(archive).map_err(|error| Error::io(archive, error))?;
    let actual = sha256_reader(&mut file, archive)?;
    if &actual != expected {
        return Err(Error::Sha256Mismatch {
            expected: expected.clone(),
            actual,
        });
    }
    file.rewind().map_err(|error| Error::io(archive, error))?;

    let partial = layout.partial_dir(id, version);
    remove_stale(&partial)?;
    fs::create_dir_all(&partial).map_err(|error| Error::io(&partial, error))?;

    let result = extract_and_validate(file, archive, &partial, id, version);
    if let Err(error) = result {
        // Best effort: the error the caller sees is the install failure.
        let _ = fs::remove_dir_all(&partial);
        return Err(error);
    }
    if let Err(error) = fs::rename(&partial, &final_dir) {
        let _ = fs::remove_dir_all(&partial);
        return Err(Error::io(&final_dir, error));
    }
    InstalledEngine::open(layout, id, version)
}

/// Removes whatever sits at `path`, whether a directory, a file, or a
/// dangling symlink.
fn remove_stale(path: &Path) -> Result<()> {
    let result = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    };
    result.map_err(|error| Error::io(path, error))
}

fn extract_and_validate(
    file: File,
    archive: &Path,
    partial: &Path,
    id: EngineId,
    version: &Version,
) -> Result<()> {
    let decoder = zstd::stream::read::Decoder::new(io::BufReader::new(file))
        .map_err(|error| Error::io(archive, error))?;
    let mut tar = tar::Archive::new(decoder);
    tar.set_preserve_permissions(true);
    tar.set_overwrite(false);
    for entry in tar.entries().map_err(|error| Error::io(archive, error))? {
        let mut entry = entry.map_err(|error| Error::io(archive, error))?;
        let entry_path = entry
            .path()
            .map_err(|error| Error::io(archive, error))?
            .into_owned();
        let kind = entry.header().entry_type();
        if kind.is_symlink() || kind.is_hard_link() {
            let target = entry
                .link_name()
                .map_err(|error| Error::io(archive, error))?
                .map(std::borrow::Cow::into_owned)
                .unwrap_or_default();
            if !link_stays_inside(&entry_path, &target, kind.is_hard_link()) {
                return Err(Error::InvalidBundle(format!(
                    "archive entry `{}` links to `{}` outside the bundle",
                    entry_path.display(),
                    target.display()
                )));
            }
        }
        // `unpack_in` reports an entry whose own path would leave `partial`
        // by returning false instead of failing.
        let unpacked = entry
            .unpack_in(partial)
            .map_err(|error| Error::io(archive, error))?;
        if !unpacked {
            return Err(Error::InvalidBundle(format!(
                "archive entry `{}` escapes the bundle root",
                entry_path.display()
            )));
        }
    }

    if !fs::symlink_metadata(partial.join(BUNDLE_FILE)).is_ok_and(|meta| meta.is_file()) {
        return Err(Error::InvalidBundle(
            "archive has no BUNDLE.toml at its root; entries must not be nested under a top-level directory"
                .to_owned(),
        ));
    }
    let bundle = BundleToml::load(partial)?;
    if bundle.id != id || &bundle.version != version {
        return Err(Error::BundleIdentityMismatch {
            expected: id,
            expected_version: version.clone(),
            found: bundle.id,
            found_version: bundle.version,
        });
    }
    Ok(())
}

/// Lexically resolves a link target and reports whether it stays under the
/// archive root. A hard-link target is root-relative; a symlink target is
/// relative to the entry's own directory.
fn link_stays_inside(entry: &Path, target: &Path, hard_link: bool) -> bool {
    if target.is_absolute() {
        return false;
    }
    let mut depth: Vec<&OsStr> = Vec::new();
    if !hard_link && let Some(parent) = entry.parent() {
        for component in parent.components() {
            match component {
                PathComponent::Normal(name) => depth.push(name),
                PathComponent::CurDir => {}
                _ => return false,
            }
        }
    }
    for component in target.components() {
        match component {
            PathComponent::Normal(name) => depth.push(name),
            PathComponent::CurDir => {}
            PathComponent::ParentDir => {
                if depth.pop().is_none() {
                    return false;
                }
            }
            PathComponent::RootDir | PathComponent::Prefix(_) => return false,
        }
    }
    true
}

/// SHA-256 of a file's contents as lowercase hex.
///
/// # Errors
///
/// Returns [`Error::Io`].
pub fn sha256_file(path: &Path) -> Result<Sha256> {
    let mut file = File::open(path).map_err(|error| Error::io(path, error))?;
    sha256_reader(&mut file, path)
}

/// SHA-256 of everything `reader` yields; `path` names it in errors.
fn sha256_reader(reader: &mut impl Read, path: &Path) -> Result<Sha256> {
    let mut hasher = sha2::Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| Error::io(path, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    let hex = digest
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            use std::fmt::Write;
            let _ = write!(hex, "{byte:02x}");
            hex
        });
    Sha256::try_from(hex).map_err(Error::Manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_targets_are_resolved_against_the_entry_directory() {
        let p = Path::new;
        assert!(link_stays_inside(
            p("wine/bin/wine64"),
            p("../lib/wine64"),
            false
        ));
        assert!(link_stays_inside(
            p("./wine/bin/wine64"),
            p("../../lib/x"),
            false
        ));
        assert!(!link_stays_inside(
            p("wine/bin/wine64"),
            p("../../../etc"),
            false
        ));
        assert!(!link_stays_inside(
            p("wine/bin/wine64"),
            p("/usr/bin/true"),
            false
        ));
        assert!(link_stays_inside(p("a/b"), p("c/d"), true));
        assert!(!link_stays_inside(p("a/b"), p("../x"), true));
        assert!(!link_stays_inside(p("a/b"), p("/etc/hosts"), true));
    }
}
