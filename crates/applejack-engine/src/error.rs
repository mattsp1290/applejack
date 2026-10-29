//! Typed errors. Every variant renders a message a CLI can print as-is.

use std::io;
use std::path::PathBuf;

use applejack_manifest::Sha256;

use crate::{EngineId, Version};

/// Errors from engine install, validation, version management, and launch.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The downloaded or supplied archive does not hash to the pinned digest.
    /// The archive is left in place for inspection.
    #[error("archive SHA-256 mismatch: expected {expected}, actual {actual}")]
    Sha256Mismatch { expected: Sha256, actual: Sha256 },

    /// `BUNDLE.toml` parsed but breaks an invariant.
    #[error("invalid BUNDLE.toml: {0}")]
    InvalidBundle(String),

    /// `BUNDLE.toml` names a loader that is not a file in the bundle.
    #[error("BUNDLE.toml names loader `{}` but the bundle has no such file", path.display())]
    MissingLoader { path: PathBuf },

    /// A sync mode name outside `msync`, `esync`, and `none`.
    #[error("unknown sync mode `{0}` (expected msync, esync, or none)")]
    UnknownSyncMode(String),

    /// An engine id outside `rosetta-wine` and `native-arm64`.
    #[error("unknown engine id `{0}` (expected rosetta-wine or native-arm64)")]
    UnknownEngineId(String),

    /// A version string that cannot name a directory under `engines/<id>/`.
    #[error("invalid engine version `{version}`: {reason}")]
    InvalidVersion {
        version: String,
        reason: &'static str,
    },

    /// Removal refused because the version is the active one.
    #[error(
        "version {version} of engine {id} is active; select another with `applejack engine use` first"
    )]
    ActiveVersion { id: EngineId, version: Version },

    /// Removal refused because a game configuration pins the version.
    #[error("version {version} of engine {id} is referenced by a game configuration")]
    ReferencedVersion { id: EngineId, version: Version },

    /// The requested version has no `engines/<id>/<version>/` directory.
    #[error("engine {id} version {version} is not installed")]
    NotInstalled { id: EngineId, version: Version },

    /// Install refused because `engines/<id>/<version>/` already exists.
    #[error("engine {id} version {version} is already installed")]
    AlreadyInstalled { id: EngineId, version: Version },

    /// The archive's `BUNDLE.toml` describes a different engine or version
    /// than the one being installed.
    #[error(
        "BUNDLE.toml describes engine {found} version {found_version}, expected {expected} version {expected_version}"
    )]
    BundleIdentityMismatch {
        expected: EngineId,
        expected_version: Version,
        found: EngineId,
        found_version: Version,
    },

    /// Rosetta 2 is required and not installed.
    #[error(
        "Rosetta 2 is not installed; run `softwareupdate --install-rosetta --agree-to-license`"
    )]
    RosettaMissing,

    /// A filesystem operation failed; `path` is the file or directory involved.
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
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
