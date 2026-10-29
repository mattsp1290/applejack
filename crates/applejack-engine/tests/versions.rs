//! Installed-version listing, removal, and prune.

mod common;

use std::fs;

use applejack_engine::{Error, versions};
use common::{fake_installed, layout, rosetta, version};

#[test]
fn list_ignores_partial_overlay_and_files() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    assert_eq!(versions::list(&layout, rosetta()).unwrap(), Vec::new());

    fake_installed(&layout, "10.0.1");
    fake_installed(&layout, "10.0.0");
    fs::create_dir_all(layout.partial_dir(rosetta(), &version("10.0.2"))).unwrap();
    fs::create_dir_all(layout.overlay_dir(rosetta(), &version("10.0.0"))).unwrap();
    fs::write(layout.download_path(rosetta(), &version("10.0.3")), b"zst").unwrap();
    fs::write(layout.engine_root(rosetta()).join("10.0.4"), b"a file").unwrap();

    assert_eq!(
        versions::list(&layout, rosetta()).unwrap(),
        [version("10.0.0"), version("10.0.1")]
    );
}

#[test]
fn list_counts_only_directories_with_a_descriptor() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    fake_installed(&layout, "10.0.0");
    // A stale directory named like a version, and one whose descriptor is a
    // directory rather than a file, are not installed versions: `list`
    // omits them and `prune` leaves them alone.
    let stale = layout.engine_root(rosetta()).join("10.0.5");
    fs::create_dir_all(&stale).unwrap();
    fs::create_dir_all(layout.engine_root(rosetta()).join("10.0.6/BUNDLE.toml")).unwrap();

    assert_eq!(
        versions::list(&layout, rosetta()).unwrap(),
        [version("10.0.0")]
    );
    let removed = versions::prune(&layout, rosetta(), 0, &[]).unwrap();
    assert_eq!(removed, [version("10.0.0")]);
    assert!(stale.is_dir());
    assert!(layout.engine_root(rosetta()).join("10.0.6").is_dir());

    // `remove` deletes a stale directory, so the three agree on what is
    // there: an interrupted removal can always be finished.
    versions::remove(&layout, rosetta(), &version("10.0.5"), &[]).unwrap();
    assert!(!stale.exists());
    versions::remove(&layout, rosetta(), &version("10.0.6"), &[]).unwrap();
    assert!(!layout.engine_root(rosetta()).join("10.0.6").exists());
}

#[test]
fn prune_never_removes_protected() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    for v in ["10.0.0", "10.0.1", "10.0.2", "10.0.3", "10.0.4"] {
        fake_installed(&layout, v);
    }
    // The active version, one pinned by a game, one in use by a session.
    let protected = [version("10.0.1"), version("10.0.0")];

    let removed = versions::prune(&layout, rosetta(), 1, &protected).unwrap();
    assert_eq!(removed, [version("10.0.2"), version("10.0.3")]);
    assert_eq!(
        versions::list(&layout, rosetta()).unwrap(),
        [version("10.0.0"), version("10.0.1"), version("10.0.4")]
    );

    // keep = 0 still leaves the protected versions.
    let removed = versions::prune(&layout, rosetta(), 0, &protected).unwrap();
    assert_eq!(removed, [version("10.0.4")]);
    assert_eq!(
        versions::list(&layout, rosetta()).unwrap(),
        [version("10.0.0"), version("10.0.1")]
    );
}

#[test]
fn prune_keeps_the_newest_by_version_order() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    for v in ["10.0.9", "10.0.10", "9.9.9", "10.0.10-rc9", "10.0.10-rc10"] {
        fake_installed(&layout, v);
    }
    let removed = versions::prune(&layout, rosetta(), 1, &[]).unwrap();
    assert_eq!(
        removed,
        [
            version("9.9.9"),
            version("10.0.9"),
            version("10.0.10-rc9"),
            version("10.0.10-rc10"),
        ]
    );
    assert_eq!(
        versions::list(&layout, rosetta()).unwrap(),
        [version("10.0.10")]
    );
}

#[test]
fn remove_refuses_protected_and_missing() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    fake_installed(&layout, "10.0.0");
    fake_installed(&layout, "10.0.1");
    fake_installed(&layout, "10.0.2");

    let error =
        versions::remove(&layout, rosetta(), &version("10.0.0"), &[version("10.0.0")]).unwrap_err();
    assert!(matches!(error, Error::ProtectedVersion { .. }), "{error}");
    let error = versions::remove(&layout, rosetta(), &version("10.0.9"), &[]).unwrap_err();
    assert!(matches!(error, Error::NotInstalled { .. }), "{error}");
    // A protected version that is not installed reports the accurate state.
    let error =
        versions::remove(&layout, rosetta(), &version("10.0.9"), &[version("10.0.9")]).unwrap_err();
    assert!(matches!(error, Error::NotInstalled { .. }), "{error}");

    versions::remove(&layout, rosetta(), &version("10.0.2"), &[version("10.0.0")]).unwrap();
    assert_eq!(
        versions::list(&layout, rosetta()).unwrap(),
        [version("10.0.0"), version("10.0.1")]
    );
}

#[test]
fn download_path_is_outside_the_partial_directory() {
    let layout = layout(std::path::Path::new("/data"));
    let v = version("10.0.0");
    let download = layout.download_path(rosetta(), &v);
    assert_eq!(
        download,
        std::path::Path::new("/data/data/engines/rosetta-wine/10.0.0.download")
    );
    assert!(!download.starts_with(layout.partial_dir(rosetta(), &v)));
    assert_ne!(download, layout.engine_dir(rosetta(), &v));
}
