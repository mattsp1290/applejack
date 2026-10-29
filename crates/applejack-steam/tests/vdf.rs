//! The reader and writer round-trip every fixture byte for byte, and the
//! reader accepts the forms Valve's parser accepts that the writer never emits.

use std::fs;
use std::path::PathBuf;

use applejack_steam::Error;
use applejack_steam::vdf::{Document, MAX_DEPTH, Node, Value};

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

#[test]
fn fixtures_round_trip_byte_for_byte() {
    for name in ["appmanifest_000000.acf", "libraryfolders.vdf", "config.vdf"] {
        let text = fixture(name);
        let document = Document::parse(&text).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(document.to_text(), text, "{name} did not round-trip");
    }
}

#[test]
fn typed_accessors_follow_paths_case_insensitively() {
    let document = Document::parse(&fixture("libraryfolders.vdf")).unwrap();
    assert_eq!(
        document.get_str("LibraryFolders/1/label"),
        Some("applejack")
    );
    assert_eq!(
        document.get_u64("libraryfolders/0/apps/0"),
        Some(123_456_789)
    );
    assert_eq!(document.get_u32("libraryfolders/0/contentid"), Some(0));
    assert_eq!(
        document.get_str("libraryfolders/0/path"),
        Some(r"C:\Program Files (x86)\Steam")
    );
    assert_eq!(document.get_map("libraryfolders/1/apps"), Some(&[][..]));
    assert_eq!(document.get_str("libraryfolders/1/apps"), None);
    assert_eq!(document.get("libraryfolders/9"), None);
    assert_eq!(document.get("missing"), None);
}

#[test]
fn reader_accepts_comments_bare_tokens_conditionals_and_escapes() {
    let text = "// leading comment\n\"root\" // trailing\n{\n\tbare value\n\t\"quoted\" \"a \\\"b\\\" \\\\ c\\n\" [$WIN32]\n\t\"nested\" { \"k\" \"v\" }\n}\n";
    let document = Document::parse(text).unwrap();
    assert_eq!(document.get_str("root/bare"), Some("value"));
    assert_eq!(document.get_str("root/quoted"), Some("a \"b\" \\ c\n"));
    assert_eq!(document.get_str("root/nested/k"), Some("v"));
    assert_eq!(
        document.to_text(),
        "\"root\"\n{\n\t\"bare\"\t\t\"value\"\n\t\"quoted\"\t\t\"a \\\"b\\\" \\\\ c\\n\"\n\t\"nested\"\n\t{\n\t\t\"k\"\t\t\"v\"\n\t}\n}\n"
    );
}

#[test]
fn duplicate_keys_are_kept_in_order_and_first_wins_on_lookup() {
    let document = Document::parse("\"r\"\n{\n\t\"k\"\t\t\"1\"\n\t\"k\"\t\t\"2\"\n}\n").unwrap();
    let Some(Value::Map(children)) = document.get("r") else {
        panic!("root is a map");
    };
    assert_eq!(children, &[Node::str("k", "1"), Node::str("k", "2")]);
    assert_eq!(document.get_str("r/k"), Some("1"));
}

#[test]
fn malformed_text_reports_the_line() {
    let error = Document::parse("\"r\"\n{\n\t\"k\"\n").unwrap_err();
    assert!(
        error.to_string().starts_with("vdf: line 4:"),
        "unexpected message: {error}"
    );
    assert!(Document::parse("}").is_err());
    assert!(Document::parse("\"k\" \"unterminated").is_err());
    assert!(Document::parse("{ \"k\" \"v\" }").is_err());
}

#[test]
fn nesting_depth_64_parses_and_65_is_rejected() {
    let nested = |levels: usize| format!("{}{}", "\"a\"{".repeat(levels), "}".repeat(levels));
    assert_eq!(MAX_DEPTH, 64);
    assert!(Document::parse(&nested(64)).is_ok());
    let error = Document::parse(&nested(65)).unwrap_err();
    assert!(matches!(error, Error::VdfParse { .. }), "{error}");
    assert_eq!(
        error.to_string(),
        "vdf: line 1: nesting deeper than 64 levels"
    );
}

#[test]
fn conditional_brackets_follow_a_value_or_a_block_key_only() {
    // `a[b]` is one bare token; ` [b]` after a value is a conditional. That
    // asymmetry is Valve's, pinned here so a tokenizer change cannot regress it.
    let text = "\"r\"\n{\n\ta[b] v\n\tk v [b]\n\t\"q\" \"w\"[$WIN32]\n\t\"blk\" [!$OSX]\n\t{\n\t\t\"x\" \"y\"\n\t}\n}\n";
    let document = Document::parse(text).unwrap();
    assert_eq!(document.get_str("r/a[b]"), Some("v"));
    assert_eq!(document.get_str("r/k"), Some("v"));
    assert_eq!(document.get_str("r/q"), Some("w"));
    assert_eq!(document.get_str("r/blk/x"), Some("y"));
    assert_eq!(
        document.to_text(),
        "\"r\"\n{\n\t\"a[b]\"\t\t\"v\"\n\t\"k\"\t\t\"v\"\n\t\"q\"\t\t\"w\"\n\t\"blk\"\n\t{\n\t\t\"x\"\t\t\"y\"\n\t}\n}\n"
    );

    // A `[` that starts a token is a bare key, not a conditional, even on the
    // line after a value.
    let document = Document::parse("\"k\" \"v\"\n[abc] w\n").unwrap();
    assert_eq!(document.get_str("k"), Some("v"));
    assert_eq!(document.get_str("[abc]"), Some("w"));

    // A conditional after a key must be followed by `{`, and the error names
    // the key's line rather than one further down.
    for text in [
        "\"r\"\n{\n\t\"k\" [abc] \"v\"\n}\n",
        "\"r\"\n{\n\t\"k\" [abc]\n\t\"v2\" \"x\"\n}\n",
    ] {
        let error = Document::parse(text).unwrap_err();
        assert_eq!(
            error.to_string(),
            "vdf: line 3: conditional after key `k` must be followed by `{`"
        );
    }
    assert!(Document::parse("\"k\" \"v\" [unterminated\n").is_err());
}

#[test]
fn top_level_base_and_include_directives_are_rejected() {
    for text in [
        "#base \"other.vdf\"\n\"r\"\n{\n}\n",
        "#include \"other.vdf\"\n",
        "\"#Base\" \"other.vdf\"\n",
    ] {
        let error = Document::parse(text).unwrap_err();
        assert!(matches!(error, Error::VdfParse { line: 1, .. }), "{error}");
        assert!(error.to_string().contains("not supported"), "{error}");
    }
    // Below the top level the same text is an ordinary key.
    let document = Document::parse("\"r\"\n{\n\t\"#base\"\t\t\"x\"\n}\n").unwrap();
    assert_eq!(document.get_str("r/#base"), Some("x"));
}

#[test]
fn leading_bom_is_stripped() {
    let document = Document::parse("\u{feff}\"root\"\n{\n\t\"k\"\t\t\"v\"\n}\n").unwrap();
    assert_eq!(document.get_str("root/k"), Some("v"));
    assert_eq!(document.to_text(), "\"root\"\n{\n\t\"k\"\t\t\"v\"\n}\n");
}

#[test]
fn unknown_escapes_are_rejected() {
    let error = Document::parse("\"k\" \"a\\qb\"\n").unwrap_err();
    assert_eq!(error.to_string(), "vdf: line 1: unknown escape `\\q`");
    let document = Document::parse("\"k\" \"a\\\\qb\"\n").unwrap();
    assert_eq!(document.get_str("k"), Some("a\\qb"));
    assert_eq!(document.to_text(), "\"k\"\t\t\"a\\\\qb\"\n");
}

#[test]
fn raw_control_characters_inside_quotes_are_written_back_escaped() {
    // Valve's writer escapes these, so a client file is unaffected; a
    // hand-edited one is normalised rather than kept byte for byte.
    let document = Document::parse("\"k\" \"a\tb\r\nc\"\n").unwrap();
    assert_eq!(document.get_str("k"), Some("a\tb\r\nc"));
    assert_eq!(document.to_text(), "\"k\"\t\t\"a\\tb\\r\\nc\"\n");
}

#[test]
fn crlf_input_parses_and_is_written_back_with_lf() {
    let lf = fixture("libraryfolders.vdf");
    let crlf = lf.replace('\n', "\r\n");
    let document = Document::parse(&crlf).unwrap();
    assert_eq!(document, Document::parse(&lf).unwrap());
    assert_eq!(document.to_text(), lf);
}
