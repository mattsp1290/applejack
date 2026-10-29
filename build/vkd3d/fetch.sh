#!/bin/sh
# Fetches the pinned vkd3d-proton release and stages its DLLs under build/work/vkd3d/.
set -eu
. "$(dirname "$0")/../lib.sh"

need zstd "brew install zstd; tar needs it for .tar.zst"
archive=$(fetch_pin vkd3d-proton)
extract "$archive" "$WORK/vkd3d/src"
version=$(pin_field vkd3d-proton version)
echo "vkd3d-proton $version DLLs:"
find "$WORK/vkd3d/src" -type f -name '*.dll' | sort
