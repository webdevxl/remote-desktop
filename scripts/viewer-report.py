#!/usr/bin/env python3
"""Summarizes scripts/viewer-bench.sh runs side by side, one column per run.

    python3 scripts/viewer-report.py target/e2e/bench/RUN... [--json]

From a run's scope.jsonl (every frame the screen scope read off the viewer window, with when it
reached the glass) joined by frame number with frame-source.jsonl (when each source frame was
committed on the host's virtual display; a virtual display reports no presentation times):
  source→glass      per distinct source frame (the first time its number shows): glass time
                    (the capture's displayTime) - commit time, p50/p95/p99; source→scope the same
                    with when the scope got the frame
  frames on glass   distinct source frames shown, their rate (overall, and the median and worst
                    whole second), frames skipped (the number jumped over them), late (an older
                    number after a newer one), frames committed in that span never shown, and the
                    share of captured frames the scope couldn't read
  source            frames committed while measured, and their rate
From viewer-stats.log: the median of the viewer's per-second stats while frames flowed (as
latency-report.py). From cpu.log: median CPU % (CPU time over each second) and RSS per
process while the scope measured. --json prints the numbers instead of the table (host stats too).
"""

import bisect
import importlib.util
import sys
from pathlib import Path

# The helpers and conventions of latency-report.py (quantiles, JSON lines, meta, viewer stats),
# loaded without leaving a __pycache__ in scripts/.
sys.dont_write_bytecode = True
_spec = importlib.util.spec_from_file_location("latency_report", Path(__file__).resolve().parent / "latency-report.py")
latency = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(latency)
quantile, read_jsonl, read_meta, fmt = latency.quantile, latency.read_jsonl, latency.read_meta, latency.fmt

# A bigger jump in frame numbers is the source starting over, not frames skipped.
MAX_FRAME_JUMP = latency.MAX_FRAME_JUMP
# The strip carries n modulo 2^16.
WRAP = 1 << 16

def unwrap(frames, commits):
    """Absolute frame numbers for the scope's frame lines (n modulo 2^16), in place ("abs").

    Each number is taken as the one nearest the previous; the first, and any after a jump bigger
    than MAX_FRAME_JUMP (the source started over), as the one nearest the source frame committed
    last before it reached the glass."""
    times = sorted((t, n) for n, t in commits.items())
    stamps = [t for t, _ in times]

    def anchor(n, at):
        i = bisect.bisect_right(stamps, at) - 1
        if i < 0:
            return n
        source = times[i][1]
        return n + WRAP * round((source - n) / WRAP)

    last = None
    for f in frames:
        n = f["n"]
        if last is None:
            f["abs"] = anchor(n, f["display_us"])
        else:
            delta = (n - last + WRAP // 2) % WRAP - WRAP // 2
            f["abs"] = last + delta if abs(delta) <= MAX_FRAME_JUMP else anchor(n, f["display_us"])
        last = f["abs"]


def scope_numbers(run):
    scope = read_jsonl(run / "scope.jsonl")
    source = read_jsonl(run / "frame-source.jsonl")
    out = {}
    commits = {f["n"]: f["commit_us"] for f in source if f.get("type") == "frame" and f.get("commit_us")}
    if commits:
        out["source_drawn"] = len(commits)
    if not scope:
        return out
    errors =[s.get("message") for s in scope if s.get("type") == "error"]
    if errors:
        out["scope_errors"] = errors
    window = next((s for s in reversed(scope) if s.get("type") == "window"), None)
    if window:
        out["window"] = f"{fmt(window.get('w'))}×{fmt(window.get('h'))} pt @{fmt(window.get('scale'))}x"
    strip = next((s for s in reversed(scope) if s.get("type") == "strip"), None)
    if strip:
        out["strip_scale"] = strip.get("s")
    summary = next((s for s in scope if s.get("type") == "summary"), {})
    for key in ("frames", "idle", "blank", "undetected", "detections", "seconds"):
        if key in summary:
            out[f"scope_{key}"] = summary[key]
    start = next((s for s in scope if s.get("type") == "start"), {})
    if summary.get("capture_epoch"):
        out["scope_epoch"] = [summary["capture_epoch"], summary["capture_epoch"] + (summary.get("seconds") or 0)]
    elif start.get("epoch"):
        out["scope_epoch"] = [start["epoch"], start["epoch"] + (start.get("seconds") or 0) + 1]

    frames = [s for s in scope if s.get("type") == "frame" and "n" in s and s.get("display_us", 0) > 0]
    unreadable = sum(1 for s in scope if s.get("type") == "unreadable")
    out["scope_readable"] = len(frames)
    out["scope_unreadable"] = unreadable
    if frames or unreadable:
        out["unreadable_share"] = unreadable / (len(frames) + unreadable)
    if not frames:
        return out

    unwrap(frames, commits)
    first_seen, seen, newest, late, shown = [], set(), None, 0, []
    for f in frames:
        if f["abs"] in seen:
            continue
        seen.add(f["abs"])
        first_seen.append(f)
        if newest is None or f["abs"] > newest:
            shown.append(f)
            newest = f["abs"]
        else:
            late += 1
    out["frames_shown"] = len(first_seen)
    out["frames_repeated"] = len(frames) - len(first_seen)
    out["frames_late"] = late
    span = (first_seen[-1]["display_us"] - first_seen[0]["display_us"]) / 1e6
    if span > 0:
        out["frames_fps"] = (len(first_seen) - 1) / span
    # Whole seconds from the first frame shown: how steady the rate is.
    per_second = {}
    for f in first_seen:
        second = int((f["display_us"] - first_seen[0]["display_us"]) // 1_000_000)
        per_second[second] = per_second.get(second, 0) + 1
    whole = [per_second.get(s, 0) for s in range(int(span))]
    if whole:
        out["frames_fps_p50"] = quantile(whole, 0.5)
        out["frames_fps_min"] = min(whole)
    numbers = [f["abs"] for f in shown]
    out["frames_skipped"] = sum(b - a - 1 for a, b in zip(numbers, numbers[1:]) if 0 < b - a <= MAX_FRAME_JUMP)

    if commits:
        lo, hi = numbers[0], max(numbers)
        out["frames_never_shown"] = sum(1 for n in commits if lo <= n <= hi and n not in seen)
        joined = [f for f in first_seen if f["abs"] in commits]
        out["frames_joined"] = len(joined)
        glass = [(f["display_us"] - commits[f["abs"]]) / 1000 for f in joined]
        arrival = [(f["arrival_us"] - commits[f["abs"]]) / 1000 for f in joined if f.get("arrival_us")]
        out["glass_ms"] = [quantile(glass, q) for q in (0.5, 0.95, 0.99)]
        out["arrival_ms"] = [quantile(arrival, q) for q in (0.5, 0.95, 0.99)]
        # What the source drew while the scope watched.
        t0, t1 = first_seen[0]["display_us"], first_seen[-1]["display_us"]
        during = sorted(t for t in commits.values() if t0 <= t <= t1)
        if len(during) > 1:
            out["source_fps"] = (len(during) - 1) / ((during[-1] - during[0]) / 1e6)
    return out


def cpu_seconds(text):
    """ps's cumulative CPU time ([[dd-]hh:]mm:ss.ss) in seconds."""
    days, _, rest = text.rpartition("-")
    total = 0.0
    for part in rest.split(":"):
        total = total * 60 + float(part)
    return total + (float(days) * 86400 if days else 0)


def cpu_numbers(run, window=None):
    """Median CPU % and RSS (MB) per process from cpu.log, within `window` (epoch s) if given."""
    samples = {}
    try:
        lines = (run / "cpu.log").read_text().splitlines()
    except OSError:
        return {}
    for line in lines:
        parts = line.split()
        if len(parts) < 5:
            continue
        try:
            t, name, pid, pcpu, rss = float(parts[0]), parts[1], parts[2], float(parts[3]), float(parts[4])
            used = cpu_seconds(parts[5]) if len(parts) > 5 else None
        except ValueError:
            continue
        samples.setdefault(name, []).append((t, pid, pcpu, rss, used))
    out = {}
    for name, rows in samples.items():
        rows.sort()
        inside = [r for r in rows if window is None or window[0] - 0.5 <= r[0] <= window[1] + 0.5]
        if len(inside) < 2:
            inside = rows  # too short a window: the whole run
        rates = [
            100 * (b[4] - a[4]) / (b[0] - a[0])
            for a, b in zip(inside, inside[1:])
            if a[1] == b[1] and a[4] is not None and b[4] is not None and b[0] > a[0]
        ]
        out[f"cpu_{name}"] = quantile(rates, 0.5) if rates else quantile([r[2] for r in inside], 0.5)
        out[f"rss_{name}"] = quantile([r[3] / 1024 for r in inside], 0.5)
    return out


def summarize(run):
    run = Path(run)
    numbers = {"run": run.name, **{f"meta_{k}": v for k, v in read_meta(run).items()}}
    numbers.update(scope_numbers(run))
    numbers.update(latency.viewer_numbers(run))
    numbers.update(latency.host_numbers(run))
    numbers.update(cpu_numbers(run, numbers.get("scope_epoch")))
    return numbers


# Processes in cpu.log, in the order they're shown.
PROCESSES = ["host", "viewer", "frame-source", "scope"]


def rows(runs):
    """(title, one cell per run) for every row some run has a value for."""
    def row(title, cell):
        cells = [cell(r) for r in runs]
        return (title, cells) if any(c not in (None, "-", "") for c in cells) else None

    def share(count_key, total_keys):
        def cell(r):
            if count_key not in r:
                return None
            total = sum(r.get(k, 0) for k in total_keys)
            return f"{r[count_key]} ({100 * r[count_key] / total:.1f}%)" if total else str(r[count_key])
        return cell

    def scope_frames(r):
        if "scope_readable" not in r:
            return None
        return f"{r.get('scope_frames', '-')} ({r['scope_readable']} / {r['scope_unreadable']} / {fmt(r.get('scope_idle'))})"

    out = [
        row("mode", lambda r: r.get("meta_mode")),
        row("app", lambda r: r.get("meta_app")),
        row("app built", lambda r: r.get("meta_app_built")),
        row("client", lambda r: r.get("meta_client")),
        row("display", lambda r: f"{r['meta_size']} @ {r.get('meta_refresh', '?')} Hz" if "meta_size" in r else None),
        row("power", lambda r: r.get("meta_power")),
        row("knobs", lambda r: " ".join(f"{k[5:]}={v}" for k, v in r.items() if k.startswith("meta_LANKVM_")) or None),
        row("viewer window", lambda r: r.get("window") and f"{r['window']}, strip ×{fmt(r.get('strip_scale'), 2)}"),
        row("scope errors", lambda r: "; ".join(r["scope_errors"]) if r.get("scope_errors") else None),
        row("captured (read / unread / idle)", scope_frames),
        row("  unreadable", lambda r: f"{100 * r['unreadable_share']:.1f}%" if "unreadable_share" in r else None),
        row("source→glass p50/95/99 ms", lambda r: fmt(r.get("glass_ms"))),
        row("source→scope p50/95/99 ms", lambda r: fmt(r.get("arrival_ms"))),
        row("frames on glass", lambda r: fmt(r.get("frames_shown"))),
        row("  rate (fps)", lambda r: fmt(r.get("frames_fps"))),
        row("  per second p50 / min", lambda r: f"{fmt(r['frames_fps_p50'])} / {fmt(r['frames_fps_min'])}" if "frames_fps_p50" in r else None),
        row("  skipped", share("frames_skipped", ["frames_skipped", "frames_shown"])),
        row("  late", lambda r: fmt(r.get("frames_late"))),
        row("  committed, never shown", lambda r: fmt(r.get("frames_never_shown"))),
        row("source committed (fps measured)", lambda r: f"{r['source_drawn']} ({fmt(r.get('source_fps'))} fps)" if "source_drawn" in r else None),
        row("viewer seconds", lambda r: fmt(r.get("viewer_seconds"))),
    ]
    out += [row(f"viewer {key} (p50)", lambda r, k=key: fmt(r.get(f"viewer_{k}"))) for key in latency.VIEWER_MEDIANS]
    out += [row(f"viewer {key} (+)", lambda r, k=key: fmt(r.get(f"viewer_{k}+"))) for key in latency.VIEWER_COUNTERS]
    names = PROCESSES + sorted({k[4:] for r in runs for k in r if k.startswith("cpu_")} - set(PROCESSES))
    out += [row(f"cpu % {name} (p50)", lambda r, n=name: fmt(r.get(f"cpu_{n}"))) for name in names]
    out += [row(f"rss MB {name} (p50)", lambda r, n=name: fmt(r.get(f"rss_{n}"), 0)) for name in names]
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
        import json

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
