//! Installed engine versions: listing, the active selection, and pruning.
//!
//! The active version is recorded in `engines/<id>/ACTIVE` rather than the
//! config database because the database belongs to `applejack-config`, which
//! this crate never depends on. The core keeps the two in step.

use std::cmp::Ordering;
use std::fmt;
use std::fs;
use std::io;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{EngineId, EngineLayout, Error, Result};

/// An engine version string that doubles as a directory name under
/// `engines/<id>/`.
///
/// Ordering compares dot- and dash-separated segments left to right, numeric
/// segments by value and other segments lexically, so `10.0.0 < 10.0.1 <
/// 10.1.0` and `10.0.0-rc1 < 10.0.0`. The newest version is the greatest.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Version(String);

impl Version {
    /// Validates `value` as a version: non-empty, no path separators, not a
    /// dot-file, and not carrying a layout suffix.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidVersion`] with the rule that failed.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        let reason = if value.is_empty() {
            Some("empty")
        } else if value.starts_with('.') {
            Some("starts with `.`")
        } else if value.contains(['/', '\\']) || value.contains('\0') {
            Some("contains a path separator or NUL")
        } else if value.ends_with(EngineLayout::PARTIAL_SUFFIX)
            || value.ends_with(EngineLayout::OVERLAY_SUFFIX)
        {
            Some("ends with a reserved directory suffix")
        } else if value.chars().any(char::is_whitespace) {
            Some("contains whitespace")
        } else {
            None
        };
        match reason {
            Some(reason) => Err(Error::InvalidVersion {
                version: value,
                reason,
            }),
            None => Ok(Self(value)),
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn segments(&self) -> impl Iterator<Item = Segment<'_>> {
        self.0.split(['.', '-']).map(Segment::parse)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Segment<'a> {
    Number(u64),
    Text(&'a str),
}

impl<'a> Segment<'a> {
    fn parse(text: &'a str) -> Self {
        text.parse().map_or(Self::Text(text), Self::Number)
    }
}

impl Ord for Segment<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Number(a), Self::Number(b)) => a.cmp(b),
            (Self::Text(a), Self::Text(b)) => a.cmp(b),
            // Numbers sort after text so `10.0.0-rc1 < 10.0.0` when the
            // shorter side runs out (see `Ord for Version`).
            (Self::Number(_), Self::Text(_)) => Ordering::Greater,
            (Self::Text(_), Self::Number(_)) => Ordering::Less,
        }
    }
}

impl PartialOrd for Segment<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let mut a = self.segments();
        let mut b = other.segments();
        loop {
            match (a.next(), b.next()) {
                (None, None) => return self.0.cmp(&other.0),
                // A pre-release tag after the same numeric core sorts lower:
                // `10.0.0-rc1 < 10.0.0`; a longer numeric core sorts higher.
                (Some(Segment::Text(_)), None) | (None, Some(Segment::Number(_))) => {
                    return Ordering::Less;
                }
                (None, Some(Segment::Text(_))) | (Some(Segment::Number(_)), None) => {
                    return Ordering::Greater;
                }
                (Some(x), Some(y)) => match x.cmp(&y) {
                    Ordering::Equal => {}
                    other => return other,
                },
            }
        }
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Version {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::new(s)
    }
}

impl TryFrom<String> for Version {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl From<Version> for String {
    fn from(version: Version) -> Self {
        version.0
    }
}

/// Installed versions of `id`, oldest first. Extraction-in-progress and
/// overlay directories are not versions.
///
/// # Errors
///
/// Returns [`Error::Io`] when `engines/<id>/` exists but cannot be read.
pub fn list(layout: &EngineLayout, id: EngineId) -> Result<Vec<Version>> {
    let root = layout.engine_root(id);
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(Error::io(root, error)),
    };
    let mut versions = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| Error::io(&root, error))?;
        let is_dir = entry
            .file_type()
            .map_err(|error| Error::io(entry.path(), error))?
            .is_dir();
        if !is_dir {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if let Ok(version) = Version::new(name) {
            versions.push(version);
        }
    }
    versions.sort();
    Ok(versions)
}

/// The version recorded in `engines/<id>/ACTIVE`, or `None` when no version
/// is selected or the selected one is no longer installed.
///
/// # Errors
///
/// Returns [`Error::Io`] when the file exists but cannot be read, or
/// [`Error::InvalidVersion`] when its content is not a version.
pub fn active(layout: &EngineLayout, id: EngineId) -> Result<Option<Version>> {
    let path = layout.active_file(id);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Error::io(path, error)),
    };
    let version = Version::new(text.trim())?;
    if layout.engine_dir(id, &version).is_dir() {
        Ok(Some(version))
    } else {
        Ok(None)
    }
}

/// Records `version` as the active one for `id`.
///
/// # Errors
///
/// Returns [`Error::NotInstalled`] when `engines/<id>/<version>/` does not
/// exist, or [`Error::Io`] when the record cannot be written.
pub fn set_active(layout: &EngineLayout, id: EngineId, version: &Version) -> Result<()> {
    if !layout.engine_dir(id, version).is_dir() {
        return Err(Error::NotInstalled {
            id,
            version: version.clone(),
        });
    }
    let path = layout.active_file(id);
    // Write then rename so a crash never leaves a half-written record.
    let staged = path.with_extension("tmp");
    fs::write(&staged, format!("{version}\n")).map_err(|error| Error::io(&staged, error))?;
    fs::rename(&staged, &path).map_err(|error| Error::io(&path, error))
}

/// Removes one installed version.
///
/// # Errors
///
/// Returns [`Error::ActiveVersion`] when `version` is active,
/// [`Error::ReferencedVersion`] when it is in `referenced`,
/// [`Error::NotInstalled`] when it is not installed, or [`Error::Io`].
pub fn remove(
    layout: &EngineLayout,
    id: EngineId,
    version: &Version,
    referenced: &[Version],
) -> Result<()> {
    if active(layout, id)?.as_ref() == Some(version) {
        return Err(Error::ActiveVersion {
            id,
            version: version.clone(),
        });
    }
    if referenced.contains(version) {
        return Err(Error::ReferencedVersion {
            id,
            version: version.clone(),
        });
    }
    let dir = layout.engine_dir(id, version);
    if !dir.is_dir() {
        return Err(Error::NotInstalled {
            id,
            version: version.clone(),
        });
    }
    fs::remove_dir_all(&dir).map_err(|error| Error::io(dir, error))
}

/// Removes installed versions beyond the newest `keep`, never the active
/// version or one in `referenced`. Per-user overlays (`<version>.user/`) are
/// left alone because they hold user-supplied files. Returns what it removed,
/// oldest first.
///
/// # Errors
///
/// Returns [`Error::Io`] from listing or removal; versions removed before the
/// failure stay removed.
pub fn prune(
    layout: &EngineLayout,
    id: EngineId,
    keep: usize,
    referenced: &[Version],
) -> Result<Vec<Version>> {
    let active = active(layout, id)?;
    let mut candidates: Vec<Version> = list(layout, id)?
        .into_iter()
        .filter(|version| Some(version) != active.as_ref() && !referenced.contains(version))
        .collect();
    // `list` is oldest first; the newest `keep` sit at the end.
    let remove_count = candidates.len().saturating_sub(keep);
    candidates.truncate(remove_count);
    for version in &candidates {
        let dir = layout.engine_dir(id, version);
        fs::remove_dir_all(&dir).map_err(|error| Error::io(dir, error))?;
    }
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::new(text).unwrap()
    }

    #[test]
    fn version_ordering_is_numeric_per_segment() {
        assert!(v("10.0.0") < v("10.0.1"));
        assert!(v("10.0.9") < v("10.0.10"));
        assert!(v("9.9.9") < v("10.0.0"));
        assert!(v("10.0.0-rc1") < v("10.0.0"));
        assert!(v("10.0.0-rc1") < v("10.0.0-rc2"));
        assert!(v("10.0") < v("10.0.0"));
        assert_eq!(v("10.0.0").cmp(&v("10.0.0")), Ordering::Equal);
    }

    #[test]
    fn version_rejects_directory_hazards() {
        for bad in [
            "",
            ".hidden",
            "a/b",
            "a\\b",
            "1.0.partial",
            "1.0.user",
            "1 0",
        ] {
            assert!(Version::new(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(Version::new("10.0.0").is_ok());
    }
}
