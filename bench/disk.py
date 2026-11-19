#!/usr/bin/env python3
"""How much disk does a real tree take, in both tools, each at its best?

The number this replaces was wrong twice over. It compared gesta against a git repository that had
never been packed, and it named a table as the suspect without measuring it. Both mistakes made the
ratio look like a design failure rather than what it was.

So this script does three things a comparison has to do to be worth reporting:

- Packs git before measuring it, because a loose object store is not git's steady state.
- VACUUMs gesta before measuring it, for the same reason.
- Breaks the database down by table, so the largest number names a table instead of a suspicion.

Run: python3 bench/disk.py [tree]
Default tree is the vault mount. Any directory works; it is copied twice.
"""

import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile

# Found relative to this file. A benchmark that only runs in one container is not a
# measurement another machine can repeat.
_PROJECT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
GESTA = os.environ.get("GESTA_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))
DEFAULT_TREE = "/workspace"
WORK = "/tmp/gesta_disk"


def sh(cmd, cwd=None):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True, cwd=cwd)


def size_of(path):
    total = 0
    for root, _dirs, files in os.walk(path):
        for name in files:
            p = os.path.join(root, name)
            try:
                total += os.path.getsize(p)
            except OSError:
                continue
    return total


def mb(n):
    return f"{n / 1048576:.1f} MB"


def table_sizes(db):
    """Bytes per table and index, largest first, or None when dbstat is unavailable."""
    conn = sqlite3.connect(db)
    try:
        rows = conn.execute(
            "SELECT name, SUM(pgsize) FROM dbstat GROUP BY name ORDER BY 2 DESC"
        ).fetchall()
    except sqlite3.Error:
        return None
    finally:
        conn.close()
    return rows


def main(argv):
    tree = argv[1] if len(argv) > 1 else DEFAULT_TREE
    if not os.path.isdir(tree):
        sys.exit(f"not a directory: {tree}")
    if not os.path.exists(GESTA):
        sys.exit(f"binary not found: {GESTA}")

    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(WORK)

    # One copy per tool, from the same source, so a difference is the tool's.
    src = os.path.join(WORK, "src")
    print(f"copying {tree} ...")
    shutil.copytree(tree, src, symlinks=True, ignore_dangling_symlinks=True)

    # A checkout that carries its own metadata would be measured twice.
    for name in (".git", ".gesta"):
        p = os.path.join(src, name)
        if os.path.isdir(p):
            shutil.rmtree(p, ignore_errors=True)

    gtree = os.path.join(WORK, "git")
    ztree = os.path.join(WORK, "gesta")
    shutil.copytree(src, gtree, symlinks=True, ignore_dangling_symlinks=True)
    shutil.copytree(src, ztree, symlinks=True, ignore_dangling_symlinks=True)

    files = sum(len(fs) for _r, _d, fs in os.walk(src))
    content = size_of(src)
    print(f"tree: {files} files, {mb(content)}")

    # git, then packed.
    sh("git init -q -b main .", cwd=gtree)
    sh("git config user.email bench@example.com", cwd=gtree)
    sh("git config user.name bench", cwd=gtree)
    r = sh("git add -A", cwd=gtree)
    if r.returncode != 0:
        sys.exit(f"git add failed: {r.stderr[:400]}")
    sh("git commit -qm seed", cwd=gtree)
    before = size_of(os.path.join(gtree, ".git"))
    print(f"git, loose objects        {mb(before)}")
    sh("git gc -q", cwd=gtree)
    # `git gc` detaches and returns before it finishes, so a size taken right after the call
    # measured the repository it had not packed yet. --no-detach is the flag that makes the number
    # mean what the line above it says.
    sh("git gc -q --no-detach", cwd=gtree)
    # Verified against git's own accounting, so a stray file in .git cannot be blamed on the pack.
    counted = sh("git count-objects -v", cwd=gtree).stdout
    pack_kb = 0
    for line in counted.splitlines():
        if line.startswith("size-pack:"):
            pack_kb = int(line.split(":")[1].strip() or 0)
    after = size_of(os.path.join(gtree, ".git"))
    print(f"git, after gc             {mb(after)}  (pack alone {mb(pack_kb * 1024)})")

    # gesta, then vacuumed.
    sh(f"{GESTA} init", cwd=ztree)
    r = sh(f'{GESTA} changeset create "seed" --author bench --json', cwd=ztree)
    import json

    try:
        cs = json.loads(r.stdout).get("changeset")
    except Exception:
        sys.exit(f"gesta created no changeset: {r.stdout[:300]} {r.stderr[:300]}")
    if not cs:
        sys.exit("gesta created no changeset, so its store is empty")
    sh(f"{GESTA} changeset commit {cs}", cwd=ztree)

    db = os.path.join(ztree, ".gesta", "gesta.db")
    zbefore = size_of(os.path.join(ztree, ".gesta"))
    conn = sqlite3.connect(db)
    conn.execute("VACUUM")
    conn.close()
    zafter = size_of(os.path.join(ztree, ".gesta"))
    print(f"gesta, before vacuum      {mb(zbefore)}")
    print(f"gesta, after vacuum       {mb(zafter)}")

    print()
    print(f"ratio, both at their best {zafter / after:.2f}x" if after else "ratio: git store is empty")
    print()

    rows = table_sizes(db)
    if rows:
        total = sum(r[1] for r in rows)
        print("gesta by table:")
        for name, n in rows[:8]:
            print(f"  {name:<26} {mb(n):>9}  {100 * n / total:>5.1f}%")
        rest = total - sum(n for _name, n in rows[:8])
        if rest > 0:
            print(f"  {'everything else':<26} {mb(rest):>9}  {100 * rest / total:>5.1f}%")
        print()
        print(f"  {'total':<26} {mb(total):>9}")
    else:
        print("dbstat is not available in this SQLite build, so no per table breakdown")

    print()
    print(f"the copies are in {WORK}; remove them when done")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
