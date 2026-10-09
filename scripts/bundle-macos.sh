#!/bin/bash
# Create a macOS .app bundle for Keet
set -euo pipefail

# The version from the git tag, as build.rs embeds it (v1.21.0 -> 1.21.0);
# Cargo.toml's otherwise. It used to be a fixed 0.1.0 in every bundle.
VERSION="$(git describe --tags --abbrev=0 2>/dev/null || true)"
VERSION="${VERSION#v}"
if [ -z "$VERSION" ]; then
    VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
fi

APP="Keet.app"
CONTENTS="$APP/Contents"
MACOS="$CONTENTS/MacOS"
RESOURCES="$CONTENTS/Resources"

# Build release binary
cargo build --release

# Create bundle structure
rm -rf "$APP"
mkdir -p "$MACOS" "$RESOURCES"

# Copy binary and icon
cp target/release/keet "$MACOS/keet"
cp assets/icon.icns "$RESOURCES/keet.icns"

# Create Info.plist
cat > "$CONTENTS/Info.plist" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>Keet</string>
    <key>CFBundleDisplayName</key>
    <string>Keet</string>
    <key>CFBundleIdentifier</key>
    <string>com.keet.audio-player</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleExecutable</key>
    <string>keet</string>
    <key>CFBundleIconFile</key>
    <string>keet</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSMinimumSystemVersion</key>
    <string>10.15</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
EOF

echo "Created $APP ($VERSION)"
