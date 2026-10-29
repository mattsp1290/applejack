#!/bin/sh
# Builds hello32.exe (i386 PE) with mingw_cc from build/lib.sh (llvm-mingw
# pin first, Homebrew mingw-w64 gcc as fallback).
set -eu
. "$(dirname "$0")/../../../../build/lib.sh"

out="$WORK/probes"
mkdir -p "$out"
CC=$(mingw_cc i686)
"$CC" -O2 -Wall -Wextra -Werror -static -o "$out/hello32.exe" "$(dirname "$0")/main.c"
echo "built $out/hello32.exe with $CC"
