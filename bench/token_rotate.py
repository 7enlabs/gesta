#!/usr/bin/env python3
"""Does the old secret stop working after a rotation, and the new one start?

A rotation that changes the file but leaves the old credential usable is worse than no rotation,
because the person rotating believes the leak is closed. The check is a request with the old token
against a running server, not a comparison of two strings in the file.
"""
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

_HERE = os.path.dirname(os.path.abspath(__file__))
_PROJECT = os.path.dirname(_HERE)
ZF = os.environ.get("ZF_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))


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


def status(url, token):
    """HTTP status for a request carrying `token`."""
    req = urllib.request.Request(url, headers={"Authorization": "Bearer " + token})
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return r.status
    except urllib.error.HTTPError as e:
        return e.code
    except Exception:
        return 0


def sh(cmd, cwd=None):
    r = subprocess.run(cmd, shell=True, cwd=cwd, capture_output=True, text=True, timeout=120)
    return r.returncode, (r.stdout or "") + (r.stderr or "")


def main():
    d = tempfile.mkdtemp(prefix="token-rotate-")
    try:
        sh(ZF + " init", d)
        with open(os.path.join(d, "a.txt"), "w") as fh:
            fh.write("one\n")
        m = re.search(r"(cs[0-9a-f]+)", sh(ZF + ' changeset create "s" --author t --json', d)[1])
        if m:
            sh(ZF + " changeset commit " + m.group(1), d)

        # A scoped token, so the rotation has a grant worth preserving.
        rc, out = sh(ZF + ' remote token add write buildbot --scope "src/**" --branches "dev*"', d)
        old = ""
        for line in out.splitlines():
            if re.fullmatch(r"[0-9a-f]{64}", line.strip()):
                old = line.strip()
        auth_before = open(os.path.join(d, ".gesta", "auth")).read().strip()

        port = free_port()
        server = subprocess.Popen([ZF, "serve", "--port", str(port)], cwd=d,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            wait_port(port)
            url = "http://127.0.0.1:" + str(port) + "/v1/branches"
            before_ok = status(url, old) == 200

            rc, rout = sh(ZF + " remote token rotate buildbot", d)
            new = ""
            for line in rout.splitlines():
                if re.fullmatch(r"[0-9a-f]{64}", line.strip()):
                    new = line.strip()
            auth_after = open(os.path.join(d, ".gesta", "auth")).read().strip()

            # The server re-reads the file per request, so no restart is needed. That is the property
            # that makes a rotation take effect when an operator runs it.
            old_after = status(url, old)
            new_after = status(url, new)

            # The grant is what a rotation must not change. Field by field, ignoring the secret.
            def grant(line):
                f = line.split(":")
                return f[1:] if f[0] != "sha256" else f[2:]

            grant_kept = grant(auth_before) == grant(auth_after)
            secret_changed = bool(old) and bool(new) and old != new
        finally:
            server.terminate()
            try:
                server.wait(timeout=10)
            except Exception:
                server.kill()

        checks = [
            ("the old token worked before the rotation", before_ok),
            ("the rotation printed a new secret", secret_changed),
            ("the old secret is refused afterwards", old_after in (401, 403)),
            ("the new secret is accepted", new_after == 200),
            ("the grant kept its role, scope and name", grant_kept),
        ]
        for name, ok in checks:
            print(("  [PASS] " if ok else "  [FAIL] ") + name)
        print()
        print("  old secret after rotation: HTTP " + str(old_after) + "; new secret: HTTP " + str(new_after))
        print()
        if all(ok for _, ok in checks):
            print(str(len(checks)) + "/" + str(len(checks)) + " rotation checks pass")
            return 0
        print("before=" + auth_before[:60])
        print("after =" + auth_after[:60])
        return 1
    finally:
        shutil.rmtree(d, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
