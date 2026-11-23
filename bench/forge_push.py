#!/usr/bin/env python3
"""Push to this tool's own git server using only the forge client, then read it back with git.

This is the one step that proves the forge client works end to end rather than in parts: a store is
built, served over HTTP, pushed to with `forge push`, and the result is cloned with a real git client
and its file contents compared. Every piece of that already existed except the client, so a failure
here is a failure in one of the three requests a push is made of.

The git binary is needed for the final clone and only there. If it is absent the step skips rather than
fails, because a machine without git can still push to a forge; it just cannot be checked this way.
"""

import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time

# The binary is looked up relative to this file, not hardcoded: the gate runs this on a machine where
# the checkout is somewhere else, and an absolute path from the author's container makes the step fail
# with "binary not found" in a way that looks like a defect in the code rather than in the benchmark.
_PROJECT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ZF = os.environ.get("GESTA_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))


def free_port() -> int:
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def run(args, cwd, env=None, timeout=120):
    e = dict(os.environ)
    if env:
        e.update(env)
    return subprocess.run(args, cwd=cwd, capture_output=True, text=True, env=e, timeout=timeout)


def changeset_id(out: str):
    m = re.search(r"\b(cs[0-9a-f]{6,})\b", out)
    return m.group(1) if m else None


def real_forge_check() -> int:
    """Push to a real forge and read it back, when a token and a repository are supplied.

    The local server above never chunks its responses, so it cannot catch the class of defect that
    broke GitHub: a chunked advertisement reads its chunk length as a pkt-line length, the ref list
    comes back empty, the push claims every ref is new, and the forge refuses it as a
    non-fast-forward. Only a server that actually chunks - GitHub, GitLab behind nginx - exercises
    that path, so this half runs against one or skips.

    Skipped rather than failed when either is absent: a missing credential is not a defect here, and
    a gate that goes red for it teaches people to ignore the gate.
    """
    token = os.environ.get("GESTA_FORGE_TOKEN", "")
    repo = os.environ.get("GESTA_FORGE_TEST_REPO", "")
    if not token or not repo:
        print("\nskipped the real-forge half: set GESTA_FORGE_TOKEN and GESTA_FORGE_TEST_REPO")
        return 0
    if shutil.which("git") is None:
        print("\nskipped the real-forge half: git is not installed")
        return 0

    # `owner/name` is short enough to type but is not a host; GitHub is assumed for it, because that
    # is the forge every caller so far has meant. Anything with a dot in the first segment is a host.
    host_path = repo if "/" in repo and "." in repo.split("/")[0] else f"github.com/{repo}"
    probe = subprocess.run(
        ["git", "ls-remote", f"https://x-access-token:{token}@{host_path}.git"],
        capture_output=True,
        text=True,
        timeout=120,
    )
    if probe.returncode != 0:
        print(f"\nskipped the real-forge half: {probe.stderr.strip()[:200]}")
        return 0

    base = tempfile.mkdtemp(prefix="forge-real-")
    src = os.path.join(base, "src")
    clone = os.path.join(base, "clone")
    os.makedirs(src)
    try:
        run([ZF, "init"], src)
        local_ids = []
        for name, body in (("a.txt", "hello forge"), ("b.txt", "second")):
            with open(os.path.join(src, name), "w") as fh:
                fh.write(body + "\n")
            made = run([ZF, "changeset", "create", f"add {name}", "--author", "bench", "--json"], src)
            cid = changeset_id(made.stdout)
            if not cid:
                print(f"  [FAIL] could not create a changeset for {name}")
                return 1
            run([ZF, "changeset", "commit", cid], src)
        # The ids to compare against `git log` are git commit ids, which `gesta log` does not print:
        # it prints this store's own changeset ids (`cs...`). Comparing the two compares namespaces and
        # can never match - the first version of this check reported FAIL while the push was correct.
        # `git export` is the only thing that names the same commits git will compute.
        export = os.path.join(base, "export")
        run([ZF, "git", "export", export], src)
        local_ids = [
            l.split()[0]
            for l in run(["git", "log", "--oneline"], export).stdout.splitlines()
            if l.strip()
        ]

        pushed = run(
            [ZF, "forge", "push", f"https://{host_path}", "main"],
            src,
            env={"GESTA_FORGE_TOKEN": token},
        )
        print(f"\n  [{'PASS' if pushed.returncode == 0 else 'FAIL'}] forge push to {host_path} exits 0")
        if pushed.returncode != 0:
            print(f"         {(pushed.stdout + pushed.stderr).strip()[:300]}")
            return 1

        # The judge is git, not gesta. Everything below reads the remote through a client that shares
        # no code with the one under test.
        got = run(["git", "clone", "-q", f"https://x-access-token:{token}@{host_path}.git", clone], base)
        print(f"  [{'PASS' if got.returncode == 0 else 'FAIL'}] git clone of the pushed repository succeeds")
        if got.returncode != 0:
            print(f"         {got.stderr.strip()[:300]}")
            return 1

        names = sorted(n for n in os.listdir(clone) if n != ".git")
        print(f"  [{'PASS' if names == ['a.txt', 'b.txt'] else 'FAIL'}] the clone holds both files")
        with open(os.path.join(clone, "a.txt")) as fh:
            same = fh.read() == "hello forge\n"
        print(f"  [{'PASS' if same else 'FAIL'}] the clone holds the committed bytes")

        subjects = [
            l
            for l in run(["git", "log", "--format=%s", "main"], clone).stdout.splitlines()
            if l.strip()
        ]
        print(
            f"  [{'PASS' if subjects == ['add b.txt', 'add a.txt'] else 'FAIL'}] "
            "the clone holds both commits, newest first"
        )

        git_ids = [
            l.split()[0]
            for l in run(["git", "log", "--oneline"], clone).stdout.splitlines()
            if l.strip()
        ]
        ids_match = git_ids == local_ids
        print(f"  [{'PASS' if ids_match else 'FAIL'}] the commit ids match what gesta logged")
        if not ids_match:
            print(f"         gesta={local_ids} git={git_ids}")

        # Pushing an unchanged branch has to be a no-op, not a re-send of the same pack.
        again = run(
            [ZF, "forge", "push", f"https://{host_path}", "main"],
            src,
            env={"GESTA_FORGE_TOKEN": token},
        )
        print(
            f"  [{'PASS' if again.returncode == 0 else 'FAIL'}] "
            "pushing an unchanged branch again exits 0"
        )

        return 0 if (got.returncode == 0 and names == ["a.txt", "b.txt"] and same and ids_match) else 1
    finally:
        shutil.rmtree(base, ignore_errors=True)


def main() -> int:
    if not os.path.exists(ZF):
        print(f"binary not found: {ZF}")
        return 1
    if shutil.which("git") is None:
        print("git is not installed, so the push cannot be read back: skipped")
        return 0

    base = tempfile.mkdtemp(prefix="forge-push-")
    src = os.path.join(base, "src")
    remote = os.path.join(base, "remote")
    clone = os.path.join(base, "clone")
    for d in (src, remote, clone):
        os.makedirs(d, exist_ok=True)

    server = None
    try:
        # A store with one committed file.
        run([ZF, "init"], src)
        with open(os.path.join(src, "a.txt"), "w") as fh:
            fh.write("alpha\n")
        r = run([ZF, "changeset", "create", "seed", "--json"], src)
        cid = changeset_id(r.stdout)
        if not cid:
            print(f"could not create a changeset: {r.stdout}{r.stderr}")
            return 1
        run([ZF, "changeset", "commit", cid], src)

        # An empty repository to receive the push, served over HTTP.
        run([ZF, "init"], remote)
        port = free_port()
        server = subprocess.Popen(
            [ZF, "serve", "--bind", "127.0.0.1", "--port", str(port)],
            cwd=remote,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        for _ in range(60):
            try:
                socket.create_connection(("127.0.0.1", port), timeout=1).close()
                break
            except OSError:
                time.sleep(0.1)
        else:
            print("the server never came up")
            return 1

        url = f"http://127.0.0.1:{port}/git/remote"
        p = run(
            [ZF, "forge", "push", url, "main", "--remote-ref", "refs/heads/main"],
            src,
            env={"GESTA_FORGE_TOKEN": "dummy"},
        )
        if p.returncode != 0:
            print(f"the push failed: {p.stdout}{p.stderr}")
            return 1
        if "pushed" not in p.stdout:
            print(f"the push printed no confirmation: {p.stdout}{p.stderr}")
            return 1

        # Read it back with a real git client, which is the only way to know the objects arrived
        # rather than just the ref.
        g = run(["git", "clone", "-q", url, clone], base)
        if g.returncode != 0:
            print(f"git could not clone what the forge client pushed: {g.stderr}")
            return 1
        with open(os.path.join(clone, "a.txt")) as fh:
            body = fh.read()
        if body.strip() != "alpha":
            print(f"the clone has the wrong contents: {body!r}")
            return 1
        log = run(["git", "log", "--oneline"], clone)
        if "seed" not in log.stdout:
            print(f"the clone has no history: {log.stdout}{log.stderr}")
            return 1

        print("a push with the forge client is readable by git")
        print("  the ref moved, the objects arrived, and the file contents match")
        return real_forge_check()
    finally:
        if server is not None:
            server.terminate()
            try:
                server.wait(timeout=10)
            except subprocess.TimeoutExpired:
                server.kill()
        shutil.rmtree(base, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
