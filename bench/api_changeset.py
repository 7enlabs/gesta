#!/usr/bin/env python3
"""Check the remote changeset endpoint and path scoping against a live server.

The point of these checks is that an agent with no checkout can create a
changeset, and that a token scoped to one directory cannot write outside it.

Run: python3 bench/api_changeset.py
"""

import json
import os
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request

# The binary and the project are found relative to this file, not from a fixed mount path. A suite
# that only runs in one container cannot be part of the check a change is measured against.
_HERE = os.path.dirname(os.path.abspath(__file__))
_PROJECT = os.path.dirname(_HERE)
ZF = os.environ.get("ZF_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))
ROOT = "/tmp/zf_api"
PORT = 8791
BASE = f"http://127.0.0.1:{PORT}"
results = []


def record(name, ok, detail=""):
    results.append((name, ok, detail))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}")
    if not ok and detail:
        for line in str(detail).split("\n")[:4]:
            print(f"         {line}")


def run(*args, cwd=None):
    r = subprocess.run([ZF, *args], capture_output=True, text=True, cwd=cwd)
    return r.returncode, (r.stdout + r.stderr).strip()


def http(method, path, body=None, token=None, timeout=20):
    url = BASE + path
    data = None
    headers = {}
    if body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    if token:
        headers["Authorization"] = f"Bearer {token}"
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace")
    except Exception as e:  # connection refused, timeout
        return 0, str(e)


def wait_health(tries=80):
    for _ in range(tries):
        st, _ = http("GET", "/v1/health", timeout=2)
        if st == 200:
            return True
        time.sleep(0.15)
    return False


def main():
    if not os.path.exists(ZF):
        sys.exit(f"binary not found: {ZF}")
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(ROOT)

    repo = os.path.join(ROOT, "server")
    os.makedirs(repo)
    run("init", cwd=repo)
    with open(os.path.join(repo, "README.md"), "w") as fh:
        fh.write("# server repo\n")
    run("changeset", "create", "seed", "--author", "seed", cwd=repo)
    out = run("changeset", "list", cwd=repo)[1]
    for word in out.split():
        if word.startswith("cs"):
            run("changeset", "commit", word, cwd=repo)
            break

    # Two tokens: full write, and one limited to docs/.
    admin = "admintoken-0123456789abcdef"
    docsonly = "doctoken-0123456789abcdef"
    auth_file = os.path.join(repo, ".gesta", "auth")
    with open(auth_file, "w") as fh:
        fh.write(f"{admin}:write:admin:src/**\n")
        fh.write(f"{admin}:write:admin\n")
        fh.write(f"{docsonly}:write:docbot:docs/**\n")

    proc = subprocess.Popen(
        [ZF, "serve", "--bind", "127.0.0.1", "--port", str(PORT)],
        cwd=repo,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        if not wait_health():
            st, body = http("GET", "/v1/health")
            print(f"server did not start (status {st}): {body}")
            return 1

        st, body = http("POST", "/v1/changeset", token=admin, body={
            "branch": "main",
            "message": "from an agent with no checkout",
            "author": "sandbox-agent",
            "files": [
                {"path": "src/app.py", "content": "print('hello')\n"},
                {"path": "docs/guide.md", "content": "# guide\n"},
            ],
        })
        ok = st == 200
        payload = {}
        if ok:
            try:
                payload = json.loads(body)
            except json.JSONDecodeError:
                ok = False
        record("a remote agent creates a changeset with no working copy",
               ok and payload.get("files") == 2 and payload.get("id", "").startswith("cs"),
               body[:250])

        cid = payload.get("id")

        # The server's own disk was not touched: no checkout, no src/app.py.
        record("the server working tree is untouched by the API write",
               not os.path.exists(os.path.join(repo, "src", "app.py")),
               "a file appeared on the server's disk")

        st, body = http("GET", f"/v1/file?path=src/app.py&rev={cid}", token=admin)
        record("the new content is readable through the API before checkout",
               st == 200 and "print('hello')" in body, f"{st} {body[:150]}")

        # Scope: a token limited to docs/ may not write src/.
        st, body = http("POST", "/v1/changeset", token=docsonly, body={
            "message": "try to escape the scope",
            "files": [{"path": "src/evil.py", "content": "bad\n"}],
        })
        record("a scoped token is refused outside its scope",
               st == 403 and "may not write" in body, f"{st} {body[:180]}")

        st, body = http("POST", "/v1/changeset", token=docsonly, body={
            "message": "inside the scope",
            "files": [{"path": "docs/more.md", "content": "# more\n"}],
        })
        record("the same scoped token is allowed inside its scope",
               st == 200 and "cs" in body, f"{st} {body[:180]}")

        # A path that escapes the repository must never be accepted.
        st, body = http("POST", "/v1/changeset", token=admin, body={
            "message": "path escape",
            "files": [{"path": "../../etc/passwd", "content": "x\n"}],
        })
        record("a path escaping the repository is refused", st == 400, f"{st} {body[:180]}")

        # An empty request is a mistake, not a no-op changeset.
        st, body = http("POST", "/v1/changeset", token=admin, body={"message": "nothing"})
        record("a request with no files is refused", st == 400, f"{st} {body[:180]}")

        # Committing through the same call.
        st, body = http("POST", "/v1/changeset", token=admin, body={
            "message": "commit straight away",
            "files": [{"path": "src/committed.py", "content": "x = 1\n"}],
            "commit": True,
        })
        committed = False
        if st == 200:
            try:
                committed = json.loads(body).get("committed") is True
            except json.JSONDecodeError:
                pass
        record("the same call can commit the changeset", committed, f"{st} {body[:200]}")

        # Without a token, a configured server must reject the write.
        st, body = http("POST", "/v1/changeset", body={
            "message": "anonymous",
            "files": [{"path": "src/anon.py", "content": "y\n"}],
        })
        record("an unauthenticated write is rejected when tokens exist", st == 401,
               f"{st} {body[:150]}")

        # Deletion through the API.
        st, body = http("POST", "/v1/changeset", token=admin, body={
            "message": "drop the guide",
            "delete": ["docs/guide.md"],
            "commit": True,
        })
        ok = st == 200
        record("a file can be deleted through the API", ok, f"{st} {body[:180]}")

        st, body = http("GET", "/v1/file?path=docs/guide.md", token=admin)
        record("the deleted file is gone from the branch", st == 404, f"{st} {body[:150]}")

        # The access log must record who did what.
        log_path = os.path.join(repo, ".gesta", "access.log")
        logged = ""
        if os.path.exists(log_path):
            with open(log_path) as fh:
                logged = fh.read()
        # The log must name the authenticated token, and keep the claimed author
        # beside it. A client-supplied name is not an identity.
        record("the access log names the authenticated token and the action",
               "\tadmin\tchangeset\t" in logged and "changeset" in logged, logged[:250])
        record("the access log keeps the client's claimed author as a claim",
               "claimed_author=sandbox-agent" in logged, logged[:250])

    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()

    passed = sum(1 for _, ok, _ in results if ok)
    total = len(results)
    print(f"\n{passed}/{total} API checks pass")
    if passed < total:
        print("\nfailing:")
        for name, ok, _ in results:
            if not ok:
                print(f"  - {name}")
    return 0 if passed == total else 1


if __name__ == "__main__":
    sys.exit(main())
