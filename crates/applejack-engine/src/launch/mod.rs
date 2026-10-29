//! Launch inputs and outputs. Process spawning, log capture, and process
//! discovery by session id are implemented by the `rosetta-wine` unit;
//! [`mod@env`] holds the environment allowlist every engine applies.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::EnvPair;

pub mod env;

/// What to run inside a prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    /// Windows executable path (Wine path syntax or a path under a mapped
    /// drive) or the Steam client when `args` select it.
    pub exe: PathBuf,
    /// Arguments passed to `exe`.
    pub args: Vec<OsString>,
    /// Per-game variables; filtered through [`env::assemble`].
    pub env_extras: Vec<EnvPair>,
    /// Working directory of the loader process.
    pub cwd: PathBuf,
    /// Directory that receives `wine.log` for the session.
    pub log_dir: PathBuf,
}

/// A running launch. `processes()` and cancellation arrive with the
/// `rosetta-wine` unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// Value of `APPLEJACK_SESSION` in every process of the launch.
    pub id: String,
    /// Pid of the loader process.
    pub pid: u32,
    /// `<log_dir>/wine.log` for this session.
    pub log_path: PathBuf,
}
