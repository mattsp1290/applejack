//! The failures of the engine, prefix, and configuration operations, each
//! with the wire code the RPC layer answers with. The CLI prints the same
//! text.

use std::io;
use std::path::PathBuf;

use applejack_engine::{EngineId, Error as EngineError, SyncMode};

use crate::config::{ALLOW_UNSIGNED_ENV, DataDirError, Error as ConfigError};
use crate::rpc::wire::{ErrorCode, RpcError};

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    DataDir(#[from] DataDirError),
    /// A local bundle was requested without the opt-in.
    #[error("{}", unsigned_refused_message())]
    UnsignedRefused,
    #[error("{}", no_published_message(*id))]
    NoPublishedVersion { id: EngineId },
    #[error("{}", download_unavailable_message(*id, version))]
    DownloadUnavailable { id: EngineId, version: String },
    #[error("engine {id} is not available in this build")]
    EngineUnavailable { id: EngineId },
    /// No active engine; `fix` says what to run.
    #[error("no engine is active: {fix}")]
    NoActiveEngine { fix: String },
    #[error("no prefix named `{name}`")]
    UnknownPrefix { name: String },
    #[error("engine version {version} does not offer sync mode {mode}")]
    SyncUnavailable { version: String, mode: SyncMode },
    /// Plan 02: every path handed to Wine is inside the data directory.
    #[error("`{}` is outside the data directory", exe.display())]
    PathOutsideData { exe: PathBuf },
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot encode: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("{0}")]
    Message(String),
}

/// How to install a local bundle; a release build ignores the variable, so
/// it has no such advice to give.
fn local_install(id: EngineId) -> Option<String> {
    cfg!(debug_assertions).then(|| {
        format!("{ALLOW_UNSIGNED_ENV}=1 `applejack engine install {id} --from <dir-or-archive>`")
    })
}

fn unsigned_refused_message() -> String {
    let remedy = if cfg!(debug_assertions) {
        format!("set {ALLOW_UNSIGNED_ENV}=1 (debug builds only)")
    } else {
        "a release build installs signed bundles only".to_owned()
    };
    format!("refusing to install an unsigned local bundle: {remedy}")
}

fn no_published_message(id: EngineId) -> String {
    local_install(id).map_or_else(
        || format!("no published version of engine {id} exists, and a release build cannot install a local bundle"),
        |command| format!("no published version of engine {id} exists: {command} installs a local bundle"),
    )
}

fn download_unavailable_message(id: EngineId, version: &str) -> String {
    let advice = if cfg!(debug_assertions) {
        "; use `--from <dir-or-archive>`"
    } else {
        ""
    };
    format!("engine {id} {version} is published but this build cannot download it{advice}")
}

/// What `engine doctor` advises with no version installed and none published.
pub(crate) fn no_bundle_fix(id: EngineId) -> String {
    local_install(id).map_or_else(
        || {
            "no signed bundle is published yet, and a release build cannot install a local one"
                .to_owned()
        },
        |command| format!("no signed bundle is published yet; run {command}"),
    )
}

impl CoreError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// The code a response or a `job.failed` carries.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Engine(error) => engine_code(error),
            Self::Config(ConfigError::UnknownField(_) | ConfigError::InvalidValue { .. }) => {
                ErrorCode::InvalidParams
            }
            Self::UnsignedRefused => ErrorCode::UnsignedEngineRefused,
            Self::NoPublishedVersion { .. } | Self::DownloadUnavailable { .. } => {
                ErrorCode::DownloadFailed
            }
            Self::NoActiveEngine { .. } => ErrorCode::EngineNotInstalled,
            Self::Io { source, .. } => io_code(source),
            Self::UnknownPrefix { .. }
            | Self::SyncUnavailable { .. }
            | Self::PathOutsideData { .. }
            | Self::EngineUnavailable { .. } => ErrorCode::InvalidParams,
            Self::Config(_) | Self::DataDir(_) | Self::Encode(_) | Self::Message(_) => {
                ErrorCode::Internal
            }
        }
    }
}

fn io_code(error: &io::Error) -> ErrorCode {
    if error.kind() == io::ErrorKind::StorageFull {
        ErrorCode::DiskFull
    } else {
        ErrorCode::Internal
    }
}

fn engine_code(error: &EngineError) -> ErrorCode {
    match error {
        EngineError::Sha256Mismatch { .. } => ErrorCode::ChecksumMismatch,
        EngineError::InvalidArchive(_)
        | EngineError::InvalidBundle(_)
        | EngineError::MissingLoader { .. }
        | EngineError::ManifestMismatch { .. }
        | EngineError::BundleIdentityMismatch { .. }
        | EngineError::Toml(_) => ErrorCode::InvalidBundle,
        EngineError::NotInstalled { .. } => ErrorCode::EngineNotInstalled,
        EngineError::Io { source, .. } => io_code(source),
        EngineError::PrefixBusy { .. } => ErrorCode::PrefixBusy,
        EngineError::ResetFailed { source, .. } => engine_code(source),
        EngineError::UnknownEngineId(_)
        | EngineError::UnknownGraphicsBackend(_)
        | EngineError::UnknownSyncMode(_)
        | EngineError::UnknownHidrawMode(_)
        | EngineError::InvalidVersion { .. }
        | EngineError::InvalidSnapshotName { .. }
        | EngineError::SnapshotNotFound { .. }
        | EngineError::LicenseNotAccepted { .. }
        | EngineError::D3DMetalSource { .. } => ErrorCode::InvalidParams,
        _ => ErrorCode::Internal,
    }
}

impl From<CoreError> for RpcError {
    fn from(error: CoreError) -> Self {
        Self::new(error.code(), error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs with `--release` in `cargo xtask check`, which is the side that
    /// matters: a release build ignores the variable.
    #[test]
    fn unsigned_advice_follows_the_build() {
        let id = EngineId::RosettaWine;
        for text in [
            CoreError::UnsignedRefused.to_string(),
            CoreError::NoPublishedVersion { id }.to_string(),
            CoreError::DownloadUnavailable {
                id,
                version: "1".to_owned(),
            }
            .to_string(),
            no_bundle_fix(id),
        ] {
            let advises = text.contains(ALLOW_UNSIGNED_ENV) || text.contains("--from");
            assert_eq!(advises, cfg!(debug_assertions), "{text}");
        }
    }

    #[test]
    fn a_failed_reset_keeps_the_code_of_its_cause() {
        let full = EngineError::Io {
            path: PathBuf::from("/x"),
            source: io::ErrorKind::StorageFull.into(),
        };
        let reset = CoreError::from(EngineError::ResetFailed {
            name: "p".to_owned(),
            timestamp: "t".to_owned(),
            source: Box::new(full),
        });
        assert_eq!(reset.code(), ErrorCode::DiskFull);
    }
}
