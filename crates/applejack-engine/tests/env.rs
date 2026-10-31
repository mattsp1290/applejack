//! The launch environment allowlist.

use std::ffi::OsString;
use std::fs;
use std::path::Path;

use applejack_engine::launch::env::{
    ALLOWLIST, AllowedRoots, DropReason, Dropped, Kind, Owner, assemble,
};
use applejack_engine::{EnvPair, Error};

fn pair(name: &str, value: &str) -> EnvPair {
    (OsString::from(name), OsString::from(value))
}

const ENGINE_DIR: &str = "/data/engines/rosetta-wine/10.0.0";
const OVERLAY_DIR: &str = "/data/engines/rosetta-wine/10.0.0.user";
const PREFIX_DIR: &str = "/data/prefixes/steam";
const SESSION: &str = "session-1";

/// Roots that do not exist on disk; they resolve to themselves.
fn roots() -> AllowedRoots {
    AllowedRoots::new(
        Path::new(ENGINE_DIR),
        Path::new(OVERLAY_DIR),
        Path::new(PREFIX_DIR),
    )
    .unwrap()
}

fn drop_of(name: &str, reason: DropReason, from: Owner) -> Dropped {
    Dropped {
        name: OsString::from(name),
        reason,
        from,
    }
}

fn outside(value: &str) -> DropReason {
    DropReason::OutsideRoots {
        value: OsString::from(value),
    }
}

/// The two rows every launch must carry, as the engine would set them.
fn required(prefix: &str) -> [EnvPair; 2] {
    [
        pair("WINEPREFIX", prefix),
        pair("APPLEJACK_SESSION", SESSION),
    ]
}

/// Runs `assemble` with the required engine rows added to `engine` unless
/// `engine` names them itself, and strips the rows it added from the
/// accepted list so each test states only what it is about. The required
/// rows have their own tests below.
fn assemble_with(
    roots: &AllowedRoots,
    prefix: &str,
    base: &[EnvPair],
    extras: &[EnvPair],
    engine: &[EnvPair],
) -> (Vec<EnvPair>, Vec<Dropped>) {
    let mut engine_set = engine.to_vec();
    let mut added = Vec::new();
    for (name, value) in required(prefix) {
        if !engine.iter().any(|(given, _)| *given == name) {
            added.push(name.clone());
            engine_set.push((name, value));
        }
    }
    let (env, dropped) = assemble(base, extras, &engine_set, roots).unwrap();
    let env = env
        .into_iter()
        .filter(|(name, _)| !added.contains(name))
        .collect();
    (env, dropped)
}

fn run(base: &[EnvPair], extras: &[EnvPair], engine: &[EnvPair]) -> (Vec<EnvPair>, Vec<Dropped>) {
    assemble_with(&roots(), PREFIX_DIR, base, extras, engine)
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
            "ROSETTA_ADVERTISE_AVX",
            "APPLEJACK_SESSION",
        ]
    );
    let required: Vec<&str> = ALLOWLIST
        .iter()
        .filter(|entry| entry.required)
        .map(|entry| entry.name)
        .collect();
    assert_eq!(required, ["WINEPREFIX", "APPLEJACK_SESSION"]);
    for entry in ALLOWLIST {
        let is_path = matches!(entry.kind, Kind::Path | Kind::PrefixDir | Kind::PathList);
        let has_roots = entry.roots.engine || entry.roots.overlay || entry.roots.prefix;
        assert_eq!(
            is_path, has_roots,
            "{}: roots only on path rows",
            entry.name
        );
        assert!(
            !entry.required || entry.owner == Owner::Engine,
            "{}: only the engine can satisfy a required row",
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
    let (env, dropped) = run(&base, &[], &[]);
    assert_eq!(env, [pair("PATH", "/usr/bin")]);
    assert_eq!(
        dropped,
        [
            drop_of("LD_PRELOAD", DropReason::NotAllowlisted, Owner::Host),
            drop_of("FOO", DropReason::NotAllowlisted, Owner::Host),
        ]
    );
}

#[test]
fn drops_dyld_insert_libraries() {
    let extras = [pair("DYLD_INSERT_LIBRARIES", "/tmp/evil.dylib")];
    let (env, dropped) = run(&[], &extras, &[]);
    assert_eq!(env, []);
    assert_eq!(
        dropped,
        [drop_of(
            "DYLD_INSERT_LIBRARIES",
            DropReason::Dyld,
            Owner::Game
        )]
    );
}

#[test]
fn drops_vk_icd_filenames_outside_roots() {
    let value = "/opt/homebrew/share/vulkan/icd.d/MoltenVK_icd.json";
    let (env, dropped) = run(&[], &[pair("VK_ICD_FILENAMES", value)], &[]);
    assert_eq!(env, []);
    assert_eq!(
        dropped,
        [drop_of("VK_ICD_FILENAMES", outside(value), Owner::Game)]
    );
}

#[test]
fn keeps_vk_icd_filenames_inside_roots() {
    let value = format!("{ENGINE_DIR}/vulkan/MoltenVK_icd.json");
    let (env, dropped) = run(&[], &[pair("VK_ICD_FILENAMES", &value)], &[]);
    assert_eq!(env, [pair("VK_ICD_FILENAMES", &value)]);
    assert_eq!(dropped, []);
}

#[test]
fn path_list_requires_every_element_inside_roots() {
    let inside = format!("{OVERLAY_DIR}/vulkan/kk.json");
    let mixed = format!("{inside}:/usr/share/vulkan/icd.d/x.json");
    let (env, dropped) = run(&[], &[pair("VK_ICD_FILENAMES", &mixed)], &[]);
    assert_eq!(env, []);
    assert_eq!(dropped.len(), 1);

    let both = format!("{inside}:{PREFIX_DIR}/icd.json");
    let (env, dropped) = run(&[], &[pair("GST_PLUGIN_PATH", &both)], &[]);
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
        let (env, dropped) = run(&[], &[pair("DXMT_CONFIG_FILE", value)], &[]);
        assert!(env.is_empty(), "{value:?} should be dropped");
        assert_eq!(
            dropped,
            [drop_of("DXMT_CONFIG_FILE", outside(value), Owner::Game)]
        );
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
    let engine = [
        pair("APPLEJACK_SESSION", "abc"),
        pair("WINEPREFIX", PREFIX_DIR),
    ];
    let (env, dropped) = assemble(&base, &extras, &engine, &roots()).unwrap();
    assert_eq!(
        env,
        [
            pair("PATH", "/usr/bin"),
            pair("HOME", "/Users/me"),
            pair("WINEPREFIX", PREFIX_DIR),
            pair("WINEDEBUG", "+seh"),
            pair("APPLEJACK_SESSION", "abc"),
        ]
    );
    // The superseded value came from the host tier.
    assert_eq!(
        dropped,
        [drop_of("WINEDEBUG", DropReason::Superseded, Owner::Host)]
    );
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
    let (env, dropped) = assemble(&base, &extras, &engine, &roots()).unwrap();
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
            drop_of("APPLEJACK_SESSION", DropReason::NotOwner, Owner::Host),
            drop_of("WINEDLLOVERRIDES", DropReason::NotOwner, Owner::Host),
            drop_of("APPLEJACK_SESSION", DropReason::NotOwner, Owner::Game),
            drop_of("WINEPREFIX", DropReason::NotOwner, Owner::Game),
            drop_of("WINEMSYNC", DropReason::NotOwner, Owner::Game),
        ]
    );
}

#[test]
fn host_owned_variables_ignore_game_extras() {
    let base = [pair("HOME", "/Users/me")];
    let extras = [pair("HOME", "/elsewhere"), pair("PATH", "/evil/bin")];
    let engine = [pair("PATH", "/data/engines/rosetta-wine/10.0.0/wine/bin")];
    let (env, dropped) = run(&base, &extras, &engine);
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
            drop_of("HOME", DropReason::NotOwner, Owner::Game),
            drop_of("PATH", DropReason::NotOwner, Owner::Game),
        ]
    );
}

#[test]
fn wineprefix_accepts_only_the_prefix_root() {
    // An engine-set prefix that is not the prefix directory itself is
    // dropped (Wine would create a fresh prefix at `<prefix>/game-1`), and
    // since no other tier may supply it, the launch cannot proceed. The
    // error says the engine set a value and why it was rejected.
    for bad in [
        format!("{ENGINE_DIR}/wine"),
        format!("{OVERLAY_DIR}/pfx"),
        format!("{PREFIX_DIR}/game-1"),
        format!("{PREFIX_DIR}/drive_c"),
        "/data/prefixes".to_owned(),
    ] {
        let engine = [pair("WINEPREFIX", &bad), pair("APPLEJACK_SESSION", SESSION)];
        let error = assemble(&[], &[], &engine, &roots()).unwrap_err();
        assert!(
            matches!(
                &error,
                Error::MissingEngineVariable {
                    name: "WINEPREFIX",
                    rejected: Some(DropReason::OutsideRoots { value })
                } if *value == OsString::from(&bad)
            ),
            "{bad}: {error:?}"
        );
        let message = error.to_string();
        assert!(message.contains("rejected value"), "{bad}: {message}");
        assert!(message.contains(&bad), "{bad}: {message}");
        assert!(!message.contains("did not set"), "{bad}: {message}");
    }

    // Any spelling that resolves to the prefix directory is the prefix.
    for exact in [
        PREFIX_DIR.to_owned(),
        format!("{PREFIX_DIR}/"),
        format!("{PREFIX_DIR}/."),
    ] {
        let engine = [
            pair("WINEPREFIX", &exact),
            pair("APPLEJACK_SESSION", SESSION),
        ];
        let (env, dropped) = assemble(&[], &[], &engine, &roots()).unwrap();
        assert_eq!(
            env,
            [
                pair("WINEPREFIX", &exact),
                pair("APPLEJACK_SESSION", SESSION)
            ]
        );
        assert_eq!(dropped, []);
    }
}

#[test]
fn empty_session_id_is_rejected() {
    let engine = [
        pair("WINEPREFIX", PREFIX_DIR),
        pair("APPLEJACK_SESSION", ""),
    ];
    let error = assemble(&[], &[], &engine, &roots()).unwrap_err();
    assert!(
        matches!(
            error,
            Error::MissingEngineVariable {
                name: "APPLEJACK_SESSION",
                rejected: Some(DropReason::Empty)
            }
        ),
        "{error:?}"
    );
    assert!(
        error.to_string().contains("rejected value: empty"),
        "{error}"
    );

    // A good engine value is kept when a later engine value is rejected;
    // the rejection is reported, not the launch refused.
    let engine = [
        pair("WINEPREFIX", PREFIX_DIR),
        pair("APPLEJACK_SESSION", SESSION),
        pair("APPLEJACK_SESSION", ""),
    ];
    let (env, dropped) = assemble(&[], &[], &engine, &roots()).unwrap();
    assert_eq!(
        env,
        [
            pair("WINEPREFIX", PREFIX_DIR),
            pair("APPLEJACK_SESSION", SESSION)
        ]
    );
    assert_eq!(
        dropped,
        [drop_of(
            "APPLEJACK_SESSION",
            DropReason::Empty,
            Owner::Engine
        )]
    );
}

#[test]
fn missing_required_engine_variable_is_an_error() {
    let base = [pair("PATH", "/usr/bin"), pair("WINEPREFIX", PREFIX_DIR)];
    // A host WINEPREFIX does not count: only the engine may set it, and the
    // host value's rejection is not what the error reports.
    let error = assemble(&base, &[], &[], &roots()).unwrap_err();
    assert!(
        matches!(
            error,
            Error::MissingEngineVariable {
                name: "WINEPREFIX",
                rejected: None
            }
        ),
        "{error:?}"
    );
    assert!(
        error
            .to_string()
            .contains("did not set the required launch variable WINEPREFIX"),
        "{error}"
    );

    let engine = [pair("WINEPREFIX", PREFIX_DIR)];
    let error = assemble(&base, &[], &engine, &roots()).unwrap_err();
    assert!(
        matches!(
            error,
            Error::MissingEngineVariable {
                name: "APPLEJACK_SESSION",
                rejected: None
            }
        ),
        "{error:?}"
    );

    let (env, _) = assemble(&base, &[], &required(PREFIX_DIR), &roots()).unwrap();
    assert_eq!(
        env,
        [
            pair("PATH", "/usr/bin"),
            pair("WINEPREFIX", PREFIX_DIR),
            pair("APPLEJACK_SESSION", SESSION),
        ]
    );
}

#[test]
fn rejected_extra_falls_back_to_base() {
    let inside = format!("{PREFIX_DIR}/dxmt.conf");
    let base = [pair("DXMT_CONFIG_FILE", &inside)];
    let extras = [pair("DXMT_CONFIG_FILE", "/elsewhere/dxmt.conf")];
    let (env, dropped) = run(&base, &extras, &[]);
    assert_eq!(env, [pair("DXMT_CONFIG_FILE", &inside)]);
    assert_eq!(
        dropped,
        [drop_of(
            "DXMT_CONFIG_FILE",
            outside("/elsewhere/dxmt.conf"),
            Owner::Game
        )]
    );
}

#[test]
fn dxmt_config_is_plain_and_config_file_is_a_path() {
    let options = "d3d11.preferredMaxFrameRate=60;d3d11.metalSpatialUpscaleFactor=2";
    let extras = [
        pair("DXMT_CONFIG", options),
        pair("DXMT_CONFIG_FILE", "/etc/dxmt.conf"),
    ];
    let (env, dropped) = run(&[], &extras, &[]);
    assert_eq!(env, [pair("DXMT_CONFIG", options)]);
    assert_eq!(
        dropped,
        [drop_of(
            "DXMT_CONFIG_FILE",
            outside("/etc/dxmt.conf"),
            Owner::Game
        )]
    );

    let inside = format!("{OVERLAY_DIR}/dxmt.conf");
    let (env, dropped) = run(&[], &[pair("DXMT_CONFIG_FILE", &inside)], &[]);
    assert_eq!(env, [pair("DXMT_CONFIG_FILE", &inside)]);
    assert_eq!(dropped, []);
}

/// Roots under a temp dir with a prefix that exists on disk.
fn disk_roots(dir: &Path) -> (AllowedRoots, std::path::PathBuf) {
    let prefix = dir.join("prefix");
    fs::create_dir_all(prefix.join("dosdevices")).unwrap();
    fs::create_dir_all(prefix.join("drive_c")).unwrap();
    let roots = AllowedRoots::new(&dir.join("engine"), &dir.join("overlay"), &prefix).unwrap();
    (roots, prefix)
}

#[test]
fn drops_path_through_prefix_z_drive_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let (roots, prefix) = disk_roots(dir.path());
    std::os::unix::fs::symlink("/", prefix.join("dosdevices/z:")).unwrap();
    let prefix_text = prefix.to_str().unwrap();
    let run = |extras: &[EnvPair]| assemble_with(&roots, prefix_text, &[], extras, &[]);

    let through_z = prefix.join("dosdevices/z:/etc/hosts");
    let through_z = through_z.to_str().unwrap();
    let (env, dropped) = run(&[pair("DXMT_CONFIG_FILE", through_z)]);
    assert_eq!(env, []);
    assert_eq!(
        dropped,
        [drop_of("DXMT_CONFIG_FILE", outside(through_z), Owner::Game)]
    );

    // A path list element through the link is dropped too.
    let list = format!("{through_z}:{}", prefix.join("plugins").display());
    let (env, dropped) = run(&[pair("GST_PLUGIN_PATH", &list)]);
    assert_eq!(env, []);
    assert_eq!(dropped.len(), 1);

    // A file that does not exist yet under the real prefix is still inside,
    // and the temp dir's own `/var` versus `/private/var` spelling does not
    // matter because both sides are canonicalized.
    let not_yet = prefix.join("drive_c/users/me/dxmt.conf");
    let not_yet = not_yet.to_str().unwrap();
    let (env, dropped) = run(&[pair("DXMT_CONFIG_FILE", not_yet)]);
    assert_eq!(env, [pair("DXMT_CONFIG_FILE", not_yet)]);
    assert_eq!(dropped, []);
}

#[test]
fn drops_path_through_dangling_symlink_inside_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let (roots, prefix) = disk_roots(dir.path());
    // Wine prefixes carry such links (`dosdevices/<x>:` to an unmounted
    // volume); the target could be created later, outside every root.
    let outside_dir = dir.path().join("outside-not-yet");
    std::os::unix::fs::symlink(&outside_dir, prefix.join("drive_c/later")).unwrap();
    let prefix_text = prefix.to_str().unwrap();

    let value = prefix.join("drive_c/later/evil.conf");
    let value = value.to_str().unwrap();
    let (env, dropped) = assemble_with(
        &roots,
        prefix_text,
        &[],
        &[pair("DXMT_CONFIG_FILE", value)],
        &[],
    );
    assert_eq!(env, []);
    assert_eq!(
        dropped,
        [drop_of("DXMT_CONFIG_FILE", outside(value), Owner::Game)]
    );

    // Once the target exists the path resolves outside and is still dropped.
    fs::create_dir_all(&outside_dir).unwrap();
    let (env, dropped) = assemble_with(
        &roots,
        prefix_text,
        &[],
        &[pair("DXMT_CONFIG_FILE", value)],
        &[],
    );
    assert_eq!(env, []);
    assert_eq!(dropped.len(), 1);

    // A dangling link as a root is refused outright.
    let error = AllowedRoots::new(
        &dir.path().join("engine"),
        &dir.path().join("overlay"),
        &prefix.join("drive_c/later"),
    )
    .unwrap_err();
    assert!(matches!(error, Error::InvalidRoot { .. }), "{error}");
}

#[test]
fn drops_trailing_slash_spellings_of_a_dangling_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let (roots, prefix) = disk_roots(dir.path());
    let outside_dir = dir.path().join("outside-not-yet");
    std::os::unix::fs::symlink(&outside_dir, prefix.join("drive_c/later")).unwrap();
    let prefix_text = prefix.to_str().unwrap();

    // `lstat("later/")` follows the link and reports "missing", which would
    // let the value through as "not created yet" until the volume appears.
    for (name, value) in [
        ("GST_PLUGIN_PATH", prefix.join("drive_c/later/")),
        ("GST_PLUGIN_PATH", prefix.join("drive_c/later/.")),
        ("DXMT_CONFIG_FILE", prefix.join("drive_c/later/evil.conf/")),
        ("VK_ICD_FILENAMES", prefix.join("drive_c/later/./icd.json")),
    ] {
        let value = value.to_str().unwrap();
        let (env, dropped) = assemble_with(&roots, prefix_text, &[], &[pair(name, value)], &[]);
        assert_eq!(env, [], "{name}={value}");
        assert_eq!(dropped, [drop_of(name, outside(value), Owner::Game)]);
    }

    // The same spellings of a real directory still resolve inside.
    let plugins = prefix.join("drive_c/plugins/");
    let plugins = plugins.to_str().unwrap();
    let (env, dropped) = assemble_with(
        &roots,
        prefix_text,
        &[],
        &[pair("GST_PLUGIN_PATH", plugins)],
        &[],
    );
    assert_eq!(env, [pair("GST_PLUGIN_PATH", plugins)]);
    assert_eq!(dropped, []);
}

#[test]
fn allowed_roots_rejects_symlink_root_with_trailing_slash() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    fs::create_dir_all(&real).unwrap();
    let link = dir.path().join("homelink");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let ok = Path::new(PREFIX_DIR);

    for spelling in [
        format!("{}/", link.display()),
        format!("{}/.", link.display()),
    ] {
        let error = AllowedRoots::new(Path::new(&spelling), ok, ok).unwrap_err();
        assert!(
            matches!(error, Error::InvalidRoot { .. }),
            "{spelling}: {error}"
        );
        assert!(
            error.to_string().contains("must not be a symlink"),
            "{spelling}: {error}"
        );
    }
    // A real directory spelled with a trailing slash is the directory.
    let roots = AllowedRoots::new(Path::new(&format!("{}/", real.display())), ok, ok).unwrap();
    assert_eq!(roots.engine_dir(), fs::canonicalize(&real).unwrap());
}

#[test]
fn allowed_roots_rejects_empty_relative_root_and_symlinks() {
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

    // A root that is itself a symlink would widen the allowed area to
    // wherever it points, whether `/` or a real directory.
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    fs::create_dir_all(&real).unwrap();
    for (name, target) in [("z", Path::new("/")), ("homelink", real.as_path())] {
        let link = dir.path().join(name);
        std::os::unix::fs::symlink(target, &link).unwrap();
        let error = AllowedRoots::new(&link, ok, ok).unwrap_err();
        assert!(
            matches!(error, Error::InvalidRoot { .. }),
            "{name}: {error}"
        );
        assert!(
            error.to_string().contains("must not be a symlink"),
            "{name}: {error}"
        );
    }
    // A real directory under the temp dir is fine, and so is one that does
    // not exist yet.
    AllowedRoots::new(&real, ok, &dir.path().join("not-yet")).unwrap();

    let roots = roots();
    assert_eq!(roots.prefix_dir(), Path::new(PREFIX_DIR));
    assert_eq!(roots.engine_dir(), Path::new(ENGINE_DIR));
    assert_eq!(roots.overlay_dir(), Path::new(OVERLAY_DIR));
}

/// `ROSETTA_ADVERTISE_AVX` is engine-owned: the engine sets it only from the
/// recorded CPUID probe, so neither the user's shell nor a game preset can
/// advertise AVX on their own.
#[test]
fn advertise_avx_comes_only_from_the_engine() {
    let base = [pair("ROSETTA_ADVERTISE_AVX", "1")];
    let extras = [pair("ROSETTA_ADVERTISE_AVX", "1")];
    let engine = [
        pair("APPLEJACK_SESSION", "real"),
        pair("WINEPREFIX", PREFIX_DIR),
    ];
    let (env, dropped) = assemble(&base, &extras, &engine, &roots()).unwrap();
    assert!(
        env.iter().all(|(name, _)| name != "ROSETTA_ADVERTISE_AVX"),
        "{env:?}"
    );
    assert_eq!(
        dropped,
        [
            drop_of("ROSETTA_ADVERTISE_AVX", DropReason::NotOwner, Owner::Host),
            drop_of("ROSETTA_ADVERTISE_AVX", DropReason::NotOwner, Owner::Game),
        ]
    );
    let engine = [
        pair("APPLEJACK_SESSION", "real"),
        pair("WINEPREFIX", PREFIX_DIR),
        pair("ROSETTA_ADVERTISE_AVX", "1"),
    ];
    let (env, _) = assemble(&base, &extras, &engine, &roots()).unwrap();
    assert!(env.contains(&pair("ROSETTA_ADVERTISE_AVX", "1")));
}
