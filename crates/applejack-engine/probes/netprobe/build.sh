#!/bin/sh
# Builds netprobe.exe (x86_64 PE, console subsystem) with mingw_cc from
# build/lib.sh (llvm-mingw pin first, Homebrew mingw-w64 gcc as fallback).
# Run it in a prefix with
#   applejack prefix exec steam-client -- Z:$ROOT/build/work/probes/netprobe.exe
# and add --env WINEDEBUG=+winsock,+iphlpapi,+nsi,+dnsapi for Wine's view.
set -eu
. "$(dirname "$0")/../../../../build/lib.sh"

out="$WORK/probes"
mkdir -p "$out"
CC=$(mingw_cc x86_64)
"$CC" -O2 -Wall -Wextra -Werror -mconsole -static \
  -o "$out/netprobe.exe" "$(dirname "$0")/main.c" \
  -liphlpapi -ldnsapi -lwinhttp -lws2_32
echo "built $out/netprobe.exe with $CC"
