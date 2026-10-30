//! Engine abstraction: the [`Engine`] trait, the on-disk engine bundle layout
//! and `BUNDLE.toml`, engine install and rollback, and the launch environment
//! allowlist. Accepts and returns plain structs and never depends on
//! `applejack-config`.
//!
//! This crate holds the domain model and its first implementation,
//! [`RosettaWine`] (prefix creation, process spawning, process discovery,
//! `D3DMetal` overlay, CPUID probe, snapshots). See
//! `docs/engines/rosetta-wine.md`.

#![warn(missing_docs)]

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub mod bundle;
mod error;
pub mod launch;
pub mod manifest;
pub mod rosetta_wine;
pub mod versions;

pub use applejack_manifest::{Arch, Sha256};
pub use bundle::{ArchiveSource, BundleToml, Capabilities, InstallRequest, InstalledEngine};
pub use error::{Error, Result};
pub use launch::{LaunchSpec, Session};
pub use manifest::embedded_manifest;
pub use rosetta_wine::{CpuidProbe, RosettaWine};
pub use versions::Version;

/// Which engine a bundle, prefix, or configuration refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EngineId {
    /// x86-64 Wine under Rosetta 2.
    RosettaWine,
    /// Native arm64 engine (gated track).
    NativeArm64,
}

impl EngineId {
    /// The directory and manifest name of the engine.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RosettaWine => "rosetta-wine",
            Self::NativeArm64 => "native-arm64",
        }
    }

    /// The architecture a bundle of this engine must declare.
    #[must_use]
    pub const fn arch(self) -> Arch {
        match self {
            Self::RosettaWine => Arch::X86_64,
            Self::NativeArm64 => Arch::Arm64,
        }
    }
}

impl fmt::Display for EngineId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EngineId {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "rosetta-wine" => Ok(Self::RosettaWine),
            "native-arm64" => Ok(Self::NativeArm64),
            other => Err(Error::UnknownEngineId(other.to_owned())),
        }
    }
}

/// Direct3D translation layer applied to a prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GraphicsBackend {
    /// DXMT for DirectX 10 and 11 (default for those).
    Dxmt,
    /// Wine's built-in Direct3D over OpenGL; no DLL overrides.
    #[serde(rename = "wined3d")]
    WineD3D,
    /// vkd3d-proton for DirectX 12 over a Vulkan-on-Metal driver.
    Vkd3dProton,
    /// Apple's `D3DMetal`, user-supplied through the per-user overlay.
    #[serde(rename = "d3dmetal")]
    D3DMetal,
}

impl GraphicsBackend {
    /// The name used in `BUNDLE.toml`, the config database, and on the
    /// command line.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dxmt => "dxmt",
            Self::WineD3D => "wined3d",
            Self::Vkd3dProton => "vkd3d-proton",
            Self::D3DMetal => "d3dmetal",
        }
    }
}

impl fmt::Display for GraphicsBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for GraphicsBackend {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "dxmt" => Ok(Self::Dxmt),
            "wined3d" => Ok(Self::WineD3D),
            "vkd3d-proton" => Ok(Self::Vkd3dProton),
            "d3dmetal" => Ok(Self::D3DMetal),
            other => Err(Error::UnknownGraphicsBackend(other.to_owned())),
        }
    }
}

/// Wine synchronization primitive implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum SyncMode {
    /// `WINEMSYNC=1`.
    Msync,
    /// `WINEESYNC=1`.
    Esync,
    /// Neither variable set.
    None,
}

impl SyncMode {
    /// The name used in `BUNDLE.toml` and on the command line.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Msync => "msync",
            Self::Esync => "esync",
            Self::None => "none",
        }
    }
}

impl fmt::Display for SyncMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SyncMode {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "msync" => Ok(Self::Msync),
            "esync" => Ok(Self::Esync),
            "none" => Ok(Self::None),
            other => Err(Error::UnknownSyncMode(other.to_owned())),
        }
    }
}

impl TryFrom<String> for SyncMode {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        value.parse()
    }
}

impl From<SyncMode> for String {
    fn from(mode: SyncMode) -> Self {
        mode.as_str().to_owned()
    }
}

/// Whether Wine's `winebus.sys` exposes raw HID devices to the prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HidrawMode {
    /// Games see raw HID devices through `winebus.sys`.
    Enabled,
    /// Raw HID access is disabled in the prefix registry.
    Disabled,
}

impl HidrawMode {
    /// The name used in the config database and on the command line.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
        }
    }
}

impl fmt::Display for HidrawMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for HidrawMode {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "enabled" => Ok(Self::Enabled),
            "disabled" => Ok(Self::Disabled),
            other => Err(Error::UnknownHidrawMode(other.to_owned())),
        }
    }
}

/// How a bundle's Wine runs 32-bit Windows code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Wow64 {
    /// Upstream "new WoW64": 32-bit PE on a 64-bit Unix side.
    #[serde(rename = "new")]
    New,
    /// CrossOver-style `x86_32on64-unix`.
    #[serde(rename = "32on64")]
    ThirtyTwoOn64,
    /// 64-bit only.
    #[serde(rename = "none")]
    None,
}

impl Wow64 {
    /// True when 32-bit Windows executables can run in this bundle.
    #[must_use]
    pub const fn supports_32bit(self) -> bool {
        matches!(self, Self::New | Self::ThirtyTwoOn64)
    }
}

/// Where engines live under the launcher's data directory.
///
/// ```text
/// <data_dir>/engines/<id>/<version>/          installed bundle
/// <data_dir>/engines/<id>/<version>.partial/  extraction in progress
/// <data_dir>/engines/<id>/<version>.user/     per-user overlay (D3DMetal)
/// <data_dir>/engines/<id>/<version>.download  archive being downloaded
/// ```
///
/// The active version is not recorded here; the core owns it and passes it
/// to [`versions::prune`] and [`versions::remove`] as a protected version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineLayout {
    data_dir: PathBuf,
}

impl EngineLayout {
    /// Directory name suffix of an extraction in progress.
    pub const PARTIAL_SUFFIX: &'static str = ".partial";
    /// Directory name suffix of a per-user overlay.
    pub const OVERLAY_SUFFIX: &'static str = ".user";
    /// File name suffix of an archive being downloaded.
    pub const DOWNLOAD_SUFFIX: &'static str = ".download";

    /// A layout rooted at the launcher's data directory.
    #[must_use]
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }

    /// The launcher's data directory.
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// `<data_dir>/engines/<id>`.
    #[must_use]
    pub fn engine_root(&self, id: EngineId) -> PathBuf {
        self.data_dir.join("engines").join(id.as_str())
    }

    /// `<data_dir>/engines/<id>/<version>`.
    #[must_use]
    pub fn engine_dir(&self, id: EngineId, version: &Version) -> PathBuf {
        self.engine_root(id).join(version.as_str())
    }

    /// `<data_dir>/engines/<id>/<version>.partial`.
    #[must_use]
    pub fn partial_dir(&self, id: EngineId, version: &Version) -> PathBuf {
        self.engine_root(id)
            .join(format!("{version}{}", Self::PARTIAL_SUFFIX))
    }

    /// `<data_dir>/engines/<id>/<version>.user`.
    #[must_use]
    pub fn overlay_dir(&self, id: EngineId, version: &Version) -> PathBuf {
        self.engine_root(id)
            .join(format!("{version}{}", Self::OVERLAY_SUFFIX))
    }

    /// `<data_dir>/engines/<id>/<version>.download`: where a downloaded
    /// archive lands. It sits outside `<version>.partial/`, so a download
    /// whose digest does not match is kept for inspection while the
    /// extraction directory is discarded.
    #[must_use]
    pub fn download_path(&self, id: EngineId, version: &Version) -> PathBuf {
        self.engine_root(id)
            .join(format!("{version}{}", Self::DOWNLOAD_SUFFIX))
    }
}

/// Facts about the machine, gathered by the core and handed to
/// [`Engine::doctor`] and [`Engine::install`]. Read-only input; the engine
/// never probes the host itself, so there is one source of truth for what
/// `doctor` reported and what `install` refused on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    /// `ProductVersion` from `sw_vers`, for example `15.1`.
    pub macos_version: String,
    /// Native CPU architecture of the host.
    pub arch: Arch,
    /// Whether Rosetta 2 is installed; `None` when the core could not tell.
    pub rosetta_present: Option<bool>,
}

/// Severity of one [`DoctorReport`] finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Informational; does not affect whether the engine runs.
    Info,
    /// The engine runs, with a caveat worth showing.
    Warning,
    /// The engine cannot run until this is fixed.
    Error,
}

/// One line of a [`DoctorReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// How serious the finding is.
    pub severity: Severity,
    /// What was found, printable as is.
    pub message: String,
}

/// Result of [`Engine::doctor`]: whether the engine can run on this host and
/// why not when it cannot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorReport {
    /// False when any finding has [`Severity::Error`].
    pub runnable: bool,
    /// Every finding, in the order checked.
    pub findings: Vec<Finding>,
}

/// What [`Engine::create_prefix`] applies to a fresh prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixSpec {
    /// Backend whose DLL overrides go into the prefix.
    pub backend: GraphicsBackend,
    /// Sync mode recorded for launches from the prefix.
    pub sync_mode: SyncMode,
    /// Whether raw HID access is enabled in the prefix registry.
    pub hidraw: HidrawMode,
    /// Shared game library mounted as drive `S:`; `None` leaves the drive
    /// unmapped.
    pub library_dir: Option<PathBuf>,
    /// Log root. Prefix creation is a session like any launch: its
    /// `wineboot` and `reg import` output goes to `<log_dir>/<session-id>/`,
    /// outside the prefix, so the log survives the prefix being removed on
    /// failure.
    pub log_dir: PathBuf,
    /// How long the engine's boot step (`wineboot --init` for
    /// `rosetta-wine`) may run; `None` is the engine default
    /// (`rosetta_wine::prefix::WINEBOOT_TIMEOUT`, 300 s). On expiry the
    /// boot process and the prefix's server are killed, the prefix is
    /// removed, and the error names the log.
    pub boot_timeout: Option<Duration>,
}

/// A Wine prefix created by an engine, with the configuration last applied
/// to it. The sync mode and backend are per-process choices that Wine does
/// not persist, so [`Engine::launch`] derives `WINEMSYNC`/`WINEESYNC`,
/// `WINEDLLOVERRIDES`, and `VK_ICD_FILENAMES` from the prefix it is handed
/// rather than from a second source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prefix {
    /// The prefix directory (`WINEPREFIX`).
    pub dir: PathBuf,
    /// Engine that created the prefix.
    pub engine: EngineId,
    /// Engine version that last touched the prefix.
    pub engine_version: Version,
    /// Backend whose DLL overrides [`Engine::create_prefix`] or the last
    /// [`Engine::apply_overrides`] applied.
    pub backend: GraphicsBackend,
    /// Sync mode applied with it.
    pub sync_mode: SyncMode,
    /// HID toggle applied with it.
    pub hidraw: HidrawMode,
}

/// Reported by [`Engine::install`] as work proceeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Progress {
    /// Bytes of the archive received so far; `total` when the server said.
    Downloading {
        /// Bytes received.
        received: u64,
        /// Content length, when known.
        total: Option<u64>,
    },
    /// Hashing the archive against the pinned digest.
    Verifying,
    /// Unpacking into `<version>.partial/`.
    Extracting,
    /// Checking links and `BUNDLE.toml` before the rename.
    Validating,
}

/// The operations the launcher programs against. Each engine bundle is
/// described by its `BUNDLE.toml`; [`RosettaWine`] is the first
/// implementation.
pub trait Engine {
    /// The engine's identity.
    fn id(&self) -> EngineId;

    /// What the bundle can do, derived from its `BUNDLE.toml`. The default
    /// is [`Capabilities::from_bundle`]; an engine overrides it only when
    /// its capabilities depend on more than the descriptor.
    fn capabilities(&self, bundle: &BundleToml) -> Capabilities {
        Capabilities::from_bundle(bundle)
    }

    /// Read-only, network-free check of the host and an installed bundle.
    fn doctor(&self, host: &Host, installed: Option<&InstalledEngine>) -> DoctorReport;

    /// Fetches, verifies, extracts, and validates the requested bundle into
    /// `layout`, after the engine's own checks against `host`. Same shape as
    /// [`bundle::install`], which does the bundle part. Writes only
    /// `engines/<id>/<version>.download` and under
    /// `engines/<id>/<version>.partial/` until the final rename, apart from
    /// removing a stale `engines/<id>/<version>` that holds no bundle once
    /// the archive has verified, and never touches another version.
    ///
    /// `rosetta-wine` refuses with [`Error::RosettaMissing`] when
    /// `host.rosetta_present == Some(false)` and proceeds when it is `None`:
    /// `doctor` is where uncertainty about the host is reported, and an
    /// install must not be blocked by a probe the core could not run.
    ///
    /// # Errors
    ///
    /// Returns the first failure; a failed install leaves no `<version>/`.
    fn install(
        &self,
        host: &Host,
        layout: &EngineLayout,
        request: &InstallRequest,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<InstalledEngine>;

    /// Creates a prefix at `prefix_dir` and applies `spec`. `prefix_dir`
    /// must be absolute and either absent or an empty directory.
    ///
    /// # Errors
    ///
    /// Returns the failure; the prefix directory is removed on error.
    fn create_prefix(
        &self,
        installed: &InstalledEngine,
        prefix_dir: &Path,
        spec: &PrefixSpec,
    ) -> Result<Prefix>;

    /// Re-applies DLL overrides from `installed`, the sync mode, and the HID
    /// toggle to a prefix, and returns the prefix with those values
    /// recorded so the caller holds the current state without re-reading
    /// the prefix.
    ///
    /// # Errors
    ///
    /// Returns the failure, including a refusal when `backend` is not
    /// available for the installed bundle or `prefix` belongs to another
    /// engine.
    fn apply_overrides(
        &self,
        installed: &InstalledEngine,
        prefix: &Prefix,
        backend: GraphicsBackend,
        sync_mode: SyncMode,
        hidraw: HidrawMode,
    ) -> Result<Prefix>;

    /// Spawns the loader of `installed` with the allowlisted environment.
    /// The engine-set tier carries `WINEPREFIX` and `APPLEJACK_SESSION` and
    /// derives `WINEMSYNC`/`WINEESYNC`, `WINEDLLOVERRIDES`, and
    /// `VK_ICD_FILENAMES` from `prefix.sync_mode` and `prefix.backend`.
    ///
    /// # Errors
    ///
    /// Returns the spawn failure, or [`Error::MissingEngineVariable`] from
    /// [`launch::env::assemble`].
    fn launch(
        &self,
        installed: &InstalledEngine,
        prefix: &Prefix,
        spec: &LaunchSpec,
    ) -> Result<Session>;
}

/// An environment variable pair as handed to a child process.
pub type EnvPair = (OsString, OsString);

#[cfg(test)]
mod tests {
    use super::*;

    fn name<T: Serialize>(value: T) -> String {
        #[derive(Serialize)]
        struct Wrap<T> {
            v: T,
        }
        toml::to_string(&Wrap { v: value })
            .unwrap()
            .trim()
            .trim_start_matches("v = ")
            .to_owned()
    }

    #[test]
    fn serde_names_match_the_bundle_schema() {
        assert_eq!(name(EngineId::RosettaWine), "\"rosetta-wine\"");
        assert_eq!(name(EngineId::NativeArm64), "\"native-arm64\"");
        assert_eq!(name(GraphicsBackend::Dxmt), "\"dxmt\"");
        assert_eq!(name(GraphicsBackend::WineD3D), "\"wined3d\"");
        assert_eq!(name(GraphicsBackend::Vkd3dProton), "\"vkd3d-proton\"");
        assert_eq!(name(GraphicsBackend::D3DMetal), "\"d3dmetal\"");
        assert_eq!(name(SyncMode::Msync), "\"msync\"");
        assert_eq!(name(Wow64::New), "\"new\"");
        assert_eq!(name(Wow64::ThirtyTwoOn64), "\"32on64\"");
        assert_eq!(name(Wow64::None), "\"none\"");
        assert_eq!(name(HidrawMode::Enabled), "\"enabled\"");
        assert_eq!(
            "rosetta-wine".parse::<EngineId>().unwrap(),
            EngineId::RosettaWine
        );
        assert!(matches!(
            "fsync".parse::<SyncMode>().unwrap_err(),
            Error::UnknownSyncMode(_)
        ));
        assert!(matches!(
            "wine".parse::<EngineId>().unwrap_err(),
            Error::UnknownEngineId(_)
        ));
    }

    #[test]
    fn graphics_backend_and_hidraw_mode_round_trip() {
        for backend in [
            GraphicsBackend::Dxmt,
            GraphicsBackend::WineD3D,
            GraphicsBackend::Vkd3dProton,
            GraphicsBackend::D3DMetal,
        ] {
            assert_eq!(
                backend.as_str().parse::<GraphicsBackend>().unwrap(),
                backend
            );
            assert_eq!(
                name(backend),
                format!("\"{backend}\""),
                "serde and as_str agree"
            );
        }
        for mode in [HidrawMode::Enabled, HidrawMode::Disabled] {
            assert_eq!(mode.as_str().parse::<HidrawMode>().unwrap(), mode);
            assert_eq!(name(mode), format!("\"{mode}\""));
        }
        assert!(matches!(
            "d3d9".parse::<GraphicsBackend>().unwrap_err(),
            Error::UnknownGraphicsBackend(_)
        ));
        assert!(matches!(
            "on".parse::<HidrawMode>().unwrap_err(),
            Error::UnknownHidrawMode(_)
        ));
        assert_eq!(EngineId::RosettaWine.arch(), Arch::X86_64);
        assert_eq!(EngineId::NativeArm64.arch(), Arch::Arm64);
    }
}
