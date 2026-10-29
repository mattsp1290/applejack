use std::fs;
use std::path::PathBuf;

use applejack_steam::acf::{AcfWriter, AppManifest, InstalledDepot, STATE_FLAGS_FULLY_INSTALLED};

fn fixture() -> String {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/appmanifest_000000.acf");
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

#[test]
fn parse_then_write_equals_the_fixture() {
    let text = fixture();
    let manifest = AppManifest::parse(&text).unwrap();
    assert_eq!(AcfWriter::new(&manifest).write(), text);
}

#[test]
fn parse_reads_every_field() {
    let manifest = AppManifest::parse(&fixture()).unwrap();
    assert_eq!(
        manifest,
        AppManifest {
            appid: 0,
            name: "Redacted Title".to_owned(),
            installdir: "Redacted Title".to_owned(),
            state_flags: STATE_FLAGS_FULLY_INSTALLED,
            buildid: 1,
            last_updated: 1_700_000_000,
            size_on_disk: 123_456_789,
            installed_depots: vec![
                InstalledDepot {
                    depot_id: 1,
                    manifest_id: 1_111_111_111_111_111_111,
                    size: 123_456_789,
                },
                InstalledDepot {
                    depot_id: 2,
                    manifest_id: 2_222_222_222_222_222_222,
                    size: 0,
                },
            ],
        }
    );
    assert!(manifest.is_fully_installed());
    assert_eq!(manifest.file_name(), "appmanifest_0.acf");
}

#[test]
fn state_flags_other_than_four_are_not_fully_installed() {
    let text = fixture().replace("\"StateFlags\"\t\t\"4\"", "\"StateFlags\"\t\t\"1026\"");
    let manifest = AppManifest::parse(&text).unwrap();
    assert_eq!(manifest.state_flags, 1026);
    assert!(!manifest.is_fully_installed());
}

#[test]
fn missing_and_malformed_fields_name_their_path() {
    let missing = AppManifest::parse("\"AppState\"\n{\n\t\"appid\"\t\t\"0\"\n}\n").unwrap_err();
    assert_eq!(missing.to_string(), "acf: field `name`: missing");

    let bad = fixture().replace(
        "\"manifest\"\t\t\"1111111111111111111\"",
        "\"manifest\"\t\t\"x\"",
    );
    let error = AppManifest::parse(&bad).unwrap_err();
    assert_eq!(
        error.to_string(),
        "acf: field `InstalledDepots/1/manifest`: not an unsigned integer"
    );

    let no_root = AppManifest::parse("\"Other\"\n{\n}\n").unwrap_err();
    assert_eq!(no_root.to_string(), "acf: field `AppState`: missing");
}
