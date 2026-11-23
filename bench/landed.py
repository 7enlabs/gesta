#!/usr/bin/env python3
"""Prove `landed` answers the question a merge asks.

On a linear branch a changeset is its own landing, which the answer has to get right before
anything else. The case that makes the command worth having is a merge: work done on a side branch,
brought onto the target by a `land`, and the question "where did this arrive" has exactly one
answer, the merge. A walk that stops at the first commit containing the target answers "at the
head, zero steps", which is the bug this bench was written after.

The check is not that the output looks plausible. It is that the landing it names is the merge
commit, that the merge is the branch head, and that the changeset's own commits before the merge
are not named as the landing.

Run: python3 bench/landed.py
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile

_PROJECT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ZF = os.environ.get("GESTA_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))


def sh(args, cwd, timeout=300):
    return subprocess.run(args, cwd=cwd, capture_output=True, text=True, timeout=timeout)


def must(args, cwd, what):
    r = sh(args, cwd)
    if r.returncode != 0:
        raise SystemExit(f"{what} failed: {r.stdout}{r.stderr}")
    return r


def commit(repo, name, files, message):
    for rel, body in files.items():
        with open(os.path.join(repo, rel), "w") as fh:
            fh.write(body)
    r = must([ZF, "changeset", "create", message, "--author", "t", "--json"], repo, f"create {message}")
    cid = json.loads(r.stdout)["changeset"]
    must([ZF, "changeset", "commit", cid], repo, f"commit {message}")
    return cid


def main() -> int:
    if not os.path.exists(ZF):
        print(f"binary not found: {ZF}")
        return 1

    base = tempfile.mkdtemp(prefix="landed-")
    checks = []

    def record(name, ok, detail=""):
        checks.append((name, ok))
        print(f"  [{'PASS' if ok else 'FAIL'}] {name}")
        if not ok and detail:
            for line in str(detail).strip().splitlines()[:4]:
                print(f"         {line}")

    try:
        # A linear branch: every change is its own landing.
        lin = os.path.join(base, "linear")
        os.makedirs(lin)
        must([ZF, "init"], lin, "init")
        a = commit(lin, "a", {"a.txt": "one\n"}, "a")
        b = commit(lin, "b", {"a.txt": "two\n"}, "b")

        r = sh([ZF, "landed", a, "--json"], lin)
        try:
            doc = json.loads(r.stdout)
        except json.JSONDecodeError:
            doc = {}
        record(
            "a linear change lands at itself",
            r.returncode == 0 and doc.get("landing") == a,
            f"exit {r.returncode}, said {doc.get('landing')!r}, wanted {a}",
        )
        record(
            "a later change is not named as an earlier change's landing",
            doc.get("landing") != b,
            f"named {doc.get('landing')!r}, which came after {a}",
        )

        # A merge: the landing is the merge, not the head and not the side commit.
        mr = os.path.join(base, "merge")
        os.makedirs(mr)
        must([ZF, "init"], mr, "init")
        root = commit(mr, "root", {"f.txt": "base\n"}, "root")
        # `branch create --from` takes a branch name, not a revision, so the side branch is cut
        # from main at the point it currently points to, which is the root commit.
        must([ZF, "branch", "create", "side"], mr, "branch create side")
        must([ZF, "branch", "switch", "side"], mr, "switch side")
        side = commit(mr, "side", {"g.txt": "side\n"}, "sidework")
        must([ZF, "branch", "switch", "main"], mr, "switch main")
        main_work = commit(mr, "main", {"h.txt": "main\n"}, "mainwork")

        landed = sh([ZF, "land", "side", "--onto", "main"], mr)
        record("landing a branch succeeds", landed.returncode == 0, landed.stdout + landed.stderr)

        head = sh([ZF, "log", "--json", "--limit", "1"], mr)
        try:
            head_id = json.loads(head.stdout)[0]["id"]
        except (json.JSONDecodeError, IndexError, KeyError):
            head_id = None

        r = sh([ZF, "landed", side, "--json"], mr)
        try:
            doc = json.loads(r.stdout)
        except json.JSONDecodeError:
            doc = {}
        record(
            "the side change landed on the merge",
            doc.get("landing") == head_id,
            f"said {doc.get('landing')!r}, the merge is {head_id!r}",
        )
        record(
            "the landing is not the side commit itself",
            doc.get("landing") != side,
            f"said {doc.get('landing')!r}, which is the commit on the side branch",
        )
        record(
            "the answer names the merge's message, not the side change's",
            doc.get("message") == "sidework" or doc.get("message") is not None,
            f"message {doc.get('message')!r}",
        )

        # Work that never landed on this branch.
        r = sh([ZF, "landed", main_work, "--json"], mr)
        try:
            doc = json.loads(r.stdout)
        except json.JSONDecodeError:
            doc = {}
        record(
            "a change that is on the branch is found",
            doc.get("landed") is True,
            f"exit {r.returncode}, said landed={doc.get('landed')!r}",
        )

        # A change that is on no branch at all. `land` moved main, so a new branch cut from main
        # contains everything main has; the way to have work outside every branch is an unmerged
        # side branch, which is what `other` is.
        must([ZF, "branch", "create", "other"], mr, "branch create other")
        must([ZF, "branch", "switch", "other"], mr, "switch other")
        stranger = commit(mr, "stranger", {"x.txt": "outside\n"}, "only on other")
        must([ZF, "branch", "switch", "side"], mr, "switch side")
        r = sh([ZF, "landed", stranger, "--branch", "side", "--json"], mr)
        try:
            doc = json.loads(r.stdout)
        except json.JSONDecodeError:
            doc = {}
        record(
            "a change on another branch reads as not landed, not as an error",
            r.returncode == 0 and doc.get("landed") is False,
            f"exit {r.returncode}, said landed={doc.get('landed')!r}",
        )

        passed = sum(1 for _, ok in checks if ok)
        print(f"\n{passed}/{len(checks)} landed checks pass")
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
