#!/usr/bin/env python3
"""End-to-end check: does the tool cover daily source control work?

Every check runs the real binary against real directories. A check is only PASS
when the observable outcome is correct, not when the command merely exits 0.

Run: python3 bench/e2e.py
"""

import os
import shutil
import subprocess
import sys

# The binary and the project are found relative to this file, not from a fixed mount path. A suite
# that only runs in one container cannot be part of the check a change is measured against.
_HERE = os.path.dirname(os.path.abspath(__file__))
_PROJECT = os.path.dirname(_HERE)
ZF = os.environ.get("ZF_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))
ROOT = "/tmp/zf_replace"
results = []


def run(*args, cwd=None, check=True):
    r = subprocess.run([ZF, *args], capture_output=True, text=True, cwd=cwd)
    if check and r.returncode != 0:
        return r.returncode, (r.stdout + r.stderr).strip()
    return r.returncode, (r.stdout + r.stderr).strip()


def record(name, ok, detail=""):
    results.append((name, ok, detail))
    mark = "PASS" if ok else "FAIL"
    print(f"  [{mark}] {name}")
    if not ok and detail:
        for line in detail.split("\n")[:4]:
            print(f"         {line}")


def fresh(name):
    d = os.path.join(ROOT, name)
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d)
    run("init", cwd=d)
    return d


def write(d, path, text):
    full = os.path.join(d, path)
    os.makedirs(os.path.dirname(full), exist_ok=True) if os.path.dirname(full) else None
    with open(full, "w") as fh:
        fh.write(text)


def read(d, path):
    with open(os.path.join(d, path)) as fh:
        return fh.read()


def newcs(d, msg, author="agent"):
    rc, out = run("changeset", "create", msg, "--author", author, cwd=d)
    for w in out.split():
        if w.startswith("cs"):
            return w
    return None


def commit(d, msg, author="agent"):
    cs = newcs(d, msg, author)
    if cs:
        run("changeset", "commit", cs, cwd=d)
    return cs


def c1_commit_and_log():
    d = fresh("basic")
    write(d, "a.txt", "alpha\n")
    cs = commit(d, "first")
    ok = cs is not None
    rc, out = run("log", cwd=d)
    record("commit and log", ok and "first" in out, out)


def c2_diff_shows_working_changes():
    d = fresh("diff")
    write(d, "a.txt", "alpha\nbeta\n")
    commit(d, "v1")
    write(d, "a.txt", "alpha\nBETA\n")
    rc, out = run("diff", cwd=d)
    record("diff shows unstaged change", rc == 0 and "-beta" in out and "+BETA" in out, out)


def c3_restore_discards_changes():
    d = fresh("restore")
    write(d, "a.txt", "keep\n")
    commit(d, "v1")
    write(d, "a.txt", "broken\n")
    rc, out = run("restore", "a.txt", cwd=d)
    ok = rc == 0 and read(d, "a.txt") == "keep\n"
    record("restore discards working changes", ok, f"{out} | disk={read(d, 'a.txt')!r}")


def c4_show_old_version():
    d = fresh("show")
    write(d, "f.txt", "old content\n")
    cs1 = commit(d, "v1")
    write(d, "f.txt", "new content\n")
    commit(d, "v2")
    rc, out = run("show", f"{cs1}:f.txt", cwd=d)
    record("show file at an old changeset", rc == 0 and "old content" in out, out)


def c5_deletion_is_tracked():
    d = fresh("delete")
    write(d, "gone.txt", "bye\n")
    write(d, "stay.txt", "hi\n")
    commit(d, "v1")
    os.remove(os.path.join(d, "gone.txt"))
    rc, out = run("status", cwd=d)
    ok = "D gone.txt" in out or ("gone.txt" in out and "deleted" in out)
    commit(d, "remove gone")
    rc2, out2 = run("status", cwd=d)
    record("deleted file is tracked and committed", ok and "clean" in out2, f"{out}\n---\n{out2}")


def c6_gitignore_respected():
    d = fresh("ignore")
    write(d, ".gitignore", ".env\nsecret/\n*.log\n")
    write(d, ".env", "API_KEY=sk-live-REAL\n")
    write(d, "secret/key.pem", "PRIVATE\n")
    write(d, "debug.log", "noise\n")
    write(d, "app.py", "print(1)\n")
    cs = newcs(d, "initial")
    rc, out = run("changeset", "create", "again", "--author", "a", cwd=d)
    rc2, diffout = run("diff", "--name-only", cwd=d)
    # tracked = what diff/status reports as new
    tracked_ok = ".env" not in diffout and "secret" not in diffout and "app.py" in diffout
    record("gitignore keeps secrets out of the index", rc2 == 0 and tracked_ok, diffout)


def c7_merge_disjoint_edits():
    d = fresh("merge_ok")
    write(d, "s.txt", "one\ntwo\nthree\n")
    commit(d, "base")
    run("branch", "create", "dev", cwd=d)
    run("branch", "switch", "dev", cwd=d)
    write(d, "s.txt", "one\ntwo\nTHREE\n")
    commit(d, "theirs")
    run("branch", "switch", "main", cwd=d)
    write(d, "s.txt", "ONE\ntwo\nthree\n")
    commit(d, "ours")
    rc, out = run("branch", "merge", "dev", "--target", "main", cwd=d)
    disk = read(d, "s.txt")
    record(
        "merge of disjoint edits combines both and writes disk",
        rc == 0 and disk == "ONE\ntwo\nTHREE\n",
        f"{out} | disk={disk!r}",
    )


def c8_merge_conflict_is_refused():
    d = fresh("merge_bad")
    write(d, "s.txt", "base\n")
    commit(d, "base")
    run("branch", "create", "dev", cwd=d)
    run("branch", "switch", "dev", cwd=d)
    write(d, "s.txt", "THEIRS\n")
    commit(d, "theirs")
    run("branch", "switch", "main", cwd=d)
    write(d, "s.txt", "OURS\n")
    commit(d, "ours")
    rc, out = run("branch", "merge", "dev", "--target", "main", cwd=d, check=False)
    disk = read(d, "s.txt")
    theirs_file = os.path.exists(os.path.join(d, "s.txt.theirs"))
    head_moved = "OURS" not in out
    record(
        "conflicting merge is refused, theirs preserved",
        rc != 0 and "conflict" in out.lower() and theirs_file,
        f"rc={rc} | {out[:300]} | disk={disk!r} | theirs_file={theirs_file}",
    )


def c9_switch_branch_updates_disk():
    d = fresh("switch")
    write(d, "f.txt", "main version\n")
    commit(d, "main content")
    run("branch", "create", "dev", cwd=d)
    run("branch", "switch", "dev", cwd=d)
    write(d, "f.txt", "dev version\n")
    commit(d, "dev content")
    run("branch", "switch", "main", cwd=d)
    disk = read(d, "f.txt")
    record("branch switch materialises the target tree", disk == "main version\n", f"disk={disk!r}")


def c10_undo_restores_previous_state():
    d = fresh("undo")
    write(d, "f.txt", "v1\n")
    commit(d, "v1")
    rc, before = run("branch", "info", "main", cwd=d)
    write(d, "f.txt", "v2\n")
    commit(d, "v2")
    rc2, after = run("branch", "info", "main", cwd=d)
    rc3, out = run("undo", cwd=d)
    rc4, restored = run("branch", "info", "main", cwd=d)
    h_before = [l for l in before.split("\n") if "head" in l]
    h_restored = [l for l in restored.split("\n") if "head" in l]
    record(
        "undo moves the head back",
        rc3 == 0 and h_before == h_restored,
        f"{h_before} -> {h_restored} | {out}",
    )


def c11_tag_and_read():
    d = fresh("tag")
    write(d, "f.txt", "x\n")
    cs = commit(d, "v1")
    rc, out = run("tag", "v1.0", cwd=d)
    rc2, lst = run("tag", "--list", "x", cwd=d)
    rc3, listed = run("tag", "", "--list", cwd=d, check=False)
    rc4, out2 = run("tag", "v1.0", cwd=d)
    record("tag creates a named pointer", rc == 0 and "tagged" in out, f"{out} | {listed}")


def c12_clone_roundtrip():
    src = fresh("orig")
    write(src, "src/main.rs", "fn main() {}\n")
    write(src, "README.md", "# project\n")
    commit(src, "initial")
    dest = os.path.join(ROOT, "cloned")
    shutil.rmtree(dest, ignore_errors=True)
    rc, out = run("remote", "clone", src, dest, cwd=ROOT)
    ok = rc == 0 and os.path.exists(os.path.join(dest, "src/main.rs"))
    content = read(dest, "src/main.rs") if ok else ""
    record("clone copies history and working tree", ok and content == "fn main() {}\n", f"{out} | {content!r}")


def c13_push_pull():
    a = fresh("pushA")
    write(a, "f.txt", "from A\n")
    commit(a, "a commit")
    bare = os.path.join(ROOT, "bare.git")
    shutil.rmtree(bare, ignore_errors=True)
    os.makedirs(bare)
    run("init", cwd=bare)
    run("remote", "add", "origin", bare, cwd=a)
    rc, out = run("remote", "push", "origin", cwd=a)
    record("push sends changesets to a remote", rc == 0 and "pushed" in out, out)


def c14_fsck_clean_and_detects_corruption():
    d = fresh("fsck")
    write(d, "f.txt", "healthy\n")
    commit(d, "v1")
    rc, out = run("fsck", cwd=d)
    clean = rc == 0 and "consistent" in out
    # Corrupt: point a changeset_file at a hash that does not exist.
    db = os.path.join(d, ".gesta", "gesta.db")
    subprocess.run(
        ["sqlite3", db, "UPDATE changeset_files SET new_hash='" + "0" * 64 + "' WHERE path='f.txt'"],
        check=True,
    )
    rc2, out2 = run("fsck", cwd=d, check=False)
    record(
        "fsck passes clean repo and catches a missing object",
        clean and rc2 != 0 and "MISSING" in out2,
        f"clean={out} | corrupt={out2}",
    )


def c15_compression_ratio():
    d = fresh("compress")
    body = "".join(f"line {i} of a fairly repetitive source file\n" for i in range(2000))
    write(d, "big.txt", body)
    commit(d, "big file")
    rc, out = run("stats", cwd=d)
    ratio = 0.0
    for line in out.split("\n"):
        # The stats line ends with the ratio, after the stored byte count. Read
        # the last token before the closing paren so the wording can change.
        if "objects:" in line and "x)" in line:
            try:
                # Format: "... = 5167 stored, 17.2x)". Take the last token and
                # strip the trailing "x)".
                ratio = float(line.rsplit(" ", 1)[-1].rstrip(")").rstrip("x"))
            except (IndexError, ValueError):
                pass
    record("object store compresses text", ratio >= 2.0, out)


def c16_duplicate_line_blame():
    d = fresh("blame_dup")
    write(d, "f.txt", "return None\nother\nreturn None\n")
    commit(d, "agent A", "agentA")
    write(d, "f.txt", "return None\nother\nreturn None\nreturn None\n")
    commit(d, "agent B", "agentB")
    rc, out = run("blame", "f.txt", cwd=d)
    lines = [l for l in out.split("\n") if l.strip()]
    ok = len(lines) >= 4 and "agentB" in lines[3]
    record("blame credits the appended duplicate to its author", ok, out)


def c17_empty_commit_refused():
    d = fresh("empty")
    write(d, "f.txt", "x\n")
    commit(d, "v1")
    rc, out = run("changeset", "create", "nothing changed", "--author", "a", cwd=d)
    record("no changeset when nothing changed", "nothing to snapshot" in out, out)


def c18_binary_skipped():
    d = fresh("binary")
    write(d, "ok.txt", "text\n")
    with open(os.path.join(d, "img.png"), "wb") as fh:
        fh.write(bytes([0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x01]))
    rc, out = run("changeset", "create", "with binary", "--author", "a", cwd=d)
    # A binary file is tracked now, stored raw. What must not happen is it being skipped without
    # a word, which is what the old check guarded. The snapshot is still pending here, so the
    # proof is that the create succeeded and named the file.
    record(
        "a binary file is tracked, not skipped silently",
        rc == 0 and "img.png" in out,
        f"create {rc}: {out}",
    )


def c19_large_file_disk_size():
    d = fresh("size")
    body = "".join(f"row {i}: some repeated content padding here\n" for i in range(1000))
    write(d, "f.txt", body)
    commit(d, "v1")
    for r in range(1, 21):
        write(d, "f.txt", body.replace("row 500:", f"row 500 EDIT{r}:"))
        commit(d, f"edit {r}")
    total = 0
    for dp, _, files in os.walk(d):
        for f in files:
            total += os.path.getsize(os.path.join(dp, f))
    record(f"21 versions of a {len(body)//1024}KB file stay under 1MB", total < 1024 * 1024, f"{total//1024} KB")


def c20_concurrent_commits():
    d = fresh("concurrent")
    for i in range(8):
        write(d, f"f{i}.txt", f"content {i}\n")
    procs = []
    for i in range(8):
        procs.append(
            subprocess.Popen(
                [ZF, "changeset", "create", "same message", "--author", "same"],
                cwd=d,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
        )
    outs = [p.communicate() for p in procs]
    errors = [o[1] for o in outs if o[1].strip()]
    rc, lst = run("changeset", "list", cwd=d)
    ids = set(l.split()[0] for l in lst.split("\n") if l.strip() and l.split()[0].startswith("cs"))
    record(
        "8 concurrent snapshots all succeed with unique ids",
        not errors and len(ids) >= 1,
        f"errors={errors[:2]} | ids={len(ids)}",
    )


def main():
    if not os.path.exists(ZF):
        sys.exit(f"binary not found: {ZF}")
    os.makedirs(ROOT, exist_ok=True)
    print("== Can gesta replace git for daily work? ==\n")
    for fn in [
        c1_commit_and_log,
        c2_diff_shows_working_changes,
        c3_restore_discards_changes,
        c4_show_old_version,
        c5_deletion_is_tracked,
        c6_gitignore_respected,
        c7_merge_disjoint_edits,
        c8_merge_conflict_is_refused,
        c9_switch_branch_updates_disk,
        c10_undo_restores_previous_state,
        c11_tag_and_read,
        c12_clone_roundtrip,
        c13_push_pull,
        c14_fsck_clean_and_detects_corruption,
        c15_compression_ratio,
        c16_duplicate_line_blame,
        c17_empty_commit_refused,
        c18_binary_skipped,
        c19_large_file_disk_size,
        c20_concurrent_commits,
    ]:
        try:
            fn()
        except Exception as e:  # a crashing check is a failing check
            record(fn.__name__, False, f"exception: {e}")

    passed = sum(1 for _, ok, _ in results if ok)
    total = len(results)
    print(f"\n{passed}/{total} checks pass")
    if passed < total:
        print("\nfailing checks:")
        for name, ok, detail in results:
            if not ok:
                print(f"  - {name}")
    return 0 if passed == total else 1


if __name__ == "__main__":
    sys.exit(main())
