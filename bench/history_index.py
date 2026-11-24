#!/usr/bin/env python3
"""Measure what a history question costs when the answer is not indexed.

Wren's design doc forbids answering a history query by walking the commit log
(invariant 7), and uses a generation number plus skip lists instead. Jujutsu,
Sapling and Dolt all keep an explicit commit graph for the same reason. This
measures gesta against git on the same linear history, so the claim "our
ancestry is a linear walk" stops being a reading of the code and becomes a
number.

Three questions, each asked the way a user asks it:

  ancestry   is commit A an ancestor of B          (gesta ancestors / merge_bases)
  path log   which commits touched this file       (gesta log -- path)
  merge base the common ancestor of two branches   (gesta merge-base)

Git answers all three from its commit-graph and its path-limited rev walk.
The history is linear with one branch point, because a linear history is where
an unindexed walk has the most work to do per answer and where git's own
indexes help least.

Run: python3 bench/history_index.py [commits]
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
N = int(sys.argv[1]) if len(sys.argv) > 1 else 1000

results = []


def sh(args, cwd, timeout=600):
    return subprocess.run(args, cwd=cwd, capture_output=True, text=True, timeout=timeout)


def timed(fn, repeat=3):
    """Median of `repeat` runs, in milliseconds."""
    runs = []
    for _ in range(repeat):
        t = time.perf_counter()
        fn()
        runs.append((time.perf_counter() - t) * 1000.0)
    runs.sort()
    return runs[len(runs) // 2]


def record(question, git_ms, gesta_ms, note=""):
    ratio = gesta_ms / git_ms if git_ms else float("inf")
    results.append((question, git_ms, gesta_ms, ratio, note))
    print(f"  {question:<26} git {git_ms:>8.2f} ms   gesta {gesta_ms:>9.2f} ms   {ratio:>6.2f}x")


def main() -> int:
    if not os.path.exists(ZF):
        print(f"binary not found: {ZF}")
        return 1
    if shutil.which("git") is None:
        print("git is not installed: skipped")
        return 0

    base = tempfile.mkdtemp(prefix="history-index-")
    try:
        g = os.path.join(base, "g")
        z = os.path.join(base, "z")
        os.makedirs(g)
        os.makedirs(z)

        print(f"building a {N}-commit linear history with one branch point\n")

        # git side
        sh(["git", "init", "-q", "-b", "main"], g)
        sh(["git", "config", "user.email", "t@t"], g)
        sh(["git", "config", "user.name", "t"], g)
        sh(["git", "config", "core.commitGraph", "true"], g)
        sh(["git", "config", "gc.commitGraph", "true"], g)

        # gesta side
        sh([ZF, "init"], z)

        # Both trees get the same commits in the same order, so the histories are
        # the same shape and the comparison is about the question, not the data.
        branch_at = N // 2
        for i in range(N):
            body = f"line {i} of the only file\n"
            for tree in (g, z):
                with open(os.path.join(tree, "f.txt"), "w") as fh:
                    fh.write(body)
            sh(["git", "add", "-A"], g)
            sh(["git", "commit", "-qm", f"c{i}"], g)

            made = sh([ZF, "changeset", "create", f"c{i}", "--author", "t", "--json"], z)
            try:
                cid = json.loads(made.stdout).get("changeset")
            except json.JSONDecodeError:
                cid = None
            if cid:
                sh([ZF, "changeset", "commit", cid], z)

            if i == branch_at:
                sh(["git", "branch", "side"], g)
                sh([ZF, "branch", "side"], z)

        # Git's own index for ancestry. Without this the comparison would be against
        # a git that has not built what it builds in production.
        sh(["git", "commit-graph", "write", "--reachable"], g)

        oldest = sh(["git", "rev-list", "--max-parents=0", "HEAD"], g).stdout.strip()
        head = sh(["git", "rev-parse", "HEAD"], g).stdout.strip()
        side_tip = sh(["git", "rev-parse", "side"], g).stdout.strip()

        # `log` takes a limit because a transcript has to stay bounded; the bench
        # needs the whole history to pick its endpoints.
        z_oldest = sh([ZF, "log", "--json", "--limit", str(N + 10)], z)
        try:
            ids = [row["id"] for row in json.loads(z_oldest.stdout)]
        except (json.JSONDecodeError, KeyError, TypeError):
            print("  could not read gesta's log; the shape of `log --json` may have changed")
            print(z_oldest.stdout[:400])
            return 1
        if len(ids) < branch_at + 1:
            print(f"  gesta recorded {len(ids)} of {N} changesets")
            return 1
        z_head = ids[0]
        z_oldest_id = ids[-1]
        # The branch `side` was created at the same point on both sides.
        z_side = ids[len(ids) - 1 - branch_at]

        print(f"  {N} commits on each side, one branch at {branch_at}\n")

        # 1. Range between two points. `bisect start` resolves both ancestries and
        #    subtracts them, which is exactly the walk under test, and it is a
        #    command an agent runs rather than an internal function.
        def gesta_range():
            made = sh([ZF, "bisect", "start", z_side, z_head], z)
            if made.returncode != 0 and "no changeset between" not in (made.stdout + made.stderr):
                # A leftover session from the previous run is the normal case; clear and retry.
                sh([ZF, "bisect", "clear"], z)
                sh([ZF, "bisect", "start", z_side, z_head], z)

        record(
            "range (side -> head)",
            timed(lambda: sh(["git", "rev-list", "--count", f"side..HEAD"], g)),
            timed(gesta_range),
        )

        # 2. History of one path. git reads a tree per commit it walks; a store with
        #    no path index reads every changeset's file list.
        record(
            "path history (f.txt)",
            timed(lambda: sh(["git", "log", "--oneline", "--", "f.txt"], g)),
            timed(lambda: sh([ZF, "history", "f.txt"], z)),
        )

        # 3. A string in the tree. Cheap on both sides, and the control: if this one
        #    is also slow, the gap is not about history.
        record(
            "grep tree (needle)",
            timed(lambda: sh(["git", "grep", "-c", "needle", "--", "f.txt"], g)),
            timed(lambda: sh([ZF, "grep", "needle"], z)),
        )

        print()
        worst = max(results, key=lambda r: r[3])
        print(f"worst question: {worst[0]} at {worst[3]:.2f}x")
        print()
        # These are whole CLI invocations, so each figure contains the cost of starting a
        # process. That is the number a user feels, and it is also why a ratio here says more
        # about startup than about the algorithm: `grep tree` has no history question in it and
        # still shows a ratio, which is how much of every row is startup.
        print("each figure is a whole command, so it includes process startup.")
        print("read the grep row as the startup floor: whatever it shows is in every other row.")
        print("the ancestry walk is covered by a statement-count test, not a timer.")
        return 0
    finally:
        shutil.rmtree(base, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
