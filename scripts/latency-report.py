#!/usr/bin/env python3
"""Summarizes scripts/latency-bench.sh runs side by side, one column per run.

    python3 scripts/latency-report.py target/e2e/bench/RUN... [--json]

From a run's trace.jsonl (the probe's line per finished update) joined by frame number with
frame-source.jsonl (when each source frame reached the screen: presented, or where the screen
reports no presentation times, as a virtual display doesn't, committed):
  updates           finished updates; capture -> decoded p50/p95/p99 (the host's capture to the
                    update's last tile decoded here), from the first source frame shown to the
                    last (or all of them, without frame numbers)
  source frames     frames shown (the first update showing each), their rate, frames skipped (the
                    number jumped over them), frames the source put on screen that never showed
                    here, updates that finished late (an older frame than one shown) or couldn't
                    be read
  source ->         source on screen -> decoded p50/p95/p99 per frame shown, -> captured p50
  source            frames drawn and on screen, and their rate
From viewer-stats.log (VIEWER=1 runs): the median of the viewer's per-second stats while frames
flowed (shownFps, totalP50Ms, displayP50Ms, ...) and how much its counters grew. From
host-stats.log: the median of every number on the host's per-second lines.
--json prints the numbers instead of the table.
"""

import json
import re
import sys
from pathlib import Path

# A bigger jump in frame numbers is the source starting over, not frames skipped (as the probe).
MAX_FRAME_JUMP = 1024
# Viewer stats worth a row: medians of the per-second lines, then counters (growth over the run).
VIEWER_MEDIANS = [
    "shownFps", "fps", "mbps", "totalP50Ms", "totalP95Ms", "displayP50Ms", "displayP95Ms", "totalMs", "captureMs",
    "encodeMs", "networkMs", "networkP95Ms", "decodeMs", "displayMs", "displayWakeMs", "displayCpuMs", "displayGpuMs",
    "displayCompositorMs", "maxStallMs", "rttMs",
]
VIEWER_COUNTERS = ["framesShown", "framesLost", "keyframeRequests", "stalls", "warmPresents", "droppedPresents"]
# A per-second line counts once frames flow at least this fast (connecting and switching don't).
FLOWING_FPS = 5


def quantile(values, q):
    """The probe's percentile: the value at round((n - 1) * q) of the sorted values."""
    if not values:
        return None
    s = sorted(values)
    return s[min(len(s) - 1, int((len(s) - 1) * q + 0.5))]


def read_jsonl(path):
    out = []
    try:
        with open(path) as f:
            for line in f:
                try:
                    out.append(json.loads(line))
                except ValueError:
                    pass  # a line cut short when the writer stopped
    except OSError:
        pass
    return out


def read_meta(run):
    meta = {}
    try:
        for line in (run / "meta.txt").read_text().splitlines():
            key, _, value = line.partition("=")
            meta[key] = value
    except OSError:
        pass
    return meta


def probe_numbers(run):
    trace = read_jsonl(run / "trace.jsonl")
    source = read_jsonl(run / "frame-source.jsonl")
    out = {}
    drawn = {f["n"]: f for f in source if f.get("type") == "frame" and "n" in f}
    # When each frame reached the screen (as the probe has it).
    if any(f.get("presented_us", 0) > 0 for f in drawn.values()):
        out["source_basis"] = "presented"
        on_screen = {n: f["presented_us"] for n, f in drawn.items() if f.get("presented_us", 0) > 0}
    else:
        out["source_basis"] = "committed"
        on_screen = {n: f["commit_us"] for n, f in drawn.items() if f.get("commit_us")}
    if drawn:
        out["source_drawn"] = len(drawn)
        out["source_on_screen"] = len(on_screen)
        times = sorted(on_screen.values())
        if len(times) > 1:
            out["source_fps"] = (len(times) - 1) / ((times[-1] - times[0]) / 1e6)
    if not trace:
        return out

    # While the source ran: connecting and switching displays aren't what's measured.
    first = next((i for i, t in enumerate(trace) if t.get("new")), 0)
    last = max((i for i, t in enumerate(trace) if t.get("new")), default=len(trace) - 1)
    measured = trace[first : last + 1]
    latency = [(t["decoded_us"] - t["capture_local_us"]) / 1000 for t in measured if t.get("capture_local_us") is not None]
    out["updates"] = len(measured)
    out["update_ms"] = [quantile(latency, q) for q in (0.5, 0.95, 0.99)]
    out["motion_updates"] = sum(1 for t in measured if t.get("motion"))
    out["full_updates"] = sum(1 for t in measured if t.get("full"))
    sizes = [t["bytes"] for t in measured if t.get("bytes") is not None]
    if sizes:
        out["update_bytes_p50"] = quantile(sizes, 0.5)

    # The source frames, as the probe counted them while the updates finished.
    shown, late, unread, newest = [], 0, 0, None
    for t in trace:
        n = t.get("n")
        if n is None:
            unread += newest is not None  # before the first number read, the strip wasn't there yet
        elif t.get("new"):
            shown.append(t)
            newest = n
        elif newest is not None and n < newest:
            late += 1
    if not shown:
        return out
    numbers = [t["n"] for t in shown]
    out["frames_shown"] = len(shown)
    span = (shown[-1]["decoded_us"] - shown[0]["decoded_us"]) / 1e6
    if span > 0:
        out["frames_fps"] = (len(shown) - 1) / span
    out["frames_skipped"] = sum(b - a - 1 for a, b in zip(numbers, numbers[1:]) if 0 < b - a <= MAX_FRAME_JUMP)
    out["frames_late"] = late
    out["updates_unread"] = unread
    if on_screen:
        first, last, seen = numbers[0], max(numbers), set(numbers)
        out["frames_never_shown"] = sum(1 for n in on_screen if first <= n <= last and n not in seen)
        to_decoded = [(t["decoded_us"] - on_screen[t["n"]]) / 1000 for t in shown if t["n"] in on_screen]
        to_captured = [
            (t["capture_local_us"] - on_screen[t["n"]]) / 1000
            for t in shown
            if t["n"] in on_screen and t.get("capture_local_us") is not None
        ]
        out["source_decoded_ms"] = [quantile(to_decoded, q) for q in (0.5, 0.95, 0.99)]
        out["source_captured_ms"] = quantile(to_captured, 0.5)
        out["frames_joined"] = len(to_decoded)
    return out


def stats_lines(path, marker):
    """The JSON of `stats={...}` on each line with `marker`, or else its key=value numbers."""
    out = []
    try:
        lines = Path(path).read_text().splitlines()
    except OSError:
        return out
    for line in lines:
        if marker not in line:
            continue
        found = re.search(r"stats=(\{.*\})", line)
        if found:
            try:
                out.append(json.loads(found.group(1)))
                continue
            except ValueError:
                pass
        fields = {}
        for key, value in re.findall(r"(\w+)=(-?[0-9][0-9.e+-]*)\b", line.split(marker, 1)[1]):
            try:
                fields[key] = float(value)
            except ValueError:
                pass
        out.append(fields)
    return out


def viewer_numbers(run):
    lines = [s for s in stats_lines(run / "viewer-stats.log", "viewer stats") if (s.get("fps") or 0) >= FLOWING_FPS]
    lines = lines[1:]  # the first second is a partial one
    out = {}
    for key in VIEWER_MEDIANS:
        values = [s[key] for s in lines if isinstance(s.get(key), (int, float))]
        if values:
            out[f"viewer_{key}"] = quantile(values, 0.5)
    for key in VIEWER_COUNTERS:
        values = [s[key] for s in lines if isinstance(s.get(key), (int, float))]
        if len(values) > 1:
            out[f"viewer_{key}+"] = values[-1] - values[0]
    if lines:
        out["viewer_seconds"] = len(lines)
    return out


def host_numbers(run):
    lines = stats_lines(run / "host-stats.log", "host stats")
    keys = sorted({k for s in lines for k, v in s.items() if isinstance(v, (int, float)) and not isinstance(v, bool)})
    return {f"host_{key}": quantile([s[key] for s in lines if isinstance(s.get(key), (int, float))], 0.5) for key in keys}


def summarize(run):
    run = Path(run)
    numbers = {"run": run.name, **{f"meta_{k}": v for k, v in read_meta(run).items()}}
    numbers.update(probe_numbers(run))
    numbers.update(viewer_numbers(run))
    numbers.update(host_numbers(run))
    return numbers


def fmt(value, digits=1):
    if value is None:
        return "-"
    if isinstance(value, list):
        return " / ".join(fmt(v, digits) for v in value)
    if isinstance(value, float) and not value.is_integer():
        return f"{value:.{digits}f}"
    if isinstance(value, (int, float)):
        return str(int(value))
    return str(value)


def rows(runs):
    """(title, one cell per run) for every row some run has a value for."""
    def row(title, cell):
        cells = [cell(r) for r in runs]
        return (title, cells) if any(c not in (None, "-", "") for c in cells) else None

    def skipped(r):
        if "frames_skipped" not in r:
            return None
        total = r["frames_skipped"] + r.get("frames_shown", 0)
        return f"{r['frames_skipped']} ({100 * r['frames_skipped'] / total:.1f}%)" if total else "0"

    def source(r):
        if "source_drawn" not in r:
            return None
        return f"{r['source_drawn']} / {r['source_on_screen']} ({fmt(r.get('source_fps'))} fps)"

    out = [
        row("mode", lambda r: r.get("meta_mode")),
        row("app", lambda r: r.get("meta_app")),
        row("app built", lambda r: r.get("meta_app_built")),
        row("client", lambda r: r.get("meta_client")),
        row("display", lambda r: f"{r['meta_size']} @ {r.get('meta_refresh', '?')} Hz" if "meta_size" in r else None),
        row("knobs", lambda r: " ".join(f"{k[5:]}={v}" for k, v in r.items() if k.startswith("meta_LANKVM_")) or None),
        row("updates (motion, full)", lambda r: f"{r['updates']} ({r['motion_updates']}, {r['full_updates']})" if "updates" in r else None),
        row("capture→decoded p50/95/99 ms", lambda r: fmt(r.get("update_ms"))),
        row("bytes per update p50", lambda r: fmt(r.get("update_bytes_p50"))),
        row("source frames shown", lambda r: fmt(r.get("frames_shown"))),
        row("  rate (fps)", lambda r: fmt(r.get("frames_fps"))),
        row("  skipped", skipped),
        row("  on screen, never shown", lambda r: fmt(r.get("frames_never_shown"))),
        row("  late / unread updates", lambda r: f"{r['frames_late']} / {r['updates_unread']}" if "frames_late" in r else None),
        row("source on screen means", lambda r: r.get("source_basis")),
        row("source→decoded p50/95/99 ms", lambda r: fmt(r.get("source_decoded_ms"))),
        row("source→captured p50 ms", lambda r: fmt(r.get("source_captured_ms"))),
        row("source drawn / on screen", source),
        row("viewer seconds", lambda r: fmt(r.get("viewer_seconds"))),
    ]
    out += [row(f"viewer {key} (p50)", lambda r, k=key: fmt(r.get(f"viewer_{k}"))) for key in VIEWER_MEDIANS]
    out += [row(f"viewer {key} (+)", lambda r, k=key: fmt(r.get(f"viewer_{k}+"))) for key in VIEWER_COUNTERS]
    host_keys = sorted({k for r in runs for k in r if k.startswith("host_")})
    out += [row(f"host {key[5:]} (p50)", lambda r, k=key: fmt(r.get(k))) for key in host_keys]
    return [r for r in out if r]


def main(argv):
    as_json = "--json" in argv
    dirs = [a for a in argv if a != "--json"]
    if not dirs:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    missing = [d for d in dirs if not Path(d).is_dir()]
    if missing:
        print(f"not a run directory: {', '.join(missing)}", file=sys.stderr)
        return 2
    runs = [summarize(d) for d in dirs]
    if as_json:
        print(json.dumps(runs, indent=2))
        return 0
    table = [("", [r["run"] for r in runs])] + rows(runs)
    width = max(len(title) for title, _ in table)
    columns = [max(len(str(cells[i])) for _, cells in table) for i in range(len(runs))]
    for title, cells in table:
        print("  ".join([title.ljust(width)] + [str(c if c is not None else "-").ljust(w) for c, w in zip(cells, columns)]).rstrip())
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
