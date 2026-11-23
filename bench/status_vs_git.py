#!/usr/bin/env python3
"""How long does it take to find out what changed, here and in the tool this replaces?

Every number in `docs/git-parity.md` that compares the two tools is supposed to come from a run of
this file, because absolute times move between runs on the same machine and between machines. A ratio
measured in one run is stable; a ratio assembled from two runs is not, and this file exists because
that mistake was made three times in one session.

Method, and why each part is there:

- The two tools are alternated run by run, so any drift in the machine hits both equally.
- The median is reported with p10 and p90, because a single number hides a spread this wide.
- Both directories are checked for a repository before anything is timed. A `git status` in a
  directory that is not a repository returns in 2 ms and looks like a win.
- The binary is copied off the bind mount first: running it from there was measured about 50% slower,
  which is larger than most of the changes being evaluated.

Run: python3 bench/status_vs_git.py [--dir GESTA_DIR] [--git-dir GIT_DIR] [--rounds N]
"""

import argparse
import os
import shutil
import statistics
import subprocess
import sys
import time

_HERE = os.path.dirname(os.path.abspath(__file__))
_PROJECT = os.path.dirname(_HERE)
ZF = os.environ.get("ZF_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))


def timed(cmd, cwd):
    t0 = time.perf_counter()
    r = subprocess.run(cmd, shell=True, capture_output=True, cwd=cwd)
    return (time.perf_counter() - t0) * 1000, r.returncode


def summarise(values):
    s = sorted(values)
    return (
        statistics.median(s),
        s[max(0, len(s) // 10)],
        s[min(len(s) - 1, -max(1, len(s) // 10))],
    )


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dir", default="/tmp/gesta_scale/g100000", help="a gesta repository")
    ap.add_argument("--git-dir", default="/tmp/gesta_scale/t100000", help="a git repository of the same tree")
    ap.add_argument("--rounds", type=int, default=21)
    args = ap.parse_args()

    if not os.path.isdir(os.path.join(args.dir, ".gesta")):
        sys.exit(f"not a gesta repository: {args.dir} (build one with bench/scale.py)")
    if not os.path.isdir(os.path.join(args.git_dir, ".git")):
        # Reporting an empty directory as fast is the mistake this check exists to prevent.
        sys.exit(f"not a git repository: {args.git_dir}")

    local = "/tmp/gesta_status_bench"
    shutil.copy2(ZF, local)
    os.chmod(local, 0o755)

    # A ratio is only a measurement if the shape of the tree it was taken on travels with it. Two
    # runs on differently shaped 100,000 file trees came out 3.6x apart and neither record said
    # which shape it used, which left the pair impossible to reconcile.
    count = 0
    total = 0
    for root, _, files in os.walk(args.dir):
        if ".gesta" in root:
            continue
        for name in files:
            count += 1
            try:
                total += os.path.getsize(os.path.join(root, name))
            except OSError:
                pass
    shape = f"{count} files, {total // max(1, count)} bytes each on average"

    ours, theirs = [], []
    for _ in range(args.rounds):
        a, ra = timed(f"{local} status", args.dir)
        b, rb = timed("git status --porcelain", args.git_dir)
        if ra != 0 or rb != 0:
            sys.exit(f"a run failed: gesta exit {ra}, git exit {rb}")
        ours.append(a)
        theirs.append(b)

    gm, g10, g90 = summarise(ours)
    tm, t10, t90 = summarise(theirs)
    print(f"tree: {shape}")
    print(f"{'tool':12} {'median':>9} {'p10':>9} {'p90':>9}")
    print("-" * 42)
    print(f"{'gesta':12} {gm:>8.1f}ms {g10:>8.1f}ms {g90:>8.1f}ms")
    print(f"{'git':12} {tm:>8.1f}ms {t10:>8.1f}ms {t90:>8.1f}ms")
    print()
    print(f"ratio: {gm / tm:.2f}x over {args.rounds} interleaved rounds")
    return 0


if __name__ == "__main__":
    sys.exit(main())
