#!/usr/bin/env python3
"""Compare history size against a baseline on a file edited many times.

Both systems get the same history: a 558 KB file, one line changed, fifty
commits. git is run in the same directory tree so both see the same bytes on
disk. This is the case delta compression exists for.

Run: python3 bench/history_size.py
"""

import os
import shutil
import subprocess
import sys

ZF = os.environ.get("ZF_BIN", "/work/gesta/target/release/gesta")
ROOT = "/tmp/zf_vs_git"
EDIT_COUNT = 50
results = []


def record(name, ok, detail=""):
    results.append((name, ok, detail))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}")
    if not ok and detail:
        for line in str(detail).split("\n")[:4]:
            print(f"         {line}")


def run(cmd, cwd=None):
    r = subprocess.run(cmd, capture_output=True, text=True, cwd=cwd)
    return r.returncode, (r.stdout + r.stderr).strip()


def size_of(path):
    total = 0
    if os.path.isfile(path):
        return os.path.getsize(path)
    for dirpath, _, files in os.walk(path):
        for f in files:
            try:
                total += os.path.getsize(os.path.join(dirpath, f))
            except OSError:
                pass
    return total


def build_history(d):
    lines = ["line %05d: %s" % (i, "x" * 80) for i in range(6000)]
    with open(os.path.join(d, "big.txt"), "w") as fh:
        fh.write("\n".join(lines) + "\n")
    for v in range(1, EDIT_COUNT + 1):
        lines[3000] = "line 03000: CHANGED %d %s" % (v, "x" * 80)
        with open(os.path.join(d, "big.txt"), "w") as fh:
            fh.write("\n".join(lines) + "\n")
        yield v


def main():
    if not os.path.exists(ZF):
        sys.exit(f"binary not found: {ZF}")
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(ROOT)

    # gesta run.
    zf_dir = os.path.join(ROOT, "zf")
    os.makedirs(zf_dir)
    run([ZF, "init"], cwd=zf_dir)
    run([ZF, "changeset", "create", "v0", "--author", "a"], cwd=zf_dir)
    out = run([ZF, "changeset", "list"], cwd=zf_dir)[1]
    for w in out.split():
        if w.startswith("cs"):
            run([ZF, "changeset", "commit", w], cwd=zf_dir)
            break
    for v in build_history(zf_dir):
        out = run([ZF, "changeset", "create", f"v{v}", "--author", "a"], cwd=zf_dir)[1]
        for w in out.split():
            if w.startswith("cs"):
                run([ZF, "changeset", "commit", w], cwd=zf_dir)
                break
    g_bytes = size_of(os.path.join(zf_dir, ".gesta", "gesta.db"))
    zf_stats = run([ZF, "stats"], cwd=zf_dir)[1]

    # git run, same bytes.
    git_dir = os.path.join(ROOT, "git")
    os.makedirs(git_dir)
    run(["git", "init", "-q", "-b", "main"], cwd=git_dir)
    run(["git", "config", "user.email", "a@example.com"], cwd=git_dir)
    run(["git", "config", "user.name", "a"], cwd=git_dir)
    for v in build_history(git_dir):
        run(["git", "add", "big.txt"], cwd=git_dir)
        run(["git", "commit", "-q", "-m", f"v{v}"], cwd=git_dir)
    git_bytes = size_of(os.path.join(git_dir, ".git"))

    ratio = g_bytes / git_bytes if git_bytes else 0
    print(f"gesta database: {g_bytes/1024:.0f} KB")
    print(f"git .git:          {git_bytes/1024:.0f} KB")
    print(f"ratio:             {ratio:.2f}x git")
    for line in zf_stats.split("\n"):
        if "objects:" in line or "deltas:" in line:
            print(f"  {line.strip()}")

    record("the delta keeps history to a sane multiple of git",
           ratio < 4.0, f"{ratio:.2f}x, which is worse than a packfile should be")
    record("the delta beats storing every version whole",
           g_bytes < 1504 * 1024,
           f"{g_bytes/1024:.0f} KB, was 1504 KB before deltas")
    record("stats reports the delta bytes rather than hiding them",
           "in deltas" in zf_stats and "deltas:" in zf_stats, zf_stats[:200])

    passed = sum(1 for _, ok, _ in results if ok)
    total = len(results)
    print(f"\n{passed}/{total} delta checks pass")
    return 0 if passed == total else 1


if __name__ == "__main__":
    sys.exit(main())
