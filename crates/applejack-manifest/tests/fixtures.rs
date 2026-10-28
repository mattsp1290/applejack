//! Every file under `tests/fixtures/` is accounted for here: `valid.toml`
//! parses and validates, every other TOML file fails for the reason its name
//! states, and any file not listed fails the suite.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use applejack_manifest::{Error, Manifest};

const REQUIRED_FIELDS: &[(&str, &[&str])] = &[
    (
        "engine",
        &[
            "id",
            "version",
            "arch",
            "min_macos",
            "url",
            "sha256",
            "components",
            "notarization_submission_id",
        ],
    ),
    ("tool", &["id", "version", "url", "sha256", "runs_on"]),
    (
        "probe",
        &[
            "id",
            "version",
            "url",
            "sha256",
            "arch",
            "runs_in",
            "ships_in_bundle",
        ],
    ),
];

const OTHER_FILES: &[&str] = &[
    "generate.sh",
    "valid.toml",
    "bad-sha256.toml",
    "bad-url.toml",
    "duplicate-id.toml",
];

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn read(name: &str) -> String {
    let path = fixtures_dir().join(name);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn expected_names() -> BTreeSet<String> {
    let missing = REQUIRED_FIELDS.iter().flat_map(|(table, fields)| {
        fields
            .iter()
            .map(move |field| format!("missing-{table}-{field}.toml"))
    });
    OTHER_FILES
        .iter()
        .map(|name| (*name).to_owned())
        .chain(missing)
        .collect()
}

fn parse_error_path(name: &str) -> String {
    let error = Manifest::parse(&read(name)).expect_err(name);
    match error {
        Error::Parse {
            path: Some(path), ..
        } => path,
        other => panic!("{name}: expected a located parse error, got {other}"),
    }
}

#[test]
fn fixture_directory_is_exactly_the_expected_set() {
    let actual: BTreeSet<String> = fs::read_dir(fixtures_dir())
        .expect("fixtures directory")
        .map(|entry| entry.expect("directory entry").file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    assert_eq!(actual, expected_names());
}

#[test]
fn valid_parses_and_validates() {
    let manifest = Manifest::parse(&read("valid.toml")).expect("valid.toml parses");
    manifest.validate().expect("valid.toml validates");
    assert_eq!(manifest.engine.len(), 1);
    assert_eq!(manifest.tool.len(), 1);
    assert_eq!(manifest.probe.len(), 1);
}

#[test]
fn every_missing_field_fixture_names_the_field() {
    for (table, fields) in REQUIRED_FIELDS {
        for field in *fields {
            let name = format!("missing-{table}-{field}.toml");
            let error = Manifest::parse(&read(&name)).expect_err(&name);
            let rendered = error.to_string();
            assert!(
                rendered.contains(&format!("`{table}[0]`"))
                    && rendered.contains(&format!("missing field `{field}`")),
                "{name}: {rendered}"
            );
        }
    }
}

#[test]
fn bad_sha256_is_rejected_at_the_field() {
    assert_eq!(parse_error_path("bad-sha256.toml"), "engine[0].sha256");
}

#[test]
fn bad_url_is_rejected_at_the_field() {
    assert_eq!(parse_error_path("bad-url.toml"), "engine[0].url");
}

#[test]
fn duplicate_id_parses_but_fails_validation() {
    let manifest = Manifest::parse(&read("duplicate-id.toml")).expect("duplicate-id.toml parses");
    let error = manifest.validate().expect_err("duplicate ids");
    match error {
        Error::DuplicateId {
            table,
            id,
            first,
            second,
        } => {
            assert_eq!(
                (table, id.as_str(), first, second),
                ("tool", "depotdownloader", 0, 1)
            );
        }
        other => panic!("expected DuplicateId, got {other}"),
    }
}
