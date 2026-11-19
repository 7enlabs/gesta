#!/usr/bin/env python3
"""Does the cost of a commit stay flat over a long history?

`scale.py` measures growth in the number of files. This measures growth in the number of commits,
which is the other axis and the one a daily user crosses: a repository used for a year has a long
history and a modest tree.

The claim under test is the project's central one: the cost of a commit follows the size of the
change, not the size of the repository. A history that makes every commit slower would make the tool
worse the longer it is used, which is the failure mode this measures.

Each sample is the median of several commits made after the history has reached that size, so a
single slow run does not move the answer. The scan is measured apart from the write, because only the
write is the claim; see `scale.py` for why.

Run: python3 bench/long_history.py [--max N]
"""

import os
import shutil
import statistics
import subprocess
import sys
import time

_HERE = os.path.dirname(os.path.abspath(__file__))
_PROJECT = os.path.dirname(_HERE)
ZF = os.environ.get("ZF_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))
ROOT = "/tmp/gesta_long_history"

# The sizes to sample. Between them the history grows tenfold twice, which is enough to see a slope
# and not so many points that the run takes an afternoon.
SAMPLES = [100, 1000, 10000]


def run(args, cwd=None, timeout=900):
    return subprocess.run(args, shell=True, cwd=cwd, capture_output=True, text=True, timeout=timeout)


def commit_one(root, i):
    """One commit touching one file, timed in its two phases."""
    path = os.path.join(root, "f.txt")
    with open(path, "a") as fh:
        fh.write(f"line {i}\n")

    t0 = time.perf_counter()
    out = run(f'{ZF} changeset create "c{i}" --author b --json', cwd=root)
    t_scan = (time.perf_counter() - t0) * 1000

    cid = ""
    if out.returncode == 0:
        import json
        try:
            doc = json.loads(out.stdout)
            cid = doc.get("changeset") or ""
        except Exception:
            cid = ""
    if not cid:
        # No change to record, or the create failed. Either way there is nothing to commit.
        return None, t_scan, out.stdout[-300:] + out.stderr[-300:]

    t0 = time.perf_counter()
    out2 = run(f"{ZF} changeset commit {cid}", cwd=root)
    t_write = (time.perf_counter() - t0) * 1000
    return (t_write if out2.returncode == 0 else None), t_scan, out2.stdout[-300:] + out2.stderr[-300:]


def measure(root, reps=5):
    """Median write and scan over `reps` commits at the current history size."""
    writes, scans, errs = [], [], []
    for i in range(reps):
        w, s, err = commit_one(root, f"m{i}_{time.time_ns()}")
        scans.append(s)
        if w is None:
            errs.append(err)
        else:
            writes.append(w)
    return (
        statistics.median(writes) if writes else None,
        statistics.median(scans) if scans else None,
        errs,
    )


def main():
    max_n = 10000
    if "--max" in sys.argv:
        max_n = int(sys.argv[sys.argv.index("--max") + 1])

    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(ROOT, exist_ok=True)
    r = run(f"{ZF} init", cwd=ROOT)
    if r.returncode != 0:
        print(f"init failed: {r.stdout}{r.stderr}")
        return 1

    with open(os.path.join(ROOT, "f.txt"), "w") as fh:
        fh.write("start\n")

    results = []
    made = 0
    print(f"{'commits':>8}  {'write ms':>9}  {'scan ms':>8}  {'db MB':>7}")
    print("-" * 40)

    for target in SAMPLES:
        if target > max_n:
            break
        # Build up to the target, without timing: this is the setup, and timing it would report the
        # cost of the setup.
        while made < target:
            made += 1
            w, _s, err = commit_one(ROOT, f"b{made}")
            if w is None and made % 500 == 0:
                print(f"  commit {made} failed: {err}")
                return 1
            if made % 500 == 0:
                print(f"  built {made}/{target}", flush=True)

        w, s, errs = measure(ROOT)
        db = os.path.getsize(os.path.join(ROOT, ".gesta", "gesta.db"))
        results.append((target, w, s, db))
        print(
            f"{target:>8}  {w if w is not None else -1:>9.1f}  {s if s is not None else -1:>8.1f}  "
            f"{db / 1048576:>7.1f}"
        )
        if errs:
            print(f"  a commit failed: {errs[0][:200]}")

    print()
    if len(results) < 2:
        print("not enough samples to say anything")
        return 3

    first, last = results[0], results[-1]
    if first[1] and last[1]:
        ratio = last[1] / first[1]
        grew = last[0] / first[0]
        print(
            f"history grew {grew:.0f}x ({first[0]} -> {last[0]} commits); "
            f"a commit costs {ratio:.2f}x ({first[1]:.1f} -> {last[1]:.1f} ms)"
        )
        # Flat means the ratio tracks the history at most weakly. Doubling the cost over a hundredfold
        # history is a slope worth naming; staying inside 3x is what "flat" has to mean in practice.
        if ratio <= 3.0:
            print("the cost is flat over this range")
        else:
            print(f"the cost is NOT flat: {ratio:.2f}x over {grew:.0f}x history")
            return 1
    else:
        print("a sample produced no timing")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
