#!/bin/sh
# Fetches the pinned MoltenVK release and stages the dylib and ICD JSON under
# build/work/vulkan/. The dylib is universal; the payload keeps the x86_64 slice.
set -eu
. "$(dirname "$0")/../lib.sh"

archive=$(fetch_pin moltenvk)
list_archive "$archive" | check_listing
extract "$archive" "$WORK/vulkan/src"
dylib="$WORK/vulkan/src/MoltenVK/MoltenVK/dynamic/dylib/macOS/libMoltenVK.dylib"
icd="$WORK/vulkan/src/MoltenVK/MoltenVK/dynamic/dylib/macOS/MoltenVK_icd.json"
[ -f "$dylib" ] && [ -f "$icd" ] || { echo "MoltenVK archive layout changed; expected $dylib and $icd" >&2; exit 1; }
mkdir -p "$WORK/vulkan/stage"
lipo "$dylib" -thin x86_64 -output "$WORK/vulkan/stage/libMoltenVK.dylib"
cp "$icd" "$WORK/vulkan/stage/MoltenVK_icd.json"
echo "moltenvk $(pin_field moltenvk version): $(file "$WORK/vulkan/stage/libMoltenVK.dylib")"
