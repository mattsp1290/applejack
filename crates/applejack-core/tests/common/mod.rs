//! A bundle tree with stub `wine` and `wineserver` scripts, so the engine,
//! prefix, and RPC paths run offline with nothing of Wine involved.

#![allow(dead_code, reason = "each test binary uses a subset of the helpers")]

use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;

use applejack_engine::rosetta_wine::process::server_socket;

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
    "probes/steamwebhelper-shim.exe",
];

/// `wineboot` makes the directories a real boot would; `exit7.exe` prints a
/// line and exits 7, standing in for a console probe; `title.exe` becomes
/// [`stub_title`], a game whose argv names its exe; `stubborn.exe` the same
/// with `SIGTERM` ignored; `quick.exe` exits at once; `launcher.exe` leaves
/// `game.exe` running and exits, `immortal.exe` leaves a helper no server stop
/// or `quit` file ends, and `forking.exe` is a launcher that takes a second
/// before it starts `helper.exe`. The arguments go to `$WINEPREFIX/args`, so a
/// test can see them without the log having to.
///
/// The Steam client, in the shape the real one has: `SteamSetup.exe /S` lays
/// only `steam.exe` under `drive_c/Program Files (x86)/Steam`; the first
/// `steam.exe` start is the self-update, which writes the client manifest
/// and the CEF tree (`bin/cef/cef.win7x64/steamwebhelper.exe` =
/// `REAL-HELPER-1`) unless `$WINEPREFIX/no-self-update` is there (a
/// self-update that never gets that far); a later start lays a missing
/// helper down only when its file check runs (no `-noverifyfiles`) and
/// `$WINEPREFIX/no-file-check` is absent (under it the check lays nothing down, the live shape the
/// recovery has not been seen to rule out), and `$WINEPREFIX/self-update`
/// makes it write `REAL-HELPER-2` over the helper once, as a client update
/// does. `steam.exe` is a client (`steam.exe` plus a `steamwebhelper.exe`
/// when a helper file exists to run; without one no window line is written)
/// that stays until `$WINEPREFIX/client-quit`; its arguments append to
/// `$WINEPREFIX/client-args`; with
/// `-applaunch <appid>` it starts the client if none is up, then `game.exe`
/// as the title and `$WINEPREFIX/running-appid` as the registry value, which
/// `reg query` prints and `reg add` writes, and clears it when the game
/// exits. A `$WINEPREFIX/redownload` file makes `-applaunch` rewrite the
/// `.acf` as a client fetching the whole title does (update bits, same
/// build), `patch` as a client applying a routine update does (update bits,
/// a newer build), `never-start` makes it report nothing, and `no-marker`
/// starts the title without touching the value. A new client start clears a
/// `client-quit` left by a stop, and a new title start a `quit`.
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
  echo "title up: $name"
  shift
  echo "$*" > "$WINEPREFIX/args"
  exec "@HELPER@" common::stub_title --exact --ignored --nocapture "$name"
fi
if [ "$name" = launcher.exe ]; then
  "@HELPER@" common::stub_title --exact --ignored --nocapture game.exe &
  exit 3
fi
if [ "$name" = immortal.exe ]; then
  "@HELPER@" common::stub_title --exact --ignored --nocapture helper.exe &
  exec "@HELPER@" common::stub_title --exact --ignored --nocapture immortal.exe
fi
if [ "$name" = forking.exe ]; then
  sleep 1
  "@HELPER@" common::stub_title --exact --ignored --nocapture helper.exe &
  exit 3
fi
if [ "$name" = quick.exe ]; then
  exit 0
fi
if [ "$name" = SteamSetup.exe ]; then
  steam="$WINEPREFIX/drive_c/Program Files (x86)/Steam"
  mkdir -p "$steam/config"
  echo "$*" > "$steam/bootstrapped"
  : > "$steam/steam.exe"
  exit 0
fi
if [ "$name" = reg ] && [ "$2" = query ]; then
  [ -f "$WINEPREFIX/running-appid" ] || exit 1
  printf 'HKEY_CURRENT_USER\\Software\\Valve\\Steam\n    RunningAppID    REG_DWORD    0x%x\n' "$(cat "$WINEPREFIX/running-appid")"
  exit 0
fi
if [ "$name" = reg ] && [ "$2" = add ]; then
  data=
  while [ $# -gt 0 ]; do
    if [ "$1" = /d ]; then data=$2; shift; fi
    shift
  done
  echo "$data" > "$WINEPREFIX/running-appid"
  echo "$data" >> "$WINEPREFIX/reg-add"
  exit 0
fi
if [ "$name" = steam.exe ]; then
  echo "$*" >> "$WINEPREFIX/client-args"
  if [ "$2" = -applaunch ]; then
    appid=$3
    shift 3
    echo "$*" > "$WINEPREFIX/args"
    if [ -e "$WINEPREFIX/redownload" ]; then
      acf=$(ls "$WINEPREFIX"/dosdevices/s:/steamapps/appmanifest_*.acf | head -1)
      sed -i '' -e 's/"StateFlags"\t\t"4"/"StateFlags"\t\t"1026"/' "$acf"
      exit 0
    fi
    if [ -e "$WINEPREFIX/patch" ]; then
      acf=$(ls "$WINEPREFIX"/dosdevices/s:/steamapps/appmanifest_*.acf | head -1)
      sed -i '' -e 's/"StateFlags"\t\t"4"/"StateFlags"\t\t"1026"/' -e 's/"buildid"\t\t"100"/"buildid"\t\t"101"/' "$acf"
      exit 0
    fi
    if [ -e "$WINEPREFIX/never-start" ]; then
      exit 0
    fi
    rm -f "$WINEPREFIX/quit"
    if [ -e "$WINEPREFIX/no-marker" ]; then
      "@HELPER@" common::stub_title --exact --ignored --nocapture game.exe &
    else
      ( "@HELPER@" common::stub_title --exact --ignored --nocapture game.exe; echo 0 > "$WINEPREFIX/running-appid" ) &
      echo "$appid" > "$WINEPREFIX/running-appid"
    fi
    if [ -e "$WINEPREFIX/client-up" ]; then
      exit 0
    fi
  fi
  steam="$WINEPREFIX/drive_c/Program Files (x86)/Steam"
  mkdir -p "$steam/logs"
  if [ -e "$WINEPREFIX/stall-dump" ]; then
    mkdir -p "$steam/dumps"
    : > "$steam/dumps/assert_steam.exe_20260101000000_1.dmp"
    exit 0
  fi
  if [ -e "$WINEPREFIX/stall" ]; then
    printf '%s\n' 'src\clientdll\steamengine.cpp (2716) : Assertion Failed: CSteamEngine::BMainLoop appears to have stalled > 15 seconds without event signalled' >> "$steam/logs/console_log.txt"
    exit 0
  fi
  cef="$steam/bin/cef/cef.win7x64"
  if [ ! -f "$steam/package/steam_client_win32.manifest" ]; then
    if [ ! -e "$WINEPREFIX/no-self-update" ]; then
      mkdir -p "$steam/package" "$cef"
      printf '"steam_client_win32"\n{\n\t"version"\t\t"1761720601"\n}\n' > "$steam/package/steam_client_win32.manifest"
      [ -f "$cef/steamwebhelper.exe" ] || printf 'REAL-HELPER-1' > "$cef/steamwebhelper.exe"
    fi
  elif [ ! -f "$cef/steamwebhelper.exe" ] && [ ! -e "$WINEPREFIX/no-file-check" ]; then
    case " $* " in
      *" -noverifyfiles "*) ;;
      *) mkdir -p "$cef"; printf 'REAL-HELPER-1' > "$cef/steamwebhelper.exe" ;;
    esac
  fi
  if [ -e "$WINEPREFIX/self-update" ]; then
    mkdir -p "$cef"
    printf 'REAL-HELPER-2' > "$cef/steamwebhelper.exe"
    rm -f "$WINEPREFIX/self-update"
  fi
  # A client a stop just ended polls every 10 ms for its quit file; a fresh
  # start must not clear it from under them. The window line follows the
  # sleep so the launcher's wait, which counts lines past the start, sees it.
  sleep 0.1
  rm -f "$WINEPREFIX/client-quit"
  : > "$WINEPREFIX/client-up"
  if [ -f "$cef/steamwebhelper.exe" ]; then
    echo 'SP DesktopLoginWindow_uid0: Created window: size: 700,440 pos: 0,0 mode: System window: 0000000000030106' >> "$steam/logs/webhelper.txt"
    "@HELPER@" common::stub_title --exact --ignored --nocapture steamwebhelper.exe &
  fi
  exec "@HELPER@" common::stub_title --exact --ignored --nocapture steam.exe
fi
if [ "$name" = wineboot ]; then
  mkdir -p "$WINEPREFIX/dosdevices" "$WINEPREFIX/drive_c/windows/system32" "$WINEPREFIX/drive_c/windows/syswow64" "$WINEPREFIX/drive_c/users/me"
fi
if [ "$name" = exit7.exe ]; then
  echo "hello from the stub: $2"
  exit 7
fi
if [ "$name" = env.exe ]; then
  echo "WINEDEBUG=${WINEDEBUG:-unset} DXMT_LOG_LEVEL=${DXMT_LOG_LEVEL:-unset} NOT_ALLOWED=${NOT_ALLOWED:-unset}"
  exit 0
fi
exit 0
"#;

/// With a [`stub_server`] up, `-k` ends it and the stub title, as a real server
/// ends everything in its prefix. With none up it does nothing, as Wine's does
/// in the window before a loader has started one.
const SERVER: &str = r#"#!/bin/sh
if [ "$1" = -k ] && [ -e "$WINEPREFIX/server-up" ]; then
  echo 99 > "$WINEPREFIX/quit"
  : > "$WINEPREFIX/client-quit"
  rm -f "$WINEPREFIX/client-up"
  : > "$WINEPREFIX/server-quit"
fi
exit 0
"#;

/// Stands in for the payload's Authenticode tool: the file's text says what
/// the real tool would find. The output is the real tool's, cut down.
const OSSLSIGNCODE: &str = r#"#!/bin/sh
file=
while [ $# -gt 0 ]; do
  if [ "$1" = -in ]; then file=$2; shift; fi
  shift
done
if grep -q VALVE-SIGNED "$file" 2>/dev/null; then
  printf "Signer's certificate:\n\tSigner #0:\n\t\tSubject: CN=Valve Corp.,O=Valve Corp.,L=Bellevue,ST=Washington,C=US\n\nSignature verification: ok\n\nNumber of verified signatures: 1\nSucceeded\n"
  exit 0
fi
if grep -q OTHER-SIGNED "$file" 2>/dev/null; then
  printf "Signer's certificate:\n\tSigner #0:\n\t\tSubject: CN=Fixture Signer,O=Not Valve\n\nSignature verification: ok\n\nNumber of verified signatures: 1\nSucceeded\n"
  exit 0
fi
printf "No signature found\nUnable to extract existing signature\nFailed\n"
exit 1
"#;

/// Prints the JSON line `cpuid` prints; `ROSETTA_ADVERTISE_AVX` changes it.
const CPUID: &str = r#"#!/bin/sh
avx=false
[ "$ROSETTA_ADVERTISE_AVX" = 1 ] && avx=true
echo "{\"avx\":$avx,\"avx2\":false,\"osxsave\":true,\"xcr0_ymm\":$avx}"
"#;

/// What a stub title runs: stays until `$WINEPREFIX/quit` holds an exit code.
/// `helper.exe` ignores that; the client's processes (`steam.exe`,
/// `steamwebhelper.exe`) ignore it too and leave on `$WINEPREFIX/client-quit`.
/// Each leaves with its prefix, so a failed test leaves nothing running.
/// Without a prefix there is nothing to wait for, for `cargo test -- --ignored`.
#[test]
#[ignore = "the stub title, started by the stub loader"]
fn stub_title() {
    let Some(prefix) = std::env::var_os("WINEPREFIX") else {
        return;
    };
    let prefix = Path::new(&prefix);
    let quit = prefix.join("quit");
    let name = std::env::args().next_back().unwrap_or_default();
    let client = name == "steam.exe" || name == "steamwebhelper.exe";
    let obeys = name != "helper.exe" && !client;
    while prefix.is_dir() {
        if client && prefix.join("client-quit").exists() {
            std::process::exit(0);
        }
        if let Some(code) = fs::read_to_string(&quit)
            .ok()
            .filter(|_| obeys)
            .and_then(|text| text.trim().parse().ok())
        {
            std::process::exit(code);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// What a stub server runs: holds the `lock` file a real `wineserver` holds,
/// so `wineserver_running` reads true, until `$WINEPREFIX/server-quit` exists.
#[test]
#[ignore = "the stub server, started by spawn_stub_server"]
fn stub_server() {
    let Some(prefix) = std::env::var_os("WINEPREFIX") else {
        return;
    };
    let prefix = Path::new(&prefix);
    let socket = server_socket(prefix).unwrap();
    let dir = socket.parent().unwrap();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .unwrap();
    let lock = fs::File::create(dir.join("lock")).unwrap();
    lock.lock().unwrap();
    fs::write(prefix.join("server-up"), "").unwrap();
    while prefix.is_dir() && !prefix.join("server-quit").exists() {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let _ = fs::remove_file(prefix.join("server-up"));
    let _ = fs::remove_dir_all(dir);
}

/// Starts a [`stub_server`] and returns once it holds its lock; `key` is the
/// session it counts as part of, as a real server's inherited variable does.
pub fn spawn_stub_server(key: &str, prefix: &Path) -> std::process::Child {
    let _ = fs::remove_file(prefix.join("server-up"));
    let _ = fs::remove_file(prefix.join("server-quit"));
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["common::stub_server", "--exact", "--ignored", "--nocapture"])
        .env("APPLEJACK_SESSION", key)
        .env("WINEPREFIX", prefix)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !prefix.join("server-up").exists() {
        assert!(std::time::Instant::now() < deadline, "no stub server");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    child
}

/// Starts the stub title directly, with no loader: a title nobody spawned.
pub fn spawn_stub_title(key: &str, prefix: &Path, exe: &str) -> std::process::Child {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["common::stub_title", "--exact", "--ignored", "--nocapture"])
        .arg(exe)
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
    // The payload's shim carries its marker, as every real build does.
    fs::write(
        root.join("probes/steamwebhelper-shim.exe"),
        b"STUB applejack steamwebhelper shim STUB",
    )
    .unwrap();
    let helper = std::env::current_exe().unwrap();
    let wine = WINE.replace("@HELPER@", helper.to_str().unwrap());
    script(&root.join("wine/bin/wine64"), &wine);
    script(&root.join("wine/bin/wineserver"), SERVER);
    script(&root.join("probes/cpuid"), CPUID);
    script(&root.join("probes/osslsigncode"), OSSLSIGNCODE);
    let toml = BUNDLE_TOML.replace("version = \"10.0.0\"", &format!("version = \"{version}\""));
    fs::write(root.join("BUNDLE.toml"), toml).unwrap();
}

/// The least the launch path reads as a Windows executable.
pub fn pe_image() -> Vec<u8> {
    let mut image = vec![0; 64];
    image[..2].copy_from_slice(b"MZ");
    image[60] = 64;
    image.extend_from_slice(b"PE\0\0\x64\x86");
    image
}

/// `<library>/steamapps` with a manifest and `<library>/steamapps/common/Title`
/// holding `exes`, as a finished install leaves them.
pub fn write_title(library: &Path, appid: u32, exes: &[&str]) {
    let common = library.join("steamapps/common/Title");
    fs::create_dir_all(&common).unwrap();
    for exe in exes {
        fs::write(common.join(exe), pe_image()).unwrap();
    }
    fs::write(
        library.join(format!("steamapps/appmanifest_{appid}.acf")),
        format!(
            "\"AppState\"\n{{\n\t\"appid\"\t\t\"{appid}\"\n\t\"Universe\"\t\t\"1\"\n\t\"name\"\t\t\"Title\"\n\t\"StateFlags\"\t\t\"4\"\n\t\"installdir\"\t\t\"Title\"\n\t\"buildid\"\t\t\"100\"\n\t\"LastUpdated\"\t\t\"0\"\n\t\"SizeOnDisk\"\t\t\"8\"\n}}\n"
        ),
    )
    .unwrap();
}
