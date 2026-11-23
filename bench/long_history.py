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


def commit_one(root, i, path_name="f.txt"):
    """One commit touching one file, timed in its two phases."""
    path = os.path.join(root, path_name)
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


PROBE = "probe.txt"
PROBE_LINES = 20


def measure(root, reps=15):
    """Fastest commit of a fixed-size file at the current history size.

    Two properties are easy to confuse here and the gate needs them separate:

    * cost depends on the history, which is what "flat as the history grows" is about;
    * cost depends on the size of the file being committed, which `docs/speed.md` records as the one
      place this design is quadratic where git is not.

    Appending one line per commit changes both at once - the history grows and so does the file -
    so a slope in that measurement cannot be attributed to either. This writes a fixed twenty line
    file every time, so only the history changes. The append-shaped measurement is still worth
    having and lives in `bench/file_growth.py`.
    """
    writes, scans, errs = [], [], []
    for i in range(reps):
        with open(os.path.join(root, PROBE), "w") as fh:
            for line in range(PROBE_LINES):
                fh.write(f"probe {i} line {line}\n")
        w, s, err = commit_one(root, f"m{i}_{time.time_ns()}", PROBE)
        scans.append(s)
        if w is None:
            errs.append(err)
        else:
            writes.append(w)
    return (
        min(writes) if writes else None,
        min(scans) if scans else None,
        errs,
    )


def startup_ms(root, reps=9):
    """Cost of a command that opens the store and does almost nothing.

    `log --limit 1` reads the schema, migrates, and prints one row: it pays process start, the
    connection, and whatever `migrate` decides to do, and it pays nothing for the commit path. That
    is exactly the fixed cost a commit also pays, so subtracting it is what separates the work from
    the process. Without the subtraction the ratio is dominated by startup at small history sizes,
    where the work is a few milliseconds and the process is thirty.
    """
    best = None
    for _ in range(reps):
        t0 = time.perf_counter()
        r = run(f"{ZF} log --limit 1", cwd=root)
        dt = (time.perf_counter() - t0) * 1000
        if r.returncode == 0 and (best is None or dt < best):
            best = dt
    return best


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
    print(f"{'commits':>8}  {'write ms':>9}  {'scan ms':>8}  {'startup':>8}  {'db MB':>7}")
    print("-" * 48)

    for target in SAMPLES:
        if target > max_n:
            break
        # Build up to the target, without timing: this is the setup, and timing it would report the
        # cost of the setup.
        while made < target:
            made += 1
            # Each setup commit writes its own file and the file is deleted after the commit, so the
            # working tree the measurement runs in holds only `probe.txt`.
            #
            # This is not tidiness. `changeset create` scans and hashes every file in the tree, so a
            # setup that left one growing file behind made `create` cost 35 ms at 100 commits and
            # 107 ms at 1000 - a 3x slope with the measured commit unchanged at 1.3 ms against 1.5 ms
            # inside the process. The bench reported that as the cost of a commit growing with the
            # history.
            name = f"hist{made}.txt"
            with open(os.path.join(ROOT, name), "w") as fh:
                fh.write(f"line {made}\n")
            w, _s, err = commit_one(ROOT, f"b{made}", name)
            try:
                os.unlink(os.path.join(ROOT, name))
            except OSError:
                pass
            if w is None and made % 500 == 0:
                print(f"  commit {made} failed: {err}")
                return 1
            if made % 500 == 0:
                print(f"  built {made}/{target}", flush=True)

        w, s, errs = measure(ROOT)
        base = startup_ms(ROOT)
        db = os.path.getsize(os.path.join(ROOT, ".gesta", "gesta.db"))
        results.append((target, w, s, db, base))
        print(
            f"{target:>8}  {w if w is not None else -1:>9.1f}  {s if s is not None else -1:>8.1f}  "
            f"{(base if base is not None else -1):>8.1f}  {db / 1048576:>7.1f}"
        )
        if errs:
            print(f"  a commit failed: {errs[0][:200]}")

    print()
    if len(results) < 2:
        print("not enough samples to say anything")
        return 3

    first, last = results[0], results[-1]
    if first[1] and last[1]:
        # A `startup` column is printed but is not subtracted, and that is deliberate. Subtracting a
        # baseline only helps when the baseline is smaller and steadier than the signal; here startup
        # ranged 25 to 88 ms while the work at the small end was a few milliseconds, so the
        # subtraction produced work times that were negative in one run and 27x in another. A
        # correction that its own noise can invert is worse than no correction.
        #
        # What does help is the minimum of several runs: a shared machine only ever adds time, so the
        # fastest run is the closest reading of the work. See `measure`.
        grew = last[0] / first[0]
        ratio = last[1] / first[1]
        print(
            f"history grew {grew:.0f}x ({first[0]} -> {last[0]} commits); "
            f"a commit costs {ratio:.2f}x ({first[1]:.1f} -> {last[1]:.1f} ms)"
        )
        # Flat means the ratio tracks the history at most weakly. Two things set this number. The
        # instrument: minima of nine runs on an unchanged repository, which read 1.61x to 1.78x over
        # five consecutive runs once the schema-check cost was removed. And the shape of the answer: a
        # commit on a twenty line file has a few milliseconds of work at either end of this range, so
        # the ratio is mostly startup, and startup on a shared machine swings by tens of milliseconds.
        #
        # 2.5x sits above that swing and below the 3.0x the old medians could not defend. A commit
        # that really did get more expensive with the history - the 5.9x this gate first reported -
        # lands outside it by a wide margin.
        if ratio <= 2.5:
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
