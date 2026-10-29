//! No token ever rides on a process command line: this crate constructs no
//! `std::process::Command` at all, and the spike example does not either.

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
