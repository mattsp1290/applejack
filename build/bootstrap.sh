#!/bin/sh
# Prerequisite check and installer for the Rosetta engine payload build.
#   bootstrap.sh --check              report every missing prerequisite, install nothing (exit 1 if any)
#   bootstrap.sh                      install the Rust target and llvm-mingw under build/work/toolchain/
#   bootstrap.sh --install-homebrew   also install x86-64 Homebrew under /usr/local and the Wine deps
set -eu
. "$(dirname "$0")/lib.sh"

MODE=install
INSTALL_HOMEBREW=0
for arg in "$@"; do
  case "$arg" in
    --check) MODE=check ;;
    --install-homebrew) INSTALL_HOMEBREW=1 ;;
    *) echo "usage: $0 [--check] [--install-homebrew]" >&2; exit 2 ;;
  esac
done

X86_BREW=/usr/local/bin/brew
BREW_DEPS="freetype gnutls sdl2 molten-vk bison flex pkgconf"
# Homebrew renamed pkg-config to pkgconf; either satisfies Wine's configure.
alt_for() { case "$1" in pkgconf) echo pkg-config ;; *) echo "$1" ;; esac; }
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
for tool in clang otool install_name_tool codesign bison flex curl shasum tar pkgutil; do
  if command -v "$tool" >/dev/null 2>&1; then ok "$tool"; else gap "$tool" "install Xcode or the command line tools"; fi
done

echo "== Rust"
if rustup target list --installed 2>/dev/null | grep -qx x86_64-apple-darwin; then
  ok "rustup target x86_64-apple-darwin"
else
  gap "rustup target x86_64-apple-darwin (cpuid probe)" "rustup target add x86_64-apple-darwin"
fi

echo "== llvm-mingw (PE side of Wine and the probes)"
LLVM_MINGW_VERSION=$(pin_field llvm-mingw version || echo unknown)
bin=$(llvm_mingw_bin || true)
if [ -n "$bin" ] && [ -x "$bin/x86_64-w64-mingw32-clang" ] && [ -x "$bin/i686-w64-mingw32-clang" ]; then
  ok "llvm-mingw at $bin"
else
  gap "llvm-mingw $LLVM_MINGW_VERSION under $TOOLCHAIN/" "sh build/bootstrap.sh (extracts the llvm-mingw pin)"
fi
for cc in x86_64-w64-mingw32-gcc i686-w64-mingw32-gcc; do
  if command -v "$cc" >/dev/null 2>&1; then ok "$cc on PATH (Homebrew mingw-w64, fallback only)"; else echo "info:    $cc not on PATH (optional fallback)"; fi
done

echo "== x86-64 Homebrew (Wine build dependencies)"
if [ -x "$X86_BREW" ] && arch -x86_64 "$X86_BREW" --prefix >/dev/null 2>&1; then
  ok "x86-64 Homebrew at $(arch -x86_64 "$X86_BREW" --prefix)"
  for dep in $BREW_DEPS; do
    if arch -x86_64 "$X86_BREW" list --versions "$dep" >/dev/null 2>&1 \
      || arch -x86_64 "$X86_BREW" list --versions "$(alt_for "$dep")" >/dev/null 2>&1; then
      ok "brew (x86-64) $dep"
    else
      gap "brew (x86-64) $dep" "arch -x86_64 $X86_BREW install $dep"
    fi
  done
else
  gap "x86-64 Homebrew at /usr/local" "sh build/bootstrap.sh --install-homebrew"
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
if [ "$INSTALL_HOMEBREW" -eq 1 ]; then
  if [ ! -x "$X86_BREW" ]; then
    echo "installing x86-64 Homebrew under /usr/local (interactive; see https://brew.sh)"
    arch -x86_64 /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
  fi
  # shellcheck disable=SC2086
  arch -x86_64 "$X86_BREW" install $BREW_DEPS
elif [ ! -x "$X86_BREW" ]; then
  echo "x86-64 Homebrew is still missing; rerun with --install-homebrew to install it"
fi
echo "rerun: sh build/bootstrap.sh --check"
