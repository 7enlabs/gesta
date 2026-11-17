#!/usr/bin/env python3
"""Check the behaviours a daily source control user relies on.

A speed number says nothing about whether a tool can be used instead of another.
Each check runs the real binary and looks at the observable outcome.

Run: python3 bench/parity.py
"""

import os
import re
import shutil
import subprocess
import sys

GESTA = os.environ.get("GESTA_BIN", "/work/gesta/target/release/gesta")
ROOT = "/tmp/gesta_parity"
results = []


def sh(cmd, cwd=None):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True, cwd=cwd)


def check(name, ok, detail=""):
    results.append((name, ok, detail))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}")
    if not ok and detail:
        print(f"         {detail.strip()[:200]}")


def commit(root, msg="m", author="bench"):
    o = sh(f"{GESTA} changeset create {msg} --author {author}", cwd=root)
    m = re.search(r"\b(cs[0-9a-f]{6,})\b", o.stdout)
    if not m:
        return None
    sh(f"{GESTA} changeset commit {m.group(1)}", cwd=root)
    return m.group(1)


def main():
    if not os.path.exists(GESTA):
        sys.exit(f"binary not found: {GESTA}")
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(ROOT)

    # A working repository to test against.
    repo = os.path.join(ROOT, "repo")
    os.makedirs(repo)
    sh(f"{GESTA} init", cwd=repo)
    os.makedirs(os.path.join(repo, "src"))
    with open(os.path.join(repo, "src", "a.txt"), "w") as fh:
        fh.write("one\ntwo\nthree\n")
    commit(repo, "first")

    # 1. The basics any user does first.
    check("init creates a repository", os.path.isdir(os.path.join(repo, ".gesta")))
    out = sh(f"{GESTA} status", cwd=repo).stdout
    check("status reports the branch", "On branch" in out, out)

    # 2. A change can be recorded and read back.
    with open(os.path.join(repo, "src", "a.txt"), "a") as fh:
        fh.write("four\n")
    out = sh(f"{GESTA} status", cwd=repo).stdout
    check("status shows a modified file", "modified" in out.lower(), out)
    out = sh(f"{GESTA} diff", cwd=repo).stdout
    check("diff shows the added line", "+four" in out, out)
    commit(repo, "second")
    out = sh(f"{GESTA} log", cwd=repo).stdout
    check("log lists both changesets", out.count("cs") >= 2, out)

    # 3. Undo, which is the reason to use an operation log at all.
    # The head is what undo moves, so read the head rather than the log text.
    head = lambda: sh(f"{GESTA} status", cwd=repo).stdout
    before = head()
    r = sh(f"{GESTA} undo", cwd=repo)
    after = head()
    check(
        "undo moves the head back",
        r.returncode == 0 and before != after and "HEAD" in after,
        r.stdout + r.stderr,
    )

    # 4. A branch can be created, switched to, and merged.
    sh(f"{GESTA} branch create feature", cwd=repo)
    out = sh(f"{GESTA} branch list", cwd=repo).stdout
    check("branch is created", "feature" in out, out)
    sh(f"{GESTA} branch switch feature", cwd=repo)
    with open(os.path.join(repo, "feature.txt"), "w") as fh:
        fh.write("on the branch\n")
    commit(repo, "branch work")
    sh(f"{GESTA} branch switch main", cwd=repo)
    r = sh(f"{GESTA} branch merge feature", cwd=repo)
    check("branch merge succeeds", r.returncode == 0, r.stdout + r.stderr)

    # 5. History queries an agent depends on.
    out = sh(f"{GESTA} blame src/a.txt", cwd=repo).stdout
    check("blame names an author per line", "bench" in out, out)
    out = sh(f"{GESTA} history src/a.txt", cwd=repo).stdout
    # history prints a short hash per version, not the changeset id
    check("history lists versions of a file", "v1" in out and "v2" in out, out)
    out = sh(f"{GESTA} show main:src/a.txt", cwd=repo)
    check("show reads a file out of history", "one" in out.stdout, out.stdout + out.stderr)

    # 6. Structured output, without which an agent has to parse prose.
    out = sh(f"{GESTA} status --json", cwd=repo).stdout
    check("status --json emits JSON", out.strip().startswith("{"), out[:120])
    out = sh(f"{GESTA} log --json", cwd=repo).stdout
    check("log --json emits JSON", out.strip().startswith("["), out[:120])

    # 7. Network transport exists.
    sh(f"{GESTA} init --bare {os.path.join(ROOT, 'bare')}", cwd=ROOT)
    r = sh(f"{GESTA} remote add origin {os.path.join(ROOT, 'bare')}", cwd=repo)
    check("remote add accepts a path", r.returncode == 0, r.stderr)
    r = sh(f"{GESTA} remote push origin", cwd=repo)
    check("remote push transfers history", r.returncode == 0, r.stdout + r.stderr)

    # 8. Integrity check, the thing that makes a store trustworthy.
    out = sh(f"{GESTA} fsck", cwd=repo)
    check("fsck passes on a healthy repository", out.returncode == 0, out.stdout + out.stderr)

    # 9. Saving work aside without committing it.
    with open(os.path.join(repo, "src", "a.txt"), "a") as fh:
        fh.write("wip line\n")
    with open(os.path.join(repo, "scratch.txt"), "w") as fh:
        fh.write("temporary\n")
    r = sh(f"{GESTA} stash save --message wip", cwd=repo)
    check("stash saves the working tree", r.returncode == 0, r.stdout + r.stderr)
    out = sh(f"{GESTA} status", cwd=repo).stdout
    check("stash leaves a clean tree", "clean" in out.lower(), out)
    check(
        "stash removed the added file from disk",
        not os.path.exists(os.path.join(repo, "scratch.txt")),
    )
    out = sh(f"{GESTA} stash list", cwd=repo).stdout
    check("stash list shows the entry", "wip" in out, out)
    r = sh(f"{GESTA} stash pop", cwd=repo)
    check("stash pop restores the work", r.returncode == 0, r.stdout + r.stderr)
    restored = open(os.path.join(repo, "src", "a.txt")).read()
    check("the stashed edit is back", "wip line" in restored, restored)
    check(
        "the stashed new file is back",
        os.path.exists(os.path.join(repo, "scratch.txt")),
    )

    # Pop must refuse rather than overwrite a local change.
    with open(os.path.join(repo, "src", "a.txt"), "a") as fh:
        fh.write("second wip\n")
    sh(f"{GESTA} stash save --message second", cwd=repo)
    with open(os.path.join(repo, "src", "a.txt"), "a") as fh:
        fh.write("local only\n")
    out = sh(f"{GESTA} stash pop", cwd=repo).stdout
    check("stash pop refuses over a local change", "not applied" in out, out)
    check(
        "the local change survives the refusal",
        "local only" in open(os.path.join(repo, "src", "a.txt")).read(),
    )

    # 10. Hooks.
    out = sh(f"{GESTA} hook list", cwd=repo).stdout
    check("hook list names pre-commit", "pre-commit" in out, out)

    hook = os.path.join(ROOT, "refuse.sh")
    with open(hook, "w") as fh:
        fh.write("#!/bin/sh\necho refusing on purpose >&2\nexit 1\n")
    sh(f"{GESTA} hook install pre-commit --file {hook}", cwd=repo)
    with open(os.path.join(repo, "src", "a.txt"), "a") as fh:
        fh.write("hook test\n")
    r = sh(f"{GESTA} changeset create hooktest --author bench", cwd=repo)
    m = re.search(r"\b(cs[0-9a-f]{6,})\b", r.stdout)
    head_before = sh(f"{GESTA} status", cwd=repo).stdout
    if m:
        r = sh(f"{GESTA} changeset commit {m.group(1)}", cwd=repo)
        check("a failing pre-commit hook blocks the commit", r.returncode != 0, r.stdout + r.stderr)
    else:
        check("a failing pre-commit hook blocks the commit", False, r.stdout)
    head_after = sh(f"{GESTA} status", cwd=repo).stdout
    check(
        "the blocked commit left the head unchanged",
        [l for l in head_before.splitlines() if "HEAD" in l]
        == [l for l in head_after.splitlines() if "HEAD" in l],
        head_after,
    )

    passing = os.path.join(ROOT, "pass.sh")
    with open(passing, "w") as fh:
        fh.write("#!/bin/sh\nexit 0\n")
    sh(f"{GESTA} hook install pre-commit --file {passing}", cwd=repo)
    if m:
        r = sh(f"{GESTA} changeset commit {m.group(1)}", cwd=repo)
        check("a passing pre-commit hook lets the commit through", r.returncode == 0, r.stdout + r.stderr)

    failing_post = os.path.join(ROOT, "post.sh")
    with open(failing_post, "w") as fh:
        fh.write("#!/bin/sh\nprintenv > /dev/null\nexit 9\n")
    sh(f"{GESTA} hook install post-commit --file {failing_post}", cwd=repo)
    with open(os.path.join(repo, "src", "a.txt"), "a") as fh:
        fh.write("post test\n")
    r = sh(f"{GESTA} changeset create posttest --author bench", cwd=repo)
    m2 = re.search(r"\b(cs[0-9a-f]{6,})\b", r.stdout)
    if m2:
        r = sh(f"{GESTA} changeset commit {m2.group(1)}", cwd=repo)
        check("a failing post-commit hook reports failure", r.returncode != 0, r.stdout + r.stderr)
        log = sh(f"{GESTA} log --limit 1", cwd=repo).stdout
        check("the commit survives a failing post-commit hook", "posttest" in log, log)
    sh(f"{GESTA} hook remove pre-commit", cwd=repo)
    sh(f"{GESTA} hook remove post-commit", cwd=repo)

    # 11. Bisect finds the changeset that introduced a fault.
    bs = os.path.join(ROOT, "bisect")
    os.makedirs(bs)
    sh(f"{GESTA} init", cwd=bs)
    ids = []
    for i in range(1, 13):
        # Every step has to change the content, or there is nothing to commit and
        # the history never gets long enough to search.
        tag = "good" if i < 7 else "FAULT"
        body = f"version {i} {tag}\n"
        with open(os.path.join(bs, "f.txt"), "w") as fh:
            fh.write(body)
        r = sh(f"{GESTA} changeset create v{i} --author bench", cwd=bs)
        m = re.search(r"\b(cs[0-9a-f]{6,})\b", r.stdout)
        if m:
            sh(f"{GESTA} changeset commit {m.group(1)}", cwd=bs)
            ids.append(m.group(1))

    if len(ids) == 12:
        r = sh(f"{GESTA} bisect start {ids[0]} {ids[-1]}", cwd=bs)
        check("bisect starts", r.returncode == 0, r.stdout + r.stderr)

        questions = 0
        found = None
        while questions < 8:
            body = open(os.path.join(bs, "f.txt")).read()
            ok = "FAULT" not in body
            questions += 1
            out = sh(f"{GESTA} bisect {'good' if ok else 'bad'}", cwd=bs).stdout
            if "first bad changeset" in out:
                found = [l for l in out.splitlines() if l.startswith("first bad")][0].split()[-1]
                break
        # v7 is the first bad one, which is ids[6].
        check("bisect names the first bad changeset", found == ids[6], f"got {found}, want {ids[6]}")
        check(
            "bisect asks a logarithmic number of questions",
            questions <= 6,
            f"took {questions} questions for 12 changesets",
        )
        body = open(os.path.join(bs, "f.txt")).read()
        check("bisect restores the working tree", "FAULT" in body, body)
        out = sh(f"{GESTA} bisect status", cwd=bs).stdout
        check("bisect is finished after answering", "no bisect running" in out, out)

        # Cancelling must also restore, which is what makes it safe to interrupt.
        sh(f"{GESTA} bisect start {ids[0]} {ids[-1]}", cwd=bs)
        r = sh(f"{GESTA} bisect cancel", cwd=bs)
        check("bisect cancel succeeds", r.returncode == 0, r.stdout + r.stderr)
        body = open(os.path.join(bs, "f.txt")).read()
        check("bisect cancel restores the tree", "FAULT" in body, body)
    else:
        check("bisect could build its history", False, f"only {len(ids)} changesets")

    # 12. Cherry-pick moves one change onto another branch.
    cp = os.path.join(ROOT, "pick")
    os.makedirs(cp)
    sh(f"{GESTA} init", cwd=cp)
    with open(os.path.join(cp, "f.txt"), "w") as fh:
        fh.write("one\ntwo\nthree\n")
    r = sh(f"{GESTA} changeset create base --author bench", cwd=cp)
    m = re.search(r"\b(cs[0-9a-f]{6,})\b", r.stdout)
    base_id = m.group(1) if m else None
    if base_id:
        sh(f"{GESTA} changeset commit {base_id}", cwd=cp)

    sh(f"{GESTA} branch create feature", cwd=cp)
    sh(f"{GESTA} branch switch feature", cwd=cp)
    with open(os.path.join(cp, "f.txt"), "w") as fh:
        fh.write("one\nTWO-PICKED\nthree\n")
    r = sh(f"{GESTA} changeset create fix --author bench", cwd=cp)
    m = re.search(r"\b(cs[0-9a-f]{6,})\b", r.stdout)
    fix_id = m.group(1) if m else None
    if fix_id:
        sh(f"{GESTA} changeset commit {fix_id}", cwd=cp)

    sh(f"{GESTA} branch switch main", cwd=cp)
    before = open(os.path.join(cp, "f.txt")).read()
    check("the branch does not have the change before picking", "TWO-PICKED" not in before, before)

    if fix_id:
        r = sh(f"{GESTA} cherry-pick {fix_id}", cwd=cp)
        check("cherry-pick succeeds", r.returncode == 0, r.stdout + r.stderr)
        after = open(os.path.join(cp, "f.txt")).read()
        check("cherry-pick applied the change", "TWO-PICKED" in after, after)

        log = sh(f"{GESTA} log --limit 1", cwd=cp).stdout
        check("the pick was recorded with its own message", "pick" in log, log)

        # Picking the same change again must not create a second changeset.
        count_before = sh(f"{GESTA} log --limit 50 --json", cwd=cp).stdout.count('"id"')
        r = sh(f"{GESTA} cherry-pick {fix_id}", cwd=cp)
        count_after = sh(f"{GESTA} log --limit 50 --json", cwd=cp).stdout.count('"id"')
        check(
            "picking an already applied change records nothing",
            count_before == count_after,
            f"{count_before} -> {count_after}",
        )
    else:
        check("cherry-pick could build its branches", False, "no changeset id")

    # A conflicting pick must refuse and leave the head alone.
    with open(os.path.join(cp, "f.txt"), "w") as fh:
        fh.write("one\nMAIN-ONLY\nthree\n")
    r = sh(f"{GESTA} changeset create mainedit --author bench", cwd=cp)
    m = re.search(r"\b(cs[0-9a-f]{6,})\b", r.stdout)
    if m:
        sh(f"{GESTA} changeset commit {m.group(1)}", cwd=cp)
    head_before = sh(f"{GESTA} status", cwd=cp).stdout
    if fix_id:
        r = sh(f"{GESTA} cherry-pick {fix_id}", cwd=cp)
        check("a conflicting pick refuses", r.returncode != 0, r.stdout + r.stderr)
    head_after = sh(f"{GESTA} status", cwd=cp).stdout
    check(
        "a refused pick leaves the head alone",
        [l for l in head_before.splitlines() if "HEAD" in l]
        == [l for l in head_after.splitlines() if "HEAD" in l],
        head_after,
    )
    check(
        "a refused pick keeps the local content",
        "MAIN-ONLY" in open(os.path.join(cp, "f.txt")).read(),
    )

    # 13. Things a daily user will look for and not find.
    for missing in ("rebase", "submodule"):
        r = sh(f"{GESTA} {missing} --help", cwd=repo)
        check(f"{missing} is absent (known gap)", r.returncode != 0)

    print()
    passed = sum(1 for _, ok, _ in results if ok)
    print(f"{passed}/{len(results)} checks pass")
    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
