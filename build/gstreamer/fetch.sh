#!/bin/sh
# Fetches the upstream GStreamer macOS runtime package and expands it under
# build/work/gstreamer/ without installing it. Never the Gcenx asset.
set -eu
. "$(dirname "$0")/../lib.sh"

archive=$(fetch_pin gstreamer)
expanded="$WORK/gstreamer/expanded"
stage="$WORK/gstreamer/stage/GStreamer.framework"
rm -rf "$expanded" "$stage"
pkgutil --expand-full "$archive" "$expanded"
mkdir -p "$stage"
for component in "$expanded"/*.pkg; do
  [ -d "$component/Payload" ] || continue
  # Each sub-package declares where under /Library/Frameworks/GStreamer.framework it installs.
  location=$(sed -n 's/.*install-location="\/Library\/Frameworks\/GStreamer.framework\/\{0,1\}\([^"]*\)".*/\1/p' "$component/PackageInfo" | head -1)
  mkdir -p "$stage/$location"
  cp -R "$component/Payload/." "$stage/$location/"
done
find "$stage" -type f | sed "s|$WORK/gstreamer/stage/||" | check_listing
echo "gstreamer $(pin_field gstreamer version) expanded into $stage"
echo "x86_64 slices: $(find "$stage/Versions" -name '*.dylib' -maxdepth 3 | head -1 | xargs file)"
