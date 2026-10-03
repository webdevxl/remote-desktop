#!/usr/bin/env bash
# End-to-end test of remote control on one Mac, with separate LanKVM instances acting as
# separate Macs. Nothing here touches a LanKVM you already run on the default port (47800).
#
#   scripts/e2e-control.sh setup            # build, create identities, pre-pair host/viewer/probe
#   scripts/e2e-control.sh host [record|hid|pid] # start the host copy (default: record, injects nothing)
#   scripts/e2e-control.sh viewer           # start "LanKVM 2" as the viewer (connects itself, logs stats)
#   scripts/e2e-control.sh lab              # start LanKVM Input Lab (logs what it receives)
#   scripts/e2e-control.sh probe ARGS...    # run the headless viewer against the host copy
#   scripts/e2e-control.sh stop             # quit the test copies
#
# The host copy is the same signed app (bundle id dev.lankvm.LanKVM), so it shares the Screen
# Recording and Accessibility grants of your LanKVM. Modes:
#   record  writes the events it would inject to $E2E/host/injected.jsonl, posts nothing
#           (no permission needed, no effect on this Mac);
#   hid     really moves the mouse and types, but only into Input Lab's windows (start the lab
#           first; everything else is dropped by LANKVM_INJECT_GUARD_PID). Run it only while
#           nobody uses this Mac;
#   pid     posts every event to Input Lab only, wherever focus and the pointer are.
# Same-Mac control is enabled for the host copy (LANKVM_ALLOW_SAME_MAC_CONTROL); the viewer drops
# events its host injected, so they don't loop. Control ends by itself after TTL seconds.
#
# Env: E2E (default target/e2e), HOST_PORT (47810), VIEWER_PORT (47811), TTL (120).
set -euo pipefail
cd "$(dirname "$0")/.."

E2E="${E2E:-$PWD/target/e2e}"
HOST_PORT="${HOST_PORT:-47810}"
VIEWER_PORT="${VIEWER_PORT:-47811}"
APP="target/release/LanKVM.app"
export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"

holder() { lsof -nP -t -iUDP:"$1" 2>/dev/null | head -1 || true; }

fingerprint() { cargo run -q --release -p lankvm-core --example ident -- "$1"; }

stop_port() {
    local pid
    pid="$(holder "$1")"
    if [[ -n "$pid" ]]; then
        kill "$pid"
        for _ in {1..50}; do [[ -z "$(holder "$1")" ]] && break; sleep 0.1; done
    fi
}

# The Input Lab started by "lab", if it still runs.
lab_pid() {
    local pid
    pid="$(cat "$E2E/lab.pid" 2>/dev/null || true)"
    if [[ -n "$pid" ]] && ps -p "$pid" -o comm= 2>/dev/null | grep -q input-lab; then echo "$pid"; fi
}

stop_lab() {
    local pid
    pid="$(lab_pid)"
    [[ -n "$pid" ]] && kill "$pid" 2>/dev/null
    rm -f "$E2E/lab.pid"
}

wait_port() {
    for _ in {1..100}; do [[ -n "$(holder "$1")" ]] && return 0; sleep 0.1; done
    echo "error: nothing is listening on UDP $1" >&2
    return 1
}

case "${1:-}" in
setup)
    ./scripts/bundle.sh
    cargo build -q --release -p lankvm-core --examples
    swiftc -O scripts/input-lab.swift -o target/input-lab
    mkdir -p "$E2E"/{host,viewer,probe}
    host_fp="$(fingerprint "$E2E/host")"
    viewer_fp="$(fingerprint "$E2E/viewer")"
    probe_fp="$(fingerprint "$E2E/probe")"
    printf '%s e2e-viewer\n%s e2e-probe\n' "$viewer_fp" "$probe_fp" > "$E2E/host/trusted-viewers.txt"
    printf '%s e2e-host\n' "$host_fp" > "$E2E/viewer/trusted-hosts.txt"
    printf '%s e2e-host\n' "$host_fp" > "$E2E/probe/trusted-hosts.txt"
    echo "host   $host_fp"
    echo "viewer $viewer_fp"
    echo "probe  $probe_fp"
    ;;
host)
    mode="${2:-record}"
    stop_port "$HOST_PORT"
    lab_pid="$(lab_pid)"
    case "$mode" in
    record) inject=(--env "LANKVM_INJECT=record:$E2E/host/injected.jsonl") ;;
    hid)
        [[ -n "$lab_pid" ]] || { echo "error: start the lab first (hid mode only injects into it)" >&2; exit 1; }
        inject=(--env "LANKVM_INJECT=hid" --env "LANKVM_INJECT_GUARD_PID=$lab_pid")
        ;;
    pid)
        [[ -n "$lab_pid" ]] || { echo "error: start the lab first" >&2; exit 1; }
        inject=(--env "LANKVM_INJECT=pid:$lab_pid")
        ;;
    *) echo "error: mode is record, hid or pid" >&2; exit 2 ;;
    esac
    open -n -g "$APP" --env LANKVM_PORT="$HOST_PORT" --env LANKVM_DATA_DIR="$E2E/host" \
        --env LANKVM_ALLOW_SAME_MAC_CONTROL=1 --env LANKVM_NO_PROMPTS=1 --env LANKVM_TEST_CONTROL_TTL="${TTL:-120}" \
        "${inject[@]}" --args -ApplePersistenceIgnoreState YES
    wait_port "$HOST_PORT"
    echo "host ($mode): pid $(holder "$HOST_PORT"), UDP $HOST_PORT, log $E2E/host/lankvm.log"
    ;;
viewer)
    # Connects to the host copy by itself, and logs the overlay's stats (including on-screen
    # latency) every second to $E2E/viewer/lankvm.log.
    LANKVM_NO_PROMPTS=1 LANKVM_CONNECT="127.0.0.1:$HOST_PORT" LANKVM_LOG_STATS=1 LANKVM_APP="$APP" PORT="$VIEWER_PORT" \
        DATA_DIR="$E2E/viewer" ./scripts/second-instance.sh --restart
    echo "viewer: connecting to 127.0.0.1:$HOST_PORT; stats in $E2E/viewer/lankvm.log"
    ;;
lab)
    stop_lab
    rm -f "$E2E/input-lab.jsonl"
    ./target/input-lab --log "$E2E/input-lab.jsonl" --frame "${LAB_FRAME:-1180,80,840,620}" >/dev/null 2>&1 &
    echo $! > "$E2E/lab.pid"
    for _ in {1..50}; do grep -q '"ready"' "$E2E/input-lab.jsonl" 2>/dev/null && break; sleep 0.1; done
    grep '"window"' "$E2E/input-lab.jsonl" | tail -1
    ;;
probe)
    shift
    LANKVM_DATA_DIR="$E2E/probe" LANKVM_PORT=0 ./target/release/examples/probe "127.0.0.1:$HOST_PORT" "$@"
    ;;
stop)
    stop_port "$HOST_PORT"
    stop_port "$VIEWER_PORT"
    stop_lab
    ;;
*)
    sed -n '2,20p' "$0"
    exit 2
    ;;
esac
