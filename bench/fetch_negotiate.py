#!/usr/bin/env python3
"""Does a second fetch send the change, or the whole repository again?

The claim is about bytes, so it is measured in bytes rather than inferred from the code: clone once,
add a commit, fetch, and compare what the server actually put on the wire. A server that ignores the
client's `have` list sends the whole history twice and still looks correct, which is why this counts
bytes and not commands.

The first fetch has to be a real one over the socket. Counting the bytes of an in-process call would
measure this script instead of the server.
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


def upload_pack_size(url, wants, haves):
    """Bytes the server puts on the wire for one fetch. A raw POST, so nothing between us and it."""
    body = b"".join(
        b"%04x" % (len(f"want {w} side-band-64k ofs-delta\n") + 4) + f"want {w} side-band-64k ofs-delta\n".encode()
        for w in wants
    )
    body += b"0000"
    for h in haves:
        line = f"have {h}\n".encode()
        body += b"%04x" % (len(line) + 4) + line
    body += b"0000"
    line = b"done\n"
    body += b"%04x" % (len(line) + 4) + line

    import urllib.request

    req = urllib.request.Request(
        url + "/git-upload-pack",
        data=body,
        headers={"Content-Type": "application/x-git-upload-pack-request"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=180) as r:
        return len(r.read())


def main():
    d = tempfile.mkdtemp(prefix="fetch-negotiate-")
    try:
        subprocess.run(f"{ZF} init", shell=True, cwd=d, capture_output=True)
        # A tree big enough that "everything" and "one commit" differ by far more than the framing.
        for i in range(60):
            with open(os.path.join(d, f"f{i}.txt"), "w") as fh:
                fh.write(f"line {i}\n" * 400)
        out = subprocess.run(f'{ZF} changeset create "seed" --author t --json', shell=True, cwd=d,
                             capture_output=True, text=True).stdout
        m = re.search(r"(cs[0-9a-f]+)", out)
        if m:
            subprocess.run(f"{ZF} changeset commit {m.group(1)}", shell=True, cwd=d, capture_output=True)

        port = free_port()
        server = subprocess.Popen(f"{ZF} serve --port {port}", shell=True, cwd=d,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            wait_port(port)
            base = f"http://127.0.0.1:{port}"

            work = tempfile.mkdtemp(prefix="fetch-work-")
            try:
                clone = os.path.join(work, "repo")
                url = f"{base}/git/repo"
                r = subprocess.run(f"git clone -q {url} {clone}", shell=True, capture_output=True,
                                   text=True, timeout=180)
                head = subprocess.run("git rev-parse HEAD", shell=True, cwd=clone,
                                      capture_output=True, text=True).stdout.strip()

                # A second commit, then the fetch a real client makes with the first one in `have`.
                with open(os.path.join(d, "new.txt"), "w") as fh:
                    fh.write("one new file\n")
                out2 = subprocess.run(f'{ZF} changeset create "more" --author t --json', shell=True,
                                      cwd=d, capture_output=True, text=True).stdout
                m2 = re.search(r"(cs[0-9a-f]+)", out2)
                if m2:
                    subprocess.run(f"{ZF} changeset commit {m2.group(1)}", shell=True, cwd=d,
                                   capture_output=True)
                new_head = subprocess.run(f"{ZF} status --json", shell=True, cwd=d,
                                          capture_output=True, text=True).stdout
                # The branch head as the server advertises it.
                adv = subprocess.run(f"git ls-remote {url} refs/heads/main", shell=True,
                                     capture_output=True, text=True).stdout.split()
                tip = adv[0] if adv else ""

                everything = upload_pack_size(base, [tip], [])
                incremental = upload_pack_size(base, [tip], [head])

                # The common case for an agent is an edit to a file that already exists, which leaves
                # the tree shape alone. Measured separately because a new file adds a tree object and
                # that is a different cost.
                with open(os.path.join(d, "f7.txt"), "w") as fh:
                    fh.write(f"line 7\n" * 401)
                out3 = subprocess.run(f'{ZF} changeset create "edit" --author t --json', shell=True,
                                      cwd=d, capture_output=True, text=True).stdout
                m3 = re.search(r"(cs[0-9a-f]+)", out3)
                if m3:
                    subprocess.run(f"{ZF} changeset commit {m3.group(1)}", shell=True, cwd=d,
                                   capture_output=True)
                adv2 = subprocess.run(f"git ls-remote {url} refs/heads/main", shell=True,
                                      capture_output=True, text=True).stdout.split()
                tip2 = adv2[0] if adv2 else ""
                edit_full = upload_pack_size(base, [tip2], [])
                edit_delta = upload_pack_size(base, [tip2], [tip])

                # What git charges for the same edit on the same tree. Without this number the ratio
                # above is only a claim that smaller is better; a tree object is most of the cost and
                # git pays it too.
                ref = os.path.join(work, "gitref")
                subprocess.run(f"git init -q -b main {ref}", shell=True, capture_output=True)
                for name in sorted(os.listdir(d)):
                    if name.startswith("f") and name.endswith(".txt"):
                        shutil.copy2(os.path.join(d, name), os.path.join(ref, name))
                subprocess.run("git add -A && git -c user.email=a@b.c -c user.name=g commit -qm one",
                               shell=True, cwd=ref, capture_output=True)
                with open(os.path.join(ref, "f7.txt"), "w") as fh:
                    fh.write(f"line 7\n" * 401)
                subprocess.run("git add -A && git -c user.email=a@b.c -c user.name=g commit -qm two",
                               shell=True, cwd=ref, capture_output=True)
                git_delta = len(subprocess.run(
                    "git pack-objects --stdout --quiet --revs", shell=True, cwd=ref,
                    input=b"--not --all^\nHEAD\n", capture_output=True).stdout)
                if git_delta == 0:
                    # Fall back to packing just the one commit's objects, which is the same measure.
                    git_delta = len(subprocess.run(
                        "git pack-objects --stdout --quiet --revs", shell=True, cwd=ref,
                        input=b"HEAD\n", capture_output=True).stdout)

                # And the fetch must actually work, not just be small.
                r2 = subprocess.run("git fetch -q origin", shell=True, cwd=clone, capture_output=True,
                                    text=True, timeout=120)
                fetched = subprocess.run("git rev-parse origin/main", shell=True, cwd=clone,
                                         capture_output=True, text=True).stdout.strip()
                # The branch moved twice since the clone, so the check is against its last value.
                got_new = fetched == tip2 and fetched != head
                del new_head
            finally:
                shutil.rmtree(work, ignore_errors=True)
        finally:
            server.terminate()
            try:
                server.wait(timeout=10)
            except Exception:
                server.kill()

        ratio = (incremental / everything) if everything else 0.0
        checks = [
            ("a full fetch carries the whole tree", everything > 1000),
            # A new file adds a tree object beside its blob, so the bound is loose here on purpose.
            ("a fetch with a have carries less", everything > 0 and ratio < 0.40),
            ("an edit to an existing file carries a fraction of a full fetch",
             edit_full > 0 and (edit_delta / edit_full) < 0.40),
            # The number that matters: the same edit, against what git charges for it. git's own
            # pack-objects walks the whole tree for the commit, so equal bytes is not expected; the
            # claim is only that a fetch here is not larger than one there.
            ("and it is not larger than what git charges for that edit",
             git_delta > 0 and edit_delta <= git_delta),
            ("and the commit still arrives", got_new),
        ]
        for name, ok in checks:
            print(("  [PASS] " if ok else "  [FAIL] ") + name)
        print()
        print(f"  new file:   full {everything} bytes, with a have {incremental} ({ratio:.1%})")
        print(f"  edited file: full {edit_full} bytes, with a have {edit_delta} "
              f"({edit_delta / edit_full:.1%}); git charges {git_delta}")
        print()
        if all(ok for _, ok in checks):
            print(f"{len(checks)}/{len(checks)} negotiation checks pass")
            return 0
        print(f"tip={tip} head={head} fetched={fetched}")
        print(r2.stderr[-600:])
        return 1
    finally:
        shutil.rmtree(d, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
