#!/bin/sh
# Builds x86-64 Wine under Rosetta (ADR-0010 Option A) into the payload tree.
# Needs: build/wine/fetch.sh and build/gstreamer/fetch.sh done, x86-64
# Homebrew at /usr/local, llvm-mingw under build/work/toolchain/
# (sh build/bootstrap.sh). Takes hours.
#   --preflight            run the tool checks and the patch trials, change
#                          nothing, and stop before configure. Exit 0: the
#                          build can start; 3: the only gaps are ones
#                          `sh build/bootstrap.sh` installs; 1: anything else
#   WINE_JOBS=<n>          make parallelism (default: hw.ncpu)
#   WINE_SKIP_MSYNC=1      do not attempt the msync patches (gate G4b)
# Reruns skip the patch steps through the stamps build/work/wine/.msync-applied
# and .patches-applied, which build/wine/fetch.sh removes when it re-extracts.
set -eu
. "$(dirname "$0")/../lib.sh"

PREFLIGHT=0
for arg in "$@"; do
  case "$arg" in
    --preflight) PREFLIGHT=1 ;;
    *) echo "usage: $0 [--preflight]" >&2; exit 2 ;;
  esac
done

WINE_VERSION=$(pin_field wine version)
LLVM_MINGW_VERSION=$(pin_field llvm-mingw version)
WINE_SRC="$WORK/wine/src/wine-$WINE_VERSION"
BUILD="$WORK/wine/build"
STAGE="$WORK/wine/stage"
PAYLOAD=$(payload_dir)
X86_BREW=/usr/local/bin/brew
LOG="$WORK/wine/build.log"
MSYNC_STAMP="$WORK/wine/.msync-applied"
PATCHES_STAMP="$WORK/wine/.patches-applied"
# The devel package supplies headers and .pc files to configure; the staged,
# thinned libraries are what the link references point at, so the closure
# step rewrites them to what ships.
GST_DEVEL="$WORK/gstreamer/devel/GStreamer.framework/Versions/1.0"
GST_STAGE="$WORK/gstreamer/stage/GStreamer.framework/Versions/1.0"
GST_PKGS="gstreamer-1.0 gstreamer-video-1.0 gstreamer-audio-1.0 gstreamer-tag-1.0"
GAPS=0
BOOTSTRAP_GAPS=0

# fail <what> [bootstrap]: fatal, except in preflight, which reports every gap
# and goes on. "bootstrap" marks a gap `sh build/bootstrap.sh` closes.
fail() {
  echo "missing: $1" >&2
  [ "$PREFLIGHT" = 1 ] || exit 1
  GAPS=$((GAPS + 1))
  if [ "${2:-}" = bootstrap ]; then
    BOOTSTRAP_GAPS=$((BOOTSTRAP_GAPS + 1))
  fi
}

[ -f "$WINE_SRC/configure" ] || { echo "run build/wine/fetch.sh first (missing $WINE_SRC/configure)" >&2; exit 1; }
arch -x86_64 /usr/bin/true 2>/dev/null || fail "Rosetta 2"
MINGW_BIN=$(llvm_mingw_bin || true)
[ -n "$MINGW_BIN" ] || fail "llvm-mingw $LLVM_MINGW_VERSION under $TOOLCHAIN (sh build/bootstrap.sh)" bootstrap
BREW_PREFIX=/usr/local
if [ -x "$X86_BREW" ]; then
  BREW_PREFIX=$(arch -x86_64 "$X86_BREW" --prefix)
else
  fail "x86-64 Homebrew at /usr/local (docs/engines/payload-build.md)"
fi
[ -f "$GST_DEVEL/lib/pkgconfig/gstreamer-1.0.pc" ] || fail "GStreamer devel package under $GST_DEVEL (sh build/gstreamer/fetch.sh)"
[ -f "$GST_STAGE/lib/libgstreamer-1.0.dylib" ] || fail "staged GStreamer under $GST_STAGE (sh build/gstreamer/fetch.sh)"

# Every pin value is read above this line: the restricted PATH below has no
# cargo, so pin_field and payload_dir cannot be called after it.
# Homebrew's bison and flex are keg-only. Apple's /usr/bin/bison is 2.3 and
# wine-11.0's configure refuses anything older than 3.0.
export PATH="${MINGW_BIN:+$MINGW_BIN:}$BREW_PREFIX/opt/bison/bin:$BREW_PREFIX/opt/flex/bin:$BREW_PREFIX/bin:/usr/bin:/bin:/usr/sbin:/sbin"
BISON=$(command -v bison || true)
FLEX=$(command -v flex || true)
[ -n "$BISON" ] || fail "bison (sh build/bootstrap.sh --install-brew-deps)" bootstrap
[ -n "$FLEX" ] || fail "flex (sh build/bootstrap.sh --install-brew-deps)" bootstrap
export CPPFLAGS="-I$BREW_PREFIX/include"
export LDFLAGS="-L$BREW_PREFIX/lib"
# The upstream .pc files are relocatable (prefix=${pcfiledir}/../..), so the
# expanded devel tree resolves through PKG_CONFIG_PATH alone. It goes last:
# it also carries freetype2.pc, fontconfig.pc, zlib.pc, and the rest of
# GStreamer's base system, which must not shadow the Homebrew packages Wine
# links (the devel lib/ holds static archives and dangling .dylib links).
export PKG_CONFIG_PATH="$BREW_PREFIX/lib/pkgconfig:$BREW_PREFIX/opt/openssl/lib/pkgconfig:$GST_DEVEL/lib/pkgconfig"
export MACOSX_DEPLOYMENT_TARGET=14.0
JOBS=${WINE_JOBS:-$(sysctl -n hw.ncpu)}

# Failing the conftest here names the fix instead of configure's "Your bison
# version is too old".
if [ -n "$BISON" ]; then
  if bison_accepts "$BISON"; then
    echo "bison: $("$BISON" --version | head -1) at $BISON accepts '%define parse.error verbose'"
  else
    fail "bison >= 3.0: $BISON is $("$BISON" --version | head -1) and rejects '%define parse.error verbose' (sh build/bootstrap.sh --install-brew-deps)" bootstrap
  fi
fi
[ -z "$FLEX" ] || echo "flex: $("$FLEX" --version | head -1) at $FLEX"

# What configure resolves through pkg-config. A miss is not a configure
# error: it silently disables the matching Wine feature (GStreamer: a notice
# and no winegstreamer), so it is a gap here.
PKG_CONFIG=$(command -v pkg-config || command -v pkgconf || true)
if [ -n "$PKG_CONFIG" ]; then
  for pkg in $GST_PKGS freetype2 gnutls sdl2; do
    if arch -x86_64 "$PKG_CONFIG" --exists "$pkg" 2>/dev/null; then
      echo "pkg-config: $pkg $(arch -x86_64 "$PKG_CONFIG" --modversion "$pkg") in $(arch -x86_64 "$PKG_CONFIG" --variable=pcfiledir "$pkg")"
    else
      case " $GST_PKGS " in
        *" $pkg "*) fail "pkg-config package $pkg under $GST_DEVEL/lib/pkgconfig (sh build/gstreamer/fetch.sh; configure builds no winegstreamer without it)" ;;
        *) fail "pkg-config package $pkg (sh build/bootstrap.sh --install-brew-deps; configure disables the matching Wine feature without it)" bootstrap ;;
      esac
    fi
  done
else
  fail "pkg-config (sh build/bootstrap.sh --install-brew-deps); not probed: $GST_PKGS freetype2 gnutls sdl2" bootstrap
fi
# Vulkan is not a pkg-config package on macOS: configure links -lMoltenVK
# (WINE_CHECK_SONAME) from LDFLAGS, so the dylib is what to look for.
if [ -f "$BREW_PREFIX/lib/libMoltenVK.dylib" ]; then
  echo "vulkan: $BREW_PREFIX/lib/libMoltenVK.dylib (molten-vk)"
else
  fail "libMoltenVK.dylib under $BREW_PREFIX/lib (sh build/bootstrap.sh --install-brew-deps; configure disables Vulkan without it)" bootstrap
fi

# patch_dry_run <patch>: GNU patch's report on stdout; exit 1 when any hunk fails.
patch_dry_run() {
  (cd "$WINE_SRC" && patch -p1 -N -f -s --dry-run <"$1" 2>&1)
}
apply_patch() {
  if [ "$PREFLIGHT" = 1 ]; then
    echo "preflight: would apply $(basename "$1")"
  else
    (cd "$WINE_SRC" && patch -p1 -N -f -s <"$1")
  fi
}
write_stamp() {
  [ "$PREFLIGHT" = 1 ] || printf '%s\n' "$2" >"$1"
}

# Gate G4b: msync. The first candidate whose dry run is clean is applied; when
# none is, the tree stays untouched and the gate records "deferred". No
# candidate at all is a missing prerequisite (build/wine/fetch.sh), not a gate result.
if [ -f "$MSYNC_STAMP" ]; then
  MSYNC_APPLIED=$(cat "$MSYNC_STAMP")
  echo "G4b: stamp present ($MSYNC_APPLIED); msync step skipped"
elif [ "${WINE_SKIP_MSYNC:-0}" = 1 ]; then
  MSYNC_APPLIED=skipped
  echo "G4b: WINE_SKIP_MSYNC=1; msync step skipped"
  write_stamp "$MSYNC_STAMP" "$MSYNC_APPLIED"
else
  MSYNC_APPLIED=deferred
  found=0
  for patch in "$WORK"/wine/msync/*/msync-devel.patch "$WORK"/wine/msync/*/msync-staging.patch; do
    [ -f "$patch" ] || continue
    found=1
    if report=$(patch_dry_run "$patch"); then
      apply_patch "$patch"
      MSYNC_APPLIED=$(basename "$patch")
      echo "G4b: applied $MSYNC_APPLIED"
      break
    fi
    echo "G4b: $(basename "$patch") does not apply to wine-$WINE_VERSION:"
    printf '%s\n' "$report" | grep 'hunks failed' | sed 's/^/  /'
  done
  if [ "$found" = 0 ]; then
    fail "msync patches under $WORK/wine/msync (sh build/wine/fetch.sh)"
  else
    [ "$MSYNC_APPLIED" != deferred ] || echo "G4b: deferred (no msync patch applies; the tree is unchanged)"
    write_stamp "$MSYNC_STAMP" "$MSYNC_APPLIED"
  fi
fi
[ "$PREFLIGHT" = 1 ] || echo "G4b msync: $MSYNC_APPLIED" >"$WORK/wine/g4b.txt"

# Project patches: the whole set is applied in file-name order to a clone of
# the tree first (clonefile on APFS, so the copy is cheap) and to the real
# tree only when every patch applied there, so a failing set never leaves the
# tree half patched. In preflight the clone is the unpatched tree, since msync
# is only "would apply" there; the real run patches after msync.
if [ -f "$PATCHES_STAMP" ]; then
  echo "project patches: stamp present; step skipped"
else
  set -- "$ROOT"/build/wine/patches/*.patch
  if [ -f "$1" ]; then
    trial="$WORK/wine/patch-trial"
    rm -rf "$trial"
    cp -cR "$WINE_SRC" "$trial"
    failed=""
    for patch in "$@"; do
      if ! report=$(cd "$trial" && patch -p1 -N -f -s <"$patch" 2>&1); then
        failed=$(basename "$patch")
        echo "project patch $failed does not apply in sequence to wine-$WINE_VERSION:" >&2
        printf '%s\n' "$report" | sed 's/^/  /' >&2
        break
      fi
    done
    rm -rf "$trial"
    if [ -n "$failed" ]; then
      # Fatal in the real run; preflight records the gap and reports the rest.
      fail "project patch set that applies in sequence ($failed fails; fix build/wine/patches)"
    else
      names=""
      for patch in "$@"; do
        apply_patch "$patch"
        names="$names $(basename "$patch")"
      done
      if [ "$PREFLIGHT" = 1 ]; then
        echo "preflight: $# project patch(es) apply in sequence (trial on the tree without msync applied):$names"
      else
        echo "applied $# project patch(es):$names"
      fi
      write_stamp "$PATCHES_STAMP" "${names# }"
    fi
  else
    echo "project patches: none"
    write_stamp "$PATCHES_STAMP" ""
  fi
fi

# Positional parameters carry the configure arguments, so a checkout path
# with a space survives. --with-gstreamer turns the "GStreamer won't be
# supported" notice into a configure error.
set -- "--prefix=$STAGE" --enable-archs=i386,x86_64 --with-mingw=llvm-mingw --without-x --enable-winemac-drv --with-gstreamer --disable-tests
if grep -q 'AC_ARG_WITH(metal' "$WINE_SRC/configure.ac"; then
  set -- "$@" --with-metal
fi
# Link against the staged, thinned libraries rather than the devel tree's;
# the .pc files still supply GSTREAMER_CFLAGS.
GSTREAMER_LIBS="-L$GST_STAGE/lib -lgstreamer-1.0 -lgstvideo-1.0 -lgstaudio-1.0 -lgsttag-1.0 -lgobject-2.0 -lglib-2.0"

if [ "$PREFLIGHT" = 1 ]; then
  echo "preflight: configure would run as: arch -x86_64 $WINE_SRC/configure $* CC=clang CXX=clang++ BISON=${BISON:-bison} FLEX=${FLEX:-flex} GSTREAMER_LIBS='$GSTREAMER_LIBS'"
  if [ "$GAPS" -gt 0 ]; then
    if [ "$GAPS" -eq "$BOOTSTRAP_GAPS" ]; then
      echo "preflight: $GAPS prerequisite(s) missing, all installed by sh build/bootstrap.sh; stopping before configure (exit 3)"
      exit 3
    fi
    echo "preflight: $GAPS prerequisite(s) missing; stopping before configure"
    exit 1
  fi
  echo "preflight: ok; stopping before configure"
  exit 0
fi

# logged <cmd...>: run under arch -x86_64 in $BUILD, append to $LOG, and keep
# the command's exit status (sh has no pipefail, so `cmd | tee` would hide it).
logged() {
  (set +e; cd "$BUILD" && arch -x86_64 "$@" 2>&1; echo "$?" >"$WORK/wine/.status") | tee -a "$LOG"
  status=$(cat "$WORK/wine/.status")
  [ "$status" -eq 0 ] || { echo "$1 failed ($status); see $LOG" >&2; exit 1; }
}

mkdir -p "$BUILD"
: >"$LOG"
echo "configure: $* (log: $LOG)"
logged "$WINE_SRC/configure" "$@" CC=clang CXX=clang++ BISON="$BISON" FLEX="$FLEX" GSTREAMER_LIBS="$GSTREAMER_LIBS"
logged make -j"$JOBS"
# The stage is replaced only after make succeeded, so a failed rerun keeps the last good one.
rm -rf "$STAGE"
logged make install-lib
echo "wine installed into $STAGE"
echo "next: sh build/assemble.sh copies it into $PAYLOAD/wine and strips it"
