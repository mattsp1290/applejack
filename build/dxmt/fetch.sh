#!/bin/sh
# Fetches the pinned DXMT release and stages its DLLs under build/work/dxmt/.
set -eu
. "$(dirname "$0")/../lib.sh"

archive=$(fetch_pin dxmt)
extract "$archive" "$WORK/dxmt/src"
version=$(pin_field dxmt version)
echo "dxmt $version DLLs:"
find "$WORK/dxmt/src" -type f \( -name '*.dll' -o -name '*.so' \) | sort
