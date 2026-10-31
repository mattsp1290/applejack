//! Launch inputs and outputs. [`mod@env`] holds the environment allowlist
//! every engine applies; spawning, log capture, cancellation, and process
//! discovery are implemented by [`crate::rosetta_wine`], which gives
//! [`Session`] its methods.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Child;

use crate::{CpuidProbe, EnvPair};

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
    /// Log root: the session writes `<log_dir>/<session-id>/wine.log`. The
    /// launcher's root is `~/Library/Logs/applejack`; tests pass a temporary
    /// directory.
    pub log_dir: PathBuf,
    /// The game configuration asks for `ROSETTA_ADVERTISE_AVX=1`. It is set
    /// only when `cpuid_probe` recorded that the variable changes the
    /// advertised feature bits on this host.
    pub advertise_avx: bool,
    /// The host's recorded CPUID probe (`RosettaWine::probe_cpuid`), when
    /// one has been run; `None` means AVX is never advertised.
    pub cpuid_probe: Option<CpuidProbe>,
}

/// A running launch. Owns the loader's child handle, so it is neither
/// cloned nor compared; the plain fields describe the launch for logging
/// and for the core's session row.
#[derive(Debug)]
pub struct Session {
    /// Value of `APPLEJACK_SESSION` in every process of the launch.
    pub id: String,
    /// Pid of the loader process.
    pub pid: u32,
    /// Absolute path of the bundle's `wineserver` (`InstalledEngine::root`
    /// joined with `wine.server`), for `<server> -k`.
    pub server: PathBuf,
    /// The `WINEPREFIX` the launch used; `<server> -k` must target it.
    pub prefix_dir: PathBuf,
    /// `<log_dir>/<session-id>/wine.log` for this session.
    pub log_path: PathBuf,
    /// `PATH` the children see; `wineserver -k` runs with the same one.
    pub(crate) path: OsString,
    pub(crate) child: Child,
}
