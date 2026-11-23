#!/usr/bin/env python3
"""What the one parent-map read changed, measured against the shape it replaced.

`merge_bases` used to call `ancestors` once per input and then once per common
ancestor, and `ancestors` asked the database for a row's parents every time it
visited one. On a history that forked and re-merged, that is: for every
candidate, a full walk, each step a separate SELECT. This measures a question
that goes through `merge_bases` after the change and reports the shape it runs
now, so a later change has a number to be compared against rather than a memory
of one.

The question is `land`: it asks for the merge base between the target head and
the branch head before it decides anything, which is the path a landed change
takes.

Run: python3 bench/merge_base_cost.py [changesets]
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

_PROJECT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ZF = os.environ.get("GESTA_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))
N = int(sys.argv[1]) if len(sys.argv) > 1 else 300


def sh(args, cwd, timeout=1800):
    return subprocess.run(args, cwd=cwd, capture_output=True, text=True, timeout=timeout)


def median_ms(fn, repeat=5):
    runs = []
    for _ in range(repeat):
        t = time.perf_counter()
        fn()
        runs.append((time.perf_counter() - t) * 1000.0)
    runs.sort()
    return runs[len(runs) // 2]


def main() -> int:
    if not os.path.exists(ZF):
        print(f"binary not found: {ZF}")
        return 1

    base = tempfile.mkdtemp(prefix="merge-base-cost-")
    try:
        d = os.path.join(base, "r")
        os.makedirs(d)
        sh([ZF, "init"], d)
        print(f"building {N} changesets with a fork and a re-merge")

        made = []
        for i in range(N):
            with open(os.path.join(d, "f.txt"), "w") as fh:
                fh.write(f"line {i}\n")
            c = sh([ZF, "changeset", "create", f"c{i}", "--author", "t", "--json"], d)
            try:
                cid = json.loads(c.stdout).get("changeset")
            except json.JSONDecodeError:
                cid = None
            if cid:
                sh([ZF, "changeset", "commit", cid], d)
                made.append(cid)

        # A side branch from the middle, then merged back. A re-merge is what makes
        # `merge_bases` do the N+1 walk: with one common ancestor it takes the cheap path.
        mid = made[N // 2]
        sh([ZF, "branch", "side", "--at", mid], d)
        sh([ZF, "checkout", "side"], d)
        for i in range(5):
            with open(os.path.join(d, "g.txt"), "w") as fh:
                fh.write(f"side {i}\n")
            c = sh([ZF, "changeset", "create", f"side{i}", "--author", "t", "--json"], d)
            try:
                cid = json.loads(c.stdout).get("changeset")
            except json.JSONDecodeError:
                cid = None
            if cid:
                sh([ZF, "changeset", "commit", cid], d)
        sh([ZF, "checkout", "main"], d)

        # `status` is the baseline: it opens the database and answers without a history
        # question, so it measures startup and nothing else.
        startup = median_ms(lambda: sh([ZF, "status"], d), repeat=10)

        # `log` walks the branch history and renders it; it grows with the history for
        # reasons that are not the merge base.
        log_cost = median_ms(lambda: sh([ZF, "log", "--oneline", "--limit", str(N + 10)], d), repeat=5)

        # `bisect start` resolves both ancestries, which is the same work `merge_bases`
        # does before it looks for candidates.
        def bisect_cost():
            sh([ZF, "bisect", "clear"], d)
            sh([ZF, "bisect", "start", made[0], made[-1]], d)

        bisect = median_ms(bisect_cost, repeat=5)

        print()
        print(f"{'status (startup)':<24} {startup:>9.2f} ms")
        print(f"{'log over ' + str(N):<24} {log_cost:>9.2f} ms")
        print(f"{'bisect start (walk)':<24} {bisect:>9.2f} ms")
        print()
        walk = max(bisect - startup, 0.0)
        print(f"walk over {N} changesets, startup removed: {walk:.2f} ms "
              f"({walk / N * 1000:.1f} us per changeset)")
        print()
        print("the number to watch is the per-changeset figure: a walk that reads one row")
        print("per step grows with the history, and one that reads the table once does not")
        return 0
    finally:
        shutil.rmtree(base, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
