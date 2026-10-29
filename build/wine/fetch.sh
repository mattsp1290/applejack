#!/bin/sh
# Fetches the pinned Wine source and the msync patch set into build/work/wine/.
# build/wine/build.sh adds --with-metal to configure when configure.ac offers it.
set -eu
. "$(dirname "$0")/../lib.sh"

WINE_VERSION=$(pin_field wine version)
SRC="$WORK/wine/src"
archive=$(fetch_pin wine)
extract "$archive" "$SRC"
# A fresh tree has no patches; build.sh's stamps must not claim otherwise.
rm -f "$WORK/wine/.msync-applied" "$WORK/wine/.patches-applied"
WINE_SRC="$SRC/wine-$WINE_VERSION"
[ -f "$WINE_SRC/configure.ac" ] || { echo "wine source not at $WINE_SRC" >&2; exit 1; }

msync=$(fetch_pin wine-msync)
extract "$msync" "$WORK/wine/msync"
echo "msync patches:"
ls "$WORK"/wine/msync/*/*.patch

echo "wine source: $WINE_SRC"
