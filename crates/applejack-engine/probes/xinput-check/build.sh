#!/bin/sh
# Builds xinput-check.exe (x86_64 PE) against xinput1_4. Compiler as for
# dx11-sample: llvm-mingw pin first, Homebrew mingw-w64 gcc as fallback.
set -eu
. "$(dirname "$0")/../../../../build/lib.sh"

arch=${PROBE_ARCH:-x86_64}
out="$WORK/probes"
mkdir -p "$out"
bin=$(llvm_mingw_bin || true)
if [ -n "$bin" ] && [ -x "$bin/$arch-w64-mingw32-clang" ]; then
  CC="$bin/$arch-w64-mingw32-clang"
elif command -v "$arch-w64-mingw32-gcc" >/dev/null 2>&1; then
  CC="$arch-w64-mingw32-gcc"
else
  echo "missing: $arch-w64-mingw32-clang (llvm-mingw pin) or $arch-w64-mingw32-gcc" >&2
  exit 1
fi
"$CC" -O2 -Wall -Wextra -Werror -static -o "$out/xinput-check-$arch.exe" \
  "$(dirname "$0")/main.c" -lxinput1_4
echo "built $out/xinput-check-$arch.exe with $CC"
