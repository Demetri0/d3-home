#!/bin/sh
# Build d3home.app.
#
# A command line program does not normally need a bundle. This one does, for
# one reason: `osascript display notification` shows the icon of whichever
# application called it, and offers no way to choose. Run from a terminal,
# every notification carries Terminal's icon. Run from inside a bundle, it
# carries the bundle's -- so on macOS the bundle is not decoration, it is the
# only way d3home's notifications can look like d3home's.
#
# The binary inside stays a normal command line program: the bundle is for
# the daemon, and `brew install d3home` remains the way to get `d3home` onto
# a PATH.
set -eu

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/d3home/Cargo.toml | head -1)
BIN=${BIN:-target/release/d3home}
OUT=${OUT:-dist}
APP="$OUT/d3home.app"
ICONSET="$OUT/d3home.iconset"

[ -x "$BIN" ] || { echo "no binary at $BIN -- run: cargo build --release" >&2; exit 1; }
command -v iconutil >/dev/null || { echo "iconutil is missing: this runs on macOS" >&2; exit 1; }

rm -rf "$APP" "$ICONSET"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$ICONSET"

# The master is 512, so the 512@2x slot is left out rather than upscaled:
# iconutil is content with a partial set, and a blurry 1024 helps nobody.
for spec in 16:icon_16x16 32:icon_16x16@2x 32:icon_32x32 64:icon_32x32@2x \
            128:icon_128x128 256:icon_128x128@2x 256:icon_256x256 \
            512:icon_256x256@2x 512:icon_512x512; do
	size=${spec%%:*}
	name=${spec#*:}
	sips -z "$size" "$size" assets/icons/d3home.png --out "$ICONSET/$name.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/d3home.icns"
rm -rf "$ICONSET"

cp "$BIN" "$APP/Contents/MacOS/d3home"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key><string>d3home</string>
	<key>CFBundleDisplayName</key><string>d3home</string>
	<key>CFBundleIdentifier</key><string>io.github.demetri0.d3home</string>
	<key>CFBundleExecutable</key><string>d3home</string>
	<key>CFBundleIconFile</key><string>d3home</string>
	<key>CFBundlePackageType</key><string>APPL</string>
	<key>CFBundleShortVersionString</key><string>$VERSION</string>
	<key>CFBundleVersion</key><string>$VERSION</string>
	<key>LSMinimumSystemVersion</key><string>11.0</string>
	<!-- No Dock icon and no menu bar: this is a background watcher, not a
	     window somebody switches to. -->
	<key>LSUIElement</key><true/>
</dict>
</plist>
PLIST

echo "built $APP"
echo "the LaunchAgent in contrib/ should point at $APP/Contents/MacOS/d3home"
