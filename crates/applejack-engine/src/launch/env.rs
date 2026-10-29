//! The fixed environment allowlist for every engine launch.
//!
//! The host environment and per-game extras pass through one table. Names
//! outside the table are dropped, `DYLD_*` is always dropped (the hardened
//! runtime strips it anyway, so the payload must be self-contained), and
//! path-valued variables must point inside the engine directory, its `.user`
//! overlay, or the prefix.

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

use crate::EnvPair;

/// How a variable's value is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Any value.
    Plain,
    /// One absolute path inside an allowed root.
    Path,
    /// Colon-separated absolute paths, each inside an allowed root.
    PathList,
}

/// One allowlist row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub name: &'static str,
    pub kind: Kind,
}

/// The variables a launched process may receive, in output order.
pub const ALLOWLIST: &[Entry] = &[
    Entry {
        name: "PATH",
        kind: Kind::Plain,
    },
    Entry {
        name: "HOME",
        kind: Kind::Plain,
    },
    Entry {
        name: "TMPDIR",
        kind: Kind::Plain,
    },
    Entry {
        name: "WINEPREFIX",
        kind: Kind::Path,
    },
    Entry {
        name: "WINEDEBUG",
        kind: Kind::Plain,
    },
    Entry {
        name: "WINEDLLOVERRIDES",
        kind: Kind::Plain,
    },
    Entry {
        name: "WINEMSYNC",
        kind: Kind::Plain,
    },
    Entry {
        name: "WINEESYNC",
        kind: Kind::Plain,
    },
    Entry {
        name: "DXMT_CONFIG",
        kind: Kind::Path,
    },
    Entry {
        name: "DXMT_LOG_LEVEL",
        kind: Kind::Plain,
    },
    Entry {
        name: "MTL_HUD_ENABLED",
        kind: Kind::Plain,
    },
    Entry {
        name: "ROSETTA_ADVERTISE_AVX",
        kind: Kind::Plain,
    },
    Entry {
        name: "GST_PLUGIN_PATH",
        kind: Kind::PathList,
    },
    Entry {
        name: "APPLEJACK_SESSION",
        kind: Kind::Plain,
    },
    Entry {
        name: "VK_ICD_FILENAMES",
        kind: Kind::PathList,
    },
];

/// Directories a path-valued variable may point into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedRoots {
    pub engine_dir: PathBuf,
    pub overlay_dir: PathBuf,
    pub prefix_dir: PathBuf,
}

impl AllowedRoots {
    fn contains(&self, path: &Path) -> bool {
        [&self.engine_dir, &self.overlay_dir, &self.prefix_dir]
            .into_iter()
            .any(|root| path.starts_with(root))
    }
}

/// Why a variable was dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropReason {
    /// The name is not in [`ALLOWLIST`].
    NotAllowlisted,
    /// The name starts with `DYLD_`.
    Dyld,
    /// A path-valued variable is not absolute, contains `..`, or lies
    /// outside every allowed root.
    OutsideRoots { value: OsString },
    /// An earlier value of the same name was overridden by a later one.
    Superseded,
}

/// A variable that did not make it into the child environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dropped {
    pub name: OsString,
    pub reason: DropReason,
}

/// Filters `base` (the host environment) and `extras` (per-game variables)
/// through [`ALLOWLIST`]. An extra wins over a base value of the same name;
/// among duplicates within one list the last wins. The accepted pairs come
/// back in allowlist order; `dropped` names every pair that was not used so
/// the caller can log them.
#[must_use]
pub fn assemble(
    base: &[EnvPair],
    extras: &[EnvPair],
    roots: &AllowedRoots,
) -> (Vec<EnvPair>, Vec<Dropped>) {
    let mut dropped = Vec::new();
    let mut chosen: Vec<Option<&OsStr>> = vec![None; ALLOWLIST.len()];

    // Base goes first so an extra of the same name replaces it.
    for (name, value) in base.iter().chain(extras) {
        match classify(name, value, roots) {
            Ok(slot) => {
                if chosen[slot].replace(value.as_os_str()).is_some() {
                    dropped.push(Dropped {
                        name: name.clone(),
                        reason: DropReason::Superseded,
                    });
                }
            }
            Err(reason) => dropped.push(Dropped {
                name: name.clone(),
                reason,
            }),
        }
    }

    let accepted = ALLOWLIST
        .iter()
        .zip(chosen)
        .filter_map(|(entry, value)| Some((OsString::from(entry.name), value?.to_owned())))
        .collect();
    (accepted, dropped)
}

fn classify(name: &OsStr, value: &OsStr, roots: &AllowedRoots) -> Result<usize, DropReason> {
    let Some(name) = name.to_str() else {
        return Err(DropReason::NotAllowlisted);
    };
    if name.starts_with("DYLD_") {
        return Err(DropReason::Dyld);
    }
    let slot = ALLOWLIST
        .iter()
        .position(|entry| entry.name == name)
        .ok_or(DropReason::NotAllowlisted)?;
    let inside = match ALLOWLIST[slot].kind {
        Kind::Plain => true,
        Kind::Path => is_inside(Path::new(value), roots),
        Kind::PathList => {
            let paths: Vec<&Path> = value
                .to_str()
                .map(|text| text.split(':').map(Path::new).collect())
                .unwrap_or_default();
            !paths.is_empty() && paths.into_iter().all(|path| is_inside(path, roots))
        }
    };
    if inside {
        Ok(slot)
    } else {
        Err(DropReason::OutsideRoots {
            value: value.to_owned(),
        })
    }
}

// Lexical only: canonicalizing would require the path to exist and would
// diverge from the roots the caller computed (for example `/var` versus
// `/private/var`). `components()` drops interior `.`; rejecting `..` keeps
// the prefix check sound.
fn is_inside(path: &Path, roots: &AllowedRoots) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|component| component != Component::ParentDir)
        && roots.contains(path)
}
