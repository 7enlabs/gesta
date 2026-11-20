#!/usr/bin/env python3
"""Does a protected branch actually refuse a push, and does a second credential let it through?

The property is about a real git client over a real socket, because that is where a review gate either
holds or becomes decoration. A unit test that calls the gate directly proves the gate answers; this
proves the server asks it.

Four things are checked, in the order a person would hit them:

1. A push to a protected branch with no approval is refused, and the branch does not move.
2. An approval from the pusher alone is still refused. This is the rule that keeps a review a review.
3. An approval from another name lets the push through.
4. An unprotected branch needs none of this.
"""
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


def sh(cmd, cwd=None):
    r = subprocess.run(cmd, shell=True, cwd=cwd, capture_output=True, text=True, timeout=180)
    return r.returncode, (r.stdout or "") + (r.stderr or "")


def main():
    d = tempfile.mkdtemp(prefix="gate-protect-")
    work = tempfile.mkdtemp(prefix="gate-protect-work-")
    try:
        sh(f"{ZF} init", d)
        with open(os.path.join(d, "a.txt"), "w") as fh:
            fh.write("one\n")
        m = re.search(r"(cs[0-9a-f]+)", sh(f'{ZF} changeset create "seed" --author t --json', d)[1])
        if m:
            sh(f"{ZF} changeset commit {m.group(1)}", d)

        rc, out = sh(f"{ZF} branch protect main", d)
        protect_said = "is protected" in out

        # A protected branch needs named credentials, so the server gets a write token and the clone
        # authenticates with it. Without one every pusher is "anonymous" and the gate refuses
        # everything, which is correct but proves nothing about approvals.
        # The token's name is the identity the server reports as `who`, so the reviewer can be told
        # apart from the writer by credential rather than by a claim in the commit.
        rc_tok, tok_out = sh(f"{ZF} remote token add write pusher", d)
        # The command prints the token once, because it stores only its digest.
        token = ""
        for line in tok_out.splitlines():
            m_tok = re.search(r"([0-9a-f]{32,})", line)
            if m_tok and "pusher" in line.lower():
                token = m_tok.group(1)
        if not token:
            m_tok = re.search(r"([0-9a-f]{32,})", tok_out)
            token = m_tok.group(1) if m_tok else ""
        print(f"  token: {'yes' if token else 'MISSING: ' + tok_out.strip()[:200]}")

        port = free_port()
        server = subprocess.Popen(f"{ZF} serve --port {port}", shell=True, cwd=d,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            wait_port(port)
            url = f"http://127.0.0.1:{port}/git/repo"
            auth_url = f"http://pusher:{token}@127.0.0.1:{port}/git/repo"
            clone = os.path.join(work, "repo")
            sh(f"git clone -q {auth_url} {clone}")
            sh("git config user.email a@b.c && git config user.name pusher", clone)
            # The clone keeps the credential in origin, but a later `git remote set-url` is what makes
            # the intent explicit; a push that fell back to an unauthenticated URL would look like a
            # refused approval rather than a missing credential.
            sh(f"git remote set-url origin {auth_url}", clone)

            head_before = sh("git rev-parse HEAD", clone)[1].strip()

            # 1. No approval: refused.
            with open(os.path.join(clone, "a.txt"), "w") as fh:
                fh.write("one\ntwo\n")
            sh("git commit -qam second", clone)
            rc, out = sh("git push origin main", clone)
            refused = rc != 0 and "protected" in out
            # The branch on the server has not moved.
            remote_after = sh(f"git ls-remote {auth_url} refs/heads/main")[1].split()[0]
            moved = remote_after != head_before
            print(f"  refused push: head_before={head_before[:8]} remote={remote_after[:8]}")

            # 2. The pusher approves its own change: still refused.
            newsha = sh("git rev-parse HEAD", clone)[1].strip()
            # The changeset the push produced. The server imported the commit, so asking the store
            # which changeset holds that git sha is the direct answer; asking for a list returns the
            # pending ones and would silently pick the wrong id.
            cs = None
            rc2, found = sh(f"{ZF} changeset find --commit {newsha}", d)
            m = re.search(r"(cs[0-9a-f]+)", found)
            if m:
                cs = m.group(1)
            if cs is None:
                # Fall back to the branch head, which is what the server just moved it to.
                rc3, head_out = sh(f"{ZF} branch info main", d)
                m = re.search(r"(cs[0-9a-f]+)", head_out)
                cs = m.group(1) if m else None
            self_approved = False
            still_refused = False
            if cs:
                sh(f"{ZF} changeset approve {cs} --by pusher", d)
                rc, out = sh("git push origin main", clone)
                self_approved = "approved by pusher" in out or rc == 0
                still_refused = rc != 0 and "pusher" in out

            # 3. Another credential approves: allowed.
            allowed = False
            approved_out = ""
            if cs:
                approved_out = sh(f"{ZF} changeset approve {cs} --by reviewer", d)[1]
                rc, out = sh("git push origin main", clone)
                allowed = rc == 0
                pushed = sh(f"git ls-remote {auth_url} refs/heads/main")[1].split()[0]
                allowed = allowed and pushed == newsha
                if not allowed:
                    print(f"  push after reviewer approval: rc={rc} {out[-300:]}")

            # 4. Unprotecting lets a push through with no approval at all. The server still requires a
            # token, which is a separate question from the branch gate.
            sh(f"{ZF} branch protect main --unset", d)
            with open(os.path.join(clone, "a.txt"), "w") as fh:
                fh.write("one\ntwo\nthree\n")
            sh("git commit -qam third", clone)
            rc, out = sh("git push origin main", clone)
            open_allowed = rc == 0
        finally:
            server.terminate()
            try:
                server.wait(timeout=10)
            except Exception:
                server.kill()

        checks = [
            ("protect says what it did", protect_said),
            ("a push with no approval is refused", refused),
            ("and the branch did not move", not moved),
            ("the pusher's own approval is refused", still_refused),
            ("an approval from another name lets it through", allowed),
            ("an unprotected branch needs no approval", open_allowed),
        ]
        for name, ok in checks:
            print(("  [PASS] " if ok else "  [FAIL] ") + name)
        print()
        if all(ok for _, ok in checks):
            print(f"{len(checks)}/{len(checks)} protected-branch checks pass")
            return 0
        print(f"cs={cs} self_approved={self_approved}")
        print(out[-600:])
        return 1
    finally:
        shutil.rmtree(d, ignore_errors=True)
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
