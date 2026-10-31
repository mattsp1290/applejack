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
//!
//! Wine rewrites a Windows process's argv in place: the Windows command
//! line, then an empty slot that the kernel's argument count still covers,
//! then the zeroed rest of the old area, then the environment. `sysinfo`
//! skips NUL runs while it counts arguments, so for every such process the
//! first environment entry is reported as the last `cmd` entry and is absent
//! from `environ`. `std::process::Command` sorts the environment it passes,
//! and `APPLEJACK_SESSION` sorts first among the allowlisted names, so that
//! entry is exactly the session variable. [`match_record`] therefore accepts
//! the exact `NAME=value` entry in `environ` or as the last
//! `APPLEJACK_SESSION=` entry of `cmd` (the fold always follows the Windows
//! arguments, so an earlier one is an argument the program was given) and
//! cuts argv at it before deriving the executable name.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
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
///
/// One call reads the arguments and environment of every process on the
/// host (31 to 34 ms with about 1,700 processes on an M4 Max), so a caller
/// that polls should do so at a cadence it is happy to pay for. The
/// prefix's `wineserver` inherits the environment of the session that
/// started it and so is reported under that session's id for as long as it
/// runs, including a later session's launches into the same prefix.
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

/// The [`Process`] for `record` when it is owned by `uid` and belongs to
/// `session_id`; `None` otherwise. An `APPLEJACK_SESSION=` entry in
/// `environ` decides when there is one, and `cmd` is then all arguments.
/// Without one (Wine's argv rewrite, see the module doc) the last
/// `APPLEJACK_SESSION=` entry of `cmd` decides, and entries from it on are
/// not arguments and take no part in the executable name. So a process of
/// another session that was given this id as a literal argument is not
/// matched: its `environ` or its own folded entry names that session. A
/// same-uid process with no session of its own, under Wine (a sessionless
/// `cmd.exe`) or not (`pgrep -f APPLEJACK_SESSION=<id>`), whose arguments
/// end in the exact entry is matched, since nothing else tells it apart.
#[must_use]
pub fn match_record(record: &ProcessRecord<'_>, session_id: &str, uid: u32) -> Option<Process> {
    if record.uid != Some(uid) {
        return None;
    }
    let mut wanted = Vec::with_capacity(SESSION_VARIABLE.len() + 1 + session_id.len());
    wanted.extend_from_slice(SESSION_VARIABLE.as_bytes());
    wanted.push(b'=');
    wanted.extend_from_slice(session_id.as_bytes());
    let names_a_session = |entry: &OsString| {
        entry
            .as_bytes()
            .starts_with(&wanted[..=SESSION_VARIABLE.len()])
    };
    let is_session = |entry: &OsString| entry.as_bytes() == wanted.as_slice();
    let cmd = if let Some(entry) = record.environ.iter().find(|entry| names_a_session(entry)) {
        is_session(entry).then_some(record.cmd)?
    } else {
        let at = record.cmd.iter().rposition(names_a_session)?;
        is_session(&record.cmd[at]).then_some(&record.cmd[..at])?
    };
    Some(Process {
        pid: record.pid,
        exe: executable_name(cmd, record.exe),
        session_id: session_id.to_owned(),
    })
}

/// The Windows executable's file name (Wine accepts both `\` and `/`
/// separators): argv\[0\] when it names a `.exe` other than `start.exe`
/// (after Wine's rewrite argv\[0\] is the process's own image, so
/// `launcher.exe --run patcher.exe` is `launcher.exe`); else the first
/// later argv entry naming a `.exe` (`wine Z:\g\game.exe` before the
/// rewrite, and `start.exe /exec title.exe`, which is about to become
/// `title.exe`), so a program's own arguments do not name it in those forms
/// either; else argv\[0\]'s file name; else the exec path's file name.
#[must_use]
pub fn executable_name(cmd: &[OsString], exe: Option<&Path>) -> String {
    let is_exe = |arg: &OsString| {
        let bytes = arg.as_bytes();
        bytes.len() > 4 && bytes[bytes.len() - 4..].eq_ignore_ascii_case(b".exe")
    };
    let own_image = cmd
        .first()
        .filter(|arg| is_exe(arg) && !windows_file_name(arg).eq_ignore_ascii_case("start.exe"));
    let chosen = own_image
        .or_else(|| cmd.iter().skip(1).find(|arg| is_exe(arg)))
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

/// Where Wine keeps its server directories: `<SERVER_BASE>/.wine-<uid>/`.
/// Wine hard-codes `/tmp`, creates both that directory and each
/// `server-<dev>-<inode>` below it with mode 0700, and refuses to start
/// when either is accessible to the group or others. The engine never
/// creates them; a host where something else made `/tmp/.wine-<uid>` with
/// another mode is repaired with `chmod 700 /tmp/.wine-$(id -u)`.
pub const SERVER_BASE: &str = "/tmp";

/// Where the `wineserver` for `prefix_dir` listens:
/// [`SERVER_BASE`]`/.wine-<uid>/server-<dev>-<inode>/socket`, with the
/// prefix directory's device and inode in hex, as Wine derives it.
///
/// # Errors
///
/// Returns [`Error::Io`] when the prefix directory cannot be stat'ed.
pub fn server_socket(prefix_dir: &Path) -> Result<PathBuf> {
    server_socket_in(Path::new(SERVER_BASE), prefix_dir)
}

/// [`server_socket`] with `base` in place of [`SERVER_BASE`], so a test
/// can stand a listener up under a directory of its own instead of Wine's.
///
/// # Errors
///
/// Returns [`Error::Io`] when the prefix directory cannot be stat'ed.
pub fn server_socket_in(base: &Path, prefix_dir: &Path) -> Result<PathBuf> {
    let meta = fs::metadata(prefix_dir).map_err(|error| Error::io(prefix_dir, error))?;
    let uid = rustix::process::getuid().as_raw();
    Ok(base.join(format!(
        ".wine-{uid}/server-{:x}-{:x}/socket",
        meta.dev(),
        meta.ino()
    )))
}

/// True when a `wineserver` is listening for `prefix_dir`: the canonical
/// "is anything running in this prefix" check. A connect to the socket
/// succeeds only while the server is alive, so a socket file left by a
/// killed server reads as not alive; the server drops the unused
/// connection. A prefix directory that no longer exists has no inode a
/// server could be keyed on, so it reads as not alive rather than failing.
///
/// # Errors
///
/// Returns [`Error::Io`] when the prefix directory exists but cannot be
/// stat'ed.
pub fn wineserver_alive(prefix_dir: &Path) -> Result<bool> {
    wineserver_alive_in(Path::new(SERVER_BASE), prefix_dir)
}

/// [`wineserver_alive`] against the socket under `base` (see
/// [`server_socket_in`]).
///
/// # Errors
///
/// Returns [`Error::Io`] when the prefix directory exists but cannot be
/// stat'ed.
pub fn wineserver_alive_in(base: &Path, prefix_dir: &Path) -> Result<bool> {
    let socket = match server_socket_in(base, prefix_dir) {
        Ok(socket) => socket,
        Err(Error::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    Ok(UnixStream::connect(socket).is_ok())
}
