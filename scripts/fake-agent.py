#!/usr/bin/env python3
# Fake agent: prints yes-style lines forever, echoes input lines, exits on "q".
import select, sys

n, stdin_open = 0, True
while True:
    r, _, _ = select.select([sys.stdin] if stdin_open else [], [], [], 0.1)
    if r:
        line = sys.stdin.readline()
        if not line:
            stdin_open = False
        elif line.strip() == "q":
            break
        else:
            print(f"got: {line.strip()}", flush=True)
    n += 1
    print(f"y {n}", flush=True)
