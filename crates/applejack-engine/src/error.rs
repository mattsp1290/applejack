//! Typed errors. Every variant renders a message a CLI can print as-is.

use std::io;
use std::path::PathBuf;

use applejack_manifest::Sha256;

use crate::launch::env::DropReason;
use crate::{EngineId, Version};

/// Errors from engine install, validation, version management, and launch.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The downloaded or supplied archive does not hash to the pinned digest.
    /// The archive at `path` is left in place for inspection.
    #[error("archive `{}` SHA-256 mismatch: expected {expected}, actual {actual}", path.display())]
    Sha256Mismatch {
        /// The archive that was hashed.
        path: PathBuf,
        /// The digest the request pinned.
        expected: Sha256,
        /// The digest of the bytes on disk.
        actual: Sha256,
    },

    /// The archive breaks a layout rule: an entry or link leaves the bundle
    /// root, a symlink resolves outside the extracted tree, or `BUNDLE.toml`
    /// is not a top-level entry.
    #[error("invalid archive: {0}")]
    InvalidArchive(String),

    /// `BUNDLE.toml` parsed but breaks an invariant.
    #[error("invalid BUNDLE.toml: {0}")]
    InvalidBundle(String),

    /// `BUNDLE.toml` names a loader that is not a file in the bundle.
    #[error("BUNDLE.toml names loader `{}` but the bundle has no such file", path.display())]
    MissingLoader {
        /// The `wine.loader` path as written in `BUNDLE.toml`.
        path: PathBuf,
    },

    /// `BUNDLE.toml` disagrees with the manifest entry the install came from.
    #[error("BUNDLE.toml `{field}` is `{bundle}` but the manifest pins `{manifest}`")]
    ManifestMismatch {
        /// The `BUNDLE.toml` field that disagrees.
        field: &'static str,
        /// The value the manifest entry pins.
        manifest: String,
        /// The value `BUNDLE.toml` carries.
        bundle: String,
    },

    /// A sync mode name outside `msync`, `esync`, and `none`.
    #[error("unknown sync mode `{0}` (expected msync, esync, or none)")]
    UnknownSyncMode(String),

    /// An engine id outside `rosetta-wine` and `native-arm64`.
    #[error("unknown engine id `{0}` (expected rosetta-wine or native-arm64)")]
    UnknownEngineId(String),

    /// A graphics backend name outside `dxmt`, `wined3d`, `vkd3d-proton`, and
    /// `d3dmetal`.
    #[error("unknown graphics backend `{0}` (expected dxmt, wined3d, vkd3d-proton, or d3dmetal)")]
    UnknownGraphicsBackend(String),

    /// A HID mode name outside `enabled` and `disabled`.
    #[error("unknown hidraw mode `{0}` (expected enabled or disabled)")]
    UnknownHidrawMode(String),

    /// A version string that cannot name a directory under `engines/<id>/`.
    #[error("invalid engine version `{version}`: {reason}")]
    InvalidVersion {
        /// The rejected text.
        version: String,
        /// The rule it broke.
        reason: &'static str,
    },

    /// Removal refused because the caller listed the version as protected:
    /// it is active, pinned by a game configuration, or in use by a running
    /// session.
    #[error(
        "version {version} of engine {id} is protected (active, pinned by a game configuration, or in use)"
    )]
    ProtectedVersion {
        /// The engine.
        id: EngineId,
        /// The protected version.
        version: Version,
    },

    /// The requested version has no `engines/<id>/<version>/` directory, or
    /// the directory holds no `BUNDLE.toml` and so is not a bundle.
    #[error("engine {id} version {version} is not installed")]
    NotInstalled {
        /// The engine.
        id: EngineId,
        /// The version that is absent.
        version: Version,
    },

    /// Install refused because `engines/<id>/<version>/` already holds a
    /// bundle.
    #[error("engine {id} version {version} is already installed")]
    AlreadyInstalled {
        /// The engine.
        id: EngineId,
        /// The version already present.
        version: Version,
    },

    /// The archive's `BUNDLE.toml` describes a different engine or version
    /// than the one being installed.
    #[error(
        "BUNDLE.toml describes engine {found} version {found_version}, expected {expected} version {expected_version}"
    )]
    BundleIdentityMismatch {
        /// The engine the request named.
        expected: EngineId,
        /// The version the request named.
        expected_version: Version,
        /// The engine `BUNDLE.toml` names.
        found: EngineId,
        /// The version `BUNDLE.toml` names.
        found_version: Version,
    },

    /// An allowed root for path-valued environment variables is unusable.
    #[error("allowed root `{}` {reason}", path.display())]
    InvalidRoot {
        /// The root as given.
        path: PathBuf,
        /// The rule it broke.
        reason: &'static str,
    },

    /// A variable every launch must carry (`WINEPREFIX`,
    /// `APPLEJACK_SESSION`) has no accepted value: the engine did not set
    /// it, or set one that failed its check, so the child would start
    /// without it.
    #[error("{}", required_variable_message(name, rejected.as_ref()))]
    MissingEngineVariable {
        /// The allowlist row that stayed empty.
        name: &'static str,
        /// Why the engine's own value was dropped, when it set one; `None`
        /// when the engine never set the variable.
        rejected: Option<DropReason>,
    },

    /// Rosetta 2 is required and not installed.
    #[error(
        "Rosetta 2 is not installed; run `softwareupdate --install-rosetta --agree-to-license`"
    )]
    RosettaMissing,

    /// A filesystem operation failed; `path` is the file or directory involved.
    #[error("{}: {source}", path.display())]
    Io {
        /// The file or directory involved.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: io::Error,
    },

    /// `BUNDLE.toml` is not valid TOML or does not match the schema.
    #[error("BUNDLE.toml: {0}")]
    Toml(#[from] toml::de::Error),

    /// The engine manifest failed to parse or validate.
    #[error("engine manifest: {0}")]
    Manifest(#[from] applejack_manifest::Error),
}

fn required_variable_message(name: &str, rejected: Option<&DropReason>) -> String {
    match rejected {
        Some(reason) => {
            format!("engine set the required launch variable {name} to a rejected value: {reason}")
        }
        None => format!("engine did not set the required launch variable {name}"),
    }
}

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

/// Result alias for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;
