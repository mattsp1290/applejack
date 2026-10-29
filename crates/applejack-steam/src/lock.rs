//! Advisory file locks under `<data>/locks/<name>.lock` that serialize
//! applejack processes. They say nothing about the Windows Steam client: the
//! ownership rule in `docs/steam-layer.md` uses `wineserver_alive` for that.
//!
//! Lock order is per-appid first, then the library. The type system encodes
//! it: a [`LibraryLock`] exists only as a borrow of an [`AppLock`], obtained
//! through [`AppLock::library`].

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{FlockOperation, flock};
use serde::{Deserialize, Serialize};

use crate::Error;

/// Whether `acquire` waits for a held lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Blocking,
    NonBlocking,
}

/// The one JSON line a holder writes into its lock file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub pid: u32,
    pub command: String,
    /// Seconds since the Unix epoch.
    pub acquired_at: u64,
}

/// An exclusive `flock` on `<dir>/<name>.lock`, released on drop.
#[derive(Debug)]
pub struct Lock {
    name: String,
    path: PathBuf,
    file: File,
}

impl Lock {
    /// Creates `dir` if needed, locks `<dir>/<name>.lock`, and records the
    /// holder. `command` labels the holding process for other processes'
    /// error messages.
    ///
    /// # Errors
    ///
    /// [`Error::LockHeld`] when `mode` is [`Mode::NonBlocking`] and another
    /// process holds the lock; [`Error::LockIo`] for filesystem failures.
    pub fn acquire(dir: &Path, name: &str, mode: Mode, command: &str) -> Result<Self, Error> {
        let path = dir.join(format!("{name}.lock"));
        let lock_io = |source: io::Error| Error::LockIo {
            name: name.to_owned(),
            path: path.clone(),
            source,
        };
        std::fs::create_dir_all(dir).map_err(lock_io)?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(lock_io)?;
        let operation = match mode {
            Mode::Blocking => FlockOperation::LockExclusive,
            Mode::NonBlocking => FlockOperation::NonBlockingLockExclusive,
        };
        if let Err(errno) = flock(&file, operation) {
            let error = io::Error::from(errno);
            if mode == Mode::NonBlocking && error.kind() == io::ErrorKind::WouldBlock {
                let holder = read_holder(&mut file);
                return Err(Error::LockHeld {
                    name: name.to_owned(),
                    pid: holder.as_ref().map(|holder| holder.pid),
                    command: holder.map(|holder| holder.command),
                });
            }
            return Err(lock_io(error));
        }
        let holder = Holder {
            pid: std::process::id(),
            command: command.to_owned(),
            acquired_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs()),
        };
        write_holder(&mut file, &holder).map_err(lock_io)?;
        Ok(Self {
            name: name.to_owned(),
            path,
            file,
        })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads the holder record from an existing lock file without locking it.
    /// Stale after release; meaningful only alongside a failed acquire.
    #[must_use]
    pub fn holder(dir: &Path, name: &str) -> Option<Holder> {
        let mut file = File::open(dir.join(format!("{name}.lock"))).ok()?;
        read_holder(&mut file)
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Closing the descriptor also releases the lock; the explicit unlock
        // keeps the release from depending on drop order of the file handle.
        let _ = flock(&self.file, FlockOperation::Unlock);
    }
}

fn read_holder(file: &mut File) -> Option<Holder> {
    let mut text = String::new();
    file.rewind().ok()?;
    file.read_to_string(&mut text).ok()?;
    serde_json::from_str(text.lines().next()?).ok()
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
#[derive(Debug, Clone)]
pub struct LockDir {
    root: PathBuf,
    command: String,
}

impl LockDir {
    /// `data_dir` is the applejack data directory; locks live in its `locks/`
    /// subdirectory.
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

    /// Takes the per-appid lock, the first lock in the order.
    ///
    /// # Errors
    ///
    /// See [`Lock::acquire`].
    pub fn app(&self, appid: u32, mode: Mode) -> Result<AppLock, Error> {
        let lock = Lock::acquire(&self.root, &app_lock_name(appid), mode, &self.command)?;
        Ok(AppLock {
            lock,
            dir: self.clone(),
            appid,
        })
    }
}

#[must_use]
pub fn app_lock_name(appid: u32) -> String {
    format!("app-{appid}")
}

pub const LIBRARY_LOCK_NAME: &str = "library";

/// The per-appid lock. Holding one is the only way to take the library lock.
#[derive(Debug)]
pub struct AppLock {
    lock: Lock,
    dir: LockDir,
    appid: u32,
}

impl AppLock {
    #[must_use]
    pub fn appid(&self) -> u32 {
        self.appid
    }

    #[must_use]
    pub fn lock(&self) -> &Lock {
        &self.lock
    }

    /// Takes the library lock, the second lock in the order. The result borrows
    /// this lock, so it cannot outlive the per-appid lock.
    ///
    /// # Errors
    ///
    /// See [`Lock::acquire`].
    pub fn library(&self, mode: Mode) -> Result<LibraryLock<'_>, Error> {
        let lock = Lock::acquire(&self.dir.root, LIBRARY_LOCK_NAME, mode, &self.dir.command)?;
        Ok(LibraryLock {
            lock,
            _app: PhantomData,
        })
    }
}

/// The shared library lock, held under an [`AppLock`].
#[derive(Debug)]
pub struct LibraryLock<'a> {
    lock: Lock,
    _app: PhantomData<&'a AppLock>,
}

impl LibraryLock<'_> {
    #[must_use]
    pub fn lock(&self) -> &Lock {
        &self.lock
    }
}
