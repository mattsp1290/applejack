#!/bin/sh
# Copies the staged components into the plan 02 payload layout:
#   wine/                                   build/work/wine/stage (build/wine/build.sh), stripped; skipped with a message when not built
#   wine/lib/wine/x86_64-unix/winemetal.so  DXMT's native half
#   wine/lib/GStreamer.framework            the staged, x86_64-thinned framework
#   dxmt/{x86_64-windows,i386-windows}      DXMT DLLs
#   vkd3d/{x64,x86}                         vkd3d-proton DLLs
#   vulkan/                                 libMoltenVK.dylib (x86_64) and MoltenVK_icd.json
#   probes/                                 hello32.exe, dx11-sample.exe, xinput-check.exe, cpuid, osslsigncode (host-arch)
#   BUNDLE.toml, THIRD_PARTY_NOTICES.md     rendered from build/bundle-spec.toml
#   wine/lib/*.dylib                        the x86-64 Homebrew libraries Wine links or dlopens, copied in by
#                                           `cargo xtask engine-payload --close` (docs/engines/payload-build.md step 7),
#                                           which also rewrites install names and run paths and ends with --check
# The tree is built under <payload>.tmp, closed and checked there, and moved
# into place, so nothing an earlier run produced survives a rerun and a run
# that fails anywhere (the closure included) leaves the previous payload
# untouched. ASSEMBLE_PARTIAL=1 tolerates unbuilt probes and an unbuilt
# osslsigncode.
set -eu
. "$(dirname "$0")/lib.sh"

P=$(payload_dir)
T="$P.tmp"
# A run that died between the two mv's of the swap below left the last good
# tree as $P.old only; put it back before anything here can fail.
[ -e "$P" ] || [ ! -e "$P.old" ] || mv "$P.old" "$P"
dxmt="$WORK/dxmt/src/v$(pin_field dxmt version)"
vkd3d="$WORK/vkd3d/src/vkd3d-proton-$(pin_field vkd3d-proton version)"
vulkan="$WORK/vulkan/stage"
gstreamer="$WORK/gstreamer/stage/GStreamer.framework"
probes="$WORK/probes"
wine_stage="$WORK/wine/stage"

require() {
  [ -e "$1" ] || { echo "assemble: missing $1 (run $2)" >&2; exit 1; }
}
require "$dxmt/x86_64-windows" "sh build/dxmt/fetch.sh"
require "$dxmt/i386-windows" "sh build/dxmt/fetch.sh"
require "$dxmt/x86_64-unix/winemetal.so" "sh build/dxmt/fetch.sh"
require "$vkd3d/x64" "sh build/vkd3d/fetch.sh"
require "$vkd3d/x86" "sh build/vkd3d/fetch.sh"
require "$vulkan/libMoltenVK.dylib" "sh build/vulkan/fetch.sh"
require "$vulkan/MoltenVK_icd.json" "sh build/vulkan/fetch.sh"
require "$gstreamer/Versions/1.0/lib/libgstreamer-1.0.0.dylib" "sh build/gstreamer/fetch.sh"

rm -rf "$T"
mkdir -p "$T"
if [ -x "$wine_stage/bin/wine" ]; then
  cp -R "$wine_stage" "$T/wine"
  # `find -exec` would hide a strip failure; the loop's exit is the pipeline's.
  find "$T/wine" -type f \( -name '*.dylib' -o -name '*.so' -o -perm -u+x \) -print | while IFS= read -r f; do
    if file "$f" | grep -q Mach-O; then
      strip -S "$f" || { echo "assemble: strip failed on $f" >&2; exit 1; }
    fi
  done
  echo "assemble: wine/ from $wine_stage (stripped)"
else
  echo "assemble: no built wine at $wine_stage (sh build/wine/build.sh); wine/ skipped"
fi

mkdir -p "$T/dxmt/x86_64-windows" "$T/dxmt/i386-windows" "$T/vkd3d/x64" "$T/vkd3d/x86" "$T/vulkan" "$T/probes" "$T/wine/lib/wine/x86_64-unix"
cp "$dxmt"/x86_64-windows/*.dll "$T/dxmt/x86_64-windows/"
cp "$dxmt"/i386-windows/*.dll "$T/dxmt/i386-windows/"
cp "$dxmt/x86_64-unix/winemetal.so" "$T/wine/lib/wine/x86_64-unix/winemetal.so"
cp "$vkd3d"/x64/*.dll "$T/vkd3d/x64/"
cp "$vkd3d"/x86/*.dll "$T/vkd3d/x86/"
cp "$vulkan/libMoltenVK.dylib" "$vulkan/MoltenVK_icd.json" "$T/vulkan/"
cp -R "$gstreamer" "$T/wine/lib/GStreamer.framework"

# copy_probe <built file> <payload name> <how to build it>
copy_probe() {
  if [ -f "$1" ]; then
    cp "$1" "$T/probes/$2"
  elif [ "${ASSEMBLE_PARTIAL:-0}" = 1 ]; then
    echo "assemble: $1 not built; probes/$2 skipped (ASSEMBLE_PARTIAL=1)"
  else
    echo "assemble: missing $1 (run $3, or set ASSEMBLE_PARTIAL=1)" >&2
    exit 1
  fi
}
copy_probe "$probes/hello32.exe" hello32.exe "sh crates/applejack-engine/probes/hello32/build.sh"
copy_probe "$probes/dx11-sample-x86_64.exe" dx11-sample.exe "sh crates/applejack-engine/probes/dx11-sample/build.sh"
copy_probe "$probes/xinput-check-x86_64.exe" xinput-check.exe "sh crates/applejack-engine/probes/xinput-check/build.sh"
copy_probe "$probes/cpuid" cpuid "sh crates/applejack-engine/probes/cpuid/build.sh"
copy_probe "$WORK/osslsigncode/osslsigncode" osslsigncode "sh build/osslsigncode/build.sh"

xtask engine-payload --write-bundle-toml "$ROOT/build/bundle-spec.toml" "$T/BUNDLE.toml"

# Last, so a reassembly never loses it: the closure reads BUNDLE.toml for the
# library rows it records provenance against, and checks the closed tree.
if [ -x "$T/wine/bin/wine" ]; then
  xtask engine-payload --close "$T"
else
  echo "assemble: no wine/ in the tree; the dylib closure (xtask engine-payload --close) skipped"
fi

rm -rf "$P.old"
[ ! -e "$P" ] || mv "$P" "$P.old"
mv "$T" "$P"
rm -rf "$P.old"

echo "assembled $P ($(du -sh "$P" | cut -f1)):"
[ "${ASSEMBLE_PARTIAL:-0}" != 1 ] || echo "assemble: PARTIAL tree: BUNDLE.toml names files that are absent (wine/bin/wine, wine/bin/wineserver, probes), so the engine reader's validate rejects it; not installable"
(cd "$P" && find . -maxdepth 2 -not -path './wine/lib/GStreamer.framework/*' | sort | sed 's|^\./|  |')
