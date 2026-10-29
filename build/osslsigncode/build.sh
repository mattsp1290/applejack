#!/bin/sh
# Builds the pinned osslsigncode as a host-arch tool for Authenticode checks.
# Needs cmake and OpenSSL (brew install cmake openssl@3).
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
cmake -S "$src" -B "$build" -DCMAKE_BUILD_TYPE=Release ${openssl_root:+-DOPENSSL_ROOT_DIR="$openssl_root"}
cmake --build "$build" --parallel
out="$WORK/osslsigncode/osslsigncode"
cp "$build/osslsigncode" "$out"
"$out" --version
echo "osslsigncode built at $out"
