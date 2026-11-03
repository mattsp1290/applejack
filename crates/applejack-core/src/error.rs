//! The failures of the engine, prefix, and configuration operations, each
//! with the wire code the RPC layer answers with. The CLI prints the same
//! text.

use std::io;
use std::path::PathBuf;

use applejack_engine::{EngineId, Error as EngineError, SyncMode};
use applejack_steam::{AuthError, Error as SteamError};

use crate::config::{ALLOW_UNSIGNED_ENV, DataDirError, Drm, Error as ConfigError};
use crate::rpc::wire::{ErrorCode, RpcError};

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    DataDir(#[from] DataDirError),
    #[error(transparent)]
    Steam(#[from] SteamError),
    /// Only a login answers this; the CLI exits 3.
    #[error("not signed in to Steam ({reason}); run `applejack steam login`")]
    SteamLoginRequired { reason: String },
    /// The silent logon with the stored token failed for another reason, so
    /// a new login may not help. The CLI exits 3 here too.
    #[error("could not sign in to Steam with the stored token: {0}")]
    SteamRefreshFailed(SteamError),
    /// Steam took the answer and issued no token. Ends the login, so a
    /// caller does not poll for good.
    #[error("Steam issued no token within {seconds} seconds of the sign-in; begin again")]
    SteamLoginStalled { seconds: u64 },
    /// `steam.login.complete` for a login that is not the pending one.
    #[error("no such Steam login in progress; begin again")]
    NoSuchLogin,
    /// A request whose fields cannot go together.
    #[error("{0}")]
    InvalidRequest(String),
    #[error(
        "title {appid} has no launch entry that can be chosen for you ({}); pick one \
         with `applejack config {appid} set launch_exe_override <file>`",
        candidates.join(", ")
    )]
    LaunchExeAmbiguous { appid: u32, candidates: Vec<String> },
    #[error(
        "title {appid} differs from its manifest: {mismatched} files changed, {missing} missing \
         (first: {first})"
    )]
    VerifyFailed {
        appid: u32,
        mismatched: usize,
        missing: usize,
        first: String,
    },
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
    /// `apps` has no row for the title, or the library does not hold it.
    #[error("title {appid} is not installed: {reason}")]
    TitleNotInstalled { appid: u32, reason: String },
    /// `applejack/<appid>.download.json` exists: the files are mid-write.
    #[error(
        "title {appid} has a download in progress or a stopped one; its files are not complete \
         until `{finish}` finishes it"
    )]
    DownloadInProgress { appid: u32, finish: String },
    #[error("title {appid} is already running")]
    AlreadyRunning { appid: u32 },
    /// The title takes the client path, which does not exist yet.
    #[error(
        "title {appid} (drm = {drm}) launches through the Steam client, and the client runtime \
         is not available yet. {route}"
    )]
    ClientRuntimeUnavailable { appid: u32, drm: Drm, route: String },
    #[error("title {appid} has no usable launch executable: {reason}")]
    NoLaunchExe { appid: u32, reason: String },
    #[error("title {appid} is not running")]
    NotRunning { appid: u32 },
    /// Plan 05's error path for a direct launch.
    #[error(
        "title {appid} exited within {seconds} seconds: title may require the Steam client; \
         switch launch_mode to client"
    )]
    TitleExitedEarly { appid: u32, seconds: u64 },
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

    /// `update` says the title was whole before the download that stopped,
    /// which only `--update` finishes: a plain install finds its `.acf`.
    pub(crate) fn download_in_progress(appid: u32, update: bool) -> Self {
        let flag = if update { " --update" } else { "" };
        Self::DownloadInProgress {
            appid,
            finish: format!("applejack install {appid}{flag}"),
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
            Self::Steam(error) | Self::SteamRefreshFailed(error) => steam_code(error),
            Self::SteamLoginRequired { .. } => ErrorCode::SteamLoginRequired,
            Self::NoSuchLogin | Self::SteamLoginStalled { .. } => ErrorCode::SteamLoginFailed,
            Self::LaunchExeAmbiguous { .. } => ErrorCode::LaunchExeAmbiguous,
            Self::VerifyFailed { .. } => ErrorCode::VerifyFailed,
            Self::UnsignedRefused => ErrorCode::UnsignedEngineRefused,
            Self::NoPublishedVersion { .. } | Self::DownloadUnavailable { .. } => {
                ErrorCode::DownloadFailed
            }
            Self::NoActiveEngine { .. } => ErrorCode::EngineNotInstalled,
            Self::TitleNotInstalled { .. } => ErrorCode::TitleNotInstalled,
            Self::DownloadInProgress { .. } => ErrorCode::DownloadInProgress,
            Self::AlreadyRunning { .. } => ErrorCode::AlreadyRunning,
            Self::ClientRuntimeUnavailable { .. } => ErrorCode::ClientRuntimeUnavailable,
            Self::NoLaunchExe { .. } => ErrorCode::NoLaunchExe,
            Self::NotRunning { .. } => ErrorCode::NotRunning,
            Self::TitleExitedEarly { .. } => ErrorCode::TitleExitedEarly,
            Self::Io { source, .. } => io_code(source),
            Self::InvalidRequest(_)
            | Self::UnknownPrefix { .. }
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

fn auth_code(error: &AuthError) -> ErrorCode {
    match error {
        AuthError::RateLimited { .. } => ErrorCode::SteamRateLimited,
        AuthError::InvalidGuardCode => ErrorCode::SteamGuardRejected,
        AuthError::NoStoredToken => ErrorCode::SteamLoginRequired,
        AuthError::Connection(_) | AuthError::CmUnavailable(_) | AuthError::CallDenied { .. } => {
            ErrorCode::SteamUnavailable
        }
        _ => ErrorCode::SteamLoginFailed,
    }
}

fn steam_code(error: &SteamError) -> ErrorCode {
    match error {
        SteamError::Auth(error) => auth_code(error),
        SteamError::AppNotOwned { .. } => ErrorCode::AppNotOwned,
        SteamError::NoWindowsDepot { .. }
        | SteamError::NoManifestForBranch { .. }
        | SteamError::NoLicensedDepot { .. }
        | SteamError::UnresolvedSharedDepot { .. } => ErrorCode::TitleNotInstallable,
        SteamError::LockHeld { .. } => ErrorCode::LockHeld,
        SteamError::ClientRunning { .. } | SteamError::ClientOwned { .. } => {
            ErrorCode::SteamClientOwnsTitle
        }
        SteamError::AlreadyInstalled { .. } => ErrorCode::AlreadyInstalled,
        SteamError::NotInstalled { .. } => ErrorCode::TitleNotInstalled,
        SteamError::NothingToVerify { .. } => ErrorCode::VerifyFailed,
        SteamError::Cancelled { .. } => ErrorCode::InstallCancelled,
        SteamError::DiskFull { .. } => ErrorCode::DiskFull,
        SteamError::CorruptChunk { .. } => ErrorCode::ChecksumMismatch,
        SteamError::ManifestChanged { .. }
        | SteamError::ManifestNotServed { .. }
        | SteamError::Depot { .. } => ErrorCode::DownloadFailed,
        SteamError::Io { source, .. } => io_code(source),
        _ => ErrorCode::Internal,
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
        EngineError::PrefixUnusable { .. } | EngineError::InvalidPrefix { .. } => {
            ErrorCode::PrefixUnusable
        }
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
