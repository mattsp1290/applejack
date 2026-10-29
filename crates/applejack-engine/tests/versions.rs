//! Installed-version listing, active selection, removal, and prune.

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
    fs::write(layout.active_file(rosetta()), "10.0.0\n").unwrap();

    assert_eq!(
        versions::list(&layout, rosetta()).unwrap(),
        [version("10.0.0"), version("10.0.1")]
    );
}

#[test]
fn active_round_trips_and_requires_an_installed_version() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    assert_eq!(versions::active(&layout, rosetta()).unwrap(), None);

    let error = versions::set_active(&layout, rosetta(), &version("10.0.0")).unwrap_err();
    assert!(matches!(error, Error::NotInstalled { .. }), "{error}");

    fake_installed(&layout, "10.0.0");
    versions::set_active(&layout, rosetta(), &version("10.0.0")).unwrap();
    assert_eq!(
        versions::active(&layout, rosetta()).unwrap(),
        Some(version("10.0.0"))
    );

    // A version removed behind the record's back is no longer active.
    fs::remove_dir_all(layout.engine_dir(rosetta(), &version("10.0.0"))).unwrap();
    assert_eq!(versions::active(&layout, rosetta()).unwrap(), None);
}

#[test]
fn prune_never_removes_active_or_referenced() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    for v in ["10.0.0", "10.0.1", "10.0.2", "10.0.3", "10.0.4"] {
        fake_installed(&layout, v);
    }
    versions::set_active(&layout, rosetta(), &version("10.0.1")).unwrap();
    let referenced = [version("10.0.0")];

    let removed = versions::prune(&layout, rosetta(), 1, &referenced).unwrap();
    assert_eq!(removed, [version("10.0.2"), version("10.0.3")]);
    assert_eq!(
        versions::list(&layout, rosetta()).unwrap(),
        [version("10.0.0"), version("10.0.1"), version("10.0.4")]
    );

    // keep = 0 still leaves the protected versions.
    let removed = versions::prune(&layout, rosetta(), 0, &referenced).unwrap();
    assert_eq!(removed, [version("10.0.4")]);
    assert_eq!(
        versions::list(&layout, rosetta()).unwrap(),
        [version("10.0.0"), version("10.0.1")]
    );
    assert_eq!(
        versions::active(&layout, rosetta()).unwrap(),
        Some(version("10.0.1"))
    );
}

#[test]
fn prune_keeps_the_newest_by_version_order() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    for v in ["10.0.9", "10.0.10", "9.9.9"] {
        fake_installed(&layout, v);
    }
    let removed = versions::prune(&layout, rosetta(), 1, &[]).unwrap();
    assert_eq!(removed, [version("9.9.9"), version("10.0.9")]);
}

#[test]
fn remove_refuses_active_and_referenced() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    fake_installed(&layout, "10.0.0");
    fake_installed(&layout, "10.0.1");
    fake_installed(&layout, "10.0.2");
    versions::set_active(&layout, rosetta(), &version("10.0.0")).unwrap();

    let error = versions::remove(&layout, rosetta(), &version("10.0.0"), &[]).unwrap_err();
    assert!(matches!(error, Error::ActiveVersion { .. }), "{error}");
    let error =
        versions::remove(&layout, rosetta(), &version("10.0.1"), &[version("10.0.1")]).unwrap_err();
    assert!(matches!(error, Error::ReferencedVersion { .. }), "{error}");
    let error = versions::remove(&layout, rosetta(), &version("10.0.9"), &[]).unwrap_err();
    assert!(matches!(error, Error::NotInstalled { .. }), "{error}");

    versions::remove(&layout, rosetta(), &version("10.0.2"), &[]).unwrap();
    assert_eq!(
        versions::list(&layout, rosetta()).unwrap(),
        [version("10.0.0"), version("10.0.1")]
    );
}
