use std::io;
use std::path::PathBuf;

use crate::auth::AuthError;
use crate::lock::Holder;

/// Every failure this crate reports.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The text is not valid `KeyValues`.
    #[error("vdf: line {line}: {message}")]
    VdfParse { line: usize, message: String },
    /// An `appmanifest` is missing a field or holds a value of the wrong shape.
    #[error("acf: field `{path}`: {reason}")]
    AcfField { path: String, reason: String },
    /// Another process holds the lock. `holder` is the last line a holder
    /// wrote into the lock file; it is `None` while the new holder has not
    /// written its line yet, when the line does not parse, or when the
    /// recorded pid is no longer alive (a stale line left by a crash). A
    /// line left by a crash whose pid has since been reused names an
    /// unrelated process, so the message calls it the last recorded holder
    /// rather than inviting a `kill`.
    #[error("lock `{name}` is held{}", holder.as_ref().map_or_else(|| " (holder unknown)".to_owned(), |holder| format!(" (last recorded holder: pid {} `{}`)", holder.pid, holder.command)))]
    LockHeld {
        name: String,
        holder: Option<Holder>,
    },
    /// The lock file could not be created, locked, or written.
    #[error("lock `{name}` at {}: {source}", path.display())]
    LockIo {
        name: String,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The Security framework rejected a Keychain operation.
    #[error("keychain: {message} (OSStatus {code})")]
    Keychain { code: i32, message: String },
    /// A stored token is not UTF-8.
    #[error("keychain: stored token is not valid UTF-8")]
    KeychainEncoding,
    /// A login step or a call on a logged-in session failed. A CDN rate limit
    /// arrives here as [`AuthError::RateLimited`].
    #[error(transparent)]
    Auth(#[from] AuthError),
    /// The account does not own this app.
    #[error("app {appid} is not in this account's library")]
    AppNotOwned { appid: u32 },
    /// The app ships no Windows depot; `platforms` is what it does list.
    #[error("app {appid} has no Windows depot; it lists: {}", if platforms.is_empty() { "no platforms".to_owned() } else { platforms.join(", ") })]
    NoWindowsDepot { appid: u32, platforms: Vec<String> },
    /// No depot this app would install has a manifest on the branch, or the
    /// branch has no build id.
    #[error("app {appid} has no installable depot on branch `{branch}`")]
    NoManifestForBranch { appid: u32, branch: String },
    /// A name from Steam would land outside the install directory.
    #[error("refusing unsafe path from Steam: `{path}`")]
    UnsafePath { path: String },
    /// A path runs through a symlink that leaves the install directory, or
    /// the install directory of a title with no `.acf` is itself a symlink.
    #[error("refusing to write through `{}`: it is a symlink to `{}`, outside the install", link.display(), target.display())]
    LinkOutOfInstall { link: PathBuf, target: PathBuf },
    /// A Steam client runs in a prefix that maps this library, so the
    /// library is read-only.
    #[error("a Steam client is running against this library; app {appid} was not written")]
    ClientRunning { appid: u32 },
    /// The `.acf` reports something other than fully installed, so the
    /// Steam client owns the title's update.
    #[error("app {appid} has StateFlags {state_flags}; `applejack runtime steam` owns its update")]
    ClientOwned { appid: u32, state_flags: u32 },
    /// There is no `.acf` for this app.
    #[error("app {appid} is not installed")]
    NotInstalled { appid: u32 },
    /// The title is already fully installed; update or verify it instead.
    #[error("app {appid} is already installed; update or verify it instead")]
    AlreadyInstalled { appid: u32 },
    /// The manifest was gone, and so was the one a fresh read of the product
    /// info named.
    #[error(
        "app {appid} depot {depot_id}: manifest {listed} changed to {current} and could not be fetched"
    )]
    ManifestChanged {
        appid: u32,
        depot_id: u32,
        listed: u64,
        current: u64,
    },
    /// A stopped run wrote into a depot the plan no longer selects, so
    /// finishing without it would leave mixed files under a good `.acf`.
    #[error(
        "app {appid}: a stopped update touched depot {depot_id}, which this plan leaves out; rerun with a plan that selects it"
    )]
    UnfinishedDepot { appid: u32, depot_id: u32 },
    /// `verify_install` found no stored manifest for an installed depot.
    #[error("app {appid} depot {depot_id}: manifest {manifest_id} is not stored")]
    ManifestNotStored {
        appid: u32,
        depot_id: u32,
        manifest_id: u64,
    },
    /// A chunk or assembled file failed its hash. Nothing corrupt reached
    /// the install, and the partial state is kept.
    #[error("depot {depot_id}: corrupt download: {detail}")]
    CorruptChunk { depot_id: u32, detail: String },
    /// The disk filled. The partial state is kept; run the install again
    /// after freeing space.
    #[error("disk full while installing app {appid}; partial state kept, last at {}", record.display())]
    DiskFull { appid: u32, record: PathBuf },
    /// Any other depot failure.
    #[error("depot {depot_id}: {message}")]
    Depot { depot_id: u32, message: String },
    /// A library file could not be read or written.
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}
