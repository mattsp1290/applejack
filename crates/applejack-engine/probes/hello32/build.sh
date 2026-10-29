#!/bin/sh
# Builds hello32.exe (i386 PE). Compiler: i686-w64-mingw32-clang from the
# llvm-mingw pin, or i686-w64-mingw32-gcc from Homebrew mingw-w64 as fallback.
set -eu
. "$(dirname "$0")/../../../../build/lib.sh"

out="$WORK/probes"
mkdir -p "$out"
bin=$(llvm_mingw_bin || true)
if [ -n "$bin" ] && [ -x "$bin/i686-w64-mingw32-clang" ]; then
  CC="$bin/i686-w64-mingw32-clang"
elif command -v i686-w64-mingw32-gcc >/dev/null 2>&1; then
  CC=i686-w64-mingw32-gcc
else
  echo "missing: i686-w64-mingw32-clang (llvm-mingw pin) or i686-w64-mingw32-gcc" >&2
  exit 1
fi
"$CC" -O2 -Wall -Wextra -Werror -static -o "$out/hello32.exe" "$(dirname "$0")/main.c"
echo "built $out/hello32.exe with $CC"
