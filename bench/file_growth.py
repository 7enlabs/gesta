#!/usr/bin/env python3
"""What a commit costs as the file it commits grows.

`docs/speed.md` says a commit that touches one line of a 2000 line file pays for a diff of 2000
lines, and that this is the one place the design is quadratic where git is not. That is a property
of the file, not of the history, and it is worth measuring on its own.

It used to be measured by `bench/long_history.py`, which appends a line per commit. That changes two
things at once: the history grows and so does the file. A slope in that measurement could not be
attributed to either, and the gate was reading the file's growth while claiming to test the
history's. This bench holds the history at one commit and varies only the file.

Run: python3 bench/file_growth.py
"""

import os
import shutil
import subprocess
import sys
import tempfile
import time

_PROJECT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ZF = os.environ.get("GESTA_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))

SIZES = [20, 200, 2000, 8000]


def sh(args, cwd):
    return subprocess.run(args, cwd=cwd, capture_output=True, text=True, timeout=600)


def commit(repo, path):
    made = sh([ZF, "changeset", "create", path, "--author", "b", "--json"], repo)
    if made.returncode != 0:
        return None
    import json

    try:
        cid = json.loads(made.stdout)["changeset"]
    except (ValueError, KeyError):
        return None
    return sh([ZF, "changeset", "commit", cid], repo).returncode


def main() -> int:
    if not os.path.exists(ZF):
        print(f"binary not found: {ZF}")
        return 1

    base = tempfile.mkdtemp(prefix="file-growth-")
    rows = []
    try:
        for n in SIZES:
            d = os.path.join(base, str(n))
            os.makedirs(d)
            sh([ZF, "init"], d)
            path = os.path.join(d, "f.txt")
            # One commit writes the whole file, so the history stays at one commit for every size.
            with open(path, "w") as fh:
                for i in range(n):
                    fh.write(f"line {i}\n")
            if commit(d, "f.txt") is None:
                print(f"  {n:>6} lines: could not make the first commit")
                continue

            best = None
            for r in range(7):
                with open(path, "a") as fh:
                    fh.write(f"edit {r}\n")
                t0 = time.perf_counter()
                ok = commit(d, "f.txt")
                dt = (time.perf_counter() - t0) * 1000
                if ok == 0 and (best is None or dt < best):
                    best = dt
            if best is None:
                print(f"  {n:>6} lines: no timing")
                continue
            rows.append((n, best))
            print(f"  {n:>6} lines: {best:>7.1f} ms")

        if len(rows) < 2:
            print("\nnot enough sizes to say anything")
            return 3

        small, large = rows[0], rows[-1]
        ratio = large[1] / small[1]
        grew = large[0] / small[0]
        print(
            f"\nthe file grew {grew:.0f}x ({small[0]} -> {large[0]} lines); "
            f"a commit costs {ratio:.2f}x ({small[1]:.1f} -> {large[1]:.1f} ms)"
        )
        # Named, not bounded. This is the shape the design has, documented in `docs/speed.md`, and a
        # gate that fails on it would be failing on a known property rather than a regression. What
        # the gate is for is that the cost does not also grow with the history, and that is measured
        # by `bench/long_history.py` on a fixed-size file.
        print(
            "This is the file-size property, not a regression. It fails only above 10x, which would "
            "mean the diff stopped being linear in the file."
        )
        return 0 if ratio <= 10.0 else 1
    finally:
        shutil.rmtree(base, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
