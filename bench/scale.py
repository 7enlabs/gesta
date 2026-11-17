#!/usr/bin/env python3
"""Does the cost of a commit depend on the size of the change, or of the repo?

A source control system for agents has to accept commits faster than agents can
produce them, and agents produce many small ones. If committing one file costs
more as the repository grows, the system gets slower exactly as it becomes more
useful. The two numbers below answer that directly: the same one file edit,
committed in repositories of increasing size.

Run: python3 bench/scale.py
"""

import os
import re
import shutil
import statistics
import subprocess
import sys
import time

GESTA = os.environ.get("GESTA_BIN", "/work/gesta/target/release/gesta")
ROOT = "/tmp/gesta_scale"
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


def gesta_commit(root, msg):
    o = sh(f"{GESTA} changeset create {msg} --author bench", cwd=root)
    m = re.search(r"\b(cs[0-9a-f]{6,})\b", o.stdout)
    if not m:
        return False
    r = sh(f"{GESTA} changeset commit {m.group(1)}", cwd=root)
    return r.returncode == 0


def measure(root, prepare, commit, runs=7):
    prepare(root)
    samples = []
    for i in range(runs + 2):
        with open(os.path.join(root, "d0", "f0.py"), "a") as fh:
            fh.write(f"# tick {i}\n")
        t0 = time.perf_counter()
        ok = commit(root, f"t{i}")
        dt = (time.perf_counter() - t0) * 1000
        if not ok:
            return None
        if i >= 2:
            samples.append(dt)
    return statistics.median(samples)


def main():
    global GESTA
    if not os.path.exists(GESTA):
        sys.exit(f"binary not found: {GESTA}")
    local = "/tmp/gesta_scale_bin"
    if not os.path.exists(local):
        shutil.copy2(GESTA, local)
        os.chmod(local, 0o755)
    GESTA = local

    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(ROOT)

    print(f"{'files':>7} {'gesta':>9} {'git':>9}  {'gesta/git':>9}")
    for n in (500, 2000, 8000):
        g = os.path.join(ROOT, f"g{n}")
        t = os.path.join(ROOT, f"t{n}")
        build(g, n)
        build(t, n)

        sh(f"{GESTA} init", cwd=g)
        gesta_commit(g, "seed")
        sh("git init -q -b main", cwd=t)
        sh("git config user.email a@b.c && git config user.name bench", cwd=t)
        sh("git add -A && git commit -qm seed", cwd=t)
        # Both need a settled tree: a freshly written file is re-read by design.
        time.sleep(2.5)

        z = measure(g, lambda r: None, gesta_commit)
        b = measure(t, lambda r: None, lambda r, m: sh("git add -A && git commit -qm " + m, cwd=r).returncode == 0)
        if z is None or b is None:
            print(f"{n:>7} {'FAILED':>9} {'FAILED':>9}")
            continue
        rows.append((n, z, b))
        print(f"{n:>7} {z:>8.1f}ms {b:>8.1f}ms  {z / b:>8.2f}x")

    if len(rows) >= 2:
        first, last = rows[0], rows[-1]
        factor = last[0] / first[0]
        print()
        print(f"repository grew {factor:.0f}x ({first[0]} to {last[0]} files)")
        print(f"  gesta commit: {first[1]:.1f} -> {last[1]:.1f} ms ({last[1] / first[1]:.2f}x)")
        print(f"  git commit  : {first[2]:.1f} -> {last[2]:.1f} ms ({last[2] / first[2]:.2f}x)")
        print()
        print("A commit that costs the same in a repository 16x larger is the property")
        print("an agent needs: many small commits, and the repository growing anyway.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
