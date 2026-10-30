//! Shared helpers: a bundle tree matching `fixtures/BUNDLE.toml`, a
//! `.tar.zst` archive of it, and its digest. Built at test time so no binary
//! fixture is committed.

#![allow(dead_code, reason = "each test binary uses a subset of the helpers")]

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use applejack_engine::{EngineId, EngineLayout, Sha256, Version};

pub const BUNDLE_TOML: &str = include_str!("../fixtures/BUNDLE.toml");

/// Files, relative to the bundle root, that the fixture descriptor names.
pub const BUNDLE_FILES: &[&str] = &[
    "wine/bin/wine64",
    "wine/bin/wineserver",
    "wine/bin/wine64-preloader",
    "wine/lib/wine/x86_64-windows/d3d11.dll",
    "dxmt/d3d11.dll",
    "dxmt/dxgi.dll",
    "dxmt/d3d10core.dll",
    "dxmt/i386/d3d11.dll",
    "vkd3d/d3d12.dll",
    "vkd3d/d3d12core.dll",
    "probes/cpuid",
    "probes/dx11-sample.exe",
    "probes/xinput-check.exe",
];

pub fn version(text: &str) -> Version {
    Version::new(text).expect("valid version")
}

pub fn rosetta() -> EngineId {
    EngineId::RosettaWine
}

/// Writes the fixture tree with `bundle_toml` as its descriptor.
pub fn write_bundle_tree(root: &Path, bundle_toml: &str) {
    for relative in BUNDLE_FILES {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, relative.as_bytes()).unwrap();
    }
    fs::write(root.join("BUNDLE.toml"), bundle_toml).unwrap();
}

/// The fixture text with `version` substituted for the default `10.0.0`.
pub fn bundle_toml_for_version(version: &str) -> String {
    BUNDLE_TOML.replace("version = \"10.0.0\"", &format!("version = \"{version}\""))
}

/// Packs `tree` as `<out>` (`.tar.zst`) and returns its digest.
pub fn pack(tree: &Path, out: &Path) -> Sha256 {
    pack_with_links(tree, out, &[])
}

/// Packs `tree`, then appends one symlink entry per `(path, target)` in
/// order, so an archive can carry links a directory tree cannot express
/// (a link whose archive path runs through an earlier link).
pub fn pack_with_links(tree: &Path, out: &Path, links: &[(&str, &str)]) -> Sha256 {
    let file = File::create(out).unwrap();
    let encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
    let mut builder = tar::Builder::new(encoder);
    // Symlinks in the tree become symlink entries, as a hostile bundle would
    // carry them.
    builder.follow_symlinks(false);
    builder.append_dir_all(".", tree).unwrap();
    for (path, target) in links {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_mode(0o777);
        header.set_size(0);
        builder.append_link(&mut header, path, target).unwrap();
    }
    let encoder = builder.into_inner().unwrap();
    encoder.finish().unwrap();
    applejack_engine::bundle::sha256_file(out).unwrap()
}

/// Installs `archive` as `version` through `bundle::install` with no manifest
/// and no progress consumer.
pub fn install_local(
    layout: &EngineLayout,
    version: &Version,
    archive: &Path,
    digest: &Sha256,
) -> applejack_engine::Result<applejack_engine::InstalledEngine> {
    let request = applejack_engine::InstallRequest::local(
        EngineId::RosettaWine,
        version.clone(),
        archive,
        digest.clone(),
    );
    applejack_engine::bundle::install(layout, &request, &mut |_| {})
}

/// A packed fixture archive for `version` under `dir`, with its digest.
pub fn fixture_archive(dir: &Path, version: &str) -> (PathBuf, Sha256) {
    let tree = dir.join(format!("tree-{version}"));
    write_bundle_tree(&tree, &bundle_toml_for_version(version));
    let archive = dir.join(format!("rosetta-wine-{version}.tar.zst"));
    let digest = pack(&tree, &archive);
    (archive, digest)
}

pub fn layout(dir: &Path) -> EngineLayout {
    EngineLayout::new(dir.join("data"))
}

/// An installed version directory with a descriptor, without going through
/// the installer.
pub fn fake_installed(layout: &EngineLayout, version: &str) -> PathBuf {
    let dir = layout.engine_dir(EngineId::RosettaWine, &self::version(version));
    write_bundle_tree(&dir, &bundle_toml_for_version(version));
    dir
}

pub fn wrong_digest() -> Sha256 {
    Sha256::try_from("f".repeat(64)).unwrap()
}
