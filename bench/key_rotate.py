#!/usr/bin/env python3
"""Does a rotation leave everything signed with the old key still verifiable?

That is the whole claim. A rotation that invalidates history is worse than no rotation, because the
person rotating believes they improved things. The check signs, rotates, and verifies the old
signature against the key the changeset itself records.

It also checks the two things that make the archive worth having: the old private key still exists
under a name that identifies it, and the record names both keys.
"""
import os
import re
import shutil
import subprocess
import sys
import tempfile

_HERE = os.path.dirname(os.path.abspath(__file__))
_PROJECT = os.path.dirname(_HERE)
ZF = os.environ.get("ZF_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))


def sh(cmd, cwd=None):
    r = subprocess.run(cmd, shell=True, cwd=cwd, capture_output=True, text=True, timeout=120)
    return r.returncode, (r.stdout or "") + (r.stderr or "")


def main():
    d = tempfile.mkdtemp(prefix="key-rotate-")
    try:
        sh(f"{ZF} init", d)
        key = os.path.join(d, "sign.key")

        out = sh(f"{ZF} key generate --path {key}", d)[1]
        first = ""
        m = re.search(r"public key: ([0-9a-f]+)", out)
        if m:
            first = m.group(1)

        with open(os.path.join(d, "a.txt"), "w") as fh:
            fh.write("one\n")
        made = sh(f'{ZF} changeset create "first" --author t --json', d)[1]
        cs = ""
        m = re.search(r"(cs[0-9a-f]+)", made)
        if m:
            cs = m.group(1)
        sh(f"{ZF} changeset sign {cs} --key {key}", d)

        # The signature verifies before the rotation.
        before = sh(f"{ZF} verify {cs}", d)
        verified_before = before[0] == 0

        # Rotate.
        rot = sh(f"{ZF} key rotate --path {key}", d)[1]
        m = re.search(r"public key: ([0-9a-f]+)", rot)
        second = m.group(1) if m else ""
        rotated = bool(second) and second != first

        # The signature still verifies after it. This is the claim.
        after = sh(f"{ZF} verify {cs}", d)
        verified_after = after[0] == 0

        # The old private key is on disk under a name that names it.
        archived = [
            f
            for f in os.listdir(d)
            if f.startswith("sign.key.") and f.endswith(".retired")
        ]
        old_kept = len(archived) == 1 and first[:16] in archived[0]

        # The record names both, with exactly one active.
        listing = sh(f"{ZF} key list", d)[1]
        both_recorded = first[:16] in listing and second[:16] in listing
        one_active = listing.count("active") == 1

        # A signature made by the new key also verifies, so rotation did not break signing.
        with open(os.path.join(d, "b.txt"), "w") as fh:
            fh.write("two\n")
        made2 = sh(f'{ZF} changeset create "second" --author t --json', d)[1]
        cs2 = ""
        m = re.search(r"(cs[0-9a-f]+)", made2)
        if m:
            cs2 = m.group(1)
        sh(f"{ZF} changeset sign {cs2} --key {key}", d)
        new_verifies = sh(f"{ZF} verify {cs2}", d)[0] == 0

        checks = [
            ("key generate prints a public key", bool(first)),
            ("a changeset signs and verifies", verified_before),
            ("rotation makes a different key", rotated),
            ("the old signature still verifies after the rotation", verified_after),
            ("the old private key is archived under a name that names it", old_kept),
            ("both keys are recorded with one active", both_recorded and one_active),
            ("the new key signs changesets that verify", new_verifies),
        ]
        for name, ok in checks:
            print(("  [PASS] " if ok else "  [FAIL] ") + name)
        print()
        if all(ok for _, ok in checks):
            print(f"{len(checks)}/{len(checks)} key rotation checks pass")
            return 0
        print(f"first={first[:16]} second={second[:16]} archived={archived}")
        print("--- verify after ---")
        print(after[1][-400:])
        print("--- key list ---")
        print(listing)
        return 1
    finally:
        shutil.rmtree(d, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
