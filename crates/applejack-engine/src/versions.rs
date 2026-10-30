//! Installed engine versions: listing, removal, and pruning.
//!
//! The active version is not recorded here. The core owns that fact (in
//! `engine_versions.active`) and passes every version it must keep, including
//! the active one, to [`remove`] and [`prune`] as `protected`.

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
/// The alphabet is lowercase ASCII `[0-9a-z._+-]`, starting with a digit and
/// not ending with `.`, so that byte equality is filesystem identity on a
/// case-insensitive volume and no version collides with a layout file.
///
/// Ordering compares dot- and dash-separated segments left to right, each
/// split into alternating digit and letter runs: numbers by value (of any
/// width), letters lexically. So `10.0.0 < 10.0.1 < 10.1.0`,
/// `10.0.0-rc1 < 10.0.0`, and `10.0.0-rc9 < 10.0.0-rc10`. The newest
/// version is the greatest.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Version(String);

impl Version {
    /// Validates `value` as a version: lowercase ASCII `[0-9a-z._+-]`, a
    /// leading digit, no trailing `.`, and no layout suffix.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidVersion`] with the rule that failed.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        let reason = if value.is_empty() {
            Some("empty")
        } else if !value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'z' | b'.' | b'_' | b'+' | b'-'))
        {
            Some("must use only lowercase ASCII letters, digits, `.`, `_`, `+`, `-`")
        } else if !value.as_bytes()[0].is_ascii_digit() {
            Some("must start with a digit")
        } else if value.ends_with('.') {
            Some("ends with `.`")
        } else if value.ends_with(EngineLayout::PARTIAL_SUFFIX)
            || value.ends_with(EngineLayout::OVERLAY_SUFFIX)
            || value.ends_with(EngineLayout::DOWNLOAD_SUFFIX)
        {
            Some("ends with a reserved layout suffix")
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

    /// The version text, which is also its directory name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn segments(&self) -> Vec<Segment<'_>> {
        let mut out = Vec::new();
        for part in self.0.split(['.', '-']) {
            if part.is_empty() {
                out.push(Segment::Text(""));
                continue;
            }
            // `rc10` -> Text("rc"), Number("10"): a digit run and a letter
            // run never compare lexically against each other.
            let bytes = part.as_bytes();
            let mut start = 0;
            for i in 1..=bytes.len() {
                if i == bytes.len() || bytes[i].is_ascii_digit() != bytes[start].is_ascii_digit() {
                    out.push(Segment::parse(&part[start..i]));
                    start = i;
                }
            }
        }
        out
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Segment<'a> {
    /// A digit run, kept as text so no width overflows.
    Number(&'a str),
    Text(&'a str),
}

impl<'a> Segment<'a> {
    fn parse(text: &'a str) -> Self {
        if text.bytes().all(|b| b.is_ascii_digit()) {
            Self::Number(text)
        } else {
            Self::Text(text)
        }
    }
}

impl Ord for Segment<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            // Numeric order without parsing: after dropping leading zeros, a
            // longer digit run is the larger number, and equal lengths
            // compare lexically.
            (Self::Number(a), Self::Number(b)) => {
                let (a, b) = (a.trim_start_matches('0'), b.trim_start_matches('0'));
                a.len().cmp(&b.len()).then_with(|| a.cmp(b))
            }
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
        let mut a = self.segments().into_iter();
        let mut b = other.segments().into_iter();
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

/// Installed versions of `id`, oldest first. Only a real directory (not a
/// symlink) whose name is a version and that holds a regular `BUNDLE.toml`
/// counts; extraction, overlay, download, and stale directories (a
/// `<version>/` without a descriptor, left by an interrupted removal) are
/// not versions.
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
        if !crate::bundle::is_bundle_dir(&entry.path()) {
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

/// Removes one version directory, whatever it holds: an installed bundle or
/// the stale remains of an interrupted removal. `protected` lists every
/// version the caller must keep: the active one, those pinned by a game
/// configuration, and those in use by a running session. Comparison is
/// exact.
///
/// # Errors
///
/// Returns [`Error::NotInstalled`] when nothing sits at
/// `engines/<id>/<version>` (checked first, so a protected version that is
/// not installed reports the accurate state), [`Error::ProtectedVersion`]
/// when `version` is in `protected`, or [`Error::Io`].
pub fn remove(
    layout: &EngineLayout,
    id: EngineId,
    version: &Version,
    protected: &[Version],
) -> Result<()> {
    let dir = layout.engine_dir(id, version);
    match fs::symlink_metadata(&dir) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(Error::NotInstalled {
                id,
                version: version.clone(),
            });
        }
        Err(error) => return Err(Error::io(dir, error)),
    }
    if protected.contains(version) {
        return Err(Error::ProtectedVersion {
            id,
            version: version.clone(),
        });
    }
    crate::bundle::remove_stale(&dir)
}

/// Removes installed versions beyond the newest `keep`, never one in
/// `protected` (see [`remove`]). Per-user overlays (`<version>.user/`) are
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
    protected: &[Version],
) -> Result<Vec<Version>> {
    let mut candidates: Vec<Version> = list(layout, id)?
        .into_iter()
        .filter(|version| !protected.contains(version))
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
        assert!(v("0.1.0") < v("0.10.0"));
        assert!(v("10.0.0-rc1") < v("10.0.0"));
        assert!(v("10.0.0-rc1") < v("10.0.0-rc2"));
        assert!(v("10.0") < v("10.0.0"));
        assert!(
            v("10.0.0") < v("10.0.0-1"),
            "a numeric tag is a post-release"
        );
        assert_eq!(v("10.0.0").cmp(&v("10.0.0")), Ordering::Equal);
    }

    #[test]
    fn version_orders_numbers_wider_than_u64() {
        let wide = "1.99999999999999999999";
        assert!(v("1.2") < v(wide));
        assert!(v(wide) < v("1.100000000000000000000"));
        assert!(v("1.007") < v("1.8"));
        assert!(v("1.0000") < v("1.1"));
        assert!(
            v("1.0") < v("1.00"),
            "equal numbers fall back to text order"
        );
    }

    #[test]
    fn version_orders_rc9_before_rc10() {
        assert!(v("0.1.0-rc9") < v("0.1.0-rc10"));
        assert!(v("10.0.0-rc9") < v("10.0.0-rc10"));
        assert!(v("10.0.0-rc10") < v("10.0.0"));
        assert!(v("10.0.0-beta2") < v("10.0.0-rc1"));
    }

    #[test]
    fn version_rejects_uppercase() {
        for bad in ["ACTIVE", "ACTIVE.tmp", "10.0.0-RC1", "10.0.0-Rc1"] {
            let error = Version::new(bad).unwrap_err();
            assert!(
                matches!(error, Error::InvalidVersion { .. }),
                "{bad:?}: {error}"
            );
        }
        assert!(Version::new("10.0.0-rc1").is_ok());
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
            "1.0.download",
            "1 0",
            "10.0.0.",
            "10:0",
            "x10",
            "-1",
            "1\u{e9}",
        ] {
            assert!(Version::new(bad).is_err(), "{bad:?} should be rejected");
        }
        for good in ["10.0.0", "0.1.0-rc9", "1.0+build_3", "7"] {
            assert!(Version::new(good).is_ok(), "{good:?} should be accepted");
        }
    }
}
