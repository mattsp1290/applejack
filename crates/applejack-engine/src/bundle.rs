//! `BUNDLE.toml` and bundle installation: SHA-256 verification, extraction
//! into `<version>.partial/`, validation, and the final rename.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read};
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
    /// Loader executable, relative to the bundle root (`bin/wine64`).
    pub loader: PathBuf,
    /// `wineserver`, relative to the bundle root.
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
    /// - `wine.loader` and `wine.server` (and `wine.preloader` when set) are
    ///   files under the root;
    /// - `sync_modes` is non-empty with no duplicates;
    /// - every override DLL is a file under the root;
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

        if !bundle_root.join(&self.wine.loader).is_file() {
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
    if root.join(path).is_file() {
        Ok(())
    } else {
        Err(Error::InvalidBundle(format!(
            "{field}: `{}` is not a file in the bundle",
            path.display()
        )))
    }
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
/// 2. hashes the archive and refuses on mismatch, leaving the archive as is;
/// 3. extracts into `engines/<id>/<version>.partial/` (a stale one is
///    replaced);
/// 4. loads and validates `BUNDLE.toml`, checking it names `id` and
///    `version`; on failure removes `.partial`;
/// 5. renames `.partial` to `<version>/`.
///
/// No other version directory is read or written.
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
    let actual = sha256_file(archive)?;
    if &actual != expected {
        return Err(Error::Sha256Mismatch {
            expected: expected.clone(),
            actual,
        });
    }

    let partial = layout.partial_dir(id, version);
    if partial.exists() {
        fs::remove_dir_all(&partial).map_err(|error| Error::io(&partial, error))?;
    }
    fs::create_dir_all(&partial).map_err(|error| Error::io(&partial, error))?;

    let result = extract_and_validate(archive, &partial, id, version);
    if let Err(error) = result {
        // Best effort: the error the caller sees is the install failure.
        let _ = fs::remove_dir_all(&partial);
        return Err(error);
    }
    fs::rename(&partial, &final_dir).map_err(|error| Error::io(&final_dir, error))?;
    InstalledEngine::open(layout, id, version)
}

fn extract_and_validate(
    archive: &Path,
    partial: &Path,
    id: EngineId,
    version: &Version,
) -> Result<()> {
    let file = File::open(archive).map_err(|error| Error::io(archive, error))?;
    let decoder = zstd::stream::read::Decoder::new(io::BufReader::new(file))
        .map_err(|error| Error::io(archive, error))?;
    let mut tar = tar::Archive::new(decoder);
    tar.set_preserve_permissions(true);
    tar.set_overwrite(false);
    // `tar` refuses entries that would escape `partial`.
    tar.unpack(partial)
        .map_err(|error| Error::io(archive, error))?;

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

/// SHA-256 of a file's contents as lowercase hex.
///
/// # Errors
///
/// Returns [`Error::Io`].
pub fn sha256_file(path: &Path) -> Result<Sha256> {
    let mut file = File::open(path).map_err(|error| Error::io(path, error))?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file
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
