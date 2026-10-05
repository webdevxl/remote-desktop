#!/usr/bin/env bash
# Builds target/release/LanKVMMicrophone.driver, the Audio Server plug-in that gives a Mac the
# "LanKVM Microphone" input (macos/AudioDriver), and checks it with macos/AudioDriver/driver-test.c,
# which loads it as coreaudiod would. scripts/bundle.sh puts it into LanKVM.app;
# scripts/install-audio-driver.sh (or This Mac → Microphone → Install in the app) installs it.
set -euo pipefail
cd "$(dirname "$0")/.."

# The driver's own version: LanKVM offers to update an installed driver only when this changes
# (installing asks for an administrator password and restarts the Mac's audio).
DRIVER_VERSION="1"
SRC="macos/AudioDriver"
OUT="target/release/LanKVMMicrophone.driver"
TEST_BIN="target/release/lankvm-audio-driver-test"
CFLAGS=(-std=c11 -O2 -Wall -Wextra -Werror -mmacosx-version-min=14.0 -arch arm64)
FRAMEWORKS=(-framework CoreAudio -framework CoreFoundation)

rm -rf "$OUT"
mkdir -p "$OUT/Contents/MacOS"
clang "${CFLAGS[@]}" -bundle -fvisibility=hidden "$SRC/LanKVMMicrophone.c" "${FRAMEWORKS[@]}" -o "$OUT/Contents/MacOS/LanKVMMicrophone"
sed -e "s/__VERSION__/$DRIVER_VERSION.0/" -e "s/__BUILD__/$DRIVER_VERSION/" "$SRC/Info.plist" > "$OUT/Contents/Info.plist"
plutil -lint -s "$OUT/Contents/Info.plist"
# The factory must be the one symbol the bundle exports.
nm -gU "$OUT/Contents/MacOS/LanKVMMicrophone" | grep -q " _LanKVMMicrophone_Create$" || { echo "error: factory not exported" >&2; exit 1; }
clang "${CFLAGS[@]}" "$SRC/driver-test.c" "${FRAMEWORKS[@]}" -o "$TEST_BIN"
"$TEST_BIN" "$OUT"
echo "Built $OUT (driver version $DRIVER_VERSION)"
