#!/bin/bash
# Create a macOS .app bundle for Keet
set -e

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
cat > "$CONTENTS/Info.plist" << 'EOF'
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
    <string>0.1.0</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
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
    <key>NSMicrophoneUsageDescription</key>
    <string>Keet needs audio device access for playback.</string>
</dict>
</plist>
EOF

echo "Created $APP"
