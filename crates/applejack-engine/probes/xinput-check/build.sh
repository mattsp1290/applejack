#!/bin/sh
# Builds xinput-check.exe (x86_64 PE) against xinput1_4 with mingw_cc from
# build/lib.sh (llvm-mingw pin first, Homebrew mingw-w64 gcc as fallback).
# Set PROBE_ARCH=i686 for a 32-bit build.
set -eu
. "$(dirname "$0")/../../../../build/lib.sh"

arch=${PROBE_ARCH:-x86_64}
out="$WORK/probes"
mkdir -p "$out"
CC=$(mingw_cc "$arch")
"$CC" -O2 -Wall -Wextra -Werror -static -o "$out/xinput-check-$arch.exe" \
  "$(dirname "$0")/main.c" -lxinput1_4
echo "built $out/xinput-check-$arch.exe with $CC"
