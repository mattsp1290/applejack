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
//! and libintl, the pinned `MoltenVK`) is linked or reached by a run path, never
//! copied again, and a duplicate the tree carries becomes a relative link to
//! the file the SONAME policy names, or is refused.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io::Read as _;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::bundle_toml::{BundleSpec, is_formula_name};
use crate::denylist::Denylist;
use crate::engine_payload::{BUNDLE_FILE, check, is_macho, load_denylist, otool, walk};
use crate::self_contained::{
    Violation, binary_dir, dependency_entries, id_entry, is_executable, normalize_relative,
    rpath_entries,
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
    /// A file the payload already ships at this bundle-relative path;
    /// `wine/lib/<soname>` becomes a relative link to it, and any other
    /// regular file carrying its install name becomes a link to it too.
    Payload(&'static str),
    /// A macOS library dyld finds in the shared cache on its own.
    System,
    /// Copied when the x86-64 Homebrew has it, skipped with a message
    /// otherwise: Wine degrades without it.
    Optional,
}

/// The SONAMEs that are not plain Homebrew copies. A new SONAME in
/// `config.h` that is neither here nor in the x86-64 Homebrew fails the
/// closure, which is the moment to classify it; so does a SONAME the payload
/// already ships somewhere without a row here.
pub const SONAME_POLICY: &[(&str, SonamePolicy)] = &[
    // Wine's SONAME_LIBVULKAN and SONAME_LIBMOLTENVK: the pinned MoltenVK
    // release (`components.moltenvk`), not Homebrew's molten-vk formula. The
    // framework ships a MoltenVK of its own (1.2.9, for its vulkan and
    // applemedia plugins) under the same install name; it becomes a link to
    // the pin, whose exports cover every symbol those plugins import.
    (
        "libMoltenVK.dylib",
        SonamePolicy::Payload("vulkan/libMoltenVK.dylib"),
    ),
    // The framework's freetype, which its fontconfig, harfbuzz, cairo, pango,
    // and libass link, rather than a second copy from Homebrew; it exports
    // every FT_* symbol win32u and dwrite look up.
    (
        "libfreetype.6.dylib",
        SonamePolicy::Payload("wine/lib/GStreamer.framework/Versions/1.0/lib/libfreetype.6.dylib"),
    ),
    // No file under /usr/lib since macOS 11; dyld serves it from the cache.
    ("libcups.2.dylib", SonamePolicy::System),
    // unixODBC; odbc32 dlopens it lazily and reports no driver without it.
    ("libodbc.dylib", SonamePolicy::Optional),
];

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
}

/// The tree a round plans against.
#[derive(Debug, Default)]
pub struct Tree {
    /// Every regular Mach-O file.
    pub images: Vec<Image>,
    /// Payload-relative paths of every regular file and of every symlink that
    /// resolves to a file inside the payload.
    pub files: BTreeSet<PathBuf>,
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
/// an error. For every SONAME: present under `wine/lib/`, or linked per
/// [`SONAME_POLICY`], or copied from the x86-64 Homebrew (an error when the
/// payload ships it elsewhere without a policy row). For every image: a
/// foreign install name becomes `@rpath/<file name>`; a foreign absolute
/// `LC_RPATH` is dropped; a Unix module naming a supplied SONAME gets the run
/// path to `wine/lib/`; each foreign absolute dependency is rewritten to
/// `@rpath/<leaf>` with the run path to the one payload directory already
/// holding the leaf, else copied to `wine/lib/`; each `@rpath/<leaf>`
/// dependency must be reachable through the image's own `@loader_path` run
/// paths (existing or planned this round), else the run path to the one
/// directory holding the leaf is added (the image's own directory wins a
/// tie; any other tie is an error naming the directories), else the leaf is
/// copied from Homebrew. `@loader_path/` and `@executable_path/` references
/// are left for the checker, whose chain rules decide them.
pub fn plan(tree: &Tree, sonames: &[String], source: &Source<'_>) -> Plan {
    let mut round = Round {
        tree,
        source,
        plan: Plan::default(),
        planned: BTreeSet::new(),
        supplied: BTreeSet::new(),
    };
    round.duplicates();
    for soname in sonames {
        round.soname(soname);
    }
    for image in &tree.images {
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
                SonamePolicy::Payload(canonical) if files.iter().any(|file| file == canonical) => {
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

    fn soname(&mut self, soname: &str) {
        let dest = Path::new(LIB_DIR).join(soname);
        if self.tree.files.contains(&dest) {
            self.supplied.insert(soname.to_owned());
            return;
        }
        let policy = policy(soname);
        match policy {
            SonamePolicy::System => {}
            SonamePolicy::Payload(path) => {
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
        let (ambiguous, plain): (Vec<_>, Vec<_>) =
            rpath_references.into_iter().partition(|(_, rest)| {
                dirs_providing(self.tree, &self.planned, Path::new(rest)).len() > 1
            });
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
        let candidates = dirs_providing(self.tree, &self.planned, Path::new(rest));
        if candidates
            .iter()
            .any(|candidate| pending.reachable.contains(candidate))
        {
            return;
        }
        if candidates.contains(&pending.dir) {
            let own = pending.dir.clone();
            pending.reach(&own);
            return;
        }
        match candidates.as_slice() {
            [] if rest.contains('/') => {
                self.fail(&image.relative, reference, "names no file in the payload");
            }
            [] => match (self.source)(rest) {
                Ok(file) => {
                    self.copy(file, Path::new(LIB_DIR).join(rest));
                    pending.reach(Path::new(LIB_DIR));
                }
                Err(reason) => self.fail(
                    &image.relative,
                    reference,
                    &format!("names no file in the payload, and {HOMEBREW_LIB}/{rest}: {reason}"),
                ),
            },
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
            PathBuf::from(LIB_DIR)
        } else {
            let providers = dirs_providing(self.tree, &self.planned, Path::new(leaf));
            match providers.as_slice() {
                [] => match (self.source)(reference) {
                    Ok(file) => {
                        self.copy(file, dest);
                        PathBuf::from(LIB_DIR)
                    }
                    Err(reason) => {
                        self.fail(&image.relative, reference, &reason);
                        return;
                    }
                },
                // The payload ships the leaf already (the framework's
                // libintl for gnutls): one file per install name, so the
                // reference turns to that file instead of a second copy.
                [only] => only.clone(),
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
            return Err(format!(
                "{}: not under {HOMEBREW_CELLAR}/<formula>/<version>/, so its formula and version are unknown",
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
            None => missing.push(format!(
                "{} comes from {formula} {version} and build/bundle-spec.toml has no [components.{formula}] row",
                copy.dest.display()
            )),
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
        if entry.link_target.is_some() {
            if fs::canonicalize(&entry.absolute)
                .is_ok_and(|real| real.starts_with(payload) && real.is_file())
            {
                tree.files.insert(PathBuf::from(&entry.relative));
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
        let mut image = Image {
            relative: entry.relative.clone(),
            id: id_entry(&load_commands).map(str::to_owned),
            dependencies: dependency_entries(&load_commands)
                .map(str::to_owned)
                .collect(),
            rpaths: rpath_entries(&load_commands).map(str::to_owned).collect(),
            executable,
            sonames: BTreeSet::new(),
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

/// Applies one round, whole or not at all: every file the round creates,
/// replaces, or rewrites is journaled, and a failure puts each one back from
/// its pre-image before the error (the file and the tool's stderr) returns.
fn apply(payload: &Path, plan: &Plan, run: &mut Runner<'_>) -> Result<(), String> {
    let mut journal = Journal::new()?;
    match apply_journaled(payload, plan, run, &mut journal) {
        Ok(()) => {
            journal.discard();
            Ok(())
        }
        Err(error) => Err(match journal.restore() {
            Ok(restored) => {
                format!("{error}; the round was rolled back ({restored} file(s) restored)")
            }
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
        let dest = payload.join(&copy.dest);
        journal.created(&dest);
        copy_library(&copy.source, &dest, run)?;
        println!(
            "[xtask] close: copied {} <- {}",
            copy.dest.display(),
            copy.source.display()
        );
    }
    for link in &plan.links {
        let path = payload.join(&link.link);
        journal.created(&path);
        symlink(&link.target, &path).map_err(|error| format!("{}: {error}", path.display()))?;
        println!(
            "[xtask] close: linked {} -> {}",
            link.link.display(),
            link.target.display()
        );
    }
    for link in &plan.replaced {
        let path = payload.join(&link.link);
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
        let path = payload.join(binary);
        journal.save(&path)?;
        install_name_tool(&path, edit, run)?;
        codesign(&path, run)?;
        println!("[xtask] close: rewrote {binary}: {}", describe(edit));
    }
    Ok(())
}

static JOURNALS: AtomicUsize = AtomicUsize::new(0);

/// The pre-images of one round: files it created (to remove) and copies of
/// files it rewrote or replaced (to put back).
struct Journal {
    dir: PathBuf,
    created: Vec<PathBuf>,
    saved: Vec<(PathBuf, PathBuf)>,
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

    /// Records a path the round is about to create.
    fn created(&mut self, path: &Path) {
        self.created.push(path.to_path_buf());
    }

    /// Copies a file aside before the round rewrites or replaces it.
    fn save(&mut self, path: &Path) -> Result<(), String> {
        let backup = self.dir.join(self.saved.len().to_string());
        fs::copy(path, &backup).map_err(|error| {
            format!(
                "{}: cannot save a copy aside before rewriting it: {error}",
                path.display()
            )
        })?;
        self.saved.push((path.to_path_buf(), backup));
        Ok(())
    }

    /// Removes what the round created and puts back what it changed, in
    /// reverse order; the count of files touched, or every failure.
    fn restore(self) -> Result<usize, String> {
        let mut restored = 0;
        let mut failures = Vec::new();
        for path in self.created.iter().rev() {
            match remove(path) {
                Ok(()) => restored += 1,
                Err(error) => failures.push(error),
            }
        }
        for (path, backup) in self.saved.iter().rev() {
            // A replaced file is a link now; the copy must not follow it.
            if let Err(error) = remove(path) {
                failures.push(error);
                continue;
            }
            match fs::copy(backup, path) {
                Ok(_) => restored += 1,
                Err(error) => failures.push(format!("{}: {error}", path.display())),
            }
        }
        let _ = fs::remove_dir_all(&self.dir);
        if failures.is_empty() {
            Ok(restored)
        } else {
            Err(failures.join("; "))
        }
    }

    fn discard(self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Removes a file or symlink; an absent path is fine.
fn remove(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
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

/// Copies one library, thinning a universal file to x86-64, and refuses
/// anything that is not an x86-64 Mach-O afterwards.
fn copy_library(source: &Path, dest: &Path, run: &mut Runner<'_>) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }
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

fn codesign(path: &Path, run: &mut Runner<'_>) -> Result<(), String> {
    run(
        "codesign",
        &[
            OsStr::new("--force"),
            OsStr::new("--sign"),
            OsStr::new("-"),
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
    use std::collections::BTreeSet;
    use std::ffi::OsStr;
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{
        Copy, Edit, Header, Image, Link, Outcome, Plan, Tree, apply, gate_copies, header,
        homebrew_formula, plan, relative_path, rpath_from, settle, sonames_from_config_h,
    };
    use crate::bundle_toml::BundleSpec;
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
        }
    }

    fn naming(mut image: Image, sonames: &[&str]) -> Image {
        image.sonames = sonames.iter().map(|soname| (*soname).to_owned()).collect();
        image
    }

    fn tree(images: Vec<Image>, extra_files: &[&str]) -> Tree {
        let mut files: BTreeSet<PathBuf> = images
            .iter()
            .map(|image| PathBuf::from(&image.relative))
            .collect();
        files.extend(extra_files.iter().map(PathBuf::from));
        Tree { images, files }
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
            ],
            &[
                &format!("{MODULES}/ntdll.so"),
                "vulkan/libMoltenVK.dylib",
                &format!("{GST_LIB}/libfreetype.6.dylib"),
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
    fn copied_libraries_are_closed_over_their_dependencies() {
        let names = sonames(&SONAMES);
        let mut t = win32u_tree();
        for file in [
            "wine/lib/libgnutls.30.dylib",
            "wine/lib/libfreetype.6.dylib",
            "wine/lib/libMoltenVK.dylib",
        ] {
            t.files.insert(file.into());
        }
        for module in &mut t.images {
            module.rpaths.push("@loader_path/../..".into());
        }
        t.images.push(image(
            "wine/lib/libgnutls.30.dylib",
            Some("/usr/local/opt/gnutls/lib/libgnutls.30.dylib"),
            &[
                "/usr/local/opt/gettext/lib/libintl.8.dylib",
                "/usr/local/opt/p11-kit/lib/libp11-kit.0.dylib",
                "/usr/lib/libSystem.B.dylib",
            ],
            &[],
        ));
        let p = plan(&t, &names, &homebrew);
        assert_eq!(p.errors, []);
        assert_eq!(p.links, []);
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
        t.images[2] = image(
            "wine/lib/libgnutls.30.dylib",
            Some("@rpath/libgnutls.30.dylib"),
            &["@rpath/libintl.8.dylib", "@rpath/libp11-kit.0.dylib"],
            &[
                "@loader_path/GStreamer.framework/Versions/1.0/lib",
                "@loader_path",
            ],
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

        t.images[3] = image(
            "wine/lib/libp11-kit.0.dylib",
            Some("@rpath/libp11-kit.0.dylib"),
            &["/usr/lib/libSystem.B.dylib"],
            &["/usr/lib/swift"],
        );
        assert!(plan(&t, &names, &homebrew).is_empty());
    }

    /// A leaf in two directories, one of them a link: the image whose run
    /// path reaches either is fine; one reaching neither is ambiguous, and
    /// the error names both directories.
    #[test]
    fn a_leaf_behind_a_link_is_no_tie_for_a_reaching_image_and_names_both_dirs_otherwise() {
        let other = format!("{MODULES}/other.so");
        let t = tree(
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
        let p = plan(&t, &[], &homebrew);
        assert_eq!(p.edits.len(), 0, "{p:?}");
        assert_eq!(p.errors.len(), 1, "{p:?}");
        assert_eq!(p.errors[0].binary, other);
        assert_eq!(p.errors[0].reference, "@rpath/libfreetype.6.dylib");
        assert!(
            p.errors[0]
                .reason
                .contains(&format!("2 directories (wine/lib, {GST_LIB})")),
            "{}",
            p.errors[0]
        );
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
        assert_eq!(p.errors.len(), 1, "{p:?}");
        assert_eq!(
            p.errors[0].binary,
            format!("wine/lib/libintl.8.dylib, {GST_LIB}/libintl.8.dylib")
        );
        assert_eq!(p.errors[0].reference, "@rpath/libintl.8.dylib");
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
        let t = tree(
            vec![loader, framework],
            &[
                "wine/lib/libfreetype.6.dylib",
                &format!("{GST_LIB}/libfreetype.6.dylib"),
            ],
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
    /// copy and the link are gone, the first binary and the replaced
    /// duplicate are back to their bytes, and the error names the binary and
    /// the tool's stderr.
    #[test]
    fn a_tool_failure_rolls_the_round_back() {
        let dir = scratch("rollback");
        let payload = dir.join("payload");
        fake_macho(&payload.join("wine/lib/a.so"), b"a");
        fake_macho(&payload.join("wine/lib/b.so"), b"b");
        fake_macho(&payload.join("vulkan/libMoltenVK.dylib"), b"pin");
        fake_macho(&payload.join("fw/libMoltenVK.dylib"), b"dup");
        fake_macho(&dir.join("src/libA.1.dylib"), b"A");
        let before = |name: &str| fs::read(payload.join(name)).expect(name);
        let plan = Plan {
            copies: vec![Copy {
                source: dir.join("src/libA.1.dylib"),
                dest: "wine/lib/libA.1.dylib".into(),
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
        // The copy, the link, the replaced file, a.so, and b.so (saved
        // before its tool failed) are all put back.
        assert!(
            error.contains("wine/lib/b.so")
                && error.contains("do not fit")
                && error.contains("rolled back (5 file(s) restored)"),
            "{error}"
        );
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert!(calls[0].ends_with("a.so") && calls[1].starts_with("codesign"));
        assert!(!payload.join("wine/lib/libA.1.dylib").exists());
        assert!(fs::symlink_metadata(payload.join("wine/lib/libMoltenVK.dylib")).is_err());
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
            calls[1].starts_with("codesign --force --sign - "),
            "{}",
            calls[1]
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
