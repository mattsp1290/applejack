# Shared helpers for the payload build scripts. Source with
#   . "$(dirname "$0")/../lib.sh"
# after `set -eu`. Pins come from build/pins.toml through `cargo xtask`.

ROOT=$(cd "$(dirname "$0")" && pwd)
while [ ! -f "$ROOT/build/pins.toml" ]; do
  if [ "$ROOT" = / ]; then
    echo "lib.sh: cannot find the repository root (build/pins.toml) above $0" >&2
    exit 1
  fi
  ROOT=$(dirname "$ROOT")
done
WORK="$ROOT/build/work"
TOOLCHAIN="$WORK/toolchain"
PAYLOAD_DIR=${APPLEJACK_PAYLOAD_DIR:-}

xtask() {
  (cd "$ROOT" && cargo -q xtask "$@")
}

# pin_field <id> <field>: one field of a pin, on stdout. Cargo's own output
# stays on stderr so a failing xtask build is visible.
pin_field() {
  xtask engine-payload --print-pin "$1" "$2"
}

# fetch_pin <id>: download, verify, and denylist-scan one pin; prints the
# archive path. The Rust loader lists and scans every archive, present ones
# too, so the scripts extract without a second scan.
fetch_pin() {
  xtask engine-payload --fetch "$1" >&2
  printf '%s/%s\n' "$WORK" "$(pin_field "$1" dest)"
}

# check_listing: denylist scan of a listing on stdin (the staged GStreamer
# tree, where payload-scoped rules apply); an empty listing fails.
check_listing() {
  xtask engine-payload --check-listing - >&2
}

# payload_dir: where the payload tree is assembled.
payload_dir() {
  if [ -n "$PAYLOAD_DIR" ]; then
    printf '%s\n' "$PAYLOAD_DIR"
  else
    printf '%s/fixtures/payloads/rosetta-wine-%s\n' "$ROOT" "$(pin_field wine version)"
  fi
}

# llvm_mingw_bin: the bin directory of the extracted llvm-mingw pin, if present.
# Only the pinned version counts; a stale toolchain must not satisfy the check.
llvm_mingw_bin() {
  version=$(pin_field llvm-mingw version) || return 1
  for dir in "$TOOLCHAIN"/llvm-mingw-"$version"-*/bin; do
    if [ -d "$dir" ]; then
      printf '%s\n' "$dir"
      return 0
    fi
  done
  return 1
}

# mingw_cc <i686|x86_64>: the PE compiler for the probes, llvm-mingw pin
# first, Homebrew mingw-w64 gcc as fallback; prints the compiler.
mingw_cc() {
  bin=$(llvm_mingw_bin || true)
  if [ -n "$bin" ] && [ -x "$bin/$1-w64-mingw32-clang" ]; then
    printf '%s\n' "$bin/$1-w64-mingw32-clang"
  elif command -v "$1-w64-mingw32-gcc" >/dev/null 2>&1; then
    printf '%s\n' "$1-w64-mingw32-gcc"
  else
    echo "missing: $1-w64-mingw32-clang (llvm-mingw pin) or $1-w64-mingw32-gcc" >&2
    return 1
  fi
}

# bison_accepts <bison>: true when <bison> takes the grammar wine-11.0's
# configure feeds it (wine_cv_recent_bison); Apple's bison 2.3 does not.
bison_accepts() {
  conftest=$(mktemp -t bison-conftest) || return 1
  printf '%%define parse.error verbose\n%%%%\nempty: ;\n' >"$conftest"
  "$1" -o /dev/null "$conftest" 2>/dev/null
  status=$?
  rm -f "$conftest"
  return "$status"
}

# need <tool> [hint]: fail with a message when a tool is not on PATH.
need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing: $1${2:+ ($2)}" >&2
    exit 1
  fi
}

# extract <archive> <dir>: unpack into a clean directory.
extract() {
  rm -rf "$2"
  mkdir -p "$2"
  case "$1" in
    *.tar.zst) tar --zstd -xf "$1" -C "$2" ;;
    *.tar.xz | *.tar.gz | *.tar) tar -xf "$1" -C "$2" ;;
    *.zip) unzip -q "$1" -d "$2" ;;
    *) echo "extract: unknown archive type: $1" >&2; exit 1 ;;
  esac
}

