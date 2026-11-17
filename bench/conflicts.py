#!/usr/bin/env python3
"""Check structured conflict reports on a real repository.

A merge that conflicts must hand an agent both sides of every region with line
numbers, not just marker text. These checks build a real conflict and read the
JSON the CLI and the MCP tool produce.

Run: python3 bench/conflicts.py
"""

import json
import os
import shutil
import subprocess
import sys

ZF = os.environ.get("ZF_BIN", "/work/gesta/target/release/gesta")
ROOT = "/tmp/zf_conflicts"
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
    """stdout only. The error line goes to stderr, and mixing the two makes JSON
    parsing fail on exactly the cases where the command is reporting a failure."""
    r = subprocess.run([ZF, *args], capture_output=True, text=True, cwd=cwd)
    return r.returncode, r.stdout.strip()


def commit(d, msg, author="agent"):
    rc, out = run("changeset", "create", msg, "--author", author, cwd=d)
    for w in out.split():
        if w.startswith("cs"):
            run("changeset", "commit", w, cwd=d)
            return w
    return None


def write(d, path, text):
    full = os.path.join(d, path)
    parent = os.path.dirname(full)
    if parent:
        os.makedirs(parent, exist_ok=True)
    with open(full, "w") as fh:
        fh.write(text)


def main():
    if not os.path.exists(ZF):
        sys.exit(f"binary not found: {ZF}")
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(ROOT)
    repo = os.path.join(ROOT, "repo")
    os.makedirs(repo)
    run("init", cwd=repo)

    base = "line one\nline two\nline three\nline four\nline five\n"
    write(repo, "app.py", base)
    commit(repo, "base")

    run("branch", "create", "feature", cwd=repo)

    # Both sides edit the same line differently.
    write(repo, "app.py", "line one\nline two\nMAIN CHANGE\nline four\nline five\n")
    commit(repo, "main edit")

    run("branch", "switch", "feature", cwd=repo)
    write(repo, "app.py", "line one\nline two\nFEATURE CHANGE\nline four\nline five\n")
    commit(repo, "feature edit")

    # Preview must not touch the working tree.
    before = open(os.path.join(repo, "app.py")).read()
    rc, out = run("branch", "conflicts", "feature", "--target", "main", cwd=repo)
    after = open(os.path.join(repo, "app.py")).read()
    try:
        parsed = json.loads(out)
    except json.JSONDecodeError:
        parsed = {}
    record("conflicts preview emits valid JSON",
           rc == 0 and parsed.get("conflict_count") == 1, out[:300])
    record("the preview does not modify the working tree",
           before == after, "app.py changed during a dry run")

    files = parsed.get("files", [])
    regions = files[0].get("regions", []) if files else []
    reg = regions[0] if regions else {}
    record("the region names our side as lines",
           reg.get("ours") == ["MAIN CHANGE"], json.dumps(reg)[:250])
    record("the region names their side as lines",
           reg.get("theirs") == ["FEATURE CHANGE"], json.dumps(reg)[:250])
    record("the region carries line numbers on both sides",
           isinstance(reg.get("ours_start"), int)
           and isinstance(reg.get("theirs_start"), int)
           and reg["ours_start"] > 0 and reg["theirs_start"] > 0,
           json.dumps({k: reg.get(k) for k in ("ours_start", "ours_end", "theirs_start", "theirs_end")}))
    record("the region carries context so a resolver can judge",
           reg.get("context_before") == ["line one", "line two"]
           and reg.get("context_after") == ["line four", "line five"],
           json.dumps({k: reg.get(k) for k in ("context_before", "context_after")}))

    record("both whole files travel with the report",
           files and files[0].get("ours", "").count("MAIN CHANGE") == 1
           and files[0].get("theirs", "").count("FEATURE CHANGE") == 1,
           str(files[0].get("ours"))[:120] if files else "no files")

    record("the base version travels too",
           files and files[0].get("base") == base,
           str(files[0].get("base"))[:120] if files else "no files")

    # A real merge must still refuse and keep their side on disk.
    rc, out = run("branch", "merge", "feature", "--target", "main", cwd=repo)
    record("a conflicted merge refuses and says so",
           rc != 0 and "conflict" in out.lower(), out[:200])
    record("their version is preserved beside the file",
           os.path.exists(os.path.join(repo, "app.py.theirs")),
           "app.py.theirs is missing")

    # Merge JSON mode reports the same regions and exits non-zero.
    rc, out = run_stdout("branch", "merge", "feature", "--target", "main", "--json", cwd=repo)
    try:
        mj = json.loads(out)
    except json.JSONDecodeError:
        mj = {}
    record("merge --json reports conflicts as data",
           rc != 0 and mj.get("merged") is False
           and mj.get("conflicts", {}).get("conflict_count") == 1,
           out[:300])

    # An agent resolves, then merges for real.
    write(repo, "app.py", "line one\nline two\nRESOLVED BY AGENT\nline four\nline five\n")
    os.remove(os.path.join(repo, "app.py.theirs"))
    commit(repo, "resolve conflict")

    rc, out = run("branch", "conflicts", "feature", "--target", "main", cwd=repo)
    # After resolving on main, the feature branch is still conflicting, so this
    # should still report one region rather than silently succeeding.
    record("the preview still reports the unresolved branch side",
           rc == 0 and '"conflict_count":1' in out.replace(" ", ""), out[:200])

    # MCP exposes the same report as a tool.
    proc = subprocess.Popen(
        [ZF, "mcp"],
        cwd=repo,
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True,
    )
    msgs = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/call",
         "params": {"name": "gesta_merge_conflicts",
                    "arguments": {"source": "feature", "target": "main"}}},
    ]
    payload = "\n".join(json.dumps(m) for m in msgs) + "\n"
    out, _ = proc.communicate(payload, timeout=60)
    responses = {}
    for line in out.splitlines():
        line = line.strip()
        if line:
            try:
                r = json.loads(line)
                responses[r.get("id")] = r
            except json.JSONDecodeError:
                pass
    sc = responses.get(2, {}).get("result", {}).get("structuredContent", {})
    record("the MCP tool returns the same structured conflicts",
           sc.get("conflict_count") == 1, json.dumps(sc)[:250])

    passed = sum(1 for _, ok, _ in results if ok)
    total = len(results)
    print(f"\n{passed}/{total} conflict checks pass")
    if passed < total:
        print("\nfailing:")
        for name, ok, _ in results:
            if not ok:
                print(f"  - {name}")
    return 0 if passed == total else 1


if __name__ == "__main__":
    sys.exit(main())
