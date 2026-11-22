#!/usr/bin/env python3
"""Differential test: gesta and git given the same work, compared by git's own reader.

Every other benchmark here checks that this tool does what this tool intends. That can pass while the
answer is wrong, because the intention and the check are written by the same person. This one takes the
answer away: a scenario is played twice, once with gesta and once with git, and both results are read
back with the git binary.

That is the only honest form of the parity claim. If gesta exports a tree that git can read and the file
contents match what git produced for the same edits, the export is right regardless of what the tool
believes about itself.

Run: python3 bench/differential.py
"""

import os
import shutil
import subprocess
import sys
import tempfile

_PROJECT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
GESTA = os.environ.get("GESTA_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))

results = []


def run(args, cwd, env=None):
    e = dict(os.environ)
    if env:
        e.update(env)
    return subprocess.run(args, cwd=cwd, capture_output=True, text=True, env=e, timeout=180)


def run_bytes(args, cwd, env=None):
    """The same runner without decoding.

    `text=True` decodes stdout, so reading a binary blob through it raises UnicodeDecodeError rather
    than comparing anything. Blob contents have to be read as bytes, because the whole point of the
    check is that they were not re-encoded on the way through.
    """
    e = dict(os.environ)
    if env:
        e.update(env)
    return subprocess.run(args, cwd=cwd, capture_output=True, env=e, timeout=180)


def record(name, ok, detail=""):
    results.append((name, ok, detail))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}")
    if not ok and detail:
        for line in detail.strip().splitlines()[:6]:
            print(f"         {line}")


def git(args, cwd):
    return run(["git"] + args, cwd)


def gesta(args, cwd):
    return run([GESTA] + args, cwd)


def gesta_commit(root, msg):
    """Create and commit one changeset, returning the id."""
    o = gesta(["changeset", "create", msg, "--json"], root)
    if o.returncode != 0:
        return None, o.stdout + o.stderr
    import json

    try:
        d = json.loads(o.stdout)
    except json.JSONDecodeError:
        return None, o.stdout
    cid = d.get("changeset")
    if not cid:
        return None, o.stdout
    c = gesta(["changeset", "commit", cid], root)
    if c.returncode != 0:
        return None, c.stdout + c.stderr
    return cid, ""


def read_tree(root):
    """Every file under root, as path to bytes, skipping .git and .gesta."""
    out = {}
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in (".git", ".gesta")]
        for f in filenames:
            p = os.path.join(dirpath, f)
            rel = os.path.relpath(p, root)
            with open(p, "rb") as fh:
                out[rel] = fh.read()
    return out


def scenario_edits():
    """The same sequence of edits applied to both tools.

    Chosen to cover the shapes a real edit takes rather than a happy path. Every entry here is a shape
    that has a way to go wrong silently, which is why it is in the list rather than a fifth variation
    of a text file:

      created, modified, removed, renamed   the four operations a commit is made of
      nested directory                      a tree that is not flat
      no trailing newline                   the byte a diff tool likes to add
      empty file                            a blob with no content, which some writers skip
      binary bytes                          a body that is not UTF-8 and must not be re-encoded
      name with a space and a unicode char  quoting in the tree writer
      executable bit                        a mode, not a content, that git records and readers lose
      many files at once                    a tree wide enough to exercise ordering
    """
    return [
        ("create a.txt", lambda r: _write(r, "a.txt", b"alpha\n")),
        ("create nested/b.txt", lambda r: _write(r, "nested/b.txt", b"beta\n")),
        ("modify a.txt", lambda r: _write(r, "a.txt", b"alpha\nsecond\n")),
        ("rename nested/b.txt to nested/c.txt", _rename),
        ("delete a.txt", lambda r: os.remove(os.path.join(r, "a.txt"))),
        ("create no newline at end", lambda r: _write(r, "d.txt", b"no trailing newline")),
        ("create an empty file", lambda r: _write(r, "empty.txt", b"")),
        (
            "create a binary file",
            lambda r: _write(r, "blob.bin", bytes(range(256)) * 4),
        ),
        (
            "create a name with a space and a non-ascii character",
            lambda r: _write(r, "dir with space/t\u00ean file.txt", b"unicode name\n"),
        ),
        ("create an executable script", _exec_script),
        (
            "create twenty files at once",
            lambda r: [_write(r, f"bulk/f{i:02d}.txt", f"bulk {i}\n".encode()) for i in range(20)],
        ),
    ]


def _exec_script(root):
    p = os.path.join(root, "run.sh")
    with open(p, "wb") as fh:
        fh.write(b"#!/bin/sh\necho hi\n")
    os.chmod(p, 0o755)


def _write(root, rel, body):
    p = os.path.join(root, rel)
    os.makedirs(os.path.dirname(p), exist_ok=True) if os.path.dirname(p) else None
    with open(p, "wb") as fh:
        fh.write(body)


def _rename(root):
    src = os.path.join(root, "nested", "b.txt")
    dst = os.path.join(root, "nested", "c.txt")
    if os.path.exists(src):
        os.rename(src, dst)


def main() -> int:
    if not os.path.exists(GESTA):
        print(f"binary not found: {GESTA}")
        return 1
    if shutil.which("git") is None:
        print("git is not installed, so there is nothing to compare against: skipped")
        return 0

    base = tempfile.mkdtemp(prefix="differential-")
    gd = os.path.join(base, "gesta")
    gtd = os.path.join(base, "git")
    os.makedirs(gd)
    os.makedirs(gtd)

    try:
        # Two empty repositories, one per tool.
        if gesta(["init"], gd).returncode != 0:
            print("gesta init failed")
            return 1
        git(["init", "-q", "."], gtd)
        git(["config", "user.email", "bench@local"], gtd)
        git(["config", "user.name", "bench"], gtd)

        # Apply the same edits to both, committing each step.
        for label, fn in scenario_edits():
            fn(gd)
            fn(gtd)
            cid, err = gesta_commit(gd, label)
            if cid is None:
                record(f"gesta commits: {label}", False, err)
                continue
            a = git(["add", "-A"], gtd)
            c = git(["commit", "-qm", label], gtd)
            if a.returncode != 0 or c.returncode != 0:
                record(f"git commits: {label}", False, a.stderr + c.stderr)
                continue

        # gesta exports to a bare git repository; git already is one.
        bare = os.path.join(base, "exported")
        run(["git", "init", "-q", "--bare", bare], base)
        e = gesta(["git", "export", bare], gd)
        record("gesta exports to a bare git repository", e.returncode == 0, e.stdout + e.stderr)

        if e.returncode == 0:
            # Read both heads with git, which knows nothing about which tool wrote them.
            gt_head = git(["rev-parse", "HEAD"], gtd).stdout.strip()
            ge_head = git(["--git-dir", bare, "rev-parse", "refs/heads/main"], base).stdout.strip()

            # The tree at each head, as git lists it.
            def ls(gitdir_arg):
                t = git(gitdir_arg + ["ls-tree", "-r", "--long", "HEAD"], gtd if not gitdir_arg else base)
                return t.stdout

            gt_tree = git(["ls-tree", "-r", "--long", "HEAD"], gtd).stdout
            ge_tree = run(
                ["git", "--git-dir", bare, "ls-tree", "-r", "--long", "refs/heads/main"], base
            ).stdout

            def norm(text):
                # The long form carries mode, type, blob id, size and path. Modes and ids must match if
                # the contents match, which is the point: a lost executable bit keeps the bytes and
                # changes the mode, and a lost blob changes the id.
                out = []
                for line in text.strip().splitlines():
                    if not line.strip():
                        continue
                    meta, path = line.split("\t", 1) if "\t" in line else (line, "")
                    out.append((meta.strip(), path))
                return sorted(out)

            record(
                "the exported tree lists the same paths, modes, sizes and blob ids as git's",
                norm(gt_tree) == norm(ge_tree),
                f"git:\n{gt_tree}\ngesta:\n{ge_tree}",
            )

            # Read file contents out of the export with git and compare to git's own. This is the check
            # a ref movement cannot fake, and it is byte-exact: `git cat-file -p` returns the blob as it
            # is stored, so a body that was re-encoded (a binary, a file with no trailing newline) shows
            # up here and nowhere else.
            for rel in (
                "nested/c.txt",
                "d.txt",
                "empty.txt",
                "blob.bin",
                "dir with space/t\u00ean file.txt",
                "run.sh",
                "bulk/f00.txt",
                "bulk/f19.txt",
            ):
                a = run_bytes(["git", "--git-dir", bare, "cat-file", "-p", f"refs/heads/main:{rel}"], base)
                b = run_bytes(["git", "cat-file", "-p", f"HEAD:{rel}"], gtd)
                record(
                    f"the exported bytes of {rel} match git's",
                    a.returncode == 0 and a.stdout == b.stdout,
                    f"export rc={a.returncode} err={a.stderr.decode('utf-8', 'replace')}",
                )

            # The executable bit is a mode, not content, and it is the thing a tree writer loses while
            # every byte checks out.
            a = run(["git", "--git-dir", bare, "ls-tree", "refs/heads/main", "run.sh"], base)
            b = run(["git", "ls-tree", "HEAD", "run.sh"], gtd)
            mode_a = a.stdout.split()[0] if a.stdout.strip() else ""
            mode_b = b.stdout.split()[0] if b.stdout.strip() else ""
            record(
                "the executable bit survives the export",
                mode_a == mode_b == "100755",
                f"export mode={mode_a!r} git mode={mode_b!r}",
            )

            # An empty file is a blob of length zero, and a writer that skips empty bodies drops it.
            a = run_bytes(["git", "--git-dir", bare, "cat-file", "-p", "refs/heads/main:empty.txt"], base)
            b = run_bytes(["git", "cat-file", "-p", "HEAD:empty.txt"], gtd)
            record(
                "an empty file is present in both and carries no bytes",
                a.returncode == 0 and b.returncode == 0 and a.stdout == b"" and b.stdout == b"",
                f"export rc={a.returncode} git rc={b.returncode}",
            )

            # A file deleted in both must be absent from both.
            a = run(["git", "--git-dir", bare, "cat-file", "-e", "refs/heads/main:a.txt"], base)
            b = git(["cat-file", "-e", "HEAD:a.txt"], gtd)
            record(
                "a file deleted in both is absent from both",
                a.returncode != 0 and b.returncode != 0,
                f"export rc={a.returncode} git rc={b.returncode}",
            )

            # And a clone of the export equals a clone of git, file for file.
            cl1 = os.path.join(base, "clone-export")
            cl2 = os.path.join(base, "clone-git")
            c1 = run(["git", "clone", "-q", bare, cl1], base)
            c2 = run(["git", "clone", "-q", gtd, cl2], base)
            if c1.returncode == 0 and c2.returncode == 0:
                t1 = read_tree(cl1)
                t2 = read_tree(cl2)
                record(
                    "a clone of the export holds the same files and bytes as a clone of git",
                    t1 == t2,
                    f"only in export: {set(t1) - set(t2)}\nonly in git: {set(t2) - set(t1)}",
                )
            else:
                record("both clones succeed", False, c1.stderr + c2.stderr)

            # A bare export whose branch is not `main`. `git init --bare` puts HEAD on
            # `refs/heads/master`, so a tool that leaves HEAD alone after writing `refs/heads/trunk`
            # produces a repository that clones empty while every object is present. This branch name
            # is the one that catches that, and it is why the check exists rather than trusting the
            # `main` case.
            #
            # Verified to catch it: with the HEAD write reverted to the pre-#58 behaviour, this run is
            # 5/7 and both clone checks fail. A test that passes before the fix proves nothing.
            bare2 = os.path.join(base, "exported-trunk")
            run(["git", "init", "-q", "--bare", bare2], base)
            cb = gesta(["branch", "create", "trunk"], gd)
            tk = gesta(["branch", "switch", "trunk"], gd) if cb.returncode == 0 else cb
            if tk.returncode == 0:
                _write(gd, "trunkonly.txt", b"on trunk\n")
                cid2, err2 = gesta_commit(gd, "on trunk")
                if cid2 is None:
                    record("gesta commits on trunk", False, err2)
                else:
                    e2 = gesta(["git", "export", bare2, "--branch", "trunk"], gd)
                    if e2.returncode != 0:
                        record("gesta exports a non-main branch", False, e2.stdout + e2.stderr)
                    else:
                        cl3 = os.path.join(base, "clone-trunk")
                        c3 = run(["git", "clone", "-q", bare2, cl3], base)
                        got = read_tree(cl3) if c3.returncode == 0 else {}
                        record(
                            "a bare export of a branch not named main clones to its files",
                            "trunkonly.txt" in got and got["trunkonly.txt"] == b"on trunk\n",
                            f"clone rc={c3.returncode} err={c3.stderr} files={sorted(got)}",
                        )

        passed = sum(1 for _, ok, _ in results if ok)
        total = len(results)
        print(f"\n{passed}/{total} differential checks pass")
        if passed != total:
            print("\nfailing:")
            for n, ok, _ in results:
                if not ok:
                    print(f"  - {n}")
        return 0 if passed == total else 1
    finally:
        shutil.rmtree(base, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
