//! `BUNDLE.toml` validation and archive installation.

mod common;

use std::fs;
use std::time::SystemTime;

use applejack_engine::bundle::{self, ArchiveSource, BundleToml, Capabilities};
use applejack_engine::{Error, GraphicsBackend, InstalledEngine, SyncMode};
use common::{
    BUNDLE_TOML, fake_installed, fixture_archive, layout, rosetta, version, write_bundle_tree,
    wrong_digest,
};

fn validate_variant(replace: &str, with: &str) -> Result<BundleToml, Error> {
    assert!(BUNDLE_TOML.contains(replace), "fixture lacks {replace:?}");
    let text = BUNDLE_TOML.replace(replace, with);
    let dir = tempfile::tempdir().unwrap();
    write_bundle_tree(dir.path(), &text);
    BundleToml::load(dir.path())
}

#[test]
fn fixture_bundle_validates() {
    let bundle = validate_variant("", "").expect("fixture validates");
    assert_eq!(bundle.id, rosetta());
    assert_eq!(bundle.version, version("10.0.0"));
    assert_eq!(bundle.default_sync_mode(), Some(SyncMode::Msync));
    assert_eq!(bundle.components.len(), 4);
    assert!(
        bundle
            .registry
            .as_ref()
            .unwrap()
            .hidraw_key
            .ends_with("winebus\\Parameters\\DisableHidraw")
    );
}

#[test]
fn validation_rejects_missing_loader() {
    let error =
        validate_variant("loader = \"wine/bin/wine64\"", "loader = \"wine/bin/wine\"").unwrap_err();
    assert!(matches!(error, Error::MissingLoader { .. }), "{error}");
    assert!(error.to_string().contains("wine/bin/wine"), "{error}");
}

#[test]
fn validation_rejects_unknown_sync_mode() {
    let error = validate_variant("\"esync\"", "\"fsync\"").unwrap_err();
    assert!(matches!(error, Error::Toml(_)), "{error}");
    assert!(
        error.to_string().contains("unknown sync mode `fsync`"),
        "{error}"
    );
}

#[test]
fn validation_rejects_empty_sync_modes() {
    let error = validate_variant(
        "sync_modes = [\"msync\", \"esync\", \"none\"]",
        "sync_modes = []",
    )
    .unwrap_err();
    assert!(matches!(error, Error::InvalidBundle(_)), "{error}");
    assert!(error.to_string().contains("sync_modes"), "{error}");
}

#[test]
fn validation_rejects_duplicate_sync_modes() {
    let error = validate_variant("\"esync\", \"none\"", "\"esync\", \"msync\"").unwrap_err();
    assert!(matches!(error, Error::InvalidBundle(_)), "{error}");
    assert!(error.to_string().contains("more than once"), "{error}");
}

#[test]
fn validation_rejects_absolute_path() {
    let error = validate_variant(
        "server = \"wine/bin/wineserver\"",
        "server = \"/usr/bin/true\"",
    )
    .unwrap_err();
    assert!(matches!(error, Error::InvalidBundle(_)), "{error}");
    assert!(error.to_string().contains("relative path"), "{error}");
}

#[test]
fn validation_rejects_parent_component() {
    let error = validate_variant("\"dxmt/dxgi.dll\"", "\"dxmt/../dxmt/dxgi.dll\"").unwrap_err();
    assert!(matches!(error, Error::InvalidBundle(_)), "{error}");
    assert!(error.to_string().contains("overrides.dxmt.dlls"), "{error}");
}

#[test]
fn validation_rejects_unknown_field() {
    let error = validate_variant("[registry]", "[registry]\nfoo = 1").unwrap_err();
    assert!(matches!(error, Error::Toml(_)), "{error}");
}

#[test]
fn override_tables_reference_only_present_dlls() {
    validate_variant("", "").expect("every fixture override DLL exists");
    let error = validate_variant("\"vkd3d/d3d12core.dll\"", "\"vkd3d/missing.dll\"").unwrap_err();
    assert!(matches!(error, Error::InvalidBundle(_)), "{error}");
    assert!(error.to_string().contains("vkd3d/missing.dll"), "{error}");
}

#[test]
fn capabilities_follow_the_descriptor() {
    let bundle = validate_variant("", "").unwrap();
    let caps = Capabilities::from_bundle(&bundle);
    assert!(caps.supports_32bit);
    assert_eq!(
        caps.backends,
        [
            GraphicsBackend::Dxmt,
            GraphicsBackend::WineD3D,
            GraphicsBackend::Vkd3dProton,
            GraphicsBackend::D3DMetal,
        ]
    );
    assert_eq!(caps.sync_modes, bundle.sync_modes);

    let bundle = validate_variant("wow64 = \"new\"", "wow64 = \"none\"").unwrap();
    assert!(!Capabilities::from_bundle(&bundle).supports_32bit);
    let bundle = validate_variant("wow64 = \"new\"", "wow64 = \"32on64\"").unwrap();
    assert!(Capabilities::from_bundle(&bundle).supports_32bit);
}

#[test]
fn hash_is_checked_before_extraction() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    let (archive, _digest) = fixture_archive(dir.path(), "10.0.0");
    let archive_bytes = fs::read(&archive).unwrap();

    let error = bundle::install_from_archive(
        &layout,
        rosetta(),
        &version("10.0.0"),
        &archive,
        &wrong_digest(),
    )
    .unwrap_err();
    assert!(matches!(error, Error::Sha256Mismatch { .. }), "{error}");
    assert!(
        error.to_string().starts_with("archive SHA-256 mismatch"),
        "{error}"
    );

    let id = rosetta();
    let v = version("10.0.0");
    assert!(
        !layout.partial_dir(id, &v).exists(),
        "no .partial extraction"
    );
    assert!(!layout.engine_dir(id, &v).exists(), "no version directory");
    assert_eq!(fs::read(&archive).unwrap(), archive_bytes, "archive kept");
}

#[test]
fn install_extracts_validates_and_renames() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    let (archive, digest) = fixture_archive(dir.path(), "10.0.0");
    let v = version("10.0.0");

    let installed =
        bundle::install_from_archive(&layout, rosetta(), &v, &archive, &digest).unwrap();
    assert_eq!(installed.root, layout.engine_dir(rosetta(), &v));
    assert_eq!(installed.version, v);
    assert!(installed.root.join("wine/bin/wine64").is_file());
    assert!(!layout.partial_dir(rosetta(), &v).exists());

    let reopened = InstalledEngine::open(&layout, rosetta(), &v).unwrap();
    assert_eq!(reopened, installed);

    let again = bundle::install(
        &layout,
        rosetta(),
        &v,
        &ArchiveSource::LocalFile(archive),
        &digest,
    )
    .unwrap_err();
    assert!(matches!(again, Error::AlreadyInstalled { .. }), "{again}");
}

#[test]
fn invalid_bundle_removes_partial_and_leaves_no_version() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    let tree = dir.path().join("tree");
    write_bundle_tree(&tree, BUNDLE_TOML);
    fs::remove_file(tree.join("wine/bin/wine64")).unwrap();
    let archive = dir.path().join("bad.tar.zst");
    let digest = common::pack(&tree, &archive);
    let v = version("10.0.0");

    let error =
        bundle::install_from_archive(&layout, rosetta(), &v, &archive, &digest).unwrap_err();
    assert!(matches!(error, Error::MissingLoader { .. }), "{error}");
    assert!(!layout.partial_dir(rosetta(), &v).exists());
    assert!(!layout.engine_dir(rosetta(), &v).exists());
}

#[test]
fn install_rejects_descriptor_naming_another_version() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    let (archive, digest) = fixture_archive(dir.path(), "10.0.0");
    let v = version("10.0.1");

    let error =
        bundle::install_from_archive(&layout, rosetta(), &v, &archive, &digest).unwrap_err();
    assert!(
        matches!(error, Error::BundleIdentityMismatch { .. }),
        "{error}"
    );
    assert!(!layout.partial_dir(rosetta(), &v).exists());
    assert!(!layout.engine_dir(rosetta(), &v).exists());
}

#[test]
fn install_of_version_b_never_touches_version_a() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    let a_dir = fake_installed(&layout, "10.0.0");
    let marker = a_dir.join("wine/bin/wine64");
    let before_content = fs::read(&marker).unwrap();
    let before = snapshot(&a_dir);
    // Give a second install a chance to show up as a later mtime.
    std::thread::sleep(std::time::Duration::from_millis(20));

    let (archive, digest) = fixture_archive(dir.path(), "10.0.1");
    bundle::install_from_archive(&layout, rosetta(), &version("10.0.1"), &archive, &digest)
        .unwrap();

    assert_eq!(
        snapshot(&a_dir),
        before,
        "version A entries and mtimes unchanged"
    );
    assert_eq!(fs::read(&marker).unwrap(), before_content);
    assert!(layout.engine_dir(rosetta(), &version("10.0.1")).is_dir());
}

/// Every path under `root` with its modification time.
fn snapshot(root: &std::path::Path) -> Vec<(std::path::PathBuf, SystemTime)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            out.push((path.clone(), entry.metadata().unwrap().modified().unwrap()));
            if path.is_dir() {
                stack.push(path);
            }
        }
    }
    out.sort();
    out
}
