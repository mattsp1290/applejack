//! The launch environment allowlist.

use std::ffi::OsString;
use std::fs;
use std::path::Path;

use applejack_engine::Error;
use applejack_engine::launch::env::{
    ALLOWLIST, AllowedRoots, DropReason, Dropped, Kind, Owner, assemble,
};

fn pair(name: &str, value: &str) -> (OsString, OsString) {
    (OsString::from(name), OsString::from(value))
}

const ENGINE_DIR: &str = "/data/engines/rosetta-wine/10.0.0";
const OVERLAY_DIR: &str = "/data/engines/rosetta-wine/10.0.0.user";
const PREFIX_DIR: &str = "/data/prefixes/steam";

/// Roots that do not exist on disk; they resolve to themselves.
fn roots() -> AllowedRoots {
    AllowedRoots::new(
        Path::new(ENGINE_DIR),
        Path::new(OVERLAY_DIR),
        Path::new(PREFIX_DIR),
    )
    .unwrap()
}

fn drop_of(name: &str, reason: DropReason) -> Dropped {
    Dropped {
        name: OsString::from(name),
        reason,
    }
}

fn outside(value: &str) -> DropReason {
    DropReason::OutsideRoots {
        value: OsString::from(value),
    }
}

#[test]
fn allowlist_matches_the_plan() {
    let names: Vec<&str> = ALLOWLIST.iter().map(|entry| entry.name).collect();
    assert_eq!(
        names,
        [
            "PATH",
            "HOME",
            "TMPDIR",
            "WINEPREFIX",
            "WINEDEBUG",
            "WINEDLLOVERRIDES",
            "WINEMSYNC",
            "WINEESYNC",
            "DXMT_CONFIG",
            "DXMT_CONFIG_FILE",
            "DXMT_LOG_LEVEL",
            "MTL_HUD_ENABLED",
            "ROSETTA_ADVERTISE_AVX",
            "GST_PLUGIN_PATH",
            "APPLEJACK_SESSION",
            "VK_ICD_FILENAMES",
        ]
    );
    let engine_owned: Vec<&str> = ALLOWLIST
        .iter()
        .filter(|entry| entry.owner == Owner::Engine)
        .map(|entry| entry.name)
        .collect();
    assert_eq!(
        engine_owned,
        [
            "WINEPREFIX",
            "WINEDLLOVERRIDES",
            "WINEMSYNC",
            "WINEESYNC",
            "APPLEJACK_SESSION",
        ]
    );
    for entry in ALLOWLIST {
        let is_path = entry.kind != Kind::Plain;
        let has_roots = entry.roots.engine || entry.roots.overlay || entry.roots.prefix;
        assert_eq!(
            is_path, has_roots,
            "{}: roots only on path rows",
            entry.name
        );
    }
}

#[test]
fn drops_unknown_variable() {
    let base = [
        pair("PATH", "/usr/bin"),
        pair("LD_PRELOAD", "x"),
        pair("FOO", "bar"),
    ];
    let (env, dropped) = assemble(&base, &[], &[], &roots());
    assert_eq!(env, [pair("PATH", "/usr/bin")]);
    assert_eq!(
        dropped,
        [
            drop_of("LD_PRELOAD", DropReason::NotAllowlisted),
            drop_of("FOO", DropReason::NotAllowlisted),
        ]
    );
}

#[test]
fn drops_dyld_insert_libraries() {
    let extras = [pair("DYLD_INSERT_LIBRARIES", "/tmp/evil.dylib")];
    let (env, dropped) = assemble(&[], &extras, &[], &roots());
    assert_eq!(env, []);
    assert_eq!(
        dropped,
        [drop_of("DYLD_INSERT_LIBRARIES", DropReason::Dyld)]
    );
}

#[test]
fn drops_vk_icd_filenames_outside_roots() {
    let value = "/opt/homebrew/share/vulkan/icd.d/MoltenVK_icd.json";
    let (env, dropped) = assemble(&[], &[pair("VK_ICD_FILENAMES", value)], &[], &roots());
    assert_eq!(env, []);
    assert_eq!(dropped, [drop_of("VK_ICD_FILENAMES", outside(value))]);
}

#[test]
fn keeps_vk_icd_filenames_inside_roots() {
    let value = format!("{ENGINE_DIR}/vulkan/MoltenVK_icd.json");
    let (env, dropped) = assemble(&[], &[pair("VK_ICD_FILENAMES", &value)], &[], &roots());
    assert_eq!(env, [pair("VK_ICD_FILENAMES", &value)]);
    assert_eq!(dropped, []);
}

#[test]
fn path_list_requires_every_element_inside_roots() {
    let inside = format!("{OVERLAY_DIR}/vulkan/kk.json");
    let mixed = format!("{inside}:/usr/share/vulkan/icd.d/x.json");
    let (env, dropped) = assemble(&[], &[pair("VK_ICD_FILENAMES", &mixed)], &[], &roots());
    assert_eq!(env, []);
    assert_eq!(dropped.len(), 1);

    let both = format!("{inside}:{PREFIX_DIR}/icd.json");
    let (env, dropped) = assemble(&[], &[pair("GST_PLUGIN_PATH", &both)], &[], &roots());
    assert_eq!(env, [pair("GST_PLUGIN_PATH", &both)]);
    assert_eq!(dropped, []);
}

#[test]
fn rejects_traversal_relative_and_empty_paths() {
    let climbs = format!("{ENGINE_DIR}/../../../etc");
    let sibling = format!("{ENGINE_DIR}-other/x");
    for value in [
        climbs.as_str(),
        "engines/rosetta-wine/10.0.0/x",
        "",
        sibling.as_str(),
    ] {
        let (env, dropped) = assemble(&[], &[pair("DXMT_CONFIG_FILE", value)], &[], &roots());
        assert!(env.is_empty(), "{value:?} should be dropped");
        assert_eq!(dropped, [drop_of("DXMT_CONFIG_FILE", outside(value))]);
    }
}

#[test]
fn extras_override_base_and_output_is_in_allowlist_order() {
    let base = [
        pair("WINEDEBUG", "-all"),
        pair("HOME", "/Users/me"),
        pair("PATH", "/usr/bin"),
    ];
    let extras = [pair("WINEDEBUG", "+seh")];
    let engine = [pair("APPLEJACK_SESSION", "abc")];
    let (env, dropped) = assemble(&base, &extras, &engine, &roots());
    assert_eq!(
        env,
        [
            pair("PATH", "/usr/bin"),
            pair("HOME", "/Users/me"),
            pair("WINEDEBUG", "+seh"),
            pair("APPLEJACK_SESSION", "abc"),
        ]
    );
    assert_eq!(dropped, [drop_of("WINEDEBUG", DropReason::Superseded)]);
}

#[test]
fn engine_owned_session_cannot_be_spoofed_by_extras() {
    let base = [
        pair("APPLEJACK_SESSION", "from-host"),
        pair("WINEDLLOVERRIDES", "d3d11=n"),
    ];
    let extras = [
        pair("APPLEJACK_SESSION", "spoofed"),
        pair("WINEPREFIX", PREFIX_DIR),
        pair("WINEMSYNC", "0"),
    ];
    let engine = [
        pair("APPLEJACK_SESSION", "real"),
        pair("WINEPREFIX", PREFIX_DIR),
        pair("WINEMSYNC", "1"),
    ];
    let (env, dropped) = assemble(&base, &extras, &engine, &roots());
    assert_eq!(
        env,
        [
            pair("WINEPREFIX", PREFIX_DIR),
            pair("WINEMSYNC", "1"),
            pair("APPLEJACK_SESSION", "real"),
        ]
    );
    assert_eq!(
        dropped,
        [
            drop_of("APPLEJACK_SESSION", DropReason::NotOwner),
            drop_of("WINEDLLOVERRIDES", DropReason::NotOwner),
            drop_of("APPLEJACK_SESSION", DropReason::NotOwner),
            drop_of("WINEPREFIX", DropReason::NotOwner),
            drop_of("WINEMSYNC", DropReason::NotOwner),
        ]
    );
}

#[test]
fn host_owned_variables_ignore_game_extras() {
    let base = [pair("HOME", "/Users/me")];
    let extras = [pair("HOME", "/elsewhere"), pair("PATH", "/evil/bin")];
    let engine = [pair("PATH", "/data/engines/rosetta-wine/10.0.0/wine/bin")];
    let (env, dropped) = assemble(&base, &extras, &engine, &roots());
    assert_eq!(
        env,
        [
            pair("PATH", "/data/engines/rosetta-wine/10.0.0/wine/bin"),
            pair("HOME", "/Users/me"),
        ]
    );
    assert_eq!(
        dropped,
        [
            drop_of("HOME", DropReason::NotOwner),
            drop_of("PATH", DropReason::NotOwner),
        ]
    );
}

#[test]
fn wineprefix_accepts_only_the_prefix_root() {
    let engine = [pair("WINEPREFIX", &format!("{ENGINE_DIR}/wine"))];
    let (env, dropped) = assemble(&[], &[], &engine, &roots());
    assert_eq!(env, []);
    assert_eq!(
        dropped,
        [drop_of(
            "WINEPREFIX",
            outside(&format!("{ENGINE_DIR}/wine"))
        )]
    );

    let engine = [pair("WINEPREFIX", &format!("{OVERLAY_DIR}/pfx"))];
    let (env, _) = assemble(&[], &[], &engine, &roots());
    assert_eq!(env, []);

    let engine = [pair("WINEPREFIX", &format!("{PREFIX_DIR}/game-1"))];
    let (env, dropped) = assemble(&[], &[], &engine, &roots());
    assert_eq!(env, [pair("WINEPREFIX", &format!("{PREFIX_DIR}/game-1"))]);
    assert_eq!(dropped, []);
}

#[test]
fn rejected_extra_falls_back_to_base() {
    let inside = format!("{PREFIX_DIR}/dxmt.conf");
    let base = [pair("DXMT_CONFIG_FILE", &inside)];
    let extras = [pair("DXMT_CONFIG_FILE", "/elsewhere/dxmt.conf")];
    let (env, dropped) = assemble(&base, &extras, &[], &roots());
    assert_eq!(env, [pair("DXMT_CONFIG_FILE", &inside)]);
    assert_eq!(
        dropped,
        [drop_of("DXMT_CONFIG_FILE", outside("/elsewhere/dxmt.conf"))]
    );
}

#[test]
fn dxmt_config_is_plain_and_config_file_is_a_path() {
    let options = "d3d11.preferredMaxFrameRate=60;d3d11.metalSpatialUpscaleFactor=2";
    let extras = [
        pair("DXMT_CONFIG", options),
        pair("DXMT_CONFIG_FILE", "/etc/dxmt.conf"),
    ];
    let (env, dropped) = assemble(&[], &extras, &[], &roots());
    assert_eq!(env, [pair("DXMT_CONFIG", options)]);
    assert_eq!(
        dropped,
        [drop_of("DXMT_CONFIG_FILE", outside("/etc/dxmt.conf"))]
    );

    let inside = format!("{OVERLAY_DIR}/dxmt.conf");
    let (env, dropped) = assemble(&[], &[pair("DXMT_CONFIG_FILE", &inside)], &[], &roots());
    assert_eq!(env, [pair("DXMT_CONFIG_FILE", &inside)]);
    assert_eq!(dropped, []);
}

#[test]
fn drops_path_through_prefix_z_drive_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let prefix = dir.path().join("prefix");
    fs::create_dir_all(prefix.join("dosdevices")).unwrap();
    std::os::unix::fs::symlink("/", prefix.join("dosdevices/z:")).unwrap();
    let roots = AllowedRoots::new(
        &dir.path().join("engine"),
        &dir.path().join("overlay"),
        &prefix,
    )
    .unwrap();

    let through_z = prefix.join("dosdevices/z:/etc/hosts");
    let through_z = through_z.to_str().unwrap();
    let (env, dropped) = assemble(&[], &[pair("DXMT_CONFIG_FILE", through_z)], &[], &roots);
    assert_eq!(env, []);
    assert_eq!(dropped, [drop_of("DXMT_CONFIG_FILE", outside(through_z))]);

    // A path list element through the link is dropped too.
    let list = format!("{through_z}:{}", prefix.join("plugins").display());
    let (env, dropped) = assemble(&[], &[pair("GST_PLUGIN_PATH", &list)], &[], &roots);
    assert_eq!(env, []);
    assert_eq!(dropped.len(), 1);

    // A file that does not exist yet under the real prefix is still inside,
    // and the temp dir's own `/var` versus `/private/var` spelling does not
    // matter because both sides are canonicalized.
    let not_yet = prefix.join("drive_c/users/me/dxmt.conf");
    let not_yet = not_yet.to_str().unwrap();
    let (env, dropped) = assemble(&[], &[pair("DXMT_CONFIG_FILE", not_yet)], &[], &roots);
    assert_eq!(env, [pair("DXMT_CONFIG_FILE", not_yet)]);
    assert_eq!(dropped, []);
}

#[test]
fn allowed_roots_rejects_empty_relative_and_root() {
    let ok = Path::new(PREFIX_DIR);
    for (bad, reason) in [
        ("", "is empty"),
        ("relative/prefix", "must be absolute"),
        ("/", "filesystem root"),
        ("/data/../prefixes", "must not contain `..`"),
    ] {
        for position in 0..3 {
            let mut dirs = [ok, ok, ok];
            dirs[position] = Path::new(bad);
            let error = AllowedRoots::new(dirs[0], dirs[1], dirs[2]).unwrap_err();
            assert!(
                matches!(error, Error::InvalidRoot { .. }),
                "{bad:?}: {error}"
            );
            assert!(error.to_string().contains(reason), "{bad:?}: {error}");
        }
    }

    // A root that is itself a symlink to `/` would admit everything.
    let dir = tempfile::tempdir().unwrap();
    let link = dir.path().join("z");
    std::os::unix::fs::symlink("/", &link).unwrap();
    let error = AllowedRoots::new(&link, ok, ok).unwrap_err();
    assert!(matches!(error, Error::InvalidRoot { .. }), "{error}");
    assert!(error.to_string().contains("filesystem root"), "{error}");

    let roots = roots();
    assert_eq!(roots.prefix_dir(), Path::new(PREFIX_DIR));
    assert_eq!(roots.engine_dir(), Path::new(ENGINE_DIR));
    assert_eq!(roots.overlay_dir(), Path::new(OVERLAY_DIR));
}
