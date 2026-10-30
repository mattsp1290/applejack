//! Self-containment check: every Mach-O in the payload may link only against
//! the system (`/System`, `/usr/lib`) or relocatable references inside the
//! payload. The hardened runtime drops `DYLD_*`, so anything else fails at
//! load time on a machine without the build host's Homebrew.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Component, Path, PathBuf};

/// One dependency reference that would not resolve on a clean machine.
#[derive(Debug, PartialEq, Eq)]
pub struct Violation {
    pub binary: String,
    pub reference: String,
    pub reason: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} ({})", self.binary, self.reference, self.reason)
    }
}

const SYSTEM_ROOTS: [&str; 2] = ["/System/", "/usr/lib/"];
const RELATIVE_PREFIXES: [&str; 3] = ["@rpath/", "@loader_path/", "@executable_path/"];
/// Load commands that name a dependency, as opposed to `LC_ID_DYLIB`, which
/// names the binary itself.
const DEPENDENCY_COMMANDS: [&str; 4] = [
    "LC_LOAD_DYLIB",
    "LC_LOAD_WEAK_DYLIB",
    "LC_REEXPORT_DYLIB",
    "LC_LOAD_UPWARD_DYLIB",
];

/// Checks the text of `otool -L <binary>` for one Mach-O.
///
/// `binary` is the payload-relative path reported in each violation.
/// `payload_root` and `build_dir` are absolute; references into either are
/// violations because the payload moves after the build.
pub fn check_otool_listing(
    binary: &str,
    listing: &str,
    payload_root: &Path,
    build_dir: &Path,
) -> Vec<Violation> {
    listing
        .lines()
        // `<binary>:` and per-slice `<binary> (architecture x86_64):` headers
        .filter(|line| !line.trim_end().ends_with(':'))
        .filter_map(|line| {
            let reference = line.trim().split(" (compatibility").next()?.trim();
            (!reference.is_empty()).then_some(reference)
        })
        .filter_map(|reference| {
            classify(reference, payload_root, build_dir).map(|reason| Violation {
                binary: binary.to_owned(),
                reference: reference.to_owned(),
                reason: reason.to_owned(),
            })
        })
        .collect()
}

/// Checks the text of `otool -l <binary>` for run-path entries that would not
/// resolve on a clean machine.
///
/// `@rpath/` references pass [`check_otool_listing`] whatever they resolve
/// through, so an absolute `LC_RPATH` outside the system is the leftover
/// this catches: an install name rewritten while the binary keeps
/// `/usr/local/lib` as a search path.
pub fn check_rpaths(
    binary: &str,
    load_commands: &str,
    payload_root: &Path,
    build_dir: &Path,
) -> Vec<Violation> {
    rpath_entries(load_commands)
        .filter_map(|path| {
            let reason = if path == "@loader_path"
                || path.starts_with("@loader_path/")
                || path == "@executable_path"
                || path.starts_with("@executable_path/")
            {
                None
            } else if path.starts_with('/') {
                classify(path, payload_root, build_dir)
            } else {
                Some("rpath is neither @loader_path, @executable_path, nor absolute")
            };
            reason.map(|reason| Violation {
                binary: binary.to_owned(),
                reference: format!("LC_RPATH {path}"),
                reason: reason.to_owned(),
            })
        })
        .collect()
}

/// The `path` of every `LC_RPATH` load command in `otool -l` output.
fn rpath_entries(load_commands: &str) -> impl Iterator<Item = &str> {
    command_values(load_commands, "path ", |command| command == "LC_RPATH")
}

/// The `name` of every dependency load command in `otool -l` output. The
/// binary's own `LC_ID_DYLIB` is not a dependency and is left out.
fn dependency_entries(load_commands: &str) -> impl Iterator<Item = &str> {
    command_values(load_commands, "name ", |command| {
        DEPENDENCY_COMMANDS.contains(&command)
    })
}

/// The `<field> <value> (offset N)` line of each load command `wanted`
/// accepts.
fn command_values<'a>(
    load_commands: &'a str,
    field: &'static str,
    wanted: impl Fn(&str) -> bool,
) -> impl Iterator<Item = &'a str> {
    let mut in_wanted = false;
    load_commands
        .lines()
        .map(str::trim)
        .filter_map(move |line| {
            if let Some(command) = line.strip_prefix("cmd ") {
                in_wanted = wanted(command.trim());
                return None;
            }
            if !in_wanted {
                return None;
            }
            let rest = line.strip_prefix(field)?;
            let value = rest
                .rsplit_once(" (offset ")
                .map_or(rest, |(value, _)| value);
            Some(value.trim())
        })
}

/// Whether `otool -l` output describes an executable (`LC_MAIN`, or
/// `LC_UNIXTHREAD` before 10.8) rather than a dylib or bundle.
pub fn is_executable(load_commands: &str) -> bool {
    load_commands
        .lines()
        .map(str::trim)
        .any(|line| line == "cmd LC_MAIN" || line == "cmd LC_UNIXTHREAD")
}

/// Whether any `LC_RPATH` is a system root such as `/usr/lib/swift`. dyld
/// searches it on its own, so nothing in the payload can show an `@rpath/`
/// reference of that binary unresolved.
pub fn has_system_rpath(load_commands: &str) -> bool {
    rpath_entries(load_commands).any(is_system)
}

/// The payload-relative directories a binary's relocatable `LC_RPATH`
/// entries name: `@loader_path` against the binary's own directory,
/// `@executable_path` against each of `exe_dirs`, the directory of the
/// executable whose load chain is being followed. An entry that climbs out
/// of the payload or is absolute names nothing here; `check_rpaths` reports
/// the absolute ones.
pub fn relocated_rpaths(binary: &str, load_commands: &str, exe_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let bin_dir = binary_dir(binary);
    rpath_entries(load_commands)
        .flat_map(|path| {
            let (bases, rest): (Vec<&Path>, &str) =
                if let Some(rest) = path.strip_prefix("@loader_path") {
                    (vec![bin_dir.as_path()], rest)
                } else if let Some(rest) = path.strip_prefix("@executable_path") {
                    (exe_dirs.iter().map(PathBuf::as_path).collect(), rest)
                } else {
                    (Vec::new(), "")
                };
            let rest = rest.strip_prefix('/').unwrap_or(rest);
            bases
                .into_iter()
                .filter_map(|base| normalize_relative(&base.join(rest)))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// What one binary's `@rpath/` and `@executable_path/` references resolve
/// through along one load chain. dyld consults the rpaths of the image
/// itself and of the images that loaded it up to the main executable, never
/// those of unrelated images, so the set is the binary's own rpaths plus the
/// chain's executable's (which also covers a dlopen'd Unix module or plugin).
pub struct Search<'a> {
    /// Relocated `LC_RPATH` directories of the binary and of the chain's
    /// executable.
    pub rpath_dirs: &'a BTreeSet<PathBuf>,
    /// The directory of the chain's executable; empty when no executable
    /// can load the binary.
    pub exe_dirs: &'a [PathBuf],
    /// An `LC_RPATH` of the binary or of the chain's executable is a system
    /// root, so an `@rpath/` reference the payload does not satisfy is left
    /// to dyld.
    pub system_rpath: bool,
}

/// How one relative dependency fared under a [`Search`].
enum Outcome {
    Resolved,
    Unresolved(&'static str),
    /// An `@rpath/` reference the payload does not satisfy under a system
    /// `LC_RPATH`: dyld may still find it in the shared cache.
    LeftToDyld,
}

/// Checks that every `@rpath/`, `@loader_path/`, and `@executable_path/`
/// dependency of one Mach-O names a file in the payload.
///
/// `@rpath/x` is tried through `search.rpath_dirs`; `@loader_path/x`
/// resolves against the binary's own directory and `@executable_path/x`
/// against the chain's executable directory. `resolves` says whether a
/// payload-relative candidate is a file inside the payload; it follows
/// symlinks, so a framework's `Versions/Current` link works.
pub fn check_references_resolve(
    binary: &str,
    load_commands: &str,
    search: &Search<'_>,
    resolves: &dyn Fn(&Path) -> bool,
) -> Vec<Violation> {
    resolve_references(binary, load_commands, search, resolves)
        .filter_map(|(reference, outcome)| match outcome {
            Outcome::Unresolved(reason) => Some(Violation {
                binary: binary.to_owned(),
                reference: reference.to_owned(),
                reason: reason.to_owned(),
            }),
            Outcome::Resolved | Outcome::LeftToDyld => None,
        })
        .collect()
}

/// The `@rpath/` dependencies of one Mach-O that the payload does not
/// satisfy and [`check_references_resolve`] left to dyld because the search
/// carries a system `LC_RPATH`.
pub fn references_left_to_dyld<'a>(
    binary: &str,
    load_commands: &'a str,
    search: &Search<'_>,
    resolves: &dyn Fn(&Path) -> bool,
) -> Vec<&'a str> {
    resolve_references(binary, load_commands, search, resolves)
        .filter_map(|(reference, outcome)| {
            matches!(outcome, Outcome::LeftToDyld).then_some(reference)
        })
        .collect()
}

fn resolve_references<'a>(
    binary: &str,
    load_commands: &'a str,
    search: &Search<'_>,
    resolves: &dyn Fn(&Path) -> bool,
) -> impl Iterator<Item = (&'a str, Outcome)> {
    let loader_dirs = vec![binary_dir(binary)];
    let rpath_dirs: Vec<PathBuf> = search.rpath_dirs.iter().cloned().collect();
    let exe_dirs: Vec<PathBuf> = search.exe_dirs.to_vec();
    let system_rpath = search.system_rpath;
    dependency_entries(load_commands).filter_map(move |reference| {
        let (prefix, rest) = RELATIVE_PREFIXES
            .iter()
            .find_map(|prefix| reference.strip_prefix(prefix).map(|rest| (*prefix, rest)))?;
        let (bases, reason): (&[PathBuf], &'static str) = match prefix {
            "@rpath/" => (
                &rpath_dirs,
                "@rpath reference resolves through no LC_RPATH of the binary or of the executable loading it",
            ),
            "@loader_path/" => (
                &loader_dirs,
                "@loader_path reference does not resolve to a file in the payload",
            ),
            _ => (
                &exe_dirs,
                if exe_dirs.is_empty() {
                    "@executable_path reference, and the payload has no executable to resolve it against"
                } else {
                    "@executable_path reference does not resolve against the executable's directory"
                },
            ),
        };
        let found = bases
            .iter()
            .filter_map(|base| normalize_relative(&base.join(rest)))
            .any(|candidate| resolves(&candidate));
        let outcome = if found {
            Outcome::Resolved
        } else if prefix == "@rpath/" && system_rpath {
            Outcome::LeftToDyld
        } else {
            Outcome::Unresolved(reason)
        };
        Some((reference, outcome))
    })
}

/// The payload-relative directory of a payload-relative binary path.
pub(crate) fn binary_dir(binary: &str) -> PathBuf {
    Path::new(binary)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

/// Resolves `.` and `..` lexically; `None` when the path climbs above its
/// root at any point (even if it comes back in) or is not relative.
pub(crate) fn normalize_relative(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            Component::Normal(name) => out.push(name),
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

fn classify(reference: &str, payload_root: &Path, build_dir: &Path) -> Option<&'static str> {
    if RELATIVE_PREFIXES
        .iter()
        .any(|prefix| reference.starts_with(prefix))
    {
        return None;
    }
    // `/usr/lib/../../usr/local/lib/x` is not a system path, and
    // `/System/Volumes/Data/...` is a firmlink back into the data volume.
    if reference.starts_with('/') && reference.split('/').any(|component| component == "..") {
        return Some("absolute path with a `..` component");
    }
    if reference.starts_with("/System/Volumes/") {
        return Some("/System/Volumes/ is a firmlink into the data volume, not the system");
    }
    if is_system(reference) {
        return None;
    }
    if starts_with_dir(reference, "/usr/local") {
        return Some("links into the x86-64 Homebrew prefix /usr/local");
    }
    if starts_with_dir(reference, "/opt/homebrew") {
        return Some("links into the arm64 Homebrew prefix /opt/homebrew");
    }
    if starts_with_dir(reference, &build_dir.to_string_lossy()) {
        return Some("links into the build directory");
    }
    if starts_with_dir(reference, &payload_root.to_string_lossy()) {
        return Some("links into the payload by absolute path instead of @rpath");
    }
    if reference.starts_with('/') {
        return Some("absolute path outside /System and /usr/lib");
    }
    Some("not an absolute, @rpath, @loader_path, or @executable_path reference")
}

fn is_system(reference: &str) -> bool {
    SYSTEM_ROOTS
        .iter()
        .any(|root| starts_with_dir(reference, root))
}

fn starts_with_dir(reference: &str, dir: &str) -> bool {
    let dir = dir.trim_end_matches('/');
    !dir.is_empty()
        && reference
            .strip_prefix(dir)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    use super::{
        Search, check_otool_listing, check_references_resolve, check_rpaths, has_system_rpath,
        is_executable, relocated_rpaths,
    };

    fn files(paths: &[&str]) -> BTreeSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    fn references(violations: &[super::Violation]) -> Vec<&str> {
        violations
            .iter()
            .map(|violation| violation.reference.as_str())
            .collect()
    }

    const LISTING: &str = "\
wine/bin/wine64:
\t@rpath/libwine.1.dylib (compatibility version 1.0.0, current version 1.0.0)
\t@loader_path/../lib/libz.1.dylib (compatibility version 1.0.0, current version 1.3.1)
\t/usr/local/opt/freetype/lib/libfreetype.6.dylib (compatibility version 27.0.0, current version 27.1.0)
\t/opt/homebrew/opt/gnutls/lib/libgnutls.30.dylib (compatibility version 68.0.0, current version 68.2.0)
\t/Users/ci/build/work/wine/stage/lib/libwine.1.dylib (compatibility version 1.0.0, current version 1.0.0)
\t/Users/ci/payload/wine/lib/libwine.1.dylib (compatibility version 1.0.0, current version 1.0.0)
\t/Library/Frameworks/GStreamer.framework/Versions/1.0/lib/libgstreamer-1.0.0.dylib (compatibility version 1.0.0, current version 1.0.0)
\t/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation (compatibility version 150.0.0, current version 1000.0.0)
\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1000.0.0)
";

    #[test]
    fn flags_every_non_system_absolute_reference() {
        let violations = check_otool_listing(
            "wine/bin/wine64",
            LISTING,
            Path::new("/Users/ci/payload"),
            Path::new("/Users/ci/build/work"),
        );
        let references: Vec<&str> = violations
            .iter()
            .map(|violation| violation.reference.as_str())
            .collect();
        assert_eq!(
            references,
            [
                "/usr/local/opt/freetype/lib/libfreetype.6.dylib",
                "/opt/homebrew/opt/gnutls/lib/libgnutls.30.dylib",
                "/Users/ci/build/work/wine/stage/lib/libwine.1.dylib",
                "/Users/ci/payload/wine/lib/libwine.1.dylib",
                "/Library/Frameworks/GStreamer.framework/Versions/1.0/lib/libgstreamer-1.0.0.dylib",
            ]
        );
        assert!(
            violations[0].reason.contains("/usr/local"),
            "{}",
            violations[0]
        );
        assert!(
            violations[2].reason.contains("build directory"),
            "{}",
            violations[2]
        );
        assert!(violations[3].reason.contains("@rpath"), "{}", violations[3]);
        assert!(
            violations
                .iter()
                .all(|violation| violation.binary == "wine/bin/wine64")
        );
    }

    #[test]
    fn accepts_a_clean_listing() {
        let listing = "\
wine/lib/libwine.1.dylib:
\t@rpath/libwine.1.dylib (compatibility version 1.0.0, current version 1.0.0)
\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1000.0.0)
";
        assert_eq!(
            check_otool_listing("x", listing, Path::new("/p"), Path::new("/b")),
            []
        );
    }

    #[test]
    fn skips_fat_binary_slice_headers() {
        let listing = "\
x (architecture x86_64):
\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1000.0.0)
x (architecture arm64):
\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1000.0.0)
";
        assert_eq!(
            check_otool_listing("x", listing, Path::new("/p"), Path::new("/b")),
            []
        );
    }

    #[test]
    fn rejects_spoofed_system_prefixes() {
        let listing = "\
x:
\t/usr/lib/../../usr/local/lib/x.dylib (compatibility version 1.0.0, current version 1.0.0)
\t/System/Volumes/Data/usr/local/lib/x.dylib (compatibility version 1.0.0, current version 1.0.0)
\t@loader_path/../lib/libz.1.dylib (compatibility version 1.0.0, current version 1.3.1)
";
        let violations = check_otool_listing("x", listing, Path::new("/p"), Path::new("/b"));
        assert_eq!(violations.len(), 2, "{violations:?}");
        assert!(violations[0].reason.contains(".."), "{}", violations[0]);
        assert!(
            violations[1].reason.contains("firmlink"),
            "{}",
            violations[1]
        );
    }

    // The shape `otool -l` prints for the Pin Auditor's fixture
    // (`clang main.c -lfoo -Wl,-rpath,/usr/local/lib`).
    const LOAD_COMMANDS: &str = "\
wine/bin/rpath_exe:
Load command 12
          cmd LC_LOAD_DYLIB
      cmdsize 48
         name @rpath/libfoo.dylib (offset 24)
Load command 13
          cmd LC_RPATH
      cmdsize 32
         path /usr/local/lib (offset 12)
Load command 14
          cmd LC_RPATH
      cmdsize 40
         path @loader_path/../lib (offset 12)
Load command 15
          cmd LC_RPATH
      cmdsize 40
         path @executable_path/../lib (offset 12)
Load command 16
          cmd LC_RPATH
      cmdsize 48
         path /Users/ci/build/work/wine/stage/lib (offset 12)
Load command 17
          cmd LC_RPATH
      cmdsize 32
         path /usr/lib/swift (offset 12)
Load command 18
          cmd LC_RPATH
      cmdsize 32
         path @rpath/lib (offset 12)
";

    #[test]
    fn flags_absolute_rpaths_outside_the_system() {
        let violations = check_rpaths(
            "wine/bin/rpath_exe",
            LOAD_COMMANDS,
            Path::new("/Users/ci/payload"),
            Path::new("/Users/ci/build/work"),
        );
        let references: Vec<&str> = violations
            .iter()
            .map(|violation| violation.reference.as_str())
            .collect();
        assert_eq!(
            references,
            [
                "LC_RPATH /usr/local/lib",
                "LC_RPATH /Users/ci/build/work/wine/stage/lib",
                "LC_RPATH @rpath/lib",
            ]
        );
        assert!(
            violations[0].reason.contains("/usr/local"),
            "{}",
            violations[0]
        );
        assert!(
            violations[1].reason.contains("build directory"),
            "{}",
            violations[1]
        );
        assert!(
            violations
                .iter()
                .all(|violation| violation.binary == "wine/bin/rpath_exe")
        );
    }

    // A Unix module after the step 7 rewrite: its own LC_RPATH reaches
    // wine/lib two levels up.
    const MODULE_LOAD_COMMANDS: &str = "\
wine/lib/wine/x86_64-unix/ntdll.so:
Load command 0
          cmd LC_ID_DYLIB
         name @rpath/ntdll.so (offset 24)
Load command 1
          cmd LC_LOAD_DYLIB
         name @rpath/libfreetype.6.dylib (offset 24)
Load command 2
          cmd LC_LOAD_DYLIB
         name @loader_path/../../libgnutls.30.dylib (offset 24)
Load command 3
          cmd LC_LOAD_WEAK_DYLIB
         name @rpath/libmissing.dylib (offset 24)
Load command 4
          cmd LC_LOAD_DYLIB
         name @loader_path/../../../../../../escape.dylib (offset 24)
Load command 5
          cmd LC_LOAD_DYLIB
         name @executable_path/../lib/libz.1.dylib (offset 24)
Load command 6
          cmd LC_LOAD_DYLIB
         name /usr/lib/libSystem.B.dylib (offset 24)
Load command 7
          cmd LC_RPATH
         path @loader_path/../.. (offset 12)
Load command 8
          cmd LC_RPATH
         path /usr/local/lib (offset 12)
";

    // The loader: an executable whose own rpath reaches wine/lib.
    const LOADER_LOAD_COMMANDS: &str = "\
wine/bin/wine:
Load command 8
          cmd LC_MAIN
      cmdsize 24
Load command 9
          cmd LC_RPATH
         path @loader_path/../lib (offset 12)
";

    #[test]
    fn relative_references_must_resolve_to_payload_files() {
        let binary = "wine/lib/wine/x86_64-unix/ntdll.so";
        assert!(!is_executable(MODULE_LOAD_COMMANDS));
        assert!(is_executable(LOADER_LOAD_COMMANDS));
        let exe_dirs = vec![PathBuf::from("wine/bin")];
        let own = relocated_rpaths(binary, MODULE_LOAD_COMMANDS, &exe_dirs);
        assert_eq!(own, [PathBuf::from("wine/lib")]);
        let files = files(&[
            "wine/lib/libfreetype.6.dylib",
            "wine/lib/libgnutls.30.dylib",
            "wine/lib/wine/x86_64-unix/ntdll.so",
        ]);
        let resolves = |candidate: &Path| files.contains(candidate);
        let rpath_dirs: BTreeSet<PathBuf> = own.into_iter().collect();
        let search = Search {
            rpath_dirs: &rpath_dirs,
            exe_dirs: &exe_dirs,
            system_rpath: false,
        };
        let violations = check_references_resolve(binary, MODULE_LOAD_COMMANDS, &search, &resolves);
        assert_eq!(
            references(&violations),
            [
                "@rpath/libmissing.dylib",
                "@loader_path/../../../../../../escape.dylib",
                "@executable_path/../lib/libz.1.dylib",
            ],
            "{violations:?}"
        );
        assert!(
            violations[0].reason.contains("LC_RPATH"),
            "{}",
            violations[0]
        );
        assert!(
            violations
                .iter()
                .all(|violation| violation.binary == binary)
        );

        // A module with no LC_RPATH of its own resolves through the loader's.
        let bare = "wine/lib/wine/x86_64-unix/ntdll.so:\n\
Load command 1\n          cmd LC_LOAD_DYLIB\n         name @rpath/libfreetype.6.dylib (offset 24)\n";
        assert_eq!(
            relocated_rpaths(binary, bare, &exe_dirs),
            Vec::<PathBuf>::new()
        );
        let none = BTreeSet::new();
        let search = Search {
            rpath_dirs: &none,
            exe_dirs: &exe_dirs,
            system_rpath: false,
        };
        assert_eq!(
            check_references_resolve(binary, bare, &search, &resolves).len(),
            1
        );
        let rpath_dirs: BTreeSet<PathBuf> =
            relocated_rpaths("wine/bin/wine", LOADER_LOAD_COMMANDS, &exe_dirs)
                .into_iter()
                .collect();
        assert_eq!(rpath_dirs, BTreeSet::from([PathBuf::from("wine/lib")]));
        let search = Search {
            rpath_dirs: &rpath_dirs,
            exe_dirs: &exe_dirs,
            system_rpath: false,
        };
        assert_eq!(
            check_references_resolve(binary, bare, &search, &resolves),
            []
        );
    }

    // The Fix Verifier's `union` fixture: `app` names `@rpath/libfoo.dylib`
    // with no LC_RPATH, and an unrelated plugin's rpath is the only one that
    // reaches `deps/`. dyld fails with "no LC_RPATH's found".
    const UNION_APP: &str = "\
wine/bin/app:
Load command 10
          cmd LC_MAIN
Load command 11
          cmd LC_LOAD_DYLIB
         name @rpath/libfoo.dylib (offset 24)
Load command 12
          cmd LC_LOAD_DYLIB
         name /usr/lib/libSystem.B.dylib (offset 24)
";
    const UNION_PLUGIN: &str = "\
wine/lib/gst/libplug.dylib:
Load command 0
          cmd LC_ID_DYLIB
         name @rpath/libplug.dylib (offset 24)
Load command 1
          cmd LC_LOAD_DYLIB
         name /usr/lib/libSystem.B.dylib (offset 24)
Load command 2
          cmd LC_RPATH
         path @loader_path/../deps (offset 12)
";

    #[test]
    fn union_of_unrelated_rpaths_does_not_resolve_an_executable() {
        let files = files(&[
            "wine/bin/app",
            "wine/lib/deps/libfoo.dylib",
            "wine/lib/gst/libplug.dylib",
        ]);
        let resolves = |candidate: &Path| files.contains(candidate);
        let exe_dirs = vec![PathBuf::from("wine/bin")];
        let exe_rpaths: BTreeSet<PathBuf> = relocated_rpaths("wine/bin/app", UNION_APP, &exe_dirs)
            .into_iter()
            .collect();
        assert!(exe_rpaths.is_empty());
        let plugin_rpaths: BTreeSet<PathBuf> =
            relocated_rpaths("wine/lib/gst/libplug.dylib", UNION_PLUGIN, &exe_dirs)
                .into_iter()
                .collect();
        assert_eq!(
            plugin_rpaths,
            BTreeSet::from([PathBuf::from("wine/lib/deps")])
        );

        // The plugin's rpath would resolve the reference; it is not searched.
        let search = Search {
            rpath_dirs: &exe_rpaths,
            exe_dirs: &exe_dirs,
            system_rpath: false,
        };
        let violations = check_references_resolve("wine/bin/app", UNION_APP, &search, &resolves);
        assert_eq!(references(&violations), ["@rpath/libfoo.dylib"]);
        let search = Search {
            rpath_dirs: &plugin_rpaths,
            exe_dirs: &exe_dirs,
            system_rpath: false,
        };
        assert_eq!(
            check_references_resolve(
                "wine/lib/gst/libplug.dylib",
                UNION_PLUGIN,
                &search,
                &resolves
            ),
            []
        );
    }

    // The Fix Verifier's `exe-good` and `exe-bad` fixtures: `app` loads a
    // Unix module that depends on `@executable_path/../lib/libbar.dylib`.
    // dyld resolves it against `wine/bin`, so `wine/lib/libbar.dylib` loads
    // and `wine/lib/wine/lib/libbar.dylib` (next to the module) does not.
    const EXE_APP: &str = "\
wine/bin/app:
Load command 10
          cmd LC_MAIN
Load command 11
          cmd LC_LOAD_DYLIB
         name @loader_path/../lib/wine/unix/libmod.dylib (offset 24)
";
    const EXE_MODULE: &str = "\
wine/lib/wine/unix/libmod.dylib:
Load command 0
          cmd LC_ID_DYLIB
         name @loader_path/../lib/wine/unix/libmod.dylib (offset 24)
Load command 1
          cmd LC_LOAD_DYLIB
         name @executable_path/../lib/libbar.dylib (offset 24)
";

    fn module_violations(files: &BTreeSet<PathBuf>) -> Vec<super::Violation> {
        let resolves = |candidate: &Path| files.contains(candidate);
        let exe_dirs = vec![PathBuf::from("wine/bin")];
        let none = BTreeSet::new();
        let search = Search {
            rpath_dirs: &none,
            exe_dirs: &exe_dirs,
            system_rpath: false,
        };
        let mut violations = check_references_resolve("wine/bin/app", EXE_APP, &search, &resolves);
        violations.extend(check_references_resolve(
            "wine/lib/wine/unix/libmod.dylib",
            EXE_MODULE,
            &search,
            &resolves,
        ));
        violations
    }

    #[test]
    fn exe_good_executable_path_resolves_against_the_executable() {
        let files = files(&[
            "wine/bin/app",
            "wine/lib/libbar.dylib",
            "wine/lib/wine/unix/libmod.dylib",
        ]);
        assert_eq!(module_violations(&files), []);
    }

    #[test]
    fn exe_bad_executable_path_does_not_resolve_against_the_dylib() {
        let files = files(&[
            "wine/bin/app",
            "wine/lib/wine/lib/libbar.dylib",
            "wine/lib/wine/unix/libmod.dylib",
        ]);
        let violations = module_violations(&files);
        assert_eq!(
            references(&violations),
            ["@executable_path/../lib/libbar.dylib"],
            "{violations:?}"
        );
        assert_eq!(violations[0].binary, "wine/lib/wine/unix/libmod.dylib");
        assert!(
            violations[0].reason.contains("executable"),
            "{}",
            violations[0]
        );

        // Without any executable the reference has nothing to resolve against.
        let resolves = |candidate: &Path| files.contains(candidate);
        let none = BTreeSet::new();
        let search = Search {
            rpath_dirs: &none,
            exe_dirs: &[],
            system_rpath: false,
        };
        let violations = check_references_resolve(
            "wine/lib/wine/unix/libmod.dylib",
            EXE_MODULE,
            &search,
            &resolves,
        );
        assert_eq!(violations.len(), 1);
        assert!(
            violations[0].reason.contains("no executable"),
            "{}",
            violations[0]
        );
    }

    #[test]
    fn executable_path_rpaths_relocate_against_the_executables() {
        // GStreamer's shape: a plugin with `@executable_path/../lib` next to
        // `@loader_path/..`, in a payload whose executables are elsewhere.
        let plugin = "\
wine/lib/GStreamer.framework/Versions/1.0/lib/gstreamer-1.0/libgstx.dylib:
Load command 1
          cmd LC_RPATH
         path @executable_path/../lib (offset 12)
Load command 2
          cmd LC_RPATH
         path @loader_path/.. (offset 12)
Load command 3
          cmd LC_RPATH
         path @executable_path/../../../../../.. (offset 12)
";
        let exe_dirs = vec![PathBuf::from("wine/bin"), PathBuf::from("probes")];
        let dirs = relocated_rpaths(
            "wine/lib/GStreamer.framework/Versions/1.0/lib/gstreamer-1.0/libgstx.dylib",
            plugin,
            &exe_dirs,
        );
        assert_eq!(
            dirs,
            [
                PathBuf::from("wine/lib"),
                PathBuf::from("lib"),
                PathBuf::from("wine/lib/GStreamer.framework/Versions/1.0/lib"),
            ]
        );
    }

    #[test]
    fn a_system_rpath_leaves_rpath_references_to_dyld() {
        // LOAD_COMMANDS carries `LC_RPATH /usr/lib/swift`, which dyld
        // searches from the shared cache; the payload cannot prove the
        // reference unresolved.
        assert!(has_system_rpath(LOAD_COMMANDS));
        assert!(!has_system_rpath(MODULE_LOAD_COMMANDS));
        let resolves = |_: &Path| false;
        let exe_dirs = vec![PathBuf::from("wine/bin")];
        let none = BTreeSet::new();
        let search = Search {
            rpath_dirs: &none,
            exe_dirs: &exe_dirs,
            system_rpath: has_system_rpath(LOAD_COMMANDS),
        };
        assert_eq!(
            check_references_resolve("wine/bin/rpath_exe", LOAD_COMMANDS, &search, &resolves),
            []
        );
        let search = Search {
            rpath_dirs: &none,
            exe_dirs: &exe_dirs,
            system_rpath: false,
        };
        assert_eq!(
            check_references_resolve("wine/bin/rpath_exe", LOAD_COMMANDS, &search, &resolves).len(),
            1
        );
    }

    #[test]
    fn a_prefix_match_needs_a_path_boundary() {
        let listing =
            "x:\n\t/usr/localfoo/lib.dylib (compatibility version 1.0.0, current version 1.0.0)\n";
        let violations = check_otool_listing("x", listing, Path::new("/p"), Path::new("/b"));
        assert_eq!(violations.len(), 1);
        assert_eq!(
            violations[0].reason,
            "absolute path outside /System and /usr/lib"
        );
    }
}
