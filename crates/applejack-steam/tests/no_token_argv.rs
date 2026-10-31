//! No token ever rides on a process command line or reaches a print: this
//! crate constructs no `std::process::Command` at all, and the spike examples
//! do not either. `clippy.toml` (`disallowed-types`) is the type-resolved
//! guard that an import alias cannot bypass; this grep is the textual
//! backstop. The grep walks `src/` and `examples/`, so the session, library,
//! and depot modules and `depot_spike` are covered as they are added.

use std::fs;
use std::path::{Path, PathBuf};

const FORBIDDEN: &[&str] = &[
    "process::Command",
    "Command::new",
    "security find-",
    "security add-",
];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// A same-line backstop: a call split across lines is not seen.
#[test]
fn no_print_or_debug_format_touches_a_token() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    rust_files(&root.join("examples"), &mut files);
    for path in files {
        let text = fs::read_to_string(&path).unwrap();
        for line in text.lines() {
            let printing = ["println!", "eprintln!", "print!(", "tracing::", "dbg!"]
                .iter()
                .any(|call| line.contains(call));
            let secret = ["expose()", "refresh_token", "access_token", "depot_key"]
                .iter()
                .any(|name| line.contains(name));
            assert!(
                !(printing && secret),
                "{}: `{}` prints a secret",
                path.display(),
                line.trim()
            );
        }
    }
}

#[test]
fn crate_sources_and_examples_build_no_commands() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    rust_files(&root.join("examples"), &mut files);
    assert_ne!(files.len(), 0, "no Rust sources found");
    for path in files {
        let text = fs::read_to_string(&path).unwrap();
        for needle in FORBIDDEN {
            assert!(
                !text.contains(needle),
                "{} contains `{needle}`; tokens must never reach argv",
                path.display()
            );
        }
    }
}
