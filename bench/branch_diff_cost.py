#!/usr/bin/env python3
"""What a flat branch tree costs when two branches are compared.

Dolt replaced the B-tree with a Prolly tree so that a diff between two versions scales with the
difference instead of with the data. The argument is structural: a content-addressed node gives two
versions a shared root for every subtree they did not touch, so a comparison descends only into the
parts that differ.

This store keeps `branch_tree` as a flat table of (branch, path, hash). Comparing two branches means
reading two sets of rows and diffing them in Python. That is fine at a thousand paths. This measures
what it costs as the tree and the difference both grow, so the decision to replace the structure is
made on a number rather than on an analogy with a database engine built for a different problem.

Two shapes, because they answer different questions:

  wide-diff   two branches that differ in a fixed fraction of a large tree: the case a Prolly tree
              is built for, where the cost should track the difference
  one-file    two branches that differ in one path of a large tree: the case a person actually asks
              ("what changed on this branch")

Run: python3 bench/branch_diff_cost.py
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

_PROJECT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ZF = os.environ.get("GESTA_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))


def sh(args, cwd, timeout=1800):
    return subprocess.run(args, cwd=cwd, capture_output=True, text=True, timeout=timeout)


def median_ms(fn, repeat=3):
    runs = []
    for _ in range(repeat):
        t = time.perf_counter()
        fn()
        runs.append((time.perf_counter() - t) * 1000.0)
    runs.sort()
    return runs[len(runs) // 2]


def commit(repo, files, message):
    for rel, body in files.items():
        full = os.path.join(repo, rel)
        os.makedirs(os.path.dirname(full), exist_ok=True) if os.path.dirname(full) else None
        with open(full, "w") as fh:
            fh.write(body)
    r = sh([ZF, "changeset", "create", message, "--author", "t", "--json"], repo)
    if r.returncode != 0:
        raise SystemExit(f"create failed: {r.stdout}{r.stderr}")
    cid = json.loads(r.stdout)["changeset"]
    sh([ZF, "changeset", "commit", cid], repo)


def build(n, base, name):
    """A branch of n paths, with a side branch that changes `changelist` of them."""
    d = os.path.join(base, name)
    os.makedirs(d)
    sh([ZF, "init"], d)
    files = {f"d{i // 100}/f{i:05d}.txt": f"body {i}\n" for i in range(n)}
    commit(d, files, "all")
    sh([ZF, "branch", "create", "side"], d)
    return d


def main() -> int:
    if not os.path.exists(ZF):
        print(f"binary not found: {ZF}")
        return 1

    base = tempfile.mkdtemp(prefix="branch-diff-")
    try:
        print(f"{'paths':>8} {'changes':>9} {'diff ms':>10} {'us/change':>11} {'us/path':>9}")
        rows = []
        for n in (200, 1000, 4000):
            d = build(n, base, f"r{n}")
            sh([ZF, "branch", "switch", "side"], d)
            for kind, changes in (("one", 1), ("many", n // 10)):
                # Each round adds `changes` modified paths on the side branch, then asks what
                # separates the two branches.
                batch = {
                    f"d{i // 100}/f{i:05d}.txt": f"changed {i}\n"
                    for i in range(0, changes * 10, 10)
                }
                commit(d, batch, f"{kind} {changes}")
                cost = median_ms(
                    lambda: sh([ZF, "diff", "--from", "main", "--to", "side", "--name-only"], d),
                    repeat=3,
                )
                per_change = cost / max(len(batch), 1) * 1000
                per_path = cost / n * 1000
                rows.append((n, len(batch), cost, per_change, per_path))
                print(f"{n:>8} {len(batch):>9} {cost:>9.2f}  {per_change:>10.1f} {per_path:>8.1f}")

        print()
        # The structural claim: a flat table reads every path on both sides, so the floor is the
        # size of the tree, not the size of the change. That is what the per-path column says.
        widths = [r[4] for r in rows]
        print(f"cost per path in the tree: {min(widths):.1f} to {max(widths):.1f} us")
        print("a cost that tracks the tree rather than the change is the property a Prolly tree")
        print("removes; whether it is worth the change here is what this table is for")
        return 0
    finally:
        shutil.rmtree(base, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
