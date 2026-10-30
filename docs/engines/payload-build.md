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
| x86-64 Homebrew at `/usr/local` with `freetype gnutls sdl2 molten-vk bison flex pkgconf` | Wine's Unix-side dependencies; build inputs only. Install Homebrew per <https://brew.sh> under `arch -x86_64`; `bootstrap.sh` never runs that installer (it is an unpinned script). `pkgconf` is what `configure` runs for the Homebrew packages and what the report in `build/wine/build.sh` probes them with | present; `molten-vk` and `pkgconf` **missing** |
| GStreamer devel package (pin `gstreamer-devel`) expanded under `build/work/gstreamer/devel/` | headers and `.pc` files `build/wine/build.sh` computes `GSTREAMER_CFLAGS` and `GSTREAMER_LIBS` from, off `PKG_CONFIG_PATH`; a build input (`kind = "toolchain"`), never staged | expanded by `sh build/gstreamer/fetch.sh` |
| bison 3.0 or newer (`<x86 prefix>/opt/bison/bin/bison`, keg-only) | wine-11.0's `configure` feeds bison `%define parse.error verbose` and aborts on Apple's bison 2.3; `--check` runs that conftest, and `build/wine/build.sh` puts the keg on `PATH` and passes `BISON=`/`FLEX=` to `configure` | present (3.8.2) |
| Homebrew mingw-w64 (`i686-` and `x86_64-w64-mingw32-gcc`), host Homebrew only | fallback compiler for the probes when the llvm-mingw pin is absent; in the x86-64 Homebrew it would shadow llvm-mingw's prefixed tools on the build PATH, so `build/wine/build.sh` refuses to run with it there | present (14.0.0, GCC 15.2), host Homebrew |
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
4. `sh build/dxmt/fetch.sh`, `sh build/vkd3d/fetch.sh`, `sh build/vulkan/fetch.sh`,
   `sh build/gstreamer/fetch.sh` — extract each component under `build/work/`
   after a denylist scan of its listing. `vulkan/fetch.sh` thins the universal
   `libMoltenVK.dylib` to x86-64. `gstreamer/fetch.sh` expands the upstream
   `.pkg` without installing it (and the devel `.pkg` of the same release
   into `build/work/gstreamer/devel/`, a build input for step 5 that is
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
   removes the command-line tools and helpers (`Versions/1.0/bin`,
   `Versions/1.0/libexec`, and the `Commands` links: nothing in
   winegstreamer runs them, `libgstreamer` scans plugins in-process when
   `gst-plugin-scanner` is absent, and an executable the bundle does not
   declare is dead weight step 9 never follows) and fails if any `LC_MAIN`
   executable remains under the framework,
   thins every universal Mach-O to x86-64 with `lipo` (a failure names the
   file), prunes the symlinks that point into unstaged sub-packages
   (`Headers`, into the devel package) and fails if any link still dangles,
   and scans the staged listing against the denylist, whose
   `scope = "payload"` rules name the excluded plugin dylibs. The
   `LGPL-2.1-or-later` recorded for the component describes that allowlist,
   not the whole upstream package. Once `configure` has found GStreamer
   (step 5 asserts it), winegstreamer decodes
   H.264/HEVC and AAC through VideoToolbox/AudioToolbox, VP8/VP9/AV1, Theora,
   Vorbis, Opus, FLAC, MP3, and PCM in MP4, Matroska, WebM, Ogg, AVI, and
   WAV. It does not decode WMV/VC-1, WMA, MPEG-1/2 video, MPEG-PS/TS, or
   MPEG-4 Part 2 (DivX/Xvid/MS-MPEG4), nor Cinepak, Indeo, or ADPCM in AVI:
   `codecs-restricted` holds the ASF, MPEG-PS/TS, and RealMedia demuxers,
   and `libav` is the only decoder in the package for those formats (it has
   no `mpeg2dec` or `faad`). Games with such cutscenes show a black or
   skipped video until a codec decision (an ADR) adds `libav`. For the same
   reason `build/wine/build.sh` configures `--without-ffmpeg`: with the x86-64
   Homebrew ffmpeg present, `winedmo.so` links it by absolute path, and
   `libavcodec`, `libavformat`, and `libavutil` are payload denylist rules
   (`build/denylist.toml`), so step 9 would reject the closure that step 7
   copies in; a pinned ffmpeg in the payload is that ADR's call. Without
   `winedmo`, `mfsrcsnk` falls back to the GStreamer byte-stream handler for
   every container. Bink and
   Smacker are decoded by the games themselves and are unaffected. The
   staged `codecs` set also carries `libgstopenh264`, built from source
   (BSD-2-Clause); Cisco's H.264 patent coverage applies to Cisco's own
   binaries only, so that plugin carries H.264 patent exposure of its own,
   while VideoToolbox in `system` already decodes H.264.
5. `sh build/wine/build.sh` — tool checks (the prefixed llvm-mingw tools must
   be the ones PATH resolves, and `/usr/bin/clang` must link and run an x86-64
   program under Rosetta), the bison conftest, the pkg-config report,
   gate G4b (below), project patches from `build/wine/patches/`, then
   `arch -x86_64 configure --enable-archs=i386,x86_64 --with-mingw=llvm-mingw --without-x --without-ffmpeg --enable-winemac-drv --with-gstreamer --disable-tests`
   with `CC=/usr/bin/clang` (Apple's; llvm-mingw's unprefixed `clang` sits last
   on PATH and cannot link against the macOS SDK), `BISON=`, `FLEX=`, `GSTREAMER_CFLAGS=`,
   `GSTREAMER_LIBS=`, `make`, `make install-lib` into
   `build/work/wine/stage/`. Hours. GStreamer reaches `configure` only
   through those two precious variables, computed from the devel tree alone
   (`PKG_CONFIG_LIBDIR=<devel>/lib/pkgconfig` with `PKG_CONFIG_PATH` cleared,
   so neither a Homebrew `gstreamer` nor Homebrew's `glib` can answer;
   `configure` never asks pkg-config for these packages when the variables
   are set): `GSTREAMER_CFLAGS` is `pkg-config --cflags` of the four packages
   (the `.pc` files are relocatable, so every `-I` points into the devel
   tree), and `GSTREAMER_LIBS` is `-L<stage>/lib` plus
   `pkg-config --libs-only-l`, pointed at the staged, thinned `lib/` so the
   link references name what ships and the closure in step 7 rewrites them;
   the script fails if any `-l` has no dylib in the stage. `--libs-only-l`
   also drops the devel `.pc`'s `-Wl,-rpath,${libdir}` entry on purpose: a
   plain `--libs` would embed an absolute `LC_RPATH` into the devel tree in
   `winegstreamer.so`, which step 9 rejects as a link into the build
   directory. The script also fails when `GSTREAMER_CFLAGS` comes back
   empty, since `configure`'s `WINE_PACKAGE_FLAGS` would then ask
   pkg-config on the real `PKG_CONFIG_PATH`. The devel tree stays off
   `PKG_CONFIG_PATH`: it also carries `freetype2.pc`,
   `fontconfig.pc`, and the rest of GStreamer's base system, which must not
   shadow the Homebrew packages Wine links. `--with-gstreamer` makes a miss
   a configure error instead of the silent "GStreamer won't be supported"
   notice. The pkg-config report resolves
   `gstreamer-1.0 gstreamer-video-1.0 gstreamer-audio-1.0 gstreamer-tag-1.0 glib-2.0 gobject-2.0`
   the same devel-only way, prints each version and `pcfiledir`, and fails
   if any `pcfiledir` is outside the devel tree (a missing package is a
   gap, since `configure` builds no winegstreamer without it); the devel
   queries read `.pc` text only, so the host's pkg-config serves them until
   the x86-64 one is installed. It then runs the x86-64
   `arch -x86_64 pkg-config --exists` for `freetype2 gnutls sdl2` on the
   real `PKG_CONFIG_PATH`, printing version and `pcfiledir` (a miss is a
   bootstrap gap, since `configure` would otherwise disable that feature
   without failing), and lists any GStreamer or glib package that path also
   offers as unused. Vulkan
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
   the host compiler conftest (the failure configure would report as "C
   compiler cannot create executables"), the bison outcome, and the
   pkg-config report. It exits 0 when the build
   can start, 3 when the only gaps are ones `sh build/bootstrap.sh` installs
   (llvm-mingw, the Homebrew bison, flex, pkgconf, molten-vk, and the
   packages the report misses), and 1 for anything else (Rosetta, the x86-64
   Homebrew, the msync patch set, the GStreamer stage or devel tree, a
   project patch that does not apply, a host compiler that cannot link, a
   prefixed mingw tool shadowed on PATH or missing from the extracted
   llvm-mingw), so CI can run it before bootstrap and
   still catch a recipe defect.
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
   `osslsigncode`. The script's last step, inside the `.tmp` tree so a
   failure leaves the previous payload untouched, is the dylib closure,
   `cargo xtask engine-payload --close <payload>` (`xtask/src/closure.rs`),
   which makes the tree self-contained and ends with step 9's check.
   `cargo xtask engine-payload --build` runs steps 3 through 7 in order and
   then checks the payload at its final location.

   The closure is a fixed-point loop: read every Mach-O's load commands
   (`otool -l`), plan one round, apply it, look again, until a round plans
   nothing (a closed payload is left as it is; a tree that has not settled
   after eight rounds fails). A round lands whole or not at all. One round
   does, in this order:

   - **One file per install name.** dyld keeps one image per install name
     per process: once any image with `LC_ID_DYLIB @rpath/libfreetype.6.dylib`
     is loaded, every later `@rpath/libfreetype.6.dylib` reference and every
     bare-leaf `dlopen("libfreetype.6.dylib")` binds to it, whatever run
     paths the client has. Two regular files with one install name would
     therefore let load order pick the library (the staged GStreamer
     framework ships `libfreetype.6.dylib`, `libintl.8.dylib` (proxy-libintl,
     a stub gettext runtime, not gettext's), and a `libMoltenVK.dylib` of
     its own, 1.2.9, for its vulkan and applemedia plugins). The round
     first groups the tree's regular Mach-O files by install name: where
     the `SONAME_POLICY` table names one of a group's files as the
     payload's copy, the others become relative links to it (the
     framework's `libMoltenVK.dylib` becomes a link to the pinned
     `vulkan/libMoltenVK.dylib`, whose exports cover every symbol those
     plugins import); any other group fails the closure with every file
     named. A file replaced this way gets no edit in the same round: an
     edit planned from its own load commands would run through the new
     link onto the canonical file, and `apply` refuses to run
     `install_name_tool` or `codesign` on any path that is a link, naming
     the link and its target. The rules below never create a duplicate: a
     library the payload already ships is linked or reached by a run path,
     not copied again.
   - **SONAMEs.** Wine `dlopen`s some libraries by bare SONAME rather than
     linking them: `config.h` in the build tree defines
     `SONAME_LIBFREETYPE` (`win32u`, `dwrite`), `SONAME_LIBGNUTLS`
     (`bcrypt`, `crypt32`, `secur32`), `SONAME_LIBSDL2` (`winebus.sys`),
     `SONAME_LIBVULKAN`/`SONAME_LIBMOLTENVK` (`win32u`), `SONAME_LIBCUPS`
     (`localspl`, `winspool.drv`), and `SONAME_LIBODBC` (`odbc32`). Wine
     sets no search path for them (`dlls/ntdll/unix/loader.c` touches
     `DYLD_LIBRARY_PATH` only for an in-tree build), and dyld resolves a
     bare leaf name as `@rpath/<leaf>` through the run paths of the
     calling image and of the main executable, then the working directory
     and `/usr/lib`; it never looks in `/usr/local/lib`, so a payload
     without the closure runs with no fonts, no TLS, no game controllers
     through SDL, and no Vulkan even on the build host. Each SONAME must
     therefore be a file under `wine/lib/`: copied from the x86-64
     Homebrew (`/usr/local/lib/<soname>`) by default, or per the
     `SONAME_POLICY` table in `closure.rs`: `libMoltenVK.dylib` is a
     relative link to the pinned `vulkan/libMoltenVK.dylib` (not
     Homebrew's `molten-vk`), `libfreetype.6.dylib` is a relative link to
     the framework's `GStreamer.framework/Versions/1.0/lib/libfreetype.6.dylib`
     (which its fontconfig, harfbuzz, cairo, pango, and libass link, and
     which exports every `FT_*` symbol `win32u` and `dwrite` look up),
     `libcups.2.dylib` is the system's (dyld serves it from the shared
     cache; no file exists under `/usr/lib`), and `libodbc.dylib` is
     optional (copied when the x86-64 Homebrew has unixODBC, skipped with
     a message otherwise). A SONAME that is neither in the table nor in
     the x86-64 Homebrew fails the closure, and so does one the payload
     already ships somewhere without a table row. Every Unix module under
     `wine/lib/wine/` whose bytes contain a supplied SONAME is a `dlopen`
     caller and gets `LC_RPATH @loader_path/../..`, which is `wine/lib/`.
     The run path goes on the callers, not on the executables, because the
     caller's run paths are what dyld consults for a bare-leaf `dlopen`:
     the module that calls `dlopen` is the right owner, and the two main
     executables (`wine/bin/wine` for the first process,
     `wine/lib/wine/x86_64-unix/wine` for every child, in different
     directories) stay untouched. (`install_name_tool -add_rpath` would
     fit a short entry into all three, 72, 72, and 48 bytes of header
     slack; that is not the reason.)
   - **Links.** For every Mach-O: an install name outside `/usr/lib` and
     `/System` becomes `@rpath/<file name>`; an absolute `LC_RPATH` outside
     them is deleted; a dependency on an absolute non-system path (a
     Homebrew `opt/` or `Cellar/` path) is rewritten to `@rpath/<leaf>`
     and bound to `wine/lib/<leaf>` when that exists (the closure's own
     copies and policy links), else to the one file of that name another
     component ships, which takes a `SONAME_POLICY` row naming the file:
     the framework's proxy-libintl `libintl.8.dylib` for gnutls and
     libidn2. Either way the planner checks the dependent's required
     compatibility version against the file's `LC_ID_DYLIB` current
     version and fails with both numbers when the requirement is higher,
     unless the row carries `allow_older_compat` with a reason, which
     every round that relies on it prints. The libintl row does: gnutls
     and libidn2 record 13.0.0 against gettext 0.26, proxy-libintl reports
     10.5.0, the two symbols they import are exported, and dyld does not
     enforce the compatibility version (observed on macOS 26; unverified
     on the macOS 14 to 25 range the plan supports on a best-effort basis).
     A leaf the payload holds nowhere is copied into `wine/lib/` (thinned
     to x86-64 with `lipo` if universal; the copy must be an x86-64
     Mach-O); and every `@rpath/<leaf>` dependency must then resolve
     through the binary's own `@loader_path` run paths, else the closure
     adds the run path to the one payload directory holding that leaf
     (the binary's own directory wins a tie; a leaf nowhere in the payload
     is copied from Homebrew). A link and the file it resolves to are one
     image, so a leaf in two directories that way is no tie; a leaf in two
     directories as two files is judged after the binary's unambiguous
     references have settled its run paths, so load-command order does
     not decide, and a reference still ambiguous then is an error naming
     both directories. This is what gives `winegstreamer.so`
     `@loader_path/../../GStreamer.framework/Versions/1.0/lib` for its
     `libgst*` references and gives each copied library `@loader_path`
     for its siblings. `@loader_path/` and `@executable_path/` references
     are left to step 9's chain rules. Each rewritten binary is re-signed
     ad hoc (`codesign --force --sign -
     --preserve-metadata=entitlements,flags,runtime`, so an edit of a
     signed binary keeps what it had rather than dropping it silently).
   - **Links the tree carries.** Every path a round writes resolves inside
     the payload. A destination at or below a symlink that is dangling,
     leaves the payload, or names a directory is an occupied slot, not an
     empty one: the planner fails naming the link, its target, and the
     component, and `apply` walks each path component by component again
     before writing, as the engine's install check does, and refuses to
     overwrite anything the plan expected absent. What a round rewrites or
     replaces is journaled first (a pre-existing link as its target, so it
     comes back as a link), and the rollback message counts created paths
     removed and pre-existing files restored separately.
   - **Gates.** Before a round lands, the destinations are scanned against
     `build/denylist.toml`, and every copy's Homebrew provenance
     (`/usr/local/Cellar/<formula>/<version>/`) must match a
     `[components.<formula>]` row of `BUNDLE.toml` with that version, so
     the notices list every copied library family or the build fails;
     `build/bundle-spec.toml` carries those rows (gnutls, p11-kit,
     libidn2, libunistring, libtasn1, nettle, gmp, sdl2; the Cellar path
     of each copy is the source of the versions, and a versioned formula
     such as `openssl@3` is a quoted key). The two libraries Wine binds
     to inside the framework have rows too, rendered at their real
     location rather than as copies: `freetype` (FreeType 2.13.3 under
     the `FTL`, whose binary redistribution requires the FreeType credit
     the notices carry) and `proxy-libintl` (LGPL-2.0-or-later); the
     GStreamer row's LGPL-2.1-or-later is GStreamer's own and covers
     neither. A run that copied anything reports a library row no copy
     came from, so a stale row cannot sit in the notices unnoticed.
     Anything the closure cannot resolve fails the run with the binary
     and the reference named, and nothing of that round is applied. A
     tool failure part-way through a round (`install_name_tool`,
     `codesign`, `lipo`) removes what the round created and puts every
     file it replaced or rewrote back from its pre-image, then fails
     naming the file and the tool's stderr.

   On the planning host (2026-10-30) the closure applies three rounds and
   the fourth plans nothing: it replaces the framework's `libMoltenVK.dylib`
   with a link to the pin, links `wine/lib/libfreetype.6.dylib` and
   `wine/lib/libMoltenVK.dylib`, copies nine dylibs (`libgnutls.30`,
   `libp11-kit.0`, `libidn2.0`, `libunistring.5`, `libtasn1.6`,
   `libhogweed.6`, `libnettle.8`, `libgmp.10`, `libSDL2-2.0.0`), and
   rewrites sixteen binaries: the nine copies, `winegstreamer.so`, and the
   six `dlopen` callers of a supplied SONAME (`win32u`, `dwrite`, `bcrypt`,
   `crypt32`, `secur32`, `winebus`; `localspl` and `winspool` name only the
   system's cups, `odbc32` only the absent odbc). About a minute, most of
   it `otool`. A rerun on the closed tree changes nothing. The copies keep
   compile-time paths the load commands do not show: `libgnutls.30.dylib`
   names `/usr/local/etc/gnutls/cert.pem` (its system trust file) and
   `libp11-kit.0.dylib` names `/usr/local/etc/pkcs11/modules`,
   `/usr/local/etc/pkcs11.conf`, and its `Cellar/p11-kit/.../lib/pkcs11`
   module directory, which is a code-loading path. None of them exists on
   a user's machine, so gnutls's system trust is empty and p11-kit finds no
   PKCS#11 modules there; Wine 11.0 reaches neither
   (`gnutls_certificate_set_x509_system_trust` and `gnutls_pkcs11_init`
   have no caller under `dlls/bcrypt`, `dlls/secur32`, `dlls/crypt32`;
   crypt32 validates chains itself). A `P11_KIT_NO_USER_CONFIG`-style
   mitigation in the launch environment is for the day a Wine release calls
   them.
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
   lookup follows dyld along the payload's load chains, which are (1) the
   executables `BUNDLE.toml` declares, `wine.loader`, `wine.server`,
   `wine.preloader` when present, and the `[probes]` entries that are
   Mach-O (the `.exe` probes are PE), each resolved through symlinks to its
   real file, which is the chain (dyld sets `@executable_path` from the
   real file); and (2) every Mach-O executable (`LC_MAIN` or
   `LC_UNIXTHREAD`) under the payload's `wine/` subtree. Rule 2 covers
   Wine 11's real Unix loader: `wine/bin/wine` is a wrapper built from
   `tools/wine` that dlopens `ntdll.so` and re-execs
   `wine/lib/wine/x86_64-unix/wine` (`dlls/ntdll/unix/loader.c`,
   `wineloader`; macOS builds no preloader), so that binary is the main
   executable of every Windows process and the one that dlopens
   `winemac.so`, `winemetal.so`, and `winegstreamer.so`. Executables
   elsewhere in the payload are not chains. `--check` reads `BUNDLE.toml`
   from the payload root for the declared list and fails with "no
   BUNDLE.toml: cannot determine load chains" when it is absent; a declared
   path with no entry at all is tolerated (the tree before Wine is built),
   while a declared path that is a dangling link, resolves outside the
   payload, or names a Mach-O that is no executable is a violation reported
   against `BUNDLE.toml`. Every library and module must resolve each
   `@rpath/` and `@executable_path/` reference under each chain present in
   the tree: `@executable_path` relocates against that executable's
   directory, in dependencies and `LC_RPATH` entries alike, and `@rpath/x`
   is searched through that executable's relocated `LC_RPATH` entries plus
   the binary's own, since dyld consults the rpaths of the image and of the
   images that loaded it up to the main executable and never those of
   unrelated images. A dlopen'd Unix module without an rpath therefore
   resolves through the real loader's; a library one chain can load and
   another cannot is reported once, with every chain that fails named on
   the line (`load chain X`, or `load chains X, Y`); and an executable that
   is no chain (one outside `wine/`, such as a component's tool)
   contributes nothing to any other binary's search and is checked through
   its own rpaths alone. Before Wine is built no chain exists, so each
   binary searches its own rpaths and `@executable_path/` references have
   nothing to resolve against. `@loader_path/x` resolves against the
   binary's own directory. When the binary's `LC_RPATH` entries, or the
   chain's executable's, include a system root such as `/usr/lib/swift`, its
   unresolved `@rpath/` references are left to dyld and `--check` prints how
   many; other absolute `LC_RPATH` entries contribute nothing to the lookup.
   The report prints the chain list it derived. After the real build
   `wineserver` is a chain too, although it dlopens no Unix module: a
   violation reported only under its chain is a candidate for a rule
   change, not a payload defect. Two regular Mach-O files anywhere in the
   payload with the same `LC_ID_DYLIB` are a violation naming both: dyld
   keeps one image per install name per process, so the closure (step 7)
   links such duplicates to one file, and a payload that still carries two
   would bind clients by load order. The check sees load commands only: a
   library Wine `dlopen`s by bare SONAME (step 7) is not a reference it can
   resolve, so the runtime probe below is what proves those load; and the
   compile-time `/usr/local` paths inside the copied gnutls and p11-kit
   (step 7) are strings, not load commands, so it does not see them either.

## Running the built Wine

The loader is `<payload>/wine/bin/wine` (`wine.loader` in `BUNDLE.toml`) and
the server `<payload>/wine/bin/wineserver`. A throwaway prefix:

```sh
export WINEPREFIX=$(mktemp -d)
<payload>/wine/bin/wine wineboot --init
<payload>/wine/bin/wineserver -w
```

To see where every library of a running process comes from, keep one
alive and list its mapped files; nothing may come from `/usr/local` or
`/opt/homebrew`:

```sh
<payload>/wine/bin/wine <payload>/probes/xinput-check.exe &
sleep 5; lsof -p "$(pgrep -f xinput-check.exe | head -1)" | grep -E 'dylib|\.so'
```

`DYLD_PRINT_LIBRARIES=1` prints the same for a single process, but
`arch -x86_64` strips `DYLD_*` from the environment, so pass it through
`/usr/bin/env` after `arch` or run the (thin x86-64) loader directly.

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
