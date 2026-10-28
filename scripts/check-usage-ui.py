#!/usr/bin/env python3
"""Visual check of the plan usage chip and popover, on the real GUI.

Renders the window offscreen (ARRANGER_SNAPSHOT) in a throwaway ARRANGER_HOME whose daemon has
claude at 42% of its 5-hour window and codex at 87%, once as it opens and once with the usage
popover open (ARRANGER_MENU=usage), then checks the pixels: the chip and its bars in the right
half of the status bar (codex's amber, claude's blue), no popover until it's opened, then a popover
above the chip holding both agents' bars. Needs a display (macOS window server, or X/Wayland).

usage: scripts/check-usage-ui.py [path/to/arranger]   (default: target/debug/arranger)
"""
import json, os, shutil, signal, sqlite3, subprocess, sys, tempfile, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "target/debug/arranger"))
# Ink's colours (src/bin/arranger-gui/theme.rs).
AMBER, BLUE, DIALOG, RAISED = (0xF2, 0xB3, 0x5B), (0x7D, 0xA6, 0xFF), (0x14, 0x18, 0x1F), (0x1C, 0x21, 0x2A)
STATUS_H, LOGICAL_W = 26.0, 1280.0


def run(home, *args, check=True):
    env = {**os.environ, "ARRANGER_HOME": home}
    env.pop("ARRANGER_CLI", None)
    return subprocess.run([BIN, *args], env=env, capture_output=True, text=True, timeout=30, check=check)


def snapshot(home, repo, menu):
    """The window's first frames as (width, height, rgb bytes), with `menu` open ("" for none)."""
    shot = os.path.join(home, f"shot-{menu or 'plain'}.ppm")
    env = {**os.environ, "ARRANGER_HOME": home, "ARRANGER_SNAPSHOT": shot, "ARRANGER_MENU": menu}
    env.pop("ARRANGER_CLI", None)
    gui = subprocess.Popen([BIN], cwd=repo, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    try:
        deadline = time.time() + 20
        while not os.path.exists(shot) and time.time() < deadline and gui.poll() is None:
            time.sleep(0.2)
        time.sleep(2)  # let the daemon's limits arrive and redraw
    finally:
        try:
            os.killpg(gui.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        gui.wait(timeout=10)
    if not os.path.exists(shot):
        sys.exit(f"FAIL: the GUI wrote no snapshot (is there a display?); exit status {gui.returncode}")
    data = open(shot, "rb").read()
    head, _, rest = data.partition(b"\n")
    _, w, h, _ = head.split()
    return int(w), int(h), rest


def count(img, color, x0, y0, x1, y1, tol=6):
    """Pixels in the box within `tol` of `color`, and the leftmost one's x."""
    w, h, px = img
    n, left = 0, None
    for y in range(max(0, int(y0)), min(h, int(y1))):
        row = y * w * 3
        for x in range(max(0, int(x0)), min(w, int(x1))):
            i = row + x * 3
            if all(abs(px[i + k] - color[k]) <= tol for k in range(3)):
                n += 1
                left = x if left is None else min(left, x)
    return n, left


def main():
    if not os.path.exists(BIN):
        sys.exit(f"FAIL: {BIN} not built; run cargo build first")
    home = tempfile.mkdtemp(prefix="arranger-usage-ui-")
    failures = []
    try:
        repo = os.path.join(home, "repo")
        os.makedirs(repo)
        subprocess.run(["git", "init", "-q", "-b", "main", repo], check=True)
        # Start the daemon once so its database exists, stop it, then give it plan usage to load.
        run(home, "ls")
        run(home, "stop", check=False)
        now = int(time.time())
        limits = [
            {"agent": "claude", "window": "5h", "used": 42.0, "resets": now + 8000, "status": "allowed", "seen": now},
            {"agent": "claude", "window": "week", "used": 18.0, "resets": now + 300000, "status": "allowed", "seen": now},
            {"agent": "codex", "window": "5h", "used": 87.0, "resets": now + 2000, "status": "", "seen": now},
            {"agent": "codex", "window": "week", "used": 33.0, "resets": now + 200000, "status": "", "seen": now},
        ]
        db = sqlite3.connect(os.path.join(home, "arranger.db"))
        db.execute("INSERT OR REPLACE INTO settings (key, value) VALUES ('plan_limits', ?)", (json.dumps(limits),))
        db.commit()
        db.close()
        with open(os.path.join(home, "gui.json"), "w") as f:
            json.dump({"theme": "Ink"}, f)

        plain = snapshot(home, repo, "")
        w, h, _ = plain
        s = w / LOGICAL_W
        # The right half: the left one has the task counts, in the same colours.
        status = (w / 2, h - STATUS_H * s, w, h)
        amber, _ = count(plain, AMBER, *status)
        blue, _ = count(plain, BLUE, *status)
        chip, _ = count(plain, RAISED, *status)
        if amber < 20 * s * s:
            failures.append(f"no codex bar (amber) in the status bar's right half: {amber} px")
        if blue < 8 * s * s:
            failures.append(f"no claude bar (blue) in the status bar: {blue} px")
        if chip < 200 * s * s:
            failures.append(f"no chip background in the status bar: {chip} px")
        above = (w / 3, h * 0.4, w, h - STATUS_H * s - 2)
        stray, _ = count(plain, DIALOG, *above, tol=1)
        if stray > 2000 * s * s:
            failures.append(f"a popover is open before the chip is clicked: {stray} px")

        opened = snapshot(home, repo, "usage")
        panel, _ = count(opened, DIALOG, *above, tol=1)
        if panel < 20000 * s * s:
            failures.append(f"no usage popover above the chip: {panel} px of its background")
        bars_amber, _ = count(opened, AMBER, *above)
        bars_blue, _ = count(opened, BLUE, *above)
        # codex's 5h bar (87%) is amber; claude's two and codex's week are blue.
        if bars_amber < 100 * s * s or bars_blue < 100 * s * s:
            failures.append(f"the popover lacks the window bars: amber {bars_amber} px, blue {bars_blue} px")
    finally:
        run(home, "stop", check=False)
        shutil.rmtree(home, ignore_errors=True)
    if failures:
        sys.exit("FAIL:\n  " + "\n  ".join(failures))
    print("ok: usage chip and popover render with both agents' windows")


if __name__ == "__main__":
    main()
