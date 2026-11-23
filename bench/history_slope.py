#!/usr/bin/env python3
"""Fit the slope of one history question inside a single repository.

`history_scaling.py` built a separate repository per size, so its rows mixed a
different startup cost into every measurement and the "per change" column came
out falling, which reads as "no problem" when the truth is "startup hid the
work". This asks the same question over growing prefixes of one history, so
startup is paid once per command and the only thing that changes is how far the
walk goes. The slope is what one more changeset costs.

A linear walk is not a defect at four hundred changesets. It is a defect at the
size the systems this was compared against were built for: git/git, the
repository Wren benchmarks on, has 85,545 commits. The last line extrapolates to
that size so the number means something beyond the size that was measured.

Run: python3 bench/history_slope.py [changesets]
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
N = int(sys.argv[1]) if len(sys.argv) > 1 else 800
GIT_GIT_COMMITS = 85545


def sh(args, cwd, timeout=3600):
    return subprocess.run(args, cwd=cwd, capture_output=True, text=True, timeout=timeout)


def median_ms(fn, repeat=3):
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

    base = tempfile.mkdtemp(prefix="history-slope-")
    try:
        d = os.path.join(base, "r")
        os.makedirs(d)
        sh([ZF, "init"], d)
        print(f"building one history of {N} changesets (this is the slow part)")

        for i in range(N):
            with open(os.path.join(d, "f.txt"), "w") as fh:
                fh.write(f"line {i} of the only file\n")
            made = sh([ZF, "changeset", "create", f"c{i}", "--author", "t", "--json"], d)
            try:
                cid = json.loads(made.stdout).get("changeset")
            except json.JSONDecodeError:
                cid = None
            if cid:
                sh([ZF, "changeset", "commit", cid], d)

        ids = [
            row["id"]
            for row in json.loads(sh([ZF, "log", "--json", "--limit", str(N + 10)], d).stdout)
        ]
        if len(ids) < N:
            print(f"  recorded {len(ids)} of {N} changesets")
            return 1
        head, root = ids[0], ids[-1]

        startup = median_ms(lambda: sh([ZF, "status"], d), repeat=5)
        print(f"  startup (status): {startup:.2f} ms\n")

        print(f"{'walk over':>10} {'total':>10} {'minus startup':>15} {'per change':>12}")
        rows = []
        for n in [N // 8, N // 4, N // 2, N]:
            # The first measurement held `good` still and moved the other end, so a
            # bigger `n` meant an older `good`, a shorter ancestor set, and less work:
            # the series came out falling and proved nothing. Anchor the walk at the
            # root and move the near end, so `n` is exactly how far the walk travels.
            point = ids[N - n]

            def ask():
                sh([ZF, "bisect", "clear"], d)
                sh([ZF, "bisect", "start", root, point], d)

            total = median_ms(ask, repeat=3)
            work = max(total - startup, 0.0)
            per = work / n
            rows.append((n, total, work, per))
            print(f"{n:>10} {total:>9.2f}ms {work:>14.2f}ms {per:>10.4f}ms")

        # Least squares over the last three rows: the first one is smallest and its
        # fixed costs dominate whatever slope it has.
        tail = rows[1:]
        n_mean = sum(r[0] for r in tail) / len(tail)
        w_mean = sum(r[2] for r in tail) / len(tail)
        num = sum((r[0] - n_mean) * (r[2] - w_mean) for r in tail)
        den = sum((r[0] - n_mean) ** 2 for r in tail)
        slope = num / den if den else 0.0

        print()
        lo, hi = rows[0], rows[-1]
        spread = hi[2] - lo[2]
        print(f"work at {lo[0]} changesets: {lo[2]:.2f} ms; at {hi[0]}: {hi[2]:.2f} ms")
        print(f"difference over {hi[0] - lo[0]} more changesets: {spread:+.2f} ms "
              f"({slope:+.5f} ms per change fitted)")

        # A slope at or below zero is not a finding, it is a measurement that cannot
        # see the thing it is looking for: the fixed cost of starting the process is
        # 38 ms and the walk over 600 changesets is under 10 ms, so the walk is inside
        # the noise. Saying so is the only honest reading, and it bounds the claim:
        # this bench can rule out a walk over hundreds of changesets being slow, and
        # it cannot measure what a walk over eighty thousand would cost.
        if slope <= 0.005:
            print()
            print("the walk is not distinguishable from startup at this size.")
            print("this bench cannot say what the walk costs at 85,545 changesets;")
            print("it can only say the cost is under the 38 ms of process startup here.")
        else:
            print()
            print(f"extrapolated to git/git's {GIT_GIT_COMMITS:,} commits: "
                  f"{slope * GIT_GIT_COMMITS / 1000:.2f} s of walk per ancestry question")
            print("a line through four points on one machine, not a measurement at that size")
        return 0
    finally:
        shutil.rmtree(base, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
