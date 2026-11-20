#!/usr/bin/env python3
"""Does the record hold what the machine did, not only what a request asked for?

Before this, audit.log held refusals and served requests. A commit, a sign-off and a push left no
line, so an operator reading the file saw arrivals and nothing about the work. Each is checked here by
doing the thing and reading the line back.
"""
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time

_HERE = os.path.dirname(os.path.abspath(__file__))
_PROJECT = os.path.dirname(_HERE)
ZF = os.environ.get("ZF_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))


def sh(cmd, cwd=None):
    r = subprocess.run(cmd, shell=True, cwd=cwd, capture_output=True, text=True, timeout=180)
    return r.returncode, (r.stdout or "") + (r.stderr or "")


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def wait_port(port, timeout=15.0):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return True
        except OSError:
            time.sleep(0.1)
    return False


def events(d):
    p = os.path.join(d, ".gesta", "audit.log")
    out = []
    if not os.path.exists(p):
        return out
    with open(p, encoding="utf-8", errors="replace") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                out.append(json.loads(line))
            except json.JSONDecodeError:
                # A half line would be a bug; count it so the check can fail rather than skip.
                out.append({"action": "<unparsable>", "raw": line})
    return out


def main():
    d = tempfile.mkdtemp(prefix="audit-local-")
    work = tempfile.mkdtemp(prefix="audit-local-work-")
    try:
        sh(f"{ZF} init", d)
        with open(os.path.join(d, "a.txt"), "w") as fh:
            fh.write("one\n")
        made = sh(f'{ZF} changeset create "first" --author alice --json', d)[1]
        m = re.search(r"(cs[0-9a-f]+)", made)
        cs = m.group(1) if m else ""
        sh(f"{ZF} changeset commit {cs}", d)

        acts = [e.get("action") for e in events(d)]
        commit_logged = "commit" in acts

        sh(f"{ZF} changeset approve {cs} --by reviewer", d)
        acts = [e.get("action") for e in events(d)]
        approve_logged = "approve" in acts

        # A key rotation.
        key = os.path.join(d, "k.key")
        sh(f"{ZF} key generate --path {key}", d)
        sh(f"{ZF} key rotate --path {key}", d)
        acts = [e.get("action") for e in events(d)]
        rotate_logged = "key-rotate" in acts

        # A push, over a real socket, so the gate and the recorder run where they do in production.
        # The token goes in before the clone: once a token exists the server requires one, and a
        # clone without it fails before there is anything to push.
        tok_out = sh(f"{ZF} remote token add write pusher", d)[1]
        auth = ""
        for line in tok_out.splitlines():
            line = line.strip()
            if re.fullmatch(r"[0-9a-f]{64}", line):
                auth = line
        port = free_port()
        server = subprocess.Popen(f"{ZF} serve --port {port}", shell=True, cwd=d,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        push_logged = False
        try:
            wait_port(port)
            clone = os.path.join(work, "c")
            auth_url = f"http://pusher:{auth}@127.0.0.1:{port}/git/repo"
            rc, clone_out = sh(f"git clone -q {auth_url} {clone}")
            if rc != 0:
                print(f"  clone failed: {clone_out[-300:]}")
            sh("git config user.email a@b.c && git config user.name t", clone)
            with open(os.path.join(clone, "b.txt"), "w") as fh:
                fh.write("two\n")
            sh("git add -A && git commit -qm second", clone)
            sh("git push -q origin main", clone)
            time.sleep(0.3)
            acts = [e.get("action") for e in events(d)]
            push_logged = "push" in acts
        finally:
            server.terminate()
            try:
                server.wait(timeout=10)
            except Exception:
                server.kill()

        all_events = events(d)
        unparsable = [e for e in all_events if e.get("action") == "<unparsable>"]
        every_line_parses = not unparsable
        has_who = all(e.get("who") for e in all_events if e.get("action") != "<unparsable>")

        checks = [
            ("a commit leaves a line", commit_logged),
            ("a sign-off leaves a line", approve_logged),
            ("a key rotation leaves a line", rotate_logged),
            ("a push leaves a line", push_logged),
            ("every line is one JSON document", every_line_parses),
            ("every line names who did it", has_who),
        ]
        for name, ok in checks:
            print(("  [PASS] " if ok else "  [FAIL] ") + name)
        print()
        print("  actions recorded: " + ", ".join(sorted(set(a for a in acts if a))))
        print()
        if all(ok for _, ok in checks):
            print(f"{len(checks)}/{len(checks)} local audit checks pass")
            return 0
        return 1
    finally:
        shutil.rmtree(d, ignore_errors=True)
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
