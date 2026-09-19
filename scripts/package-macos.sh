#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
[ "$(uname -s)" = Darwin ] || { echo 'This packaging script requires macOS.' >&2; exit 1; }
TARGET="${1:-x86_64-apple-darwin}"
export MACOSX_DEPLOYMENT_TARGET=13.0
cargo build --locked --release --target "$TARGET" -p glide-cli -p glide-gui
DEST="$ROOT/dist/$TARGET"
APP="$DEST/Glide.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "target/$TARGET/release/glide-gui" "$APP/Contents/MacOS/glide-gui"
cp "target/$TARGET/release/glide" "$APP/Contents/MacOS/glide"
cp README.md LICENSE COPYING-GPL-3.0.txt "$APP/Contents/Resources/"
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>org.interchained.glide</string>
<key>CFBundleName</key><string>Glide</string>
<key>CFBundleDisplayName</key><string>Glide</string>
<key>CFBundleExecutable</key><string>glide-gui</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>0.1.0</string>
<key>CFBundleVersion</key><string>1</string>
<key>LSMinimumSystemVersion</key><string>13.0</string>
<key>NSHighResolutionCapable</key><true/>
<key>NSAppleEventsUsageDescription</key><string>Glide opens the selected VM display, ISO picker and serial console in Terminal.</string>
</dict></plist>
PLIST
codesign --force --sign - "$APP/Contents/MacOS/glide"
codesign --force --sign - "$APP"
codesign --verify --deep --strict "$APP"
file "$APP/Contents/MacOS/glide-gui" "$APP/Contents/MacOS/glide"
"$APP/Contents/MacOS/glide" --help
"$APP/Contents/MacOS/glide-gui" --help
# Ad-hoc signed development build; NOT Developer ID signed or notarized.
ditto -c -k --sequesterRsrc --keepParent "$APP" "$DEST/Glide-$TARGET.zip"
shasum -a 256 "$DEST/Glide-$TARGET.zip" > "$DEST/SHA256SUMS"
echo "$DEST/Glide-$TARGET.zip"
