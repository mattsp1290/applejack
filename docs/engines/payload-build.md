# Rosetta engine payload build

How the x86-64 Wine payload for the `rosetta-wine` engine is built, checked,
and gated. Data lives in `build/pins.toml` (every input, pinned by HTTPS URL
and SHA-256) and `build/denylist.toml` (paths that may never ship). ADR-0010
records the Wine source choice.

Gate status (2026-10-28): the tooling exists and is tested; the Wine build has
not run. Every gate below is **not yet run**.

## Prerequisites

Run `sh build/bootstrap.sh --check` from the repository root. It lists each
missing item with the command that installs it and exits non-zero while any
is missing. It installs nothing.

| Prerequisite | Why | Status on the planning host (2026-10-28) |
| --- | --- | --- |
| Apple Silicon host, Rosetta 2, Xcode command line tools | the build runs under `arch -x86_64` | present |
| `rustup target add x86_64-apple-darwin` | the `cpuid` probe is an x86-64 Mach-O | **missing** |
| llvm-mingw (pin `llvm-mingw`) extracted under `build/work/toolchain/` | PE side of Wine (`--with-mingw=llvm-mingw`), `dx11-sample`, `xinput-check` | **missing** (`sh build/bootstrap.sh` extracts it) |
| x86-64 Homebrew at `/usr/local` with `freetype gnutls sdl2 molten-vk bison flex pkgconf` | Wine's Unix-side dependencies; build inputs only | present; `molten-vk` and `pkgconf` **missing** |
| Homebrew mingw-w64 (`i686-` and `x86_64-w64-mingw32-gcc`) | fallback compiler for the probes only | present (14.0.0, GCC 15.2) |
| `zstd` | the vkd3d-proton release is `.tar.zst` | present |
| `cmake`, `openssl@3` | `osslsigncode` host tool | not checked by `--check`; `build/osslsigncode/build.sh` reports them |

`sh build/bootstrap.sh` (no flag) adds the Rust target and extracts llvm-mingw.
`sh build/bootstrap.sh --install-homebrew` additionally installs x86-64 Homebrew
and the dependency list. Neither mode has been run yet.

## Recipe

All commands run from the repository root. Output goes under `build/work/`
(git-ignored); the payload tree is assembled under
`fixtures/payloads/rosetta-wine-<wine version>/` (git-ignored) or
`$APPLEJACK_PAYLOAD_DIR`.

1. `cargo xtask pins-verify` — parses both data files offline (also part of
   `cargo xtask check`).
2. `cargo xtask engine-payload --fetch` — downloads every pin with `curl`,
   verifies SHA-256, skips files already present with a matching hash.
   `--fetch <id>` fetches one pin. Verified on the planning host for all eight
   pins.
3. `sh build/wine/fetch.sh` — extracts Wine and the msync patch set, scans the
   archive listing against the denylist, and reports whether `configure.ac`
   has `--with-metal`. At wine-11.0 it does not: Metal support is detected
   automatically (`configure.ac` checks `MTLDevice.registryID` and defines
   `HAVE_MTLDEVICE_REGISTRYID`); there is no option to pass.
4. `sh build/wine/build.sh` — gate G4b patch attempt, then
   `arch -x86_64 configure --enable-archs=i386,x86_64 --with-mingw=llvm-mingw --without-x --enable-winemac-drv --disable-tests`
   with `CC=clang`, `make`, `make install-lib` into `build/work/wine/stage/`,
   copy into `<payload>/wine/`, strip. Hours. Writes `build/work/wine/build.log`
   and `build/work/wine/g4b.txt`.
5. `sh build/dxmt/fetch.sh`, `sh build/vkd3d/fetch.sh`, `sh build/vulkan/fetch.sh`,
   `sh build/gstreamer/fetch.sh` — extract each component under `build/work/`
   after a denylist scan of its listing. `vulkan/fetch.sh` thins the universal
   `libMoltenVK.dylib` to x86-64; `gstreamer/fetch.sh` expands the upstream
   `.pkg` into a `GStreamer.framework` tree without installing it.
6. `sh crates/applejack-engine/probes/<probe>/build.sh` for `hello32`,
   `dx11-sample`, `xinput-check`, `cpuid` — outputs under `build/work/probes/`.
7. Assembly (not yet scripted; `cargo xtask engine-payload --build` runs steps
   3 through 6 in order and stops here): copy the component files into
   `dxmt/`, `vkd3d/`, `vulkan/`, `wine/lib/GStreamer.framework`, `probes/`;
   walk every Mach-O with `otool -L`, copy non-system dependencies into
   `wine/lib/`, rewrite install names to `@rpath` with
   `install_name_tool`, repeat until stable.
8. `cargo xtask engine-payload --write-bundle-toml <spec.toml> <payload>/BUNDLE.toml`
   — renders `BUNDLE.toml` from a typed spec. The override DLL lists come from
   the archive listings (DXMT 0.80: `d3d11`, `dxgi`, `d3d10core`, plus
   `winemetal` as its native component and `nvapi64`/`nvngx` optional;
   vkd3d-proton 3.1: `d3d12`, `d3d12core`). `registry.hidraw_key` at
   wine-11.0 is the `DisableHidraw` DWORD under
   `HKLM\System\CurrentControlSet\Services\WineBus` (`dlls/winebus.sys/main.c`,
   `check_bus_option`).
9. `cargo xtask engine-payload --check <payload>` — denylist scan of every
   path plus the self-containment check of every Mach-O (by magic number, then
   `otool -L`). Fails on any reference into `/usr/local`, `/opt/homebrew`,
   `build/work/`, the payload's own absolute path, or any absolute path outside
   `/System` and `/usr/lib`. `@rpath`, `@loader_path`, and `@executable_path`
   references pass.

## Gates

| Gate | How to run | Result |
| --- | --- | --- |
| G4a source choice | ADR-0010 | decided: Wine 11.0 stable (Option A) |
| G4b msync | `build/wine/build.sh` tries `msync-devel.patch` then `msync-staging.patch` from the pin; `WINEMSYNC=1 wine wineboot --init` in a temp prefix must pass | not yet run; the pin's newest patches target Wine 9.15, so expect "deferred" and `sync_modes = ["esync", "none"]` |
| G4c DXMT ABI (A6) | copy DXMT 0.80 into a prefix with `WINEDLLOVERRIDES="d3d11,dxgi,d3d10core=n,b"` and run `probes/dx11-sample.exe --frames 60`; it prints the adapter and exits 0 | not yet run |
| G4d D3DMetal (A5) | host only, never CI: import the overlay with `applejack engine d3dmetal import`, run `dx11-sample` with the D3DMetal backend | not yet run |
| G4e 32-bit and Steam client | `wine probes/hello32.exe` prints `hello32 ok`; `SteamSetup.exe` (Authenticode-verified per work package 5) installs and `steam.exe` reaches its login window | not yet run |
| G4 overall | payload from `engine-payload.yml` or from the host boots `wineboot` and renders `dx11-sample` with `/usr/local` and `/opt/homebrew` hidden; `--check` passes | not yet run |

## Reproducibility check

`cargo xtask engine-payload --verify-reproducible` is not implemented yet.
Until it is: build twice into two payload directories, then compare
`BUNDLE.toml` component versions and `find <payload> -type f | sort` listings;
they must be identical. Byte-identical binaries are not required.

## Fallback

ADR-0010 names the fallback (Option B, `apple/homebrew-apple` at the commit the
Gcenx GPTK 3.0-x builds reference) and the trigger that selects it. Switching
is a new `wine` pin, an ADR revision, and a rerun of every gate above.

## CI

`.github/workflows/engine-payload.yml` runs on manual dispatch and on
`engine-*` tags only. It runs `bootstrap.sh --check`, `--fetch`, `--build`,
and `--check`. It fails at `bootstrap.sh --check` on a stock runner until the
installing mode and the Wine build are exercised.
