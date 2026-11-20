#!/usr/bin/env python3
"""Does a large file leave the database, come back byte for byte, and survive a fsck?

The claim has two halves and both are measured: the row stops holding the bytes, and every reader
still returns them. A storage change that is invisible to callers and moves the bytes is the whole
point; one that moves them and loses a read is a data loss bug with a nice name.
"""
import hashlib
import os
import shutil
import subprocess
import sys
import tempfile

_HERE = os.path.dirname(os.path.abspath(__file__))
_PROJECT = os.path.dirname(_HERE)
ZF = os.environ.get("ZF_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))
INLINE_LIMIT = 4 * 1024 * 1024


def sh(cmd, cwd=None, binary=False):
    r = subprocess.run(cmd, shell=True, cwd=cwd, capture_output=True, timeout=300)
    if binary:
        return r.returncode, r.stdout
    return r.returncode, (r.stdout or b"").decode("utf-8", "replace") + (r.stderr or b"").decode("utf-8", "replace")


def main():
    d = tempfile.mkdtemp(prefix="sidecar-")
    try:
        sh(f"{ZF} init", d)

        # Deterministic bytes, large enough to cross the threshold and random enough that a
        # compressor cannot shrink them to nothing.
        body = bytearray()
        seed = 12345
        while len(body) < INLINE_LIMIT + 512 * 1024:
            seed = (seed * 1103515245 + 12345) & 0x7FFFFFFF
            body.append(seed & 0xFF)
        body = bytes(body)
        want = hashlib.sha256(body).hexdigest()

        big = os.path.join(d, "big.bin")
        with open(big, "wb") as fh:
            fh.write(body)

        # A small file too, so the two shapes coexist.
        with open(os.path.join(d, "small.txt"), "w") as fh:
            fh.write("small\n")

        out = sh(f'{ZF} changeset create "add a large file" --author t --json', d)[1]
        import re

        m = re.search(r"(cs[0-9a-f]+)", out)
        cs = m.group(1) if m else ""
        sh(f"{ZF} changeset commit {cs}", d)

        # The bytes are beside the database.
        sidecar_dir = os.path.join(d, ".gesta", "objects")
        files = []
        for root, _dirs, names in os.walk(sidecar_dir):
            for n in names:
                files.append(os.path.join(root, n))
        moved_out = len(files) == 1

        # The row no longer carries the body.
        import sqlite3

        conn = sqlite3.connect(os.path.join(d, ".gesta", "gesta.db"))
        flag, length = conn.execute(
            "SELECT sidecar, LENGTH(content) FROM objects WHERE size >= ?", (INLINE_LIMIT,)
        ).fetchone()
        row_is_pointer = flag == 1 and length == 0
        conn.close()

        # Reading it back gives the same bytes. `show` writes the file's content, so its output is
        # compared by hash rather than by remembering a substring of binary.
        rc, got = sh(f"{ZF} show main:big.bin", d, binary=True)
        same_bytes = rc == 0 and hashlib.sha256(got).hexdigest() == want

        # The small file still reads, so the two shapes coexist.
        rc2, got2 = sh(f"{ZF} show main:small.txt", d)
        small_ok = rc2 == 0 and "small" in got2

        # fsck accepts it, and catches a sidecar object whose file went missing.
        ok_fsck = sh(f"{ZF} fsck", d)[0] == 0
        os.remove(files[0])
        broken = sh(f"{ZF} fsck", d)
        catches_missing = broken[0] != 0 and "MISSING" in broken[1]

        checks = [
            ("the large object left the database", moved_out),
            ("its row is a pointer and not the bytes", row_is_pointer),
            ("reading it returns the same bytes", same_bytes),
            ("a small file beside it still reads", small_ok),
            ("fsck accepts the repository", ok_fsck),
            ("fsck reports a sidecar object whose file is gone", catches_missing),
        ]
        for name, ok in checks:
            print(("  [PASS] " if ok else "  [FAIL] ") + name)
        print()
        print(f"  {len(body)} byte object, {len(files)} file(s) beside the database")
        print()
        if all(ok for _, ok in checks):
            print(f"{len(checks)}/{len(checks)} sidecar checks pass")
            return 0
        print(broken[1][-500:])
        return 1
    finally:
        shutil.rmtree(d, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
