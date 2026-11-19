#!/usr/bin/env python3
"""How much does rebuilding the branch tree cost, at each repository size?

`set_branch_head` updates the tree incrementally on the common path and rebuilds it when the
changeset re-parents, which is what a merge, an undo and a remote apply all do. So the rebuild
still runs in normal use, and its cost is what bounds those operations.

This measures the rebuild directly by forcing the fallback: a commit whose parent is not the
previous head takes the rebuild path. The number answers "what does a merge pay", which is a
different question from `bench/scale.py`'s "what does an ordinary commit pay".

Run: python3 bench/rebuild_cost.py
"""

import os
import re
import shutil
import statistics
import subprocess
import sys
import time

# Found relative to this file. A benchmark that only runs in one container is not a
# measurement another machine can repeat.
_PROJECT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
GESTA = os.environ.get("GESTA_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))
ROOT = "/tmp/gesta_rebuild"
rows = []


def sh(cmd, cwd=None):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True, cwd=cwd)


def build(root, n):
    shutil.rmtree(root, ignore_errors=True)
    os.makedirs(root)
    for i in range(n):
        d = os.path.join(root, f"d{i // 200}")
        os.makedirs(d, exist_ok=True)
        with open(os.path.join(d, f"f{i}.py"), "w") as fh:
            fh.write(f"def f{i}():\n    return {i}\n")


def commit(root, msg):
    o = sh(f"{GESTA} changeset create {msg} --author bench", cwd=root)
    m = re.search(r"\b(cs[0-9a-f]{6,})\b", o.stdout)
    if not m:
        return False
    return sh(f"{GESTA} changeset commit {m.group(1)}", cwd=root).returncode == 0


def tree_rows(root):
    """Rows in the branch tree and how many branches there are, read from the database."""
    import sqlite3

    db = os.path.join(root, ".gesta", "gesta.db")
    if not os.path.exists(db):
        db = os.path.join(root, ".zenforge", "zenforge.db")
    c = sqlite3.connect(db)
    try:
        rows = c.execute("SELECT COUNT(*) FROM branch_tree").fetchone()[0]
        branches = c.execute("SELECT COUNT(*) FROM branches").fetchone()[0]
        return f"{rows} rows / {branches} br"
    finally:
        c.close()


def measure_rebuild(root, rounds=3, runs=9):
    """Time the tree rebuild alone, in a repository that holds what it started with.

    Three wrong measurements came before this one, and each is worth knowing:

    `undo` rebuilds the tree and then writes every path back to disk, so at 8000 files it timed
    the file system. Editing the same file on both sides made the land a conflict. And creating
    a fresh branch per sample grew `branch_tree` by one row per file per branch, so the table
    the rebuild reads was getting bigger while the repository was not.

    The measurement keeps one side branch, reuses it, and puts back the state it found, so the
    repository at the end of a sample is the repository at the start of the next one.
    """
    medians = []
    spread = 0.0
    sh(f"{GESTA} branch create side", cwd=root)
    for _ in range(rounds):
        samples = []
        for i in range(runs + 2):
            sh(f"{GESTA} branch switch side", cwd=root)
            # A different file each time, so no sample conflicts with the one before it.
            target = os.path.join(root, "d0", f"f{i % 199}.py")
            with open(target, "a") as fh:
                fh.write(f"# tick {i}\n")
            if not commit(root, f"r{i}"):
                print(f"    commit failed at {os.path.basename(root)} round {i}")
                return None
            # The switch writes every file, so it happens outside the timer. Timing it gave a
            # curve that described the file system: the fourth wrong version of this.
            sh(f"{GESTA} branch switch main", cwd=root)
            t0 = time.perf_counter()
            landed = sh(f"{GESTA} land side --onto main", cwd=root)
            dt = (time.perf_counter() - t0) * 1000
            if landed.returncode != 0:
                print(f"    land failed: {(landed.stdout + landed.stderr).strip()[:200]}")
                return None
            # Put the side branch back where it started, so the next sample is the same job.
            sh(f"{GESTA} branch switch side && {GESTA} undo", cwd=root)
            if i >= 2:
                samples.append(dt)
        samples.sort()
        medians.append(samples[len(samples) // 2])
        spread = (samples[(len(samples) * 3) // 4] - samples[len(samples) // 4]) / medians[-1]
    medians.sort()
    return medians[len(medians) // 2], spread


def main():
    global GESTA
    if not os.path.exists(GESTA):
        sys.exit(f"binary not found: {GESTA}")
    # Read the binary from the machine's own disk: a bind mount costs about 16 ms per run,
    # which is larger than the work being measured.
    local = "/tmp/gesta_rebuild_bin"
    shutil.copy2(GESTA, local)
    os.chmod(local, 0o755)
    GESTA = local

    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(ROOT)

    print("Cost of one operation that has to rebuild the branch tree from history.")
    print("Median of three rounds; the percentage is the spread of the middle half.")
    print()
    print(f"{'files':>7} {'branch_tree':>18} {'land':>14}")
    for n in (500, 2000, 8000):
        g = os.path.join(ROOT, f"g{n}")
        build(g, n)
        sh(f"{GESTA} init", cwd=g)
        commit(g, "seed")
        time.sleep(2.5)
        got = measure_rebuild(g)
        if got is None:
            print(f"{n:>7} {'FAILED':>12} {'FAILED':>14}")
            continue
        ms, spread = got
        rows.append((n, ms))
        print(f"{n:>7} {tree_rows(g):>18} {ms:>8.1f}ms +-{spread * 100:>3.0f}%")

    if len(rows) >= 2:
        first, last = rows[0], rows[-1]
        print()
        print(f"repository grew {last[0] / first[0]:.0f}x ({first[0]} to {last[0]} files)")
        print(f"  rebuild: {first[1]:.1f} -> {last[1]:.1f} ms ({last[1] / first[1]:.2f}x)")
        print()
        print("This is the cost a merge, a land and a remote apply pay, and the reason the")
        print("common path does not rebuild.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
