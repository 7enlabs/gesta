#!/usr/bin/env python3
"""A tag has to mean the same thing the next time it is read.

`gesta tag v1` used `INSERT ... ON CONFLICT DO UPDATE`, so running it twice silently moved the tag
to the second changeset: exit 0, a success message, and the first target gone from every record. A
name people put in a build script or a release note cannot behave that way.

Four behaviours, and the third is the one that was missing:

  same point twice     a no-op that says so, because a script that re-tags after every build should
                       not fail on the runs where nothing moved
  a different point    refused, exit non-zero, with the old target in the message
  --force              moves it, and the message names where it came from
  reading by name      `tag --list` prints the target, so a tag can be read rather than re-created

Run: python3 bench/tags.py
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile

_PROJECT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ZF = os.environ.get("GESTA_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))

checks = []


def record(name, ok, detail=""):
    checks.append((name, ok))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}")
    if not ok and detail:
        for line in str(detail).strip().splitlines()[:4]:
            print(f"         {line}")


def sh(args, cwd):
    return subprocess.run(args, cwd=cwd, capture_output=True, text=True, timeout=180)


def commit(repo, body, message):
    with open(os.path.join(repo, "f.txt"), "w") as fh:
        fh.write(body)
    made = sh([ZF, "changeset", "create", message, "--author", "t", "--json"], repo)
    cid = json.loads(made.stdout)["changeset"]
    sh([ZF, "changeset", "commit", cid], repo)
    return cid


def tag_of(repo, name):
    """The changeset a tag points at, read from the list output."""
    r = sh([ZF, "tag", "--list"], repo)
    for line in r.stdout.splitlines():
        parts = line.split()
        if len(parts) >= 2 and parts[0] == name:
            return parts[1]
    return None


def main() -> int:
    if not os.path.exists(ZF):
        print(f"binary not found: {ZF}")
        return 1

    base = tempfile.mkdtemp(prefix="tags-")
    try:
        d = os.path.join(base, "r")
        os.makedirs(d)
        sh([ZF, "init"], d)
        first = commit(d, "one\n", "one")

        r = sh([ZF, "tag", "v1"], d)
        record("a new tag is created", r.returncode == 0 and tag_of(d, "v1") == first, r.stdout + r.stderr)

        # Re-tagging the same point: a no-op, not a failure and not a move.
        r = sh([ZF, "tag", "v1"], d)
        record(
            "tagging the same point again is a no-op that exits 0",
            r.returncode == 0 and tag_of(d, "v1") == first,
            f"exit {r.returncode}, tag at {tag_of(d, 'v1')}, wanted {first}\n{r.stdout}{r.stderr}",
        )

        second = commit(d, "two\n", "two")

        # The defect: this used to move v1 to `second` and report success.
        r = sh([ZF, "tag", "v1"], d)
        record(
            "moving a tag without --force is refused",
            r.returncode != 0,
            f"exit {r.returncode}, stdout {r.stdout!r}",
        )
        record(
            "the refusal leaves the tag where it was",
            tag_of(d, "v1") == first,
            f"tag at {tag_of(d, 'v1')}, wanted {first}",
        )
        record(
            "the refusal names the old target",
            first in (r.stdout + r.stderr),
            r.stdout + r.stderr,
        )

        # An explicit move is allowed and says where it came from.
        r = sh([ZF, "tag", "v1", "--force"], d)
        record(
            "--force moves the tag",
            r.returncode == 0 and tag_of(d, "v1") == second,
            f"exit {r.returncode}, tag at {tag_of(d, 'v1')}, wanted {second}",
        )
        record(
            "--force reports the old target",
            first in (r.stdout + r.stderr),
            r.stdout + r.stderr,
        )

        # A tag can be read by name rather than only re-created.
        r = sh([ZF, "tag", "--list"], d)
        record(
            "a tag can be read back by name",
            "v1" in r.stdout and second in r.stdout,
            r.stdout,
        )
        # `tag list` used to create a tag literally named "list" instead of listing.
        record(
            "`tag list` does not exist as a name to create",
            tag_of(d, "list") is None,
            f"a tag named 'list' was created at {tag_of(d, 'list')}",
        )

        passed = sum(1 for _, ok in checks if ok)
        print(f"\n{passed}/{len(checks)} tag checks pass")
        if passed != len(checks):
            print("\nfailing:")
            for n, ok in checks:
                if not ok:
                    print(f"  - {n}")
        return 0 if passed == len(checks) else 1
    finally:
        shutil.rmtree(base, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
