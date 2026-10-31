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

use crate::{
    EngineId, EngineLayout, Error, GraphicsBackend, Progress, Result, SyncMode, Version, Wow64,
};

/// File name of the bundle descriptor at the bundle root.
pub const BUNDLE_FILE: &str = "BUNDLE.toml";

/// The `[wine]` table: where the Wine build came from and how to run it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Wine {
    /// Upstream Wine version the build is based on.
    pub version: String,
    /// Repository the Wine build came from.
    pub source_url: HttpsUrl,
    /// Revision of that repository.
    pub source_rev: String,
    /// Loader executable, relative to the bundle root (`wine/bin/wine64`).
    pub loader: PathBuf,
    /// `wineserver`, relative to the bundle root (`wine/bin/wineserver`).
    pub server: PathBuf,
    /// Preloader, relative to the bundle root, when the build has one.
    #[serde(default)]
    pub preloader: Option<PathBuf>,
    /// How 32-bit Windows code runs.
    pub wow64: Wow64,
}

/// One `[components]` entry: a third-party part of the bundle.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Component {
    /// The component's own version string.
    pub version: String,
    /// SPDX license id.
    pub license: String,
}

/// DLLs a graphics backend copies into the prefix and overrides, one list
/// per Windows architecture.
///
/// Every entry is a path relative to the bundle root (`dxmt/d3d11.dll`)
/// with a `.dll` extension, so the file is found by one rule for all
/// backends and the DLL name for `WINEDLLOVERRIDES` is its file stem. Stems
/// are unique per architecture, ignoring case.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DllOverrides {
    /// 64-bit DLLs, copied into `system32`.
    #[serde(default)]
    pub x86_64: Vec<PathBuf>,
    /// 32-bit DLLs, copied into `syswow64`. Must be empty when
    /// `wine.wow64 = "none"`.
    #[serde(default)]
    pub i386: Vec<PathBuf>,
}

impl DllOverrides {
    /// True when neither architecture lists a DLL.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.x86_64.is_empty() && self.i386.is_empty()
    }
}

/// The `[overrides]` tables, one per backend that needs DLL overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overrides {
    /// DXMT, for DirectX 10 and 11.
    #[serde(default)]
    pub dxmt: DllOverrides,
    /// vkd3d-proton, for DirectX 12.
    #[serde(default)]
    pub vkd3d: DllOverrides,
    /// Bundle-side DLLs for the user-supplied `D3DMetal` payload.
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
    /// x86-64 Mach-O that reports the CPU features Rosetta advertises.
    pub cpuid: PathBuf,
    /// PE that draws one Direct3D 11 frame.
    pub dx11_sample: PathBuf,
    /// PE that enumerates `XInput` controllers.
    pub xinput_check: PathBuf,
}

/// The `[registry]` table: keys whose exact names depend on the pinned Wine.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    /// The `winebus.sys` value that toggles raw HID access, as
    /// `<root>\<key>\...\<value>` (see [`split_registry_value`]).
    pub hidraw_key: String,
}

impl Registry {
    /// `hidraw_key` split into the full key name and the value name; `None`
    /// when it is not a registry value path a seed can import.
    #[must_use]
    pub fn hidraw_value(&self) -> Option<(String, &str)> {
        split_registry_value(&self.hidraw_key)
    }
}

/// The root keys a `[registry]` path may start with, abbreviated and in
/// full. `reg import` exits 0 for a well-formed file whose root it does not
/// know, so an unknown root must be refused before a seed is rendered.
pub const REGISTRY_ROOTS: [(&str, &str); 5] = [
    ("HKLM", "HKEY_LOCAL_MACHINE"),
    ("HKCU", "HKEY_CURRENT_USER"),
    ("HKCR", "HKEY_CLASSES_ROOT"),
    ("HKU", "HKEY_USERS"),
    ("HKCC", "HKEY_CURRENT_CONFIG"),
];

/// Splits `ROOT\Sub\Key\Value` into the full key name (the root expanded
/// to its long form, case-insensitively) and the value name. `None` unless
/// the root is one of [`REGISTRY_ROOTS`] and at least one key component and
/// a value name, none of them blank, follow it. A `"`, a bracket, or a
/// control character is refused because the seed renders the path
/// unescaped: on the real payload `reg import` exits 0 and drops a value
/// whose name holds a `"`, and a newline starts further keys.
#[must_use]
pub fn split_registry_value(spec: &str) -> Option<(String, &str)> {
    if spec
        .chars()
        .any(|c| c.is_control() || matches!(c, '"' | '[' | ']'))
    {
        return None;
    }
    let (key, value) = spec.rsplit_once('\\')?;
    let (root, rest) = key.split_once('\\')?;
    if value.trim().is_empty() || rest.split('\\').any(|part| part.trim().is_empty()) {
        return None;
    }
    let upper = root.to_ascii_uppercase();
    let (_, long) = REGISTRY_ROOTS
        .iter()
        .find(|(short, long)| upper == *short || upper == *long)?;
    Some((format!("{long}\\{rest}"), value))
}

/// The parsed `BUNDLE.toml` of an engine bundle.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleToml {
    /// The engine this bundle implements.
    pub id: EngineId,
    /// The bundle version; names the `engines/<id>/<version>/` directory.
    pub version: Version,
    /// Architecture of the Wine build; must be the one `id` runs.
    pub arch: Arch,
    /// Oldest macOS the bundle runs on, for example `14.0`.
    pub min_macos: String,
    /// The `[wine]` table.
    pub wine: Wine,
    /// Ordered; the first entry is the default. Non-empty, no duplicates.
    pub sync_modes: Vec<SyncMode>,
    /// Third-party parts by name (`dxmt`, `vkd3d_proton`, `moltenvk`, ...).
    #[serde(default)]
    pub components: BTreeMap<String, Component>,
    /// The `[overrides]` tables.
    #[serde(default)]
    pub overrides: Overrides,
    /// The `[d3dmetal]` table; required when `overrides.d3dmetal` lists a DLL.
    #[serde(default)]
    pub d3dmetal: Option<D3DMetal>,
    /// The `[probes]` table.
    #[serde(default)]
    pub probes: Option<Probes>,
    /// The `[registry]` table. Required: without `hidraw_key` no seed
    /// carries the HID toggle, whatever the prefix records.
    pub registry: Registry,
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
    /// - `arch` is the architecture `id` runs (`rosetta-wine` is `x86_64`);
    /// - every path is relative with no `.`, `..`, or empty component;
    /// - `wine.loader` and `wine.server` (and `wine.preloader` when set)
    ///   resolve, through any symlink, to files inside the canonical root,
    ///   as does every `[probes]` executable;
    /// - `sync_modes` is non-empty with no duplicates;
    /// - every override DLL ends in `.dll`, has a stem unique within its
    ///   table and architecture (ignoring case), and resolves to a file
    ///   inside the canonical root; `i386` lists are empty when
    ///   `wine.wow64 = "none"`; a non-empty `[overrides.d3dmetal]` needs a
    ///   `[d3dmetal]` table;
    /// - `entitlements` keys are relative paths;
    /// - `registry.hidraw_key` splits per [`split_registry_value`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::MissingLoader`] for an absent loader,
    /// [`Error::Io`] when `bundle_root` cannot be canonicalized, and
    /// [`Error::InvalidBundle`] for every other failure.
    pub fn validate(&self, bundle_root: &Path) -> Result<()> {
        if self.arch != self.id.arch() {
            return Err(Error::InvalidBundle(format!(
                "arch `{}` does not match engine `{}` (expected `{}`)",
                arch_name(self.arch),
                self.id,
                arch_name(self.id.arch())
            )));
        }
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
        if self.registry.hidraw_value().is_none() {
            return Err(Error::InvalidBundle(format!(
                "registry.hidraw_key: `{}` must be `<root>\\<key>\\...\\<value>` with the root one of HKLM, HKCU, HKCR, HKU, HKCC or its long form, no blank component, and no quote, bracket, or control character",
                self.registry.hidraw_key
            )));
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

        let root = fs::canonicalize(bundle_root).map_err(|error| Error::io(bundle_root, error))?;
        if !resolves_to_file_inside(&root, &self.wine.loader) {
            return Err(Error::MissingLoader {
                path: self.wine.loader.clone(),
            });
        }
        file_present("wine.server", &root, &self.wine.server)?;
        if let Some(preloader) = &self.wine.preloader {
            file_present("wine.preloader", &root, preloader)?;
        }
        if let Some(probes) = &self.probes {
            file_present("probes.cpuid", &root, &probes.cpuid)?;
            file_present("probes.dx11_sample", &root, &probes.dx11_sample)?;
            file_present("probes.xinput_check", &root, &probes.xinput_check)?;
        }

        if !self.overrides.d3dmetal.is_empty() && self.d3dmetal.is_none() {
            return Err(Error::InvalidBundle(
                "overrides.d3dmetal lists DLLs but there is no [d3dmetal] table".to_owned(),
            ));
        }
        for (table, overrides) in [
            ("dxmt", &self.overrides.dxmt),
            ("vkd3d", &self.overrides.vkd3d),
            ("d3dmetal", &self.overrides.d3dmetal),
        ] {
            if !self.wine.wow64.supports_32bit() && !overrides.i386.is_empty() {
                return Err(Error::InvalidBundle(format!(
                    "overrides.{table}.i386 must be empty when wine.wow64 is \"none\""
                )));
            }
            for (arch, dlls) in [("x86_64", &overrides.x86_64), ("i386", &overrides.i386)] {
                let field = format!("overrides.{table}.{arch}");
                validate_dll_list(&field, &root, dlls)?;
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

fn validate_dll_list(field: &str, bundle_root: &Path, dlls: &[PathBuf]) -> Result<()> {
    let mut stems = BTreeSet::new();
    for dll in dlls {
        relative(field, dll)?;
        let is_dll = dll
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("dll"));
        if !is_dll {
            return Err(Error::InvalidBundle(format!(
                "{field}: `{}` must have a `.dll` extension",
                dll.display()
            )));
        }
        let stem = dll
            .file_stem()
            .and_then(OsStr::to_str)
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        if !stems.insert(stem) {
            return Err(Error::InvalidBundle(format!(
                "{field}: `{}` repeats a DLL name already listed",
                dll.display()
            )));
        }
        file_present(field, bundle_root, dll)?;
    }
    Ok(())
}

fn arch_name(arch: Arch) -> &'static str {
    match arch {
        Arch::X86_64 => "x86_64",
        Arch::Arm64 => "arm64",
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

// `root` is canonical and the file is canonicalized too, so a symlink that
// leaves the bundle is caught even though the lexical path is relative.
fn resolves_to_file_inside(root: &Path, path: &Path) -> bool {
    fs::canonicalize(root.join(path)).is_ok_and(|full| full.starts_with(root) && full.is_file())
}

/// What a bundle can do, derived from its `BUNDLE.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// Architecture of the Wine build.
    pub arch: Arch,
    /// Oldest macOS the bundle runs on.
    pub min_macos: String,
    /// Backends the bundle carries overrides for, plus `WineD3D`, which needs
    /// none. `D3DMetal` is listed only when the bundle declares an overlay
    /// location; whether the overlay is populated is a runtime question.
    pub backends: Vec<GraphicsBackend>,
    /// True when 32-bit Windows executables can run.
    pub supports_32bit: bool,
    /// Sync modes the bundle supports, default first.
    pub sync_modes: Vec<SyncMode>,
}

impl Capabilities {
    /// Derives the capabilities from a validated descriptor.
    #[must_use]
    pub fn from_bundle(bundle: &BundleToml) -> Self {
        let mut backends = vec![GraphicsBackend::WineD3D];
        if !bundle.overrides.dxmt.is_empty() {
            backends.insert(0, GraphicsBackend::Dxmt);
        }
        if !bundle.overrides.vkd3d.is_empty() {
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
    /// The engine.
    pub id: EngineId,
    /// The installed version.
    pub version: Version,
    /// `engines/<id>/<version>/`, the signed bundle.
    pub root: PathBuf,
    /// `engines/<id>/<version>.user/`, the per-user overlay. May not exist
    /// yet; the `D3DMetal` payload lives under it.
    pub overlay_root: PathBuf,
    /// The validated descriptor read from `root`.
    pub bundle: BundleToml,
}

impl InstalledEngine {
    /// Reads an already installed bundle back from `layout`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotInstalled`] when the directory is absent, is a
    /// symlink, or holds no `BUNDLE.toml`, else the load error.
    pub fn open(layout: &EngineLayout, id: EngineId, version: &Version) -> Result<Self> {
        let root = layout.engine_dir(id, version);
        if !is_bundle_dir(&root) {
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
            overlay_root: layout.overlay_dir(id, version),
            bundle,
        })
    }
}

/// Where an engine archive comes from. Only local files today; a download
/// variant arrives with the network unit, reporting
/// [`Progress::Downloading`] through the same callback.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ArchiveSource {
    /// A `.tar.zst` already on disk.
    LocalFile(PathBuf),
}

/// Everything [`install`] needs: which bundle, where its archive comes from,
/// and the digest it must hash to. Built only through
/// [`InstallRequest::from_manifest`] (a pinned bundle) or
/// [`InstallRequest::local`] (an unsigned local bundle), so a request that
/// carries a manifest entry always agrees with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRequest {
    id: EngineId,
    version: Version,
    source: ArchiveSource,
    sha256: Sha256,
    manifest: Option<applejack_manifest::Engine>,
}

impl InstallRequest {
    /// The engine to install.
    #[must_use]
    pub const fn id(&self) -> EngineId {
        self.id
    }

    /// The version to install; names the `engines/<id>/<version>/` directory.
    #[must_use]
    pub const fn version(&self) -> &Version {
        &self.version
    }

    /// Where the archive comes from.
    #[must_use]
    pub const fn source(&self) -> &ArchiveSource {
        &self.source
    }

    /// The digest the archive must hash to.
    #[must_use]
    pub const fn sha256(&self) -> &Sha256 {
        &self.sha256
    }

    /// The manifest entry the request came from, when any. Its `id`,
    /// `sha256`, `arch`, `version`, and `min_macos` are cross-checked
    /// against the request and `BUNDLE.toml`.
    #[must_use]
    pub const fn manifest(&self) -> Option<&applejack_manifest::Engine> {
        self.manifest.as_ref()
    }

    /// Builds a request for the bundle a manifest entry pins, taking the
    /// archive from `source`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownEngineId`] or [`Error::InvalidVersion`] when
    /// the entry's `id` or `version` is not one this crate can install.
    pub fn from_manifest(
        entry: &applejack_manifest::Engine,
        source: ArchiveSource,
    ) -> Result<Self> {
        Ok(Self {
            id: entry.id.parse()?,
            version: Version::new(entry.version.clone())?,
            source,
            sha256: entry.sha256.clone(),
            manifest: Some(entry.clone()),
        })
    }

    /// A request for a local archive with no manifest cross-check: the
    /// unsigned-bundle path.
    #[must_use]
    pub fn local(
        id: EngineId,
        version: Version,
        archive: impl Into<PathBuf>,
        sha256: Sha256,
    ) -> Self {
        Self {
            id,
            version,
            source: ArchiveSource::LocalFile(archive.into()),
            sha256,
            manifest: None,
        }
    }
}

/// Installs an engine bundle, reporting [`Progress`] as it goes:
///
/// 1. refuses when `engines/<id>/<version>/` already holds a bundle (a real
///    directory with a regular `BUNDLE.toml`);
/// 2. opens the archive once, hashes it, and refuses on mismatch, leaving
///    the archive as is; nothing on disk has been touched yet;
/// 3. removes a stale `engines/<id>/<version>` without a bundle (a
///    directory, file, or symlink left by an interrupted removal or placed
///    by hand) and a stale `engines/<id>/<version>.partial`, then extracts
///    the same open archive into a fresh `.partial`, refusing any entry
///    that escapes the root or links outside it;
/// 4. loads and validates `BUNDLE.toml`, checking it names `id` and
///    `version` and, when the request came from a manifest entry, that the
///    entry's `id` and `sha256` match the request and its `arch`,
///    `version`, and `min_macos` match the descriptor; on failure removes
///    `.partial`;
/// 5. renames `.partial` to `<version>/`, removing `.partial` if that fails.
///
/// The archive's entries must sit at its root: `BUNDLE.toml` is the first
/// path component, not `<name>/BUNDLE.toml`. No other version directory is
/// read or written.
///
/// # Errors
///
/// [`Error::AlreadyInstalled`], [`Error::Sha256Mismatch`],
/// [`Error::BundleIdentityMismatch`], [`Error::ManifestMismatch`], the
/// validation error, or [`Error::Io`].
pub fn install(
    layout: &EngineLayout,
    request: &InstallRequest,
    progress: &mut dyn FnMut(Progress),
) -> Result<InstalledEngine> {
    let (id, version) = (request.id, &request.version);
    let final_dir = layout.engine_dir(id, version);
    if is_bundle_dir(&final_dir) {
        return Err(Error::AlreadyInstalled {
            id,
            version: version.clone(),
        });
    }
    let ArchiveSource::LocalFile(archive) = &request.source;
    progress(Progress::Verifying);
    // One handle for hashing and extraction so the bytes verified are the
    // bytes unpacked.
    let mut file = File::open(archive).map_err(|error| Error::io(archive, error))?;
    let actual = sha256_reader(&mut file, archive)?;
    if actual != request.sha256 {
        return Err(Error::Sha256Mismatch {
            path: archive.clone(),
            expected: request.sha256.clone(),
            actual,
        });
    }
    file.rewind().map_err(|error| Error::io(archive, error))?;

    // Nothing is removed before the archive verifies, so a request that
    // fails its hash has no side effects.
    remove_stale(&final_dir)?;
    let partial = layout.partial_dir(id, version);
    remove_stale(&partial)?;
    fs::create_dir_all(&partial).map_err(|error| Error::io(&partial, error))?;

    let bundle = match extract_and_validate(file, archive, &partial, request, progress) {
        Ok(bundle) => bundle,
        Err(error) => {
            // Best effort: the error the caller sees is the install failure.
            let _ = fs::remove_dir_all(&partial);
            return Err(error);
        }
    };
    if let Err(error) = fs::rename(&partial, &final_dir) {
        let _ = fs::remove_dir_all(&partial);
        return Err(Error::io(&final_dir, error));
    }
    Ok(InstalledEngine {
        id,
        version: version.clone(),
        root: final_dir,
        overlay_root: layout.overlay_dir(id, version),
        bundle,
    })
}

/// Removes whatever sits at `path`, whether a directory, a file, or a
/// dangling symlink.
pub(crate) fn remove_stale(path: &Path) -> Result<()> {
    let result = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    };
    result.map_err(|error| Error::io(path, error))
}

/// Extracts `file` into `partial` and loads the validated descriptor, so the
/// caller never reads `BUNDLE.toml` a second time after the rename.
fn extract_and_validate(
    file: File,
    archive: &Path,
    partial: &Path,
    request: &InstallRequest,
    progress: &mut dyn FnMut(Progress),
) -> Result<BundleToml> {
    progress(Progress::Extracting);
    let decoder = zstd::stream::read::Decoder::with_buffer(io::BufReader::new(file))
        .map_err(|error| Error::io(archive, error))?;
    let mut tar = tar::Archive::new(decoder);
    tar.set_preserve_permissions(true);
    // The loader must stay executable, but no bundle needs setuid, setgid,
    // or sticky bits.
    tar.set_mask(0o7000);
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
                return Err(Error::InvalidArchive(format!(
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
            return Err(Error::InvalidArchive(format!(
                "archive entry `{}` escapes the bundle root",
                entry_path.display()
            )));
        }
    }

    progress(Progress::Validating);
    links_resolve_inside(partial)?;
    if !is_bundle_dir(partial) {
        return Err(Error::InvalidArchive(
            "archive has no BUNDLE.toml at its root; entries must not be nested under a top-level directory"
                .to_owned(),
        ));
    }
    let bundle = BundleToml::load(partial)?;
    if bundle.id != request.id || bundle.version != request.version {
        return Err(Error::BundleIdentityMismatch {
            expected: request.id,
            expected_version: request.version.clone(),
            found: bundle.id,
            found_version: bundle.version,
        });
    }
    if let Some(entry) = &request.manifest {
        check_against_manifest(request, &bundle, entry)?;
    }
    Ok(bundle)
}

/// True when `dir` is a real directory (not a symlink to one) holding a
/// regular `BUNDLE.toml`: the one definition of an installed `<version>/`
/// shared by [`install`], [`InstalledEngine::open`], and
/// [`crate::versions::list`]. A symlink at `<version>` is never a bundle: it
/// is not listed, does not open, and `install` replaces the link (not its
/// target), which is what a launch would refuse anyway since an engine root
/// may not be a symlink.
pub(crate) fn is_bundle_dir(dir: &Path) -> bool {
    fs::symlink_metadata(dir).is_ok_and(|meta| meta.is_dir())
        && fs::symlink_metadata(dir.join(BUNDLE_FILE)).is_ok_and(|meta| meta.is_file())
}

/// Walks the extracted tree and refuses any symlink that does not resolve to
/// a path inside the canonical `root`.
///
/// The lexical check at extraction time resolves a target against the entry's
/// archive path, which an earlier symlink used as a directory (`p -> .`) can
/// make deeper than the real one. This walk is the authoritative check: it
/// follows each target component by component on disk and requires every
/// intermediate path to stay inside `root`, so a target that climbs out and
/// back in (`a/../<version>.partial/f`, which would point at a sibling once
/// the directory is renamed) is refused along with dangling and looping
/// links. The kernel must agree: a link the walk accepts but `canonicalize`
/// cannot follow (a chain longer than the host allows) is refused too, so
/// an installed bundle never holds a link that does not resolve.
fn links_resolve_inside(root: &Path) -> Result<()> {
    let root = fs::canonicalize(root).map_err(|error| Error::io(root, error))?;
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).map_err(|error| Error::io(&dir, error))? {
            let entry = entry.map_err(|error| Error::io(&dir, error))?;
            let path = entry.path();
            let kind = entry.file_type().map_err(|error| Error::io(&path, error))?;
            if kind.is_symlink() {
                if !link_resolves_inside(&root, &path) || fs::canonicalize(&path).is_err() {
                    return Err(Error::InvalidArchive(format!(
                        "symlink `{}` does not resolve inside the bundle",
                        path.strip_prefix(&root).unwrap_or(&path).display()
                    )));
                }
            } else if kind.is_dir() {
                stack.push(path);
            }
        }
    }
    Ok(())
}

/// Follows the symlink at `link` (whose parent is a real directory under the
/// canonical `root`) one component at a time. Nested symlinks are followed
/// in place, counting every link met in one lookup (`link` itself included)
/// against the macOS limit of 32 (`MAXSYMLINKS`, past which the kernel
/// fails the lookup with `ELOOP`), so a loop terminates and a chain the
/// walk accepts also resolves on the host.
fn link_resolves_inside(root: &Path, link: &Path) -> bool {
    const MAX_LINKS: usize = 32;
    let Some(mut current) = link.parent().map(Path::to_path_buf) else {
        return false;
    };
    let Ok(target) = fs::read_link(link) else {
        return false;
    };
    let mut pending: Vec<PathBuf> = vec![target];
    let mut links = 1;
    while let Some(target) = pending.pop() {
        let mut components = target.components();
        let Some(component) = components.next() else {
            continue;
        };
        let rest = components.as_path().to_path_buf();
        match component {
            PathComponent::CurDir => {}
            PathComponent::ParentDir => {
                let Some(parent) = current.parent() else {
                    return false;
                };
                current = parent.to_path_buf();
            }
            PathComponent::Normal(name) => {
                let next = current.join(name);
                let Ok(meta) = fs::symlink_metadata(&next) else {
                    return false;
                };
                if meta.is_symlink() {
                    links += 1;
                    let Ok(nested) = fs::read_link(&next) else {
                        return false;
                    };
                    if links > MAX_LINKS || nested.is_absolute() {
                        return false;
                    }
                    // Resolve the nested target before the rest of this one.
                    if !rest.as_os_str().is_empty() {
                        pending.push(rest);
                    }
                    pending.push(nested);
                    continue;
                }
                current = next;
            }
            PathComponent::RootDir | PathComponent::Prefix(_) => return false,
        }
        if !current.starts_with(root) {
            return false;
        }
        if !rest.as_os_str().is_empty() {
            pending.push(rest);
        }
    }
    true
}

/// The constructors keep a request consistent with its entry; the `id` and
/// `sha256` checks make that invariant explicit at the one place it is
/// relied on.
fn check_against_manifest(
    request: &InstallRequest,
    bundle: &BundleToml,
    entry: &applejack_manifest::Engine,
) -> Result<()> {
    let mismatch = |field, manifest: &str, bundle: &str| Error::ManifestMismatch {
        field,
        manifest: manifest.to_owned(),
        bundle: bundle.to_owned(),
    };
    if entry.id != request.id.as_str() {
        return Err(mismatch("id", &entry.id, request.id.as_str()));
    }
    if entry.sha256 != request.sha256 {
        return Err(mismatch(
            "sha256",
            entry.sha256.as_str(),
            request.sha256.as_str(),
        ));
    }
    if bundle.arch != entry.arch {
        return Err(mismatch(
            "arch",
            arch_name(entry.arch),
            arch_name(bundle.arch),
        ));
    }
    if bundle.version.as_str() != entry.version {
        return Err(mismatch("version", &entry.version, bundle.version.as_str()));
    }
    if bundle.min_macos != entry.min_macos {
        return Err(mismatch("min_macos", &entry.min_macos, &bundle.min_macos));
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
