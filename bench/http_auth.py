#!/usr/bin/env python3
"""Check access control on the HTTP remote, over a real socket.

Covers what a token must actually do: block anonymous reads, allow a read
token to clone but not push, allow a write token to push, and refuse to serve
on a public address with no tokens configured.

Run: python3 bench/http_auth.py
"""

import os
import shutil
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request

ZF = os.environ.get("ZF_BIN", "/work/gesta/target/release/gesta")
ROOT = "/tmp/zf_auth"
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


def write(d, path, text):
    full = os.path.join(d, path)
    parent = os.path.dirname(full)
    if parent:
        os.makedirs(parent, exist_ok=True)
    if isinstance(text, bytes):
        with open(full, "wb") as fh:
            fh.write(text)
    else:
        with open(full, "w") as fh:
            fh.write(text)


def read(d, path):
    with open(os.path.join(d, path)) as fh:
        return fh.read()


def commit(d, msg, author="agent"):
    rc, out = run("changeset", "create", msg, "--author", author, cwd=d)
    for w in out.split():
        if w.startswith("cs"):
            run("changeset", "commit", w, cwd=d)
            return w
    return None


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def http_get(url, token=None, timeout=5):
    req = urllib.request.Request(url)
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()
    except Exception as e:
        return 0, str(e).encode()


def wait_health(port, timeout=20.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        status, body = http_get(f"http://127.0.0.1:{port}/v1/health", timeout=1)
        if status == 200 and body.strip() == b"ok":
            return True
        time.sleep(0.15)
    return False


def main():
    if not os.path.exists(ZF):
        sys.exit(f"binary not found: {ZF}")
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(ROOT)

    # --- token generation ---
    origin = os.path.join(ROOT, "origin")
    os.makedirs(origin)
    run("init", cwd=origin)
    write(origin, "a.txt", "hello\n")
    commit(origin, "initial")

    rc, out = run("remote", "token", "add", "write", "alice", cwd=origin)
    write_token = None
    for line in out.split("\n"):
        s = line.strip()
        if len(s) == 64 and all(c in "0123456789abcdef" for c in s):
            write_token = s
            break
    record("token add prints a 64 hex character token", rc == 0 and write_token is not None, out[:300])

    rc, out = run("remote", "token", "add", "read", "bob", cwd=origin)
    read_token = None
    for line in out.split("\n"):
        s = line.strip()
        if len(s) == 64 and all(c in "0123456789abcdef" for c in s):
            read_token = s
            break
    record("a second token can be read only", rc == 0 and read_token is not None and read_token != write_token, out[:200])

    auth_file = read(origin, ".gesta/auth")
    record("tokens are stored in clear in .gesta/auth",
           write_token in auth_file and "write" in auth_file, auth_file.replace(write_token or "", "<tok>"))

    rc, out = run("remote", "token", "list", cwd=origin)
    record("token list hides the secret",
           rc == 0 and (write_token or "xxx") not in out and "alice" in out, out)

    # --- server with tokens ---
    port = free_port()
    server = subprocess.Popen(
        [ZF, "serve", "--bind", "127.0.0.1", "--port", str(port)],
        cwd=origin,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        up = wait_health(port)
        record("server starts with tokens configured", up, f"port={port}")
        if not up:
            err = server.communicate(timeout=5)
            record("server output", False, str(err))
            return finish()

        base = f"http://127.0.0.1:{port}"

        status, _ = http_get(f"{base}/v1/health")
        record("health stays open without a token", status == 200, f"status={status}")

        status, body = http_get(f"{base}/v1/branches")
        record("anonymous branch listing is refused", status == 401, f"status={status} {body[:80]!r}")

        status, _ = http_get(f"{base}/v1/bundle?branch=main")
        record("anonymous bundle fetch is refused", status == 401, f"status={status}")

        status, body = http_get(f"{base}/v1/branches", token=write_token)
        record("write token can list branches", status == 200 and b"main\t" in body, f"{status} {body[:60]!r}")

        status, body = http_get(f"{base}/v1/branches", token=read_token)
        record("read token can list branches", status == 200, f"status={status}")

        status, body = http_get(f"{base}/v1/branches", token="bogus")
        record("a wrong token is refused", status == 401, f"status={status}")

        # --- clone with a read token ---
        clone_dir = os.path.join(ROOT, "clone_read")
        rc, out = run("remote", "clone", base, clone_dir, "--token", read_token, cwd=ROOT)
        ok = rc == 0 and os.path.exists(os.path.join(clone_dir, "a.txt"))
        record("read token can clone", ok, out[:200])

        # --- push with a read token must fail ---
        write(clone_dir, "b.txt", "from reader\n")
        commit(clone_dir, "reader tries to push")
        rc, out = run("remote", "push", "origin", cwd=clone_dir)
        blocked = rc != 0 and ("403" in out or "rejected" in out.lower())
        record("read token cannot push", blocked, out[:250])

        # --- clone without a token must fail ---
        rc, out = run("remote", "clone", base, os.path.join(ROOT, "clone_anon"), cwd=ROOT)
        record("clone without a token fails", rc != 0, out[:200])

        # --- push with a write token must succeed ---
        wclone = os.path.join(ROOT, "clone_write")
        rc, out = run("remote", "clone", base, wclone, "--token", write_token, cwd=ROOT)
        record("write token can clone", rc == 0, out[:150])

        write(wclone, "c.txt", "from writer\n")
        commit(wclone, "writer pushes")
        rc, out = run("remote", "push", "origin", cwd=wclone)
        landed = os.path.exists(os.path.join(origin, "c.txt"))
        record("write token can push and the file lands", rc == 0 and landed, f"{out[:200]} | landed={landed}")

    finally:
        server.terminate()
        try:
            server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server.kill()

    # --- a public bind with no tokens must be refused ---
    bare = os.path.join(ROOT, "bare")
    os.makedirs(bare)
    run("init", cwd=bare)
    write(bare, "x.txt", "x\n")
    commit(bare, "x")
    port2 = free_port()
    proc = subprocess.Popen(
        [ZF, "serve", "--bind", "0.0.0.0", "--port", str(port2)],
        cwd=bare,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        out, err = proc.communicate(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        out, err = proc.communicate()
    combined = (out or "") + (err or "")
    refused = "refusing to serve" in combined
    record("public bind without tokens is refused", refused, combined[:250])

    # --- loopback without tokens is allowed ---
    port3 = free_port()
    proc = subprocess.Popen(
        [ZF, "serve", "--bind", "127.0.0.1", "--port", str(port3)],
        cwd=bare,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        opened = wait_health(port3)
        status, _ = http_get(f"http://127.0.0.1:{port3}/v1/branches")
        record("loopback without tokens works and stays open",
               opened and status == 200, f"health={opened} branches={status}")
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()

    return finish()


def finish():
    passed = sum(1 for _, ok, _ in results if ok)
    total = len(results)
    print(f"\n{passed}/{total} auth checks pass")
    if passed < total:
        print("\nfailing:")
        for name, ok, _ in results:
            if not ok:
                print(f"  - {name}")
    return 0 if passed == total else 1


if __name__ == "__main__":
    sys.exit(main())
