#!/bin/sh
# Builds the cpuid probe as an x86_64-apple-darwin Mach-O with the pinned Rust
# toolchain. Needs `rustup target add x86_64-apple-darwin` (bootstrap.sh).
set -eu
. "$(dirname "$0")/../../../../build/lib.sh"

if ! rustup target list --installed | grep -qx x86_64-apple-darwin; then
  echo "missing: rustup target x86_64-apple-darwin (rustup target add x86_64-apple-darwin)" >&2
  exit 1
fi
out="$WORK/probes"
mkdir -p "$out"
(cd "$(dirname "$0")" && cargo build --release --target x86_64-apple-darwin --target-dir "$WORK/probes/cpuid-target")
cp "$WORK/probes/cpuid-target/x86_64-apple-darwin/release/cpuid" "$out/cpuid"
echo "built $out/cpuid: $(file "$out/cpuid")"
