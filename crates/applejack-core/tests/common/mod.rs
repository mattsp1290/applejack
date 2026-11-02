//! A bundle tree with stub `wine` and `wineserver` scripts, so the engine,
//! prefix, and RPC paths run offline with nothing of Wine involved.

#![allow(dead_code, reason = "each test binary uses a subset of the helpers")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub const BUNDLE_TOML: &str = include_str!("../../../applejack-engine/tests/fixtures/BUNDLE.toml");

const BUNDLE_FILES: &[&str] = &[
    "wine/lib/wine/x86_64-windows/d3d11.dll",
    "wine/bin/wine64-preloader",
    "dxmt/d3d11.dll",
    "dxmt/dxgi.dll",
    "dxmt/d3d10core.dll",
    "dxmt/i386/d3d11.dll",
    "vkd3d/d3d12.dll",
    "vkd3d/d3d12core.dll",
    "probes/dx11-sample.exe",
    "probes/xinput-check.exe",
];

/// `wineboot` makes the directories a real boot would; `exit7.exe` prints a
/// line and exits 7, standing in for a console probe; `title.exe` becomes
/// [`stub_title`], a game whose argv names its exe; `stubborn.exe` the same
/// with `SIGTERM` ignored; `quick.exe` exits at once.
///
/// The title is this test binary and not the shell: macOS hides the
/// environment of platform binaries such as `/bin/sh` from `KERN_PROCARGS2`,
/// so a shell stub could never be found by its session.
const WINE: &str = r#"#!/bin/sh
name=$(basename "$1")
if [ "$name" = stubborn.exe ]; then
  trap '' TERM
fi
if [ "$name" = title.exe ] || [ "$name" = stubborn.exe ]; then
  echo "title up: $*"
  exec "@HELPER@" common::stub_title --exact --ignored --nocapture "$1"
fi
if [ "$name" = quick.exe ]; then
  exit 0
fi
if [ "$name" = wineboot ]; then
  mkdir -p "$WINEPREFIX/dosdevices" "$WINEPREFIX/drive_c/windows/system32" "$WINEPREFIX/drive_c/windows/syswow64" "$WINEPREFIX/drive_c/users/me"
fi
if [ "$name" = exit7.exe ]; then
  echo "hello from the stub: $2"
  exit 7
fi
exit 0
"#;

/// `-k` ends the stub title, as a real server ends everything in its prefix.
const SERVER: &str = r#"#!/bin/sh
[ "$1" = -k ] && [ -d "$WINEPREFIX" ] && echo 99 > "$WINEPREFIX/quit"
exit 0
"#;

/// Prints the JSON line `cpuid` prints; `ROSETTA_ADVERTISE_AVX` changes it.
const CPUID: &str = r#"#!/bin/sh
avx=false
[ "$ROSETTA_ADVERTISE_AVX" = 1 ] && avx=true
echo "{\"avx\":$avx,\"avx2\":false,\"osxsave\":true,\"xcr0_ymm\":$avx}"
"#;

/// What a stub title runs: stays until `$WINEPREFIX/quit` holds an exit code.
/// Without one there is nothing to wait for, for `cargo test -- --ignored`.
#[test]
#[ignore = "the stub title, started by the stub loader"]
fn stub_title() {
    let Some(prefix) = std::env::var_os("WINEPREFIX") else {
        return;
    };
    let quit = Path::new(&prefix).join("quit");
    loop {
        if let Some(code) = fs::read_to_string(&quit)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            std::process::exit(code);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Starts the stub title directly, with no loader: a title nobody spawned.
pub fn spawn_stub_title(key: &str, prefix: &Path) -> std::process::Child {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["common::stub_title", "--exact", "--ignored", "--nocapture"])
        .arg("title.exe")
        .env("APPLEJACK_SESSION", key)
        .env("WINEPREFIX", prefix)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap()
}

pub fn script(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A complete bundle directory at `root`, version `version`.
pub fn write_stub_bundle(root: &Path, version: &str) {
    for relative in BUNDLE_FILES {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, relative.as_bytes()).unwrap();
    }
    let helper = std::env::current_exe().unwrap();
    let wine = WINE.replace("@HELPER@", helper.to_str().unwrap());
    script(&root.join("wine/bin/wine64"), &wine);
    script(&root.join("wine/bin/wineserver"), SERVER);
    script(&root.join("probes/cpuid"), CPUID);
    let toml = BUNDLE_TOML.replace("version = \"10.0.0\"", &format!("version = \"{version}\""));
    fs::write(root.join("BUNDLE.toml"), toml).unwrap();
}

/// `<library>/steamapps` with a manifest and `<library>/steamapps/common/Title`
/// holding `exes`, as a finished install leaves them.
pub fn write_title(library: &Path, appid: u32, exes: &[&str]) {
    let common = library.join("steamapps/common/Title");
    fs::create_dir_all(&common).unwrap();
    for exe in exes {
        fs::write(common.join(exe), b"not a PE").unwrap();
    }
    fs::write(
        library.join(format!("steamapps/appmanifest_{appid}.acf")),
        format!(
            "\"AppState\"\n{{\n\t\"appid\"\t\t\"{appid}\"\n\t\"Universe\"\t\t\"1\"\n\t\"name\"\t\t\"Title\"\n\t\"StateFlags\"\t\t\"4\"\n\t\"installdir\"\t\t\"Title\"\n\t\"buildid\"\t\t\"100\"\n\t\"LastUpdated\"\t\t\"0\"\n\t\"SizeOnDisk\"\t\t\"8\"\n}}\n"
        ),
    )
    .unwrap();
}
