//! The fixed environment allowlist for every engine launch.
//!
//! The host environment, the per-game extras, and the values the engine
//! sets pass through one table. Each row says how a value is checked, who
//! owns the variable (and so who may set it), and which roots a path-valued
//! variable may point into. Names outside the table are dropped, `DYLD_*` is
//! always dropped (the hardened runtime strips it anyway, so the payload
//! must be self-contained), and path checks compare canonical paths so a
//! symlink such as the prefix's `dosdevices/z:` cannot lead outside a root.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::{EnvPair, Error, Result};

/// How a variable's value is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Any value.
    Plain,
    /// Any non-empty value.
    NonEmpty,
    /// One absolute path inside one of the row's roots.
    Path,
    /// One absolute path that resolves to the prefix directory itself
    /// ([`AllowedRoots::prefix_dir`]), not merely inside it: Wine would
    /// create a fresh prefix at any other path.
    PrefixDir,
    /// Colon-separated absolute paths, each inside one of the row's roots.
    PathList,
}

/// Who a variable belongs to. Values come from three tiers, host, game, and
/// engine, and the owner decides which tiers may supply a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Owner {
    /// Describes the host (`PATH`, `HOME`). The host environment or the
    /// engine may set it; a game preset may not.
    Host,
    /// A tunable (`WINEDEBUG`, `DXMT_CONFIG`). Host, game, and engine may
    /// set it; a later tier overrides an earlier one.
    Game,
    /// A launcher invariant (`APPLEJACK_SESSION`, `WINEPREFIX`). Only the
    /// engine may set it.
    Engine,
}

impl Owner {
    const fn accepts(self, from: Self) -> bool {
        match (self, from) {
            (_, Self::Engine) | (Self::Game, _) | (Self::Host, Self::Host) => true,
            (Self::Host, Self::Game) | (Self::Engine, Self::Host | Self::Game) => false,
        }
    }
}

/// Which of the [`AllowedRoots`] a path-valued row accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootSet {
    /// `engines/<id>/<version>/`.
    pub engine: bool,
    /// `engines/<id>/<version>.user/`.
    pub overlay: bool,
    /// The prefix directory.
    pub prefix: bool,
}

impl RootSet {
    /// For rows that are not paths.
    pub const NONE: Self = Self {
        engine: false,
        overlay: false,
        prefix: false,
    };
    /// Only the prefix directory.
    pub const PREFIX: Self = Self {
        engine: false,
        overlay: false,
        prefix: true,
    };
    /// Engine directory, overlay, and prefix.
    pub const ALL: Self = Self {
        engine: true,
        overlay: true,
        prefix: true,
    };
}

/// One allowlist row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    /// The variable name.
    pub name: &'static str,
    /// How the value is checked.
    pub kind: Kind,
    /// Who may set it.
    pub owner: Owner,
    /// Roots a [`Kind::Path`], [`Kind::PrefixDir`], or [`Kind::PathList`]
    /// value may point into; [`RootSet::NONE`] for [`Kind::Plain`] and
    /// [`Kind::NonEmpty`].
    pub roots: RootSet,
    /// The variable must be present after every launch's tiers are applied;
    /// [`assemble`] fails when it is not. Only engine-owned rows are
    /// required, so the engine alone can satisfy them.
    pub required: bool,
}

const fn plain(name: &'static str, owner: Owner) -> Entry {
    Entry {
        name,
        kind: Kind::Plain,
        owner,
        roots: RootSet::NONE,
        required: false,
    }
}

/// The variables a launched process may receive, in output order.
///
/// | Variable | Owner | Check | Roots | Required |
/// | --- | --- | --- | --- | --- |
/// | `PATH`, `HOME`, `TMPDIR` | host | any value | | |
/// | `WINEPREFIX` | engine | one absolute path resolving to the prefix directory itself | prefix | yes |
/// | `WINEDEBUG` | game | any value | | |
/// | `WINEDLLOVERRIDES`, `WINEMSYNC`, `WINEESYNC` | engine | any value | | |
/// | `DXMT_CONFIG` | game | any value (inline `key=value;` options) | | |
/// | `DXMT_CONFIG_FILE` | game | one absolute path | engine, overlay, prefix | |
/// | `DXMT_LOG_LEVEL`, `MTL_HUD_ENABLED` | game | any value | | |
/// | `ROSETTA_ADVERTISE_AVX` | engine | any value; set only from the recorded CPUID probe | | |
/// | `GST_PLUGIN_PATH` | game | colon-separated absolute paths | engine, overlay, prefix | |
/// | `APPLEJACK_SESSION` | engine | any non-empty value | | yes |
/// | `VK_ICD_FILENAMES` | game | colon-separated absolute paths | engine, overlay, prefix | |
///
/// `WINEARCH` is not a row: the prefix architecture is fixed by the loader
/// (a 64-bit `wine` creates a `win64` prefix, and the variable only forces
/// a 32-bit one, which no engine creates), so prefix creation runs
/// `wineboot` through [`assemble`] like any launch.
pub const ALLOWLIST: &[Entry] = &[
    plain("PATH", Owner::Host),
    plain("HOME", Owner::Host),
    plain("TMPDIR", Owner::Host),
    Entry {
        name: "WINEPREFIX",
        kind: Kind::PrefixDir,
        owner: Owner::Engine,
        roots: RootSet::PREFIX,
        required: true,
    },
    plain("WINEDEBUG", Owner::Game),
    plain("WINEDLLOVERRIDES", Owner::Engine),
    plain("WINEMSYNC", Owner::Engine),
    plain("WINEESYNC", Owner::Engine),
    // DXMT follows the DXVK split: `DXMT_CONFIG` carries inline
    // `key=value;` options and `DXMT_CONFIG_FILE` the path to a config file.
    plain("DXMT_CONFIG", Owner::Game),
    Entry {
        name: "DXMT_CONFIG_FILE",
        kind: Kind::Path,
        owner: Owner::Game,
        roots: RootSet::ALL,
        required: false,
    },
    plain("DXMT_LOG_LEVEL", Owner::Game),
    plain("MTL_HUD_ENABLED", Owner::Game),
    // Engine-owned: a game preset or the user's shell must not advertise
    // AVX when the recorded probe says the variable changes nothing.
    plain("ROSETTA_ADVERTISE_AVX", Owner::Engine),
    Entry {
        name: "GST_PLUGIN_PATH",
        kind: Kind::PathList,
        owner: Owner::Game,
        roots: RootSet::ALL,
        required: false,
    },
    Entry {
        name: "APPLEJACK_SESSION",
        kind: Kind::NonEmpty,
        owner: Owner::Engine,
        roots: RootSet::NONE,
        required: true,
    },
    Entry {
        name: "VK_ICD_FILENAMES",
        kind: Kind::PathList,
        owner: Owner::Game,
        roots: RootSet::ALL,
        required: false,
    },
];

/// Directories a path-valued variable may point into, held in canonical
/// form. Built only through [`AllowedRoots::new`], so an empty or `/` root
/// that would admit every path cannot be represented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedRoots {
    engine: PathBuf,
    overlay: PathBuf,
    prefix: PathBuf,
}

impl AllowedRoots {
    /// Validates and canonicalizes the three roots. Each must be absolute,
    /// not `/`, free of `..`, and not itself a symlink (a link would widen
    /// the root to wherever it points); the longest existing ancestor of
    /// each is canonicalized so `/var` and `/private/var` spellings agree,
    /// and the result may not be `/` either. The directories need not exist
    /// yet.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRoot`] naming the offending root.
    pub fn new(engine_dir: &Path, overlay_dir: &Path, prefix_dir: &Path) -> Result<Self> {
        Ok(Self {
            engine: check_root(engine_dir)?,
            overlay: check_root(overlay_dir)?,
            prefix: check_root(prefix_dir)?,
        })
    }

    /// The canonical engine directory.
    #[must_use]
    pub fn engine_dir(&self) -> &Path {
        &self.engine
    }

    /// The canonical overlay directory.
    #[must_use]
    pub fn overlay_dir(&self) -> &Path {
        &self.overlay
    }

    /// The canonical prefix directory.
    #[must_use]
    pub fn prefix_dir(&self) -> &Path {
        &self.prefix
    }

    fn contains(&self, resolved: &Path, set: RootSet) -> bool {
        [
            (set.engine, &self.engine),
            (set.overlay, &self.overlay),
            (set.prefix, &self.prefix),
        ]
        .into_iter()
        .any(|(allowed, root)| allowed && resolved.starts_with(root))
    }
}

fn check_root(root: &Path) -> Result<PathBuf> {
    let invalid = |reason| Error::InvalidRoot {
        path: root.to_path_buf(),
        reason,
    };
    if root.as_os_str().is_empty() {
        return Err(invalid("is empty"));
    }
    if !root.is_absolute() {
        return Err(invalid("must be absolute"));
    }
    if root
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(invalid("must not contain `..`"));
    }
    // `lstat("link/")` follows the link, so probe the components, not the
    // spelling.
    let normalized: PathBuf = root.components().collect();
    if fs::symlink_metadata(&normalized).is_ok_and(|meta| meta.is_symlink()) {
        return Err(invalid("must not be a symlink"));
    }
    let resolved = resolve(root).ok_or_else(|| invalid("cannot be resolved"))?;
    if resolved.parent().is_none() {
        return Err(invalid("must not be the filesystem root"));
    }
    Ok(resolved)
}

/// Canonicalizes the longest existing ancestor of `path` and re-appends the
/// components that do not exist yet, so a path through a symlink is seen at
/// its real location while a not-yet-created file still resolves. A
/// component that exists but cannot be canonicalized is a dangling or
/// looping symlink; its target could appear later, outside every root, so
/// the path is unresolvable rather than "not created yet". A trailing `/`
/// or `/.` is dropped first: `lstat` follows a link spelled `link/`, which
/// would make a dangling link look like a missing component.
fn resolve(path: &Path) -> Option<PathBuf> {
    let normalized: PathBuf = path.components().collect();
    let mut existing = normalized.as_path();
    let mut missing: Vec<&OsStr> = Vec::new();
    loop {
        if let Ok(canonical) = fs::canonicalize(existing) {
            return Some(
                missing
                    .iter()
                    .rev()
                    .fold(canonical, |path, component| path.join(component)),
            );
        }
        if fs::symlink_metadata(existing).is_ok() {
            return None;
        }
        missing.push(existing.file_name()?);
        existing = existing.parent()?;
    }
}

/// Why a variable was dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropReason {
    /// The name is not in [`ALLOWLIST`].
    NotAllowlisted,
    /// The name starts with `DYLD_`.
    Dyld,
    /// The tier that supplied the value is not allowed to set this variable
    /// (see [`Owner`]).
    NotOwner,
    /// A path-valued variable is not absolute, contains `..`, resolves
    /// outside every root its row accepts, or (for [`Kind::PrefixDir`]) is
    /// not the prefix directory.
    OutsideRoots {
        /// The rejected value.
        value: OsString,
    },
    /// An earlier value of the same name was overridden by a later one.
    Superseded,
    /// A [`Kind::NonEmpty`] variable is empty.
    Empty,
}

impl fmt::Display for DropReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAllowlisted => f.write_str("not in the allowlist"),
            Self::Dyld => f.write_str("DYLD_ variables are never passed"),
            Self::NotOwner => f.write_str("set by a tier that does not own the variable"),
            Self::OutsideRoots { value } => {
                write!(
                    f,
                    "`{}` fails the row's path check",
                    Path::new(value).display()
                )
            }
            Self::Superseded => f.write_str("superseded by a later tier"),
            Self::Empty => f.write_str("empty"),
        }
    }
}

/// A variable that did not make it into the child environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dropped {
    /// The variable name as supplied.
    pub name: OsString,
    /// Why it was dropped.
    pub reason: DropReason,
    /// The tier that supplied the dropped value.
    pub from: Owner,
}

/// Filters `base` (the host environment), `extras` (per-game variables),
/// and `engine_set` (values the engine computes for the launch) through
/// [`ALLOWLIST`].
///
/// Tiers are applied in that order and a later tier replaces an earlier
/// value of the same name, subject to the row's [`Owner`]: a value from a
/// tier the owner does not accept is dropped with [`DropReason::NotOwner`],
/// so a game extra can never set `APPLEJACK_SESSION` or `WINEPREFIX`. A
/// value that fails its path check is dropped without unsetting the earlier
/// tier's value, so a rejected extra falls back to the base value. Among
/// duplicates within one list the last wins. The accepted pairs come back in
/// allowlist order; `dropped` names every pair that was not used, with the
/// tier it came from, so the caller can log them.
///
/// # Errors
///
/// Returns [`Error::MissingEngineVariable`] when a required row
/// (`WINEPREFIX`, `APPLEJACK_SESSION`) has no accepted value after the
/// tiers are applied: the engine did not set it, or set a value that failed
/// its check, in which case the error carries the [`DropReason`] of the
/// engine's last value. Without `WINEPREFIX` Wine would use `~/.wine`, and
/// with one other than the prefix directory it would create a prefix there;
/// without the session id process discovery finds nothing.
pub fn assemble(
    base: &[EnvPair],
    extras: &[EnvPair],
    engine_set: &[EnvPair],
    roots: &AllowedRoots,
) -> Result<(Vec<EnvPair>, Vec<Dropped>)> {
    let mut dropped = Vec::new();
    let mut chosen: Vec<Option<(&OsStr, Owner)>> = vec![None; ALLOWLIST.len()];

    let tiers = [
        (base, Owner::Host),
        (extras, Owner::Game),
        (engine_set, Owner::Engine),
    ];
    for (pairs, from) in tiers {
        for (name, value) in pairs {
            match classify(name, value, from, roots) {
                Ok(slot) => {
                    if let Some((_, earlier)) = chosen[slot].replace((value.as_os_str(), from)) {
                        dropped.push(Dropped {
                            name: name.clone(),
                            reason: DropReason::Superseded,
                            from: earlier,
                        });
                    }
                }
                Err(reason) => dropped.push(Dropped {
                    name: name.clone(),
                    reason,
                    from,
                }),
            }
        }
    }

    if let Some(entry) = ALLOWLIST
        .iter()
        .zip(&chosen)
        .find(|(entry, value)| entry.required && value.is_none())
        .map(|(entry, _)| entry)
    {
        let rejected = dropped
            .iter()
            .rev()
            .find(|drop| drop.from == Owner::Engine && drop.name == entry.name)
            .map(|drop| drop.reason.clone());
        return Err(Error::MissingEngineVariable {
            name: entry.name,
            rejected,
        });
    }

    let accepted = ALLOWLIST
        .iter()
        .zip(chosen)
        .filter_map(|(entry, value)| Some((OsString::from(entry.name), value?.0.to_owned())))
        .collect();
    Ok((accepted, dropped))
}

fn classify(
    name: &OsStr,
    value: &OsStr,
    from: Owner,
    roots: &AllowedRoots,
) -> std::result::Result<usize, DropReason> {
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
    let entry = &ALLOWLIST[slot];
    if !entry.owner.accepts(from) {
        return Err(DropReason::NotOwner);
    }
    if entry.kind == Kind::NonEmpty && value.is_empty() {
        return Err(DropReason::Empty);
    }
    let inside = match entry.kind {
        Kind::Plain | Kind::NonEmpty => true,
        Kind::Path => is_inside(Path::new(value), roots, entry.roots),
        Kind::PrefixDir => {
            let path = Path::new(value);
            is_inside(path, roots, entry.roots)
                && resolve(path).as_deref() == Some(roots.prefix_dir())
        }
        Kind::PathList => {
            let paths: Vec<&Path> = value
                .to_str()
                .map(|text| text.split(':').map(Path::new).collect())
                .unwrap_or_default();
            !paths.is_empty()
                && paths
                    .into_iter()
                    .all(|path| is_inside(path, roots, entry.roots))
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

// `..` is rejected lexically before resolving so a value cannot climb out
// through a component that does not exist yet.
fn is_inside(path: &Path, roots: &AllowedRoots, set: RootSet) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|component| component != Component::ParentDir)
        && resolve(path).is_some_and(|resolved| roots.contains(&resolved, set))
}
