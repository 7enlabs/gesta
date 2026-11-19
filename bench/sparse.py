#!/usr/bin/env python3
"""Check sparse checkout, including the failure mode that matters.

The dangerous case is not "fewer files on disk". It is a scan that treats every
in-scope file whose path is outside the sparse set as deleted, because the next
commit would then wipe the rest of the repository. These checks confirm the
scope is applied to both sides of the comparison.

Run: python3 bench/sparse.py
"""

import json
import os
import shutil
import subprocess
import sys

# The binary and the project are found relative to this file, not from a fixed mount path. A suite
# that only runs in one container cannot be part of the check a change is measured against.
_HERE = os.path.dirname(os.path.abspath(__file__))
_PROJECT = os.path.dirname(_HERE)
ZF = os.environ.get("ZF_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))
ROOT = "/tmp/zf_sparse"
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


def run_stdout(*args, cwd=None):
    r = subprocess.run([ZF, *args], capture_output=True, text=True, cwd=cwd)
    return r.returncode, r.stdout.strip()


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


def tracked_paths(d):
    rc, out = run_stdout("log", "--json", "--limit", "50", cwd=d)
    return out


def main():
    if not os.path.exists(ZF):
        sys.exit(f"binary not found: {ZF}")
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(ROOT)
    repo = os.path.join(ROOT, "repo")
    os.makedirs(repo)
    run("init", cwd=repo)

    # A tree with two modules and some docs.
    write(repo, "api/src/main.rs", "fn main() {}\n")
    write(repo, "api/src/lib.rs", "pub fn a() {}\n")
    write(repo, "web/src/app.ts", "export const a = 1;\n")
    write(repo, "web/src/view.ts", "export const b = 2;\n")
    write(repo, "docs/guide.md", "# guide\n")
    write(repo, "README.md", "# repo\n")
    commit(repo, "initial tree")

    rc, out = run("status", cwd=repo)
    record("a full checkout reports a clean tree",
           "working tree clean" in out, out[:200])

    # Narrow the working set to api/.
    rc, out = run("sparse", "set", "api", cwd=repo)
    record("sparse set accepts a directory prefix",
           rc == 0 and "pattern" in out, out[:200])

    rc, out = run("sparse", "list", cwd=repo)
    record("sparse list shows the working set", "api" in out, out[:150])

    # The critical check: the tree must still be clean.
    rc, out = run("status", cwd=repo)
    record("files outside the scope are not reported as deleted",
           "deleted" not in out.lower().split("sparse")[0]
           or "deleted (0)" in out, out[:400])
    record("status names the sparse scope so the user knows why",
           "sparse:" in out, out[:300])

    # And no changeset is produced from the narrowed tree.
    rc, out = run("changeset", "create", "should do nothing", cwd=repo)
    record("a narrowed tree with no edits creates no changeset",
           "no changes" in out.lower() or "nothing" in out.lower(), out[:200])

    # Edit inside the scope: that must be seen.
    write(repo, "api/src/lib.rs", "pub fn a() {}\npub fn b() {}\n")
    rc, out = run("status", cwd=repo)
    record("an edit inside the scope is reported",
           "modified" in out.lower() and "api/src/lib.rs" in out, out[:300])
    commit(repo, "edit inside the scope")

    # Edit outside the scope on disk: the scope says ignore it.
    write(repo, "docs/guide.md", "# guide changed outside the scope\n")
    rc, out = run("status", cwd=repo)
    record("an edit outside the scope is ignored by status",
           "docs/guide.md" not in out, out[:400])

    # Committing must not pick that file up either.
    write(repo, "api/src/main.rs", "// touched\nfn main() {}\n")
    commit(repo, "second edit inside the scope")
    rc, out = run_stdout("show", "docs/guide.md", cwd=repo)
    record("the outside file kept its committed content",
           out.strip() == "# guide", f"got {out[:80]!r}")

    # Widening back must restore everything, including the untouched edits.
    rc, out = run("sparse", "clear", cwd=repo)
    record("sparse clear restores a full checkout",
           rc == 0 and "restored" in out, out[:200])

    files = ["api/src/main.rs", "api/src/lib.rs", "web/src/app.ts",
             "web/src/view.ts", "docs/guide.md", "README.md"]
    missing = [f for f in files if not os.path.exists(os.path.join(repo, f))]
    record("every tracked file exists on disk after clearing",
           not missing, f"missing: {missing}")

    with open(os.path.join(repo, "docs/guide.md")) as fh:
        guide = fh.read()
    record("clearing the scope discards a change made outside it",
           guide.strip() == "# guide", f"docs/guide.md now holds {guide[:60]!r}")

    # A glob scope, to check the matcher is not just a prefix test.
    def deleted_list(d):
        """The deleted paths, as the machine-readable field rather than prose."""
        rc, out = run_stdout("status", "--json", cwd=d)
        try:
            return json.loads(out).get("deleted", None)
        except json.JSONDecodeError:
            return None

    rc, out = run("sparse", "set", "web/**", cwd=repo)
    record("a glob scope is accepted", rc == 0, out[:150])
    dele = deleted_list(repo)
    record("a glob scope also keeps the tree clean",
           dele == [], f"deleted={dele}")

    # A scope that matches nothing must not silently empty the repository.
    rc, out = run("sparse", "set", "no-such-dir", cwd=repo)
    dele = deleted_list(repo)
    record("a scope matching nothing does not report mass deletion",
           dele == [], f"deleted={dele}")

    # The real protection: after all that, committing must not remove anything.
    head_before = run("show", "web/src/app.ts", cwd=repo)[1].strip()
    rc, out = run("changeset", "create", "noop under an empty scope", cwd=repo)
    rc2, after = run("show", "web/src/app.ts", cwd=repo)
    record("a narrow scope never removes a tracked file from history",
           rc2 == 0 and after.strip() == head_before, f"got {after[:60]!r}")

    rc, out = run("sparse", "clear", cwd=repo)
    passed = sum(1 for _, ok, _ in results if ok)
    total = len(results)
    print(f"\n{passed}/{total} sparse checks pass")
    if passed < total:
        print("\nfailing:")
        for name, ok, _ in results:
            if not ok:
                print(f"  - {name}")
    return 0 if passed == total else 1


if __name__ == "__main__":
    sys.exit(main())
