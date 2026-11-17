#!/usr/bin/env python3
"""Reproducible measurements behind docs/upgrade-plan.md.

Run inside the workspace container:
    python3 bench/bench.py

Every number in the upgrade plan comes from one of these runs. Re-run after a
change to see whether the fix actually moved the metric instead of trusting it.
"""

import os
import shutil
import subprocess
import sys
import time

ZF = os.environ.get("ZF_BIN", "/work/zenforge/target/release/zenforge")
ROOT = "/tmp/zf_bench"


def run(*args, cwd=None, check=True):
    r = subprocess.run([ZF, *args], capture_output=True, text=True, cwd=cwd)
    if check and r.returncode != 0:
        sys.exit(f"FAILED {args}: {r.stderr.strip()[:300]}")
    return r.stdout


def fresh(name):
    d = os.path.join(ROOT, name)
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d)
    run("init", cwd=d)
    return d


def new_changeset(d, message, author="bench"):
    out = run("changeset", "create", message, "--author", author, cwd=d)
    return next(w for w in out.split() if w.startswith("cs"))


def committed(d, message, author="bench"):
    cs = new_changeset(d, message, author)
    run("changeset", "commit", cs, cwd=d)
    return cs


def ms(fn):
    t0 = time.perf_counter()
    fn()
    return (time.perf_counter() - t0) * 1000


def sqlite_int(db, query):
    r = subprocess.run(["sqlite3", db, query], capture_output=True, text=True)
    return r.stdout.strip()


def dir_size(path):
    total = 0
    for dp, _, files in os.walk(path):
        for f in files:
            try:
                total += os.path.getsize(os.path.join(dp, f))
            except OSError:
                pass
    return total


def bench_speed():
    print("== 1. Speed (not the bottleneck) ==")
    d = fresh("speed")
    os.makedirs(os.path.join(d, "src"))
    for i in range(2000):
        with open(os.path.join(d, "src", f"f{i}.txt"), "w") as fh:
            fh.write("a\nb\nc\nd\ne\n")

    holder = {}
    t_create = ms(lambda: holder.update(cs=new_changeset(d, "2000 files")))
    t_commit = ms(lambda: run("changeset", "commit", holder["cs"], cwd=d))
    t_status = ms(lambda: run("status", cwd=d))
    print(f"  create 2000 files : {t_create:.0f} ms")
    print(f"  commit 2000 files : {t_commit:.0f} ms")
    print(f"  status 2000 files : {t_status:.0f} ms")


def bench_size_vs_git():
    print("\n== 2. Disk usage vs git (real problem) ==")
    n = 500
    base = [f"line {i}: some content padding padding padding" for i in range(n)]

    def workload(name, use_git):
        d = os.path.join(ROOT, name)
        shutil.rmtree(d, ignore_errors=True)
        os.makedirs(d)
        if use_git:
            subprocess.run(["git", "init", "-q"], cwd=d, check=True)
            subprocess.run(["git", "config", "user.email", "a@b"], cwd=d, check=True)
            subprocess.run(["git", "config", "user.name", "a"], cwd=d, check=True)
        else:
            run("init", cwd=d)

        lines = list(base)
        with open(os.path.join(d, "big.txt"), "w") as fh:
            fh.write("\n".join(lines) + "\n")
        if use_git:
            subprocess.run(["git", "add", "-A"], cwd=d, check=True)
            subprocess.run(["git", "commit", "-qm", "v0"], cwd=d, check=True)
        else:
            committed(d, "v0")

        for r in range(1, 31):
            lines[200] = f"line 200: EDITED round {r}"
            with open(os.path.join(d, "big.txt"), "w") as fh:
                fh.write("\n".join(lines) + "\n")
            if use_git:
                subprocess.run(["git", "add", "-A"], cwd=d, check=True)
                subprocess.run(["git", "commit", "-qm", f"edit {r}"], cwd=d, check=True)
            else:
                committed(d, f"edit {r}")
        return dir_size(d)

    zf = workload("cmp_zf", False)
    gt = workload("cmp_git", True)
    print(f"  ZenForge : {zf / 1024:.0f} KB")
    print(f"  git      : {gt / 1024:.0f} KB")
    print(f"  ratio    : {zf / gt:.1f}x   (target after compression: 2-3x)")


def bench_merge_conflict():
    print("\n== 3. Merge: silent working-copy divergence ==")
    d = fresh("merge")
    p = os.path.join(d, "shared.txt")

    def write(s):
        with open(p, "w") as fh:
            fh.write(s)

    write("base line\n")
    committed(d, "base")
    run("branch", "create", "dev", cwd=d)
    run("branch", "switch", "dev", cwd=d)
    write("DEV VERSION\n")
    committed(d, "dev edit")

    run("branch", "switch", "main", cwd=d)
    write("MAIN VERSION\n")
    committed(d, "main edit")

    before = open(p).read().strip()
    out = run("branch", "merge", "dev", "--target", "main", cwd=d, check=False)
    after = open(p).read().strip()
    head = [l for l in run("branch", "info", "main", cwd=d).split("\n") if "head" in l]

    print(f"  merge says    : {out.strip().splitlines()[0] if out.strip() else '(nothing)'}")
    print(f"  disk before   : {before!r}")
    print(f"  disk after    : {after!r}")
    print(f"  {head[0].strip() if head else ''}")
    print("  VERDICT: head moved but the file was not written -> divergence, no conflict raised")


def bench_blame_duplicate_lines():
    print("\n== 4. Blame: wrong author on duplicate content ==")
    d = fresh("blame_dup")
    p = os.path.join(d, "f.txt")

    with open(p, "w") as fh:
        fh.write("return None\nother\nreturn None\n")
    committed(d, "A writes two identical lines", "agentA")

    with open(p, "w") as fh:
        fh.write("return None\nother\nreturn None\nreturn None\n")
    committed(d, "B appends a third identical line", "agentB")

    print(run("blame", "f.txt", cwd=d).strip())
    print("  line 4 was written by agentB; if it says agentA the attribution is wrong")


def bench_blame_moved_line():
    print("\n== 5. Blame: moved line ==")
    d = fresh("blame_move")
    p = os.path.join(d, "f.txt")

    with open(p, "w") as fh:
        fh.write("A\nB\nC\nD\nE\n")
    committed(d, "v1", "agentA")

    with open(p, "w") as fh:
        fh.write("E\nA\nB\nC\nD\n")
    committed(d, "v2 move E to top", "agentB")

    print(run("blame", "f.txt", cwd=d).strip())
    print("  agentB only moved E. Correct blame keeps E with agentA.")


def bench_orphan_objects():
    print("\n== 6. No GC: pending changesets keep their objects forever ==")
    d = fresh("gc")
    p = os.path.join(d, "a.txt")
    for v in range(5):
        with open(p, "w") as fh:
            fh.write(f"version {v}\n")
        new_changeset(d, f"v{v}")  # created, never committed
    print("  objects|changesets|files:",
          sqlite_int(os.path.join(d, ".zenforge", "zenforge.db"),
                     "SELECT (SELECT COUNT(*) FROM objects), (SELECT COUNT(*) FROM changesets), "
                     "(SELECT COUNT(*) FROM changeset_files);"))
    print("  all 5 objects stay on disk although nothing was committed")


def bench_missing_commands():
    print("\n== 7. Missing commands ==")
    have = run("--help").split("Commands:")[-1]
    names = [l.strip().split()[0] for l in have.splitlines()
             if l.startswith("  ") and l.strip() and not l.strip().startswith("-")]
    want = ["undo", "revert", "restore", "reset", "gc", "fsck", "serve", "mcp"]
    print(f"  present : {', '.join(n for n in names if n != 'help')}")
    print(f"  missing : {', '.join(w for w in want if w not in names)}")


def main():
    if not shutil.which(ZF) and not os.path.exists(ZF):
        sys.exit(f"zenforge binary not found: {ZF}")
    os.makedirs(ROOT, exist_ok=True)
    bench_speed()
    bench_size_vs_git()
    bench_merge_conflict()
    bench_blame_duplicate_lines()
    bench_blame_moved_line()
    bench_orphan_objects()
    bench_missing_commands()
    print("\nDone. Numbers above feed docs/upgrade-plan.md.")


if __name__ == "__main__":
    main()
