#!/usr/bin/env bash
# Capture-only bench on this one Mac: how soon each capture API delivers a frame of a virtual
# display, and how many distinct frames a second. ScreenCaptureKit is what LanKVM's host uses;
# AVCaptureScreenInput, set up as Sunshine sets it up on macOS, is what Sunshine uses. Both read
# Frame Source's frame number straight off the display, in turns, so nothing encodes or streams.
# The host copy holds the virtual display with the Sunshine engine chosen but no Moonlight
# connected: LanKVM's own stream is stopped and Sunshine sits idle.
#
#   scripts/capture-bench.sh [LABEL]
#
# Results go to target/e2e/bench/LABEL (default capture-<time>): frame-source.jsonl, one
# scope-<sck|av>-<rep>.jsonl per turn, and the table printed at the end (commit -> captured, per
# distinct frame: "stamp" by the capture's own timestamp, "arrival" when the callback ran).
#
# Env: REPS=2 turns per API, SECS=8 seconds per turn, MODE=small (frame-source --change),
#      SIZE=2560x1600@2x, REFRESH=120, APP=target/release/LanKVM.app (it has the scope and LanKVM's
#      Screen Recording grant), SUNSHINE_PORT=48810.
# Needs `scripts/e2e-control.sh setup` once and Sunshine installed. Never touches the LanKVM on
# the default port (47800).
set -euo pipefail
cd "$(dirname "$0")/.."

export E2E="${E2E:-$PWD/target/e2e}" HOST_PORT="${HOST_PORT:-47810}"
APP="${APP:-target/release/LanKVM.app}"
REPS="${REPS:-2}"
SECS="${SECS:-8}"
MODE="${MODE:-small}"
SIZE="${SIZE:-2560x1600@2x}"
REFRESH="${REFRESH:-120}"
SUNSHINE_PORT="${SUNSHINE_PORT:-48810}"
LABEL="${1:-capture-$(date +%H%M%S)}"
OUT="$E2E/bench/$LABEL"
FRAME_SOURCE=target/frame-source

[[ "$HOST_PORT" != 47800 ]] || { echo "error: 47800 belongs to your own LanKVM" >&2; exit 2; }
[[ -d "$APP" ]] || { echo "error: $APP not found (scripts/bundle.sh builds it)" >&2; exit 1; }
grep -q " e2e-probe\$" "$E2E/host/trusted-viewers.txt" 2>/dev/null \
    || { echo "error: run scripts/e2e-control.sh setup first" >&2; exit 1; }
if [[ ! "$FRAME_SOURCE" -nt scripts/frame-source.swift ]]; then
    swiftc -O scripts/frame-source.swift -o "$FRAME_SOURCE"
fi
rm -rf "$OUT"
mkdir -p "$OUT"
# The scope runs from LaunchServices (cwd /): it needs absolute paths.
OUT="$(cd "$OUT" && pwd)"

holder() { lsof -nP -t -iUDP:"$1" 2>/dev/null | head -1 || true; }
size_of() { stat -f %z "$1" 2>/dev/null || echo 0; }

source_pid="" probe_pid=""
cleanup() {
    local pid
    for pid in "$source_pid" "$probe_pid"; do
        if [[ -n "$pid" ]]; then kill "$pid" 2>/dev/null || true; fi
    done
    if [[ -n "$(holder "$HOST_PORT")" ]]; then scripts/e2e-control.sh stop >/dev/null 2>&1 || true; fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

host_log="$E2E/host/lankvm.log"
host_from="$(size_of "$host_log")"
HIDDEN=1 LANKVM_SUNSHINE_PORT="$SUNSHINE_PORT" LANKVM_SUNSHINE_INPUT=0 scripts/e2e-control.sh host record >/dev/null
total=$((REPS * 2 * (SECS + 6) + 20))
scripts/e2e-control.sh probe --virtual "$SIZE" --refresh "$REFRESH" --arrange extend --engine sunshine --seconds "$total" \
    >"$OUT/probe.txt" 2>&1 &
probe_pid=$!
display_id=""
for _ in {1..300}; do
    if grep -q "^engine: sunshine" "$OUT/probe.txt" 2>/dev/null; then
        display_id="$(tail -c +"$((host_from + 1))" "$host_log" | sed -n 's/.*virtual display created.* display=\([0-9][0-9]*\).*/\1/p' | tail -1)"
        [[ -n "$display_id" ]] && break
    fi
    kill -0 "$probe_pid" 2>/dev/null || { echo "error: the probe stopped:" >&2; cat "$OUT/probe.txt" >&2; exit 1; }
    sleep 0.2
done
[[ -n "$display_id" ]] || { echo "error: no virtual display with the Sunshine engine; see $OUT/probe.txt" >&2; exit 1; }
caffeinate -dimsu -w $$ &

"$FRAME_SOURCE" --screen "display:$display_id" --fullscreen --background --hz "$REFRESH" --change "$MODE" \
    --log "$OUT/frame-source.jsonl" --seconds "$total" >"$OUT/frame-source.err" 2>&1 &
source_pid=$!
sleep 2
echo "$LABEL: capture of display $display_id ($SIZE @ $REFRESH Hz, $MODE), $REPS × $SECS s per API"
for rep in $(seq 1 "$REPS"); do
    for api in sck av; do
        log="$OUT/scope-$api-$rep.jsonl"
        if [[ "$api" == sck ]]; then
            # Frame Source's full-screen window: ScreenCaptureKit captures that region of the display.
            open -n -g -j "$APP" --env LANKVM_SCOPE_LOG="$log" --env LANKVM_SCOPE_PID="$source_pid" \
                --env LANKVM_SCOPE_SECONDS="$SECS" --env LANKVM_SCOPE_FPS=240
        else
            open -n -g -j "$APP" --env LANKVM_SCOPE_LOG="$log" --env LANKVM_SCOPE_AVCAPTURE="$display_id" \
                --env LANKVM_SCOPE_SECONDS="$SECS" --env LANKVM_SCOPE_FPS="$REFRESH"
        fi
        for _ in $(seq 1 $(((SECS + 15) * 5))); do
            grep -q '"summary"' "$log" 2>/dev/null && break
            sleep 0.2
        done
        grep -q '"summary"' "$log" 2>/dev/null || echo "warning: the $api scope wrote no summary ($log)" >&2
    done
done
kill "$source_pid" 2>/dev/null || true
wait "$source_pid" 2>/dev/null || true
source_pid=""

python3 - "$OUT" <<'EOF'
import json, sys
from pathlib import Path

def lines(path):
    out = []
    for line in open(path):
        try:
            out.append(json.loads(line))
        except ValueError:
            pass  # cut short when the writer stopped
    return out

def quantile(values, q):
    s = sorted(values)
    return s[min(len(s) - 1, int((len(s) - 1) * q + 0.5))] if s else float("nan")

run = Path(sys.argv[1])
commit = {f["n"] & 0xFFFF: f["commit_us"] for f in lines(run / "frame-source.jsonl") if f.get("type") == "frame"}
print(f"{'turn':10} {'distinct':>8} {'fps':>6}   {'stamp p50/95 ms':>15}   {'arrival p50/95 ms':>17}")
for path in sorted(run.glob("scope-*.jsonl")):
    first = {}
    for f in lines(path):
        if f.get("type") == "frame":
            first.setdefault(f["n"], f)
    stamp = [(f["display_us"] - commit[n]) / 1000 for n, f in first.items() if n in commit and f["display_us"]]
    arrival = [(f["arrival_us"] - commit[n]) / 1000 for n, f in first.items() if n in commit]
    times = sorted(f["arrival_us"] for f in first.values())
    fps = (len(times) - 1) / ((times[-1] - times[0]) / 1e6) if len(times) > 1 else 0
    name = path.stem.removeprefix("scope-")
    print(f"{name:10} {len(first):8} {fps:6.1f}   {quantile(stamp, .5):7.1f} / {quantile(stamp, .95):5.1f}   "
          f"{quantile(arrival, .5):9.1f} / {quantile(arrival, .95):5.1f}")
EOF
