"""Checks what LanKVM Input Lab received during scripts/e2e/basic.jsonl, played through a LanKVM
host that really injects ("hid" or "pid" mode):

    python scripts/e2e/check_lab.py target/e2e/input-lab.jsonl

Exits 1 if any check fails. Start the lab fresh before the run, so its log has only that run.
"""

import json
import sys

TAG_PREFIX = 0x4C4B564D  # "LKVM": high half of every LanKVM-injected event's source user data


def main(path):
    events = [json.loads(line) for line in open(path) if line.strip()]
    window = [e for e in events if e["type"] == "window"][-1]
    texts = [e["value"] for e in events if e["type"] == "text"]
    downs = [e for e in events if e["type"] == "down"]
    ups = [e for e in events if e["type"] == "up"]
    keydowns = [e for e in events if e["type"] == "keydown"]
    scrolls = [e for e in events if e["type"] == "scroll"]
    drags = [e for e in events if e["type"] == "drag"]
    injected = [e for e in events if "src_user_data" in e and e["type"] not in ("window", "ready", "focus")]

    def inside(e, part):
        x, y, w, h = window[part]
        px, py = e["cg_loc"]
        return x <= px < x + w and y <= py < y + h

    results = []

    def check(name, ok, detail=""):
        results.append(ok)
        print(f"{'PASS' if ok else 'FAIL'}  {name}" + (f"  ({detail})" if detail else ""))

    tagged = [e for e in injected if (e["src_user_data"] >> 32) == TAG_PREFIX]
    check("every event carries LanKVM's injection tag", injected and len(tagged) == len(injected),
          f"{len(tagged)}/{len(injected)} tagged")
    final = texts[-1] if texts else ""
    check("typed text, shortcuts, arrows, dead key, right shift, caps lock", final == "éBCDk ", repr(final))
    check("typing 'Hello, LanKVM! 123' went through first", "Hello, LanKVM! 123" in texts, f"{len(texts)} text changes")
    patch_downs = [e for e in downs if inside(e, "patch")]
    left = [e["clicks"] for e in patch_downs if e["button"] == "left"]
    check("click counts: single, double, triple, drag, command-click", left == [1, 1, 2, 1, 2, 3, 1, 1], str(left))
    check("right and middle buttons", [e["button"] for e in patch_downs if e["button"] != "left"] == ["right", "other2"])
    cmd_click = [e for e in patch_downs if e["flags"] & 0x100000]
    check("command-click carries ⌘ with the left device bit", len(cmd_click) == 1 and cmd_click[0]["flags"] & 0x8 != 0)
    check("drag arrives as drag events, inside the patch", len(drags) >= 10 and all(inside(e, "patch") for e in drags), f"{len(drags)} drags")
    # NSTextView tracks a click in its own loop, which takes the release before any app-level
    # observer sees it; count presses and releases outside the text box.
    outside = lambda es: [e for e in es if not inside(e, "text")]
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
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1] if len(sys.argv) > 1 else "target/e2e/input-lab.jsonl"))
