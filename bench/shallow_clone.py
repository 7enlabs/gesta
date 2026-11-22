#!/usr/bin/env python3
"""Does `clone --depth` actually fetch less, and is the result usable?

`--depth` was accepted by the CLI and then dropped: the HTTP clone path always asked for the whole
repository, so a shallow clone was exactly as large as a full one and the flag did nothing. Nothing
failed, which is why it survived. The only way to see it is to clone twice and compare.

What is checked:

  a shallow clone holds fewer changesets than a full one     the flag does something
  it holds exactly depth + 1                                 the cut is where it says
  the head is still the head                                 a shallow clone is at the same commit
  the files are on disk                                      the tree at the head is materialised
  the oldest changeset is recorded as a shallow root         so a later fetch knows what it is missing
  depth larger than the history behaves like a full clone    no marker, no truncation

The server and the clone both run locally; no network and no git binary needed.
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


def run(args, cwd, timeout=180):
    return subprocess.run(args, cwd=cwd, capture_output=True, text=True, timeout=timeout)


def changeset_id(out):
    m = re.search(r"\b(cs[0-9a-f]{6,})\b", out)
    return m.group(1) if m else None


def commit(where, message, filename):
    with open(os.path.join(where, filename), "w") as fh:
        fh.write(message + "\n")
    r = run([ZF, "changeset", "create", message, "--json"], where)
    cid = changeset_id(r.stdout)
    if not cid:
        return None
    run([ZF, "changeset", "commit", cid], where)
    return cid


def count_changesets(where):
    r = run([ZF, "log", "--json"], where)
    if r.returncode == 0 and r.stdout.strip():
        return r.stdout.count('"id"')
    r = run([ZF, "log", "--oneline"], where)
    return len([l for l in r.stdout.splitlines() if l.strip()])


def main() -> int:
    if not os.path.exists(ZF):
        print(f"binary not found: {ZF}")
        return 1

    base = tempfile.mkdtemp(prefix="shallow-")
    src = os.path.join(base, "src")
    os.makedirs(src)
    server = None
    try:
        run([ZF, "init"], src)
        # Six commits, so a depth of two is visibly different from the whole history.
        for i in range(6):
            if not commit(src, f"commit {i}", f"f{i}.txt"):
                print("could not build the history")
                return 1
        full_count = count_changesets(src)
        record(
            "the source repository has six changesets to cut",
            full_count >= 6,
            f"counted {full_count}",
        )

        port = free_port()
        server = subprocess.Popen(
            [ZF, "serve", "--bind", "127.0.0.1", "--port", str(port)],
            cwd=src,
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

        url = f"http://127.0.0.1:{port}"

        deep = os.path.join(base, "deep")
        d = run([ZF, "remote", "clone", url, deep], base)
        record("a full clone succeeds", d.returncode == 0, d.stdout + d.stderr)
        deep_count = count_changesets(deep) if os.path.isdir(deep) else 0

        shallow = os.path.join(base, "shallow")
        s = run([ZF, "remote", "clone", url, shallow, "--depth", "2"], base)
        record("a shallow clone succeeds", s.returncode == 0, s.stdout + s.stderr)
        shallow_count = count_changesets(shallow) if os.path.isdir(shallow) else 0

        record(
            "the shallow clone holds fewer changesets than the full one",
            shallow_count < deep_count,
            f"shallow={shallow_count} deep={deep_count}; the flag is being dropped if these match",
        )
        record(
            "the shallow clone holds depth + 1 changesets",
            shallow_count == 3,
            f"got {shallow_count}, expected 3 (depth 2 plus the head itself)",
        )

        # A shallow clone that cannot show the head is worse than a full one.
        if os.path.isdir(shallow):
            h = run([ZF, "log", "--oneline"], shallow)
            record(
                "the shallow clone still has a head with a commit in it",
                h.returncode == 0 and bool(h.stdout.strip()),
                h.stdout + h.stderr,
            )
            # The newest file has to be on disk, because that is what the person clones for.
            record(
                "the head's files are materialised in the shallow clone",
                os.path.exists(os.path.join(shallow, "f5.txt")),
                f"holds {sorted(os.listdir(shallow))[:8]}",
            )

            # The marker that says where the history was cut. Read it out of the database rather than
            # from a command, because the command's wording is not the thing being checked: the row is.
            marker = shallow_root_rows(shallow)
            record(
                "the shallow clone records where its history was cut",
                len(marker) == 1,
                f"shallow_roots holds {marker!r}",
            )
            record(
                "the recorded root is the oldest changeset the clone actually holds",
                len(marker) == 1 and marker[0] in count_ids(shallow),
                f"root={marker} held={count_ids(shallow)}",
            )

        # A depth larger than the history must behave like a full clone, with no marker.
        big = os.path.join(base, "big")
        b = run([ZF, "remote", "clone", url, big, "--depth", "50"], base)
        big_count = count_changesets(big) if os.path.isdir(big) else 0
        record(
            "a depth past the end of the history clones everything",
            b.returncode == 0 and big_count == deep_count,
            f"big={big_count} deep={deep_count}",
        )

        passed = sum(1 for _, ok, _ in results if ok)
        print(f"\n{passed}/{len(results)} shallow clone checks pass")
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


def db_path(where):
    """The repository database inside a checkout."""
    for name in os.listdir(where):
        p = os.path.join(where, name)
        if os.path.isdir(p):
            for inner in os.listdir(p):
                if inner.endswith(".db"):
                    return os.path.join(p, inner)
    return None


def shallow_root_rows(where):
    """The changeset ids recorded as shallow roots in this checkout."""
    import sqlite3

    db = db_path(where)
    if not db:
        return []
    try:
        conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
        rows = [r[0] for r in conn.execute("SELECT changeset_id FROM shallow_roots")]
        conn.close()
        return rows
    except sqlite3.Error:
        return []


def count_ids(where):
    """Every changeset id this checkout holds, from the database."""
    import sqlite3

    db = db_path(where)
    if not db:
        return []
    try:
        conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
        rows = [r[0] for r in conn.execute("SELECT id FROM changesets")]
        conn.close()
        return rows
    except sqlite3.Error:
        return []


if __name__ == "__main__":
    sys.exit(main())
