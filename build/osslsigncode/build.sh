#!/bin/sh
# Builds the pinned osslsigncode as a host-arch tool for Authenticode checks.
# Needs cmake and OpenSSL (brew install cmake openssl@3). OpenSSL is linked
# statically: the tool ships inside the payload, whose check forbids a
# load command into /opt/homebrew.
set -eu
. "$(dirname "$0")/../lib.sh"

need cmake "brew install cmake"
archive=$(fetch_pin osslsigncode)
version=$(pin_field osslsigncode version)
extract "$archive" "$WORK/osslsigncode/src"
src="$WORK/osslsigncode/src/osslsigncode-$version"
build="$WORK/osslsigncode/build"
openssl_root=${OPENSSL_ROOT_DIR:-$(brew --prefix openssl@3 2>/dev/null || true)}
rm -rf "$build"
cmake -S "$src" -B "$build" -DCMAKE_BUILD_TYPE=Release \
  -DOPENSSL_USE_STATIC_LIBS=ON \
  ${openssl_root:+-DOPENSSL_ROOT_DIR="$openssl_root"}
cmake --build "$build" --parallel
out="$WORK/osslsigncode/osslsigncode"
cp "$build/osslsigncode" "$out"
# A host tool inside the payload meets the same self-containment rule as
# Wine: nothing outside /usr/lib and /System.
if otool -L "$out" | tail -n +2 | grep -v -e '/usr/lib/' -e '/System/'; then
  echo "osslsigncode links outside /usr/lib and /System (see above); the payload check would reject it" >&2
  exit 1
fi
"$out" --version
echo "osslsigncode built at $out ($(otool -L "$out" | tail -n +2 | wc -l | tr -d ' ') system load commands)"
