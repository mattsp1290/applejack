//! The reader against the build side's fixture. `xtask/tests/fixtures/BUNDLE.toml`
//! is what `cargo xtask engine-payload --write-bundle-toml` renders from
//! `build/bundle-spec.toml` (xtask's `the_writer_reproduces_the_committed_bundle_fixture`
//! keeps the two equal), and the installer parses the same text from every
//! payload. xtask does not depend on this crate, so the gate lives here.

use applejack_engine::bundle::BundleToml;

const XTASK_FIXTURE: &str = include_str!("../../../xtask/tests/fixtures/BUNDLE.toml");

#[test]
fn the_reader_parses_the_xtask_fixture() {
    let bundle = BundleToml::parse(XTASK_FIXTURE).expect("the xtask fixture parses");
    for (key, version, license) in [
        ("freetype", "2.13.3", "FTL"),
        ("proxy-libintl", "1", "LGPL-2.0-or-later"),
        ("gnutls", "3.8.10", "LGPL-2.1-or-later"),
    ] {
        let component = bundle
            .components
            .get(key)
            .unwrap_or_else(|| panic!("no [components.{key}] in the xtask fixture"));
        assert_eq!(
            (component.version.as_str(), component.license.as_str()),
            (version, license),
            "[components.{key}]"
        );
    }
}
