//! `cargo xtask engine-payload --close`: the dylib closure of an assembled
//! payload (docs/engines/payload-build.md step 7). After it runs, every
//! Mach-O in the tree links only against the system or against files inside
//! the payload, and every library Wine `dlopen`s by bare SONAME is a file
//! under `wine/lib/` that the calling module's run path reaches.
//!
//! The closure is a fixed-point loop over [`plan`], a pure function of the
//! tree: copy what is missing from the x86-64 Homebrew, rewrite install names
//! and references to `@rpath/<leaf>`, add the `@loader_path/..` run paths
//! that make each binary's own references resolve, re-sign, and look again,
//! until a round plans nothing. A round lands whole or not at all. The
//! checker's `inspect` then has the last word, so `--close` cannot succeed
//! on a tree `--check` rejects.
//!
//! Wine passes a bare SONAME to `dlopen` (`dlls/win32u/freetype.c`,
//! `dlls/bcrypt/gnutls.c`, `dlls/winebus.sys/bus_sdl.c`, ...) and sets no
//! search path for it; dyld resolves a leaf name as `@rpath/<leaf>` through
//! the run paths of the calling image and of the main executable, then the
//! working directory and `/usr/lib`, never `/usr/local/lib`. The run path
//! goes on the Unix modules whose bytes name the SONAME: the caller's run
//! paths are what dyld consults for a bare-leaf `dlopen`, so the module that
//! calls `dlopen` is the right owner, and the two main executables
//! (`wine/bin/wine` and `wine/lib/wine/x86_64-unix/wine`, in different
//! directories) stay untouched.
//!
//! dyld keeps one image per install name per process: a second file with
//! the same install name is never loaded, whichever run path a client has.
//! The payload therefore holds exactly one regular file per install name. A
//! library the payload already ships (the `GStreamer` framework's freetype
//! and proxy-libintl, the pinned `MoltenVK`) is linked or reached by a run
//! path, never copied again, and only through a [`SONAME_POLICY`] row that
//! names the file and that the dependent's compatibility version is checked
//! against; a duplicate the tree carries becomes a relative link to that
//! file, or is refused. Every path a round writes resolves inside the
//! payload: a link that leads out of it is an occupied slot, not an empty
//! one, and a tool never runs on a link.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::Read as _;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::bundle_toml::{BundleSpec, is_formula_name, named_row, shipped_elsewhere};
use crate::denylist::Denylist;
use crate::engine_payload::{BUNDLE_FILE, check, is_macho, load_denylist, otool, walk};
use crate::self_contained::{
    Violation, binary_dir, dependency_entries, dylib_commands, id_entry, is_executable,
    normalize_relative, rpath_entries,
};

/// Where copied libraries land, and the one directory a SONAME resolves in.
pub const LIB_DIR: &str = "wine/lib";
/// Wine's Unix modules live below here; they are the images that `dlopen` a
/// SONAME, so they carry the run path to [`LIB_DIR`].
const MODULE_DIR: &str = "wine/lib/wine/";
/// The x86-64 Homebrew, the only source of copied libraries. Its formulas
/// resolve into `Cellar/<formula>/<version>/`, which is where a copied file's
/// provenance comes from.
const HOMEBREW_PREFIX: &str = "/usr/local";
const HOMEBREW_LIB: &str = "/usr/local/lib";
const HOMEBREW_CELLAR: &str = "/usr/local/Cellar";
/// Wine's `configure` records every library it `dlopen`s by name here.
const CONFIG_H: &str = "build/work/wine/build/include/config.h";
/// Each round copies at most one more level of dependencies, so a tree that
/// has not settled by now has a deeper chain than any Homebrew library
/// needs, or is cycling.
const MAX_ROUNDS: usize = 8;
const CPU_TYPE_X86_64: u32 = 0x0100_0007;
const DUPLICATE_REASON: &str = "several regular files carry this install name; dyld keeps one image per install name per process, so load order would pick the library";

/// How a SONAME from `config.h` is satisfied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SonamePolicy {
    /// Copied from `/usr/local/lib/<soname>`; the default for any SONAME
    /// without a row in [`SONAME_POLICY`].
    Homebrew,
    /// A file the payload already ships at this bundle-relative path. For a
    /// SONAME, `wine/lib/<soname>` becomes a relative link to it; any other
    /// regular file carrying its install name becomes a link to it too; and
    /// an absolute dependency whose leaf this is binds to it, provided the
    /// file's current version satisfies the dependent's compatibility
    /// version. dyld does not enforce that version, so a row may allow an
    /// older file when every imported symbol has been verified; the reason
    /// is recorded here, and the run that makes the binding prints it once.
    /// An empty reason is no allowance.
    Payload {
        file: &'static str,
        allow_older_compat: Option<&'static str>,
    },
    /// A macOS library dyld finds in the shared cache on its own.
    System,
    /// Copied when the x86-64 Homebrew has it, skipped with a message
    /// otherwise: Wine degrades without it.
    Optional,
}

/// The SONAMEs and dependency leaves that are not plain Homebrew copies. A
/// new SONAME in `config.h` that is neither here nor in the x86-64 Homebrew
/// fails the closure, which is the moment to classify it; so does a SONAME
/// or an absolute dependency's leaf the payload already ships somewhere
/// without a row here. A formula that spells a pinned component
/// (`molten-vk`) can never gain a `[components]` row, so a row here is the
/// only way its library enters the payload.
pub const SONAME_POLICY: &[(&str, SonamePolicy)] = &[
    // Wine's SONAME_LIBVULKAN and SONAME_LIBMOLTENVK: the pinned MoltenVK
    // release (`components.moltenvk`), not Homebrew's molten-vk formula. The
    // framework ships a MoltenVK of its own (1.2.9, for its vulkan and
    // applemedia plugins) under the same install name; it becomes a link to
    // the pin, whose exports cover every symbol those plugins import.
    (
        "libMoltenVK.dylib",
        SonamePolicy::Payload {
            file: "vulkan/libMoltenVK.dylib",
            allow_older_compat: None,
        },
    ),
    // The framework's freetype, which its fontconfig, harfbuzz, cairo, pango,
    // and libass link, rather than a second copy from Homebrew; it exports
    // every FT_* symbol win32u and dwrite look up.
    (
        "libfreetype.6.dylib",
        SonamePolicy::Payload {
            file: "wine/lib/GStreamer.framework/Versions/1.0/lib/libfreetype.6.dylib",
            allow_older_compat: None,
        },
    ),
    // gnutls and libidn2 link gettext's libintl by absolute path; the one
    // libintl.8.dylib the payload ships is the framework's proxy-libintl
    // (a stub whose dgettext returns the message id), so they bind to it.
    (
        "libintl.8.dylib",
        SonamePolicy::Payload {
            file: "wine/lib/GStreamer.framework/Versions/1.0/lib/libintl.8.dylib",
            allow_older_compat: Some(
                "gnutls 3.8.10 and libidn2 2.3.8 record compatibility version 13.0.0 against gettext 0.26, and proxy-libintl's current version is 10.5.0; the two symbols they import (libintl_bindtextdomain, libintl_dgettext) are both exported, and dyld does not enforce the compatibility version (gnutls_global_init ran through it on macOS 26)",
            ),
        },
    ),
    // No file under /usr/lib since macOS 11; dyld serves it from the cache.
    ("libcups.2.dylib", SonamePolicy::System),
    // unixODBC; odbc32 dlopens it lazily and reports no driver without it.
    ("libodbc.dylib", SonamePolicy::Optional),
];

/// A dylib version as `otool -l` prints it, `X.Y.Z`, ordered numerically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.trim().split('.').map(|part| part.parse::<u32>().ok());
        let version = Self(parts.next()??, parts.next()??, parts.next()??);
        parts.next().is_none().then_some(version)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

fn policy(soname: &str) -> SonamePolicy {
    SONAME_POLICY
        .iter()
        .find(|(name, _)| *name == soname)
        .map_or(SonamePolicy::Homebrew, |(_, policy)| *policy)
}

/// One Mach-O of the tree as the planner sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// Payload-relative path.
    pub relative: String,
    /// `LC_ID_DYLIB`, for a dylib.
    pub id: Option<String>,
    /// Every dependency load command's name, in load-command order.
    pub dependencies: Vec<String>,
    /// Every `LC_RPATH`, in load-command order.
    pub rpaths: Vec<String>,
    pub executable: bool,
    /// The SONAMEs this image's bytes contain, filled for Unix modules only.
    pub sonames: BTreeSet<String>,
    /// `LC_ID_DYLIB`'s current version, what the file offers its clients.
    pub current_version: Option<Version>,
    /// The compatibility version each dependency load command requires of
    /// the library it names, by reference.
    pub required: BTreeMap<String, Version>,
}

/// The tree a round plans against.
#[derive(Debug, Default)]
pub struct Tree {
    /// Every regular Mach-O file.
    pub images: Vec<Image>,
    /// Payload-relative paths of every regular file and of every symlink that
    /// resolves to a file inside the payload.
    pub files: BTreeSet<PathBuf>,
    /// Each symlink of `files` and the regular file it resolves to, both
    /// payload-relative: one image to dyld, whichever path names it.
    pub links: BTreeMap<PathBuf, PathBuf>,
    /// Symlinks that do not resolve to a regular file inside the payload
    /// (dangling, escaping, or a directory), with what each is. A planned
    /// destination at or below one of them is occupied, not free.
    pub occupied: BTreeMap<PathBuf, String>,
}

/// A library to copy in, thinned to x86-64.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copy {
    /// Absolute, canonical source file.
    pub source: PathBuf,
    /// Payload-relative destination.
    pub dest: PathBuf,
}

/// A relative symlink to create inside the payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// Payload-relative link path.
    pub link: PathBuf,
    /// Target relative to the link's directory.
    pub target: PathBuf,
}

/// One `install_name_tool` invocation on one binary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Edit {
    pub id: Option<String>,
    /// `(old, new)` dependency references.
    pub changes: Vec<(String, String)>,
    pub add_rpaths: Vec<String>,
    pub delete_rpaths: Vec<String>,
}

impl Edit {
    fn is_empty(&self) -> bool {
        self.id.is_none()
            && self.changes.is_empty()
            && self.add_rpaths.is_empty()
            && self.delete_rpaths.is_empty()
    }
}

/// What one round would do to the tree.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub copies: Vec<Copy>,
    pub links: Vec<Link>,
    /// Regular files that carry the install name of a file the SONAME
    /// policy names; each becomes a relative link to that file.
    pub replaced: Vec<Link>,
    /// By payload-relative binary.
    pub edits: BTreeMap<String, Edit>,
    /// References nothing here can close; the round applies nothing when
    /// any exist.
    pub errors: Vec<Violation>,
    /// Optional SONAMEs left out.
    pub notes: Vec<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.copies.is_empty()
            && self.links.is_empty()
            && self.replaced.is_empty()
            && self.edits.is_empty()
    }
}

/// Resolves a dependency reference (absolute) or a SONAME (a leaf, looked up
/// under `/usr/local/lib`) to the canonical file to copy, or says why not.
pub type Source<'a> = dyn Fn(&str) -> Result<PathBuf, String> + 'a;

/// Runs one of the tools `apply` needs (`lipo`, `install_name_tool`,
/// `codesign`); a test stands in a failing one.
pub type Runner<'a> = dyn FnMut(&str, &[&OsStr]) -> Result<(), String> + 'a;

/// Plans one round over `tree`.
///
/// First, every install name carried by more than one regular file: the
/// others become links to the file the SONAME policy names, or the group is
/// an error; a file replaced this way gets no edit this round. For every
/// SONAME: present under `wine/lib/`, or linked per [`SONAME_POLICY`], or
/// copied from the x86-64 Homebrew (an error when the payload ships it
/// elsewhere without a policy row). For every other image: a foreign
/// install name becomes `@rpath/<file name>`; a foreign absolute `LC_RPATH`
/// is dropped; a Unix module naming a supplied SONAME gets the run path to
/// `wine/lib/`; each foreign absolute dependency is rewritten to
/// `@rpath/<leaf>`, bound to `wine/lib/<leaf>` when that exists, else to the
/// one payload file of that name a policy row names (the dependent's
/// compatibility version checked against the file's current version), else
/// copied to `wine/lib/`; each `@rpath/<leaf>` dependency must be reachable
/// through the image's own `@loader_path` run paths (existing or planned
/// this round), else the run path to the one directory holding the leaf is
/// added (a link and its target are one file; the image's own directory
/// wins a tie; any other tie is an error naming the directories), else the
/// leaf is copied from Homebrew. A destination at or below a link that does
/// not resolve to a file inside the payload is an error naming the link.
/// `@loader_path/` and `@executable_path/` references are left for the
/// checker, whose chain rules decide them.
pub fn plan(tree: &Tree, sonames: &[String], source: &Source<'_>) -> Plan {
    let mut round = Round {
        tree,
        source,
        plan: Plan::default(),
        planned: BTreeSet::new(),
        supplied: BTreeSet::new(),
    };
    round.duplicates();
    // An edit planned from a replaced file's load commands would run through
    // the new link onto the canonical file.
    let replaced: BTreeSet<String> = round
        .plan
        .replaced
        .iter()
        .map(|link| link.link.to_string_lossy().into_owned())
        .collect();
    for soname in sonames {
        round.soname(soname);
    }
    for image in tree
        .images
        .iter()
        .filter(|image| !replaced.contains(&image.relative))
    {
        round.image(image);
    }
    round.plan
}

/// One round's state: what the tree has, plus what this round already
/// planned to add, so later decisions see earlier ones.
struct Round<'a> {
    tree: &'a Tree,
    source: &'a Source<'a>,
    plan: Plan,
    /// Payload-relative destinations of this round's copies and links.
    planned: BTreeSet<PathBuf>,
    /// SONAMEs that are, or will be, files under `wine/lib/`.
    supplied: BTreeSet<String>,
}

/// One image's edit in progress: the run-path directories it reaches,
/// existing and planned, decide whether a reference needs another.
struct ImageEdit {
    dir: PathBuf,
    edit: Edit,
    reachable: BTreeSet<PathBuf>,
}

impl ImageEdit {
    /// Adds the run path from the image's directory to `target` unless one
    /// already reaches it.
    fn reach(&mut self, target: &Path) {
        if self.reachable.insert(target.to_path_buf()) {
            self.edit.add_rpaths.push(rpath_from(&self.dir, target));
        }
    }
}

impl Round<'_> {
    fn present(&self, path: &Path) -> bool {
        self.tree.files.contains(path) || self.planned.contains(path)
    }

    fn copy(&mut self, source: PathBuf, dest: PathBuf) {
        self.plan.copies.push(Copy {
            source,
            dest: dest.clone(),
        });
        self.planned.insert(dest);
    }

    fn fail(&mut self, binary: &str, reference: &str, reason: &str) {
        self.plan.errors.push(Violation {
            binary: binary.to_owned(),
            reference: reference.to_owned(),
            reason: reason.to_owned(),
        });
    }

    /// The regular file behind `path`, which may be a link.
    fn canonical(&self, path: &Path) -> PathBuf {
        self.tree
            .links
            .get(path)
            .cloned()
            .unwrap_or_else(|| path.to_path_buf())
    }

    /// A path the round would create must be free: a link there that does
    /// not resolve to a file inside the payload is an occupied slot, not an
    /// empty one, and so is such a link among the parent directories.
    fn free(&mut self, binary: &str, reference: &str, dest: &Path) -> bool {
        let occupied = dest.ancestors().find_map(|path| {
            self.tree.occupied.get(path).map(|what| {
                format!(
                    "{} is {what}; the closure writes only inside the payload",
                    path.display()
                )
            })
        });
        match occupied {
            Some(reason) => {
                self.fail(binary, reference, &reason);
                false
            }
            None => true,
        }
    }

    /// The directories providing `rest`, one per distinct file: a link and
    /// the file it resolves to are one image to dyld, so they are no tie,
    /// and the directory holding the regular file is the one kept.
    fn providers(&self, rest: &Path) -> Vec<PathBuf> {
        let mut by_file: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
        for dir in dirs_providing(self.tree, &self.planned, rest) {
            let path = dir.join(rest);
            let file = self.canonical(&path);
            let kept = by_file.entry(file.clone()).or_insert_with(|| dir.clone());
            if path == file {
                *kept = dir;
            }
        }
        let mut dirs: Vec<PathBuf> = by_file.into_values().collect();
        dirs.sort();
        dirs
    }

    /// The dependent's required compatibility version against the current
    /// version of the payload file it will bind to (`file` payload-relative;
    /// a link is resolved). dyld does not enforce the requirement, so a
    /// policy row may allow an older file with a reason, which the round
    /// notes; without one, an older file is an error naming both numbers.
    fn compatible(
        &mut self,
        image: &Image,
        reference: &str,
        file: &Path,
        allow_older: Option<&str>,
    ) -> bool {
        let file = self.canonical(file);
        let Some(required) = image.required.get(reference).copied() else {
            return true;
        };
        let current = self
            .tree
            .images
            .iter()
            .find(|candidate| Path::new(&candidate.relative) == file)
            .and_then(|candidate| candidate.current_version);
        match (
            current,
            allow_older.filter(|reason| !reason.trim().is_empty()),
        ) {
            (Some(current), _) if required <= current => true,
            (Some(current), Some(reason)) => {
                self.plan.notes.push(format!(
                    "{}: {reference} requires compatibility version {required} and {} is current version {current}; allowed by its SONAME_POLICY row: {reason}",
                    image.relative,
                    file.display()
                ));
                true
            }
            (Some(current), None) => {
                self.fail(
                    &image.relative,
                    reference,
                    &format!(
                        "requires compatibility version {required} but {} is current version {current}; only a SONAME_POLICY row with allow_older_compat and a reason binds a dependent to an older file",
                        file.display()
                    ),
                );
                false
            }
            (None, _) => {
                self.fail(
                    &image.relative,
                    reference,
                    &format!(
                        "requires compatibility version {required} and {} has no LC_ID_DYLIB current version to check it against",
                        file.display()
                    ),
                );
                false
            }
        }
    }

    /// Two regular files with one install name: the ones that are not the
    /// policy's file become links to it, or the group is refused.
    fn duplicates(&mut self) {
        let mut by_id: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for image in &self.tree.images {
            if let Some(id) = &image.id {
                by_id
                    .entry(id.clone())
                    .or_default()
                    .push(image.relative.clone());
            }
        }
        for (id, files) in by_id {
            if files.len() < 2 {
                continue;
            }
            match policy(file_name(&id)) {
                SonamePolicy::Payload {
                    file: canonical, ..
                } if files.iter().any(|file| file == canonical) => {
                    for file in files.iter().filter(|file| *file != canonical) {
                        self.plan.replaced.push(Link {
                            link: PathBuf::from(file),
                            target: relative_path(&binary_dir(file), Path::new(canonical)),
                        });
                    }
                }
                _ => self.fail(&files.join(", "), &id, DUPLICATE_REASON),
            }
        }
    }

    /// Whether `reference` may bind to `shipped`, the regular file a
    /// destination resolves to, whichever path reached it: the closure's
    /// own copies under `wine/lib/` bind on their version alone; a file
    /// another component ships binds only through a policy row naming it,
    /// with the row's allowance. The refusal is recorded.
    fn rebind(&mut self, image: &Image, reference: &str, leaf: &str, shipped: &Path) -> bool {
        let allow_older = if shipped.parent() == Some(Path::new(LIB_DIR)) {
            None
        } else {
            match policy(leaf) {
                SonamePolicy::Payload {
                    file,
                    allow_older_compat,
                } if Path::new(file) == shipped => allow_older_compat,
                _ => {
                    self.fail(
                        &image.relative,
                        reference,
                        &format!(
                            "{} already ships {leaf}; a rebinding to a file the payload ships needs a SONAME_POLICY row naming that file",
                            shipped.display()
                        ),
                    );
                    return false;
                }
            }
        };
        self.compatible(image, reference, shipped, allow_older)
    }

    fn soname(&mut self, soname: &str) {
        let dest = Path::new(LIB_DIR).join(soname);
        if self.tree.files.contains(&dest) {
            // What is there must be a dylib: dlopen of a leaf name that
            // resolves to anything else fails at run time, whatever the
            // slot's name says.
            let file = self.canonical(&dest);
            let is_dylib = self
                .tree
                .images
                .iter()
                .any(|image| Path::new(&image.relative) == file && image.id.is_some());
            if is_dylib {
                self.supplied.insert(soname.to_owned());
            } else if file == dest {
                self.fail(
                    CONFIG_H,
                    soname,
                    &format!("{} is not a dylib (no LC_ID_DYLIB)", dest.display()),
                );
            } else {
                self.fail(
                    CONFIG_H,
                    soname,
                    &format!(
                        "{} is a link to {}, which is not a dylib (no LC_ID_DYLIB)",
                        dest.display(),
                        file.display()
                    ),
                );
            }
            return;
        }
        let policy = policy(soname);
        if policy != SonamePolicy::System && !self.free(CONFIG_H, soname, &dest) {
            return;
        }
        match policy {
            SonamePolicy::System => {}
            SonamePolicy::Payload { file: path, .. } => {
                if self.tree.files.contains(Path::new(path)) {
                    self.plan.links.push(Link {
                        link: dest.clone(),
                        target: relative_path(Path::new(LIB_DIR), Path::new(path)),
                    });
                    self.planned.insert(dest);
                    self.supplied.insert(soname.to_owned());
                } else {
                    self.fail(
                        CONFIG_H,
                        soname,
                        &format!("the SONAME policy names {path}, which is not in the payload"),
                    );
                }
            }
            SonamePolicy::Homebrew | SonamePolicy::Optional => {
                let elsewhere = dirs_providing(self.tree, &self.planned, Path::new(soname));
                if !elsewhere.is_empty() {
                    self.fail(
                        CONFIG_H,
                        soname,
                        &format!(
                            "{} already ships it; dyld keeps one image per install name per process, so the SONAME policy must name that file instead of a second copy",
                            join(&elsewhere)
                        ),
                    );
                    return;
                }
                match (self.source)(soname) {
                    Ok(file) => {
                        self.copy(file, dest);
                        self.supplied.insert(soname.to_owned());
                    }
                    Err(reason) if policy == SonamePolicy::Optional => {
                        self.plan.notes.push(format!(
                            "{soname}: optional, not shipped ({HOMEBREW_LIB}/{soname}: {reason})"
                        ));
                    }
                    Err(reason) => self.fail(
                        CONFIG_H,
                        soname,
                        &format!(
                            "dlopen'd by name; {HOMEBREW_LIB}/{soname}: {reason}; no row in the SONAME policy"
                        ),
                    ),
                }
            }
        }
    }

    fn image(&mut self, image: &Image) {
        let dir = binary_dir(&image.relative);
        let mut pending = ImageEdit {
            reachable: loader_dirs(&dir, &image.rpaths),
            dir,
            edit: Edit::default(),
        };
        if image.id.as_deref().is_some_and(is_foreign) {
            pending.edit.id = Some(format!("@rpath/{}", file_name(&image.relative)));
        }
        for rpath in &image.rpaths {
            if is_foreign(rpath) {
                pending.edit.delete_rpaths.push(rpath.clone());
            }
        }
        if !image.executable && image.relative.starts_with(MODULE_DIR) {
            for soname in &image.sonames {
                if self.supplied.contains(soname) {
                    pending.reach(Path::new(LIB_DIR));
                }
            }
        }
        let mut rpath_references = Vec::new();
        for reference in &image.dependencies {
            if reference.starts_with("/System/Volumes/") {
                self.fail(
                    &image.relative,
                    reference,
                    "/System/Volumes/ is a firmlink into the data volume; nothing to copy",
                );
            } else if is_system(reference)
                || reference.starts_with("@loader_path/")
                || reference.starts_with("@executable_path/")
            {
            } else if let Some(rest) = reference.strip_prefix("@rpath/") {
                rpath_references.push((reference.as_str(), rest));
            } else if reference.starts_with('/') {
                self.absolute_dependency(image, &mut pending, reference);
            } else {
                self.fail(
                    &image.relative,
                    reference,
                    "not an absolute, @rpath, @loader_path, or @executable_path reference",
                );
            }
        }
        // The unambiguous references settle the image's run paths first, so
        // a leaf the payload holds in two directories is judged against every
        // run path the image will have, whatever the load-command order.
        let (ambiguous, plain): (Vec<_>, Vec<_>) = rpath_references
            .into_iter()
            .partition(|(_, rest)| self.providers(Path::new(rest)).len() > 1);
        for (reference, rest) in plain.into_iter().chain(ambiguous) {
            self.rpath_dependency(image, &mut pending, reference, rest);
        }
        if !pending.edit.is_empty() {
            self.plan.edits.insert(image.relative.clone(), pending.edit);
        }
    }

    fn rpath_dependency(
        &mut self,
        image: &Image,
        pending: &mut ImageEdit,
        reference: &str,
        rest: &str,
    ) {
        // Any directory holding the leaf, link or file, satisfies a run path
        // the image already has; the tie is judged over distinct files.
        let holding = dirs_providing(self.tree, &self.planned, Path::new(rest));
        if holding
            .iter()
            .any(|candidate| pending.reachable.contains(candidate))
        {
            return;
        }
        if holding.contains(&pending.dir) {
            let own = pending.dir.clone();
            pending.reach(&own);
            return;
        }
        let candidates = self.providers(Path::new(rest));
        match candidates.as_slice() {
            [] if rest.contains('/') => {
                self.fail(&image.relative, reference, "names no file in the payload");
            }
            [] => {
                let dest = Path::new(LIB_DIR).join(rest);
                if !self.free(&image.relative, reference, &dest) {
                    return;
                }
                match (self.source)(rest) {
                    Ok(file) => {
                        self.copy(file, dest);
                        pending.reach(Path::new(LIB_DIR));
                    }
                    Err(reason) => self.fail(
                        &image.relative,
                        reference,
                        &format!(
                            "names no file in the payload, and {HOMEBREW_LIB}/{rest}: {reason}"
                        ),
                    ),
                }
            }
            [only] => pending.reach(only),
            several => self.fail(
                &image.relative,
                reference,
                &format!(
                    "{rest} is in {} directories ({}) and none is on the binary's run path",
                    several.len(),
                    join(several)
                ),
            ),
        }
    }

    fn absolute_dependency(&mut self, image: &Image, pending: &mut ImageEdit, reference: &str) {
        let leaf = file_name(reference);
        let dest = Path::new(LIB_DIR).join(leaf);
        let reach = if self.present(&dest) {
            // The leaf under wine/lib/ is a copy an earlier round made (or
            // this one plans), or a link to a file another component ships:
            // the same rule as below, over the file the link resolves to.
            if self.tree.files.contains(&dest) {
                let shipped = self.canonical(&dest);
                if !self.rebind(image, reference, leaf, &shipped) {
                    return;
                }
            }
            PathBuf::from(LIB_DIR)
        } else {
            let providers = self.providers(Path::new(leaf));
            match providers.as_slice() {
                [] => {
                    if !self.free(&image.relative, reference, &dest) {
                        return;
                    }
                    match (self.source)(reference) {
                        Ok(file) => {
                            self.copy(file, dest);
                            PathBuf::from(LIB_DIR)
                        }
                        Err(reason) => {
                            self.fail(&image.relative, reference, &reason);
                            return;
                        }
                    }
                }
                // Another component ships the leaf (the framework's
                // proxy-libintl for gnutls): one file per install name, so
                // the reference turns to that file instead of a second copy,
                // but only through a policy row that names it, and only if
                // the file is new enough for the dependent.
                [only] => {
                    let shipped = self.canonical(&only.join(leaf));
                    if !self.rebind(image, reference, leaf, &shipped) {
                        return;
                    }
                    only.clone()
                }
                several => {
                    self.fail(
                        &image.relative,
                        reference,
                        &format!(
                            "{leaf} is in {} directories ({}); dyld keeps one image per install name per process",
                            several.len(),
                            join(several)
                        ),
                    );
                    return;
                }
            }
        };
        pending
            .edit
            .changes
            .push((reference.to_owned(), format!("@rpath/{leaf}")));
        pending.reach(&reach);
    }
}

fn join(dirs: &[PathBuf]) -> String {
    dirs.iter()
        .map(|dir| dir.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(", ")
}

fn is_system(reference: &str) -> bool {
    (reference.starts_with("/usr/lib/") || reference.starts_with("/System/"))
        && !reference.starts_with("/System/Volumes/")
}

/// An absolute path outside the system: a Homebrew install name or
/// reference, a build-directory path, or a payload path spelled absolutely.
fn is_foreign(reference: &str) -> bool {
    reference.starts_with('/') && !is_system(reference)
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The payload-relative directories an image's `@loader_path` run paths name.
fn loader_dirs(dir: &Path, rpaths: &[String]) -> BTreeSet<PathBuf> {
    rpaths
        .iter()
        .filter_map(|rpath| rpath.strip_prefix("@loader_path"))
        .filter(|rest| rest.is_empty() || rest.starts_with('/'))
        .filter_map(|rest| normalize_relative(&dir.join(rest.trim_start_matches('/'))))
        .collect()
}

/// The `@loader_path` run path that reaches `target` from a binary in `dir`.
pub fn rpath_from(dir: &Path, target: &Path) -> String {
    let relative = relative_path(dir, target);
    if relative.as_os_str().is_empty() {
        "@loader_path".to_owned()
    } else {
        format!("@loader_path/{}", relative.display())
    }
}

/// `to` relative to `from`, both payload-relative and normalized: `..` for
/// each component of `from` outside the common prefix, then the rest of `to`.
pub fn relative_path(from: &Path, to: &Path) -> PathBuf {
    let from: Vec<_> = from.components().collect();
    let to: Vec<_> = to.components().collect();
    let common = from
        .iter()
        .zip(&to)
        .take_while(|(left, right)| left == right)
        .count();
    let mut out = PathBuf::new();
    for _ in common..from.len() {
        out.push("..");
    }
    for component in &to[common..] {
        out.push(component);
    }
    out
}

/// The directories `D` for which `D/rest` is a file of the tree or a planned
/// copy, sorted.
fn dirs_providing(tree: &Tree, planned: &BTreeSet<PathBuf>, rest: &Path) -> Vec<PathBuf> {
    let depth = rest.components().count();
    tree.files
        .iter()
        .chain(planned)
        .filter(|path| path.ends_with(rest))
        .map(|path| {
            path.components()
                .take(path.components().count() - depth)
                .collect::<PathBuf>()
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The `SONAME_LIB*` values `config.h` defines, sorted and unique.
pub fn sonames_from_config_h(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("#define SONAME_LIB"))
        .filter_map(|rest| rest.split_once(' '))
        .filter_map(|(_, value)| value.trim().strip_prefix('"')?.strip_suffix('"'))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// `(formula, version)` of a file under the x86-64 Homebrew Cellar, whose
/// first component is the formula name (`openssl@3` for a versioned one).
pub fn homebrew_formula(source: &Path) -> Option<(String, String)> {
    let mut rest = source.strip_prefix(HOMEBREW_CELLAR).ok()?.components();
    let formula = rest.next()?.as_os_str().to_str()?.to_owned();
    let version = rest.next()?.as_os_str().to_str()?.to_owned();
    is_formula_name(&formula).then_some((formula, version))
}

/// What the first bytes of a file say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Header {
    Thin { cputype: u32 },
    Fat,
    NotMachO,
}

/// Reads a Mach-O header: the magic and, for a thin file, its `cputype` in
/// the magic's byte order.
pub fn header(bytes: &[u8]) -> Header {
    let Some(magic) = bytes.get(..4) else {
        return Header::NotMachO;
    };
    let cputype = |big_endian: bool| {
        let field: [u8; 4] = bytes.get(4..8)?.try_into().ok()?;
        Some(if big_endian {
            u32::from_be_bytes(field)
        } else {
            u32::from_le_bytes(field)
        })
    };
    match magic {
        [0xcf | 0xce, 0xfa, 0xed, 0xfe] => cputype(false),
        [0xfe, 0xed, 0xfa, 0xcf | 0xce] => cputype(true),
        [0xca, 0xfe, 0xba, 0xbe | 0xbf] | [0xbe | 0xbf, 0xba, 0xfe, 0xca] => {
            return Header::Fat;
        }
        _ => None,
    }
    .map_or(Header::NotMachO, |cputype| Header::Thin { cputype })
}

/// Closes the payload at `payload` and runs the checker on the result.
pub fn close(root: &Path, payload: &Path) -> Result<(), String> {
    let payload = payload
        .canonicalize()
        .map_err(|error| format!("{}: {error}", payload.display()))?;
    let config_h = root.join(CONFIG_H);
    let sonames = sonames_from_config_h(&fs::read_to_string(&config_h).map_err(|error| {
        format!(
            "{}: {error} (the SONAME list comes from the Wine build tree; run sh build/wine/build.sh)",
            config_h.display()
        )
    })?);
    let denylist = load_denylist(root)?;
    let bundle = payload.join(BUNDLE_FILE);
    let spec = fs::read_to_string(&bundle)
        .map_err(|error| {
            format!(
                "{}: {error} (assemble.sh writes it before the closure)",
                bundle.display()
            )
        })
        .and_then(|text| {
            BundleSpec::parse(&text).map_err(|error| format!("{}: {error}", bundle.display()))
        })?;
    println!(
        "[xtask] close {}: {} SONAME(s) from {}: {}",
        payload.display(),
        sonames.len(),
        CONFIG_H,
        sonames.join(" ")
    );

    let source = |reference: &str| -> Result<PathBuf, String> {
        let path = if reference.starts_with('/') {
            PathBuf::from(reference)
        } else {
            Path::new(HOMEBREW_LIB).join(reference)
        };
        let file = path.canonicalize().map_err(|error| format!("{error}"))?;
        if !file.starts_with(HOMEBREW_PREFIX) {
            return Err(format!(
                "resolves to {}, outside the x86-64 Homebrew prefix {HOMEBREW_PREFIX}",
                file.display()
            ));
        }
        if !file.is_file() {
            return Err("not a regular file".into());
        }
        Ok(file)
    };
    let scan_tree = |payload: &Path| scan(payload, &sonames);
    let gate = |plan: &Plan| gate_copies(&denylist, &spec, plan);
    let mut runner = |program: &str, args: &[&OsStr]| run(program, args);
    let outcome = settle(&payload, &sonames, &source, &scan_tree, &gate, &mut runner)?;
    // A run that copied anything saw every formula the tree needs; a row
    // none of them matched would stay in the notices as a copied library.
    if outcome.copied > 0 {
        for formula in spec.components.libraries.keys() {
            if !outcome.formulas.contains(formula) && shipped_elsewhere(formula).is_none() {
                eprintln!(
                    "[xtask] close: [components.{formula}] names a formula no copy came from; a stale row stays in the notices"
                );
            }
        }
    }
    println!(
        "[xtask] close {}: settled after {} round(s): {} librar{} copied, {} link(s) made, {} binar{} rewritten",
        payload.display(),
        outcome.rounds,
        outcome.copied,
        if outcome.copied == 1 { "y" } else { "ies" },
        outcome.linked,
        outcome.rewritten.len(),
        if outcome.rewritten.len() == 1 {
            "y"
        } else {
            "ies"
        },
    );
    check(root, &payload)
}

/// What [`settle`] did to the tree.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Rounds that changed the tree.
    pub rounds: usize,
    pub copied: usize,
    /// Links created, the replaced duplicates included.
    pub linked: usize,
    pub rewritten: BTreeSet<String>,
    /// The Homebrew formulas the copies came from.
    pub formulas: BTreeSet<String>,
}

/// The fixed-point loop: scan, plan, gate, apply, until a round plans
/// nothing. A round with an unresolvable reference or a refused copy
/// applies nothing; a round a tool fails in is rolled back by [`apply`].
fn settle(
    payload: &Path,
    sonames: &[String],
    source: &Source<'_>,
    scan: &dyn Fn(&Path) -> Result<Tree, String>,
    gate: &dyn Fn(&Plan) -> Result<(), String>,
    run: &mut Runner<'_>,
) -> Result<Outcome, String> {
    let mut outcome = Outcome::default();
    let mut noted: BTreeSet<String> = BTreeSet::new();
    loop {
        if outcome.rounds == MAX_ROUNDS {
            return Err(format!(
                "no fixed point after {MAX_ROUNDS} rounds: a dependency chain deeper than that, or a tree that keeps changing"
            ));
        }
        let tree = scan(payload)?;
        let plan = plan(&tree, sonames, source);
        for note in &plan.notes {
            if noted.insert(note.clone()) {
                println!("[xtask] close: {note}");
            }
        }
        if !plan.errors.is_empty() {
            for error in &plan.errors {
                eprintln!("[xtask] close: {error}");
            }
            return Err(format!(
                "{} reference(s) cannot be closed (round {}); nothing of the round was applied",
                plan.errors.len(),
                outcome.rounds + 1
            ));
        }
        if plan.is_empty() {
            return Ok(outcome);
        }
        gate(&plan)?;
        apply(payload, &plan, run)?;
        outcome.rounds += 1;
        outcome.copied += plan.copies.len();
        outcome.linked += plan.links.len() + plan.replaced.len();
        outcome.rewritten.extend(plan.edits.keys().cloned());
        outcome.formulas.extend(
            plan.copies
                .iter()
                .filter_map(|copy| homebrew_formula(&copy.source).map(|(formula, _)| formula)),
        );
    }
}

/// Refuses a round whose copies are denylisted or whose Homebrew provenance
/// `BUNDLE.toml` does not record, before anything lands in the tree.
fn gate_copies(denylist: &Denylist, spec: &BundleSpec, plan: &Plan) -> Result<(), String> {
    let paths: Vec<String> = plan
        .copies
        .iter()
        .map(|copy| copy.dest.to_string_lossy().into_owned())
        .chain(
            plan.links
                .iter()
                .chain(&plan.replaced)
                .map(|link| link.link.to_string_lossy().into_owned()),
        )
        .collect();
    let hits = denylist.scan(paths.iter().map(String::as_str));
    for hit in &hits {
        eprintln!("[xtask] denylist: {hit}");
    }
    if !hits.is_empty() {
        return Err(format!(
            "{} denylisted path(s) would be copied in",
            hits.len()
        ));
    }
    let mut missing = Vec::new();
    for copy in &plan.copies {
        let Some((formula, version)) = homebrew_formula(&copy.source) else {
            let why = if copy.source.starts_with(HOMEBREW_CELLAR) {
                "its Cellar directory is not a Homebrew formula name"
            } else {
                "not under /usr/local/Cellar/<formula>/<version>/"
            };
            return Err(format!(
                "{}: {why}, so its formula and version are unknown",
                copy.source.display()
            ));
        };
        match spec.components.libraries.get(&formula) {
            Some(component) if component.version == version => {}
            Some(component) => missing.push(format!(
                "{} comes from {formula} {version} but [components.{formula}] records {}",
                copy.dest.display(),
                component.version
            )),
            None => match named_row(&formula) {
                // The spec refuses such a row by design; the pinned file
                // enters the payload through the SONAME policy instead.
                Some(row) => missing.push(format!(
                    "{} comes from {formula} {version}, which spells the pinned component row `{row}`; a copy of it is never right: a SONAME_POLICY row naming the pinned file is",
                    copy.dest.display()
                )),
                None => missing.push(format!(
                    "{} comes from {formula} {version} and build/bundle-spec.toml has no [components.{formula}] row",
                    copy.dest.display()
                )),
            },
        }
    }
    for line in &missing {
        eprintln!("[xtask] close: {line}");
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} copied librar{} lack a matching [components] row (the notices would not list them)",
            missing.len(),
            if missing.len() == 1 { "y" } else { "ies" }
        ))
    }
}

/// Reads every Mach-O of the tree.
fn scan(payload: &Path, sonames: &[String]) -> Result<Tree, String> {
    let mut entries = Vec::new();
    walk(payload, payload, &mut entries)?;
    let mut tree = Tree::default();
    for entry in &entries {
        if let Some(target) = &entry.link_target {
            let path = PathBuf::from(&entry.relative);
            match fs::canonicalize(&entry.absolute) {
                Ok(real) if real.starts_with(payload) && real.is_file() => {
                    let real = real
                        .strip_prefix(payload)
                        .map_or_else(|_| real.clone(), Path::to_path_buf);
                    tree.files.insert(path.clone());
                    tree.links.insert(path, real);
                }
                Ok(real) if real.starts_with(payload) => {
                    tree.occupied
                        .insert(path, format!("a link to {}, a directory", target.display()));
                }
                Ok(real) => {
                    tree.occupied.insert(
                        path,
                        format!(
                            "a link to {} that resolves outside the payload ({})",
                            target.display(),
                            real.display()
                        ),
                    );
                }
                Err(_) => {
                    tree.occupied
                        .insert(path, format!("a dangling link to {}", target.display()));
                }
            }
            continue;
        }
        if !entry.is_file {
            continue;
        }
        tree.files.insert(PathBuf::from(&entry.relative));
        if !is_macho(&entry.absolute)? {
            continue;
        }
        let load_commands = otool(&entry.absolute, "-l")?;
        let executable = is_executable(&load_commands);
        let versions = dylib_commands(&load_commands);
        let mut image = Image {
            relative: entry.relative.clone(),
            id: id_entry(&load_commands).map(str::to_owned),
            dependencies: dependency_entries(&load_commands)
                .map(str::to_owned)
                .collect(),
            rpaths: rpath_entries(&load_commands).map(str::to_owned).collect(),
            executable,
            sonames: BTreeSet::new(),
            current_version: versions
                .iter()
                .find(|command| command.command == "LC_ID_DYLIB")
                .and_then(|command| Version::parse(command.current)),
            required: versions
                .iter()
                .filter(|command| command.command != "LC_ID_DYLIB")
                .filter_map(|command| {
                    Some((
                        command.name.to_owned(),
                        Version::parse(command.compatibility)?,
                    ))
                })
                .collect(),
        };
        if !executable && entry.relative.starts_with(MODULE_DIR) {
            let bytes = fs::read(&entry.absolute)
                .map_err(|error| format!("{}: {error}", entry.absolute.display()))?;
            image.sonames = sonames
                .iter()
                .filter(|soname| contains(&bytes, soname.as_bytes()))
                .cloned()
                .collect();
        }
        tree.images.push(image);
    }
    Ok(tree)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// Applies one round, whole or not at all: every path the round creates,
/// replaces, or rewrites is journaled, and a failure removes what was
/// created and puts back what existed before, from its pre-image, before the
/// error (the file and the tool's stderr) returns. Every path is checked to
/// resolve inside the payload first, and no tool runs on a link.
fn apply(payload: &Path, plan: &Plan, run: &mut Runner<'_>) -> Result<(), String> {
    let payload = payload
        .canonicalize()
        .map_err(|error| format!("{}: {error}", payload.display()))?;
    let mut journal = Journal::new()?;
    match apply_journaled(&payload, plan, run, &mut journal) {
        Ok(()) => {
            journal.discard();
            Ok(())
        }
        Err(error) => Err(match journal.restore() {
            Ok((removed, restored)) => format!(
                "{error}; the round was rolled back ({removed} created path(s) removed, {restored} pre-existing file(s) restored)"
            ),
            Err(failure) => format!("{error}; rolling the round back failed too: {failure}"),
        }),
    }
}

fn apply_journaled(
    payload: &Path,
    plan: &Plan,
    run: &mut Runner<'_>,
    journal: &mut Journal,
) -> Result<(), String> {
    for copy in &plan.copies {
        let dest = inside(payload, &copy.dest)?;
        journal.create_dirs(payload, &copy.dest)?;
        journal.created(&dest)?;
        copy_library(&copy.source, &dest, run)?;
        println!(
            "[xtask] close: copied {} <- {}",
            copy.dest.display(),
            copy.source.display()
        );
    }
    for link in &plan.links {
        let path = inside(payload, &link.link)?;
        journal.create_dirs(payload, &link.link)?;
        journal.created(&path)?;
        symlink(&link.target, &path).map_err(|error| format!("{}: {error}", path.display()))?;
        println!(
            "[xtask] close: linked {} -> {}",
            link.link.display(),
            link.target.display()
        );
    }
    for link in &plan.replaced {
        let path = inside(payload, &link.link)?;
        regular(&link.link, &path)?;
        journal.save(&path)?;
        fs::remove_file(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        symlink(&link.target, &path).map_err(|error| format!("{}: {error}", path.display()))?;
        println!(
            "[xtask] close: replaced {} with a link to {}",
            link.link.display(),
            link.target.display()
        );
    }
    for (binary, edit) in &plan.edits {
        let path = inside(payload, Path::new(binary))?;
        regular(Path::new(binary), &path)?;
        journal.save(&path)?;
        install_name_tool(&path, edit, run)?;
        codesign(&path, run)?;
        println!("[xtask] close: rewrote {binary}: {}", describe(edit));
    }
    Ok(())
}

/// `payload/relative`, once every component on the way is known to stay
/// inside the payload: a link among them that leads out of the tree, or
/// nowhere, is an error naming the link, its target, and the component,
/// whatever a stale or hand-placed stage says. `payload` is canonical.
fn inside(payload: &Path, relative: &Path) -> Result<PathBuf, String> {
    let mut current = payload.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(format!("{}: not a plain relative path", relative.display()));
        };
        current.push(name);
        let Ok(meta) = fs::symlink_metadata(&current) else {
            // The rest does not exist yet; nothing there can redirect.
            break;
        };
        if !meta.file_type().is_symlink() {
            continue;
        }
        let component = current
            .strip_prefix(payload)
            .map_or_else(|_| current.clone(), Path::to_path_buf);
        let target =
            fs::read_link(&current).map_err(|error| format!("{}: {error}", current.display()))?;
        match fs::canonicalize(&current) {
            Ok(real) if real.starts_with(payload) => {}
            Ok(real) => {
                return Err(format!(
                    "{}: {} is a link to {} that resolves outside the payload ({}); nothing written",
                    relative.display(),
                    component.display(),
                    target.display(),
                    real.display()
                ));
            }
            Err(_) => {
                return Err(format!(
                    "{}: {} is a dangling link to {}; nothing written",
                    relative.display(),
                    component.display(),
                    target.display()
                ));
            }
        }
    }
    Ok(payload.join(relative))
}

/// Refuses a link where a regular file is to be rewritten or replaced:
/// `install_name_tool` and `codesign` open the path they are given, so an
/// edit through a link lands on its target.
fn regular(relative: &Path, path: &Path) -> Result<(), String> {
    let meta =
        fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", relative.display()))?;
    if meta.file_type().is_symlink() {
        let target = fs::read_link(path).unwrap_or_default();
        return Err(format!(
            "{}: is a link to {}; install_name_tool and codesign would rewrite its target",
            relative.display(),
            target.display()
        ));
    }
    Ok(())
}

static JOURNALS: AtomicUsize = AtomicUsize::new(0);

/// A pre-image: a regular file copied aside, or a link's target.
enum Saved {
    File { path: PathBuf, backup: PathBuf },
    Link { path: PathBuf, target: PathBuf },
}

/// The pre-images of one round: paths it created (to remove) and what
/// existed at the paths it rewrote or replaced (to put back).
struct Journal {
    dir: PathBuf,
    created: Vec<PathBuf>,
    saved: Vec<Saved>,
}

impl Journal {
    fn new() -> Result<Self, String> {
        let dir = std::env::temp_dir().join(format!(
            "applejack-closure-{}-{}",
            std::process::id(),
            JOURNALS.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        Ok(Self {
            dir,
            created: Vec::new(),
            saved: Vec::new(),
        })
    }

    /// Records a path the round is about to create, which must not exist:
    /// what is there already (a link the planner did not count, say) is
    /// never overwritten, and only what the round creates is removed on
    /// rollback.
    fn created(&mut self, path: &Path) -> Result<(), String> {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.created.push(path.to_path_buf());
                Ok(())
            }
            Ok(meta) => {
                let what = if meta.file_type().is_symlink() {
                    format!(
                        "a link to {}",
                        fs::read_link(path).unwrap_or_default().display()
                    )
                } else if meta.is_dir() {
                    "a directory".to_owned()
                } else {
                    "a file".to_owned()
                };
                Err(format!(
                    "{}: already exists ({what}); the plan expected it absent",
                    path.display()
                ))
            }
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }

    /// Creates the missing parent directories of `payload/relative`, each
    /// recorded as created.
    fn create_dirs(&mut self, payload: &Path, relative: &Path) -> Result<(), String> {
        let mut current = payload.to_path_buf();
        for component in relative.parent().unwrap_or(Path::new("")).components() {
            current.push(component);
            if fs::symlink_metadata(&current).is_err() {
                self.created.push(current.clone());
                fs::create_dir(&current)
                    .map_err(|error| format!("{}: {error}", current.display()))?;
            }
        }
        Ok(())
    }

    /// Saves what is at `path` before the round rewrites or replaces it: a
    /// regular file is copied aside, a link's target is recorded so the
    /// link comes back as a link. `apply` never saves a link today, since
    /// `regular` refuses one before any save; the link branch is defense
    /// in depth for a caller that does not, and the unit test is what
    /// reaches it.
    fn save(&mut self, path: &Path) -> Result<(), String> {
        let meta = fs::symlink_metadata(path).map_err(|error| {
            format!(
                "{}: cannot save it aside before rewriting it: {error}",
                path.display()
            )
        })?;
        if meta.file_type().is_symlink() {
            let target =
                fs::read_link(path).map_err(|error| format!("{}: {error}", path.display()))?;
            self.saved.push(Saved::Link {
                path: path.to_path_buf(),
                target,
            });
            return Ok(());
        }
        let backup = self.dir.join(self.saved.len().to_string());
        fs::copy(path, &backup).map_err(|error| {
            format!(
                "{}: cannot save a copy aside before rewriting it: {error}",
                path.display()
            )
        })?;
        self.saved.push(Saved::File {
            path: path.to_path_buf(),
            backup,
        });
        Ok(())
    }

    /// Removes what the round created and puts back what existed before, in
    /// reverse order; `(removed, restored)`, or every failure. Only what
    /// existed before the round counts as restored.
    fn restore(self) -> Result<(usize, usize), String> {
        let mut removed = 0;
        let mut restored = 0;
        let mut failures = Vec::new();
        for path in self.created.iter().rev() {
            match remove(path) {
                Ok(()) => removed += 1,
                Err(error) => failures.push(error),
            }
        }
        for saved in self.saved.iter().rev() {
            let (path, put_back): (&Path, Box<dyn Fn() -> std::io::Result<()>>) = match saved {
                Saved::File { path, backup } => {
                    (path, Box::new(move || fs::copy(backup, path).map(|_| ())))
                }
                Saved::Link { path, target } => (path, Box::new(move || symlink(target, path))),
            };
            // A replaced file is a link now; the copy must not follow it.
            if let Err(error) = remove(path) {
                failures.push(error);
                continue;
            }
            match put_back() {
                Ok(()) => restored += 1,
                Err(error) => failures.push(format!("{}: {error}", path.display())),
            }
        }
        let _ = fs::remove_dir_all(&self.dir);
        if failures.is_empty() {
            Ok((removed, restored))
        } else {
            Err(failures.join("; "))
        }
    }

    fn discard(self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Removes a file, a symlink, or an empty directory; an absent path is fine.
fn remove(path: &Path) -> Result<(), String> {
    let result = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => Err(error),
    };
    result.map_err(|error| format!("{}: {error}", path.display()))
}

fn describe(edit: &Edit) -> String {
    let mut parts = Vec::new();
    if let Some(id) = &edit.id {
        parts.push(format!("id {id}"));
    }
    for (old, new) in &edit.changes {
        parts.push(format!("{old} -> {new}"));
    }
    for rpath in &edit.add_rpaths {
        parts.push(format!("+rpath {rpath}"));
    }
    for rpath in &edit.delete_rpaths {
        parts.push(format!("-rpath {rpath}"));
    }
    parts.join(", ")
}

/// Copies one library into an existing directory, thinning a universal file
/// to x86-64, and refuses anything that is not an x86-64 Mach-O afterwards.
fn copy_library(source: &Path, dest: &Path, run: &mut Runner<'_>) -> Result<(), String> {
    let mut magic = [0_u8; 8];
    fs::File::open(source)
        .and_then(|mut file| file.read(&mut magic).map(|_| ()))
        .map_err(|error| format!("{}: {error}", source.display()))?;
    match header(&magic) {
        Header::Fat => run(
            "lipo",
            &[
                source.as_os_str(),
                OsStr::new("-thin"),
                OsStr::new("x86_64"),
                OsStr::new("-output"),
                dest.as_os_str(),
            ],
        )?,
        Header::Thin { .. } => {
            fs::copy(source, dest)
                .map_err(|error| format!("{} -> {}: {error}", source.display(), dest.display()))?;
        }
        Header::NotMachO => {
            return Err(format!("{}: not a Mach-O file", source.display()));
        }
    }
    fs::set_permissions(dest, fs::Permissions::from_mode(0o644))
        .map_err(|error| format!("{}: {error}", dest.display()))?;
    let mut magic = [0_u8; 8];
    fs::File::open(dest)
        .and_then(|mut file| file.read(&mut magic).map(|_| ()))
        .map_err(|error| format!("{}: {error}", dest.display()))?;
    match header(&magic) {
        Header::Thin {
            cputype: CPU_TYPE_X86_64,
        } => Ok(()),
        other => {
            let _ = fs::remove_file(dest);
            Err(format!(
                "{}: not an x86-64 Mach-O after copying ({other:?})",
                source.display()
            ))
        }
    }
}

fn install_name_tool(path: &Path, edit: &Edit, run: &mut Runner<'_>) -> Result<(), String> {
    let mut args: Vec<&OsStr> = Vec::new();
    if let Some(id) = &edit.id {
        args.extend([OsStr::new("-id"), OsStr::new(id)]);
    }
    for (old, new) in &edit.changes {
        args.extend([OsStr::new("-change"), OsStr::new(old), OsStr::new(new)]);
    }
    for rpath in &edit.add_rpaths {
        args.extend([OsStr::new("-add_rpath"), OsStr::new(rpath)]);
    }
    for rpath in &edit.delete_rpaths {
        args.extend([OsStr::new("-delete_rpath"), OsStr::new(rpath)]);
    }
    args.push(path.as_os_str());
    run("install_name_tool", &args)
}

/// Ad-hoc re-sign after `install_name_tool`. Nothing the closure edits today
/// was signed, so there is nothing to keep; the flag makes an edit of a
/// signed binary keep its entitlements, flags, and hardened runtime rather
/// than drop them silently.
fn codesign(path: &Path, run: &mut Runner<'_>) -> Result<(), String> {
    run(
        "codesign",
        &[
            OsStr::new("--force"),
            OsStr::new("--sign"),
            OsStr::new("-"),
            OsStr::new("--preserve-metadata=entitlements,flags,runtime"),
            path.as_os_str(),
        ],
    )
}

/// Runs a tool; stderr travels into the error so `install_name_tool`'s
/// "larger updated load commands do not fit" names the binary to relink
/// with `-headerpad_max_install_names`.
fn run(program: &str, args: &[&OsStr]) -> Result<(), String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("{program} failed to start: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "{program} {} failed ({}): {}",
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::ffi::OsStr;
    use std::fs;
    use std::io::Write as _;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};

    use super::{
        Copy, Edit, Header, Image, Journal, Link, Outcome, Plan, Round, SONAME_POLICY,
        SonamePolicy, Tree, Version, apply, gate_copies, header, homebrew_formula, plan,
        relative_path, rpath_from, scan, settle, sonames_from_config_h,
    };
    use crate::bundle_toml::{BundleSpec, SHIPPED_ELSEWHERE};
    use crate::denylist::Denylist;

    const MODULES: &str = "wine/lib/wine/x86_64-unix";
    const GST_LIB: &str = "wine/lib/GStreamer.framework/Versions/1.0/lib";
    const GST_RPATH: &str = "@loader_path/../../GStreamer.framework/Versions/1.0/lib";
    /// A thin x86-64 Mach-O header, enough for [`header`].
    const X86_64_MAGIC: [u8; 8] = [0xcf, 0xfa, 0xed, 0xfe, 0x07, 0x00, 0x00, 0x01];

    fn image(relative: &str, id: Option<&str>, deps: &[&str], rpaths: &[&str]) -> Image {
        Image {
            relative: relative.to_owned(),
            id: id.map(str::to_owned),
            dependencies: deps.iter().map(|dep| (*dep).to_owned()).collect(),
            rpaths: rpaths.iter().map(|rpath| (*rpath).to_owned()).collect(),
            executable: false,
            sonames: BTreeSet::new(),
            current_version: None,
            required: BTreeMap::new(),
        }
    }

    fn naming(mut image: Image, sonames: &[&str]) -> Image {
        image.sonames = sonames.iter().map(|soname| (*soname).to_owned()).collect();
        image
    }

    fn version(text: &str) -> Version {
        Version::parse(text).expect("a version")
    }

    /// The image with its own current version and the compatibility version
    /// it requires of each named dependency.
    fn versioned(mut image: Image, current: &str, required: &[(&str, &str)]) -> Image {
        image.current_version = Some(version(current));
        image.required = required
            .iter()
            .map(|(reference, compat)| ((*reference).to_owned(), version(compat)))
            .collect();
        image
    }

    fn tree(images: Vec<Image>, extra_files: &[&str]) -> Tree {
        let mut files: BTreeSet<PathBuf> = images
            .iter()
            .map(|image| PathBuf::from(&image.relative))
            .collect();
        files.extend(extra_files.iter().map(PathBuf::from));
        Tree {
            images,
            files,
            ..Tree::default()
        }
    }

    /// The tree with a symlink `link` resolving to the payload file `target`.
    fn linked(mut tree: Tree, link: &str, target: &str) -> Tree {
        tree.files.insert(link.into());
        tree.links.insert(link.into(), target.into());
        tree
    }

    fn replace_image(tree: &mut Tree, image: Image) {
        let slot = tree
            .images
            .iter_mut()
            .find(|candidate| candidate.relative == image.relative)
            .unwrap_or_else(|| panic!("no image {}", image.relative));
        *slot = image;
    }

    /// A Homebrew with gnutls, p11-kit, libpng, freetype, and gettext, none
    /// of them universal; `libodbc.dylib` is absent and `libarm.dylib` is
    /// arm64.
    fn homebrew(reference: &str) -> Result<PathBuf, String> {
        let leaf = reference.rsplit('/').next().unwrap_or(reference);
        let cellar = |formula: &str, version: &str| {
            Ok(PathBuf::from(format!(
                "/usr/local/Cellar/{formula}/{version}/lib/{leaf}"
            )))
        };
        match leaf {
            "libgnutls.30.dylib" => cellar("gnutls", "3.8.10"),
            "libp11-kit.0.dylib" => cellar("p11-kit", "0.25.5"),
            "libpng16.16.dylib" => cellar("libpng", "1.6.50"),
            "libfreetype.6.dylib" => cellar("freetype", "2.13.3"),
            "libintl.8.dylib" => cellar("gettext", "0.26"),
            "libarm.dylib" => Err(
                "resolves to /opt/homebrew/Cellar/arm/1/lib/libarm.dylib, outside the x86-64 Homebrew prefix /usr/local"
                    .into(),
            ),
            _ => Err("No such file or directory (os error 2)".into()),
        }
    }

    fn sonames(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn edit<'a>(plan: &'a Plan, binary: &str) -> &'a Edit {
        plan.edits
            .get(binary)
            .unwrap_or_else(|| panic!("no edit for {binary} in {plan:?}"))
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xtask-closure-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// Writes a stand-in x86-64 Mach-O with `tail` after the header.
    fn fake_macho(path: &Path, tail: &[u8]) {
        fs::create_dir_all(path.parent().expect("parent")).expect("parent dir");
        fs::write(path, [&X86_64_MAGIC[..], tail].concat()).expect("fake Mach-O");
    }

    #[test]
    fn relative_paths_climb_then_descend() {
        let rel = |from: &str, to: &str| relative_path(Path::new(from), Path::new(to));
        assert_eq!(rel(MODULES, "wine/lib"), PathBuf::from("../.."));
        assert_eq!(
            rel(MODULES, GST_LIB),
            PathBuf::from("../../GStreamer.framework/Versions/1.0/lib")
        );
        assert_eq!(
            rel("wine/lib", "vulkan/libMoltenVK.dylib"),
            PathBuf::from("../../vulkan/libMoltenVK.dylib")
        );
        assert_eq!(
            rel(GST_LIB, "vulkan/libMoltenVK.dylib"),
            PathBuf::from("../../../../../../vulkan/libMoltenVK.dylib")
        );
        assert_eq!(rel("wine/lib", "wine/lib"), PathBuf::new());
        assert_eq!(rel("", "wine/lib"), PathBuf::from("wine/lib"));
        assert_eq!(
            rpath_from(Path::new("wine/lib"), Path::new("wine/lib")),
            "@loader_path"
        );
        assert_eq!(
            rpath_from(Path::new(MODULES), Path::new("wine/lib")),
            "@loader_path/../.."
        );
    }

    #[test]
    fn config_h_lists_the_defined_sonames_only() {
        let text = "\
/* Define to the soname of the libcups library. */
#define SONAME_LIBCUPS \"libcups.2.dylib\"
/* #undef SONAME_LIBDBUS_1 */
#define SONAME_LIBFREETYPE \"libfreetype.6.dylib\"
#define SONAME_LIBMOLTENVK \"libMoltenVK.dylib\"
#define SONAME_LIBVULKAN \"libMoltenVK.dylib\"
#define HAVE_SOMETHING 1
";
        assert_eq!(
            sonames_from_config_h(text),
            [
                "libMoltenVK.dylib",
                "libcups.2.dylib",
                "libfreetype.6.dylib"
            ]
        );
    }

    #[test]
    fn cellar_paths_name_the_formula_and_version() {
        assert_eq!(
            homebrew_formula(Path::new(
                "/usr/local/Cellar/nettle/3.10.2/lib/libhogweed.6.11.dylib"
            )),
            Some(("nettle".to_owned(), "3.10.2".to_owned()))
        );
        assert_eq!(
            homebrew_formula(Path::new(
                "/usr/local/Cellar/openssl@3/3.5.1/lib/libssl.3.dylib"
            )),
            Some(("openssl@3".to_owned(), "3.5.1".to_owned()))
        );
        assert_eq!(
            homebrew_formula(Path::new("/usr/local/lib/libfoo.dylib")),
            None
        );
        assert_eq!(
            homebrew_formula(Path::new("/opt/homebrew/Cellar/x/1/lib/a")),
            None
        );
        assert_eq!(
            homebrew_formula(Path::new("/usr/local/Cellar/not a formula/1/lib/a")),
            None
        );
    }

    #[test]
    fn headers_tell_thin_x86_64_from_fat_and_arm64() {
        assert_eq!(
            header(&X86_64_MAGIC),
            Header::Thin {
                cputype: super::CPU_TYPE_X86_64
            }
        );
        let arm64 = [0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0x00, 0x00, 0x01];
        assert_eq!(
            header(&arm64),
            Header::Thin {
                cputype: 0x0100_000c
            }
        );
        let big_endian = [0xfe, 0xed, 0xfa, 0xcf, 0x01, 0x00, 0x00, 0x07];
        assert_eq!(
            header(&big_endian),
            Header::Thin {
                cputype: super::CPU_TYPE_X86_64
            }
        );
        assert_eq!(header(&[0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 2]), Header::Fat);
        assert_eq!(header(&[0xca, 0xfe, 0xba, 0xbf, 0, 0, 0, 2]), Header::Fat);
        assert_eq!(header(b"#!/bin/sh"), Header::NotMachO);
        assert_eq!(header(&[0xcf, 0xfa]), Header::NotMachO);
    }

    /// The staged framework's shape: `libgst*` libraries and `libintl` with
    /// the framework's own run paths.
    fn framework_images() -> Vec<Image> {
        vec![
            image(
                &format!("{GST_LIB}/libgstvideo-1.0.0.dylib"),
                Some("@rpath/libgstvideo-1.0.0.dylib"),
                &["@rpath/libgstreamer-1.0.0.dylib", "@rpath/libintl.8.dylib"],
                &["@executable_path/../lib", "@loader_path/../lib"],
            ),
            image(
                &format!("{GST_LIB}/libgstreamer-1.0.0.dylib"),
                Some("@rpath/libgstreamer-1.0.0.dylib"),
                &["@rpath/libintl.8.dylib"],
                &["@executable_path/../lib", "@loader_path/../lib"],
            ),
            image(
                &format!("{GST_LIB}/libintl.8.dylib"),
                Some("@rpath/libintl.8.dylib"),
                &[],
                &["@loader_path/../lib"],
            ),
        ]
    }

    /// The real payload's shape before the closure: winegstreamer's
    /// `@rpath/libgst*` references resolve only through a run path into the
    /// framework; the Wine modules' `@rpath/ntdll.so` already resolves
    /// through `@loader_path/`.
    #[test]
    fn a_module_gets_the_run_path_to_the_framework_library() {
        let winegstreamer = format!("{MODULES}/winegstreamer.so");
        let mut images = vec![
            image(
                &format!("{MODULES}/ntdll.so"),
                Some("@rpath/ntdll.so"),
                &["/usr/lib/libSystem.B.dylib"],
                &["@loader_path/"],
            ),
            image(
                &winegstreamer,
                Some("@rpath/winegstreamer.so"),
                &[
                    "@rpath/ntdll.so",
                    "@rpath/libgstvideo-1.0.0.dylib",
                    "@rpath/libgstreamer-1.0.0.dylib",
                    "@rpath/libintl.8.dylib",
                    "/usr/lib/libSystem.B.dylib",
                ],
                &["@loader_path/"],
            ),
        ];
        images.extend(framework_images());
        let t = tree(images, &[]);
        let p = plan(&t, &[], &homebrew);
        assert_eq!(p.errors, []);
        assert_eq!(p.copies, []);
        assert_eq!(
            p.edits.keys().collect::<Vec<_>>(),
            [&winegstreamer],
            "{p:?}"
        );
        assert_eq!(
            edit(&p, &winegstreamer),
            &Edit {
                add_rpaths: vec![GST_RPATH.into()],
                ..Edit::default()
            }
        );

        // After the rewrite the framework directory is on the run path and
        // the plan is empty: the closure is idempotent.
        let mut closed = t;
        closed.images[1].rpaths.push(GST_RPATH.into());
        assert!(plan(&closed, &[], &homebrew).is_empty());
    }

    /// A leaf the payload holds in two directories, one of them a link: the
    /// reference listed first is judged after the image's other references
    /// have settled its run paths, so load-command order does not decide.
    #[test]
    fn an_ambiguous_reference_listed_first_resolves_like_one_listed_last() {
        let winegstreamer = format!("{MODULES}/winegstreamer.so");
        let mut images = vec![image(
            &winegstreamer,
            Some("@rpath/winegstreamer.so"),
            &[
                "@rpath/libintl.8.dylib",
                "@rpath/libgstvideo-1.0.0.dylib",
                "@rpath/libgstreamer-1.0.0.dylib",
                "/usr/lib/libSystem.B.dylib",
            ],
            &["@loader_path/"],
        )];
        images.extend(framework_images());
        let t = tree(images, &["wine/lib/libintl.8.dylib"]);
        let p = plan(&t, &[], &homebrew);
        assert_eq!(p.errors, [], "{p:?}");
        assert_eq!(
            edit(&p, &winegstreamer),
            &Edit {
                add_rpaths: vec![GST_RPATH.into()],
                ..Edit::default()
            }
        );
        assert_eq!(p.edits.len(), 1, "{p:?}");
    }

    fn win32u_tree() -> Tree {
        tree(
            vec![
                naming(
                    image(
                        &format!("{MODULES}/win32u.so"),
                        Some("@rpath/win32u.so"),
                        &["@rpath/ntdll.so"],
                        &["@loader_path/"],
                    ),
                    &["libfreetype.6.dylib", "libMoltenVK.dylib"],
                ),
                naming(
                    image(
                        &format!("{MODULES}/bcrypt.so"),
                        Some("@rpath/bcrypt.so"),
                        &["@rpath/ntdll.so"],
                        &["@loader_path/"],
                    ),
                    &["libgnutls.30.dylib"],
                ),
                image(
                    "vulkan/libMoltenVK.dylib",
                    Some("@rpath/libMoltenVK.dylib"),
                    &["/usr/lib/libSystem.B.dylib"],
                    &[],
                ),
                image(
                    &format!("{GST_LIB}/libfreetype.6.dylib"),
                    Some("@rpath/libfreetype.6.dylib"),
                    &["/usr/lib/libSystem.B.dylib"],
                    &["@loader_path/../lib"],
                ),
            ],
            &[
                &format!("{MODULES}/ntdll.so"),
                &format!("{GST_LIB}/libintl.8.dylib"),
            ],
        )
    }

    const SONAMES: [&str; 5] = [
        "libfreetype.6.dylib",
        "libMoltenVK.dylib",
        "libgnutls.30.dylib",
        "libcups.2.dylib",
        "libodbc.dylib",
    ];

    /// The first round: freetype and `MoltenVK` are linked to the files the
    /// payload ships, gnutls is copied, cups is the system's, odbc is noted;
    /// each module naming a supplied SONAME gets the run path once.
    #[test]
    fn sonames_are_copied_linked_or_left_per_policy() {
        let p = plan(&win32u_tree(), &sonames(&SONAMES), &homebrew);
        assert_eq!(p.errors, []);
        assert_eq!(
            p.copies,
            [Copy {
                source: "/usr/local/Cellar/gnutls/3.8.10/lib/libgnutls.30.dylib".into(),
                dest: "wine/lib/libgnutls.30.dylib".into(),
            }]
        );
        assert_eq!(
            p.links,
            [
                Link {
                    link: "wine/lib/libfreetype.6.dylib".into(),
                    target: "GStreamer.framework/Versions/1.0/lib/libfreetype.6.dylib".into(),
                },
                Link {
                    link: "wine/lib/libMoltenVK.dylib".into(),
                    target: "../../vulkan/libMoltenVK.dylib".into(),
                }
            ]
        );
        assert_eq!(p.replaced, []);
        for module in ["win32u.so", "bcrypt.so"] {
            assert_eq!(
                edit(&p, &format!("{MODULES}/{module}")),
                &Edit {
                    add_rpaths: vec!["@loader_path/../..".into()],
                    ..Edit::default()
                }
            );
        }
        assert_eq!(p.notes.len(), 1, "{:?}", p.notes);
        assert!(
            p.notes[0].starts_with("libodbc.dylib: optional"),
            "{}",
            p.notes[0]
        );
    }

    /// The rounds after the copy: the copied library's own install name, a
    /// dependency the framework already ships (reached by a run path, not
    /// copied), a dependency copied beside it; then that one's install name
    /// and a stray absolute run path; then nothing.
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the three rounds are one walk over one tree; splitting them would repeat the setup"
    )]
    fn copied_libraries_are_closed_over_their_dependencies() {
        let names = sonames(&SONAMES);
        let mut t = linked(
            linked(
                win32u_tree(),
                "wine/lib/libfreetype.6.dylib",
                &format!("{GST_LIB}/libfreetype.6.dylib"),
            ),
            "wine/lib/libMoltenVK.dylib",
            "vulkan/libMoltenVK.dylib",
        );
        t.files.insert("wine/lib/libgnutls.30.dylib".into());
        for module in &mut t.images {
            if module.relative.starts_with(MODULES) {
                module.rpaths.push("@loader_path/../..".into());
            }
        }
        t.images.push(versioned(
            image(
                &format!("{GST_LIB}/libintl.8.dylib"),
                Some("@rpath/libintl.8.dylib"),
                &[],
                &["@loader_path/../lib"],
            ),
            "10.5.0",
            &[],
        ));
        t.images.push(versioned(
            image(
                "wine/lib/libgnutls.30.dylib",
                Some("/usr/local/opt/gnutls/lib/libgnutls.30.dylib"),
                &[
                    "/usr/local/opt/gettext/lib/libintl.8.dylib",
                    "/usr/local/opt/p11-kit/lib/libp11-kit.0.dylib",
                    "/usr/lib/libSystem.B.dylib",
                ],
                &[],
            ),
            "68.2.0",
            &[("/usr/local/opt/gettext/lib/libintl.8.dylib", "13.0.0")],
        ));
        let p = plan(&t, &names, &homebrew);
        assert_eq!(p.errors, []);
        assert_eq!(p.links, []);
        // gnutls binds to the framework's older proxy-libintl through the
        // policy row's allowance, which the round notes.
        assert!(
            p.notes
                .iter()
                .any(|note| note.contains("13.0.0")
                    && note.contains("allowed by its SONAME_POLICY row")),
            "{:?}",
            p.notes
        );
        assert_eq!(
            p.copies,
            [Copy {
                source: "/usr/local/Cellar/p11-kit/0.25.5/lib/libp11-kit.0.dylib".into(),
                dest: "wine/lib/libp11-kit.0.dylib".into(),
            }]
        );
        assert_eq!(
            edit(&p, "wine/lib/libgnutls.30.dylib"),
            &Edit {
                id: Some("@rpath/libgnutls.30.dylib".into()),
                changes: vec![
                    (
                        "/usr/local/opt/gettext/lib/libintl.8.dylib".into(),
                        "@rpath/libintl.8.dylib".into()
                    ),
                    (
                        "/usr/local/opt/p11-kit/lib/libp11-kit.0.dylib".into(),
                        "@rpath/libp11-kit.0.dylib".into()
                    ),
                ],
                add_rpaths: vec![
                    "@loader_path/GStreamer.framework/Versions/1.0/lib".into(),
                    "@loader_path".into(),
                ],
                delete_rpaths: vec![],
            }
        );
        assert_eq!(p.edits.len(), 1, "{p:?}");

        t.files.insert("wine/lib/libp11-kit.0.dylib".into());
        replace_image(
            &mut t,
            image(
                "wine/lib/libgnutls.30.dylib",
                Some("@rpath/libgnutls.30.dylib"),
                &["@rpath/libintl.8.dylib", "@rpath/libp11-kit.0.dylib"],
                &[
                    "@loader_path/GStreamer.framework/Versions/1.0/lib",
                    "@loader_path",
                ],
            ),
        );
        t.images.push(image(
            "wine/lib/libp11-kit.0.dylib",
            Some("/usr/local/opt/p11-kit/lib/libp11-kit.0.dylib"),
            &["/usr/lib/libSystem.B.dylib"],
            &["/usr/local/lib", "/usr/lib/swift"],
        ));
        let p = plan(&t, &names, &homebrew);
        assert_eq!(p.errors, []);
        assert_eq!(p.copies, []);
        assert_eq!(
            edit(&p, "wine/lib/libp11-kit.0.dylib"),
            &Edit {
                id: Some("@rpath/libp11-kit.0.dylib".into()),
                delete_rpaths: vec!["/usr/local/lib".into()],
                ..Edit::default()
            }
        );
        assert_eq!(p.edits.len(), 1, "{p:?}");

        replace_image(
            &mut t,
            image(
                "wine/lib/libp11-kit.0.dylib",
                Some("@rpath/libp11-kit.0.dylib"),
                &["/usr/lib/libSystem.B.dylib"],
                &["/usr/lib/swift"],
            ),
        );
        assert!(plan(&t, &names, &homebrew).is_empty());
    }

    /// A leaf in two directories, one of them a link to the other's file:
    /// one image to dyld, so no tie. The image whose run path reaches either
    /// is fine, and one reaching neither gets the run path to the regular
    /// file's directory.
    #[test]
    fn a_leaf_behind_a_link_is_one_file_not_a_tie() {
        let other = format!("{MODULES}/other.so");
        let mut t = tree(
            vec![
                image(
                    &format!("{GST_LIB}/libharfbuzz.0.dylib"),
                    Some("@rpath/libharfbuzz.0.dylib"),
                    &["@rpath/libfreetype.6.dylib"],
                    &["@loader_path/../lib"],
                ),
                image(
                    &other,
                    Some("@rpath/other.so"),
                    &["@rpath/libfreetype.6.dylib"],
                    &["@loader_path/"],
                ),
            ],
            &[
                "wine/lib/libfreetype.6.dylib",
                &format!("{GST_LIB}/libfreetype.6.dylib"),
            ],
        );
        t.links.insert(
            "wine/lib/libfreetype.6.dylib".into(),
            format!("{GST_LIB}/libfreetype.6.dylib").into(),
        );
        let p = plan(&t, &[], &homebrew);
        assert_eq!(p.errors, [], "{p:?}");
        assert_eq!(
            edit(&p, &other),
            &Edit {
                add_rpaths: vec![GST_RPATH.into()],
                ..Edit::default()
            }
        );
        assert_eq!(p.edits.len(), 1, "{p:?}");
    }

    fn permutations(items: &[&'static str]) -> Vec<Vec<&'static str>> {
        if items.len() <= 1 {
            return vec![items.to_vec()];
        }
        let mut out = Vec::new();
        for index in 0..items.len() {
            let mut rest = items.to_vec();
            let head = rest.remove(index);
            for mut tail in permutations(&rest) {
                tail.insert(0, head);
                out.push(tail);
            }
        }
        out
    }

    /// Every load-command order of winegstreamer's dependencies plans the
    /// same round. (The round-two closure prover's probe.)
    #[test]
    fn every_dependency_order_plans_the_same_round() {
        let winegstreamer = format!("{MODULES}/winegstreamer.so");
        let deps = [
            "@rpath/libintl.8.dylib",
            "@rpath/libgstvideo-1.0.0.dylib",
            "@rpath/libgstreamer-1.0.0.dylib",
            "/usr/lib/libSystem.B.dylib",
        ];
        let mut seen = Vec::new();
        for order in permutations(&deps) {
            let mut images = vec![image(
                &winegstreamer,
                Some("@rpath/winegstreamer.so"),
                &order,
                &["@loader_path/"],
            )];
            images.extend(framework_images());
            let t = tree(images, &["wine/lib/libintl.8.dylib"]);
            let p = plan(&t, &[], &homebrew);
            assert_eq!(p.errors, [], "{order:?}: {p:?}");
            seen.push(format!("{p:?}"));
        }
        assert_eq!(seen.len(), 24);
        assert!(seen.iter().all(|p| *p == seen[0]), "plans differ by order");
    }

    /// Two ambiguous leaves and one plain one, in every order: once the plain
    /// one puts its directory on the run path, both ambiguous references
    /// resolve through it; without it, both errors name both directories.
    /// (The round-two closure prover's probe.)
    #[test]
    fn two_ambiguous_references_and_one_plain_resolve_in_every_order() {
        let module = format!("{MODULES}/m.so");
        let deps = [
            "@rpath/libA.dylib",
            "@rpath/libB.dylib",
            "@rpath/libC.dylib",
        ];
        let files = [
            "x/libA.dylib",
            "y/libA.dylib",
            "y/libB.dylib",
            "z/libB.dylib",
            "y/libC.dylib",
        ];
        let mut seen = Vec::new();
        for order in permutations(&deps) {
            let t = tree(
                vec![image(
                    &module,
                    Some("@rpath/m.so"),
                    &order,
                    &["@loader_path/"],
                )],
                &files,
            );
            let p = plan(&t, &[], &homebrew);
            assert_eq!(p.errors, [], "{order:?}: {p:?}");
            assert_eq!(
                edit(&p, &module).add_rpaths,
                ["@loader_path/../../../../y"],
                "{order:?}"
            );
            seen.push(format!("{p:?}"));
        }
        assert!(seen.iter().all(|p| *p == seen[0]));
        for order in permutations(&deps[..2]) {
            let t = tree(
                vec![image(
                    &module,
                    Some("@rpath/m.so"),
                    &order,
                    &["@loader_path/"],
                )],
                &files,
            );
            let p = plan(&t, &[], &homebrew);
            let reasons: Vec<&str> = p.errors.iter().map(|e| e.reason.as_str()).collect();
            assert_eq!(p.errors.len(), 2, "{order:?}: {p:?}");
            assert!(
                reasons.iter().any(|r| r.contains("2 directories (x, y)")),
                "{reasons:?}"
            );
            assert!(
                reasons.iter().any(|r| r.contains("2 directories (y, z)")),
                "{reasons:?}"
            );
        }
    }

    /// Two regular files with one install name: the framework's `MoltenVK`
    /// becomes a link to the pinned one the policy names; two `libintl`s,
    /// which no policy row decides, are refused with both files named.
    #[test]
    fn a_duplicate_install_name_is_linked_to_the_policys_file_or_refused() {
        let framework_mvk = format!("{GST_LIB}/libMoltenVK.dylib");
        let mut t = tree(
            vec![
                image(
                    "vulkan/libMoltenVK.dylib",
                    Some("@rpath/libMoltenVK.dylib"),
                    &["/usr/lib/libc++.1.dylib"],
                    &[],
                ),
                image(
                    &framework_mvk,
                    Some("@rpath/libMoltenVK.dylib"),
                    &["/usr/lib/libc++.1.dylib"],
                    &["@loader_path/../lib"],
                ),
                image(
                    &format!("{GST_LIB}/gstreamer-1.0/libgstapplemedia.dylib"),
                    Some("@rpath/libgstapplemedia.dylib"),
                    &["@rpath/libMoltenVK.dylib"],
                    &["@loader_path/../../lib"],
                ),
            ],
            &[],
        );
        let p = plan(&t, &[], &homebrew);
        assert_eq!(p.errors, [], "{p:?}");
        assert_eq!(
            p.replaced,
            [Link {
                link: framework_mvk.clone().into(),
                target: "../../../../../../vulkan/libMoltenVK.dylib".into(),
            }]
        );
        assert!(
            p.copies.is_empty() && p.links.is_empty() && p.edits.is_empty(),
            "{p:?}"
        );

        // Once replaced, the link is a file of the tree and no image.
        t.images.remove(1);
        assert!(plan(&t, &[], &homebrew).is_empty());

        // A stale copy of libintl beside the framework's (the tree before
        // the one-file rule) is repaired the same way.
        let t = tree(
            vec![
                image(
                    "wine/lib/libintl.8.dylib",
                    Some("@rpath/libintl.8.dylib"),
                    &[],
                    &[],
                ),
                image(
                    &format!("{GST_LIB}/libintl.8.dylib"),
                    Some("@rpath/libintl.8.dylib"),
                    &[],
                    &["@loader_path/../lib"],
                ),
            ],
            &[],
        );
        let p = plan(&t, &[], &homebrew);
        assert_eq!(p.errors, [], "{p:?}");
        assert_eq!(
            p.replaced,
            [Link {
                link: "wine/lib/libintl.8.dylib".into(),
                target: "GStreamer.framework/Versions/1.0/lib/libintl.8.dylib".into(),
            }]
        );

        let t = tree(
            vec![
                image(
                    "wine/lib/libgmp.10.dylib",
                    Some("@rpath/libgmp.10.dylib"),
                    &[],
                    &[],
                ),
                image(
                    "wine/lib/x/libgmp.10.dylib",
                    Some("@rpath/libgmp.10.dylib"),
                    &[],
                    &[],
                ),
            ],
            &[],
        );
        let p = plan(&t, &[], &homebrew);
        assert_eq!(p.errors.len(), 1, "{p:?}");
        assert_eq!(
            p.errors[0].binary,
            "wine/lib/libgmp.10.dylib, wine/lib/x/libgmp.10.dylib"
        );
        assert_eq!(p.errors[0].reference, "@rpath/libgmp.10.dylib");
        assert!(
            p.errors[0]
                .reason
                .contains("one image per install name per process"),
            "{}",
            p.errors[0]
        );
        assert!(p.is_empty(), "{p:?}");
    }

    /// A SONAME without a policy row that the payload already ships somewhere
    /// is not copied again: the policy has to name that file.
    #[test]
    fn a_soname_the_payload_ships_elsewhere_is_not_copied_again() {
        let t = tree(vec![], &[&format!("{GST_LIB}/libgnutls.30.dylib")]);
        let p = plan(&t, &sonames(&["libgnutls.30.dylib"]), &homebrew);
        assert_eq!(p.copies, []);
        assert_eq!(p.errors.len(), 1, "{p:?}");
        assert_eq!(p.errors[0].binary, super::CONFIG_H);
        assert_eq!(p.errors[0].reference, "libgnutls.30.dylib");
        assert!(
            p.errors[0]
                .reason
                .starts_with(&format!("{GST_LIB} already ships it")),
            "{}",
            p.errors[0]
        );
    }

    #[test]
    fn unresolvable_references_fail_with_the_file_and_reference_named() {
        let t = tree(
            vec![image(
                &format!("{MODULES}/bad.so"),
                Some("@rpath/bad.so"),
                &[
                    "/opt/homebrew/lib/libarm.dylib",
                    "/usr/local/lib/libmissing.dylib",
                    "@rpath/libnowhere.dylib",
                    "/System/Volumes/Data/usr/local/lib/libx.dylib",
                    "libbare.dylib",
                    "/usr/local/opt/libpng/lib/libpng16.16.dylib",
                ],
                &["@loader_path/"],
            )],
            &[],
        );
        let p = plan(&t, &sonames(&["libnew.dylib", "libodbc.dylib"]), &homebrew);
        let described: Vec<(&str, &str)> = p
            .errors
            .iter()
            .map(|error| (error.binary.as_str(), error.reference.as_str()))
            .collect();
        assert_eq!(
            described,
            [
                (super::CONFIG_H, "libnew.dylib"),
                (
                    "wine/lib/wine/x86_64-unix/bad.so",
                    "/opt/homebrew/lib/libarm.dylib"
                ),
                (
                    "wine/lib/wine/x86_64-unix/bad.so",
                    "/usr/local/lib/libmissing.dylib"
                ),
                (
                    "wine/lib/wine/x86_64-unix/bad.so",
                    "/System/Volumes/Data/usr/local/lib/libx.dylib"
                ),
                ("wine/lib/wine/x86_64-unix/bad.so", "libbare.dylib"),
                (
                    "wine/lib/wine/x86_64-unix/bad.so",
                    "@rpath/libnowhere.dylib"
                ),
            ],
            "{p:?}"
        );
        assert!(
            p.errors[0].reason.contains("no row in the SONAME policy"),
            "{}",
            p.errors[0]
        );
        assert!(
            p.errors[1].reason.contains("/opt/homebrew"),
            "{}",
            p.errors[1]
        );
        assert!(
            p.notes.iter().any(|note| note.starts_with("libodbc.dylib")),
            "{:?}",
            p.notes
        );
        // The plan keeps going past the errors (the resolvable libpng is
        // planned); `settle` is what withholds the round.
        assert_eq!(p.copies.len(), 1, "{p:?}");
    }

    #[test]
    fn a_policy_payload_file_that_is_absent_is_an_error() {
        let t = tree(vec![], &[]);
        let p = plan(&t, &sonames(&["libMoltenVK.dylib"]), &homebrew);
        assert_eq!(p.errors.len(), 1);
        assert_eq!(p.errors[0].reference, "libMoltenVK.dylib");
        assert!(
            p.errors[0].reason.contains("vulkan/libMoltenVK.dylib"),
            "{}",
            p.errors[0]
        );
        assert_eq!(p.links, []);
    }

    #[test]
    fn executables_and_framework_libraries_get_no_dlopen_run_path() {
        let mut loader = naming(
            image(
                &format!("{MODULES}/wine"),
                None,
                &["/usr/lib/libSystem.B.dylib"],
                &[],
            ),
            &["libfreetype.6.dylib"],
        );
        loader.executable = true;
        let framework = naming(
            image(
                &format!("{GST_LIB}/libpango-1.0.0.dylib"),
                Some("@rpath/libpango-1.0.0.dylib"),
                &["@rpath/libfreetype.6.dylib"],
                &["@loader_path/../lib"],
            ),
            &["libfreetype.6.dylib"],
        );
        let freetype = image(
            &format!("{GST_LIB}/libfreetype.6.dylib"),
            Some("@rpath/libfreetype.6.dylib"),
            &[],
            &["@loader_path/../lib"],
        );
        let t = linked(
            tree(vec![loader, framework, freetype], &[]),
            "wine/lib/libfreetype.6.dylib",
            &format!("{GST_LIB}/libfreetype.6.dylib"),
        );
        let p = plan(&t, &sonames(&["libfreetype.6.dylib"]), &homebrew);
        assert_eq!(p.errors, []);
        assert!(p.is_empty(), "{p:?}");
    }

    fn fixture_spec() -> BundleSpec {
        BundleSpec::parse(include_str!("../tests/fixtures/BUNDLE.toml")).expect("fixture")
    }

    fn denylist() -> Denylist {
        Denylist::parse(include_str!("../../build/denylist.toml")).expect("denylist")
    }

    fn copy_plan(source: &str, dest: &str) -> Plan {
        Plan {
            copies: vec![Copy {
                source: source.into(),
                dest: dest.into(),
            }],
            ..Plan::default()
        }
    }

    #[test]
    fn the_gate_refuses_a_copy_without_a_row_or_with_another_version() {
        let denylist = denylist();
        let spec = fixture_spec();
        let plan = copy_plan(
            "/usr/local/Cellar/libffi/3.4.6/lib/libffi.8.dylib",
            "wine/lib/libffi.8.dylib",
        );
        let error = gate_copies(&denylist, &spec, &plan).unwrap_err();
        assert!(
            error.contains("lack a matching [components] row"),
            "{error}"
        );
        let plan = copy_plan(
            "/usr/local/Cellar/gnutls/3.8.9/lib/libgnutls.30.dylib",
            "wine/lib/libgnutls.30.dylib",
        );
        let error = gate_copies(&denylist, &spec, &plan).unwrap_err();
        assert!(error.contains("lack a matching"), "{error}");
        let plan = copy_plan(
            "/usr/local/lib/libgnutls.30.dylib",
            "wine/lib/libgnutls.30.dylib",
        );
        let error = gate_copies(&denylist, &spec, &plan).unwrap_err();
        assert!(error.contains("formula and version are unknown"), "{error}");
        let plan = copy_plan(
            "/usr/local/Cellar/gnutls/3.8.10/lib/libgnutls.30.dylib",
            "wine/lib/libgnutls.30.dylib",
        );
        gate_copies(&denylist, &spec, &plan).expect("a recorded copy passes");
    }

    #[test]
    fn the_gate_refuses_a_denylisted_destination() {
        let denylist = denylist();
        let plan = copy_plan(
            "/usr/local/Cellar/x264/1/lib/libgstx264.dylib",
            "wine/lib/libgstx264.dylib",
        );
        let error = gate_copies(&denylist, &fixture_spec(), &plan).unwrap_err();
        assert!(error.contains("denylisted"), "{error}");
    }

    /// Records every tool invocation; `install_name_tool` overwrites its
    /// binary like the real one and fails on `failing`.
    fn stub_runner<'a>(
        calls: &'a mut Vec<String>,
        failing: &'a str,
    ) -> impl FnMut(&str, &[&OsStr]) -> Result<(), String> + 'a {
        move |program: &str, args: &[&OsStr]| {
            let line = format!(
                "{program} {}",
                args.iter()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            calls.push(line.clone());
            let target = args.last().expect("a target");
            if program == "install_name_tool" {
                if target.to_string_lossy().ends_with(failing) {
                    return Err(format!(
                        "{line} failed (exit status: 1): larger updated load commands do not fit"
                    ));
                }
                fs::write(target, b"rewritten").expect("rewrite");
            }
            Ok(())
        }
    }

    /// A tool failure on the second binary rolls the whole round back: the
    /// copy, its new directory, and the link are gone, the first binary and
    /// the replaced duplicate are back to their bytes, a pre-existing link
    /// is untouched, and the error names the binary and the tool's stderr.
    #[test]
    fn a_tool_failure_rolls_the_round_back() {
        let dir = scratch("rollback");
        let payload = dir.join("payload");
        fake_macho(&payload.join("wine/lib/a.so"), b"a");
        fake_macho(&payload.join("wine/lib/b.so"), b"b");
        fake_macho(&payload.join("vulkan/libMoltenVK.dylib"), b"pin");
        fake_macho(&payload.join("fw/libMoltenVK.dylib"), b"dup");
        symlink("libMoltenVK.dylib", payload.join("fw/alias.dylib")).expect("alias");
        fake_macho(&dir.join("src/libA.1.dylib"), b"A");
        let before = |name: &str| fs::read(payload.join(name)).expect(name);
        let plan = Plan {
            copies: vec![Copy {
                source: dir.join("src/libA.1.dylib"),
                dest: "wine/lib/deps/libA.1.dylib".into(),
            }],
            links: vec![Link {
                link: "wine/lib/libMoltenVK.dylib".into(),
                target: "../../vulkan/libMoltenVK.dylib".into(),
            }],
            replaced: vec![Link {
                link: "fw/libMoltenVK.dylib".into(),
                target: "../vulkan/libMoltenVK.dylib".into(),
            }],
            edits: [
                ("wine/lib/a.so", "@loader_path"),
                ("wine/lib/b.so", "@loader_path/.."),
            ]
            .into_iter()
            .map(|(binary, rpath)| {
                (
                    binary.to_owned(),
                    Edit {
                        add_rpaths: vec![rpath.to_owned()],
                        ..Edit::default()
                    },
                )
            })
            .collect(),
            ..Plan::default()
        };
        let mut calls = Vec::new();
        let error = apply(&payload, &plan, &mut stub_runner(&mut calls, "b.so")).unwrap_err();
        // The directory, the copy, and the link are removed; the replaced
        // file, a.so, and b.so (saved before its tool failed) are put back.
        assert!(
            error.contains("wine/lib/b.so")
                && error.contains("do not fit")
                && error.contains(
                    "rolled back (3 created path(s) removed, 3 pre-existing file(s) restored)"
                ),
            "{error}"
        );
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert!(calls[0].ends_with("a.so") && calls[1].starts_with("codesign"));
        assert!(fs::symlink_metadata(payload.join("wine/lib/deps")).is_err());
        assert!(fs::symlink_metadata(payload.join("wine/lib/libMoltenVK.dylib")).is_err());
        assert_eq!(
            fs::read_link(payload.join("fw/alias.dylib")).expect("alias"),
            PathBuf::from("libMoltenVK.dylib")
        );
        assert_eq!(before("wine/lib/a.so"), [&X86_64_MAGIC[..], b"a"].concat());
        assert_eq!(before("wine/lib/b.so"), [&X86_64_MAGIC[..], b"b"].concat());
        let dup = fs::symlink_metadata(payload.join("fw/libMoltenVK.dylib")).expect("dup");
        assert!(dup.file_type().is_file());
        assert_eq!(
            before("fw/libMoltenVK.dylib"),
            [&X86_64_MAGIC[..], b"dup"].concat()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    type Scan = Box<dyn Fn(&Path) -> Result<Tree, String>>;

    /// A scratch payload with `ok.so` naming a copyable `libpng` and, when
    /// `with_bad`, `bad.so` naming a leaf nowhere; `scan` hands the planner
    /// that shape as long as `libpng` is not in `wine/lib/`.
    fn settle_fixture(name: &str, with_bad: bool) -> (PathBuf, PathBuf, Scan) {
        let dir = scratch(name);
        let payload = dir.join("payload");
        fake_macho(&payload.join(format!("{MODULES}/ok.so")), b"ok");
        fake_macho(&dir.join("src/libpng16.16.dylib"), b"png");
        if with_bad {
            fake_macho(&payload.join(format!("{MODULES}/bad.so")), b"bad");
        }
        let scan = move |payload: &Path| {
            let copied = payload.join("wine/lib/libpng16.16.dylib").exists();
            let mut images = vec![image(
                &format!("{MODULES}/ok.so"),
                Some("@rpath/ok.so"),
                &[if copied {
                    "@rpath/libpng16.16.dylib"
                } else {
                    "/usr/local/opt/libpng/lib/libpng16.16.dylib"
                }],
                if copied {
                    &["@loader_path/", "@loader_path/../.."]
                } else {
                    &["@loader_path/"]
                },
            )];
            if with_bad {
                images.push(image(
                    &format!("{MODULES}/bad.so"),
                    Some("@rpath/bad.so"),
                    &["@rpath/libnowhere.dylib"],
                    &["@loader_path/"],
                ));
            }
            let extra = if copied {
                vec!["wine/lib/libpng16.16.dylib"]
            } else {
                vec![]
            };
            Ok(tree(images, &extra))
        };
        (dir.clone(), payload, Box::new(scan))
    }

    /// A round with an unresolvable reference applies nothing, asserted on
    /// the tree: the resolvable copy beside it never lands and no tool runs.
    #[test]
    fn a_round_with_an_unresolvable_reference_leaves_the_tree_untouched() {
        let (dir, payload, scan) = settle_fixture("withhold", true);
        let src = dir.join("src/libpng16.16.dylib");
        let source = |reference: &str| -> Result<PathBuf, String> {
            if reference.ends_with("libpng16.16.dylib") {
                Ok(src.clone())
            } else {
                Err("No such file or directory (os error 2)".into())
            }
        };
        let mut calls = Vec::new();
        let error = settle(
            &payload,
            &[],
            &source,
            scan.as_ref(),
            &|_| Ok(()),
            &mut stub_runner(&mut calls, "<nothing fails>"),
        )
        .unwrap_err();
        assert!(
            error.contains("1 reference(s) cannot be closed (round 1)"),
            "{error}"
        );
        assert!(calls.is_empty(), "{calls:?}");
        assert!(!payload.join("wine/lib/libpng16.16.dylib").exists());
        let mut entries: Vec<String> = fs::read_dir(payload.join("wine/lib"))
            .expect("wine/lib")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        entries.sort();
        assert_eq!(entries, ["wine"]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Without the bad module the same tree settles in one round: the copy
    /// lands, the module is rewritten through the tools, and the next scan
    /// plans nothing.
    #[test]
    fn a_resolvable_round_lands_and_the_next_scan_settles() {
        let (dir, payload, scan) = settle_fixture("settle", false);
        let src = dir.join("src/libpng16.16.dylib");
        let source = |_: &str| -> Result<PathBuf, String> { Ok(src.clone()) };
        let mut calls = Vec::new();
        let outcome = settle(
            &payload,
            &[],
            &source,
            scan.as_ref(),
            &|_| Ok(()),
            &mut stub_runner(&mut calls, "<nothing fails>"),
        )
        .expect("settles");
        assert_eq!(
            outcome,
            Outcome {
                rounds: 1,
                copied: 1,
                linked: 0,
                rewritten: [format!("{MODULES}/ok.so")].into_iter().collect(),
                formulas: BTreeSet::new(),
            }
        );
        assert!(payload.join("wine/lib/libpng16.16.dylib").is_file());
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(
            calls[0].starts_with("install_name_tool -change /usr/local/opt/libpng/lib/libpng16.16.dylib @rpath/libpng16.16.dylib -add_rpath @loader_path/../.. "),
            "{}",
            calls[0]
        );
        assert!(
            calls[1].starts_with(
                "codesign --force --sign - --preserve-metadata=entitlements,flags,runtime "
            ),
            "{}",
            calls[1]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The framework's `MoltenVK` carries a foreign run path: the round
    /// replaces the file with a link and plans no edit for it, since the
    /// tool would follow the new link onto the pin. (Both round-two
    /// reviewers' probe.)
    #[test]
    fn a_replaced_duplicate_gets_no_edit_in_the_same_round() {
        let framework_mvk = format!("{GST_LIB}/libMoltenVK.dylib");
        let t = tree(
            vec![
                image(
                    "vulkan/libMoltenVK.dylib",
                    Some("@rpath/libMoltenVK.dylib"),
                    &["/usr/lib/libc++.1.dylib"],
                    &[],
                ),
                image(
                    &framework_mvk,
                    Some("@rpath/libMoltenVK.dylib"),
                    &["/usr/lib/libc++.1.dylib"],
                    &["/usr/local/lib", "@loader_path/../lib"],
                ),
            ],
            &[],
        );
        let p = plan(&t, &[], &homebrew);
        assert_eq!(p.errors, [], "{p:?}");
        assert_eq!(p.replaced.len(), 1, "{p:?}");
        assert!(p.edits.is_empty(), "{p:?}");
    }

    /// Opens the path it is given, as the real tools do (following a link),
    /// and appends to it.
    fn appending_runner(
        calls: &mut Vec<String>,
    ) -> impl FnMut(&str, &[&OsStr]) -> Result<(), String> + '_ {
        move |program: &str, args: &[&OsStr]| {
            calls.push(program.to_owned());
            let target = Path::new(args.last().expect("a target"));
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(target)
                .map_err(|error| error.to_string())?;
            file.write_all(program.as_bytes())
                .map_err(|error| error.to_string())
        }
    }

    /// A plan that replaces a file and edits it too, or edits a path that
    /// is already a link, is refused at `apply` naming the link and its
    /// target; no tool runs, and the pin behind the link keeps its bytes.
    #[test]
    fn apply_refuses_to_run_a_tool_on_a_link() {
        let dir = scratch("link-edit");
        let payload = dir.join("payload");
        fake_macho(&payload.join("vulkan/libMoltenVK.dylib"), b"pin");
        fake_macho(&payload.join("fw/libMoltenVK.dylib"), b"dup");
        fs::create_dir_all(payload.join("wine/lib")).expect("wine/lib");
        symlink(
            "../../vulkan/libMoltenVK.dylib",
            payload.join("wine/lib/libMoltenVK.dylib"),
        )
        .expect("link");
        let pin = || fs::read(payload.join("vulkan/libMoltenVK.dylib")).expect("pin");
        let before = pin();
        let edit = |binary: &str| {
            [(
                binary.to_owned(),
                Edit {
                    delete_rpaths: vec!["/usr/local/lib".into()],
                    ..Edit::default()
                },
            )]
            .into_iter()
            .collect()
        };
        let plan = Plan {
            replaced: vec![Link {
                link: "fw/libMoltenVK.dylib".into(),
                target: "../vulkan/libMoltenVK.dylib".into(),
            }],
            edits: edit("fw/libMoltenVK.dylib"),
            ..Plan::default()
        };
        let mut calls = Vec::new();
        let error = apply(&payload, &plan, &mut appending_runner(&mut calls)).unwrap_err();
        assert!(
            error.contains("fw/libMoltenVK.dylib: is a link to ../vulkan/libMoltenVK.dylib")
                && error.contains(
                    "rolled back (0 created path(s) removed, 1 pre-existing file(s) restored)"
                ),
            "{error}"
        );
        assert!(calls.is_empty(), "{calls:?}");
        assert_eq!(pin(), before);
        let dup = fs::symlink_metadata(payload.join("fw/libMoltenVK.dylib")).expect("dup");
        assert!(dup.file_type().is_file());
        assert_eq!(
            fs::read(payload.join("fw/libMoltenVK.dylib")).expect("dup"),
            [&X86_64_MAGIC[..], b"dup"].concat()
        );

        let plan = Plan {
            edits: edit("wine/lib/libMoltenVK.dylib"),
            ..Plan::default()
        };
        let error = apply(&payload, &plan, &mut appending_runner(&mut calls)).unwrap_err();
        assert!(
            error.contains(
                "wine/lib/libMoltenVK.dylib: is a link to ../../vulkan/libMoltenVK.dylib"
            ),
            "{error}"
        );
        assert!(calls.is_empty(), "{calls:?}");
        assert_eq!(pin(), before);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A destination occupied by a link that leaves the payload, or by a
    /// dangling one, is an error naming the link and its target, not an
    /// empty slot; a link that resolves to a payload file is that file; a
    /// link among the parent directories is caught too.
    #[test]
    fn a_destination_occupied_by_a_link_that_leaves_the_payload_is_an_error() {
        let mut t = win32u_tree();
        t.occupied.insert(
            "wine/lib/libgnutls.30.dylib".into(),
            "a link to /outside/libgnutls.30.dylib that resolves outside the payload (/outside/libgnutls.30.dylib)".into(),
        );
        t.occupied.insert(
            "wine/lib/libMoltenVK.dylib".into(),
            "a dangling link to /nonexistent/libMoltenVK.dylib".into(),
        );
        let p = plan(&t, &sonames(&SONAMES), &homebrew);
        let described: Vec<(&str, &str)> = p
            .errors
            .iter()
            .map(|error| (error.binary.as_str(), error.reference.as_str()))
            .collect();
        assert_eq!(
            described,
            [
                (super::CONFIG_H, "libMoltenVK.dylib"),
                (super::CONFIG_H, "libgnutls.30.dylib"),
            ],
            "{p:?}"
        );
        assert!(
            p.errors[0].reason.starts_with(
                "wine/lib/libMoltenVK.dylib is a dangling link to /nonexistent/libMoltenVK.dylib"
            ),
            "{}",
            p.errors[0]
        );
        assert!(
            p.errors[1].reason.starts_with(
                "wine/lib/libgnutls.30.dylib is a link to /outside/libgnutls.30.dylib that resolves outside the payload"
            ),
            "{}",
            p.errors[1]
        );
        assert_eq!(p.copies, []);
        assert_eq!(p.links.len(), 1, "{p:?}");
        assert_eq!(
            p.links[0].link,
            PathBuf::from("wine/lib/libfreetype.6.dylib")
        );

        let mut t = win32u_tree();
        t.files.insert("wine/lib/libMoltenVK.dylib".into());
        t.links.insert(
            "wine/lib/libMoltenVK.dylib".into(),
            "vulkan/libMoltenVK.dylib".into(),
        );
        let p = plan(&t, &sonames(&SONAMES), &homebrew);
        assert_eq!(p.errors, [], "{p:?}");
        assert_eq!(p.links.len(), 1, "{p:?}");

        let mut t = tree(vec![], &[]);
        t.occupied.insert(
            "wine/lib".into(),
            "a link to ../../elsewhere that resolves outside the payload (/elsewhere)".into(),
        );
        let p = plan(&t, &sonames(&["libgnutls.30.dylib"]), &homebrew);
        assert_eq!(p.errors.len(), 1, "{p:?}");
        assert!(
            p.errors[0]
                .reason
                .starts_with("wine/lib is a link to ../../elsewhere"),
            "{}",
            p.errors[0]
        );
        assert_eq!(p.copies, []);
    }

    /// `scan` tells links that resolve to a payload file (the file, with
    /// its target recorded) from links that are dangling, escaping, or a
    /// directory (occupied slots).
    #[test]
    fn scan_tells_resolving_links_from_occupied_slots() {
        let dir = scratch("scan");
        let payload = dir.join("payload");
        fs::create_dir_all(payload.join("wine/lib")).expect("wine/lib");
        fs::write(payload.join("wine/lib/real.txt"), b"text").expect("real");
        fs::write(dir.join("outside.txt"), b"outside").expect("outside");
        let link = |target: &str, name: &str| symlink(target, payload.join(name)).expect(name);
        link("real.txt", "wine/lib/alias.txt");
        link("../../../outside.txt", "wine/lib/escape.txt");
        link("missing.txt", "wine/lib/dangling.txt");
        link(".", "wine/lib/here");
        let payload = payload.canonicalize().expect("canonical");
        let t = scan(&payload, &[]).expect("scan");
        assert_eq!(
            t.files,
            ["wine/lib/alias.txt", "wine/lib/real.txt"]
                .map(PathBuf::from)
                .into_iter()
                .collect()
        );
        assert_eq!(
            t.links,
            [(
                PathBuf::from("wine/lib/alias.txt"),
                PathBuf::from("wine/lib/real.txt")
            )]
            .into_iter()
            .collect()
        );
        assert_eq!(
            t.occupied.keys().cloned().collect::<Vec<_>>(),
            [
                "wine/lib/dangling.txt",
                "wine/lib/escape.txt",
                "wine/lib/here"
            ]
            .map(PathBuf::from)
        );
        assert_eq!(
            t.occupied[Path::new("wine/lib/dangling.txt")],
            "a dangling link to missing.txt"
        );
        assert!(
            t.occupied[Path::new("wine/lib/escape.txt")]
                .starts_with("a link to ../../../outside.txt that resolves outside the payload ("),
            "{:?}",
            t.occupied
        );
        assert_eq!(
            t.occupied[Path::new("wine/lib/here")],
            "a link to ., a directory"
        );
        assert_eq!(t.images, []);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A write through a link that leaves the payload never happens: the
    /// copy whose destination is such a link, the copy below a linked
    /// directory, and the link onto a dangling link each fail naming the
    /// link and its target; nothing lands outside, and the pre-existing
    /// links survive as they were. (The round-two closure prover's
    /// clone-out and clone-dl cases.)
    #[test]
    fn apply_writes_nothing_through_a_link_that_leaves_the_payload() {
        let dir = scratch("escape");
        let payload = dir.join("payload");
        let outside = dir.join("outside");
        fs::create_dir_all(&outside).expect("outside");
        fs::create_dir_all(payload.join("wine/lib")).expect("wine/lib");
        fake_macho(&dir.join("src/libSDL2-2.0.0.dylib"), b"sdl");
        let sdl_target = outside.join("libSDL2-2.0.0.dylib");
        symlink(&sdl_target, payload.join("wine/lib/libSDL2-2.0.0.dylib")).expect("escaping link");
        let copy = |dest: &str| Plan {
            copies: vec![Copy {
                source: dir.join("src/libSDL2-2.0.0.dylib"),
                dest: dest.into(),
            }],
            ..Plan::default()
        };
        let mut calls = Vec::new();
        let mut run = stub_runner(&mut calls, "<nothing fails>");
        let error = apply(&payload, &copy("wine/lib/libSDL2-2.0.0.dylib"), &mut run).unwrap_err();
        assert!(
            error.contains("wine/lib/libSDL2-2.0.0.dylib is a dangling link to")
                && error.contains(&sdl_target.display().to_string())
                && error.contains("0 created path(s) removed, 0 pre-existing file(s) restored"),
            "{error}"
        );
        assert!(!sdl_target.exists(), "written outside the payload");
        assert_eq!(
            fs::read_link(payload.join("wine/lib/libSDL2-2.0.0.dylib")).expect("link survives"),
            sdl_target
        );

        fs::write(&sdl_target, b"theirs").expect("outside file");
        let error = apply(&payload, &copy("wine/lib/libSDL2-2.0.0.dylib"), &mut run).unwrap_err();
        assert!(
            error.contains("wine/lib/libSDL2-2.0.0.dylib is a link to")
                && error.contains("resolves outside the payload"),
            "{error}"
        );
        assert_eq!(fs::read(&sdl_target).expect("outside"), b"theirs");

        let elsewhere = dir.join("elsewhere");
        fs::create_dir_all(&elsewhere).expect("elsewhere");
        symlink(&elsewhere, payload.join("wine/ext")).expect("dir link");
        let error = apply(&payload, &copy("wine/ext/libSDL2-2.0.0.dylib"), &mut run).unwrap_err();
        assert!(
            error.contains("wine/ext is a link to")
                && error.contains("resolves outside the payload"),
            "{error}"
        );
        assert_eq!(fs::read_dir(&elsewhere).expect("elsewhere").count(), 0);

        symlink(
            "/nonexistent/libMoltenVK.dylib",
            payload.join("wine/lib/libMoltenVK.dylib"),
        )
        .expect("dangling");
        let plan = Plan {
            links: vec![Link {
                link: "wine/lib/libMoltenVK.dylib".into(),
                target: "../../vulkan/libMoltenVK.dylib".into(),
            }],
            ..Plan::default()
        };
        let error = apply(&payload, &plan, &mut run).unwrap_err();
        assert!(
            error.contains(
                "wine/lib/libMoltenVK.dylib is a dangling link to /nonexistent/libMoltenVK.dylib"
            ),
            "{error}"
        );
        assert_eq!(
            fs::read_link(payload.join("wine/lib/libMoltenVK.dylib")).expect("link survives"),
            PathBuf::from("/nonexistent/libMoltenVK.dylib")
        );
        drop(run);
        assert!(calls.is_empty(), "{calls:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The journal saves a link as its target and restores it as a link,
    /// counts only pre-existing paths as restored, and never records an
    /// existing path as created.
    #[test]
    fn the_journal_restores_a_saved_link_as_a_link() {
        let dir = scratch("journal");
        fake_macho(&dir.join("real.dylib"), b"real");
        let link = dir.join("link.dylib");
        symlink("real.dylib", &link).expect("link");
        let created = dir.join("new.dylib");
        let mut journal = Journal::new().expect("journal");
        journal.save(&link).expect("save");
        journal.created(&created).expect("created");
        fs::write(&created, b"new").expect("new");
        fs::remove_file(&link).expect("remove link");
        fs::write(&link, b"clobbered").expect("clobber");
        assert_eq!(journal.restore().expect("restore"), (1, 1));
        assert_eq!(
            fs::read_link(&link).expect("a link again"),
            PathBuf::from("real.dylib")
        );
        assert!(!created.exists());
        let mut journal = Journal::new().expect("journal");
        let error = journal.created(&link).unwrap_err();
        assert!(
            error.contains("already exists (a link to real.dylib)"),
            "{error}"
        );
        journal.discard();
        let _ = fs::remove_dir_all(&dir);
    }

    /// An absolute dependency on a leaf another component ships binds to it
    /// only through a policy row naming that file: a bare match of names is
    /// refused, and nothing is copied either.
    #[test]
    fn a_rebinding_to_a_file_the_payload_ships_needs_a_policy_row() {
        let gnutls = "wine/lib/libgnutls.30.dylib";
        let t = tree(
            vec![
                image(
                    gnutls,
                    Some("@rpath/libgnutls.30.dylib"),
                    &["/usr/local/opt/libtasn1/lib/libtasn1.6.dylib"],
                    &[],
                ),
                image(
                    &format!("{GST_LIB}/libtasn1.6.dylib"),
                    Some("@rpath/libtasn1.6.dylib"),
                    &[],
                    &["@loader_path/../lib"],
                ),
            ],
            &[],
        );
        let p = plan(&t, &[], &homebrew);
        assert_eq!(p.errors.len(), 1, "{p:?}");
        assert_eq!(
            (p.errors[0].binary.as_str(), p.errors[0].reference.as_str()),
            (gnutls, "/usr/local/opt/libtasn1/lib/libtasn1.6.dylib")
        );
        assert!(
            p.errors[0].reason.starts_with(&format!(
                "{GST_LIB}/libtasn1.6.dylib already ships libtasn1.6.dylib; a rebinding"
            )),
            "{}",
            p.errors[0]
        );
        assert!(p.is_empty(), "{p:?}");
    }

    /// A rebinding through a policy row binds only if the file's current
    /// version satisfies the dependent's compatibility version, unless the
    /// row allows an older file with a reason, which the round notes.
    #[test]
    fn a_rebinding_binds_only_to_a_new_enough_file_unless_the_row_allows_older() {
        let gnutls = "wine/lib/libgnutls.30.dylib";
        let framework_intl = format!("{GST_LIB}/libintl.8.dylib");
        let libx = "wine/lib/libx.dylib";
        let freetype = "/usr/local/opt/freetype/lib/libfreetype.6.dylib";
        let t = tree(
            vec![
                versioned(
                    image(libx, Some("@rpath/libx.dylib"), &[freetype], &[]),
                    "1.0.0",
                    &[(freetype, "27.0.0")],
                ),
                versioned(
                    image(
                        &format!("{GST_LIB}/libfreetype.6.dylib"),
                        Some("@rpath/libfreetype.6.dylib"),
                        &[],
                        &["@loader_path/../lib"],
                    ),
                    "26.2.20",
                    &[],
                ),
            ],
            &[],
        );
        let p = plan(&t, &[], &homebrew);
        assert_eq!(p.errors.len(), 1, "{p:?}");
        assert!(
            p.errors[0]
                .reason
                .contains("requires compatibility version 27.0.0 but")
                && p.errors[0]
                    .reason
                    .contains("libfreetype.6.dylib is current version 26.2.20"),
            "{}",
            p.errors[0]
        );
        assert!(p.is_empty(), "{p:?}");

        let intl = "/usr/local/opt/gettext/lib/libintl.8.dylib";
        let intl_image = |current: &str| {
            versioned(
                image(
                    &framework_intl,
                    Some("@rpath/libintl.8.dylib"),
                    &[],
                    &["@loader_path/../lib"],
                ),
                current,
                &[],
            )
        };
        let gnutls_image = |compat: &str| {
            versioned(
                image(gnutls, Some("@rpath/libgnutls.30.dylib"), &[intl], &[]),
                "68.2.0",
                &[(intl, compat)],
            )
        };
        let t = tree(vec![gnutls_image("13.0.0"), intl_image("10.5.0")], &[]);
        let p = plan(&t, &[], &homebrew);
        assert_eq!(p.errors, [], "{p:?}");
        assert_eq!(
            edit(&p, gnutls),
            &Edit {
                changes: vec![(intl.into(), "@rpath/libintl.8.dylib".into())],
                add_rpaths: vec!["@loader_path/GStreamer.framework/Versions/1.0/lib".into()],
                ..Edit::default()
            }
        );
        assert_eq!(p.notes.len(), 1, "{:?}", p.notes);
        assert!(
            p.notes[0].contains("requires compatibility version 13.0.0")
                && p.notes[0].contains("current version 10.5.0; allowed by its SONAME_POLICY row"),
            "{}",
            p.notes[0]
        );

        let t = tree(vec![gnutls_image("10.0.0"), intl_image("10.5.0")], &[]);
        let p = plan(&t, &[], &homebrew);
        assert_eq!(p.errors, [], "{p:?}");
        assert_eq!(p.notes, Vec::<String>::new());
    }

    /// A link under `wine/lib/` into another component's tree is that file,
    /// and the one rebinding rule applies through it: without a policy row
    /// naming the file nothing binds, and with one the row's allowance
    /// counts. The two shapes are the closure prover's `clone-row-link`
    /// (a probe built against the framework's glib, the link hand-placed)
    /// and `clone-intl-link` (the libintl link the row itself makes when it
    /// repairs a stale copy, then a fresh gnutls).
    #[test]
    fn a_link_under_wine_lib_to_another_components_file_follows_the_one_rebinding_rule() {
        let probe = "wine/lib/libprobe.dylib";
        let glib = "/usr/local/opt/glib/lib/libglib-2.0.0.dylib";
        let framework_glib = format!("{GST_LIB}/libglib-2.0.0.dylib");
        let t = linked(
            tree(
                vec![
                    versioned(
                        image(probe, Some("@rpath/libprobe.dylib"), &[glib], &[]),
                        "1.0.0",
                        &[(glib, "8201.0.0")],
                    ),
                    versioned(
                        image(
                            &framework_glib,
                            Some("@rpath/libglib-2.0.0.dylib"),
                            &[],
                            &["@loader_path/../lib"],
                        ),
                        "8201.4.0",
                        &[],
                    ),
                ],
                &[],
            ),
            "wine/lib/libglib-2.0.0.dylib",
            &framework_glib,
        );
        let p = plan(&t, &[], &homebrew);
        assert_eq!(p.errors.len(), 1, "{p:?}");
        assert_eq!(
            (p.errors[0].binary.as_str(), p.errors[0].reference.as_str()),
            (probe, glib)
        );
        assert!(
            p.errors[0].reason.starts_with(&format!(
                "{framework_glib} already ships libglib-2.0.0.dylib; a rebinding to a file the payload ships needs a SONAME_POLICY row"
            )),
            "{}",
            p.errors[0]
        );
        assert!(p.is_empty(), "{p:?}");

        let gnutls = "wine/lib/libgnutls.30.dylib";
        let intl = "/usr/local/opt/gettext/lib/libintl.8.dylib";
        let framework_intl = format!("{GST_LIB}/libintl.8.dylib");
        let t = linked(
            tree(
                vec![
                    versioned(
                        image(gnutls, Some("@rpath/libgnutls.30.dylib"), &[intl], &[]),
                        "68.2.0",
                        &[(intl, "13.0.0")],
                    ),
                    versioned(
                        image(
                            &framework_intl,
                            Some("@rpath/libintl.8.dylib"),
                            &[],
                            &["@loader_path/../lib"],
                        ),
                        "10.5.0",
                        &[],
                    ),
                ],
                &[],
            ),
            "wine/lib/libintl.8.dylib",
            &framework_intl,
        );
        let p = plan(&t, &[], &homebrew);
        assert_eq!(p.errors, [], "{p:?}");
        assert_eq!(
            edit(&p, gnutls),
            &Edit {
                changes: vec![(intl.into(), "@rpath/libintl.8.dylib".into())],
                add_rpaths: vec!["@loader_path".into()],
                ..Edit::default()
            }
        );
        assert_eq!(p.notes.len(), 1, "{:?}", p.notes);
        assert!(
            p.notes[0].contains("requires compatibility version 13.0.0")
                && p.notes[0].contains(&format!(
                    "{framework_intl} is current version 10.5.0; allowed by its SONAME_POLICY row"
                )),
            "{}",
            p.notes[0]
        );
    }

    /// A SONAME slot holds a dylib or nothing: a link to a payload file that
    /// is no dylib (the prover's `clone-in`, `MoltenVK_icd.json`) is refused
    /// naming the target, and so is a regular file that is no Mach-O; a
    /// dylib behind a link supplies the SONAME.
    #[test]
    fn a_soname_slot_must_resolve_to_a_dylib() {
        let soname = "libSDL2-2.0.0.dylib";
        let slot = "wine/lib/libSDL2-2.0.0.dylib";
        let t = linked(
            tree(vec![], &["vulkan/MoltenVK_icd.json"]),
            slot,
            "vulkan/MoltenVK_icd.json",
        );
        let p = plan(&t, &sonames(&[soname]), &homebrew);
        assert_eq!(p.errors.len(), 1, "{p:?}");
        assert_eq!(
            (p.errors[0].reference.as_str(), p.errors[0].reason.as_str()),
            (
                soname,
                "wine/lib/libSDL2-2.0.0.dylib is a link to vulkan/MoltenVK_icd.json, which is not a dylib (no LC_ID_DYLIB)"
            )
        );
        assert!(p.is_empty(), "{p:?}");

        let t = tree(vec![], &[slot]);
        let p = plan(&t, &sonames(&[soname]), &homebrew);
        assert_eq!(p.errors.len(), 1, "{p:?}");
        assert_eq!(
            p.errors[0].reason,
            "wine/lib/libSDL2-2.0.0.dylib is not a dylib (no LC_ID_DYLIB)"
        );
        assert!(p.is_empty(), "{p:?}");

        let winebus = format!("{MODULES}/winebus.so");
        let t = linked(
            tree(
                vec![
                    naming(
                        image(&winebus, Some("@rpath/winebus.so"), &[], &["@loader_path/"]),
                        &[soname],
                    ),
                    image(
                        "vulkan/libSDL2-2.0.0.dylib",
                        Some("@rpath/libSDL2-2.0.0.dylib"),
                        &[],
                        &[],
                    ),
                ],
                &[],
            ),
            slot,
            "vulkan/libSDL2-2.0.0.dylib",
        );
        let p = plan(&t, &sonames(&[soname]), &homebrew);
        assert_eq!(p.errors, [], "{p:?}");
        assert_eq!(
            edit(&p, &winebus),
            &Edit {
                add_rpaths: vec!["@loader_path/../..".into()],
                ..Edit::default()
            }
        );
    }

    /// `Some("")` type-checks as an allowance; the table carries none, and
    /// the planner treats an empty reason as no allowance.
    #[test]
    fn every_older_compat_allowance_carries_a_reason() {
        for (soname, policy) in SONAME_POLICY {
            if let SonamePolicy::Payload {
                allow_older_compat: Some(reason),
                ..
            } = policy
            {
                assert!(
                    !reason.trim().is_empty(),
                    "{soname}: allow_older_compat needs a reason"
                );
            }
        }
        let intl = "/usr/local/opt/gettext/lib/libintl.8.dylib";
        let framework_intl = format!("{GST_LIB}/libintl.8.dylib");
        let gnutls = versioned(
            image(
                "wine/lib/libgnutls.30.dylib",
                Some("@rpath/libgnutls.30.dylib"),
                &[intl],
                &[],
            ),
            "68.2.0",
            &[(intl, "13.0.0")],
        );
        let t = tree(
            vec![
                gnutls.clone(),
                versioned(
                    image(
                        &framework_intl,
                        Some("@rpath/libintl.8.dylib"),
                        &[],
                        &["@loader_path/../lib"],
                    ),
                    "10.5.0",
                    &[],
                ),
            ],
            &[],
        );
        let mut round = Round {
            tree: &t,
            source: &homebrew,
            plan: Plan::default(),
            planned: BTreeSet::new(),
            supplied: BTreeSet::new(),
        };
        assert!(!round.compatible(&gnutls, intl, Path::new(&framework_intl), Some("  \n")));
        assert_eq!(round.plan.errors.len(), 1, "{:?}", round.plan);
        assert!(
            round.plan.errors[0].reason.contains(
                "only a SONAME_POLICY row with allow_older_compat and a reason binds a dependent to an older file"
            ),
            "{}",
            round.plan.errors[0]
        );
        assert_eq!(round.plan.notes, Vec::<String>::new());
    }

    /// The same version check guards a leaf already under `wine/lib/`: a
    /// copy an earlier round made that is too old for a later dependent.
    #[test]
    fn a_leaf_already_under_wine_lib_must_be_new_enough_too() {
        let gmp = "/usr/local/opt/gmp/lib/libgmp.10.dylib";
        let t = tree(
            vec![
                versioned(
                    image(
                        "wine/lib/libhogweed.6.dylib",
                        Some("@rpath/libhogweed.6.dylib"),
                        &[gmp],
                        &[],
                    ),
                    "6.11.0",
                    &[(gmp, "15.0.0")],
                ),
                versioned(
                    image(
                        "wine/lib/libgmp.10.dylib",
                        Some("@rpath/libgmp.10.dylib"),
                        &[],
                        &[],
                    ),
                    "14.1.0",
                    &[],
                ),
            ],
            &[],
        );
        let p = plan(&t, &[], &homebrew);
        assert_eq!(p.errors.len(), 1, "{p:?}");
        assert!(
            p.errors[0].reason.contains(
                "requires compatibility version 15.0.0 but wine/lib/libgmp.10.dylib is current version 14.1.0"
            ),
            "{}",
            p.errors[0]
        );
    }

    /// Every policy file in another component's tree has the notices row
    /// that names its license, and every such row names a policy file.
    #[test]
    fn every_policy_file_in_another_components_tree_has_a_notices_row() {
        let spec =
            BundleSpec::parse(include_str!("../../build/bundle-spec.toml")).expect("the spec");
        let policy_files: BTreeSet<&str> = SONAME_POLICY
            .iter()
            .filter_map(|(_, policy)| match policy {
                SonamePolicy::Payload { file, .. } => Some(*file),
                _ => None,
            })
            .collect();
        for file in &policy_files {
            // The pinned MoltenVK is the `moltenvk` row.
            if file.starts_with("vulkan/") {
                continue;
            }
            let elsewhere = SHIPPED_ELSEWHERE
                .iter()
                .find(|elsewhere| elsewhere.file == *file)
                .unwrap_or_else(|| panic!("{file}: no SHIPPED_ELSEWHERE entry"));
            assert!(
                spec.components.libraries.contains_key(elsewhere.key),
                "{file}: no [components.{}] row",
                elsewhere.key
            );
        }
        for elsewhere in SHIPPED_ELSEWHERE {
            assert!(
                policy_files.contains(elsewhere.file),
                "{}: no SONAME_POLICY row",
                elsewhere.file
            );
        }
    }
}
