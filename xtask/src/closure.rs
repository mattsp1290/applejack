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
//! until a round plans nothing. The checker's `inspect` then has the last
//! word, so `--close` cannot succeed on a tree `--check` rejects.
//!
//! Wine passes a bare SONAME to `dlopen` (`dlls/win32u/freetype.c`,
//! `dlls/bcrypt/gnutls.c`, `dlls/winebus.sys/bus_sdl.c`, ...) and sets no
//! search path for it; dyld resolves a leaf name as `@rpath/<leaf>` through
//! the run paths of the calling image and of the main executable, then the
//! working directory and `/usr/lib`, never `/usr/local/lib`. The executables
//! Wine links have no header padding for a new load command, so the run path
//! goes on the Unix modules whose bytes name the SONAME.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io::Read as _;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::bundle_toml::BundleSpec;
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
/// has not settled by now is cycling.
const MAX_ROUNDS: usize = 8;
const CPU_TYPE_X86_64: u32 = 0x0100_0007;

/// How a SONAME from `config.h` is satisfied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SonamePolicy {
    /// Copied from `/usr/local/lib/<soname>`; the default for any SONAME
    /// without a row in [`SONAME_POLICY`].
    Homebrew,
    /// A component the payload already ships at this bundle-relative path;
    /// `wine/lib/<soname>` becomes a relative link to it.
    Payload(&'static str),
    /// A macOS library dyld finds in the shared cache on its own.
    System,
    /// Copied when the x86-64 Homebrew has it, skipped with a message
    /// otherwise: Wine degrades without it.
    Optional,
}

/// The SONAMEs that are not plain Homebrew copies. A new SONAME in
/// `config.h` that is neither here nor in the x86-64 Homebrew fails the
/// closure, which is the moment to classify it.
pub const SONAME_POLICY: &[(&str, SonamePolicy)] = &[
    // Wine's SONAME_LIBVULKAN and SONAME_LIBMOLTENVK: the pinned MoltenVK
    // release (`components.moltenvk`), not Homebrew's molten-vk formula.
    (
        "libMoltenVK.dylib",
        SonamePolicy::Payload("vulkan/libMoltenVK.dylib"),
    ),
    // No file under /usr/lib since macOS 11; dyld serves it from the cache.
    ("libcups.2.dylib", SonamePolicy::System),
    // unixODBC; odbc32 dlopens it lazily and reports no driver without it.
    ("libodbc.dylib", SonamePolicy::Optional),
];

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
        self.copies.is_empty() && self.links.is_empty() && self.edits.is_empty()
    }
}

/// Resolves a dependency reference (absolute) or a SONAME (a leaf, looked up
/// under `/usr/local/lib`) to the canonical file to copy, or says why not.
pub type Source<'a> = dyn Fn(&str) -> Result<PathBuf, String> + 'a;

/// Plans one round over `tree`.
///
/// For every SONAME: present under `wine/lib/`, or supplied per
/// [`SONAME_POLICY`], or copied from the x86-64 Homebrew. For every image: a
/// foreign install name becomes `@rpath/<file name>`; a foreign absolute
/// `LC_RPATH` is dropped; a Unix module naming a supplied SONAME gets the run
/// path to `wine/lib/`; each `@rpath/<leaf>` dependency must be reachable
/// through the image's own `@loader_path` run paths (existing or planned
/// this round), else the run path to the one directory holding the leaf is
/// added (the image's own directory wins a tie; any other tie is an error),
/// else the leaf is copied from Homebrew; each foreign absolute dependency
/// is copied to `wine/lib/` when absent and rewritten to `@rpath/<leaf>`.
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

    fn soname(&mut self, soname: &str) {
        let dest = Path::new(LIB_DIR).join(soname);
        if self.tree.files.contains(&dest) {
            self.supplied.insert(soname.to_owned());
            return;
        }
        let policy = SONAME_POLICY
            .iter()
            .find(|(name, _)| *name == soname)
            .map_or(SonamePolicy::Homebrew, |(_, policy)| *policy);
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
            SonamePolicy::Homebrew | SonamePolicy::Optional => match (self.source)(soname) {
                Ok(file) => {
                    self.copy(file, dest);
                    self.supplied.insert(soname.to_owned());
                }
                Err(reason) if policy == SonamePolicy::Optional => self.plan.notes.push(format!(
                    "{soname}: optional, not shipped ({HOMEBREW_LIB}/{soname}: {reason})"
                )),
                Err(reason) => self.fail(
                    CONFIG_H,
                    soname,
                    &format!(
                        "dlopen'd by name; {HOMEBREW_LIB}/{soname}: {reason}; no row in the SONAME policy"
                    ),
                ),
            },
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
                self.rpath_dependency(image, &mut pending, reference, rest);
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
            several => {
                let dirs = several
                    .iter()
                    .map(|dir| dir.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(", ");
                self.fail(
                    &image.relative,
                    reference,
                    &format!(
                        "{rest} is in {} directories ({dirs}) and none is on the binary's run path",
                        several.len()
                    ),
                );
            }
        }
    }

    fn absolute_dependency(&mut self, image: &Image, pending: &mut ImageEdit, reference: &str) {
        let leaf = file_name(reference);
        let dest = Path::new(LIB_DIR).join(leaf);
        if !self.present(&dest) {
            match (self.source)(reference) {
                Ok(file) => self.copy(file, dest),
                Err(reason) => {
                    self.fail(&image.relative, reference, &reason);
                    return;
                }
            }
        }
        pending
            .edit
            .changes
            .push((reference.to_owned(), format!("@rpath/{leaf}")));
        pending.reach(Path::new(LIB_DIR));
    }
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

/// `(formula, version)` of a file under the x86-64 Homebrew Cellar.
pub fn homebrew_formula(source: &Path) -> Option<(String, String)> {
    let mut rest = source.strip_prefix(HOMEBREW_CELLAR).ok()?.components();
    let formula = rest.next()?.as_os_str().to_str()?.to_owned();
    let version = rest.next()?.as_os_str().to_str()?.to_owned();
    Some((formula, version))
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

    let mut copied = 0;
    let mut rewritten: BTreeSet<String> = BTreeSet::new();
    let mut noted: BTreeSet<String> = BTreeSet::new();
    let mut rounds = 0;
    loop {
        rounds += 1;
        if rounds > MAX_ROUNDS {
            return Err(format!(
                "no fixed point after {MAX_ROUNDS} rounds; the tree keeps changing"
            ));
        }
        let tree = scan(&payload, &sonames)?;
        let plan = plan(&tree, &sonames, &source);
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
                "{} reference(s) cannot be closed (round {rounds})",
                plan.errors.len()
            ));
        }
        if plan.is_empty() {
            break;
        }
        gate_copies(&denylist, &spec, &plan)?;
        apply(&payload, &plan)?;
        copied += plan.copies.len();
        rewritten.extend(plan.edits.keys().cloned());
    }
    println!(
        "[xtask] close {}: settled after {} round(s): {copied} librar{} copied, {} binar{} rewritten",
        payload.display(),
        rounds - 1,
        if copied == 1 { "y" } else { "ies" },
        rewritten.len(),
        if rewritten.len() == 1 { "y" } else { "ies" },
    );
    check(root, &payload)
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
                "{}: not under {HOMEBREW_CELLAR}, so its formula and version are unknown",
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

fn apply(payload: &Path, plan: &Plan) -> Result<(), String> {
    for copy in &plan.copies {
        copy_library(&copy.source, &payload.join(&copy.dest))?;
        println!(
            "[xtask] close: copied {} <- {}",
            copy.dest.display(),
            copy.source.display()
        );
    }
    for link in &plan.links {
        let path = payload.join(&link.link);
        symlink(&link.target, &path).map_err(|error| format!("{}: {error}", path.display()))?;
        println!(
            "[xtask] close: linked {} -> {}",
            link.link.display(),
            link.target.display()
        );
    }
    for (binary, edit) in &plan.edits {
        let path = payload.join(binary);
        install_name_tool(&path, edit)?;
        codesign(&path)?;
        println!("[xtask] close: rewrote {binary}: {}", describe(edit));
    }
    Ok(())
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
fn copy_library(source: &Path, dest: &Path) -> Result<(), String> {
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

fn install_name_tool(path: &Path, edit: &Edit) -> Result<(), String> {
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

fn codesign(path: &Path) -> Result<(), String> {
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
    use std::path::{Path, PathBuf};

    use super::{
        Copy, Edit, Header, Image, Link, Plan, Tree, header, homebrew_formula, plan, relative_path,
        rpath_from, sonames_from_config_h,
    };

    const MODULES: &str = "wine/lib/wine/x86_64-unix";
    const GST_LIB: &str = "wine/lib/GStreamer.framework/Versions/1.0/lib";

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

    /// A Homebrew with freetype, libpng, gnutls, and gettext, none of them
    /// universal; `libodbc.dylib` is absent and `libarm.dylib` is arm64.
    fn homebrew(reference: &str) -> Result<PathBuf, String> {
        let leaf = reference.rsplit('/').next().unwrap_or(reference);
        let cellar = |formula: &str, version: &str| {
            Ok(PathBuf::from(format!(
                "/usr/local/Cellar/{formula}/{version}/lib/{leaf}"
            )))
        };
        match leaf {
            "libfreetype.6.dylib" => cellar("freetype", "2.13.3"),
            "libpng16.16.dylib" => cellar("libpng", "1.6.50"),
            "libgnutls.30.dylib" => cellar("gnutls", "3.8.10"),
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
            homebrew_formula(Path::new("/usr/local/lib/libfoo.dylib")),
            None
        );
        assert_eq!(
            homebrew_formula(Path::new("/opt/homebrew/Cellar/x/1/lib/a")),
            None
        );
    }

    #[test]
    fn headers_tell_thin_x86_64_from_fat_and_arm64() {
        let x86_64 = [0xcf, 0xfa, 0xed, 0xfe, 0x07, 0x00, 0x00, 0x01];
        assert_eq!(
            header(&x86_64),
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

    /// The real payload's shape before the closure: winegstreamer's
    /// `@rpath/libgst*` references resolve only through a run path into the
    /// framework; the Wine modules' `@rpath/ntdll.so` already resolves
    /// through `@loader_path/`.
    #[test]
    fn a_module_gets_the_run_path_to_the_framework_library() {
        let winegstreamer = format!("{MODULES}/winegstreamer.so");
        let t = tree(
            vec![
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
            ],
            &[],
        );
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
                add_rpaths: vec!["@loader_path/../../GStreamer.framework/Versions/1.0/lib".into()],
                ..Edit::default()
            }
        );

        // After the rewrite the framework directory is on the run path and
        // the plan is empty: the closure is idempotent.
        let mut closed = t;
        closed.images[1]
            .rpaths
            .push("@loader_path/../../GStreamer.framework/Versions/1.0/lib".into());
        assert!(plan(&closed, &[], &homebrew).is_empty());
    }

    fn win32u_tree() -> Tree {
        tree(
            vec![naming(
                image(
                    &format!("{MODULES}/win32u.so"),
                    Some("@rpath/win32u.so"),
                    &["@rpath/ntdll.so"],
                    &["@loader_path/"],
                ),
                &["libfreetype.6.dylib", "libMoltenVK.dylib"],
            )],
            &[&format!("{MODULES}/ntdll.so"), "vulkan/libMoltenVK.dylib"],
        )
    }

    const WIN32U_SONAMES: [&str; 4] = [
        "libfreetype.6.dylib",
        "libMoltenVK.dylib",
        "libcups.2.dylib",
        "libodbc.dylib",
    ];

    /// The first round: freetype is copied, `MoltenVK` linked to the pinned
    /// copy, cups is the system's, odbc is noted; win32u names two supplied
    /// SONAMEs and gets the run path once.
    #[test]
    fn sonames_are_copied_linked_or_left_per_policy() {
        let win32u = format!("{MODULES}/win32u.so");
        let p = plan(&win32u_tree(), &sonames(&WIN32U_SONAMES), &homebrew);
        assert_eq!(p.errors, []);
        assert_eq!(
            p.copies,
            [Copy {
                source: "/usr/local/Cellar/freetype/2.13.3/lib/libfreetype.6.dylib".into(),
                dest: "wine/lib/libfreetype.6.dylib".into(),
            }]
        );
        assert_eq!(
            p.links,
            [Link {
                link: "wine/lib/libMoltenVK.dylib".into(),
                target: "../../vulkan/libMoltenVK.dylib".into(),
            }]
        );
        assert_eq!(
            edit(&p, &win32u),
            &Edit {
                add_rpaths: vec!["@loader_path/../..".into()],
                ..Edit::default()
            }
        );
        assert_eq!(p.notes.len(), 1, "{:?}", p.notes);
        assert!(
            p.notes[0].starts_with("libodbc.dylib: optional"),
            "{}",
            p.notes[0]
        );
    }

    /// The rounds after the copy: the copied library's own install name,
    /// foreign dependency, and run path; then the dependency's install name
    /// and a stray absolute run path; then nothing.
    #[test]
    fn copied_libraries_are_closed_over_their_dependencies() {
        let win32u = format!("{MODULES}/win32u.so");
        let names = sonames(&WIN32U_SONAMES);
        let mut t = win32u_tree();
        t.files.insert("wine/lib/libfreetype.6.dylib".into());
        t.files.insert("wine/lib/libMoltenVK.dylib".into());
        t.images[0].rpaths.push("@loader_path/../..".into());
        t.images.push(image(
            "wine/lib/libfreetype.6.dylib",
            Some("/usr/local/opt/freetype/lib/libfreetype.6.dylib"),
            &[
                "/usr/lib/libz.1.dylib",
                "/usr/local/opt/libpng/lib/libpng16.16.dylib",
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
                source: "/usr/local/Cellar/libpng/1.6.50/lib/libpng16.16.dylib".into(),
                dest: "wine/lib/libpng16.16.dylib".into(),
            }]
        );
        assert_eq!(
            edit(&p, "wine/lib/libfreetype.6.dylib"),
            &Edit {
                id: Some("@rpath/libfreetype.6.dylib".into()),
                changes: vec![(
                    "/usr/local/opt/libpng/lib/libpng16.16.dylib".into(),
                    "@rpath/libpng16.16.dylib".into()
                )],
                add_rpaths: vec!["@loader_path".into()],
                delete_rpaths: vec![],
            }
        );
        assert!(!p.edits.contains_key(&win32u), "{p:?}");

        t.files.insert("wine/lib/libpng16.16.dylib".into());
        t.images[1] = image(
            "wine/lib/libfreetype.6.dylib",
            Some("@rpath/libfreetype.6.dylib"),
            &["/usr/lib/libz.1.dylib", "@rpath/libpng16.16.dylib"],
            &["@loader_path"],
        );
        t.images.push(image(
            "wine/lib/libpng16.16.dylib",
            Some("/usr/local/opt/libpng/lib/libpng16.16.dylib"),
            &["/usr/lib/libSystem.B.dylib"],
            &["/usr/local/lib", "/usr/lib/swift"],
        ));
        let p = plan(&t, &names, &homebrew);
        assert_eq!(p.errors, []);
        assert_eq!(p.copies, []);
        assert_eq!(
            edit(&p, "wine/lib/libpng16.16.dylib"),
            &Edit {
                id: Some("@rpath/libpng16.16.dylib".into()),
                delete_rpaths: vec!["/usr/local/lib".into()],
                ..Edit::default()
            }
        );
        assert_eq!(p.edits.len(), 1, "{p:?}");

        t.images[2] = image(
            "wine/lib/libpng16.16.dylib",
            Some("@rpath/libpng16.16.dylib"),
            &["/usr/lib/libSystem.B.dylib"],
            &["/usr/lib/swift"],
        );
        assert!(plan(&t, &names, &homebrew).is_empty());
    }

    #[test]
    fn a_leaf_present_in_two_directories_prefers_the_binarys_own() {
        // gnutls in wine/lib names @rpath/libintl.8.dylib, which exists both
        // beside it and in the framework: its own directory wins. A module
        // elsewhere that reaches neither is ambiguous.
        let t = tree(
            vec![
                image(
                    "wine/lib/libgnutls.30.dylib",
                    Some("@rpath/libgnutls.30.dylib"),
                    &["@rpath/libintl.8.dylib"],
                    &[],
                ),
                image(
                    &format!("{MODULES}/other.so"),
                    Some("@rpath/other.so"),
                    &["@rpath/libintl.8.dylib"],
                    &["@loader_path/"],
                ),
            ],
            &[
                "wine/lib/libintl.8.dylib",
                &format!("{GST_LIB}/libintl.8.dylib"),
            ],
        );
        let p = plan(&t, &[], &homebrew);
        assert_eq!(
            edit(&p, "wine/lib/libgnutls.30.dylib").add_rpaths,
            ["@loader_path"]
        );
        assert_eq!(p.errors.len(), 1, "{p:?}");
        assert_eq!(p.errors[0].binary, format!("{MODULES}/other.so"));
        assert!(
            p.errors[0].reason.contains("2 directories"),
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
                    "@rpath/libnowhere.dylib"
                ),
                (
                    "wine/lib/wine/x86_64-unix/bad.so",
                    "/System/Volumes/Data/usr/local/lib/libx.dylib"
                ),
                ("wine/lib/wine/x86_64-unix/bad.so", "libbare.dylib"),
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
        // Nothing is planned beside the errors.
        assert_eq!(p.copies, []);
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
}
