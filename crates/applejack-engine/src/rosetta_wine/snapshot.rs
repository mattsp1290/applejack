//! Prefix snapshots: `<snapshots_root>/<name>/<timestamp>/` copies of a
//! prefix directory made with `cp -c -R` (an APFS clone when the volume
//! supports it; `cp` itself falls back to a byte copy across volumes, and a
//! `cp -R` without the flag is the fallback when `-c` is refused). Restore
//! and reset take a safety snapshot of the current state first.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Error, Result};

use super::clock::Utc;
use super::process::wineserver_alive;

/// Snapshots kept per prefix name by default.
pub const DEFAULT_KEEP: usize = 3;

/// The part of a prefix copied back by a reset with `keep_user_data`.
pub const USER_DATA_DIR: &str = "drive_c/users";

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
    let base = Utc::now().compact();
    let mut timestamp = base.clone();
    let mut dir = parent.join(&timestamp);
    let mut n = 0;
    while fs::symlink_metadata(&dir).is_ok() {
        n += 1;
        // Zero-padded so `list`'s string order is creation order past `-9`.
        timestamp = format!("{base}-{n:03}");
        dir = parent.join(&timestamp);
    }
    copy_tree(prefix_dir, &dir)?;
    Ok(Snapshot {
        name: name.to_owned(),
        timestamp,
        dir,
    })
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
/// snapshot is copied to `<prefix_dir>.restoring` first and swapped in with
/// two renames, so an interrupted restore leaves the prefix either as it
/// was or fully restored. Refused while a `wineserver` runs for the prefix:
/// the swap gives the path a new inode, and Wine keys the server socket on
/// the inode, so a running session would keep writing into the unlinked
/// directory and `wineserver_alive` would no longer see it.
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
    refuse_if_running(prefix_dir)?;
    let safety = create(prefix_dir, snapshots_root, name)?;
    let staging = sibling(prefix_dir, ".restoring");
    let replaced = sibling(prefix_dir, ".replaced");
    crate::bundle::remove_stale(&staging)?;
    crate::bundle::remove_stale(&replaced)?;
    copy_tree(&source.dir, &staging)?;
    fs::rename(prefix_dir, &replaced).map_err(|error| Error::io(prefix_dir, error))?;
    if let Err(error) = fs::rename(&staging, prefix_dir) {
        let _ = fs::rename(&replaced, prefix_dir);
        let _ = fs::remove_dir_all(&staging);
        return Err(Error::io(prefix_dir, error));
    }
    fs::remove_dir_all(&replaced).map_err(|error| Error::io(&replaced, error))?;
    Ok(safety)
}

/// Takes a safety snapshot, removes `prefix_dir`, calls `recreate` to build
/// a fresh prefix at the same path, and with `keep_user_data` copies
/// `drive_c/users/` back from the safety snapshot. Returns the safety
/// snapshot. If `recreate` fails the prefix is gone and the safety
/// snapshot holds the previous state. Refused while a `wineserver` runs
/// for the prefix, for the reason given at [`restore`].
///
/// # Errors
///
/// Returns [`Error::PrefixUnusable`] while a server runs, else the first
/// failure.
pub fn reset(
    prefix_dir: &Path,
    snapshots_root: &Path,
    name: &str,
    keep_user_data: bool,
    recreate: &mut dyn FnMut(&Path) -> Result<()>,
) -> Result<Snapshot> {
    refuse_if_running(prefix_dir)?;
    let safety = create(prefix_dir, snapshots_root, name)?;
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
    Ok(safety)
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

fn sibling(prefix_dir: &Path, suffix: &str) -> PathBuf {
    let mut name = prefix_dir
        .file_name()
        .map(ToOwned::to_owned)
        .unwrap_or_default();
    name.push(suffix);
    prefix_dir.with_file_name(name)
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
/// the fallback and on failure.
pub(crate) fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
    }
    let mut last = None;
    for flags in [&["-c", "-R"][..], &["-R"][..]] {
        let output = Command::new("/bin/cp")
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
        last = Some(io::Error::other(format!(
            "cp {} failed ({}): {}",
            flags.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Err(Error::io(
        dst,
        last.unwrap_or_else(|| io::Error::other("cp failed")),
    ))
}
