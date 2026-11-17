#!/usr/bin/env python3
"""Check TLS end to end: a real handshake, a real push, and real refusals.

The checks that matter are the negative ones. A TLS setup that accepts anything
is worse than plaintext, because it looks safe. So this suite proves the client
rejects an unpinned self-signed certificate and rejects the wrong pin.

Run: python3 bench/tls.py
"""

import base64
import hashlib
import os
import shutil
import ssl
import subprocess
import sys
import time
import urllib.request

ZF = os.environ.get("ZF_BIN", "/work/zenforge/target/release/zenforge")
ROOT = "/tmp/zf_tls_bench"
PORT = 8793
TOKEN = "tls-bench-token-0123456789abcdef"
results = []


def record(name, ok, detail=""):
    results.append((name, ok, detail))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}")
    if not ok and detail:
        for line in str(detail).split("\n")[:5]:
            print(f"         {line}")


def run(*args, cwd=None):
    r = subprocess.run([ZF, *args], capture_output=True, text=True, cwd=cwd)
    return r.returncode, (r.stdout + r.stderr).strip()


def write(d, path, text):
    full = os.path.join(d, path)
    parent = os.path.dirname(full)
    if parent:
        os.makedirs(parent, exist_ok=True)
    with open(full, "w") as fh:
        fh.write(text)


def commit(d, msg, author="agent"):
    rc, out = run("changeset", "create", msg, "--author", author, cwd=d)
    for w in out.split():
        if w.startswith("cs"):
            run("changeset", "commit", w, cwd=d)
            return w
    return None


def fingerprint(cert_path):
    """SHA-256 of the DER body, the same value the server prints."""
    with open(cert_path, "rb") as fh:
        pem = fh.read().decode()
    body = "".join(l for l in pem.splitlines() if l and not l.startswith("-----"))
    return hashlib.sha256(base64.b64decode(body)).hexdigest()


def wait_https(tries=120):
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    for _ in range(tries):
        try:
            with urllib.request.urlopen(
                f"https://127.0.0.1:{PORT}/v1/health", context=ctx, timeout=2
            ) as r:
                if r.status == 200:
                    return True
        except Exception:
            time.sleep(0.15)
    return False


def main():
    if not os.path.exists(ZF):
        sys.exit(f"binary not found: {ZF}")
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(ROOT)

    server = os.path.join(ROOT, "server")
    os.makedirs(server)
    run("init", cwd=server)
    write(server, "app.py", "print('hello')\n")
    commit(server, "seed")
    with open(os.path.join(server, ".zenforge", "auth"), "w") as fh:
        fh.write(f"{TOKEN}:write:tester\n")

    proc = subprocess.Popen(
        [ZF, "serve", "--bind", "127.0.0.1", "--port", str(PORT), "--tls", "self-signed"],
        cwd=server,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    try:
        if not wait_https():
            proc.terminate()
            try:
                out, err = proc.communicate(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                out, err = "", ""
            print("server did not start over TLS")
            print((out or "")[:400])
            print((err or "")[:400])
            return 1

        cert_path = os.path.join(server, ".zenforge", "cert.pem")
        key_path = os.path.join(server, ".zenforge", "key.pem")
        record("a self-signed certificate and key were generated",
               os.path.exists(cert_path) and os.path.exists(key_path))

        if os.path.exists(key_path):
            mode = os.stat(key_path).st_mode & 0o777
            record("the private key is not world readable", mode == 0o600, f"mode is {oct(mode)}")

        fp = fingerprint(cert_path) if os.path.exists(cert_path) else ""
        record("the certificate has a SHA-256 fingerprint", len(fp) == 64, fp)

        ctx = ssl.create_default_context()
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
        req = urllib.request.Request(
            f"https://127.0.0.1:{PORT}/v1/branches",
            headers={"Authorization": f"Bearer {TOKEN}"},
        )
        with urllib.request.urlopen(req, context=ctx, timeout=10) as r:
            body = r.read().decode()
        record("the server answers over TLS with a real certificate", "main" in body, body[:150])

        strict = ssl.create_default_context()
        failed = False
        try:
            urllib.request.urlopen(f"https://127.0.0.1:{PORT}/v1/health", context=strict, timeout=10)
        except Exception as e:
            failed = "certificate" in str(e).lower()
        record("a client with public roots only refuses the self-signed certificate",
               failed, "verification unexpectedly succeeded")

        client = os.path.join(ROOT, "client")
        rc, out = run("remote", "clone", f"https://127.0.0.1:{PORT}", client,
                      "--token", TOKEN, "--pin", fp, cwd=ROOT)
        record("clone over https with the correct pin succeeds",
               rc == 0 and os.path.exists(os.path.join(client, "app.py")), out[:300])

        bad_target = os.path.join(ROOT, "badpin")
        rc, out = run("remote", "clone", f"https://127.0.0.1:{PORT}", bad_target,
                      "--token", TOKEN, "--pin", "00" * 32, cwd=ROOT)
        record("clone with the wrong pin is refused",
               rc != 0 and "fingerprint" in out.lower(), out[:300])

        nopin = os.path.join(ROOT, "nopin")
        rc, out = run("remote", "clone", f"https://127.0.0.1:{PORT}", nopin,
                      "--token", TOKEN, cwd=ROOT)
        record("clone without a pin is refused for a self-signed certificate",
               rc != 0 and ("certificate" in out.lower() or "handshake" in out.lower()),
               out[:300])

        write(client, "app.py", "print('from the tls client')\n")
        commit(client, "edit over tls")
        run("remote", "add", "origin", f"https://127.0.0.1:{PORT}",
            "--token", TOKEN, "--pin", fp, cwd=client)
        rc, out = run("remote", "push", "origin", cwd=client)
        record("push over https with the pin succeeds", rc == 0, out[:300])

        second = os.path.join(ROOT, "second")
        rc, out = run("remote", "clone", f"https://127.0.0.1:{PORT}", second,
                      "--token", TOKEN, "--pin", fp, cwd=ROOT)
        with open(os.path.join(second, "app.py")) as fh:
            content = fh.read()
        record("a second clone sees the pushed content",
               "from the tls client" in content, content[:150])

        rc, out = run("remote", "list", cwd=client)
        record("the stored remote names its pin", "pin" in out, out[:200])

        rc, out = run("remote", "add", "bad", "https://127.0.0.1:9", "--pin", "xyz", cwd=client)
        record("a malformed pin is refused at add time", rc != 0, out[:200])

    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()

    passed = sum(1 for _, ok, _ in results if ok)
    total = len(results)
    print(f"\n{passed}/{total} TLS checks pass")
    if passed < total:
        print("\nfailing:")
        for name, ok, _ in results:
            if not ok:
                print(f"  - {name}")
    return 0 if passed == total else 1


if __name__ == "__main__":
    sys.exit(main())
