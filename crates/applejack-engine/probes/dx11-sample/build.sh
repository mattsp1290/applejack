#!/bin/sh
# Builds dx11-sample.exe (x86_64 PE). Compiler: x86_64-w64-mingw32-clang from
# the llvm-mingw pin, or x86_64-w64-mingw32-gcc from Homebrew mingw-w64 as
# fallback. Set PROBE_ARCH=i686 for a 32-bit syntax-check build.
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
"$CC" -O2 -Wall -Wextra -Werror -mconsole -static -o "$out/dx11-sample-$arch.exe" \
  "$(dirname "$0")/main.c" -ld3d11 -ldxgi -ldxguid -luser32 -lole32
echo "built $out/dx11-sample-$arch.exe with $CC"
