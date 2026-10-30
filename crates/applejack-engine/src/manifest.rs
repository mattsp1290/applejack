//! The `engines/manifest.toml` compiled into this crate.

use applejack_manifest::Manifest;

use crate::Result;

// Embedded so the pins travel inside the signed binary and change only with a
// release.
const EMBEDDED_MANIFEST: &str = include_str!("../../../engines/manifest.toml");

/// Parses and validates the `engines/manifest.toml` compiled into this crate.
///
/// # Errors
///
/// Returns the parse or validation error. `cargo xtask manifest-verify`
/// rejects such a manifest before it can ship.
pub fn embedded_manifest() -> Result<Manifest> {
    parse(EMBEDDED_MANIFEST)
}

/// Parses and validates manifest text with the same rules as
/// [`embedded_manifest`].
///
/// # Errors
///
/// Returns the parse or validation error.
pub fn parse(text: &str) -> Result<Manifest> {
    let manifest = Manifest::parse(text)?;
    manifest.validate()?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use crate::Error;

    #[test]
    fn embedded_manifest_parses_and_validates() {
        super::embedded_manifest().expect("embedded manifest is valid");
    }

    #[test]
    fn corrupted_manifest_fixture_is_rejected() {
        let text = include_str!("../tests/fixtures/corrupted-manifest.toml");
        let error = super::parse(text).unwrap_err();
        assert!(matches!(error, Error::Manifest(_)), "{error}");
    }
}
