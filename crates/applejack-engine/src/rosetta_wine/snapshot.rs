//! Prefix snapshots: `<snapshots_root>/<name>/<timestamp>/` copies of a
//! prefix directory made with `cp -c -R` (an APFS clone when the volume
//! supports it; `cp` itself falls back to a byte copy across volumes, and a
//! `cp -R` without the flag is the fallback when `-c` is refused). Restore
//! and reset take a safety snapshot of the current state first. A snapshot
//! is copied to a dot-prefixed staging directory and renamed into place, so
//! a half-made copy never counts as one. [`PrefixLock`] serializes the
//! callers of one prefix.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{Error, Result};

use super::clock::Utc;
use super::process::wineserver_alive;

/// Snapshots kept per prefix name by default.
pub const DEFAULT_KEEP: usize = 3;

/// The part of a prefix copied back by a reset with `keep_user_data`.
pub const USER_DATA_DIR: &str = "drive_c/users";

const LOCKS_DIR: &str = ".locks";

const STAGING_PREFIX: &str = ".staging-";

const RESTORING: &str = "restoring";

const REPLACED: &str = "replaced";

const DISCARDING: &str = "discarding";

static NEXT_STAGING: AtomicU64 = AtomicU64::new(0);

/// Exclusive for one prefix name across every thread and process, held for
/// the whole of an operation that changes or runs in the prefix. It lives
/// beside the snapshots, not in the prefix, because those operations delete
/// and replace the prefix directory. Released on drop.
#[derive(Debug)]
pub struct PrefixLock {
    name: String,
    file: fs::File,
}

impl PrefixLock {
    /// Takes `<snapshots_root>/.locks/<name>.lock` without waiting, and
    /// settles what a killed snapshot or restore left, which nothing can
    /// still be using once the lock is held: a restore killed between its
    /// two renames has its prefix put back, and every working directory is
    /// cleared. What goes is either the snapshot being installed or state
    /// the safety snapshot already holds.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PrefixBusy`] when another operation holds it,
    /// [`Error::InvalidSnapshotName`], or [`Error::Io`].
    pub fn acquire(prefix_dir: &Path, snapshots_root: &Path, name: &str) -> Result<Self> {
        check_name(name)?;
        let dir = snapshots_root.join(LOCKS_DIR);
        fs::create_dir_all(&dir).map_err(|error| Error::io(&dir, error))?;
        let path = dir.join(format!("{name}.lock"));
        let file = fs::File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|error| Error::io(&path, error))?;
        match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(rustix::io::Errno::WOULDBLOCK) => {
                return Err(Error::PrefixBusy {
                    name: name.to_owned(),
                });
            }
            Err(error) => return Err(Error::io(path, error.into())),
        }
        sweep_staging(&snapshots_root.join(name));
        settle_restore(prefix_dir);
        Ok(Self {
            name: name.to_owned(),
            file,
        })
    }

    /// The prefix this lock holds.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for PrefixLock {
    /// Closing is not enough: a child being spawned on another thread holds
    /// a copy of the descriptor until it execs, and the lock would outlive
    /// this drop by that long, refusing the next operation.
    fn drop(&mut self) {
        let _ = rustix::fs::flock(&self.file, rustix::fs::FlockOperation::Unlock);
    }
}

fn sweep_staging(parent: &Path) {
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(STAGING_PREFIX)
        {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// A `replaced` copy with no prefix means a kill fell between the renames of
/// a restore: it is the whole prefix, so it goes back. In any other state
/// the copy is finished with.
fn settle_restore(prefix_dir: &Path) {
    let gone = fs::symlink_metadata(prefix_dir)
        .is_err_and(|error| error.kind() == io::ErrorKind::NotFound);
    if gone {
        let _ = fs::rename(work_dir(prefix_dir, REPLACED), prefix_dir);
    }
    let _ = discard_replaced(prefix_dir);
    let _ = crate::bundle::remove_stale(&work_dir(prefix_dir, RESTORING));
}

/// Renames before deleting: a deletion can stop part way, and only a whole
/// prefix may keep the name [`settle_restore`] puts back.
fn discard_replaced(prefix_dir: &Path) -> Result<()> {
    let replaced = work_dir(prefix_dir, REPLACED);
    let discarding = work_dir(prefix_dir, DISCARDING);
    crate::bundle::remove_stale(&discarding)?;
    match fs::rename(&replaced, &discarding) {
        Ok(()) => crate::bundle::remove_stale(&discarding),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::io(&replaced, error)),
    }
}

/// One snapshot directory.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Snapshot {
    /// The prefix name the snapshot belongs to.
    pub name: String,
    /// `YYYYMMDDTHHMMSSZ`, with `-N` appended when the second is taken.
    pub timestamp: String,
    /// `<snapshots_root>/<name>/<timestamp>`.
    pub dir: PathBuf,
}

/// Copies `prefix_dir` to a new snapshot under `snapshots_root/<name>/`.
///
/// # Errors
///
/// Returns [`Error::InvalidSnapshotName`] for a name that cannot be a
/// directory, or [`Error::Io`] from the copy.
pub fn create(prefix_dir: &Path, snapshots_root: &Path, name: &str) -> Result<Snapshot> {
    check_name(name)?;
    if !prefix_dir.is_dir() {
        return Err(Error::io(
            prefix_dir,
            io::Error::new(io::ErrorKind::NotFound, "prefix directory does not exist"),
        ));
    }
    let parent = snapshots_root.join(name);
    fs::create_dir_all(&parent).map_err(|error| Error::io(&parent, error))?;
    let staging = parent.join(format!(
        "{STAGING_PREFIX}{}-{}",
        std::process::id(),
        NEXT_STAGING.fetch_add(1, Ordering::Relaxed)
    ));
    crate::bundle::remove_stale(&staging)?;
    copy_tree(prefix_dir, &staging)?;
    let (timestamp, dir) = publish(&staging, &parent).inspect_err(|_| {
        let _ = fs::remove_dir_all(&staging);
    })?;
    Ok(Snapshot {
        name: name.to_owned(),
        timestamp,
        dir,
    })
}

/// Renames `staging` to the first free `<timestamp>` under `parent`. The
/// rename, not the check before it, decides: a directory that is not empty
/// cannot be replaced, so a caller that lost the race takes the next name.
fn publish(staging: &Path, parent: &Path) -> Result<(String, PathBuf)> {
    let base = Utc::now().compact();
    let mut timestamp = base.clone();
    let mut n = 0;
    loop {
        let dir = parent.join(&timestamp);
        if fs::symlink_metadata(&dir).is_err() {
            match fs::rename(staging, &dir) {
                Ok(()) => return Ok((timestamp, dir)),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::AlreadyExists
                    ) => {}
                Err(error) => return Err(Error::io(&dir, error)),
            }
        }
        n += 1;
        // Zero-padded so `list`'s string order is creation order past `-9`.
        timestamp = format!("{base}-{n:03}");
    }
}

/// The snapshots of `name`, oldest first. A missing directory is an empty
/// list.
///
/// # Errors
///
/// Returns [`Error::InvalidSnapshotName`] or [`Error::Io`].
pub fn list(snapshots_root: &Path, name: &str) -> Result<Vec<Snapshot>> {
    check_name(name)?;
    let parent = snapshots_root.join(name);
    let entries = match fs::read_dir(&parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(Error::io(&parent, error)),
    };
    let mut snapshots = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| Error::io(&parent, error))?;
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        let Some(timestamp) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if is_dir && !timestamp.starts_with('.') {
            snapshots.push(Snapshot {
                name: name.to_owned(),
                timestamp,
                dir: entry.path(),
            });
        }
    }
    snapshots.sort();
    Ok(snapshots)
}

/// Replaces `prefix_dir` with the snapshot `<name>/<timestamp>` after
/// taking a safety snapshot of the current state, which is returned. The
/// snapshot is copied aside first and swapped in with two renames. After a
/// kill between them [`PrefixLock::acquire`] puts the replaced prefix back.
/// With `prefix_dir` gone there is nothing to protect: the snapshot is
/// installed and is itself returned.
/// This is the recovery after a kill, a removal, or a failed reset. Refused
/// while a `wineserver` runs for the prefix: the swap gives the path a new
/// inode, and Wine keys the server socket on the inode, so a running session
/// would keep writing into the unlinked directory and `wineserver_alive`
/// would no longer see it.
///
/// # Errors
///
/// Returns [`Error::SnapshotNotFound`], [`Error::PrefixUnusable`] while a
/// server runs, or [`Error::Io`] from the copy or swap.
pub fn restore(
    prefix_dir: &Path,
    snapshots_root: &Path,
    name: &str,
    timestamp: &str,
) -> Result<Snapshot> {
    let source = find(snapshots_root, name, timestamp)?;
    let staging = work_dir(prefix_dir, RESTORING);
    let present = prefix_dir
        .try_exists()
        .map_err(|error| Error::io(prefix_dir, error))?;
    if !present {
        crate::bundle::remove_stale(&staging)?;
        copy_tree(&source.dir, &staging)?;
        fs::rename(&staging, prefix_dir).map_err(|error| {
            let _ = fs::remove_dir_all(&staging);
            Error::io(prefix_dir, error)
        })?;
        return Ok(source);
    }
    refuse_if_running(prefix_dir)?;
    let safety = create(prefix_dir, snapshots_root, name)?;
    let replaced = work_dir(prefix_dir, REPLACED);
    crate::bundle::remove_stale(&staging)?;
    discard_replaced(prefix_dir)?;
    copy_tree(&source.dir, &staging)?;
    fs::rename(prefix_dir, &replaced).map_err(|error| Error::io(prefix_dir, error))?;
    if let Err(error) = fs::rename(&staging, prefix_dir) {
        let _ = fs::rename(&replaced, prefix_dir);
        let _ = fs::remove_dir_all(&staging);
        return Err(Error::io(prefix_dir, error));
    }
    discard_replaced(prefix_dir)?;
    Ok(safety)
}

/// Takes a safety snapshot, removes `prefix_dir`, calls `recreate` to build
/// a fresh prefix at the same path, and with `keep_user_data` copies
/// `drive_c/users/` back from the safety snapshot. Returns the safety
/// snapshot. If `recreate` fails the prefix is gone and the safety
/// snapshot holds the previous state, for [`restore`]. Refused while a
/// `wineserver` runs for the prefix, for the reason given at [`restore`].
///
/// # Errors
///
/// Returns [`Error::PrefixUnusable`] while a server runs, the failure of
/// the safety snapshot, or [`Error::ResetFailed`], naming that snapshot, for
/// anything after it.
pub fn reset(
    prefix_dir: &Path,
    snapshots_root: &Path,
    name: &str,
    keep_user_data: bool,
    recreate: &mut dyn FnMut(&Path) -> Result<()>,
) -> Result<Snapshot> {
    refuse_if_running(prefix_dir)?;
    let safety = create(prefix_dir, snapshots_root, name)?;
    match rebuild(prefix_dir, &safety, keep_user_data, recreate) {
        Ok(()) => Ok(safety),
        Err(source) => Err(Error::ResetFailed {
            name: name.to_owned(),
            timestamp: safety.timestamp,
            source: Box::new(source),
        }),
    }
}

fn rebuild(
    prefix_dir: &Path,
    safety: &Snapshot,
    keep_user_data: bool,
    recreate: &mut dyn FnMut(&Path) -> Result<()>,
) -> Result<()> {
    fs::remove_dir_all(prefix_dir).map_err(|error| Error::io(prefix_dir, error))?;
    recreate(prefix_dir)?;
    if keep_user_data {
        let saved = safety.dir.join(USER_DATA_DIR);
        let target = prefix_dir.join(USER_DATA_DIR);
        if saved.is_dir() {
            crate::bundle::remove_stale(&target)?;
            copy_tree(&saved, &target)?;
        }
    }
    Ok(())
}

/// Removes all but the newest `keep` snapshots of `name` and returns what
/// was removed, oldest first.
///
/// # Errors
///
/// Returns [`Error::InvalidSnapshotName`] or [`Error::Io`].
pub fn prune(snapshots_root: &Path, name: &str, keep: usize) -> Result<Vec<Snapshot>> {
    let snapshots = list(snapshots_root, name)?;
    let excess = snapshots.len().saturating_sub(keep);
    let mut removed = Vec::with_capacity(excess);
    for snapshot in snapshots.into_iter().take(excess) {
        fs::remove_dir_all(&snapshot.dir).map_err(|error| Error::io(&snapshot.dir, error))?;
        removed.push(snapshot);
    }
    Ok(removed)
}

fn refuse_if_running(prefix_dir: &Path) -> Result<()> {
    if wineserver_alive(prefix_dir)? {
        return Err(Error::PrefixUnusable {
            path: prefix_dir.to_path_buf(),
            reason: "a wineserver is running for it, and replacing the directory would change the \
                     inode Wine keys the server socket on; cancel the session first",
        });
    }
    Ok(())
}

fn find(snapshots_root: &Path, name: &str, timestamp: &str) -> Result<Snapshot> {
    list(snapshots_root, name)?
        .into_iter()
        .find(|snapshot| snapshot.timestamp == timestamp)
        .ok_or_else(|| Error::SnapshotNotFound {
            name: name.to_owned(),
            timestamp: timestamp.to_owned(),
        })
}

/// A directory for `restore` to work in, beside the prefix because a rename
/// cannot cross a volume and the snapshots may be on another. [`check_name`]
/// refuses a leading dot, so no prefix has this name and none is listed.
fn work_dir(prefix_dir: &Path, tag: &str) -> PathBuf {
    let mut leaf = OsString::from(format!("{STAGING_PREFIX}{tag}-"));
    leaf.push(prefix_dir.file_name().unwrap_or_default());
    prefix_dir.with_file_name(leaf)
}

/// Refuses a prefix name that cannot be a directory under `prefixes/`.
///
/// # Errors
///
/// Returns [`Error::InvalidSnapshotName`] with the rule that failed.
pub fn check_name(name: &str) -> Result<()> {
    let reason = if name.is_empty() {
        Some("empty")
    } else if name == "." || name == ".." {
        Some("must not be `.` or `..`")
    } else if name.starts_with('.') {
        Some("must not start with `.`")
    } else if name.contains('/') {
        Some("must not contain `/`")
    } else if name.contains('\0') {
        Some("must not contain a NUL byte")
    } else {
        None
    };
    match reason {
        Some(reason) => Err(Error::InvalidSnapshotName {
            name: name.to_owned(),
            reason,
        }),
        None => Ok(()),
    }
}

/// Copies the directory `src` to the not-yet-existing `dst`, symlinks as
/// symlinks: `cp -c -R` first (clone on APFS, byte copy elsewhere), then
/// `cp -R` if the flag itself is refused. A partial `dst` is removed before
/// the fallback and on failure, so `dst` must be the caller's alone.
pub(crate) fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
    }
    let mut last = None;
    for flags in [&["-c", "-R"][..], &["-R"][..]] {
        let output = Command::new("/bin/cp")
            // `cp_error_kind` reads the message.
            .env("LC_ALL", "C")
            .args(flags)
            .arg(src)
            .arg(dst)
            .output()
            .map_err(|error| Error::Spawn {
                program: PathBuf::from("/bin/cp"),
                source: error,
            })?;
        if output.status.success() {
            return Ok(());
        }
        let _ = fs::remove_dir_all(dst);
        let stderr = String::from_utf8_lossy(&output.stderr);
        last = Some(io::Error::new(
            cp_error_kind(&stderr),
            format!(
                "cp {} failed ({}): {}",
                flags.join(" "),
                output.status,
                stderr.trim()
            ),
        ));
    }
    Err(Error::io(
        dst,
        last.unwrap_or_else(|| io::Error::other("cp failed")),
    ))
}

fn cp_error_kind(stderr: &str) -> io::ErrorKind {
    if stderr.contains("No space left on device") {
        io::ErrorKind::StorageFull
    } else {
        io::ErrorKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_disk_stays_a_full_disk_through_cp() {
        assert_eq!(
            cp_error_kind("cp: /Volumes/x/drive_c/a.dll: No space left on device\n"),
            io::ErrorKind::StorageFull
        );
        assert_eq!(
            cp_error_kind("cp: /x: Permission denied\n"),
            io::ErrorKind::Other
        );
    }
}
