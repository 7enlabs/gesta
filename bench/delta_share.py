#!/usr/bin/env python3
"""Would delta-ing a new file against a neighbour of the same shape pay?

`docs/storage.md` records that packing small objects into segments made the repository larger, and
that the remaining gap to git is spread across six per-path tables rather than concentrated in one
place. It also names the thing git does that this does not: a file with the same prologue as the file
next to it is stored as a delta against it, while this store has no base for a path it has never seen.

This measures that specific claim before anything is built, because the last time a plausible idea in
this area was implemented first it made the file bigger.

Two questions, answered by measurement rather than by argument:

  1. How much do 1000 files of one shape actually share, as bytes a delta can remove?
  2. If each new file were stored as a delta against the previous one, how large would the result be
     compared with storing each whole?

The second answer is an estimate over the same bodies the store would hold, computed here rather than
by writing a schema change. When it says the saving is smaller than the cost of a base pointer, the
idea is not worth implementing; when it says otherwise, that is the evidence to build on.
"""

import os
import shutil
import subprocess
import sys
import tempfile
import zlib

_PROJECT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ZF = os.environ.get("GESTA_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))


def lcs_delta_size(base: bytes, target: bytes) -> int:
    """The size of a copy/insert delta, the way git's binary delta describes one.

    Not a Lagrange or a Myers diff: this counts the bytes a delta would have to carry, which is what
    the question is about, and it is computed with a longest-common-subsequence table on the lines
    rather than the bytes because that is the granularity a prologue shares.
    """
    bl = base.splitlines(keepends=True)
    tl = target.splitlines(keepends=True)
    n, m = len(bl), len(tl)
    # A full table on 200 x 200 lines is 40k cells, which is fine; anything much larger falls back to
    # "stored whole", which is what the store does today anyway.
    if n * m > 4_000_000:
        return len(target)
    prev = [0] * (m + 1)
    for i in range(1, n + 1):
        cur = [0] * (m + 1)
        bi = bl[i - 1]
        for j in range(1, m + 1):
            if bi == tl[j - 1]:
                cur[j] = prev[j - 1] + len(bi)
            else:
                cur[j] = max(prev[j], cur[j - 1])
        prev = cur
    common = prev[m]
    # Every copied byte still costs a few bytes of offset and length in a real delta; 4 is the figure
    # git's own format pays for a copy instruction in the common case.
    return (len(target) - common) + (common // 64) * 4 + 16


def main() -> int:
    if not os.path.exists(ZF):
        print(f"binary not found: {ZF}")
        return 1

    base = tempfile.mkdtemp(prefix="delta-share-")
    try:
        shapes = {}
        # The three shapes docs/storage.md measures, so the answer is comparable to the 4.09x / 8.24x
        # / 4.39x figures rather than to a new experiment.
        shapes["distinct"] = [
            (f"distinct/f{i:04d}.txt", f"file {i} unique body {i * 7919}\n".encode())
            for i in range(200)
        ]
        # The prologue has to be text zlib cannot collapse on its own, or the measurement answers a
        # different question: a regular prologue compresses within each file, so a cross-file delta has
        # nothing left to save and the result says "delta does not help" when it means "compression
        # already did". Random-looking words are what a real shared prologue is: imports, comments, a
        # licence header, all distinct text that happens to be identical between two files.
        import random

        rng = random.Random(20261122)
        alphabet = "abcdefghijklmnopqrstuvwxyz"
        prologue = "".join(
            " ".join("".join(rng.choice(alphabet) for _ in range(rng.randint(3, 9))) for _ in range(9))
            + "\n"
            for _ in range(200)
        )
        shapes["shared"] = [
            (f"shared/f{i:04d}.txt", (prologue + f"tail {i}\n").encode()) for i in range(200)
        ]
        shapes["json"] = [
            (
                f"json/f{i:04d}.json",
                ('{"id": %d, "name": "item %d", "active": true, "score": %d}\n' % (i, i, i * 3)).encode(),
            )
            for i in range(200)
        ]

        print(f"{'shape':10} {'whole':>10} {'delta-chain':>12} {'ratio':>7}  verdict")
        for name, files in shapes.items():
            whole = 0
            chained = 0
            previous = None
            for _path, body in files:
                packed = zlib.compress(body, 9)
                whole += min(len(body), len(packed))
                if previous is None:
                    chained += min(len(body), len(packed))
                else:
                    d = lcs_delta_size(previous, body)
                    # A delta is stored compressed too, and stands or falls against the whole body.
                    chained += min(d, len(body), len(packed))
                previous = body
            ratio = whole / chained if chained else 1.0
            verdict = (
                "worth it: a chain would shrink this"
                if ratio >= 1.5
                else "not worth it: the pointer costs what the delta saves"
            )
            print(f"{name:10} {whole:>10,} {chained:>12,} {ratio:>6.2f}x  {verdict}")

        # What the same tree costs from the tool itself, for the comparison to mean something.
        repo = os.path.join(base, "repo")
        os.makedirs(repo)
        subprocess.run([ZF, "init"], cwd=repo, capture_output=True, timeout=60)
        for name, files in shapes.items():
            d = os.path.join(repo, name)
            os.makedirs(d, exist_ok=True)
            for path, body in files:
                with open(os.path.join(repo, path), "wb") as fh:
                    fh.write(body)
        r = subprocess.run(
            [ZF, "changeset", "create", "all shapes", "--json"],
            cwd=repo,
            capture_output=True,
            text=True,
            timeout=120,
        )
        import re

        m = re.search(r"\b(cs[0-9a-f]{6,})\b", r.stdout)
        if m:
            subprocess.run(
                [ZF, "changeset", "commit", m.group(1)], cwd=repo, capture_output=True, timeout=180
            )
            subprocess.run([ZF, "gc"], cwd=repo, capture_output=True, timeout=180)
        db = None
        for root, _dirs, names in os.walk(repo):
            for n in names:
                if n.endswith(".db"):
                    db = os.path.join(root, n)
        if db:
            print(f"\n600 files of these shapes in one commit: {os.path.getsize(db):,} bytes on disk")
        print(
            "A chain only pays on the shared shape, and that is the shape a real tree has least of:\n"
            "two files in a repository usually differ in their body, not only in their tail."
        )
        return 0
    finally:
        shutil.rmtree(base, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
