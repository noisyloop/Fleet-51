#!/usr/bin/env bash
# Package a built fleet51 binary as Fleet-51.app inside a .dmg. Runs on macOS (needs hdiutil).
#   packaging/macos/build-dmg.sh <version> <path/to/fleet51> <output.dmg>
set -euo pipefail

VERSION="$1"
BIN="$2"
OUT="$3"
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
STAGE="$(mktemp -d)"
APP="$STAGE/dmg/Fleet-51.app"

mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/Resources/fleet51"
# The native window (Swift + WebKit), built for Apple Silicon and Intel.
for arch in arm64 x86_64; do
  swiftc -O -target "$arch-apple-macos11" "$HERE/app/main.swift" -o "$STAGE/Fleet-51-$arch"
done
lipo -create -output "$APP/Contents/MacOS/Fleet-51" "$STAGE/Fleet-51-arm64" "$STAGE/Fleet-51-x86_64"
chmod +x "$APP/Contents/MacOS/Fleet-51" "$APP/Contents/Resources/fleet51"
sed "s/VERSION/$VERSION/g" "$HERE/Info.plist" > "$APP/Contents/Info.plist"

# App icon from the web UI's SVG. Cosmetic, so a failure here does not stop the build.
(
  cd "$STAGE"
  qlmanage -t -s 1024 -o . "$ROOT/ui/icon.svg" >/dev/null 2>&1
  mkdir icon.iconset
  for s in 16 32 128 256 512; do
    sips -z $s $s icon.svg.png --out "icon.iconset/icon_${s}x${s}.png" >/dev/null
    sips -z $((s * 2)) $((s * 2)) icon.svg.png --out "icon.iconset/icon_${s}x${s}@2x.png" >/dev/null
  done
  iconutil -c icns -o "$APP/Contents/Resources/icon.icns" icon.iconset
) || echo "warning: app icon not generated"

cp "$HERE/README.txt" "$STAGE/dmg/README.txt"
ln -s /Applications "$STAGE/dmg/Applications"

# Ad-hoc signature: not notarized, but keeps the bundle's code signature consistent.
codesign --force --deep --sign - "$APP"

hdiutil create -volname "Fleet-51" -srcfolder "$STAGE/dmg" -ov -format UDZO "$OUT"
rm -rf "$STAGE"
echo "built $OUT"
