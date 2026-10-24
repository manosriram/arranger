#!/bin/sh
# Wraps built binaries in arranger.app (macOS), so it opens from Finder, Spotlight and the Dock.
#   scripts/bundle.sh <dir with arranger and arranger-gui> <out dir> [version]
set -eu
bin=$1 out=$2 version=${3:-0.0.0}
here=$(cd "$(dirname "$0")/.." && pwd)
app=$out/arranger.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin/arranger" "$bin/arranger-gui" "$app/Contents/MacOS/"

# The icon at every size macOS asks for.
set=$(mktemp -d)/arranger.iconset
mkdir -p "$set"
for s in 16 32 128 256 512; do
  sips -z $s $s "$here/assets/icon.png" --out "$set/icon_${s}x${s}.png" >/dev/null
  sips -z $((s * 2)) $((s * 2)) "$here/assets/icon.png" --out "$set/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$set" -o "$app/Contents/Resources/arranger.icns"
rm -rf "$(dirname "$set")"

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>arranger</string>
  <key>CFBundleDisplayName</key><string>arranger</string>
  <key>CFBundleIdentifier</key><string>dev.arranger.app</string>
  <key>CFBundleExecutable</key><string>arranger-gui</string>
  <key>CFBundleIconFile</key><string>arranger</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${version#v}</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF
echo "built $app"
