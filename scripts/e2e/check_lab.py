"""Checks what LanKVM Input Lab received during a scenario played through a LanKVM host that
really injects ("hid" or "pid" mode):

    python scripts/e2e/check_lab.py target/e2e/input-lab.jsonl                       # basic.jsonl
    python scripts/e2e/check_lab.py --scenario gestures target/e2e/input-lab.jsonl   # gestures.jsonl

Exits 1 if any check fails. Start the lab fresh before the run, so its log has only that run.
"""

import argparse
import json
import math
import sys

TAG_PREFIX = 0x4C4B564D  # "LKVM": high half of every LanKVM-injected event's source user data


def checker():
    """A list of results and a function that prints and records one."""
    results = []

    def check(name, ok, detail=""):
        results.append(bool(ok))
        print(f"{'PASS' if ok else 'FAIL'}  {name}" + (f"  ({detail})" if detail else ""))

    return results, check


def inside(window, e, part):
    x, y, w, h = window[part]
    px, py = e["cg_loc"]
    return x <= px < x + w and y <= py < y + h


def check_basic(events):
    window = [e for e in events if e["type"] == "window"][-1]
    texts = [e["value"] for e in events if e["type"] == "text"]
    downs = [e for e in events if e["type"] == "down"]
    ups = [e for e in events if e["type"] == "up"]
    keydowns = [e for e in events if e["type"] == "keydown"]
    scrolls = [e for e in events if e["type"] == "scroll"]
    drags = [e for e in events if e["type"] == "drag"]
    injected = [e for e in events if "src_user_data" in e and e["type"] not in ("window", "ready", "focus")]
    results, check = checker()

    tagged = [e for e in injected if (e["src_user_data"] >> 32) == TAG_PREFIX]
    check("every event carries LanKVM's injection tag", injected and len(tagged) == len(injected),
          f"{len(tagged)}/{len(injected)} tagged")
    final = texts[-1] if texts else ""
    check("typed text, shortcuts, arrows, dead key, right shift, caps lock", final == "éBCDk ", repr(final))
    check("typing 'Hello, LanKVM! 123' went through first", "Hello, LanKVM! 123" in texts, f"{len(texts)} text changes")
    patch_downs = [e for e in downs if inside(window, e, "patch")]
    left = [e["clicks"] for e in patch_downs if e["button"] == "left"]
    check("click counts: single, double, triple, drag, command-click", left == [1, 1, 2, 1, 2, 3, 1, 1], str(left))
    check("right and middle buttons", [e["button"] for e in patch_downs if e["button"] != "left"] == ["right", "other2"])
    cmd_click = [e for e in patch_downs if e["flags"] & 0x100000]
    check("command-click carries ⌘ with the left device bit", len(cmd_click) == 1 and cmd_click[0]["flags"] & 0x8 != 0)
    check("drag arrives as drag events, inside the patch", len(drags) >= 10 and all(inside(window, e, "patch") for e in drags), f"{len(drags)} drags")
    # NSTextView tracks a click in its own loop, which takes the release before any app-level
    # observer sees it; count presses and releases outside the text box.
    outside = lambda es: [e for e in es if not inside(window, e, "text")]
    check("every press has its release", len(outside(downs)) == len(outside(ups)),
          f"{len(outside(downs))} downs, {len(outside(ups))} ups outside the text box")
    shift_b = [e for e in keydowns if e["code"] == 11]
    check("right shift arrives as right shift (device bit 0x4)", shift_b and shift_b[0]["flags"] & 0x4 != 0)
    # A phased (trackpad) scroll starts NSScrollView's own tracking loop, which takes the rest of
    # the gesture before any app-level observer sees it: check the start, then the effect.
    phases = [e["phase"] for e in scrolls if e["precise"]]
    check("trackpad scroll arrives with its phase (began)", phases[:1] == ["began"], " ".join(phases))
    scrolled = [e["y"] for e in events if e["type"] == "scrolled"]
    check("the gesture and the wheel scrolled the list down by ~270 pt", scrolled and max(scrolled) >= 240,
          f"{len(scrolled)} moves, max offset {max(scrolled) if scrolled else 0:.0f}")
    # The script's last key is a space held for 1.5 s (after typing ends with "cd").
    last_d = max(i for i, e in enumerate(keydowns) if e["code"] == 2)
    spaces = [e for e in keydowns[last_d + 1:] if e["code"] == 49]
    check("a held key is pressed once (no repeats made up on the host)", len(spaces) == 1 and not spaces[0]["repeat"], f"{len(spaces)} presses")
    return results


def check_gestures(events):
    """scripts/e2e/gestures.jsonl: a pinch to 1.5x, a 45 degree turn, a smart zoom and a swipe to
    the right over the patch. The Dock swipe and Mission Control that follow change the whole Mac,
    so the host must not post them; the lab can't see them either way (check the host's log for
    "inject guard dropped=N")."""
    window = [e for e in events if e["type"] == "window"][-1]
    names = ("magnify", "rotate", "smart_magnify", "swipe", "begin_gesture", "end_gesture")
    gestures = [e for e in events if e["type"] in names]
    results, check = checker()

    tagged = [e for e in gestures if (e.get("src_user_data", 0) >> 32) == TAG_PREFIX]
    check("every gesture event carries LanKVM's injection tag", gestures and len(tagged) == len(gestures),
          f"{len(tagged)}/{len(gestures)} tagged")
    check("every gesture lands on the patch", gestures and all("cg_loc" in e and inside(window, e, "patch") for e in gestures))

    def phases(kind):
        collapsed = []
        for e in events:
            if e["type"] == kind and (not collapsed or collapsed[-1] != e["phase"]):
                collapsed.append(e["phase"])
        return collapsed

    pinch = [e for e in events if e["type"] == "magnify"]
    scale = math.prod(1 + e["amount"] for e in pinch)
    check("the pinch goes began, changed, ended", phases("magnify") == ["began", "changed", "ended"], " ".join(phases("magnify")))
    check("the pinch zooms to 1.5x", abs(scale - 1.5) < 0.02, f"{scale:.3f}x over {len(pinch)} events")
    turn = [e for e in events if e["type"] == "rotate"]
    degrees = sum(e["degrees"] for e in turn)
    check("the rotation goes began, changed, ended", phases("rotate") == ["began", "changed", "ended"], " ".join(phases("rotate")))
    check("the rotation turns 45 degrees counterclockwise", abs(degrees - 45) < 0.5, f"{degrees:.2f} over {len(turn)} events")
    taps = [e for e in events if e["type"] == "smart_magnify"]
    check("one smart zoom", len(taps) == 1, f"{len(taps)}")
    swipes = [(e["dx"], e["dy"]) for e in events if e["type"] == "swipe" and (e["dx"], e["dy"]) != (0, 0)]
    check("one swipe to the right (deltaX -1)", swipes == [(-1, 0)], str(swipes))
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("log", nargs="?", default="target/e2e/input-lab.jsonl", help="Input Lab's log")
    parser.add_argument("--scenario", choices=("basic", "gestures"), default="basic", help="the script that was played")
    args = parser.parse_args()
    events = [json.loads(line) for line in open(args.log) if line.strip()]
    results = check_gestures(events) if args.scenario == "gestures" else check_basic(events)
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
