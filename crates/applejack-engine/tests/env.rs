//! The launch environment allowlist.

use std::ffi::OsString;
use std::path::PathBuf;

use applejack_engine::launch::env::{ALLOWLIST, AllowedRoots, DropReason, Dropped, assemble};

fn pair(name: &str, value: &str) -> (OsString, OsString) {
    (OsString::from(name), OsString::from(value))
}

fn roots() -> AllowedRoots {
    AllowedRoots {
        engine_dir: PathBuf::from("/data/engines/rosetta-wine/10.0.0"),
        overlay_dir: PathBuf::from("/data/engines/rosetta-wine/10.0.0.user"),
        prefix_dir: PathBuf::from("/data/prefixes/steam"),
    }
}

fn drop_of(name: &str, reason: DropReason) -> Dropped {
    Dropped {
        name: OsString::from(name),
        reason,
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
            "DXMT_LOG_LEVEL",
            "MTL_HUD_ENABLED",
            "ROSETTA_ADVERTISE_AVX",
            "GST_PLUGIN_PATH",
            "APPLEJACK_SESSION",
            "VK_ICD_FILENAMES",
        ]
    );
}

#[test]
fn drops_unknown_variable() {
    let base = [
        pair("PATH", "/usr/bin"),
        pair("LD_PRELOAD", "x"),
        pair("FOO", "bar"),
    ];
    let (env, dropped) = assemble(&base, &[], &roots());
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
    let (env, dropped) = assemble(&[], &extras, &roots());
    assert_eq!(env, []);
    assert_eq!(
        dropped,
        [drop_of("DYLD_INSERT_LIBRARIES", DropReason::Dyld)]
    );
}

#[test]
fn drops_vk_icd_filenames_outside_roots() {
    let value = "/opt/homebrew/share/vulkan/icd.d/MoltenVK_icd.json";
    let (env, dropped) = assemble(&[], &[pair("VK_ICD_FILENAMES", value)], &roots());
    assert_eq!(env, []);
    assert_eq!(
        dropped,
        [drop_of(
            "VK_ICD_FILENAMES",
            DropReason::OutsideRoots {
                value: OsString::from(value)
            }
        )]
    );
}

#[test]
fn keeps_vk_icd_filenames_inside_roots() {
    let value = "/data/engines/rosetta-wine/10.0.0/vulkan/MoltenVK_icd.json";
    let (env, dropped) = assemble(&[], &[pair("VK_ICD_FILENAMES", value)], &roots());
    assert_eq!(env, [pair("VK_ICD_FILENAMES", value)]);
    assert_eq!(dropped, []);
}

#[test]
fn path_list_requires_every_element_inside_roots() {
    let inside = "/data/engines/rosetta-wine/10.0.0.user/vulkan/kk.json";
    let mixed = format!("{inside}:/usr/share/vulkan/icd.d/x.json");
    let (env, dropped) = assemble(&[], &[pair("VK_ICD_FILENAMES", &mixed)], &roots());
    assert_eq!(env, []);
    assert_eq!(dropped.len(), 1);

    let both = format!("{inside}:/data/prefixes/steam/icd.json");
    let (env, dropped) = assemble(&[], &[pair("GST_PLUGIN_PATH", &both)], &roots());
    assert_eq!(env, [pair("GST_PLUGIN_PATH", &both)]);
    assert_eq!(dropped, []);
}

#[test]
fn rejects_traversal_relative_and_empty_paths() {
    for value in [
        "/data/engines/rosetta-wine/10.0.0/../../../etc",
        "engines/rosetta-wine/10.0.0/x",
        "",
        "/data/engines/rosetta-wine/10.0.0-other/x",
    ] {
        let (env, dropped) = assemble(&[], &[pair("WINEPREFIX", value)], &roots());
        assert!(env.is_empty(), "{value:?} should be dropped");
        assert_eq!(dropped.len(), 1, "{value:?}");
    }
}

#[test]
fn extras_override_base_and_output_is_in_allowlist_order() {
    let base = [
        pair("WINEDEBUG", "-all"),
        pair("HOME", "/Users/me"),
        pair("PATH", "/usr/bin"),
    ];
    let extras = [pair("WINEDEBUG", "+seh"), pair("APPLEJACK_SESSION", "abc")];
    let (env, dropped) = assemble(&base, &extras, &roots());
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
