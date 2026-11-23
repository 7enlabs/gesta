#!/usr/bin/env python3
"""A refusal has to name the thing the caller actually got wrong.

Three messages were wrong in ways that cost the reader a round trip, and each was found by running
the command with a plausible wrong argument rather than by reading the code:

  show nope        in a repository with no changesets, said "no such branch, tag or changeset:
                   main". The reader typed `nope`; the message named `main`, which exists. The
                   name had been read as a path against the current branch, and nothing said so.

  show nope        in a repository with changesets, said "nope does not exist at main", which is
                   true and still leaves the reader guessing whether the branch `nope` is missing
                   or the file `nope` is. Both readings are now spelled out.

  rebase nope      said "no such changeset: nope", but the argument accepts a branch, a tag or a
                   changeset. A reader who mistyped a branch name was told to go looking for a
                   changeset id, and was not told which branches exist.

`changeset commit` also required an id it could have looked up, which turned the shortest possible
session - create one changeset, commit it - into two failed attempts before the right form. With no
id it now commits the only pending changeset, and refuses with the list when there are several,
because choosing silently would commit work the caller did not name.

Run: python3 bench/refusals.py
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


def main() -> int:
    if not os.path.exists(ZF):
        print(f"binary not found: {ZF}")
        return 1

    base = tempfile.mkdtemp(prefix="refusals-")
    try:
        empty = os.path.join(base, "empty")
        os.makedirs(empty)
        sh([ZF, "init"], empty)

        full = os.path.join(base, "full")
        os.makedirs(full)
        sh([ZF, "init"], full)
        commit(full, "one\n", "one")

        # An empty repository: the name cannot be a file, because there is no tree at all. The
        # message has to say that rather than blame the branch that does exist.
        r = sh([ZF, "show", "nope"], empty)
        out = r.stdout + r.stderr
        record(
            "show in an empty repository does not blame the branch that exists",
            "no such branch, tag or changeset: main" not in out,
            out,
        )
        record(
            "show in an empty repository says there is nothing to show",
            "has no changesets yet" in out,
            out,
        )
        record("show in an empty repository still fails", r.returncode != 0, f"rc={r.returncode}")

        # A populated repository: the name was read as a path, and the message has to say so
        # while keeping the other reading reachable.
        r = sh([ZF, "show", "nope"], full)
        out = r.stdout + r.stderr
        record("show reads a bare name as a file and says so", "was read as a file" in out, out)
        record(
            "show names the revision form so the other reading is reachable",
            "<revision>:nope" in out,
            out,
        )
        record("show with a bare name still fails", r.returncode != 0, f"rc={r.returncode}")

        # The hint must not be a template the reader has to fill in: `<file>` pasted into a shell
        # is a redirect, so the path they typed is carried into the example instead.
        record(
            "the hint carries the name the reader typed, not a placeholder",
            "<file>" not in out,
            out,
        )

        # A colon means the reader named a revision, so the message has to be about the revision.
        # The first version of this change said "was read as a file" for `show nosuch:a.txt` too,
        # which sends the reader to look at the file when the name before the colon is the
        # problem. The gate's own `errors` step caught it; this check keeps it caught.
        r = sh([ZF, "show", "nosuch:a.txt"], full)
        out = r.stdout + r.stderr
        record(
            "a colon means the revision is what is missing",
            "no such branch, tag or changeset" in out and "was read as a file" not in out,
            out,
        )
        record("the colon form still exits non-zero", r.returncode != 0, f"rc={r.returncode}")

        # rebase accepts a branch, a tag or a changeset, so the refusal has to name all three and
        # list what exists.
        r = sh([ZF, "rebase", "nosuchbranch"], full)
        out = r.stdout + r.stderr
        record(
            "rebase refuses with every form the argument accepts",
            "branch, tag or changeset" in out,
            out,
        )
        record("rebase names the branches that exist", "known branches:" in out, out)
        record("rebase names the argument that was wrong", "nosuchbranch" in out, out)

        # Committing with no id: the one pending case succeeds, and the several case refuses with
        # the ids rather than picking one.
        solo = os.path.join(base, "solo")
        os.makedirs(solo)
        sh([ZF, "init"], solo)
        with open(os.path.join(solo, "f.txt"), "w") as fh:
            fh.write("one\n")
        sh([ZF, "changeset", "create", "one", "--author", "t"], solo)
        r = sh([ZF, "changeset", "commit"], solo)
        record(
            "commit with no id commits the only pending changeset",
            r.returncode == 0 and "committed" in (r.stdout + r.stderr),
            r.stdout + r.stderr,
        )
        logged = sh([ZF, "log"], solo)
        record(
            "the changeset committed that way is in the log",
            "changeset cs" in logged.stdout,
            logged.stdout,
        )

        with open(os.path.join(solo, "g.txt"), "w") as fh:
            fh.write("two\n")
        sh([ZF, "changeset", "create", "two", "--author", "t"], solo)
        with open(os.path.join(solo, "h.txt"), "w") as fh:
            fh.write("three\n")
        sh([ZF, "changeset", "create", "three", "--author", "t"], solo)
        r = sh([ZF, "changeset", "commit"], solo)
        out = r.stdout + r.stderr
        record("commit with no id refuses when there are several", r.returncode != 0, out)
        record(
            "the refusal lists the ids so the next call can name one",
            out.count("cs") >= 2,
            out,
        )

        # Nothing pending: a fresh repository with a changed file but no snapshot. The previous
        # two changesets are still pending in `solo`, so this needs its own repository - committed
        # state and pending state are different questions.
        clear = os.path.join(base, "clear")
        os.makedirs(clear)
        sh([ZF, "init"], clear)
        commit(clear, "one\n", "one")
        with open(os.path.join(clear, "i.txt"), "w") as fh:
            fh.write("four\n")
        r = sh([ZF, "changeset", "commit"], clear)
        out = r.stdout + r.stderr
        record(
            "commit with nothing pending says how to make one",
            "gesta changeset create" in out,
            out,
        )

        # checkout defaults to the whole tree, restore refuses without an argument. The
        # asymmetry is deliberate: writing the branch out cannot lose work, discarding cannot be
        # undone. Both are checked, because a default that quietly flips is the failure mode.
        # `commit()` writes f.txt, so that is the tracked path that gets modified and put back;
        # a file this test invents is untracked, and neither command owes it anything.
        tree = os.path.join(base, "tree")
        os.makedirs(tree)
        sh([ZF, "init"], tree)
        commit(tree, "tracked\n", "one")
        with open(os.path.join(tree, "f.txt"), "w") as fh:
            fh.write("MODIFIED\n")
        with open(os.path.join(tree, "untracked.txt"), "w") as fh:
            fh.write("keepme\n")

        r = sh([ZF, "checkout"], tree)
        record(
            "checkout with no argument writes the whole tree out",
            r.returncode == 0 and "restored" in (r.stdout + r.stderr),
            r.stdout + r.stderr,
        )
        with open(os.path.join(tree, "f.txt")) as fh:
            body = fh.read()
        record("checkout put the tracked file back", body == "tracked\n", body)

        with open(os.path.join(tree, "f.txt"), "w") as fh:
            fh.write("MOD2\n")
        r = sh([ZF, "restore"], tree)
        out = r.stdout + r.stderr
        record("restore with no argument refuses", r.returncode != 0, out)
        with open(os.path.join(tree, "f.txt")) as fh:
            still = fh.read()
        record("the refusal left the edit in place", still == "MOD2\n", still)
        record(
            "the refusal names the form that works",
            "gesta restore ." in out,
            out,
        )

        # An untracked file is not the tool's to delete, on either path.
        r = sh([ZF, "restore", "."], tree)
        kept = os.path.exists(os.path.join(tree, "untracked.txt"))
        record(
            "neither command deletes a file the user made",
            r.returncode == 0 and kept,
            f"rc={r.returncode} kept={kept} {r.stdout}{r.stderr}",
        )

        passed = sum(1 for _, ok in checks if ok)
        print(f"\n{passed}/{len(checks)} refusal checks pass")
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
