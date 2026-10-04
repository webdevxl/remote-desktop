#!/usr/bin/env bash
# Source-to-glass bench of LanKVM's real viewer window on this one Mac. The host copy
# (scripts/e2e-control.sh host, record mode, launched hidden) makes a virtual display that LanKVM
# Frame Source animates full screen; the GUI viewer copy ("LanKVM 2") shows that display in a
# window on this Mac's own screen; LanKVM's screen scope (the app run with LANKVM_SCOPE_LOG, for
# its Screen Recording grant) films that window's area of the glass and reads each frame's number.
# Joined with Frame Source's log by frame number: source frame committed -> that frame on the
# glass, and how many distinct source frames a second reach the glass. Unlike latency-bench.sh's
# VIEWER=1 numbers (the viewer's own stats), this is measured from outside the viewer.
#
#   scripts/viewer-bench.sh [MODE [LABEL]]
#   python3 scripts/viewer-report.py target/e2e/bench/LABEL...     # compare runs side by side
#
# MODE is what changes on the virtual display besides the frame number (frame-source --change):
# tiny, small (default), band, scroll or full. Results go to target/e2e/bench/LABEL (default
# viewer-MODE-<time>): scope.jsonl, frame-source.jsonl, cpu.log (a line a second per process:
# time name pid %cpu rss_kb cputime), host.log, host-stats.log, viewer.log, viewer-stats.log,
# meta.txt.
#
# Env:
#   APP=target/release/LanKVM.app    the build the host and viewer copies run
#   SCOPE_APP=$APP                   the build the scope runs in: it must have the scope and
#                                    LanKVM's signature (that's where the Screen Recording grant is)
#   DURATION=10 WARMUP=3             seconds measured, once frames have flowed this long
#   SIZE=2560x1600@2x REFRESH=120    the virtual display (pixels; @2x: Retina); HZ=120 Frame Source's rate
#   SCOPE_FPS=240                    the scope's capture rate limit
#   STREAM_WAIT=60                   seconds the stream may take to start
#   LANKVM_TILES, LANKVM_FULL_FRAME_AT, LANKVM_MOTION, LANKVM_ENCODER_PROPS (host),
#   LANKVM_KEEP_WARM_MS (viewer)     passed on.
#
# On one Mac the viewer shares the GPU with the host's content: expect Frame Source to draw fewer
# frames than with the headless probe (latency-bench.sh), which caps the frames shown.
# Needs `scripts/e2e-control.sh setup` once (identities and pairings), so nothing asks for a PIN.
# Leave this Mac alone during a run: the scope films the viewer window (keep other windows and the
# pointer off it, and the pointer off the virtual display), and the screen must stay unlocked (the
# run keeps it awake). Stops everything it started; never touches the LanKVM on the default port
# (47800).
set -euo pipefail
cd "$(dirname "$0")/.."

MODE="${1:-small}"
case "$MODE" in
tiny | small | band | scroll | full) ;;
*) echo "usage: scripts/viewer-bench.sh [tiny|small|band|scroll|full [LABEL]]" >&2; exit 2 ;;
esac
export APP="${APP:-target/release/LanKVM.app}"
export E2E="${E2E:-$PWD/target/e2e}"
export HOST_PORT="${HOST_PORT:-47810}" VIEWER_PORT="${VIEWER_PORT:-47811}"
SCOPE_APP="${SCOPE_APP:-$APP}"
DURATION="${DURATION:-10}"
WARMUP="${WARMUP:-3}"
SIZE="${SIZE:-2560x1600@2x}"
REFRESH="${REFRESH:-120}"
HZ="${HZ:-120}"
SCOPE_FPS="${SCOPE_FPS:-240}"
STREAM_WAIT="${STREAM_WAIT:-60}"
LABEL="${2:-viewer-$MODE-$(date +%H%M%S)}"
OUT="$E2E/bench/$LABEL"
FRAME_SOURCE=target/frame-source
# How long the scope waits for the viewer window.
SCOPE_WAIT=10

for n in "$DURATION" "$WARMUP" "$REFRESH" "$HZ" "$SCOPE_FPS" "$STREAM_WAIT"; do
    [[ "$n" =~ ^[0-9]+$ ]] || { echo "error: DURATION, WARMUP, REFRESH, HZ, SCOPE_FPS and STREAM_WAIT are whole numbers" >&2; exit 2; }
done
[[ "$HOST_PORT" != 47800 && "$VIEWER_PORT" != 47800 ]] || { echo "error: 47800 belongs to your own LanKVM" >&2; exit 2; }
[[ -d "$APP" ]] || { echo "error: $APP not found (scripts/bundle.sh builds target/release/LanKVM.app)" >&2; exit 1; }
# A build without the scope would start as a whole LanKVM instead.
grep -aq LANKVM_SCOPE_LOG "$SCOPE_APP/Contents/MacOS/LanKVM" 2>/dev/null \
    || { echo "error: $SCOPE_APP has no screen scope (rebuild with scripts/bundle.sh, or set SCOPE_APP)" >&2; exit 1; }
grep -q " e2e-viewer\$" "$E2E/host/trusted-viewers.txt" 2>/dev/null \
    || { echo "error: the host copy doesn't know e2e-viewer yet; run scripts/e2e-control.sh setup first" >&2; exit 1; }
locked="$(osascript -l JavaScript -e "ObjC.import('CoreGraphics');
    (ObjC.deepUnwrap(ObjC.castRefToObject(\$.CGSessionCopyCurrentDictionary())) || {}).CGSSessionScreenIsLocked === true" 2>/dev/null || echo false)"
[[ "$locked" != true ]] || { echo "error: the screen is locked: windows don't draw, so there would be nothing to measure" >&2; exit 1; }
if [[ ! "$FRAME_SOURCE" -nt scripts/frame-source.swift ]]; then
    swiftc -O scripts/frame-source.swift -o "$FRAME_SOURCE"
fi
rm -rf "$OUT"
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"

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

# Whether pid $1 still runs a program whose name contains $2 (pids get reused).
runs() { [[ -n "$1" ]] && ps -p "$1" -o comm= 2>/dev/null | grep -qi -- "$2"; }

# Stops pid $1 if it still runs $2: SIGTERM, up to 3 s, then SIGKILL.
stop_pid() {
    runs "$1" "$2" || return 0
    kill "$1" 2>/dev/null || true
    for _ in {1..30}; do runs "$1" "$2" || return 0; sleep 0.1; done
    kill -9 "$1" 2>/dev/null || true
}

# The processes cpu.log follows, "name pid" a line (read by the sampler every second).
note_pid() { [[ -n "$2" ]] && echo "$1 $2" >>"$OUT/.pids" || true; }

host_pid="" viewer_pid="" source_pid="" scope_pid="" sampler_pid="" awake_pid="" viewer_started=""
cleanup() {
    local pid
    stop_pid "$scope_pid" LanKVM
    stop_pid "$source_pid" frame-source
    if [[ -n "$viewer_started" ]]; then
        viewer_pid="$(holder "$VIEWER_PORT")"
        stop_pid "$viewer_pid" LanKVM
    fi
    if [[ -n "$host_pid" ]] && kill "$host_pid" 2>/dev/null; then
        # The virtual display goes with the host copy.
        for _ in {1..50}; do kill -0 "$host_pid" 2>/dev/null || break; sleep 0.1; done
    fi
    for pid in "$sampler_pid" "$awake_pid"; do
        if [[ -n "$pid" ]]; then kill "$pid" 2>/dev/null || true; fi
    done
    host_pid="" viewer_pid="" source_pid="" scope_pid="" sampler_pid="" awake_pid="" viewer_started=""
}
trap cleanup EXIT
trap 'exit 130' INT TERM

{
    echo "label=$LABEL"
    echo "mode=$MODE"
    echo "app=$APP"
    echo "app_built=$(stat -f %Sm -t %Y-%m-%dT%H:%M:%S "$APP/Contents/MacOS/LanKVM" 2>/dev/null || echo -)"
    echo "scope_app=$SCOPE_APP"
    echo "client=viewer (LanKVM 2)"
    echo "size=$SIZE"
    echo "refresh=$REFRESH"
    echo "hz=$HZ"
    echo "duration=$DURATION"
    echo "warmup=$WARMUP"
    echo "scope_fps=$SCOPE_FPS"
    echo "started=$(date +%Y-%m-%dT%H:%M:%S)"
    echo "commit=$(git describe --always --dirty 2>/dev/null || echo -)"
    echo "power=$(pmset -g batt 2>/dev/null | sed -n "s/.*'\(.*\)'.*/\1/p" | head -1)"
    for var in LANKVM_TILES LANKVM_FULL_FRAME_AT LANKVM_MOTION LANKVM_ENCODER_PROPS LANKVM_KEEP_WARM_MS; do
        if [[ -n "${!var:-}" ]]; then echo "$var=${!var}"; fi
    done
} >"$OUT/meta.txt"

# Awake for the run: a locked or sleeping screen draws nothing.
caffeinate -dimsu -w $$ &
awake_pid=$!

# CPU and memory once a second. Cumulative CPU time too: %cpu is a decaying average.
: >"$OUT/.pids"
(
    while :; do
        now="$(perl -MTime::HiRes -e 'printf "%.3f", Time::HiRes::time')"
        while read -r name pid; do
            ps -o pid=,%cpu=,rss=,time= -p "$pid" 2>/dev/null | awk -v t="$now" -v n="$name" '{ print t, n, $1, $2, $3, $4 }' || true
        done <"$OUT/.pids"
        sleep 1
    done >>"$OUT/cpu.log" 2>/dev/null
) &
sampler_pid=$!

host_log="$E2E/host/lankvm.log"
host_from="$(size_of "$host_log")"
HIDDEN=1 LANKVM_LOG_STATS=1 scripts/e2e-control.sh host record >/dev/null
host_pid="$(holder "$HOST_PORT")"
[[ -n "$host_pid" ]] || { echo "error: the host copy isn't running" >&2; exit 1; }
note_pid host "$host_pid"
sleep 0.5
host_hidden || exit 1

viewer_log="$E2E/viewer/lankvm.log"
viewer_from="$(size_of "$viewer_log")"
viewer_started=1
LANKVM_CONNECT_DISPLAY="$SIZE@$REFRESH" scripts/e2e-control.sh viewer >/dev/null
viewer_pid="$(holder "$VIEWER_PORT")"
[[ -n "$viewer_pid" ]] || { echo "error: the viewer copy isn't running" >&2; exit 1; }
note_pid viewer "$viewer_pid"

# The virtual display, once the host made it and streams it.
display_id=""
for _ in {1..400}; do
    display_id="$(since "$host_log" "$host_from" | sed -n 's/.*virtual display created.* display=\([0-9][0-9]*\).*/\1/p' | tail -1)"
    [[ -n "$display_id" ]] && break
    sleep 0.1
done
[[ -n "$display_id" ]] || { echo "error: no virtual display after 40 s; see $host_log" >&2; exit 1; }
host_hidden || exit 1

# By its id: the LanKVM people use may have a virtual display of its own here.
"$FRAME_SOURCE" --screen "display:$display_id" --fullscreen --background --hz "$HZ" --change "$MODE" \
    --log "$OUT/frame-source.jsonl" --seconds "$((STREAM_WAIT + WARMUP + DURATION + SCOPE_WAIT + 30))" \
    >"$OUT/frame-source.err" 2>&1 &
source_pid=$!
note_pid frame-source "$source_pid"
on=""
for _ in {1..50}; do
    on="$(grep '"type":"window"' "$OUT/frame-source.jsonl" 2>/dev/null | tail -1 | sed -n 's/.*"display_id":\([0-9]*\).*/\1/p' || true)"
    [[ -n "$on" ]] && break
    kill -0 "$source_pid" 2>/dev/null || break
    sleep 0.1
done
[[ "$on" == "$display_id" ]] || { echo "error: Frame Source isn't on display $display_id: $(cat "$OUT/frame-source.err")" >&2; exit 1; }

# Frames flowing: the viewer's per-second stats show them.
flowing=""
deadline=$((SECONDS + STREAM_WAIT))
while ((SECONDS < deadline)); do
    if since "$viewer_log" "$viewer_from" | grep "viewer stats" | sed -n 's/.*"fps":\([0-9.]*\).*/\1/p' \
        | awk '$1 >= 5 { found = 1 } END { exit !found }'; then
        flowing=1
        break
    fi
    runs "$viewer_pid" LanKVM || { echo "error: the viewer copy quit; see $viewer_log" >&2; exit 1; }
    sleep 0.2
done
[[ -n "$flowing" ]] || { echo "error: no frames after $STREAM_WAIT s" >&2; exit 1; }
echo "$LABEL: $MODE on display $display_id ($SIZE @ $REFRESH Hz); measuring $DURATION s after $WARMUP s"
sleep "$WARMUP"

# The viewer window must not be on the display it shows: it would film itself.
read -r vx vy vw vh <<<"$(display_bounds "$display_id")"
while read -r x y w h; do
    if [[ -n "$x" ]] && ((x < vx + vw && x + w > vx && y < vy + vh && y + h > vy)); then
        echo "warning: a viewer window is on the virtual display: the measurement films itself" >&2
    fi
done <<<"$(windows_of "$viewer_pid")"

# The scope, through LaunchServices so LanKVM's Screen Recording grant applies. The port and data
# dir are only a safety net: in scope mode the app starts no core.
open -n -g -j "$SCOPE_APP" --env LANKVM_SCOPE_LOG="$OUT/scope.jsonl" --env LANKVM_SCOPE_PID="$viewer_pid" \
    --env LANKVM_SCOPE_SECONDS="$DURATION" --env LANKVM_SCOPE_FPS="$SCOPE_FPS" --env LANKVM_SCOPE_WAIT="$SCOPE_WAIT" \
    --env LANKVM_PORT=0 --env LANKVM_DATA_DIR="$OUT/scope-data" --env LANKVM_NO_PROMPTS=1 \
    --args -ApplePersistenceIgnoreState YES
for _ in {1..100}; do
    scope_pid="$(sed -n 's/.*"pid":\([0-9][0-9]*\).*"type":"start".*/\1/p' "$OUT/scope.jsonl" 2>/dev/null | head -1 || true)"
    [[ -n "$scope_pid" ]] && break
    sleep 0.1
done
[[ -n "$scope_pid" ]] || { echo "error: the scope didn't start ($SCOPE_APP)" >&2; exit 1; }
note_pid scope "$scope_pid"
deadline=$((SECONDS + DURATION + SCOPE_WAIT + 15))
viewer_alive=1
while ! grep -q '"type":"summary"' "$OUT/scope.jsonl"; do
    runs "$scope_pid" LanKVM || break
    ((SECONDS < deadline)) || { echo "error: the scope didn't finish" >&2; exit 1; }
    if [[ -n "$viewer_alive" ]] && ! runs "$viewer_pid" LanKVM; then
        echo "warning: the viewer quit during the measurement" >&2
        viewer_alive=""
    fi
    sleep 0.2
done
scope_pid=""
scope_error="$(sed -n 's/.*"message":"\(.*\)","type":"error".*/\1/p' "$OUT/scope.jsonl" 2>/dev/null | head -1 || true)"
[[ -z "$scope_error" ]] || echo "warning: the scope: $scope_error" >&2
host_hidden || true

# Frame Source flushes its log on the way out.
kill "$source_pid" 2>/dev/null || true
wait "$source_pid" 2>/dev/null || true
source_pid=""
cleanup

since "$host_log" "$host_from" >"$OUT/host.log"
grep "host stats" "$OUT/host.log" >"$OUT/host-stats.log" || true
since "$viewer_log" "$viewer_from" >"$OUT/viewer.log"
grep "viewer stats" "$OUT/viewer.log" >"$OUT/viewer-stats.log" || true
rm -f "$OUT/.pids"
python3 scripts/viewer-report.py "$OUT"
