#!/usr/bin/env bash
# Latency and frame-rate bench on this one Mac, without the viewer filming itself: the host copy
# (scripts/e2e-control.sh host, record mode, launched hidden) streams a virtual display that no
# screen here shows, LanKVM Frame Source animates that display full screen, and the headless probe
# reads each frame's number off the decoded video. Nothing appears on this Mac's own screens but
# the virtual display itself (next to them), and the host copy's Dock icon and menu bar item.
#
#   scripts/latency-bench.sh [MODE [LABEL]]
#   python3 scripts/latency-report.py target/e2e/bench/LABEL...     # compare runs side by side
#
# MODE is what changes on the virtual display besides the frame number (frame-source --change):
# tiny, small (default), band, scroll or full. Results go to target/e2e/bench/LABEL (default
# MODE-<build dir>-<time>): probe.txt (per-second and summary lines), trace.jsonl (a line per
# update), frame-source.jsonl, host.log (the host copy's log of this run), host-stats.log,
# meta.txt; with VIEWER=1, viewer.log and viewer-stats.log instead of the probe's files.
#
# Env:
#   APP=target/release/LanKVM.app    the build the host (and viewer) copies run, e.g. target/baseline/LanKVM.app
#   PROBE=target/release/examples/probe   the probe build; it needs --barcode-log and --trace
#                                    (target/baseline/probe predates them)
#   DURATION=10                      seconds measured
#   SIZE=2560x1600@2x REFRESH=120    the virtual display (pixels; @2x: Retina); HZ=120: Frame Source's rate
#   VIEWER=1                         the GUI viewer copy ("LanKVM 2") instead of the probe: it shows the
#                                    virtual display in a window on this Mac's own screen (not captured)
#                                    and logs its stats every second. STATS=1 also shows its overlay,
#                                    MTL_HUD_ENABLED=1 Metal's HUD.
#   LANKVM_TILES, LANKVM_FULL_FRAME_AT, LANKVM_MOTION, LANKVM_ENCODER_PROPS (host),
#   LANKVM_KEEP_WARM_MS (viewer)     passed on.
#
# Needs `scripts/e2e-control.sh setup` once (identities and pairings), so nothing asks for a PIN.
# Stops what it started; never touches the LanKVM on the default port (47800).
set -euo pipefail
cd "$(dirname "$0")/.."

MODE="${1:-small}"
case "$MODE" in
tiny | small | band | scroll | full) ;;
*) echo "error: MODE is tiny, small, band, scroll or full" >&2; exit 2 ;;
esac
export APP="${APP:-target/release/LanKVM.app}"
export PROBE="${PROBE:-target/release/examples/probe}"
export E2E="${E2E:-$PWD/target/e2e}"
export HOST_PORT="${HOST_PORT:-47810}" VIEWER_PORT="${VIEWER_PORT:-47811}"
DURATION="${DURATION:-10}"
SIZE="${SIZE:-2560x1600@2x}"
REFRESH="${REFRESH:-120}"
HZ="${HZ:-120}"
VIEWER="${VIEWER:-0}"
LABEL="${2:-$MODE-$(basename "$(dirname "$APP")")-$(date +%H%M%S)}"
OUT="$E2E/bench/$LABEL"
FRAME_SOURCE=target/frame-source

[[ "$HOST_PORT" != 47800 && "$VIEWER_PORT" != 47800 ]] || { echo "error: 47800 belongs to your own LanKVM" >&2; exit 2; }
[[ -d "$APP" ]] || { echo "error: $APP not found (scripts/bundle.sh builds target/release/LanKVM.app)" >&2; exit 1; }
peer="$([[ "$VIEWER" == 1 ]] && echo e2e-viewer || echo e2e-probe)"
grep -q " $peer\$" "$E2E/host/trusted-viewers.txt" 2>/dev/null \
    || { echo "error: the host copy doesn't know $peer yet; run scripts/e2e-control.sh setup first" >&2; exit 1; }
if [[ "$VIEWER" != 1 && ! -x "$PROBE" ]]; then
    echo "error: $PROBE not found (cargo build --release -p lankvm-core --examples)" >&2
    exit 1
fi
if [[ ! "$FRAME_SOURCE" -nt scripts/frame-source.swift ]]; then
    swiftc -O scripts/frame-source.swift -o "$FRAME_SOURCE"
fi
rm -rf "$OUT"
mkdir -p "$OUT"

holder() { lsof -nP -t -iUDP:"$1" 2>/dev/null | head -1 || true; }

# Process $1's ordinary on-screen windows, one "x y width height" line each (global points).
# (CoreGraphics from JavaScript for Automation: no permission needed for window bounds.)
windows_of() {
    osascript -l JavaScript -e "ObjC.import('CoreGraphics');
        const all = ObjC.deepUnwrap(ObjC.castRefToObject(\$.CGWindowListCopyWindowInfo(\$.kCGWindowListOptionOnScreenOnly, 0))) || [];
        all.filter(w => w.kCGWindowOwnerPID == $1 && w.kCGWindowLayer == 0).map(w => w.kCGWindowBounds)
            .map(b => [b.X, b.Y, b.Width, b.Height].map(Math.round).join(' ')).join('\n')"
}

# Display $1's bounds, "x y width height" (global points).
display_bounds() {
    osascript -l JavaScript -e "ObjC.import('CoreGraphics'); const r = \$.CGDisplayBounds($1);
        [r.origin.x, r.origin.y, r.size.width, r.size.height].map(Math.round).join(' ')"
}

# The host copy is only a host: a window of it on a screen means something's off.
host_hidden() {
    local shown
    shown="$(windows_of "$host_pid")"
    [[ -z "$shown" ]] && return 0
    echo "error: the host copy shows windows ($shown)" >&2
    return 1
}

# Lines the file $1 got since it was $2 bytes long.
since() { tail -c +"$(($2 + 1))" "$1" 2>/dev/null || true; }

size_of() { stat -f %z "$1" 2>/dev/null || echo 0; }

host_pid="" probe_pid="" source_pid="" viewer_started=""
cleanup() {
    local pid viewer_pid=""
    if [[ -n "$viewer_started" ]]; then viewer_pid="$(holder "$VIEWER_PORT")"; fi
    for pid in "$source_pid" "$probe_pid" "$viewer_pid"; do
        if [[ -n "$pid" ]]; then kill "$pid" 2>/dev/null || true; fi
    done
    if [[ -n "$host_pid" ]] && kill "$host_pid" 2>/dev/null; then
        # The virtual display goes with the host copy.
        for _ in {1..50}; do kill -0 "$host_pid" 2>/dev/null || break; sleep 0.1; done
    fi
    source_pid="" probe_pid="" viewer_started="" host_pid=""
}
trap cleanup EXIT
trap 'exit 130' INT TERM

{
    echo "label=$LABEL"
    echo "mode=$MODE"
    echo "app=$APP"
    echo "app_built=$(stat -f %Sm -t %Y-%m-%dT%H:%M:%S "$APP/Contents/MacOS/LanKVM" 2>/dev/null || echo -)"
    echo "client=$([[ "$VIEWER" == 1 ]] && echo "viewer" || echo "probe $PROBE")"
    echo "size=$SIZE"
    echo "refresh=$REFRESH"
    echo "hz=$HZ"
    echo "duration=$DURATION"
    echo "started=$(date +%Y-%m-%dT%H:%M:%S)"
    echo "commit=$(git describe --always --dirty 2>/dev/null || echo -)"
    for var in LANKVM_TILES LANKVM_FULL_FRAME_AT LANKVM_MOTION LANKVM_ENCODER_PROPS LANKVM_KEEP_WARM_MS; do
        if [[ -n "${!var:-}" ]]; then echo "$var=${!var}"; fi
    done
} >"$OUT/meta.txt"

host_log="$E2E/host/lankvm.log"
host_from="$(size_of "$host_log")"
HIDDEN=1 LANKVM_LOG_STATS=1 scripts/e2e-control.sh host record >/dev/null
host_pid="$(holder "$HOST_PORT")"
[[ -n "$host_pid" ]] || { echo "error: the host copy isn't running" >&2; exit 1; }
sleep 0.5
host_hidden || exit 1

if [[ "$VIEWER" == 1 ]]; then
    viewer_log="$E2E/viewer/lankvm.log"
    viewer_from="$(size_of "$viewer_log")"
    viewer_started=1
    LANKVM_CONNECT_DISPLAY="$SIZE@$REFRESH" scripts/e2e-control.sh viewer >/dev/null
else
    scripts/e2e-control.sh probe --virtual "$SIZE" --refresh "$REFRESH" --arrange extend --seconds "$DURATION" \
        --barcode-log "$OUT/frame-source.jsonl" --trace "$OUT/trace.jsonl" >"$OUT/probe.txt" 2>"$OUT/probe.err" &
    probe_pid=$!
fi

# The virtual display, once the host made it and streams it.
display_id=""
for _ in {1..400}; do
    display_id="$(since "$host_log" "$host_from" | sed -n 's/.*virtual display created.* display=\([0-9][0-9]*\).*/\1/p' | tail -1)"
    if [[ -n "$display_id" ]]; then
        if [[ "$VIEWER" == 1 ]] || grep -q "^display: virtual" "$OUT/probe.txt"; then break; fi
    fi
    if [[ -n "$probe_pid" ]] && ! kill -0 "$probe_pid" 2>/dev/null; then
        echo "error: the probe stopped before the virtual display showed:" >&2
        cat "$OUT/probe.txt" >&2
        tail -3 "$OUT/probe.err" >&2
        exit 1
    fi
    sleep 0.1
done
[[ -n "$display_id" ]] || { echo "error: no virtual display after 40 s; see $host_log" >&2; exit 1; }
host_hidden || exit 1

# By its id: the LanKVM people use may have a virtual display of its own here.
"$FRAME_SOURCE" --screen "display:$display_id" --fullscreen --background --hz "$HZ" --change "$MODE" \
    --log "$OUT/frame-source.jsonl" --seconds "$((DURATION + 5))" >"$OUT/frame-source.err" 2>&1 &
source_pid=$!
on=""
for _ in {1..50}; do
    on="$(grep '"type":"window"' "$OUT/frame-source.jsonl" 2>/dev/null | tail -1 | sed -n 's/.*"display_id":\([0-9]*\).*/\1/p' || true)"
    [[ -n "$on" ]] && break
    kill -0 "$source_pid" 2>/dev/null || break
    sleep 0.1
done
[[ "$on" == "$display_id" ]] || { echo "error: Frame Source isn't on display $display_id: $(cat "$OUT/frame-source.err")" >&2; exit 1; }
echo "$LABEL: $MODE on display $display_id ($SIZE @ $REFRESH Hz), $DURATION s"

if [[ "$VIEWER" == 1 ]]; then
    # The viewer window must not be on the display it shows: it would film itself.
    read -r vx vy vw vh <<<"$(display_bounds "$display_id")"
    viewer_pid="$(holder "$VIEWER_PORT")"
    [[ -n "$viewer_pid" ]] || { echo "error: the viewer copy isn't running" >&2; exit 1; }
    while read -r x y w h; do
        if [[ -n "$x" ]] && ((x < vx + vw && x + w > vx && y < vy + vh && y + h > vy)); then
            echo "warning: a viewer window is on the virtual display: the measurement films itself" >&2
        fi
    done <<<"$(windows_of "$viewer_pid")"
    sleep "$DURATION"
else
    deadline=$((SECONDS + DURATION + 60))
    while kill -0 "$probe_pid" 2>/dev/null; do
        ((SECONDS < deadline)) || { echo "error: the probe didn't finish" >&2; exit 1; }
        sleep 0.2
    done
    probe_status=0
    wait "$probe_pid" || probe_status=$?
    probe_pid=""
    [[ "$probe_status" == 0 ]] || echo "warning: the probe failed ($probe_status); see $OUT/probe.txt" >&2
fi
host_hidden || true

# Frame Source flushes its log on the way out.
kill "$source_pid" 2>/dev/null || true
wait "$source_pid" 2>/dev/null || true
cleanup

since "$host_log" "$host_from" >"$OUT/host.log"
grep "host stats" "$OUT/host.log" >"$OUT/host-stats.log" || true
if [[ "$VIEWER" == 1 ]]; then
    since "$viewer_log" "$viewer_from" >"$OUT/viewer.log"
    grep "viewer stats" "$OUT/viewer.log" >"$OUT/viewer-stats.log" || true
fi
python3 scripts/latency-report.py "$OUT"
