//! Advisory file locks under `<data>/locks/<name>.lock` that serialize
//! applejack processes. They say nothing about the Windows Steam client: the
//! ownership rule in `docs/steam-layer.md` uses `wineserver_alive` for that.
//!
//! Lock order is per-appid first, then the library. Within one process the
//! types enforce it: [`LockDir::app`] borrows the [`LockDir`] mutably for as
//! long as the [`AppLock`] lives, so one `LockDir` holds at most one app lock,
//! and [`AppLock::library`] borrows the [`AppLock`] mutably for as long as the
//! [`LibraryLock`] lives, so one `AppLock` holds at most one library lock.
//! Across processes the order is a convention every applejack subcommand
//! follows.
//!
//! ```no_run
//! use applejack_steam::lock::{LockDir, Mode};
//!
//! let mut locks = LockDir::new("/path/to/data", "install");
//! let mut app = locks.app(730, Mode::NonBlocking)?;
//! let library = app.library(Mode::Wait(std::time::Duration::from_secs(5)))?;
//! # drop(library);
//! # Ok::<(), applejack_steam::Error>(())
//! ```

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::marker::PhantomData;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rustix::fs::{FlockOperation, flock};
use rustix::io::Errno;
use rustix::process::{Pid, test_kill_process};
use serde::{Deserialize, Serialize};

use crate::Error;

/// Whether `acquire` waits for a held lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Wait without bound. Use only where a hung holder cannot wedge the
    /// caller.
    Blocking,
    /// Fail at once with [`Error::LockHeld`].
    NonBlocking,
    /// Poll for up to the duration, then fail with [`Error::LockHeld`] naming
    /// the holder at the time of the last poll.
    Wait(Duration),
}

/// The one JSON line a holder writes into its lock file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub pid: u32,
    /// The fixed subcommand label given to [`LockDir::new`].
    pub command: String,
    /// Seconds since the Unix epoch.
    pub acquired_at: u64,
}

/// Interval between polls in [`Mode::Wait`].
const WAIT_POLL: Duration = Duration::from_millis(25);

/// Times `acquire` reopens the lock file after finding it unlinked or
/// replaced under a lock it just took.
const REPLACED_RETRIES: u32 = 8;

const LIBRARY_LOCK_NAME: &str = "library";

/// An exclusive `flock` on `<dir>/<name>.lock`, released on drop.
#[derive(Debug)]
pub struct Lock {
    name: String,
    path: PathBuf,
    file: File,
}

impl Lock {
    /// Creates `dir` if needed, locks `<dir>/<name>.lock`, and records the
    /// holder. Only [`LockDir`] and [`AppLock`] call this, which is what keeps
    /// the lock order in the types.
    pub(crate) fn acquire(
        dir: &Path,
        name: &str,
        mode: Mode,
        command: &str,
    ) -> Result<Self, Error> {
        let path = dir.join(format!("{name}.lock"));
        let lock_io = |source: io::Error| Error::LockIo {
            name: name.to_owned(),
            path: path.clone(),
            source,
        };
        let deadline = match mode {
            // `None` here means the limit lies past any representable
            // instant: poll without a deadline rather than panic.
            Mode::Wait(limit) => Instant::now().checked_add(limit),
            Mode::Blocking | Mode::NonBlocking => None,
        };
        let mut replaced = 0;
        loop {
            // Inside the loop so a `locks/` directory removed while waiting
            // is recreated on the next attempt.
            std::fs::create_dir_all(dir).map_err(lock_io)?;
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(&path)
                .map_err(lock_io)?;
            if !try_flock(&file, mode, deadline).map_err(lock_io)? {
                return Err(Error::LockHeld {
                    name: name.to_owned(),
                    holder: read_holder(&mut file),
                });
            }
            // `flock` locks the inode. If the path was unlinked or replaced in
            // the meantime, this lock guards an orphan and a second holder
            // could lock the new file; reopen and lock the current one.
            if !same_inode(&file, &path) {
                replaced += 1;
                if replaced > REPLACED_RETRIES {
                    return Err(lock_io(io::Error::other(
                        "lock file keeps being replaced while locking",
                    )));
                }
                continue;
            }
            let holder = Holder {
                pid: std::process::id(),
                command: command.to_owned(),
                acquired_at: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |elapsed| elapsed.as_secs()),
            };
            write_holder(&mut file, &holder).map_err(lock_io)?;
            return Ok(Self {
                name: name.to_owned(),
                path,
                file,
            });
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads the holder record of an existing lock file without locking it.
    /// `None` when the file is absent or empty, the line does not parse, or
    /// the recorded pid is no longer alive. Meaningful only alongside a
    /// failed acquire: the holder may release at any moment.
    #[must_use]
    pub fn holder(dir: &Path, name: &str) -> Option<Holder> {
        let mut file = File::open(dir.join(format!("{name}.lock"))).ok()?;
        read_holder(&mut file)
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Truncate while still holding the lock so a contender never reads
        // this process as the holder after release. Closing the descriptor
        // would also unlock; the explicit unlock keeps the release from
        // depending on the drop order of the handle.
        let _ = self.file.set_len(0);
        let _ = flock(&self.file, FlockOperation::Unlock);
    }
}

/// `Ok(true)` when the lock is taken, `Ok(false)` when another process holds
/// it and `mode` does not wait any longer. A [`Mode::Wait`] without a
/// `deadline` polls until the lock is free.
fn try_flock(file: &File, mode: Mode, deadline: Option<Instant>) -> io::Result<bool> {
    let operation = match mode {
        Mode::Blocking => FlockOperation::LockExclusive,
        Mode::NonBlocking | Mode::Wait(_) => FlockOperation::NonBlockingLockExclusive,
    };
    loop {
        match flock(file, operation) {
            Ok(()) => return Ok(true),
            Err(Errno::WOULDBLOCK) => {
                if !matches!(mode, Mode::Wait(_)) {
                    return Ok(false);
                }
                let remaining = deadline.map_or(WAIT_POLL, |deadline| {
                    deadline.saturating_duration_since(Instant::now())
                });
                if remaining.is_zero() {
                    return Ok(false);
                }
                std::thread::sleep(remaining.min(WAIT_POLL));
            }
            Err(errno) => return Err(io::Error::from(errno)),
        }
    }
}

fn same_inode(file: &File, path: &Path) -> bool {
    let (Ok(locked), Ok(current)) = (file.metadata(), std::fs::metadata(path)) else {
        return false;
    };
    locked.dev() == current.dev() && locked.ino() == current.ino()
}

fn read_holder(file: &mut File) -> Option<Holder> {
    let mut text = String::new();
    file.rewind().ok()?;
    file.read_to_string(&mut text).ok()?;
    let holder: Holder = serde_json::from_str(text.lines().next()?.trim()).ok()?;
    process_alive(holder.pid).then_some(holder)
}

/// `kill(pid, 0)`: `ESRCH` means the recorded holder has exited (the line is
/// stale, left by a crash). `EPERM` means it exists under another uid.
fn process_alive(pid: u32) -> bool {
    let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
        return false;
    };
    !matches!(test_kill_process(pid), Err(Errno::SRCH))
}

fn write_holder(file: &mut File, holder: &Holder) -> io::Result<()> {
    let mut line = serde_json::to_string(holder).map_err(io::Error::other)?;
    line.push('\n');
    file.set_len(0)?;
    file.rewind()?;
    file.write_all(line.as_bytes())?;
    file.sync_data()
}

/// `<data>/locks`, plus the command label every lock taken through it carries.
/// One `LockDir` holds at most one [`AppLock`] at a time. A subcommand that
/// needs several titles takes all of its app locks first, in ascending appid
/// order (one `LockDir` per app lock), and the library lock last, from the
/// `AppLock` it took last; it never takes an app lock while holding the
/// library lock.
#[derive(Debug)]
pub struct LockDir {
    root: PathBuf,
    command: String,
}

impl LockDir {
    /// `data_dir` is the applejack data directory; locks live in its `locks/`
    /// subdirectory. `command` is a fixed subcommand label such as `install`
    /// or `run`, written verbatim into the lock file for other processes'
    /// error messages. Never pass argv or anything derived from user input.
    pub fn new(data_dir: impl AsRef<Path>, command: impl Into<String>) -> Self {
        Self {
            root: data_dir.as_ref().join("locks"),
            command: command.into(),
        }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Takes the per-appid lock, the first lock in the order. The result
    /// borrows this `LockDir` mutably, so a second app lock cannot be taken
    /// through it until the first is dropped:
    ///
    /// ```compile_fail,E0499
    /// use applejack_steam::lock::{LockDir, Mode};
    ///
    /// let mut locks = LockDir::new("/path/to/data", "install");
    /// let mut first = locks.app(1, Mode::NonBlocking).unwrap();
    /// let library = first.library(Mode::NonBlocking).unwrap();
    /// let second = locks.app(2, Mode::NonBlocking).unwrap(); // `locks` is still borrowed
    /// drop(library);
    /// drop(first);
    /// drop(second);
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::LockHeld`] when `mode` is [`Mode::NonBlocking`] or
    /// [`Mode::Wait`] and another process holds the lock; [`Error::LockIo`]
    /// for filesystem failures.
    pub fn app(&mut self, appid: u32, mode: Mode) -> Result<AppLock<'_>, Error> {
        let lock = Lock::acquire(&self.root, &app_lock_name(appid), mode, &self.command)?;
        Ok(AppLock {
            lock,
            dir: self,
            appid,
        })
    }
}

#[must_use]
pub fn app_lock_name(appid: u32) -> String {
    format!("app-{appid}")
}

/// The per-appid lock. Holding one is the only way to take the library lock.
#[derive(Debug)]
pub struct AppLock<'d> {
    lock: Lock,
    dir: &'d LockDir,
    appid: u32,
}

impl AppLock<'_> {
    #[must_use]
    pub fn appid(&self) -> u32 {
        self.appid
    }

    #[must_use]
    pub fn lock(&self) -> &Lock {
        &self.lock
    }

    /// Takes the library lock, the second lock in the order. The result borrows
    /// this lock mutably, so it cannot outlive the per-appid lock and a second
    /// library lock cannot be taken until it is dropped. `flock` conflicts
    /// between two open descriptions of one file even inside one process, so
    /// a second call would otherwise report the caller's own pid as the holder
    /// or, in [`Mode::Blocking`], wait on itself forever:
    ///
    /// ```compile_fail,E0499
    /// use applejack_steam::lock::{LockDir, Mode};
    ///
    /// let mut locks = LockDir::new("/path/to/data", "install");
    /// let mut app = locks.app(1, Mode::NonBlocking).unwrap();
    /// let first = app.library(Mode::NonBlocking).unwrap();
    /// let second = app.library(Mode::NonBlocking).unwrap(); // `app` is still borrowed
    /// drop(first);
    /// drop(second);
    /// ```
    ///
    /// # Errors
    ///
    /// See [`LockDir::app`].
    pub fn library(&mut self, mode: Mode) -> Result<LibraryLock<'_>, Error> {
        let lock = Lock::acquire(&self.dir.root, LIBRARY_LOCK_NAME, mode, &self.dir.command)?;
        Ok(LibraryLock {
            lock,
            _app: PhantomData,
        })
    }
}

/// The shared library lock, held under an [`AppLock`]. The marker carries the
/// exclusive borrow of the `AppLock` without tying the two lifetimes together,
/// which `&'a mut AppLock<'a>` would do.
#[derive(Debug)]
pub struct LibraryLock<'a> {
    lock: Lock,
    _app: PhantomData<&'a mut ()>,
}

impl LibraryLock<'_> {
    #[must_use]
    pub fn lock(&self) -> &Lock {
        &self.lock
    }
}
