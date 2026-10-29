//! `BUNDLE.toml` validation and archive installation.

mod common;

use std::fs;
use std::time::SystemTime;

use applejack_engine::bundle::{self, ArchiveSource, BundleToml, Capabilities, InstallRequest};
use applejack_engine::{Arch, Error, GraphicsBackend, InstalledEngine, Progress, SyncMode};
use common::{
    BUNDLE_TOML, fake_installed, fixture_archive, install_local, layout, rosetta, version,
    write_bundle_tree, wrong_digest,
};

fn validate_variant(replace: &str, with: &str) -> Result<BundleToml, Error> {
    assert!(BUNDLE_TOML.contains(replace), "fixture lacks {replace:?}");
    validate_text(&BUNDLE_TOML.replace(replace, with))
}

/// Writes the fixture tree with `text` as its descriptor and loads it.
fn validate_text(text: &str) -> Result<BundleToml, Error> {
    let dir = tempfile::tempdir().unwrap();
    write_bundle_tree(dir.path(), text);
    BundleToml::load(dir.path())
}

fn manifest_entry(version: &str, sha256: &applejack_engine::Sha256) -> applejack_manifest::Engine {
    applejack_manifest::Engine {
        id: "rosetta-wine".to_owned(),
        version: version.to_owned(),
        arch: Arch::X86_64,
        min_macos: "14.0".to_owned(),
        url: applejack_manifest::HttpsUrl::try_from(
            "https://example.invalid/rosetta-wine.tar.zst".to_owned(),
        )
        .unwrap(),
        sha256: sha256.clone(),
        components: std::collections::BTreeMap::new(),
        notarization_submission_id: String::new(),
    }
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
    assert!(
        error.to_string().contains("overrides.dxmt.x86_64"),
        "{error}"
    );
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

    let no_wow64 = BUNDLE_TOML
        .replace("wow64 = \"new\"", "wow64 = \"none\"")
        .replace("i386 = [\"dxmt/i386/d3d11.dll\"]", "i386 = []");
    let bundle = validate_text(&no_wow64).unwrap();
    assert!(!Capabilities::from_bundle(&bundle).supports_32bit);
    let bundle = validate_variant("wow64 = \"new\"", "wow64 = \"32on64\"").unwrap();
    assert!(Capabilities::from_bundle(&bundle).supports_32bit);
}

#[test]
fn validation_checks_dll_entries_per_arch() {
    let i386_line = "i386 = [\"dxmt/i386/d3d11.dll\"]";
    assert!(BUNDLE_TOML.contains(i386_line));

    // A 32-bit list needs a Wine that runs 32-bit code.
    let error = validate_variant("wow64 = \"new\"", "wow64 = \"none\"").unwrap_err();
    assert!(matches!(error, Error::InvalidBundle(_)), "{error}");
    assert!(
        error
            .to_string()
            .contains("overrides.dxmt.i386 must be empty"),
        "{error}"
    );

    // Only `.dll` files can be overrides; `winemetal.so` is not one.
    let text = BUNDLE_TOML.replace(i386_line, "i386 = [\"dxmt/winemetal.so\"]");
    let error = validate_text(&text).unwrap_err();
    assert!(matches!(error, Error::InvalidBundle(_)), "{error}");
    assert!(error.to_string().contains("`.dll` extension"), "{error}");

    // Stems are unique per architecture, ignoring case.
    let text = BUNDLE_TOML.replace(
        "\"dxmt/d3d10core.dll\"]",
        "\"dxmt/d3d10core.dll\", \"dxmt/i386/D3D11.dll\"]",
    );
    let error = validate_text(&text).unwrap_err();
    assert!(matches!(error, Error::InvalidBundle(_)), "{error}");
    assert!(error.to_string().contains("repeats a DLL name"), "{error}");

    // The same stem in the other architecture's list is fine; the fixture
    // already lists `d3d11.dll` for both.
    let bundle = validate_variant("", "").unwrap();
    assert_eq!(bundle.overrides.dxmt.x86_64.len(), 3);
    assert_eq!(bundle.overrides.dxmt.i386.len(), 1);
    assert!(bundle.overrides.d3dmetal.is_empty());
}

#[test]
fn validation_requires_d3dmetal_table_for_its_overrides() {
    let text = BUNDLE_TOML
        .replace(
            "[overrides.d3dmetal]\nx86_64 = []",
            "[overrides.d3dmetal]\nx86_64 = [\"dxmt/dxgi.dll\"]",
        )
        .replace("[d3dmetal]\noverlay_dir = \"wine/lib/external\"\n", "");
    let error = validate_text(&text).unwrap_err();
    assert!(matches!(error, Error::InvalidBundle(_)), "{error}");
    assert!(error.to_string().contains("[d3dmetal] table"), "{error}");
}

#[test]
fn validation_rejects_arch_not_matching_engine_id() {
    let error = validate_variant("arch = \"x86_64\"", "arch = \"arm64\"").unwrap_err();
    assert!(matches!(error, Error::InvalidBundle(_)), "{error}");
    assert!(
        error
            .to_string()
            .contains("does not match engine `rosetta-wine`"),
        "{error}"
    );
}

#[test]
fn hash_is_checked_before_extraction() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    let id = rosetta();
    let v = version("10.0.0");
    // A stale partial is replaced only after the hash passes, and bytes that
    // are not zstd would fail as `Io` if anything decoded them first.
    let partial = layout.partial_dir(id, &v);
    fs::create_dir_all(&partial).unwrap();
    fs::write(partial.join("stale-marker"), b"x").unwrap();
    let archive = dir.path().join("garbage.tar.zst");
    fs::write(&archive, b"not zstd").unwrap();

    let error = install_local(&layout, &v, &archive, &wrong_digest()).unwrap_err();
    assert!(matches!(error, Error::Sha256Mismatch { .. }), "{error}");
    assert!(error.to_string().contains("SHA-256 mismatch"), "{error}");
    assert!(
        error.to_string().contains("garbage.tar.zst"),
        "names the archive: {error}"
    );

    assert!(
        partial.join("stale-marker").is_file(),
        "nothing touched before the hash check"
    );
    assert!(!layout.engine_dir(id, &v).exists(), "no version directory");
    assert_eq!(fs::read(&archive).unwrap(), b"not zstd", "archive kept");
}

#[test]
fn install_rejects_symlink_escaping_bundle_root() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    let v = version("10.0.0");
    for (name, target) in [
        ("absolute", "/usr/bin/true"),
        ("relative", "../../../../outside/wine64"),
    ] {
        let tree = dir.path().join(format!("tree-{name}"));
        write_bundle_tree(&tree, BUNDLE_TOML);
        let loader = tree.join("wine/bin/wine64");
        fs::remove_file(&loader).unwrap();
        std::os::unix::fs::symlink(target, &loader).unwrap();
        let archive = dir.path().join(format!("{name}.tar.zst"));
        let digest = common::pack(&tree, &archive);

        let error = install_local(&layout, &v, &archive, &digest).unwrap_err();
        assert!(matches!(error, Error::InvalidArchive(_)), "{name}: {error}");
        assert!(
            error.to_string().contains("outside the bundle"),
            "{name}: {error}"
        );
        assert!(!layout.partial_dir(rosetta(), &v).exists(), "{name}");
        assert!(!layout.engine_dir(rosetta(), &v).exists(), "{name}");
    }
}

/// Archives whose symlinks pass the lexical extraction check but resolve
/// outside the bundle on disk, because an earlier symlink is used as a
/// directory component. Each is a hand-built entry list; a tree on disk
/// cannot express them.
#[test]
fn install_rejects_chained_symlinks_resolving_outside() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    let v = version("10.0.0");
    let deep = format!("{}esc", "p/".repeat(24));
    let up = "../".repeat(24);
    let through_partial = "a/../10.0.0.partial/BUNDLE.toml";
    let cases: [(&str, Vec<(&str, &str)>); 4] = [
        // `p -> .` makes the archive path 24 deep while the real one is 0.
        ("dir-link-then-deep-escape", vec![("p", "."), (&deep, &up)]),
        // `a/b` is the root on disk, so `c -> ..` lands at `<root>/c -> ..`.
        ("double-parent", vec![("a/b", ".."), ("a/b/c", "..")]),
        // Resolves inside `.partial` while checked, to a sibling once renamed.
        (
            "climb-out-and-back-in",
            vec![("a", "."), ("x", through_partial)],
        ),
        ("dangling", vec![("wine/nowhere", "missing-target")]),
    ];
    for (name, links) in cases {
        let tree = dir.path().join(format!("tree-{name}"));
        write_bundle_tree(&tree, BUNDLE_TOML);
        let archive = dir.path().join(format!("{name}.tar.zst"));
        let digest = common::pack_with_links(&tree, &archive, &links);

        let error = install_local(&layout, &v, &archive, &digest).unwrap_err();
        assert!(matches!(error, Error::InvalidArchive(_)), "{name}: {error}");
        assert!(
            error.to_string().contains("does not resolve inside"),
            "{name}: {error}"
        );
        assert!(!layout.partial_dir(rosetta(), &v).exists(), "{name}");
        assert!(!layout.engine_dir(rosetta(), &v).exists(), "{name}");
    }
}

#[test]
fn install_keeps_chained_symlinks_that_stay_inside() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    let v = version("10.0.0");
    let tree = dir.path().join("tree");
    write_bundle_tree(&tree, BUNDLE_TOML);
    fs::write(tree.join("wine/lib/wine64-real"), b"loader").unwrap();
    let archive = dir.path().join("inside.tar.zst");
    // `..` that reaches exactly the root, a link through a link, and a link
    // whose archive path runs through an earlier directory link.
    let digest = common::pack_with_links(
        &tree,
        &archive,
        &[
            ("wine/lib64", "../wine/lib"),
            ("wine/bin/wine64-alias", "../lib64/wine64-real"),
            ("self", "."),
            ("self/self/wine/loader-alias", "bin/wine64-alias"),
        ],
    );

    let installed = install_local(&layout, &v, &archive, &digest).unwrap();
    for alias in ["wine/bin/wine64-alias", "wine/loader-alias"] {
        let path = installed.root.join(alias);
        assert!(path.symlink_metadata().unwrap().is_symlink(), "{alias}");
        assert_eq!(fs::read(&path).unwrap(), b"loader", "{alias}");
    }
}

#[test]
fn install_keeps_symlinks_that_stay_inside() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    let v = version("10.0.0");
    let tree = dir.path().join("tree");
    write_bundle_tree(&tree, BUNDLE_TOML);
    // `wine/bin/wine64 -> ../lib/wine64-real` is the shape real builds use.
    let loader = tree.join("wine/bin/wine64");
    fs::remove_file(&loader).unwrap();
    fs::write(tree.join("wine/lib/wine64-real"), b"loader").unwrap();
    std::os::unix::fs::symlink("../lib/wine64-real", &loader).unwrap();
    let archive = dir.path().join("linked.tar.zst");
    let digest = common::pack(&tree, &archive);

    let installed = install_local(&layout, &v, &archive, &digest).unwrap();
    let loader = installed.root.join("wine/bin/wine64");
    assert!(loader.symlink_metadata().unwrap().is_symlink());
    assert_eq!(fs::read(&loader).unwrap(), b"loader");
}

#[test]
fn validation_rejects_symlink_resolving_outside_root() {
    let dir = tempfile::tempdir().unwrap();
    write_bundle_tree(dir.path(), BUNDLE_TOML);
    let dll = dir.path().join("dxmt/dxgi.dll");
    fs::remove_file(&dll).unwrap();
    std::os::unix::fs::symlink("/etc/hosts", &dll).unwrap();

    let error = BundleToml::load(dir.path()).unwrap_err();
    assert!(matches!(error, Error::InvalidBundle(_)), "{error}");
    assert!(error.to_string().contains("inside the bundle"), "{error}");
}

#[test]
fn install_replaces_stale_partial_file() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    let (archive, digest) = fixture_archive(dir.path(), "10.0.0");
    let v = version("10.0.0");
    let partial = layout.partial_dir(rosetta(), &v);
    fs::create_dir_all(partial.parent().unwrap()).unwrap();
    fs::write(&partial, b"left over").unwrap();

    install_local(&layout, &v, &archive, &digest).unwrap();
    assert!(!partial.exists());
    assert!(layout.engine_dir(rosetta(), &v).is_dir());
}

#[test]
fn missing_root_descriptor_names_the_archive_root_rule() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    let v = version("10.0.0");
    let tree = dir.path().join("tree");
    write_bundle_tree(&tree.join("rosetta-wine-10.0.0"), BUNDLE_TOML);
    let archive = dir.path().join("nested.tar.zst");
    let digest = common::pack(&tree, &archive);

    let error = install_local(&layout, &v, &archive, &digest).unwrap_err();
    assert!(matches!(error, Error::InvalidArchive(_)), "{error}");
    assert!(error.to_string().contains("top-level directory"), "{error}");
    assert!(!layout.partial_dir(rosetta(), &v).exists());
    assert!(!layout.engine_dir(rosetta(), &v).exists());
}

#[test]
fn install_removes_partial_when_rename_fails() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    let (archive, digest) = fixture_archive(dir.path(), "10.0.0");
    let v = version("10.0.0");
    // Another install commits `<version>/` between the existence check and
    // the rename; a non-empty target makes the rename fail, which is the
    // failure the cleanup rule covers.
    let final_dir = layout.engine_dir(rosetta(), &v);
    let occupied = final_dir.join("occupied");
    let request = InstallRequest::local(rosetta(), v.clone(), &archive, digest);
    let error = bundle::install(&layout, &request, &mut |progress| {
        if progress == Progress::Validating {
            fs::create_dir_all(&final_dir).unwrap();
            fs::write(&occupied, b"x").unwrap();
        }
    })
    .unwrap_err();
    assert!(matches!(error, Error::Io { .. }), "{error}");
    assert!(!layout.partial_dir(rosetta(), &v).exists());
    assert!(
        occupied.is_file(),
        "the other install's directory is untouched"
    );
}

#[test]
fn install_replaces_stale_version_directory() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    let (archive, digest) = fixture_archive(dir.path(), "10.0.0");
    let v = version("10.0.0");
    let final_dir = layout.engine_dir(rosetta(), &v);

    // A `<version>/` without a descriptor (an interrupted removal) is not
    // installed: `open` says so and `install` replaces it.
    fs::create_dir_all(final_dir.join("wine")).unwrap();
    fs::write(final_dir.join("wine/leftover"), b"x").unwrap();
    let error = InstalledEngine::open(&layout, rosetta(), &v).unwrap_err();
    assert!(matches!(error, Error::NotInstalled { .. }), "{error}");
    let installed = install_local(&layout, &v, &archive, &digest).unwrap();
    assert!(!final_dir.join("wine/leftover").exists());
    assert!(installed.root.join("BUNDLE.toml").is_file());

    // A dangling link at `<version>` is not a bundle either.
    fs::remove_dir_all(&final_dir).unwrap();
    std::os::unix::fs::symlink("missing-target", &final_dir).unwrap();
    install_local(&layout, &v, &archive, &digest).unwrap();
    assert!(final_dir.is_dir());
    assert!(!final_dir.symlink_metadata().unwrap().is_symlink());
}

#[test]
fn install_extracts_validates_and_renames() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    let v = version("10.0.0");
    let tree = dir.path().join("tree");
    write_bundle_tree(&tree, BUNDLE_TOML);
    fs::set_permissions(
        tree.join("wine/bin/wine64"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    fs::set_permissions(
        tree.join("wine/bin/wineserver"),
        fs::Permissions::from_mode(0o4755),
    )
    .unwrap();
    let archive = dir.path().join("rosetta-wine-10.0.0.tar.zst");
    let digest = common::pack(&tree, &archive);

    let installed = install_local(&layout, &v, &archive, &digest).unwrap();
    assert_eq!(installed.root, layout.engine_dir(rosetta(), &v));
    assert_eq!(installed.version, v);
    let loader = installed.root.join("wine/bin/wine64");
    assert!(loader.is_file());
    let mode = loader.metadata().unwrap().permissions().mode();
    assert_ne!(mode & 0o111, 0, "executable bit survives install: {mode:o}");
    let server_mode = installed
        .root
        .join("wine/bin/wineserver")
        .metadata()
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(server_mode & 0o7000, 0, "setuid stripped: {server_mode:o}");
    assert_ne!(server_mode & 0o111, 0, "{server_mode:o}");
    assert!(!layout.partial_dir(rosetta(), &v).exists());

    let reopened = InstalledEngine::open(&layout, rosetta(), &v).unwrap();
    assert_eq!(reopened, installed);

    let request = InstallRequest::local(rosetta(), v.clone(), &archive, digest);
    assert_eq!(request.id(), rosetta());
    assert_eq!(request.version(), &v);
    assert_eq!(request.source(), &ArchiveSource::LocalFile(archive.clone()));
    assert!(request.manifest().is_none());
    let again = bundle::install(&layout, &request, &mut |_| {}).unwrap_err();
    assert!(matches!(again, Error::AlreadyInstalled { .. }), "{again}");
}

#[test]
fn installed_engine_records_overlay_root() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    let (archive, digest) = fixture_archive(dir.path(), "10.0.0");
    let v = version("10.0.0");

    let installed = install_local(&layout, &v, &archive, &digest).unwrap();
    assert_eq!(installed.overlay_root, layout.overlay_dir(rosetta(), &v));
    assert!(
        !installed.overlay_root.exists(),
        "install never creates the overlay"
    );
}

#[test]
fn install_reports_progress_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    let (archive, digest) = fixture_archive(dir.path(), "10.0.0");
    let request = InstallRequest::local(rosetta(), version("10.0.0"), archive, digest);
    let mut seen = Vec::new();
    bundle::install(&layout, &request, &mut |progress| seen.push(progress)).unwrap();
    assert_eq!(
        seen,
        [
            Progress::Verifying,
            Progress::Extracting,
            Progress::Validating
        ]
    );
}

#[test]
fn install_from_manifest_cross_checks_descriptor() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    let (archive, digest) = fixture_archive(dir.path(), "10.0.0");
    let v = version("10.0.0");
    let source = ArchiveSource::LocalFile(archive);

    let mut entry = manifest_entry("10.0.0", &digest);
    entry.min_macos = "15.0".to_owned();
    let request = InstallRequest::from_manifest(&entry, source.clone()).unwrap();
    let error = bundle::install(&layout, &request, &mut |_| {}).unwrap_err();
    assert!(
        matches!(
            error,
            Error::ManifestMismatch {
                field: "min_macos",
                ..
            }
        ),
        "{error}"
    );
    assert!(error.to_string().contains("15.0"), "{error}");
    assert!(!layout.partial_dir(rosetta(), &v).exists());
    assert!(!layout.engine_dir(rosetta(), &v).exists());

    let mut entry = manifest_entry("10.0.0", &digest);
    entry.arch = Arch::Arm64;
    let request = InstallRequest::from_manifest(&entry, source.clone()).unwrap();
    let error = bundle::install(&layout, &request, &mut |_| {}).unwrap_err();
    assert!(
        matches!(error, Error::ManifestMismatch { field: "arch", .. }),
        "{error}"
    );

    let entry = manifest_entry("10.0.0", &digest);
    let request = InstallRequest::from_manifest(&entry, source).unwrap();
    assert_eq!(request.id(), rosetta());
    assert_eq!(request.version(), &v);
    assert_eq!(request.sha256(), &digest);
    assert_eq!(request.manifest(), Some(&entry));
    let installed = bundle::install(&layout, &request, &mut |_| {}).unwrap();
    assert_eq!(installed.version, v);

    let bad = manifest_entry("10.0.0-RC1", &digest);
    let error = InstallRequest::from_manifest(&bad, request.source().clone()).unwrap_err();
    assert!(matches!(error, Error::InvalidVersion { .. }), "{error}");
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

    let error = install_local(&layout, &v, &archive, &digest).unwrap_err();
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

    let error = install_local(&layout, &v, &archive, &digest).unwrap_err();
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
    install_local(&layout, &version("10.0.1"), &archive, &digest).unwrap();

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
