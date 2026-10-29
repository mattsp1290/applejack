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
  (cd "$ROOT" && cargo xtask "$@")
}

# pin_field <id> <field>: one field of a pin, on stdout.
pin_field() {
  xtask engine-payload --print-pin "$1" "$2" 2>/dev/null
}

# fetch_pin <id>: download and verify one pin; prints the archive path.
fetch_pin() {
  xtask engine-payload --fetch "$1" >&2
  printf '%s/%s\n' "$WORK" "$(pin_field "$1" dest)"
}

# check_listing: denylist scan of the archive listing on stdin.
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
llvm_mingw_bin() {
  dir=$(ls -d "$TOOLCHAIN"/llvm-mingw-*/bin 2>/dev/null | head -1 || true)
  [ -n "$dir" ] && printf '%s\n' "$dir"
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

# list_archive <archive>: the archive listing, one path per line.
list_archive() {
  case "$1" in
    *.tar.zst) tar --zstd -tf "$1" ;;
    *.tar.xz | *.tar.gz | *.tar) tar -tf "$1" ;;
    *.zip) unzip -Z1 "$1" ;;
    *) echo "list_archive: unknown archive type: $1" >&2; exit 1 ;;
  esac
}
