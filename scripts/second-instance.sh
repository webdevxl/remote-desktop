#!/usr/bin/env bash
# Launches a second LanKVM on this Mac that acts as a separate device, to test a real
# host <-> viewer connection without a second Mac.
#
# It runs a copy of the app ("LanKVM 2", bundle id dev.lankvm.LanKVM.second) with its own UDP
# port, certificate, trust lists, log and privacy grants, so the two instances are as separate
# as two Macs: they show up as two apps in the Dock and macOS tracks their permissions apart.
#
#   ./scripts/second-instance.sh            # start it (the normal instance keeps port 47800)
#   ./scripts/second-instance.sh --restart  # quit the running second instance first
#
# Then, in LanKVM 2: Connect -> 127.0.0.1:47800 (views the first instance). To view LanKVM 2
# from the first instance instead, connect to 127.0.0.1:$PORT; LanKVM 2 then needs its own
# Screen Recording permission.
#
# Env: LANKVM_APP (default target/release/LanKVM.app), PORT (default 47801),
#      DATA_DIR (default ~/Library/Application Support/lankvm-2; pairings persist here).
set -euo pipefail
cd "$(dirname "$0")/.."

SRC="${LANKVM_APP:-target/release/LanKVM.app}"
APP="$(dirname "$SRC")/LanKVM-2.app"
BUNDLE_ID="dev.lankvm.LanKVM.second"
PORT="${PORT:-47801}"
DATA_DIR="${DATA_DIR:-$HOME/Library/Application Support/lankvm-2}"

[[ -d "$SRC" ]] || { echo "error: $SRC not found; run ./scripts/bundle.sh first." >&2; exit 1; }

holder() { lsof -nP -t -iUDP:"$PORT" 2>/dev/null | head -1; }

if [[ -n "$(holder)" ]]; then
    if [[ "${1:-}" != "--restart" ]]; then
        echo "UDP port $PORT is already in use by pid $(holder). Use --restart to replace it." >&2
        exit 1
    fi
    kill "$(holder)"
    for _ in {1..50}; do [[ -z "$(holder)" ]] && break; sleep 0.1; done
fi

# Fresh copy of the current build with its own bundle id. Ad-hoc signed by default: as a viewer
# it needs no privacy grants, and it avoids a keychain prompt on every run. Set
# LANKVM_SIGN_IDENTITY to sign it properly, e.g. so a Screen Recording grant survives rebuilds.
rm -rf "$APP"
cp -R "$SRC" "$APP"
plutil -replace CFBundleIdentifier -string "$BUNDLE_ID" "$APP/Contents/Info.plist"
plutil -replace CFBundleName -string "LanKVM 2" "$APP/Contents/Info.plist"
plutil -replace CFBundleDisplayName -string "LanKVM 2" "$APP/Contents/Info.plist"
if [[ -n "${LANKVM_SIGN_IDENTITY:-}" ]]; then
    codesign --force --options runtime --identifier "$BUNDLE_ID" --sign "$LANKVM_SIGN_IDENTITY" "$APP"
else
    codesign --force --identifier "$BUNDLE_ID" --sign - "$APP"
fi

mkdir -p "$DATA_DIR"
# Pass through settings for test runs (e.g. LANKVM_NO_PROMPTS=1 from scripts/e2e-control.sh).
extra=()
for var in LANKVM_NO_PROMPTS LANKVM_INJECT LANKVM_CONNECT LANKVM_LOG_STATS; do
    [[ -n "${!var:-}" ]] && extra+=(--env "$var=${!var}")
done
open -n "$APP" --env LANKVM_PORT="$PORT" --env LANKVM_DATA_DIR="$DATA_DIR" ${extra[@]+"${extra[@]}"} \
    --args -ApplePersistenceIgnoreState YES

for _ in {1..50}; do [[ -n "$(holder)" ]] && break; sleep 0.1; done
if [[ -z "$(holder)" ]]; then
    echo "error: LanKVM 2 didn't start listening; see $DATA_DIR/lankvm.log" >&2
    exit 1
fi
echo "LanKVM 2: pid $(holder), UDP $PORT"
echo "  data: $DATA_DIR"
echo "  log:  $DATA_DIR/lankvm.log"
echo "To view the first instance from it: Connect -> 127.0.0.1:47800"
