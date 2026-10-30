//! Process discovery by session id and the `wineserver` liveness check.
//!
//! Every process of a launch carries `APPLEJACK_SESSION=<id>` in its
//! environment (Wine passes the environment to every Windows process it
//! starts). The kernel exposes a process's arguments and environment through
//! `sysctl(KERN_PROCARGS2)`; the `sysinfo` crate performs that call and
//! returns the exec path, argv, and environment of every process, which
//! [`match_record`] filters by uid and session id. No `unsafe` lives in this
//! crate: `sysinfo` is the one safe wrapper over that `sysctl` in the local
//! registry, and the workspace forbids calling `libc::sysctl` directly.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::{Error, Result};

/// The variable process discovery keys on.
pub const SESSION_VARIABLE: &str = "APPLEJACK_SESSION";

/// One Unix process that belongs to a session.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Process {
    /// Unix pid.
    pub pid: u32,
    /// The Windows executable name as it appears in argv (`steam.exe`,
    /// `steamwebhelper.exe`, the game), or the Unix program name when argv
    /// names no `.exe`.
    pub exe: String,
    /// The `APPLEJACK_SESSION` value the process carries.
    pub session_id: String,
}

/// A process as `KERN_PROCARGS2` describes it: exec path, argv, and
/// environment, plus the owning uid. Borrowed from `sysinfo` in production
/// and built by hand from a captured record in tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessRecord<'a> {
    /// Unix pid.
    pub pid: u32,
    /// Real uid, when known.
    pub uid: Option<u32>,
    /// The exec path from the start of the buffer.
    pub exe: Option<&'a Path>,
    /// argv, which Wine rewrites in place so that argv\[0\] is the Windows
    /// executable.
    pub cmd: &'a [OsString],
    /// `NAME=value` entries.
    pub environ: &'a [OsString],
}

/// The processes owned by `uid` whose environment carries
/// `APPLEJACK_SESSION=<session_id>`, from a snapshot of every process.
#[must_use]
pub fn find_session(session_id: &str) -> Vec<Process> {
    let uid = rustix::process::getuid().as_raw();
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_user(UpdateKind::Always)
            .with_exe(UpdateKind::Always)
            .with_cmd(UpdateKind::Always)
            .with_environ(UpdateKind::Always),
    );
    let mut found: Vec<Process> = system
        .processes()
        .values()
        .filter_map(|process| {
            let record = ProcessRecord {
                pid: process.pid().as_u32(),
                uid: process.user_id().map(|id| **id),
                exe: process.exe(),
                cmd: process.cmd(),
                environ: process.environ(),
            };
            match_record(&record, session_id, uid)
        })
        .collect();
    found.sort();
    found
}

/// The [`Process`] for `record` when it is owned by `uid` and carries the
/// session id; `None` otherwise.
#[must_use]
pub fn match_record(record: &ProcessRecord<'_>, session_id: &str, uid: u32) -> Option<Process> {
    if record.uid != Some(uid) {
        return None;
    }
    let mut wanted = Vec::with_capacity(SESSION_VARIABLE.len() + 1 + session_id.len());
    wanted.extend_from_slice(SESSION_VARIABLE.as_bytes());
    wanted.push(b'=');
    wanted.extend_from_slice(session_id.as_bytes());
    if !record
        .environ
        .iter()
        .any(|entry| entry.as_bytes() == wanted.as_slice())
    {
        return None;
    }
    Some(Process {
        pid: record.pid,
        exe: executable_name(record.cmd, record.exe),
        session_id: session_id.to_owned(),
    })
}

/// The last argv entry that names a `.exe`, reduced to its file name (Wine
/// accepts both `\` and `/` separators); else argv\[0\]'s file name; else
/// the exec path's file name.
#[must_use]
pub fn executable_name(cmd: &[OsString], exe: Option<&Path>) -> String {
    let is_exe = |arg: &OsString| {
        let bytes = arg.as_bytes();
        bytes.len() > 4 && bytes[bytes.len() - 4..].eq_ignore_ascii_case(b".exe")
    };
    let chosen = cmd
        .iter()
        .rev()
        .find(|arg| is_exe(arg))
        .or_else(|| cmd.first())
        .map(|arg| windows_file_name(arg.as_os_str()))
        .or_else(|| {
            exe.and_then(Path::file_name)
                .map(|name| name.to_string_lossy().into_owned())
        });
    chosen.unwrap_or_default()
}

fn windows_file_name(arg: &OsStr) -> String {
    let bytes = arg.as_bytes();
    let start = bytes
        .iter()
        .rposition(|byte| *byte == b'\\' || *byte == b'/')
        .map_or(0, |index| index + 1);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// Where the `wineserver` for `prefix_dir` listens:
/// `/tmp/.wine-<uid>/server-<dev>-<inode>/socket`, with the prefix
/// directory's device and inode in hex, as Wine derives it.
///
/// # Errors
///
/// Returns [`Error::Io`] when the prefix directory cannot be stat'ed.
pub fn server_socket(prefix_dir: &Path) -> Result<PathBuf> {
    let meta = fs::metadata(prefix_dir).map_err(|error| Error::io(prefix_dir, error))?;
    let uid = rustix::process::getuid().as_raw();
    Ok(PathBuf::from(format!(
        "/tmp/.wine-{uid}/server-{:x}-{:x}/socket",
        meta.dev(),
        meta.ino()
    )))
}

/// True when a `wineserver` is listening for `prefix_dir`: the canonical
/// "is anything running in this prefix" check. A connect to the socket
/// succeeds only while the server is alive, so a socket file left by a
/// killed server reads as not alive; the server drops the unused
/// connection.
///
/// # Errors
///
/// Returns [`Error::Io`] when the prefix directory cannot be stat'ed.
pub fn wineserver_alive(prefix_dir: &Path) -> Result<bool> {
    let socket = server_socket(prefix_dir)?;
    Ok(UnixStream::connect(socket).is_ok())
}
