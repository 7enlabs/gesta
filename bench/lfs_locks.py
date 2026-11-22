#!/usr/bin/env python3
"""Do the git-lfs lock calls behave the way a client relies on?

The three calls are list, claim and release, and the properties that matter are the ones a client acts
on rather than the ones a happy path exercises:

  a claim answers with a lock and an id           the id is what unlock and verify use
  the same owner claiming twice gets one lock    a client whose first request timed out asks again, and
                                                 an answer of "already locked" would be a conflict with
                                                 itself
  a second owner is refused with the holder named  git-lfs shows that name to the person, so a refusal
                                                 without it is a dead end
  a filter narrows the list                      `?path=` and `?id=` are how a client asks about one
                                                 lock, and answering with everything looks like success
  releasing frees the path                       a second owner can then claim it
  releasing an id nobody holds is a refusal, not a crash

The server runs on a free port from a temporary store. No git binary is needed.
"""

import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

_PROJECT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ZF = os.environ.get("GESTA_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))

results = []


def record(name, ok, detail=""):
    results.append((name, ok, detail))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}")
    if not ok and detail:
        for line in str(detail).strip().splitlines()[:5]:
            print(f"         {line}")


def free_port() -> int:
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def call(method, url, body=None, user=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    req.add_header("Content-Type", "application/vnd.git-lfs+json")
    req.add_header("Accept", "application/vnd.git-lfs+json")
    if user:
        import base64

        tok = base64.b64encode(f"{user}:x".encode()).decode()
        req.add_header("Authorization", f"Basic {tok}")
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            raw = r.read()
            return r.status, (json.loads(raw) if raw else {})
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            return e.code, json.loads(raw) if raw else {}
        except json.JSONDecodeError:
            return e.code, {"raw": raw.decode("utf-8", "replace")}


def main() -> int:
    if not os.path.exists(ZF):
        print(f"binary not found: {ZF}")
        return 1

    base = tempfile.mkdtemp(prefix="lfs-locks-")
    repo = os.path.join(base, "repo")
    os.makedirs(repo)
    server = None
    try:
        subprocess.run([ZF, "init"], cwd=repo, capture_output=True, timeout=60)
        with open(os.path.join(repo, "a.bin"), "wb") as fh:
            fh.write(b"x" * 16)
        c = subprocess.run(
            [ZF, "changeset", "create", "seed", "--json"],
            cwd=repo,
            capture_output=True,
            text=True,
            timeout=60,
        )
        import re

        m = re.search(r"\b(cs[0-9a-f]{6,})\b", c.stdout)
        if m:
            subprocess.run(
                [ZF, "changeset", "commit", m.group(1)],
                cwd=repo,
                capture_output=True,
                timeout=60,
            )

        port = free_port()
        server = subprocess.Popen(
            [ZF, "serve", "--bind", "127.0.0.1", "--port", str(port)],
            cwd=repo,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        for _ in range(80):
            try:
                socket.create_connection(("127.0.0.1", port), timeout=1).close()
                break
            except OSError:
                time.sleep(0.1)
        else:
            print("the server never came up")
            return 1

        api = f"http://127.0.0.1:{port}/git/repo.git/info/lfs/locks"

        st, body = call("POST", api, {"path": "a.bin"}, user="alice")
        lock = body.get("lock", {})
        lid = lock.get("id", "")
        record(
            "a claim answers with a lock that has an id",
            st == 200 and len(lid) >= 16 and lock.get("path") == "a.bin",
            f"http={st} body={body}",
        )

        # The same owner, again. A retry, not a conflict.
        st2, body2 = call("POST", api, {"path": "a.bin"}, user="alice")
        record(
            "the same owner claiming twice gets the same lock back",
            st2 == 200 and body2.get("lock", {}).get("id") == lid,
            f"http={st2} body={body2}",
        )

        # A different owner is refused, and the refusal names who holds it.
        st3, body3 = call("POST", api, {"path": "a.bin"}, user="bob")
        holder = body3.get("lock", {}).get("owner", {}).get("name", "")
        record(
            "a second owner is refused and told who holds it",
            st3 == 409 and holder == "alice",
            f"http={st3} body={body3}",
        )

        # The filter has to narrow. Answering with every lock looks like success and is not.
        st4, body4 = call("GET", api + "?path=a.bin")
        record(
            "the path filter narrows the list to that lock",
            st4 == 200 and len(body4.get("locks", [])) == 1,
            f"http={st4} body={body4}",
        )
        st5, body5 = call("GET", api + "?path=nothing.bin")
        record(
            "a filter for a path nobody holds returns nothing",
            st5 == 200 and body5.get("locks", []) == [],
            f"http={st5} body={body5}",
        )

        st6, body6 = call("POST", api + "/verify", {}, user="alice")
        record(
            "verify splits alice's locks from everyone else's",
            st6 == 200
            and len(body6.get("ours", [])) == 1
            and len(body6.get("theirs", [])) == 0,
            f"http={st6} body={body6}",
        )

        # Releasing has to free the path, which is the only way a second owner gets in.
        st7, body7 = call("POST", api + "/unlock", {"id": lid}, user="alice")
        record("the holder can release the lock", st7 == 200, f"http={st7} body={body7}")

        st8, body8 = call("POST", api, {"path": "a.bin"}, user="bob")
        record(
            "after the release the other owner can claim it",
            st8 == 200 and body8.get("lock", {}).get("owner", {}).get("name") == "bob",
            f"http={st8} body={body8}",
        )

        # A release by someone who is not the holder is refused unless forced.
        bob_id = body8.get("lock", {}).get("id", "")
        st9, body9 = call("POST", api + "/unlock", {"id": bob_id}, user="alice")
        record(
            "releasing someone else's lock is refused unless forced",
            st9 == 403,
            f"http={st9} body={body9}",
        )
        st10, _ = call("POST", api + "/unlock", {"id": bob_id, "force": True}, user="alice")
        record("force releases someone else's lock", st10 == 200, f"http={st10}")

        st11, _ = call("POST", api + "/unlock", {"id": "0" * 32}, user="alice")
        record(
            "releasing an id nobody holds is a refusal, not a crash",
            st11 == 403,
            f"http={st11}",
        )

        st12, body12 = call("GET", api)
        record(
            "the list is empty once every lock is released",
            st12 == 200 and body12.get("locks", []) == [],
            f"http={st12} body={body12}",
        )

        passed = sum(1 for _, ok, _ in results if ok)
        print(f"\n{passed}/{len(results)} lock checks pass")
        if passed != len(results):
            print("\nfailing:")
            for n, ok, _ in results:
                if not ok:
                    print(f"  - {n}")
        return 0 if passed == len(results) else 1
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
