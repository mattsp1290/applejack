#!/bin/sh
# Fetches the upstream GStreamer macOS runtime package and stages the
# sub-packages winegstreamer needs under build/work/gstreamer/ without
# installing it, thinned to x86_64. Never the Gcenx asset.
#
# Sub-packages of gstreamer-1.0-<version>-universal.pkg and why each is in or out:
#   in   base-system-1.0, base-crypto  glib, cairo, pango, orc, openssl: the runtime every plugin links (LGPL, MIT, Apache)
#   in   osx-framework                 the framework's Versions/Current, Headers, Commands links
#   in   gstreamer-1.0-core            libgstreamer and the base plugins (coreelements, typefind, audioconvert, videoconvertscale)
#   in   gstreamer-1.0-system          osxaudio, osxvideo, applemedia (VideoToolbox H.264/AAC), opengl, vulkan
#   in   gstreamer-1.0-playback        playbin, decodebin, uridecodebin
#   in   gstreamer-1.0-codecs          upstream's unrestricted codec set: vorbis, theora, opus, flac, vpx, dav1d, isomp4, matroska, avi, ogg, wav, jpeg, png.
#                                      Also libgstopenh264 (BSD-2-Clause, built from source): Cisco's H.264 patent
#                                      coverage applies to Cisco's own binaries only, so this plugin carries the
#                                      H.264 patent exposure itself; VideoToolbox in -system already decodes H.264.
#   in   gstreamer-1.0-effects         audio and video filters from gst-plugins-good and -bad (LGPL)
#   in   gstreamer-1.0-net             souphttpsrc, rtp, rtsp, tcp, udp (LGPL)
#   in   gstreamer-1.0-encoding        encodebin (LGPL)
#   out  gstreamer-1.0-codecs-gpl             a52dec, dtsdec: GPL
#   out  gstreamer-1.0-codecs-gpl-restricted  x264, x265: GPL and patent-encumbered
#   out  gstreamer-1.0-codecs-restricted      asf, mpeg-ps/ts, realmedia, voaacenc, vvdec: upstream's patent-restricted set
#                                             (so no WMV/WMA/VC-1, MPEG-1/2, or MPEG-PS/TS; see docs/engines/payload-build.md)
#   out  gstreamer-1.0-net-restricted         librtmp: GPL
#   out  gstreamer-1.0-dvd-gpl                dvdread, resindvd: GPL
#   out  gstreamer-1.0-libav                  ffmpeg: duplicates the codec set with the same patent exposure
#   out  devtools, python, gtk, editing, analytics, visualizers, capture
#                                             tools, bindings, GTK4, GES, ML, visualizers, camerabin: nothing winegstreamer loads
# build/denylist.toml names the plugin dylibs of the excluded codec packages
# (scope = "payload"), so an allowlist mistake fails the stage scan below.
set -eu
. "$(dirname "$0")/../lib.sh"

ALLOW="base-system-1.0 base-crypto osx-framework gstreamer-1.0-core gstreamer-1.0-system gstreamer-1.0-playback gstreamer-1.0-codecs gstreamer-1.0-effects gstreamer-1.0-net gstreamer-1.0-encoding"

archive=$(fetch_pin gstreamer)
version=$(pin_field gstreamer version)
expanded="$WORK/gstreamer/expanded"
stage="$WORK/gstreamer/stage/GStreamer.framework"
rm -rf "$expanded" "$stage"
pkgutil --expand-full "$archive" "$expanded"
mkdir -p "$stage"
for component in "$expanded"/*.pkg; do
  name=$(basename "$component" "-$version-universal.pkg")
  case " $ALLOW " in
    *" $name "*) ;;
    *) echo "gstreamer: dropping $name"; rm -rf "$component" ;;
  esac
done
for name in $ALLOW; do
  component="$expanded/$name-$version-universal.pkg"
  [ -d "$component/Payload" ] || { echo "gstreamer: sub-package $name is missing from the pinned package" >&2; exit 1; }
  # Each sub-package declares where under /Library/Frameworks/GStreamer.framework
  # it installs; an empty match would stage the payload at the wrong depth.
  grep -q 'install-location="/Library/Frameworks/GStreamer.framework' "$component/PackageInfo" \
    || { echo "gstreamer: $name has no GStreamer.framework install-location in PackageInfo" >&2; exit 1; }
  location=$(sed -n 's/.*install-location="\/Library\/Frameworks\/GStreamer.framework\/\{0,1\}\([^"]*\)".*/\1/p' "$component/PackageInfo" | head -1)
  mkdir -p "$stage/$location"
  cp -R "$component/Payload/." "$stage/$location/"
  echo "gstreamer: staged $name"
done

# The payload runs x86_64 only; keep that slice so --check sees what the
# hardened runtime loads and the arm64 copy of every library is not shipped.
find "$stage" -type f | while IFS= read -r f; do
  if file "$f" | grep -q 'Mach-O universal binary'; then
    lipo "$f" -thin x86_64 -output "$f.x86_64"
    mv "$f.x86_64" "$f"
  fi
done

listing=$(cd "$WORK/gstreamer/stage" && find . \( -type f -o -type l \) | sed 's|^\./||')
printf '%s\n' "$listing" | check_listing
echo "gstreamer $version staged into $stage ($(du -sh "$stage" | cut -f1))"
echo "plugins: $(find "$stage/Versions/1.0/lib/gstreamer-1.0" -name 'libgst*.dylib' | wc -l | tr -d ' ')"
echo "libgstreamer: $(file "$stage/Versions/1.0/lib/libgstreamer-1.0.0.dylib" | sed "s|$stage/||")"
