use std::io;
use std::path::PathBuf;

use crate::auth::AuthError;

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
    /// Another process holds the lock. `pid` and `command` come from the lock
    /// file and are absent when the holder has not written them yet.
    #[error("lock `{name}` is held by pid {} ({})", pid.map_or_else(|| "unknown".to_owned(), |pid| pid.to_string()), command.as_deref().unwrap_or("unknown command"))]
    LockHeld {
        name: String,
        pid: Option<u32>,
        command: Option<String>,
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
