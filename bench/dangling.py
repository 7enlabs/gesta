#!/usr/bin/env python3
"""A rebase must say when it leaves a reference pointing at nothing.

A replay applies changesets one at a time. When one deletes a file and another, made in parallel,
adds a line that mentions the deleted name, they touch different files: no three-way merge sees a
conflict, both apply, and the rebase reports success while the tree now names a path that is not in
it. Git behaves the same way, and the count of inbound references taken before such a rebase is
stale by construction.

An index over the whole tree answers the question directly, so the rebase answers it: after a
finished replay it compares the tree it moved onto with the tree it produced, and reports every
name that left while another file still mentions it. Silence is the normal case, so the check that
matters most here is the one where nothing should be printed - a warning that fires on substring
matches is a warning people learn to skip.

Run: python3 bench/dangling.py
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
        for line in str(detail).strip().splitlines()[:6]:
            print(f"         {line}")


def sh(args, cwd):
    return subprocess.run(args, cwd=cwd, capture_output=True, text=True, timeout=180)


def write(repo, name, body):
    path = os.path.join(repo, name)
    os.makedirs(os.path.dirname(path) or repo, exist_ok=True)
    with open(path, "w") as fh:
        fh.write(body)


def commit(repo, message):
    sh([ZF, "changeset", "create", message, "--author", "t"], repo)
    made = sh([ZF, "changeset", "commit"], repo)
    return made


def build(base, refs_on_main):
    """A repository with a branch that deletes util.sh and a main that still names it.

    `refs_on_main` is the body main.sh gets on the second commit. The caller chooses it so one
    scenario can hold a real reference and another only a longer name that contains it.
    """
    d = os.path.join(base, "r")
    os.makedirs(d)
    sh([ZF, "init"], d)
    write(d, "util.sh", "helper\n")
    write(d, "main.sh", "source util.sh\n")
    commit(d, "base")

    sh([ZF, "branch", "create", "side"], d)
    sh([ZF, "branch", "switch", "side"], d)
    os.unlink(os.path.join(d, "util.sh"))
    commit(d, "delete util")

    sh([ZF, "branch", "switch", "main"], d)
    write(d, "main.sh", refs_on_main)
    commit(d, "names it again")

    sh([ZF, "branch", "switch", "side"], d)
    return d


def main() -> int:
    if not os.path.exists(ZF):
        print(f"binary not found: {ZF}")
        return 1

    base = tempfile.mkdtemp(prefix="dangling-")
    try:
        # A real reference survives the rebase, and the report has to name it with the line.
        d = build(base, "source util.sh\necho done\n")
        write(d, "notes.md", "note about util.sh\n")
        commit(d, "a second reference")
        r = sh([ZF, "rebase", "main"], d)
        out = r.stdout + r.stderr
        record("a rebase that leaves a reference still succeeds", r.returncode == 0, out)
        record("the report names the file that still mentions it", "main.sh:1" in out, out)
        record("the report names the path that left", "util.sh" in out, out)
        record("the report quotes the line", "source util.sh" in out, out)
        record(
            "the report says the rebase itself succeeded",
            "rebase succeeded" in out,
            out,
        )

        # The deleted path is gone from the tree, and the reference is what survived.
        listing = sh([ZF, "ls"], d).stdout
        record("the deleted path is not in the tree", "util.sh\n" not in listing, listing)

        # A name that is only a substring of a longer one is not a reference. Without this the
        # feature reports every file that says `myutil.sh`, and the reader stops reading.
        d2 = build(os.path.join(base, "sub"), "source myutil.sh\ncp util.sh.bak\n")
        r = sh([ZF, "rebase", "main"], d2)
        out = r.stdout + r.stderr
        record(
            "a longer name that contains it is not reported",
            "util.sh" not in out or "reference(s)" not in out,
            out,
        )
        record("and the rebase still succeeds", r.returncode == 0, out)

        # A rebase that deletes nothing has nothing to report, however many files exist.
        d3 = os.path.join(base, "clean")
        os.makedirs(d3)
        sh([ZF, "init"], d3)
        write(d3, "a.txt", "one\n")
        commit(d3, "one")
        sh([ZF, "branch", "create", "side"], d3)
        sh([ZF, "branch", "switch", "side"], d3)
        write(d3, "b.txt", "two\n")
        commit(d3, "two")
        sh([ZF, "branch", "switch", "main"], d3)
        write(d3, "c.txt", "three\n")
        commit(d3, "three")
        sh([ZF, "branch", "switch", "side"], d3)
        r = sh([ZF, "rebase", "main"], d3)
        out = r.stdout + r.stderr
        record(
            "a rebase that removes nothing reports nothing",
            "reference(s)" not in out,
            out,
        )

        # A rebase that deletes a file nobody mentions stays silent too: the deletion is the
        # intent, and there is no dangling reference to warn about.
        d4 = os.path.join(base, "quietdelete")
        os.makedirs(d4)
        sh([ZF, "init"], d4)
        write(d4, "only.txt", "alone\n")
        write(d4, "other.txt", "nothing related\n")
        commit(d4, "base")
        sh([ZF, "branch", "create", "side"], d4)
        sh([ZF, "branch", "switch", "side"], d4)
        os.unlink(os.path.join(d4, "only.txt"))
        commit(d4, "delete alone")
        sh([ZF, "branch", "switch", "main"], d4)
        write(d4, "other.txt", "still nothing related\n")
        commit(d4, "edit other")
        sh([ZF, "branch", "switch", "side"], d4)
        r = sh([ZF, "rebase", "main"], d4)
        out = r.stdout + r.stderr
        record(
            "a deletion nothing mentions is not a dangling reference",
            "reference(s)" not in out,
            out,
        )

        # A stopped rebase loses the non-conflicting changes of the changeset it stopped on,
        # unless the continuation re-applies them. A deletion is where that shows: the file is
        # still on disk, so recording the tree records it as present and the deletion vanishes
        # from the branch with no error at any step. This was a real data-loss defect.
        d5 = os.path.join(base, "stopped")
        os.makedirs(d5)
        sh([ZF, "init"], d5)
        write(d5, "util.sh", "helper\n")
        write(d5, "shared.txt", "v1\n")
        commit(d5, "base")
        sh([ZF, "branch", "create", "side"], d5)
        sh([ZF, "branch", "switch", "side"], d5)
        os.unlink(os.path.join(d5, "util.sh"))
        write(d5, "shared.txt", "side\n")
        commit(d5, "delete util, edit shared")
        sh([ZF, "branch", "switch", "main"], d5)
        write(d5, "shared.txt", "main\n")
        write(d5, "user.sh", "source util.sh\n")
        commit(d5, "edit shared, use util")
        sh([ZF, "branch", "switch", "side"], d5)

        r = sh([ZF, "rebase", "main"], d5)
        record("a conflicting rebase stops", "stopped" in (r.stdout + r.stderr), r.stdout + r.stderr)
        # The two flags name different missing things. Aborting when nothing is in progress must
        # not tell the caller there is nothing to continue, because they will try --continue.
        # Checked on a repository with no rebase running, since after the stop above `--abort`
        # would legitimately succeed and undo the very state this scenario is about.
        r = sh([ZF, "rebase", "--abort"], d3)
        out = r.stdout + r.stderr
        record("abort with no rebase names what is missing", "nothing to abort" in out, out)
        record(
            "abort does not tell the caller to try --continue",
            "nothing to continue" not in out,
            out,
        )
        write(d5, "shared.txt", "resolved\n")
        r = sh([ZF, "rebase", "--continue"], d5)
        out = r.stdout + r.stderr
        record("the continuation finishes", r.returncode == 0, out)
        record(
            "the deletion the stopped changeset made survives the continuation",
            not os.path.exists(os.path.join(d5, "util.sh")),
            f"util.sh still on disk: {os.path.exists(os.path.join(d5, 'util.sh'))}",
        )
        listing = sh([ZF, "ls"], d5).stdout
        record("the deletion is in the recorded tree", "util.sh\n" not in listing, listing)
        record(
            "the caller's resolution survived",
            open(os.path.join(d5, "shared.txt")).read() == "resolved\n",
            open(os.path.join(d5, "shared.txt")).read(),
        )

        passed = sum(1 for _, ok in checks if ok)
        print(f"\n{passed}/{len(checks)} dangling checks pass")
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
