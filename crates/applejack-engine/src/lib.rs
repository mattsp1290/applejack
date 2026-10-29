//! Engine abstraction: the `Engine` trait, the on-disk engine bundle layout
//! and `BUNDLE.toml`, engine install and rollback, prefix creation and
//! snapshots, and launch. Accepts and returns plain structs and never depends
//! on `applejack-config`.

use applejack_manifest::{Error, Manifest};

// Embedded so the pins travel inside the signed binary and change only with a
// release.
const EMBEDDED_MANIFEST: &str = include_str!("../../../engines/manifest.toml");

/// Parses and validates the `engines/manifest.toml` compiled into this crate.
///
/// # Errors
///
/// Returns the parse or validation error. `cargo xtask manifest-verify`
/// rejects such a manifest before it can ship.
pub fn embedded_manifest() -> Result<Manifest, Error> {
    let manifest = Manifest::parse(EMBEDDED_MANIFEST)?;
    manifest.validate()?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    #[test]
    fn embedded_manifest_parses_and_validates() {
        super::embedded_manifest().expect("embedded manifest is valid");
    }
}
