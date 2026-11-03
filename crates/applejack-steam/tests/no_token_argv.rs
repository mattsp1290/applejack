//! No token ever rides on a process command line or reaches a print: this
//! crate constructs no `std::process::Command` at all. `clippy.toml`
//! (`disallowed-types`) is the type-resolved guard that an import alias cannot
//! bypass; this grep is the textual backstop. The grep walks `src/`, so the
//! session, library, and depot modules are covered as they are added. The
//! print scan also reads the code that handles a password or a code on its
//! way here: the core's RPC and Steam modules and the CLI's commands. It
//! counts `format!`, since `CoreError::Message` and `say` print what it
//! builds.

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

/// The CLI's own print, a function, so it needs a look at what comes before.
const SAY: &str = "say(";

const PRINTS: &[&str] = &[
    SAY,
    "format!",
    "println!",
    "print!",
    "dbg!",
    "panic!",
    "write!",
    "writeln!",
    "tracing::",
];

const SECRET_NAMES: &[&str] = &[
    "expose",
    "key",
    "credentials",
    "password",
    "secret",
    "token",
    "tokens",
    "value",
    "access_token",
    "cdn_token",
    "depot_key",
    "refresh_token",
    "steam_guard_code",
];

/// What a guard code is called where one is handled. Too common a set of
/// words to hold anywhere else.
const GUARD_NAMES: &[&str] = &["code", "line", "text", "typed"];

/// Fine in a string that is built, and only there: nearly every `format!`
/// has one, and no secret goes by these names alone.
const GENERIC_NAMES: &[&str] = &["key", "value"];

/// The CDN's auth token, which belongs in the request URL it builds.
const ALLOWED: &[&str] = &[r#"format!("?{token}")"#];

/// Every print call in `text`, parentheses balanced, so a call split across
/// lines is seen whole. Panics on a call it cannot read, such as a brace form
/// or a raw string holding a quote, so that nothing goes unscanned quietly.
fn print_calls(text: &str) -> Vec<&str> {
    let mut calls = Vec::new();
    for print in PRINTS {
        for (start, _) in text.match_indices(print) {
            if *print == SAY && !calls_say(&text[..start]) {
                continue;
            }
            let after = &text[start + print.len()..];
            assert!(
                !print.ends_with('!') || after.starts_with('('),
                "unreadable call: {}",
                &text[start..]
            );
            let found = calls.len();
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
            assert_eq!(
                calls.len(),
                found + 1,
                "unreadable call: {}",
                &text[start..]
            );
        }
    }
    calls
}

/// Whether a `say(` after `before` is a call: not `fn say(`, and not the end
/// of a longer name.
fn calls_say(before: &str) -> bool {
    !before.ends_with("fn ") && !before.ends_with(|c: char| c.is_alphanumeric() || c == '_')
}

/// Splits `call` at its unescaped quotes: odd pieces are literals.
fn pieces(call: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut start = 0;
    let mut escaped = false;
    for (offset, character) in call.char_indices() {
        match character {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => {
                found.push(&call[start..offset]);
                start = offset + 1;
            }
            _ => {}
        }
    }
    found.push(&call[start..]);
    found
}

/// The first secret name a call mentions, in its code or as a `{name:?}`. The
/// rest of a literal is printed as written, so it can say `password`.
/// `guard_code` says the file handles one.
fn secret_in(call: &str, guard_code: bool) -> Option<&str> {
    if ALLOWED.contains(&call) {
        return None;
    }
    let built = call.starts_with("format!") || call.starts_with(SAY);
    let is_secret = |word: &&str| {
        let word = word.to_lowercase();
        let word = word.as_str();
        (SECRET_NAMES.contains(&word) && !(built && GENERIC_NAMES.contains(&word)))
            || (guard_code && GUARD_NAMES.contains(&word))
    };
    let separator = |character: char| !character.is_alphanumeric() && character != '_';
    pieces(call)
        .into_iter()
        .enumerate()
        .find_map(|(index, piece)| {
            if index % 2 == 0 {
                piece.split(separator).find(is_secret)
            } else {
                piece
                    .split('{')
                    .skip(1)
                    .filter_map(|hole| hole.split([':', '}']).next())
                    .find(is_secret)
            }
        })
}

#[test]
fn the_print_scan_sees_split_calls_and_inline_captures() {
    let split = "println!(\n    \"{}\",\n    token.expose()\n);\nlet other = 1;";
    assert_eq!(print_calls(split).len(), 1);
    assert_eq!(secret_in(print_calls(split)[0], false), Some("token"));

    let inline = r#"println!("{key:?} (done)");"#;
    assert_eq!(secret_in(print_calls(inline)[0], false), Some("key"));

    let tracing = r#"tracing::warn!(cdn_token = %x, "(oops");"#;
    assert_eq!(secret_in(print_calls(tracing)[0], false), Some("cdn_token"));

    let clean = r#"println!("installed: {} files", outcome.files_written);"#;
    assert_eq!(secret_in(print_calls(clean)[0], false), None);

    assert_eq!(
        secret_in("println!(\"{:?}\", Credentials)", false),
        Some("Credentials")
    );
}

/// Each of these reached a print with the scan green: `say` is how the CLI
/// prints, `fail` prints a `CoreError`, and `line` is the CLI's guard code.
#[test]
fn the_print_scan_sees_the_routes_the_cli_prints_by() {
    for (call, name) in [
        (r#"say(&format!("{password}"));"#, "password"),
        ("say(password.expose());", "password"),
        (
            r#"CoreError::Message(format!("rejected {password}"))"#,
            "password",
        ),
        (r#"println!("{line}");"#, "line"),
        (r#"say(&format!("{text:?}"));"#, "text"),
        ("say(typed);", "typed"),
        (r#"format!("{}", code)"#, "code"),
    ] {
        let calls = print_calls(call);
        assert!(
            calls.iter().any(|seen| secret_in(seen, true) == Some(name)),
            "{call}: {calls:?}"
        );
    }
}

/// `fn say` is no call, `essay(` is another name, and a message may say the
/// word `password`: a literal is printed as written.
#[test]
fn the_print_scan_reads_only_names() {
    assert_eq!(print_calls("fn say(message: &str) {}"), [] as [&str; 0]);
    assert_eq!(print_calls("essay(password);"), [] as [&str; 0]);
    assert_eq!(
        secret_in(r#"say("Type the Steam Guard code");"#, true),
        None
    );
    assert_eq!(
        secret_in(r#"format!("cannot read the password: {error}")"#, true),
        None
    );
    assert_eq!(
        secret_in(r#"say("a \" {password}");"#, false),
        Some("password")
    );
}

/// Guard names count only where a guard code goes, and `key` and `value` only
/// in a print: elsewhere they are every config line and VDF parser message.
#[test]
fn the_print_scan_keeps_common_words_to_where_they_can_be_secrets() {
    assert_eq!(secret_in(r#"println!("{line}");"#, false), None);
    assert_eq!(secret_in(r#"format!("{key} = {value:?}")"#, false), None);
    assert_eq!(secret_in(r#"println!("{value}");"#, false), Some("value"));
    assert_eq!(secret_in(r#"format!("?{token}")"#, false), None);
    assert_eq!(secret_in(r#"format!("{token}")"#, false), Some("token"));
}

#[test]
#[should_panic(expected = "unreadable call")]
fn the_print_scan_refuses_a_call_it_cannot_read() {
    print_calls("println!{\"{}\", token};");
}

#[test]
#[should_panic(expected = "unreadable call")]
fn the_print_scan_refuses_a_raw_string_with_a_quote() {
    print_calls("println!(r#\"a \" b\"#, token); let a = 1;");
}

#[test]
fn no_print_or_debug_format_touches_a_token() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut files = vec![
        root.join("../applejack-core/src/install.rs"),
        root.join("../applejack-core/src/steam.rs"),
    ];
    rust_files(&root.join("src"), &mut files);
    rust_files(&root.join("../applejack-core/src/rpc"), &mut files);
    rust_files(&root.join("../applejack/src/cmd"), &mut files);
    let guard_files = [
        root.join("src/auth.rs"),
        root.join("src/live.rs"),
        root.join("../applejack-core/src/steam.rs"),
        root.join("../applejack/src/cmd/steam.rs"),
    ];
    for path in files {
        let guard_code = guard_files.contains(&path);
        let text = fs::read_to_string(&path).unwrap();
        let code: Vec<&str> = text
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect();
        for call in print_calls(&code.join("\n")) {
            if let Some(name) = secret_in(call, guard_code) {
                panic!("{}: `{call}` mentions `{name}`", path.display());
            }
        }
    }
}

#[test]
fn crate_sources_build_no_commands() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
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
