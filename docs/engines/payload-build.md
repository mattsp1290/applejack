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
`cargo xtask engine-payload --build` runs can start. The Rust loader scans
every archive once at `--fetch`; the scripts extract without a second scan.

| Prerequisite | Why | Status on the planning host (2026-10-29) |
| --- | --- | --- |
| Apple Silicon host, Rosetta 2, Xcode command line tools (`clang`, `otool`, `install_name_tool`, `codesign`, `lipo`, `strip`, `file`, `flex`, `git`, `xar`, `lsbom`, `pkgutil`) | the build runs under `arch -x86_64`; `xar` and `lsbom` list `.pkg` archives before extraction | present |
| `cargo`, `rustup`, `rustup target add x86_64-apple-darwin` | every script reads pins through `cargo xtask`; the `cpuid` probe is an x86-64 Mach-O | `cargo`/`rustup` present; target **missing** |
| llvm-mingw (pin `llvm-mingw`) extracted under `build/work/toolchain/` | PE side of Wine (`--with-mingw=llvm-mingw`), `dx11-sample`, `xinput-check`; only the pinned version satisfies the check | **missing** (`sh build/bootstrap.sh` extracts it) |
| x86-64 Homebrew at `/usr/local` with `freetype gnutls sdl2 molten-vk bison flex pkgconf` | Wine's Unix-side dependencies; build inputs only. Install Homebrew per <https://brew.sh> under `arch -x86_64`; `bootstrap.sh` never runs that installer (it is an unpinned script). `pkgconf` also runs the pkg-config report in `build/wine/build.sh` | present; `molten-vk` and `pkgconf` **missing** |
| GStreamer devel package (pin `gstreamer-devel`) expanded under `build/work/gstreamer/devel/` | headers and `.pc` files Wine's `configure` resolves GStreamer through; a build input (`kind = "toolchain"`), never staged | expanded by `sh build/gstreamer/fetch.sh` |
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
`fixtures/payloads/rosetta-wine-<wine version>/` (git-ignored; the Wine
version by design, one bundle per Wine version) or `$APPLEJACK_PAYLOAD_DIR`.

Environment knobs:

| Knob | Read by | Effect |
| --- | --- | --- |
| `APPLEJACK_PAYLOAD_DIR` | `build/lib.sh` (every script) | assemble into this directory instead of `fixtures/payloads/rosetta-wine-<wine version>/` |
| `ASSEMBLE_PARTIAL=1` | `build/assemble.sh` | tolerate unbuilt probes and an unbuilt `osslsigncode`; the tree is announced as partial and is not installable |
| `WINE_JOBS=<n>` | `build/wine/build.sh` | `make` parallelism (default `hw.ncpu`) |
| `WINE_SKIP_MSYNC=1` | `build/wine/build.sh` | skip the msync patch attempt; gate G4b records "skipped" |
| `PROBE_ARCH=i686` | `probes/dx11-sample/build.sh`, `probes/xinput-check/build.sh` | 32-bit build of the probe (default `x86_64`) |
| `OPENSSL_ROOT_DIR` | `build/osslsigncode/build.sh` | OpenSSL prefix (default `brew --prefix openssl@3`) |

1. `cargo xtask pins-verify` — parses both data files offline (also part of
   `cargo xtask check`).
2. `cargo xtask engine-payload --fetch` — downloads every pin with `curl`
   (HTTPS only, redirects included), verifies SHA-256, then lists the archive
   (`tar -tf`, `unzip -Z1`, or the component `Bom`s of a `.pkg` through
   `xar` and `lsbom`) and rejects it if any path matches an archive-scoped
   denylist rule. A present archive is re-verified and re-scanned on every
   run. `--fetch <id>` fetches one pin. Verified on the planning host for all
   nine pins.
3. `sh build/wine/fetch.sh` — extracts Wine and the msync patch set (the
   loader in step 2 scanned both archives; the scripts do not scan again)
   and removes the patch stamps. `build.sh` adds `--with-metal` to
   `configure` when `configure.ac` offers it; at wine-11.0 it does not:
   Metal support is detected automatically (`configure.ac` checks
   `MTLDevice.registryID` and defines `HAVE_MTLDEVICE_REGISTRYID`), so there
   is no option to pass.
4. `sh build/wine/build.sh` (after step 5's `sh build/gstreamer/fetch.sh`,
   which it needs) — tool checks, the bison conftest, the pkg-config report,
   gate G4b (below), project patches from `build/wine/patches/`, then
   `arch -x86_64 configure --enable-archs=i386,x86_64 --with-mingw=llvm-mingw --without-x --enable-winemac-drv --with-gstreamer --disable-tests`
   with `CC=clang`, `BISON=`, `FLEX=`, `GSTREAMER_LIBS=`, `make`,
   `make install-lib` into `build/work/wine/stage/`. Hours. GStreamer
   reaches `configure` through `PKG_CONFIG_PATH`, which ends with the devel
   package's `lib/pkgconfig` (its `.pc` files are relocatable, so
   `GSTREAMER_CFLAGS` comes from them; it goes last because it also carries
   `freetype2.pc`, `fontconfig.pc`, and the rest of GStreamer's base system,
   which must not shadow the Homebrew packages Wine links), while
   `GSTREAMER_LIBS` points at the
   staged, thinned `lib/` so the link references name what ships and the
   closure in step 7 rewrites them; `--with-gstreamer` makes a miss a
   configure error instead of the silent "GStreamer won't be supported"
   notice. The pkg-config report runs `arch -x86_64 pkg-config --exists` for
   `gstreamer-1.0 gstreamer-video-1.0 gstreamer-audio-1.0 gstreamer-tag-1.0 freetype2 gnutls sdl2`
   and prints each version and `.pc` location; a miss is a gap, since
   `configure` would otherwise disable that feature without failing. Vulkan
   is not a pkg-config package on macOS (`configure` links `-lMoltenVK`),
   so the report looks for `libMoltenVK.dylib` under the x86-64 prefix
   instead. Writes `build/work/wine/build.log` and
   `build/work/wine/g4b.txt`. The project patches are applied in file-name
   order to a clone of the tree first (`cp -c`, an APFS clonefile) and to the
   real tree only when every patch applied there, so the tree is never half
   patched and patches may build on earlier ones; a missing msync patch set
   is a missing prerequisite (rerun `fetch.sh`), not a "deferred" gate
   result. The stamps `build/work/wine/.msync-applied` and
   `.patches-applied` make a rerun after a failed `make` skip the patch
   steps, and the stage is replaced only after `make` succeeded.
   `sh build/wine/build.sh --preflight` runs everything up to `configure`
   without changing the tree: it reports the msync dry runs, the project-patch
   trial (on the tree without msync applied, since preflight applies nothing),
   the bison outcome, and the pkg-config report. It exits 0 when the build
   can start, 3 when the only gaps are ones `sh build/bootstrap.sh` installs
   (llvm-mingw, the Homebrew bison, flex, pkgconf, molten-vk, and the
   packages the report misses), and 1 for anything else (Rosetta, the x86-64
   Homebrew, the msync patch set, the GStreamer stage or devel tree, a
   project patch that does not apply), so CI can run it before bootstrap and
   still catch a recipe defect.
5. `sh build/dxmt/fetch.sh`, `sh build/vkd3d/fetch.sh`, `sh build/vulkan/fetch.sh`,
   `sh build/gstreamer/fetch.sh` — extract each component under `build/work/`
   after a denylist scan of its listing. `vulkan/fetch.sh` thins the universal
   `libMoltenVK.dylib` to x86-64. `gstreamer/fetch.sh` expands the upstream
   `.pkg` without installing it (and the devel `.pkg` of the same release
   into `build/work/gstreamer/devel/`, a build input for step 4 that is
   never staged; the devel counterparts of the same allowlist), stages only
   the allowlisted sub-packages
   (`base-system`, `base-crypto`, `osx-framework`, `core`, `system`,
   `playback`, `codecs`, `effects`, `net`, `encoding`; the script comments
   justify each), drops the GPL and patent-restricted ones (`codecs-gpl`,
   `codecs-gpl-restricted`, `codecs-restricted`, `net-restricted`,
   `dvd-gpl`, `libav`) and the tooling ones, removes the umbrella
   `GStreamer` binary and its links (it re-exports libraries of the dropped
   sub-packages, `libgstvalidate` among them, so it could never load;
   winegstreamer links `libgstreamer-1.0` and friends, never the umbrella),
   thins every universal Mach-O to x86-64 with `lipo` (a failure names the
   file), prunes the symlinks that point into unstaged sub-packages
   (`Headers`, into the devel package) and fails if any link still dangles,
   and scans the staged listing against the denylist, whose
   `scope = "payload"` rules name the excluded plugin dylibs. The
   `LGPL-2.1-or-later` recorded for the component describes that allowlist,
   not the whole upstream package. Once `configure` has found GStreamer
   (step 4 asserts it), winegstreamer decodes
   H.264/HEVC and AAC through VideoToolbox/AudioToolbox, VP8/VP9/AV1, Theora,
   Vorbis, Opus, FLAC, MP3, and PCM in MP4, Matroska, WebM, Ogg, AVI, and
   WAV. It does not decode WMV/VC-1, WMA, MPEG-1/2 video, MPEG-PS/TS, or
   MPEG-4 Part 2 (DivX/Xvid/MS-MPEG4), nor Cinepak, Indeo, or ADPCM in AVI:
   `codecs-restricted` holds the ASF, MPEG-PS/TS, and RealMedia demuxers,
   and `libav` is the only decoder in the package for those formats (it has
   no `mpeg2dec` or `faad`). Games with such cutscenes show a black or
   skipped video until a codec decision (an ADR) adds `libav`. Bink and
   Smacker are decoded by the games themselves and are unaffected. The
   staged `codecs` set also carries `libgstopenh264`, built from source
   (BSD-2-Clause); Cisco's H.264 patent coverage applies to Cisco's own
   binaries only, so that plugin carries H.264 patent exposure of its own,
   while VideoToolbox in `system` already decodes H.264.
6. `sh build/osslsigncode/build.sh` — the host-arch Authenticode tool for
   `probes/`, built with `-DOPENSSL_USE_STATIC_LIBS=ON` so Homebrew's
   `libcrypto`/`libssl` are linked in rather than referenced from
   `/opt/homebrew` (the script asserts with `otool -L` that nothing outside
   `/usr/lib` and `/System` remains, the rule step 9 enforces); then
   `sh crates/applejack-engine/probes/<probe>/build.sh` for `hello32`,
   `dx11-sample`, `xinput-check`, `cpuid` — outputs under `build/work/probes/`.
7. `sh build/assemble.sh` — builds the plan 02 layout under
   `<payload>.tmp` and moves it into place, so nothing from an earlier run
   survives and a failed run leaves the previous payload untouched: `wine/`
   from `build/work/wine/stage` (stripped, a `strip` failure is fatal;
   skipped with a message when no built Wine exists),
   `wine/lib/wine/x86_64-unix/winemetal.so`, `wine/lib/GStreamer.framework`,
   `dxmt/{x86_64-windows,i386-windows}`, `vkd3d/{x64,x86}`, `vulkan/`,
   `probes/` (the three PE probes, `cpuid`, and `osslsigncode` as a host-arch
   tool; GPL-3.0, listed in the notices), then `BUNDLE.toml` and
   `THIRD_PARTY_NOTICES.md` rendered from `build/bundle-spec.toml` (step 8).
   `ASSEMBLE_PARTIAL=1` tolerates unbuilt probes and an unbuilt
   `osslsigncode`. `cargo xtask engine-payload --build` runs steps 3 through 7
   in order and stops here. What remains unscripted is the dylib closure: walk
   every Mach-O with `otool -L`, copy non-system dependencies (the x86-64
   Homebrew dylibs recorded in `brew-versions.txt`) into `wine/lib/`, rewrite
   install names and `LC_RPATH` entries to `@rpath`/`@loader_path` with
   `install_name_tool`, repeat until stable. A reassembly discards that work.
8. `cargo xtask engine-payload --write-bundle-toml build/bundle-spec.toml <payload>/BUNDLE.toml`
   — renders `BUNDLE.toml` and, beside it, `THIRD_PARTY_NOTICES.md` (one row
   per `[components]` entry plus Wine, with version, SPDX license, and
   location) from the committed spec, which has the same shape as the output
   and carries the real pin values; `assemble.sh` runs this, so the step is
   only rerun after editing the spec. The engine crate's reader parses the
   rendered file, and `xtask/tests/fixtures/BUNDLE.toml` is the committed
   rendering of the spec, which a test keeps in sync. The shape, abridged:

   ```toml
   id = "rosetta-wine"          # the reader's engine set; arch must match it
   version = "11.0-1"           # lowercase [0-9a-z._+-], leading digit, no layout suffix
   arch = "x86_64"
   min_macos = "14.0"
   sync_modes = ["esync", "none"]   # ordered, first is the default, no duplicates

   [wine]
   version = "11.0"
   source_url = "https://dl.winehq.org/wine/source/11.0/wine-11.0.tar.xz"
   source_rev = "wine-11.0"
   loader = "wine/bin/wine"
   server = "wine/bin/wineserver"
   wow64 = "new"                # "new" | "32on64" | "none"; G4e confirms it

   [components]                 # versions and licenses from build/pins.toml
   dxmt = { version = "0.80", license = "MIT" }
   osslsigncode = { version = "2.14", license = "GPL-3.0-or-later" }

   [overrides.dxmt]             # bundle-relative .dll paths per architecture;
   x86_64 = ["dxmt/x86_64-windows/d3d11.dll", ...]   # the stem is the override name
   i386 = ["dxmt/i386-windows/d3d11.dll", ...]       # empty when wow64 = "none"

   # [d3dmetal] overlay_dir = "wine/lib/external"   # present only once G4d passes

   [probes]
   cpuid = "probes/cpuid"
   dx11_sample = "probes/dx11-sample.exe"
   xinput_check = "probes/xinput-check.exe"

   [registry]
   hidraw_key = 'HKLM\System\CurrentControlSet\Services\WineBus\DisableHidraw'
   ```

   The DXMT 0.80 override set is `d3d11`, `dxgi`, `d3d10core`, and
   `winemetal` (the bridge to its native half); `nvapi64` and `nvngx` are
   optional and not shipped. vkd3d-proton 3.1 is `d3d12` and `d3d12core`.
   `registry.hidraw_key` at wine-11.0 is the `DisableHidraw` DWORD under
   `HKLM\System\CurrentControlSet\Services\WineBus` (`dlls/winebus.sys/main.c`,
   `check_bus_option`). Every path must be relative without `.` or `..`. The
   writer rejects what the reader rejects: an unknown `id`, an `arch` that
   does not match it, a malformed version, duplicate sync modes, an override
   entry without `.dll` or with a stem already listed for that architecture,
   an `i386` list under `wow64 = "none"`, and a non-empty `overrides.d3dmetal`
   without the `[d3dmetal]` table.
9. `cargo xtask engine-payload --check <payload>` — denylist scan of every
   path and every symlink target (case-insensitive, as APFS resolves them),
   plus the self-containment check of every Mach-O (by magic number, fat and
   fat64 included, then `otool -L` and the load commands from `otool -l`; an
   `otool` error on stderr or an empty load-command list fails the run).
   Fails on any reference or rpath into `/usr/local`, `/opt/homebrew`,
   `build/work/`, the payload's own absolute path, any absolute path outside
   `/System` and `/usr/lib`, any `..` component, or `/System/Volumes/`
   (a firmlink back into the data volume); on any symlink that is absolute,
   dangling, whose lexical walk from the link's directory climbs above the
   payload root at any point (even one that comes back in through the root's
   own name, which breaks once the installer renames the root), or whose real
   resolution leaves the payload; and on any `@rpath/`, `@loader_path/`, or
   `@executable_path/` dependency that names no file in the payload. The
   lookup follows dyld. The payload's executables are the Mach-Os with
   `LC_MAIN` (or `LC_UNIXTHREAD`); `@executable_path` relocates against each
   executable's directory, in dependencies and `LC_RPATH` entries alike.
   `@rpath/x` is searched through the binary's own relocated `LC_RPATH`
   entries plus the executables', since dyld consults the rpaths of the image
   and of the images that loaded it up to the main executable and never those
   of unrelated images: a dlopen'd Unix module without an rpath resolves
   through the loader's, and a library reachable only through another dylib's
   rpath is reported. `@loader_path/x` resolves against the binary's own
   directory. When the binary's `LC_RPATH` entries, or an executable's,
   include a system root such as `/usr/lib/swift`, its unresolved `@rpath/`
   references are left to dyld; other absolute `LC_RPATH` entries contribute
   nothing to the lookup.

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
commit SHAs, the cargo registry cached, and the fetched archives cached under
a key on `build/pins.toml` (`--fetch` re-verifies and re-scans a cached
archive). It selects Xcode 26.2 or newer like `check.yml`, then runs
`pins-verify`, `--fetch` (every archive scanned), and `bootstrap.sh --check`.
On a stock runner the job stops there: `--check` requires an x86-64 Homebrew
at `/usr/local`, which the arm64 `macos-latest` image does not ship and no
script installs (its installer is an unpinned script), so the bootstrap
installing mode cannot unblock it. The job stays at that step until a
provisioning step for x86-64 Homebrew is decided (a pinned clone of the
Homebrew tag plus the dependency list, or a self-hosted runner). `--build`
and the payload `--check` run only on a manual dispatch with the `build`
input set.
