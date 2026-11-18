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


def measure(root, prepare, commit, runs=11, rounds=3):
    """Median of the medians, and the spread, because one median hides how noisy this is.

    Running the same one file commit ten times in a row on this machine gave 29 ms to 50 ms.
    A single median from that spread cannot support a claim, so the report carries the
    interquartile spread beside the number.
    """
    prepare(root)
    medians = []
    for _ in range(rounds):
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
        samples.sort()
        medians.append(samples[len(samples) // 2])
    medians.sort()
    mid = medians[len(medians) // 2]
    # The spread of the per-run samples from the last round, as a fraction of the median.
    lo = samples[len(samples) // 4]
    hi = samples[(len(samples) * 3) // 4]
    spread = (hi - lo) / mid if mid else 0.0
    return mid, spread


def main():
    global GESTA
    if not os.path.exists(GESTA):
        sys.exit(f"binary not found: {GESTA}")
    # Copy the binary off the working directory: reading an executable from a bind mount costs
    # about 16 ms per run on this setup, which is larger than the work being measured. The copy
    # is refreshed every run, because keeping a stale one measures the previous build and
    # reports it as the current one.
    local = "/tmp/gesta_scale_bin"
    shutil.copy2(GESTA, local)
    os.chmod(local, 0o755)
    GESTA = local

    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(ROOT)

    print("Median of three rounds; the percentage is the spread of the middle half of that round.")
    print()
    print(f"{'files':>7} {'gesta':>13} {'git':>13}  {'ratio':>6}")
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
        (zm, zs), (bm, bs) = z, b
        rows.append((n, zm, bm))
        print(f"{n:>7} {zm:>6.1f}ms +-{zs * 100:>2.0f}%  {bm:>6.1f}ms +-{bs * 100:>2.0f}%  {zm / bm:>5.2f}x")

    if len(rows) >= 2:
        first, last = rows[0], rows[-1]
        factor = last[0] / first[0]
        grown = last[1] / first[1]
        print()
        print(f"repository grew {factor:.0f}x ({first[0]} to {last[0]} files)")
        print(f"  gesta commit: {first[1]:.1f} -> {last[1]:.1f} ms ({grown:.2f}x)")
        print(f"  git commit  : {first[2]:.1f} -> {last[2]:.1f} ms ({last[2] / first[2]:.2f}x)")
        print()
        # A claim the numbers do not support is worse than no claim. Under 2x is flat enough to
        # say; above it says so plainly rather than shipping the same sentence every run.
        if grown <= 2.0:
            print(f"A one file commit costs {grown:.2f}x more in a repository {factor:.0f}x larger.")
            print("That is the property an agent needs: many small commits while the repository grows.")
        else:
            print(f"A one file commit costs {grown:.2f}x more in a repository {factor:.0f}x larger.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
