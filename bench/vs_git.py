#!/usr/bin/env python3
"""Head to head with git on the operations an agent actually repeats.

An agent does three things thousands of times a day: check status, diff, and
commit. Raw throughput on those decides whether ZenForge is usable, and any
comparison that measures something else is decoration.

Run: python3 bench/vs_git.py
"""

import os
import re
import shutil
import subprocess
import statistics
import sys
import time

ZF = os.environ.get("ZF_BIN", "/work/zenforge/target/release/zenforge")
# The benchmark must not run on a bind mount. /work is a 9p drvfs share of a
# Windows drive, where every read crosses the container boundary, and a 3.7 MB
# binary then takes 7 ms to page in instead of 0.9 ms. Measuring there blames the
# tool for the transport. ROOT is overridable so the suite can be pointed at any
# fast local path.
ROOT = os.environ.get("ZF_BENCH_ROOT", "/tmp/zf_vs_git_bench")

# Copy the binary to the same local filesystem as the work it measures.
LOCAL_ZF = "/tmp/zenforge_bench_bin"
if os.path.exists(ZF) and not os.path.exists(LOCAL_ZF):
    shutil.copy2(ZF, LOCAL_ZF)
    os.chmod(LOCAL_ZF, 0o755)
if os.path.exists(LOCAL_ZF):
    ZF = LOCAL_ZF
results = []


def record(name, zf_ms, git_ms, detail=""):
    ok = zf_ms is not None and git_ms is not None
    results.append((name, zf_ms, git_ms))
    if not ok:
        print(f"  [SKIP] {name}: {detail}")
        return
    ratio = zf_ms / git_ms if git_ms else 0
    verdict = "faster" if ratio < 1 else "slower"
    print(f"  {name:34} zenforge {zf_ms:8.1f} ms | git {git_ms:8.1f} ms | {ratio:5.2f}x {verdict}")


def sh(cmd, cwd=None):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True, cwd=cwd)


def timeit(cmd, cwd, runs=7):
    """Median of N runs after two warmups.

    One warmup is not enough for git: the first `git status` after a commit
    reads and writes the index, and the second one is 3x faster. Benchmarking
    against that first number reported git as 6x faster than it is on a repeated
    call, which is the case that matters for an agent polling status.
    """
    sh(cmd, cwd)
    sh(cmd, cwd)
    samples = []
    for _ in range(runs):
        t0 = time.perf_counter()
        r = sh(cmd, cwd)
        dt = (time.perf_counter() - t0) * 1000
        if r.returncode != 0:
            return None
        samples.append(dt)
    return statistics.median(samples)


def build_tree(root, n_files):
    shutil.rmtree(root, ignore_errors=True)
    os.makedirs(root)
    for i in range(n_files):
        d = os.path.join(root, f"pkg{i // 100}")
        os.makedirs(d, exist_ok=True)
        with open(os.path.join(d, f"f{i}.py"), "w") as fh:
            fh.write(f"def f{i}():\n    return {i}\n")


def commit_all_zf(root):
    o = sh(f"{ZF} changeset create snap --author bench", cwd=root)
    m = re.search(r"\b(cs[0-9a-f]{6,})\b", o.stdout)
    if m:
        sh(f"{ZF} changeset commit {m.group(1)}", cwd=root)


def main():
    if not os.path.exists(ZF):
        sys.exit(f"binary not found: {ZF}")
    print(f"binary: {ZF}")
    print(f"work:   {ROOT}")
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(ROOT)

    for n in (500, 2000):
        print(f"\n=== {n} files ===")
        zf_dir = os.path.join(ROOT, f"zf{n}")
        gt_dir = os.path.join(ROOT, f"git{n}")
        build_tree(zf_dir, n)
        build_tree(gt_dir, n)

        # git
        sh("git init -q -b main", cwd=gt_dir)
        sh("git config user.email a@b.c", cwd=gt_dir)
        sh("git config user.name bench", cwd=gt_dir)
        sh("git add -A && git commit -qm init", cwd=gt_dir)
        # zenforge
        sh(f"{ZF} init", cwd=zf_dir)
        commit_all_zf(zf_dir)

        # status on a clean tree
        z = timeit(f"{ZF} status", zf_dir)
        g = timeit("git status --porcelain", gt_dir)
        record(f"status, {n} files clean", z, g)

        # status with one file edited
        for d in (zf_dir, gt_dir):
            p = os.path.join(d, "pkg0", "f0.py")
            with open(p, "a") as fh:
                fh.write("# edit\n")
        z = timeit(f"{ZF} status", zf_dir)
        g = timeit("git status --porcelain", gt_dir)
        record(f"status, 1 file edited", z, g)

        # diff one file
        z = timeit(f"{ZF} diff", zf_dir)
        g = timeit("git diff", gt_dir)
        record("diff, 1 file", z, g)

        # commit one change
        def bench_zf_commit():
            samples = []
            for _ in range(5):
                p = os.path.join(zf_dir, "pkg0", "f0.py")
                with open(p, "a") as fh:
                    fh.write("# tick\n")
                t0 = time.perf_counter()
                commit_all_zf(zf_dir)
                samples.append((time.perf_counter() - t0) * 1000)
            return statistics.median(samples)

        def bench_git_commit():
            samples = []
            for _ in range(5):
                p = os.path.join(gt_dir, "pkg0", "f0.py")
                with open(p, "a") as fh:
                    fh.write("# tick\n")
                t0 = time.perf_counter()
                sh("git add -A && git commit -qm tick", cwd=gt_dir)
                samples.append((time.perf_counter() - t0) * 1000)
            return statistics.median(samples)

        record("commit, 1 file", bench_zf_commit(), bench_git_commit())

        # log
        z = timeit(f"{ZF} log --limit 10", zf_dir)
        g = timeit("git log --oneline -10", gt_dir)
        record("log, last 10", z, g)

        # blame one file
        z = timeit(f"{ZF} blame pkg0/f0.py", zf_dir)
        g = timeit("git blame pkg0/f0.py", gt_dir)
        record("blame, small file", z, g)

    print()
    wins = sum(1 for _, z, g in results if z and g and z < g)
    total = sum(1 for _, z, g in results if z and g)
    print(f"zenforge faster on {wins}/{total} operations")
    return 0


if __name__ == "__main__":
    sys.exit(main())
