#!/bin/sh
# Builds dx11-sample.exe (x86_64 PE) with mingw_cc from build/lib.sh
# (llvm-mingw pin first, Homebrew mingw-w64 gcc as fallback). Set
# PROBE_ARCH=i686 for a 32-bit syntax-check build.
set -eu
. "$(dirname "$0")/../../../../build/lib.sh"

arch=${PROBE_ARCH:-x86_64}
out="$WORK/probes"
mkdir -p "$out"
CC=$(mingw_cc "$arch")
"$CC" -O2 -Wall -Wextra -Werror -mconsole -static -o "$out/dx11-sample-$arch.exe" \
  "$(dirname "$0")/main.c" -ld3d11 -ldxgi -ldxguid -luser32 -lole32
echo "built $out/dx11-sample-$arch.exe with $CC"
