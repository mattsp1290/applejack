# Rosetta engine payload build

How the x86-64 Wine payload for the `rosetta-wine` engine is built, checked,
and gated. Data lives in `build/pins.toml` (every input, pinned by HTTPS URL
and SHA-256) and `build/denylist.toml` (paths that may never ship). ADR-0010
records the Wine source choice.

Gate status (2026-10-29): the tooling exists and is tested; the Wine build has
not run. Every gate below is **not yet run**.

## Prerequisites

Run `sh build/bootstrap.sh --check` from the repository root. It lists each
missing item with the command that installs it and exits non-zero while any
is missing. It installs nothing. Exit 0 means every script that
`cargo xtask engine-payload --build` runs can start.

| Prerequisite | Why | Status on the planning host (2026-10-29) |
| --- | --- | --- |
| Apple Silicon host, Rosetta 2, Xcode command line tools (`clang`, `otool`, `install_name_tool`, `codesign`, `lipo`, `strip`, `file`, `flex`, `git`, `xar`, `lsbom`, `pkgutil`) | the build runs under `arch -x86_64`; `xar` and `lsbom` list `.pkg` archives before extraction | present |
| `cargo`, `rustup`, `rustup target add x86_64-apple-darwin` | every script reads pins through `cargo xtask`; the `cpuid` probe is an x86-64 Mach-O | `cargo`/`rustup` present; target **missing** |
| llvm-mingw (pin `llvm-mingw`) extracted under `build/work/toolchain/` | PE side of Wine (`--with-mingw=llvm-mingw`), `dx11-sample`, `xinput-check`; only the pinned version satisfies the check | **missing** (`sh build/bootstrap.sh` extracts it) |
| x86-64 Homebrew at `/usr/local` with `freetype gnutls sdl2 molten-vk bison flex pkgconf` | Wine's Unix-side dependencies; build inputs only. Install Homebrew per <https://brew.sh> under `arch -x86_64`; `bootstrap.sh` never runs that installer (it is an unpinned script) | present; `molten-vk` and `pkgconf` **missing** |
| bison 3.0 or newer (`<x86 prefix>/opt/bison/bin/bison`, keg-only) | wine-11.0's `configure` feeds bison `%define parse.error verbose` and aborts on Apple's bison 2.3; `--check` runs that conftest, and `build/wine/build.sh` puts the keg on `PATH` and passes `BISON=`/`FLEX=` to `configure` | present (3.8.2) |
| Homebrew mingw-w64 (`i686-` and `x86_64-w64-mingw32-gcc`) | fallback compiler for the probes only | present (14.0.0, GCC 15.2) |
| `zstd` | the vkd3d-proton release is `.tar.zst` | present |
| `cmake`, host-Homebrew `openssl@3` | `osslsigncode` host tool | present |

`sh build/bootstrap.sh` (no flag) adds the Rust target and extracts llvm-mingw.
`sh build/bootstrap.sh --install-brew-deps` additionally installs the
dependency list with the x86-64 Homebrew, which must already exist. Every mode
records `brew list --versions` of the dependencies into
`build/work/brew-versions.txt`, because their dylibs become payload inputs in
step 7. Neither installing mode has been run yet.

## Recipe

All commands run from the repository root. Output goes under `build/work/`
(git-ignored); the payload tree is assembled under
`fixtures/payloads/rosetta-wine-<wine version>/` (git-ignored) or
`$APPLEJACK_PAYLOAD_DIR`.

1. `cargo xtask pins-verify` — parses both data files offline (also part of
   `cargo xtask check`).
2. `cargo xtask engine-payload --fetch` — downloads every pin with `curl`
   (HTTPS only, redirects included), verifies SHA-256, then lists the archive
   (`tar -tf`, `unzip -Z1`, or the component `Bom`s of a `.pkg` through
   `xar` and `lsbom`) and rejects it if any path matches an archive-scoped
   denylist rule. A present archive is re-verified and re-scanned on every
   run. `--fetch <id>` fetches one pin. Verified on the planning host for all
   eight pins.
3. `sh build/wine/fetch.sh` — extracts Wine and the msync patch set, scans
   each listing against the denylist again, removes the patch stamps, and
   reports whether `configure.ac` has `--with-metal`. At wine-11.0 it does
   not: Metal support is detected automatically (`configure.ac` checks
   `MTLDevice.registryID` and defines `HAVE_MTLDEVICE_REGISTRYID`); there is
   no option to pass.
4. `sh build/wine/build.sh` — tool checks and the bison conftest, gate G4b
   (below), project patches from `build/wine/patches/`, then
   `arch -x86_64 configure --enable-archs=i386,x86_64 --with-mingw=llvm-mingw --without-x --enable-winemac-drv --disable-tests`
   with `CC=clang`, `BISON=`, `FLEX=`, `make`, `make install-lib` into
   `build/work/wine/stage/`. Hours. Writes `build/work/wine/build.log` and
   `build/work/wine/g4b.txt`. Patches are dry-run as a set before any is
   applied, so the tree is never half patched; the stamps
   `build/work/wine/.msync-applied` and `.patches-applied` make a rerun after
   a failed `make` skip the patch steps, and the stage is replaced only after
   `make` succeeded. `sh build/wine/build.sh --preflight` runs everything up
   to `configure` without changing the tree (it reports the patch dry runs and
   the bison outcome, and exits non-zero if a prerequisite is missing).
5. `sh build/dxmt/fetch.sh`, `sh build/vkd3d/fetch.sh`, `sh build/vulkan/fetch.sh`,
   `sh build/gstreamer/fetch.sh` — extract each component under `build/work/`
   after a denylist scan of its listing. `vulkan/fetch.sh` thins the universal
   `libMoltenVK.dylib` to x86-64. `gstreamer/fetch.sh` expands the upstream
   `.pkg` without installing it, stages only the allowlisted sub-packages
   (`base-system`, `base-crypto`, `osx-framework`, `core`, `system`,
   `playback`, `codecs`, `effects`, `net`, `encoding`; the script comments
   justify each), drops the GPL and patent-restricted ones (`codecs-gpl`,
   `codecs-gpl-restricted`, `codecs-restricted`, `net-restricted`,
   `dvd-gpl`, `libav`) and the tooling ones, thins every universal Mach-O to
   x86-64 with `lipo`, and scans the staged listing against the denylist,
   whose `scope = "payload"` rules name the excluded plugin dylibs. The
   `LGPL-2.1-or-later` recorded for the component describes that allowlist,
   not the whole upstream package. Games whose cutscenes are WMV or MPEG-TS
   will not decode through winegstreamer with this set.
6. `sh crates/applejack-engine/probes/<probe>/build.sh` for `hello32`,
   `dx11-sample`, `xinput-check`, `cpuid` — outputs under `build/work/probes/`.
7. `sh build/assemble.sh` — copies the staged components into the plan 02
   layout: `wine/` from `build/work/wine/stage` (stripped; skipped with a
   message when no built Wine exists), `wine/lib/wine/x86_64-unix/winemetal.so`,
   `wine/lib/GStreamer.framework`, `dxmt/{x86_64-windows,i386-windows}`,
   `vkd3d/{x64,x86}`, `vulkan/`, `probes/`. `ASSEMBLE_PARTIAL=1` tolerates
   unbuilt probes. `cargo xtask engine-payload --build` runs steps 3 through 7
   in order and stops here. What remains unscripted is the dylib closure: walk
   every Mach-O with `otool -L`, copy non-system dependencies (the x86-64
   Homebrew dylibs recorded in `brew-versions.txt`) into `wine/lib/`, rewrite
   install names and `LC_RPATH` entries to `@rpath`/`@loader_path` with
   `install_name_tool`, repeat until stable.
8. `cargo xtask engine-payload --write-bundle-toml <spec.toml> <payload>/BUNDLE.toml`
   — renders `BUNDLE.toml` from a typed spec. The override DLL lists come from
   the archive listings (DXMT 0.80: `d3d11`, `dxgi`, `d3d10core`, plus
   `winemetal` as its native component and `nvapi64`/`nvngx` optional;
   vkd3d-proton 3.1: `d3d12`, `d3d12core`). `registry.hidraw_key` at
   wine-11.0 is the `DisableHidraw` DWORD under
   `HKLM\System\CurrentControlSet\Services\WineBus` (`dlls/winebus.sys/main.c`,
   `check_bus_option`). Every loader, server, preloader, probe, and
   `overlay_dir` path must be relative without `..`.
9. `cargo xtask engine-payload --check <payload>` — denylist scan of every
   path and every symlink target (case-insensitive, as APFS resolves them),
   plus the self-containment check of every Mach-O (by magic number, fat and
   fat64 included, then `otool -L` and the `LC_RPATH` entries from `otool -l`).
   Fails on any reference or rpath into `/usr/local`, `/opt/homebrew`,
   `build/work/`, the payload's own absolute path, any absolute path outside
   `/System` and `/usr/lib`, any `..` component, or `/System/Volumes/`
   (a firmlink back into the data volume); and on any symlink that is
   absolute, dangling, or resolves outside the payload. `@rpath`,
   `@loader_path`, and `@executable_path` references pass.

## Running the built Wine

The loader is `<payload>/wine/bin/wine` (`wine.loader` in `BUNDLE.toml`) and
the server `<payload>/wine/bin/wineserver`. A throwaway prefix:

```sh
export WINEPREFIX=$(mktemp -d)
<payload>/wine/bin/wine wineboot --init
<payload>/wine/bin/wineserver -w
```

Gate G4c (DXMT) copies the DLLs into that prefix:
`<payload>/dxmt/x86_64-windows/{d3d11,dxgi,d3d10core,winemetal}.dll` into
`$WINEPREFIX/drive_c/windows/system32/` and
`<payload>/dxmt/i386-windows/*.dll` into `$WINEPREFIX/drive_c/windows/syswow64/`;
DXMT's native half is already at `<payload>/wine/lib/wine/x86_64-unix/winemetal.so`
next to Wine's own Unix libraries, where `assemble.sh` put it. Then
`WINEDLLOVERRIDES="d3d11,dxgi,d3d10core=n,b" <payload>/wine/bin/wine <payload>/probes/dx11-sample.exe --frames 60`.

## Gates

| Gate | How to run | Result |
| --- | --- | --- |
| G4a source choice | ADR-0010 | decided: Wine 11.0 stable (Option A) |
| G4b msync | `build/wine/build.sh` dry-runs `msync-devel.patch` then `msync-staging.patch` from the pin and applies the first clean one; `WINEMSYNC=1 wine wineboot --init` in a temp prefix must pass | dry run done on the planning host (`--preflight`): neither patch applies to wine-11.0 (the pin targets Wine 9.15), the script records "deferred" and leaves the tree untouched; expect `sync_modes = ["esync", "none"]` |
| G4c DXMT ABI (A6) | see "Running the built Wine"; `dx11-sample.exe --frames 60` prints the adapter and exits 0 | not yet run |
| G4d D3DMetal (A5) | host only, never CI: import the overlay with `applejack engine d3dmetal import`, run `dx11-sample` with the D3DMetal backend | not yet run |
| G4e 32-bit and Steam client | `wine probes/hello32.exe` prints `hello32 ok`; `SteamSetup.exe` (Authenticode-verified per work package 5) installs and `steam.exe` reaches its login window | not yet run |
| G4 overall | payload from `engine-payload.yml` or from the host boots `wineboot` and renders `dx11-sample` with `/usr/local` and `/opt/homebrew` hidden; `--check` passes | not yet run |

## Reproducibility check

`cargo xtask engine-payload --verify-reproducible` is not implemented yet.
Until it is: build twice into two payload directories, then compare
`BUNDLE.toml` component versions and `find <payload> -type f | sort` listings;
they must be identical. Byte-identical binaries are not required.

## Fallback

ADR-0010 names the fallback (Option B, `apple/homebrew-apple` at commit
`2bc4428`, the revision the Gcenx GPTK 3.0-x builds reference) and the
trigger that selects it. Switching is a new `wine` pin, an ADR revision, and
a rerun of every gate above.

## CI

`.github/workflows/engine-payload.yml` runs on manual dispatch and on
`engine-*` tags only, with `actions/checkout` and `actions/cache` pinned to
commit SHAs and only the cargo registry cached. It runs `pins-verify`,
`--fetch` (every archive scanned), `bootstrap.sh --check`, `--build`, and
`--check`. On a stock runner it fails at `bootstrap.sh --check` until the
installing mode and the Wine build are exercised; the steps before it give
signal today.
