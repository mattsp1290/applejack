//! The reader and writer round-trip every fixture byte for byte, and the
//! reader accepts the forms Valve's parser accepts that the writer never emits.

use std::fs;
use std::path::PathBuf;

use applejack_steam::vdf::{Document, Node, Value};

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
