#!/usr/bin/env python3
"""Push two branches in one request, then read both back with a real git client.

`bench/forge_push.py` proves one branch arrives. This one exists because the client could only send one
ref per request, so a repository with a `main` and a `release` needed two invocations and two packs, and
the second re-sent every object the first had already delivered. The failure this catches is not "the
push failed" but "the push said it succeeded and moved one of the two refs": a report is read per
command, and a client that reports the first name as if it spoke for all of them turns a partial push
into a success message.

Both branches are read back with git, because the objects have to arrive and not just the refs.

The git binary is needed for the read-back and only there. Without it the step skips, since a machine
without git can still push.
"""

import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time

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


def commit_all(where, message):
    """Snapshot and commit, returning the changeset id or None."""
    r = run([ZF, "changeset", "create", message, "--json"], where)
    cid = changeset_id(r.stdout)
    if not cid:
        return None
    run([ZF, "changeset", "commit", cid], where)
    return cid


def main() -> int:
    if not os.path.exists(ZF):
        print(f"binary not found: {ZF}")
        return 1
    if shutil.which("git") is None:
        print("git is not installed, so the push cannot be read back: skipped")
        return 0

    base = tempfile.mkdtemp(prefix="forge-push-all-")
    src = os.path.join(base, "src")
    remote = os.path.join(base, "remote")
    clone = os.path.join(base, "clone")
    for d in (src, remote, clone):
        os.makedirs(d, exist_ok=True)

    server = None
    try:
        run([ZF, "init"], src)
        with open(os.path.join(src, "main.txt"), "w") as fh:
            fh.write("on main\n")
        if not commit_all(src, "main work"):
            print("could not commit to main")
            return 1

        # A second branch with its own commit on top of the first, so the two refs point at different
        # commits and a push that silently sent only one of them is visible.
        run([ZF, "branch", "create", "release"], src)
        run([ZF, "branch", "switch", "release"], src)
        with open(os.path.join(src, "release.txt"), "w") as fh:
            fh.write("on release\n")
        if not commit_all(src, "release work"):
            print("could not commit to release")
            return 1

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
        p = run([ZF, "forge", "push", url, "--all"], src, env={"GESTA_FORGE_TOKEN": "dummy"})
        if p.returncode != 0:
            print(f"the push failed: {p.stdout}{p.stderr}")
            return 1

        # Both refs have to be there. One is the failure this bench is about.
        for branch, want_file, want_body in (
            ("main", "main.txt", "on main\n"),
            ("release", "release.txt", "on release\n"),
        ):
            c = run(["git", "clone", "-q", "--branch", branch, url, os.path.join(clone, branch)], base)
            if c.returncode != 0:
                print(f"a clone of {branch} failed: {c.stderr}")
                return 1
            path = os.path.join(clone, branch, want_file)
            if not os.path.exists(path):
                have = sorted(os.listdir(os.path.join(clone, branch)))
                print(f"{branch} arrived without {want_file}: it holds {have}")
                return 1
            with open(path) as fh:
                body = fh.read()
            if body != want_body:
                print(f"{branch}:{want_file} arrived as {body!r}, expected {want_body!r}")
                return 1

        # A second push with nothing new to say has to report that rather than claiming work.
        again = run([ZF, "forge", "push", url, "--all"], src, env={"GESTA_FORGE_TOKEN": "dummy"})
        if again.returncode != 0:
            print(f"a second push of unchanged branches failed: {again.stdout}{again.stderr}")
            return 1
        if "already" not in again.stdout:
            print(f"a push with nothing to send did not say so: {again.stdout!r}")
            return 1

        print("both branches arrived in one request, and a repeat push says so")
        return 0
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
