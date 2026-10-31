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
/// line and exits 7, standing in for a console probe.
const WINE: &str = r#"#!/bin/sh
name=$(basename "$1")
if [ "$name" = wineboot ]; then
  mkdir -p "$WINEPREFIX/dosdevices" "$WINEPREFIX/drive_c/windows/system32" "$WINEPREFIX/drive_c/windows/syswow64" "$WINEPREFIX/drive_c/users/me"
fi
if [ "$name" = exit7.exe ]; then
  echo "hello from the stub: $2"
  exit 7
fi
exit 0
"#;

const SERVER: &str = "#!/bin/sh\nexit 0\n";

/// Prints the JSON line `cpuid` prints; `ROSETTA_ADVERTISE_AVX` changes it.
const CPUID: &str = r#"#!/bin/sh
avx=false
[ "$ROSETTA_ADVERTISE_AVX" = 1 ] && avx=true
echo "{\"avx\":$avx,\"avx2\":false,\"osxsave\":true,\"xcr0_ymm\":$avx}"
"#;

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
    script(&root.join("wine/bin/wine64"), WINE);
    script(&root.join("wine/bin/wineserver"), SERVER);
    script(&root.join("probes/cpuid"), CPUID);
    let toml = BUNDLE_TOML.replace("version = \"10.0.0\"", &format!("version = \"{version}\""));
    fs::write(root.join("BUNDLE.toml"), toml).unwrap();
}
