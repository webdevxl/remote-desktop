#!/usr/bin/env bash
# Builds target/release/LanKVM.app: the Rust core (static library) linked into the SwiftUI app,
# then code-signs it.
#
# macOS ties privacy grants (Screen Recording, Accessibility) to the app's signature. With a
# stable signing identity the grants survive rebuilds; with ad-hoc signing ("-") you are
# re-prompted after every build.
#
# Identity: $LANKVM_SIGN_IDENTITY, else the first "Apple Development" or "lankvm-dev"
# code-signing certificate in your keychain, else ad-hoc.
set -euo pipefail
cd "$(dirname "$0")/.."

BUNDLE_ID="dev.lankvm.LanKVM"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
APP="target/release/LanKVM.app"
export MACOSX_DEPLOYMENT_TARGET=14.0

# Find Rust even in shells that haven't loaded it (Homebrew rustup or the rustup installer).
if ! command -v cargo >/dev/null; then
    export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"
fi
command -v cargo >/dev/null || { echo "error: cargo not found; install Rust (rustup) first." >&2; exit 1; }

cargo build --release -p lankvm-core
# SwiftPM doesn't track the Rust static library as an input, so it won't relink when only the
# Rust code changed. Removing the old executable forces the link step.
rm -f macos/.build/release/LanKVM
swift build -c release --package-path macos

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp macos/.build/release/LanKVM "$APP/Contents/MacOS/LanKVM"
if [[ -f macos/Resources/AppIcon.icns ]]; then
    cp macos/Resources/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
fi

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>${BUNDLE_ID}</string>
    <key>CFBundleName</key><string>LanKVM</string>
    <key>CFBundleDisplayName</key><string>LanKVM</string>
    <key>CFBundleExecutable</key><string>LanKVM</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>${VERSION}</string>
    <key>CFBundleVersion</key><string>${VERSION}</string>
    <key>LSMinimumSystemVersion</key><string>14.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSPrincipalClass</key><string>NSApplication</string>
    <key>NSLocalNetworkUsageDescription</key>
    <string>LanKVM connects to other Macs on your local network to show and control their screens.</string>
    <key>NSScreenCaptureUsageDescription</key>
    <string>LanKVM shares this Mac's screen with other Macs on your local network that you pair with.</string>
</dict>
</plist>
EOF

IDENTITY="${LANKVM_SIGN_IDENTITY:-}"
if [[ -z "$IDENTITY" ]]; then
    IDENTITY="$(security find-identity -p codesigning 2>/dev/null \
        | awk -F'"' '/Apple Development|lankvm-dev/ { print $2; exit }')"
fi
if [[ -n "$IDENTITY" ]]; then
    echo "Signing with: $IDENTITY"
    codesign --force --options runtime --identifier "$BUNDLE_ID" --sign "$IDENTITY" "$APP"
else
    echo "warning: no signing identity found; using ad-hoc signing."
    echo "         macOS will ask for Screen Recording permission again after every rebuild."
    codesign --force --identifier "$BUNDLE_ID" --sign - "$APP"
fi
codesign --verify "$APP"
echo "Built $APP"
