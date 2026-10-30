//! `D3DMetal` is user-supplied and never downloaded, bundled, or published.
//! `import` copies the payload from a mounted Game Porting Toolkit volume
//! into the engine's per-user overlay once the user has accepted Apple's
//! license, and returns the record the core stores; `remove` deletes the
//! overlay. The source is a plain path, so tests use a fixture tree and the
//! import never runs against a real volume in CI.

use std::fs;
use std::path::{Path, PathBuf};

use crate::{Error, InstalledEngine, Result};

use super::clock::Utc;
use super::snapshot::copy_tree;

/// The README at the volume root; its presence identifies a GPTK volume.
pub const README: &str = "README.md";
/// Apple's license file at the volume root.
pub const LICENSE: &str = "License.pdf";
/// Where the GPTK volume holds the payload (the plan's assumption A1: the
/// toolkit volume keeps it under `redist/lib/external/`). When the README
/// of a newer toolkit names another `lib/external` location, the import
/// error prints it.
pub const PAYLOAD_DIR: &str = "redist/lib/external";
/// The framework inside [`PAYLOAD_DIR`].
pub const FRAMEWORK: &str = "D3DMetal.framework";
/// The shared library inside [`PAYLOAD_DIR`].
pub const DYLIB: &str = "libd3dshared.dylib";

/// What an import produced; the core stores it in its `d3dmetal` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct D3DMetalOverlay {
    /// `<overlay_root>/<overlay_dir>`: where the payload now lives.
    pub overlay_dir: PathBuf,
    /// The volume the payload was copied from.
    pub imported_from: PathBuf,
    /// The toolkit version named in the volume's README, or `unknown`.
    pub gptk_version: String,
    /// When the user accepted the license, `YYYY-MM-DDTHH:MM:SSZ`.
    pub accepted_at: String,
}

/// The text printed before the license flag is required.
#[must_use]
pub fn acknowledgement(source: &Path) -> String {
    format!(
        "D3DMetal is Apple software licensed under the terms in `{}`. applejack copies it into a \
         per-user overlay on this Mac and never redistributes it. Re-run with \
         --accept-apple-license to confirm you have read and accept those terms.",
        source.join(LICENSE).display()
    )
}

/// `<overlay_root>/<overlay_dir>` when the bundle declares an overlay location.
#[must_use]
pub fn overlay_dir(installed: &InstalledEngine) -> Option<PathBuf> {
    installed
        .bundle
        .d3dmetal
        .as_ref()
        .map(|table| installed.overlay_root.join(&table.overlay_dir))
}

/// True when the overlay holds both payload entries.
#[must_use]
pub fn overlay_present(installed: &InstalledEngine) -> bool {
    overlay_dir(installed)
        .is_some_and(|dir| dir.join(FRAMEWORK).is_dir() && dir.join(DYLIB).is_file())
}

/// Imports the payload from the GPTK volume at `source` into the overlay of
/// `installed`, replacing an earlier import.
///
/// # Errors
///
/// [`Error::D3DMetalUnsupported`] when the bundle declares no overlay
/// location; [`Error::D3DMetalSource`] naming the README path when it is
/// absent, or naming the payload location when that is incomplete;
/// [`Error::LicenseNotAccepted`] with the acknowledgement text unless
/// `accept_apple_license`; [`Error::Io`] from the copy.
pub fn import(
    installed: &InstalledEngine,
    source: &Path,
    accept_apple_license: bool,
) -> Result<D3DMetalOverlay> {
    let Some(target) = overlay_dir(installed) else {
        return Err(Error::D3DMetalUnsupported {
            version: installed.version.clone(),
        });
    };
    let readme = source.join(README);
    let readme_text = fs::read_to_string(&readme).map_err(|error| Error::D3DMetalSource {
        path: readme.clone(),
        reason: format!(
            "is not readable ({error}); the path must be the root of a mounted Game Porting Toolkit volume"
        ),
    })?;
    let payload = source.join(PAYLOAD_DIR);
    for (entry, is_present) in [
        (FRAMEWORK, payload.join(FRAMEWORK).is_dir()),
        (DYLIB, payload.join(DYLIB).is_file()),
    ] {
        if !is_present {
            let named = named_payload_location(&readme_text).map_or_else(
                || {
                    format!(
                        "check `{}` for where this toolkit version keeps it",
                        readme.display()
                    )
                },
                |location| format!("`{}` names `{location}` instead", readme.display()),
            );
            return Err(Error::D3DMetalSource {
                path: payload.join(entry),
                reason: format!(
                    "is missing; the payload is expected under `{PAYLOAD_DIR}/` ({named})"
                ),
            });
        }
    }
    if !accept_apple_license {
        return Err(Error::LicenseNotAccepted {
            acknowledgement: acknowledgement(source),
        });
    }
    crate::bundle::remove_stale(&target)?;
    fs::create_dir_all(&target).map_err(|error| Error::io(&target, error))?;
    copy_tree(&payload.join(FRAMEWORK), &target.join(FRAMEWORK))?;
    fs::copy(payload.join(DYLIB), target.join(DYLIB))
        .map_err(|error| Error::io(target.join(DYLIB), error))?;
    Ok(D3DMetalOverlay {
        overlay_dir: target,
        imported_from: source.to_path_buf(),
        gptk_version: gptk_version(&readme_text).unwrap_or_else(|| "unknown".to_owned()),
        accepted_at: Utc::now().rfc3339(),
    })
}

/// Deletes the overlay payload and the empty directories it leaves behind
/// up to the overlay root, so `doctor` and the directory agree. A missing
/// overlay is not an error.
///
/// # Errors
///
/// [`Error::D3DMetalUnsupported`] or [`Error::Io`].
pub fn remove(installed: &InstalledEngine) -> Result<()> {
    let Some(target) = overlay_dir(installed) else {
        return Err(Error::D3DMetalUnsupported {
            version: installed.version.clone(),
        });
    };
    crate::bundle::remove_stale(&target)?;
    let mut parent = target.parent();
    while let Some(dir) = parent
        && dir.starts_with(&installed.overlay_root)
        && dir != installed.overlay_root
        && fs::remove_dir(dir).is_ok()
    {
        parent = dir.parent();
    }
    Ok(())
}

/// The first `digits[.digits...]` token, with or without a leading `v`,
/// after "Game Porting Toolkit" in the README, ignoring case.
#[must_use]
pub fn gptk_version(readme: &str) -> Option<String> {
    let lower = readme.to_ascii_lowercase();
    let index = lower.find("game porting toolkit")?;
    readme[index + "game porting toolkit".len()..]
        .split_whitespace()
        .take(4)
        .map(|token| token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.'))
        .map(|token| token.strip_prefix(['v', 'V']).unwrap_or(token))
        .find(|token| token.starts_with(|c: char| c.is_ascii_digit()))
        .map(ToOwned::to_owned)
}

/// A path in the README that names a `lib/external` location other than
/// [`PAYLOAD_DIR`], when the README says the payload moved.
#[must_use]
pub fn named_payload_location(readme: &str) -> Option<&str> {
    readme
        .split_whitespace()
        .map(|token| {
            token
                .trim_matches(|c: char| {
                    !(c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
                })
                .trim_end_matches(['/', '.'])
        })
        .find(|token| token.contains("lib/external") && *token != PAYLOAD_DIR)
}
