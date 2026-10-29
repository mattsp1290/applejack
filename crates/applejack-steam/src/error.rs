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
    /// Another process holds the lock. `holder` is the line that process
    /// wrote into the lock file; it is `None` while the new holder has not
    /// written its line yet, when the line does not parse, or when the
    /// recorded pid is no longer alive (a stale line left by a crash).
    #[error("lock `{name}` is held{}", holder.as_ref().map_or_else(|| " (holder unknown)".to_owned(), |holder| format!(" by pid {} ({})", holder.pid, holder.command)))]
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
    /// A login, refresh, or license-list step failed.
    #[error(transparent)]
    Auth(#[from] AuthError),
}
