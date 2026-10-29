//! Self-containment check: every Mach-O in the payload may link only against
//! the system (`/System`, `/usr/lib`) or relocatable references inside the
//! payload. The hardened runtime drops `DYLD_*`, so anything else fails at
//! load time on a machine without the build host's Homebrew.

use std::fmt;
use std::path::Path;

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
    let mut in_rpath = false;
    load_commands
        .lines()
        .map(str::trim)
        .filter_map(move |line| {
            if let Some(command) = line.strip_prefix("cmd ") {
                in_rpath = command.trim() == "LC_RPATH";
                return None;
            }
            if !in_rpath {
                return None;
            }
            let rest = line.strip_prefix("path ")?;
            let path = rest.rsplit_once(" (offset ").map_or(rest, |(path, _)| path);
            Some(path.trim())
        })
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
    if SYSTEM_ROOTS.iter().any(|root| reference.starts_with(root)) {
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

fn starts_with_dir(reference: &str, dir: &str) -> bool {
    let dir = dir.trim_end_matches('/');
    !dir.is_empty()
        && reference
            .strip_prefix(dir)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{check_otool_listing, check_rpaths};

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
