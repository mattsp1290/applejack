#!/bin/sh
# Builds steamwebhelper-shim.exe (x86_64 PE, GUI subsystem) with mingw_cc
# from build/lib.sh (llvm-mingw pin first, Homebrew mingw-w64 gcc as
# fallback). SHIM_FLAGS overrides the flags appended to the real helper's
# command line (default `--in-process-gpu`; see main.c).
set -eu
. "$(dirname "$0")/../../../../build/lib.sh"

out="$WORK/probes"
mkdir -p "$out"
CC=$(mingw_cc x86_64)
flags=${SHIM_FLAGS:---in-process-gpu}
"$CC" -O2 -Wall -Wextra -Werror -mwindows -municode -static \
  "-DSHIM_FLAGS=L\"$flags\"" \
  -o "$out/steamwebhelper-shim.exe" "$(dirname "$0")/main.c"
echo "built $out/steamwebhelper-shim.exe with $CC (flags: $flags)"
