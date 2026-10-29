#!/bin/sh
# Fetches the pinned Wine source and the msync patch set into build/work/wine/
# and reports whether the pinned configure.ac knows --with-metal.
set -eu
. "$(dirname "$0")/../lib.sh"

WINE_VERSION=$(pin_field wine version)
SRC="$WORK/wine/src"
archive=$(fetch_pin wine)
list_archive "$archive" | check_listing
extract "$archive" "$SRC"
WINE_SRC="$SRC/wine-$WINE_VERSION"
[ -f "$WINE_SRC/configure.ac" ] || { echo "wine source not at $WINE_SRC" >&2; exit 1; }

msync=$(fetch_pin wine-msync)
extract "$msync" "$WORK/wine/msync"
echo "msync patches:"
ls "$WORK"/wine/msync/*/*.patch

if grep -q 'AC_ARG_WITH(metal' "$WINE_SRC/configure.ac"; then
  echo "configure.ac: --with-metal exists at wine-$WINE_VERSION"
else
  echo "configure.ac: no --with-metal option at wine-$WINE_VERSION (Metal is auto-detected; see docs/engines/payload-build.md)"
fi
echo "wine source: $WINE_SRC"
