#!/usr/bin/env bash
#
# Build WING LiveTrax Bridge.app, and optionally a .dmg to hand someone.
#
#   packaging/bundle.sh            # build the .app into dist/
#   packaging/bundle.sh --dmg      # ...and wrap it in a disk image
#
# The app is signed ad-hoc, which is enough to run on the machine that built it.
# For anyone else's Mac you need a Developer ID: see the notes at the bottom.

set -euo pipefail
cd "$(dirname "$0")/.."

APP_NAME="WING LiveTrax Bridge"
BUNDLE_ID="com.sulfurdesign.wing-livetrax-bridge"
BINARY="wing-livetrax-bridge"
DIST="dist"
APP="$DIST/$APP_NAME.app"
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)

echo "==> building $BINARY $VERSION"
cargo build --release

echo "==> drawing the icon"
python3 packaging/make_icon.py >/dev/null

echo "==> assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "target/release/$BINARY" "$APP/Contents/MacOS/$BINARY"
cp packaging/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
cp config.example.toml "$APP/Contents/Resources/config.example.toml"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$APP_NAME</string>
  <key>CFBundleDisplayName</key><string>$APP_NAME</string>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleExecutable</key><string>$BINARY</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.music</string>
  <key>NSHighResolutionCapable</key><true/>
  <!-- The console lives on the LAN, so recent macOS asks the user first. -->
  <key>NSLocalNetworkUsageDescription</key>
  <string>WING LiveTrax Bridge talks to your WING console and to LiveTrax over your local network.</string>
</dict>
</plist>
PLIST

echo "==> signing (ad-hoc)"
codesign --force --sign - --timestamp=none "$APP" >/dev/null
codesign --verify --strict "$APP" && echo "    signature ok"

if [[ "${1:-}" == "--dmg" ]]; then
  DMG="$DIST/$APP_NAME $VERSION.dmg"
  echo "==> building $DMG"
  STAGE=$(mktemp -d)
  cp -R "$APP" "$STAGE/"
  ln -s /Applications "$STAGE/Applications"
  rm -f "$DMG"
  hdiutil create -volname "$APP_NAME" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
  rm -rf "$STAGE"
  echo "    $(du -h "$DMG" | cut -f1)  $DMG"
fi

echo
echo "Done: $APP"
echo "First launch writes a config to ~/Library/Application Support/$APP_NAME/config.toml"
echo
echo "To share it with another Mac, sign and notarise with a Developer ID:"
echo "  codesign --force --deep --options runtime --sign \"Developer ID Application: NAME (TEAMID)\" \"$APP\""
echo "  xcrun notarytool submit \"<dmg>\" --apple-id ... --team-id ... --password ... --wait"
echo "  xcrun stapler staple \"<dmg>\""
