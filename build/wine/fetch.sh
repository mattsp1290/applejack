#!/bin/sh
# Fetches the pinned Wine source and the msync patch set into build/work/wine/.
set -eu
. "$(dirname "$0")/../lib.sh"

WINE_VERSION=$(pin_field wine version)
SRC="$WORK/wine/src"
archive=$(fetch_pin wine)
WINE_SRC="$SRC/wine-$WINE_VERSION"
EXTRACTED="$WORK/wine/.extracted"
# A fresh tree has no patches; build.sh's stamps must not claim otherwise, not
# even over a tree an interrupted run left behind.
rm -f "$WORK/wine/.msync-applied" "$WORK/wine/.patches-applied"
# tar restores the archive's mtimes, and build/work/wine/build persists, so a
# file a patch changed would go back to being older than its object and a
# dropped hunk would stay compiled. Those files are the ones newer than the last
# extract (the archive stands in for a tree extracted before this stamp existed).
# The list stays on disk until the refresh is done: extract destroys what it was
# read from, so a fetch killed here could not work it out again.
PENDING="$WORK/wine/.refresh-pending"
if [ -d "$WINE_SRC" ]; then
  since=$archive
  [ ! -f "$EXTRACTED" ] || since=$EXTRACTED
  find "$WINE_SRC" -type f -newer "$since" >>"$PENDING"
fi
extract "$archive" "$SRC"
# Before the refresh, so a refreshed file stays newer than the stamp and a second
# fetch before the build lists it again.
touch "$EXTRACTED"
if [ -s "$PENDING" ]; then
  while IFS= read -r file; do
    [ ! -f "$file" ] || touch "$file"
  done <"$PENDING"
  echo "wine source: $(sort -u "$PENDING" | wc -l | tr -d ' ') file(s) restored from the archive will rebuild"
fi
rm -f "$PENDING"
[ -f "$WINE_SRC/configure.ac" ] || { echo "wine source not at $WINE_SRC" >&2; exit 1; }

msync=$(fetch_pin wine-msync)
extract "$msync" "$WORK/wine/msync"
echo "msync patches:"
ls "$WORK"/wine/msync/*/*.patch

echo "wine source: $WINE_SRC"
