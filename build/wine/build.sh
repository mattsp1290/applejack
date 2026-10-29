#!/bin/sh
# Builds x86-64 Wine under Rosetta (ADR-0010 Option A) into the payload tree.
# Needs: build/wine/fetch.sh done, x86-64 Homebrew at /usr/local, llvm-mingw
# under build/work/toolchain/ (sh build/bootstrap.sh). Takes hours.
#   WINE_JOBS=<n>          make parallelism (default: hw.ncpu)
#   WINE_SKIP_MSYNC=1      do not attempt the msync patches (gate G4b)
set -eu
. "$(dirname "$0")/../lib.sh"

WINE_VERSION=$(pin_field wine version)
WINE_SRC="$WORK/wine/src/wine-$WINE_VERSION"
BUILD="$WORK/wine/build"
STAGE="$WORK/wine/stage"
PAYLOAD=$(payload_dir)
X86_BREW=/usr/local/bin/brew
LOG="$WORK/wine/build.log"

[ -f "$WINE_SRC/configure" ] || { echo "run build/wine/fetch.sh first (missing $WINE_SRC/configure)" >&2; exit 1; }
arch -x86_64 /usr/bin/true 2>/dev/null || { echo "Rosetta 2 is required" >&2; exit 1; }
[ -x "$X86_BREW" ] || { echo "x86-64 Homebrew at /usr/local is required (sh build/bootstrap.sh --install-homebrew)" >&2; exit 1; }
MINGW_BIN=$(llvm_mingw_bin || true)
[ -n "$MINGW_BIN" ] || { echo "llvm-mingw is required under $TOOLCHAIN (sh build/bootstrap.sh)" >&2; exit 1; }
need bison
need flex

BREW_PREFIX=$(arch -x86_64 "$X86_BREW" --prefix)
export PATH="$MINGW_BIN:$BREW_PREFIX/bin:/usr/bin:/bin:/usr/sbin:/sbin"
export CPPFLAGS="-I$BREW_PREFIX/include"
export LDFLAGS="-L$BREW_PREFIX/lib"
export PKG_CONFIG_PATH="$BREW_PREFIX/lib/pkgconfig:$BREW_PREFIX/opt/openssl/lib/pkgconfig"
export MACOSX_DEPLOYMENT_TARGET=14.0
JOBS=${WINE_JOBS:-$(sysctl -n hw.ncpu)}

# Gate G4b: msync. A patch that does not apply cleanly leaves the tree
# untouched and the gate records "deferred".
MSYNC_APPLIED=no
if [ "${WINE_SKIP_MSYNC:-0}" != 1 ]; then
  for patch in "$WORK"/wine/msync/*/msync-devel.patch "$WORK"/wine/msync/*/msync-staging.patch; do
    [ -f "$patch" ] || continue
    if (cd "$WINE_SRC" && git apply --check "$patch" 2>/dev/null || patch -p1 --dry-run -s <"$patch" >/dev/null 2>&1); then
      (cd "$WINE_SRC" && patch -p1 -s <"$patch")
      MSYNC_APPLIED=$(basename "$patch")
      echo "G4b: applied $MSYNC_APPLIED"
      break
    fi
    echo "G4b: $(basename "$patch") does not apply to wine-$WINE_VERSION"
  done
fi
for patch in "$ROOT"/build/wine/patches/*.patch; do
  [ -f "$patch" ] || continue
  echo "applying project patch $(basename "$patch")"
  (cd "$WINE_SRC" && patch -p1 -s <"$patch")
done
echo "G4b msync: $MSYNC_APPLIED" >"$WORK/wine/g4b.txt"

CONFIGURE_FLAGS="--prefix=$STAGE --enable-archs=i386,x86_64 --with-mingw=llvm-mingw --without-x --enable-winemac-drv --disable-tests"
if grep -q 'AC_ARG_WITH(metal' "$WINE_SRC/configure.ac"; then
  CONFIGURE_FLAGS="$CONFIGURE_FLAGS --with-metal"
fi

mkdir -p "$BUILD"
echo "configure: $CONFIGURE_FLAGS (log: $LOG)"
# shellcheck disable=SC2086
(cd "$BUILD" && arch -x86_64 "$WINE_SRC/configure" $CONFIGURE_FLAGS CC=clang CXX=clang++) 2>&1 | tee "$LOG"
(cd "$BUILD" && arch -x86_64 make -j"$JOBS") 2>&1 | tee -a "$LOG"
rm -rf "$STAGE"
(cd "$BUILD" && arch -x86_64 make install-lib) 2>&1 | tee -a "$LOG"

mkdir -p "$PAYLOAD"
rm -rf "$PAYLOAD/wine"
cp -R "$STAGE" "$PAYLOAD/wine"
find "$PAYLOAD/wine" -type f \( -name '*.dylib' -o -name '*.so' -o -perm -u+x \) -exec sh -c 'file "$1" | grep -q Mach-O && strip -S "$1"' _ {} \;
echo "wine installed into $PAYLOAD/wine"
echo "next: copy non-system dylibs into wine/lib and rewrite install names (assembly step 3), then cargo xtask engine-payload --check $PAYLOAD"
