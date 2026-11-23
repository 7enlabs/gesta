#!/usr/bin/env python3
"""Does an audit line reach a collector across a socket, with the token and without the URL carrying it?

The unit tests use a socket they open themselves. This runs the real server against a real collector, so
the path from a refused request to a delivered line is exercised as one thing: the request, the auth
decision, the file write, the queue, and the POST.
"""
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time

ZF = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "target", "release", "gesta")


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def wait_port(port, timeout=10.0):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return True
        except OSError:
            time.sleep(0.1)
    return False


def main():
    d = tempfile.mkdtemp(prefix="audit-e2e-")
    out = os.path.join(d, "collected.log")
    try:
        subprocess.run(f"{ZF} init", shell=True, cwd=d, capture_output=True)
        with open(os.path.join(d, "a.txt"), "w") as fh:
            fh.write("one\n")
        made = subprocess.run(f'{ZF} changeset create "first" --author t --json',
                              shell=True, cwd=d, capture_output=True, text=True).stdout
        m = re.search(r"(cs[0-9a-f]+)", made)
        if m:
            subprocess.run(f"{ZF} changeset commit {m.group(1)}", shell=True, cwd=d, capture_output=True)

        # A read token so the server requires one, which is what makes a refusal an event.
        subprocess.run(f"{ZF} remote token add read tok", shell=True, cwd=d, capture_output=True)
        token = ""
        auth_file = os.path.join(d, ".gesta", "auth")
        if os.path.exists(auth_file):
            with open(auth_file) as fh:
                token = fh.readline().split(":")[0].strip()

        cport = free_port()
        collector = subprocess.Popen(
            [sys.executable, os.path.join(os.path.dirname(os.path.abspath(__file__)), "audit_collector.py"),
             str(cport), out],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        try:
            wait_port(cport)

            # The collector URL and its token live in separate files: the URL is logged by every proxy
            # on the way and must not carry a credential.
            with open(os.path.join(d, ".gesta", "audit-url"), "w") as fh:
                fh.write(f"http://127.0.0.1:{cport}/audit\n")
            with open(os.path.join(d, ".gesta", "audit-token"), "w") as fh:
                fh.write("secret-token\n")

            sport = free_port()
            server = subprocess.Popen([ZF, "serve", "--port", str(sport)], cwd=d,
                                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            try:
                wait_port(sport)

                # A request with no token: refused, and the refusal is what has to arrive.
                subprocess.run(f"curl -s -o /dev/null http://127.0.0.1:{sport}/v1/branches",
                               shell=True, capture_output=True)
                # A request with the token: served, and that is a second kind of event.
                subprocess.run(
                    f'curl -s -o /dev/null -H "Authorization: Bearer {token}" '
                    f"http://127.0.0.1:{sport}/v1/branches",
                    shell=True, capture_output=True)
                time.sleep(2)
            finally:
                server.terminate()
                try:
                    server.wait(timeout=10)
                except Exception:
                    server.kill()
        finally:
            collector.terminate()
            try:
                collector.wait(timeout=10)
            except Exception:
                collector.kill()

        got = ""
        if os.path.exists(out):
            with open(out, encoding="utf-8", errors="replace") as fh:
                got = fh.read()

        checks = [
            ("a refusal reached the collector", '"decision":"no-token"' in got),
            ("a served request reached it too", '"decision":"allowed"' in got),
            ("the token travelled in a header", "---auth:Bearer secret-token" in got),
            ("and not in the url", "secret-token@" not in got and "token=" not in got),
        ]
        for name, ok in checks:
            print(("  [PASS] " if ok else "  [FAIL] ") + name)
        print()
        if all(ok for _, ok in checks):
            print(f"{len(checks)}/{len(checks)} audit checks pass")
            return 0
        print(got[:600] or "the collector received nothing")
        return 1
    finally:
        shutil.rmtree(d, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
