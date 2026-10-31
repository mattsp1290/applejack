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

const PRINTS: &[&str] = &["println!", "print!", "dbg!", "tracing::"];

const SECRET_NAMES: &[&str] = &[
    "expose",
    "key",
    "password",
    "token",
    "value",
    "access_token",
    "cdn_token",
    "depot_key",
    "refresh_token",
];

/// Every print call in `text`, parentheses balanced, so a call split across
/// lines is seen whole.
fn print_calls(text: &str) -> Vec<&str> {
    let mut calls = Vec::new();
    for print in PRINTS {
        for (start, _) in text.match_indices(print) {
            let mut depth = 0;
            let mut quoted = false;
            let mut escaped = false;
            for (offset, character) in text[start..].char_indices() {
                match character {
                    _ if escaped => escaped = false,
                    '\\' => escaped = quoted,
                    '"' => quoted = !quoted,
                    '(' if !quoted => depth += 1,
                    ')' if !quoted => {
                        depth -= 1;
                        if depth == 0 {
                            calls.push(&text[start..=start + offset]);
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    calls
}

/// The first secret name a call mentions, in its code or as a `{name:?}`.
fn secret_in(call: &str) -> Option<&str> {
    call.split(|character: char| !character.is_alphanumeric() && character != '_')
        .find(|word| SECRET_NAMES.contains(word))
}

#[test]
fn the_print_scan_sees_split_calls_and_inline_captures() {
    let split = "println!(\n    \"{}\",\n    token.expose()\n);\nlet other = 1;";
    assert_eq!(print_calls(split).len(), 1);
    assert_eq!(secret_in(print_calls(split)[0]), Some("token"));

    let inline = r#"println!("{key:?} (done)");"#;
    assert_eq!(secret_in(print_calls(inline)[0]), Some("key"));

    let tracing = r#"tracing::warn!(cdn_token = %x, "(oops");"#;
    assert_eq!(secret_in(print_calls(tracing)[0]), Some("cdn_token"));

    let clean = r#"println!("installed: {} files", outcome.files_written);"#;
    assert_eq!(secret_in(print_calls(clean)[0]), None);
}

#[test]
fn no_print_or_debug_format_touches_a_token() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    rust_files(&root.join("examples"), &mut files);
    for path in files {
        let text = fs::read_to_string(&path).unwrap();
        for call in print_calls(&text) {
            if let Some(name) = secret_in(call) {
                panic!("{}: `{call}` mentions `{name}`", path.display());
            }
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
