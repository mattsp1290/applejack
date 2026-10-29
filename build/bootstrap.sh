#!/bin/sh
# Prerequisite check and installer for the Rosetta engine payload build.
#   bootstrap.sh --check              report every missing prerequisite, install nothing (exit 1 if any)
#   bootstrap.sh                      install the Rust target and llvm-mingw under build/work/toolchain/
#   bootstrap.sh --install-brew-deps  also install the Wine deps with the x86-64 Homebrew at /usr/local
# Homebrew itself is never installed from here: its installer is an unpinned
# script fetched from HEAD. Install it per https://brew.sh under `arch -x86_64`
# first. Every mode records the x86-64 formula versions into
# build/work/brew-versions.txt. `--check` exiting 0 means every script in
# `cargo xtask engine-payload --build` can start.
set -eu
. "$(dirname "$0")/lib.sh"

MODE=install
INSTALL_BREW_DEPS=0
for arg in "$@"; do
  case "$arg" in
    --check) MODE=check ;;
    --install-brew-deps) INSTALL_BREW_DEPS=1 ;;
    *) echo "usage: $0 [--check] [--install-brew-deps]" >&2; exit 2 ;;
  esac
done

X86_BREW=/usr/local/bin/brew
BREW_DEPS="freetype gnutls sdl2 molten-vk bison flex pkgconf"
# Homebrew renamed pkg-config to pkgconf; either satisfies Wine's configure.
alt_for() { case "$1" in pkgconf) echo pkg-config ;; *) echo "$1" ;; esac; }
hint_for() {
  case "$1" in
    zstd) echo "brew install zstd (tar needs it for .tar.zst)" ;;
    cmake) echo "brew install cmake (osslsigncode)" ;;
    cargo | rustup) echo "install Rust per https://rustup.rs" ;;
    *) echo "install Xcode or the command line tools" ;;
  esac
}
missing=0
gap() {
  echo "missing: $1"
  echo "    fix: $2"
  missing=$((missing + 1))
}
ok() {
  echo "ok:      $1"
}

echo "== host"
[ "$(uname -m)" = arm64 ] || gap "arm64 host (found $(uname -m))" "run on Apple Silicon"
if arch -x86_64 /usr/bin/true 2>/dev/null; then ok "Rosetta 2"; else gap "Rosetta 2" "softwareupdate --install-rosetta --agree-to-license"; fi
if xcode-select -p >/dev/null 2>&1; then ok "Xcode command line tools ($(xcode-select -p))"; else gap "Xcode command line tools" "xcode-select --install"; fi
# Everything a script in BUILD_SCRIPTS calls, bison apart (version-checked below).
for tool in clang otool install_name_tool codesign lipo strip file flex curl shasum tar unzip xar lsbom pkgutil git zstd cmake cargo rustup; do
  if command -v "$tool" >/dev/null 2>&1; then ok "$tool"; else gap "$tool" "$(hint_for "$tool")"; fi
done

echo "== Rust"
if rustup target list --installed 2>/dev/null | grep -qx x86_64-apple-darwin; then
  ok "rustup target x86_64-apple-darwin"
else
  gap "rustup target x86_64-apple-darwin (cpuid probe)" "rustup target add x86_64-apple-darwin"
fi

echo "== llvm-mingw (PE side of Wine and the probes)"
LLVM_MINGW_VERSION=$(pin_field llvm-mingw version)
bin=$(llvm_mingw_bin || true)
if [ -n "$bin" ] && [ -x "$bin/x86_64-w64-mingw32-clang" ] && [ -x "$bin/i686-w64-mingw32-clang" ]; then
  ok "llvm-mingw $LLVM_MINGW_VERSION at $bin"
else
  gap "llvm-mingw $LLVM_MINGW_VERSION under $TOOLCHAIN/" "sh build/bootstrap.sh (extracts the llvm-mingw pin)"
fi
for cc in x86_64-w64-mingw32-gcc i686-w64-mingw32-gcc; do
  if command -v "$cc" >/dev/null 2>&1; then ok "$cc on PATH (Homebrew mingw-w64, fallback only)"; else echo "info:    $cc not on PATH (optional fallback)"; fi
done

echo "== x86-64 Homebrew (Wine build dependencies)"
X86_PREFIX=""
if [ -x "$X86_BREW" ] && X86_PREFIX=$(arch -x86_64 "$X86_BREW" --prefix 2>/dev/null); then
  ok "x86-64 Homebrew at $X86_PREFIX"
  for dep in $BREW_DEPS; do
    if arch -x86_64 "$X86_BREW" list --versions "$dep" >/dev/null 2>&1 \
      || arch -x86_64 "$X86_BREW" list --versions "$(alt_for "$dep")" >/dev/null 2>&1; then
      ok "brew (x86-64) $dep"
    else
      gap "brew (x86-64) $dep" "arch -x86_64 $X86_BREW install $dep"
    fi
  done
  # The formula versions become payload inputs (their dylibs are copied in
  # step 7), so the record travels with the build output.
  mkdir -p "$WORK"
  # shellcheck disable=SC2086
  arch -x86_64 "$X86_BREW" list --versions $BREW_DEPS pkg-config >"$WORK/brew-versions.txt" 2>/dev/null || true
  echo "recorded $WORK/brew-versions.txt"
else
  gap "x86-64 Homebrew at /usr/local" "install Homebrew under arch -x86_64 per https://brew.sh (not run from here)"
fi

echo "== bison (Wine's configure needs 3.0 or newer; Apple's /usr/bin/bison is 2.3)"
conftest=$(mktemp -t bison-conftest)
printf '%%define parse.error verbose\n%%%%\nempty: ;\n' >"$conftest"
bison_found=""
for candidate in ${X86_PREFIX:+$X86_PREFIX/opt/bison/bin/bison} "$(command -v bison || true)"; do
  [ -n "$candidate" ] && [ -x "$candidate" ] || continue
  if "$candidate" -o /dev/null "$conftest" 2>/dev/null; then
    bison_found=$candidate
    break
  fi
done
rm -f "$conftest"
if [ -n "$bison_found" ]; then
  ok "bison >= 3.0 at $bison_found ($("$bison_found" --version | head -1))"
else
  gap "bison >= 3.0 (keg-only; build/wine/build.sh puts <x86 prefix>/opt/bison/bin first on PATH)" "arch -x86_64 $X86_BREW install bison"
fi

echo "== host Homebrew (osslsigncode host tool)"
openssl_prefix=$(brew --prefix openssl@3 2>/dev/null || true)
if [ -n "$openssl_prefix" ] && [ -d "$openssl_prefix" ]; then
  ok "brew openssl@3 at $openssl_prefix"
else
  gap "brew openssl@3 (host Homebrew)" "brew install openssl@3"
fi

echo "== summary"
if [ "$missing" -gt 0 ]; then
  echo "$missing prerequisite(s) missing"
else
  echo "all prerequisites present"
fi

if [ "$MODE" = check ]; then
  if [ "$missing" -eq 0 ]; then exit 0; else exit 1; fi
fi

echo "== install"
rustup target list --installed | grep -qx x86_64-apple-darwin || rustup target add x86_64-apple-darwin
if [ -z "$(llvm_mingw_bin || true)" ]; then
  archive=$(fetch_pin llvm-mingw)
  mkdir -p "$TOOLCHAIN"
  tar -xf "$archive" -C "$TOOLCHAIN"
  echo "extracted llvm-mingw into $TOOLCHAIN"
fi
if [ "$INSTALL_BREW_DEPS" -eq 1 ]; then
  [ -x "$X86_BREW" ] || { echo "x86-64 Homebrew is missing at /usr/local; install it per https://brew.sh under arch -x86_64, then rerun" >&2; exit 1; }
  # shellcheck disable=SC2086
  arch -x86_64 "$X86_BREW" install $BREW_DEPS
elif [ ! -x "$X86_BREW" ]; then
  echo "x86-64 Homebrew is still missing; install it per https://brew.sh under arch -x86_64, then rerun with --install-brew-deps"
fi
echo "rerun: sh build/bootstrap.sh --check"
