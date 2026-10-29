#!/bin/sh
# Builds x86-64 Wine under Rosetta (ADR-0010 Option A) into the payload tree.
# Needs: build/wine/fetch.sh done, x86-64 Homebrew at /usr/local, llvm-mingw
# under build/work/toolchain/ (sh build/bootstrap.sh). Takes hours.
#   --preflight            run the tool checks and the patch dry runs, change
#                          nothing, and stop before configure
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
WINE_SRC="$WORK/wine/src/wine-$WINE_VERSION"
BUILD="$WORK/wine/build"
STAGE="$WORK/wine/stage"
PAYLOAD=$(payload_dir)
X86_BREW=/usr/local/bin/brew
LOG="$WORK/wine/build.log"
MSYNC_STAMP="$WORK/wine/.msync-applied"
PATCHES_STAMP="$WORK/wine/.patches-applied"
GAPS=0

# fail <what>: fatal, except in preflight, which reports every gap and goes on.
fail() {
  echo "missing: $1" >&2
  [ "$PREFLIGHT" = 1 ] || exit 1
  GAPS=$((GAPS + 1))
}

[ -f "$WINE_SRC/configure" ] || { echo "run build/wine/fetch.sh first (missing $WINE_SRC/configure)" >&2; exit 1; }
arch -x86_64 /usr/bin/true 2>/dev/null || fail "Rosetta 2"
MINGW_BIN=$(llvm_mingw_bin || true)
[ -n "$MINGW_BIN" ] || fail "llvm-mingw $(pin_field llvm-mingw version) under $TOOLCHAIN (sh build/bootstrap.sh)"
BREW_PREFIX=/usr/local
if [ -x "$X86_BREW" ]; then
  BREW_PREFIX=$(arch -x86_64 "$X86_BREW" --prefix)
else
  fail "x86-64 Homebrew at /usr/local (docs/engines/payload-build.md)"
fi

# Homebrew's bison and flex are keg-only. Apple's /usr/bin/bison is 2.3 and
# wine-11.0's configure refuses anything older than 3.0.
export PATH="${MINGW_BIN:+$MINGW_BIN:}$BREW_PREFIX/opt/bison/bin:$BREW_PREFIX/opt/flex/bin:$BREW_PREFIX/bin:/usr/bin:/bin:/usr/sbin:/sbin"
BISON=$(command -v bison || true)
FLEX=$(command -v flex || true)
[ -n "$BISON" ] || fail "bison (arch -x86_64 $X86_BREW install bison)"
[ -n "$FLEX" ] || fail "flex (arch -x86_64 $X86_BREW install flex)"
export CPPFLAGS="-I$BREW_PREFIX/include"
export LDFLAGS="-L$BREW_PREFIX/lib"
export PKG_CONFIG_PATH="$BREW_PREFIX/lib/pkgconfig:$BREW_PREFIX/opt/openssl/lib/pkgconfig"
export MACOSX_DEPLOYMENT_TARGET=14.0
JOBS=${WINE_JOBS:-$(sysctl -n hw.ncpu)}

# The same grammar wine-11.0's configure feeds bison (wine_cv_recent_bison);
# failing here names the fix instead of "Your bison version is too old".
if [ -n "$BISON" ]; then
  conftest="$WORK/wine/conftest.y"
  printf '%%define parse.error verbose\n%%%%\nempty: ;\n' >"$conftest"
  if "$BISON" -o /dev/null "$conftest" 2>/dev/null; then
    echo "bison: $("$BISON" --version | head -1) at $BISON accepts '%define parse.error verbose'"
  else
    fail "bison >= 3.0: $BISON is $("$BISON" --version | head -1) and rejects '%define parse.error verbose' (arch -x86_64 $X86_BREW install bison)"
  fi
  rm -f "$conftest"
fi
[ -z "$FLEX" ] || echo "flex: $("$FLEX" --version | head -1) at $FLEX"

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
# none is, the tree stays untouched and the gate records "deferred".
if [ -f "$MSYNC_STAMP" ]; then
  MSYNC_APPLIED=$(cat "$MSYNC_STAMP")
  echo "G4b: stamp present ($MSYNC_APPLIED); msync step skipped"
elif [ "${WINE_SKIP_MSYNC:-0}" = 1 ]; then
  MSYNC_APPLIED=skipped
  echo "G4b: WINE_SKIP_MSYNC=1; msync step skipped"
  write_stamp "$MSYNC_STAMP" "$MSYNC_APPLIED"
else
  MSYNC_APPLIED=deferred
  for patch in "$WORK"/wine/msync/*/msync-devel.patch "$WORK"/wine/msync/*/msync-staging.patch; do
    [ -f "$patch" ] || continue
    if report=$(patch_dry_run "$patch"); then
      apply_patch "$patch"
      MSYNC_APPLIED=$(basename "$patch")
      echo "G4b: applied $MSYNC_APPLIED"
      break
    fi
    echo "G4b: $(basename "$patch") does not apply to wine-$WINE_VERSION:"
    printf '%s\n' "$report" | grep 'hunks failed' | sed 's/^/  /'
  done
  [ "$MSYNC_APPLIED" != deferred ] || echo "G4b: deferred (no msync patch applies; the tree is unchanged)"
  write_stamp "$MSYNC_STAMP" "$MSYNC_APPLIED"
fi
[ "$PREFLIGHT" = 1 ] || echo "G4b msync: $MSYNC_APPLIED" >"$WORK/wine/g4b.txt"

# Project patches: every patch must dry-run clean before any is applied, so a
# failing set never leaves the tree half patched.
if [ -f "$PATCHES_STAMP" ]; then
  echo "project patches: stamp present; step skipped"
else
  applied=""
  for patch in "$ROOT"/build/wine/patches/*.patch; do
    [ -f "$patch" ] || continue
    if ! report=$(patch_dry_run "$patch"); then
      echo "project patch $(basename "$patch") does not apply to wine-$WINE_VERSION:" >&2
      printf '%s\n' "$report" | sed 's/^/  /' >&2
      exit 1
    fi
  done
  for patch in "$ROOT"/build/wine/patches/*.patch; do
    [ -f "$patch" ] || continue
    apply_patch "$patch"
    applied="$applied $(basename "$patch")"
    echo "applied project patch $(basename "$patch")"
  done
  [ -n "$applied" ] || echo "project patches: none"
  write_stamp "$PATCHES_STAMP" "${applied# }"
fi

CONFIGURE_FLAGS="--prefix=$STAGE --enable-archs=i386,x86_64 --with-mingw=llvm-mingw --without-x --enable-winemac-drv --disable-tests"
if grep -q 'AC_ARG_WITH(metal' "$WINE_SRC/configure.ac"; then
  CONFIGURE_FLAGS="$CONFIGURE_FLAGS --with-metal"
fi

if [ "$PREFLIGHT" = 1 ]; then
  echo "preflight: configure would run as: arch -x86_64 $WINE_SRC/configure $CONFIGURE_FLAGS CC=clang CXX=clang++ BISON=${BISON:-bison} FLEX=${FLEX:-flex}"
  if [ "$GAPS" -gt 0 ]; then
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
echo "configure: $CONFIGURE_FLAGS (log: $LOG)"
# shellcheck disable=SC2086
logged "$WINE_SRC/configure" $CONFIGURE_FLAGS CC=clang CXX=clang++ BISON="$BISON" FLEX="$FLEX"
logged make -j"$JOBS"
# The stage is replaced only after make succeeded, so a failed rerun keeps the last good one.
rm -rf "$STAGE"
logged make install-lib
echo "wine installed into $STAGE"
echo "next: sh build/assemble.sh copies it into $PAYLOAD/wine and strips it"
