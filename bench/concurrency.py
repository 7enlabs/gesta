#!/usr/bin/env python3
"""Check that parallel writers all succeed.

SQLite allows one writer at a time. `busy_timeout` queues a reader behind a
writer, but two writers arriving at once get SQLITE_BUSY immediately once a
transaction has begun reading. Before the retry loop, 20 parallel commits gave
one success and nineteen `database is locked` failures.

Run: python3 bench/concurrency.py
"""

import os
import shutil
import sqlite3
import subprocess
import sys

ZF = os.environ.get("ZF_BIN", "/work/zenforge/target/release/zenforge")
ROOT = "/tmp/zf_concurrency"
WORKERS = 20
results = []


def record(name, ok, detail=""):
    results.append((name, ok, detail))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}")
    if not ok and detail:
        for line in str(detail).split("\n")[:4]:
            print(f"         {line}")


def run(*args, cwd=None):
    r = subprocess.run([ZF, *args], capture_output=True, text=True, cwd=cwd)
    return r.returncode, (r.stdout + r.stderr).strip()


def spawn_all(args_list, cwd):
    procs = [
        subprocess.Popen(
            [ZF, *args],
            cwd=cwd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        for args in args_list
    ]
    return [p.communicate() for p in procs]


def main():
    if not os.path.exists(ZF):
        sys.exit(f"binary not found: {ZF}")
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(ROOT)

    repo = os.path.join(ROOT, "repo")
    os.makedirs(repo)
    run("init", cwd=repo)

    for i in range(WORKERS):
        with open(os.path.join(repo, f"f{i:02d}.txt"), "w") as fh:
            fh.write(f"content {i}\n")

    # Parallel snapshots.
    outs = spawn_all(
        [("changeset", "create", f"parallel {i}", "--author", f"agent{i}") for i in range(WORKERS)],
        repo,
    )
    errors = [o[1].strip() for o in outs if o[1].strip()]
    record(f"{WORKERS} parallel snapshots all succeed", not errors,
           "\n".join(errors[:3]))

    db = os.path.join(repo, ".zenforge", "zenforge.db")
    conn = sqlite3.connect(db)
    created = conn.execute("SELECT COUNT(*) FROM changesets").fetchone()[0]
    record("every snapshot produced a distinct changeset", created == WORKERS,
           f"{created} of {WORKERS}")

    ids = [r[0] for r in conn.execute(
        "SELECT id FROM changesets WHERE status='pending' ORDER BY created_at, rowid"
    ).fetchall()]
    conn.close()
    record("pending changesets are queued, not committed", len(ids) == WORKERS,
           f"{len(ids)} pending")

    # Parallel commits.
    outs = spawn_all([("changeset", "commit", cid) for cid in ids], repo)
    errors = [o[1].strip() for o in outs if o[1].strip()]
    record(f"{WORKERS} parallel commits all succeed", not errors,
           "\n".join(errors[:3]))

    conn = sqlite3.connect(db)
    committed = conn.execute(
        "SELECT COUNT(*) FROM changesets WHERE status='committed'"
    ).fetchone()[0]
    conn.close()
    record("every changeset reached committed", committed == WORKERS,
           f"{committed} of {WORKERS}")

    # A serial chain, not a fork: each commit re-parents onto the live head.
    rc, out = run("log", "--json", "--limit", str(WORKERS + 5), cwd=repo)
    import json
    try:
        entries = json.loads(out)
    except json.JSONDecodeError:
        entries = []
    committed_entries = [e for e in entries if e.get("status") == "committed"]
    parents = {e.get("parent") for e in committed_entries}
    record("the commits form one chain rather than a fork",
           len(committed_entries) == WORKERS and len(parents) == WORKERS,
           f"{len(committed_entries)} entries, {len(parents)} distinct parents")

    rc, out = run("fsck", cwd=repo)
    record("the repository is consistent afterwards",
           rc == 0 and "consistent" in out, out[:200])

    # Files that were snapshotted before any commit landed must still be tracked
    # once every commit is applied. The last head has to contain all of them.
    rc, out = run("show", f"main:f{WORKERS-1:02d}.txt", cwd=repo)
    record("the final head contains the last written file",
           rc == 0 and f"content {WORKERS-1}" in out, out[:150])

    passed = sum(1 for _, ok, _ in results if ok)
    total = len(results)
    print(f"\n{passed}/{total} concurrency checks pass")
    if passed < total:
        print("\nfailing:")
        for name, ok, _ in results:
            if not ok:
                print(f"  - {name}")
    return 0 if passed == total else 1


if __name__ == "__main__":
    sys.exit(main())
