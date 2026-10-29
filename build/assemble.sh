#!/bin/sh
# Copies the staged components into the plan 02 payload layout:
#   wine/                                   build/work/wine/stage (build/wine/build.sh), stripped; skipped with a message when not built
#   wine/lib/wine/x86_64-unix/winemetal.so  DXMT's native half
#   wine/lib/GStreamer.framework            the staged, x86_64-thinned framework
#   dxmt/{x86_64-windows,i386-windows}      DXMT DLLs
#   vkd3d/{x64,x86}                         vkd3d-proton DLLs
#   vulkan/                                 libMoltenVK.dylib (x86_64) and MoltenVK_icd.json
#   probes/                                 hello32.exe, dx11-sample.exe, xinput-check.exe, cpuid
# The dylib closure and install-name rewrite (docs/engines/payload-build.md
# step 7) run after this script. ASSEMBLE_PARTIAL=1 tolerates missing probes.
set -eu
. "$(dirname "$0")/lib.sh"

P=$(payload_dir)
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

mkdir -p "$P"
if [ -x "$wine_stage/bin/wine" ]; then
  rm -rf "$P/wine"
  cp -R "$wine_stage" "$P/wine"
  find "$P/wine" -type f \( -name '*.dylib' -o -name '*.so' -o -perm -u+x \) -exec sh -c 'file "$1" | grep -q Mach-O && strip -S "$1"' _ {} \;
  echo "assemble: wine/ from $wine_stage (stripped)"
else
  echo "assemble: no built wine at $wine_stage (sh build/wine/build.sh); wine/ skipped"
fi

mkdir -p "$P/dxmt/x86_64-windows" "$P/dxmt/i386-windows" "$P/vkd3d/x64" "$P/vkd3d/x86" "$P/vulkan" "$P/probes" "$P/wine/lib/wine/x86_64-unix"
cp "$dxmt"/x86_64-windows/*.dll "$P/dxmt/x86_64-windows/"
cp "$dxmt"/i386-windows/*.dll "$P/dxmt/i386-windows/"
cp "$dxmt/x86_64-unix/winemetal.so" "$P/wine/lib/wine/x86_64-unix/winemetal.so"
cp "$vkd3d"/x64/*.dll "$P/vkd3d/x64/"
cp "$vkd3d"/x86/*.dll "$P/vkd3d/x86/"
cp "$vulkan/libMoltenVK.dylib" "$vulkan/MoltenVK_icd.json" "$P/vulkan/"
rm -rf "$P/wine/lib/GStreamer.framework"
cp -R "$gstreamer" "$P/wine/lib/GStreamer.framework"

# copy_probe <built name> <payload name>
copy_probe() {
  if [ -f "$probes/$1" ]; then
    cp "$probes/$1" "$P/probes/$2"
  elif [ "${ASSEMBLE_PARTIAL:-0}" = 1 ]; then
    echo "assemble: probe $probes/$1 not built; skipped (ASSEMBLE_PARTIAL=1)"
  else
    echo "assemble: missing probe $probes/$1 (run its build.sh under crates/applejack-engine/probes/, or set ASSEMBLE_PARTIAL=1)" >&2
    exit 1
  fi
}
copy_probe hello32.exe hello32.exe
copy_probe dx11-sample-x86_64.exe dx11-sample.exe
copy_probe xinput-check-x86_64.exe xinput-check.exe
copy_probe cpuid cpuid

echo "assembled $P ($(du -sh "$P" | cut -f1)):"
(cd "$P" && find . -maxdepth 2 -not -path './wine/lib/GStreamer.framework/*' | sort | sed 's|^\./|  |')
echo "next: the dylib closure (docs/engines/payload-build.md step 7), then cargo xtask engine-payload --check $P"
