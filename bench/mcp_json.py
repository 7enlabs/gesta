#!/usr/bin/env python3
"""Check the MCP server and JSON output against a real process.

The MCP server is driven over stdio the way a client would: newline-delimited
JSON-RPC. Every assertion reads the JSON the server printed, not an exit code.

Run: python3 bench/mcp_json.py
"""

import json
import os
import shutil
import subprocess
import sys

ZF = os.environ.get("ZF_BIN", "/work/gesta/target/release/gesta")
ROOT = "/tmp/zf_mcp"
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


def mcp_session(cwd, messages):
    """Send newline-delimited JSON-RPC and return the parsed responses."""
    proc = subprocess.Popen(
        [ZF, "mcp"],
        cwd=cwd,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    payload = "\n".join(json.dumps(m) for m in messages) + "\n"
    out, err = proc.communicate(payload, timeout=60)
    responses = []
    for line in out.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            responses.append(json.loads(line))
        except json.JSONDecodeError as e:
            responses.append({"__parse_error__": str(e), "__raw__": line[:200]})
    return responses, err


def main():
    if not os.path.exists(ZF):
        sys.exit(f"binary not found: {ZF}")
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(ROOT)

    repo = os.path.join(ROOT, "repo")
    os.makedirs(repo)
    run("init", cwd=repo)
    write(repo, "src/main.rs", "fn main() {\n    println!(\"hi\");\n}\n")
    write(repo, "README.md", "# demo\n")
    commit(repo, "initial commit")

    # --- MCP over stdio ---
    responses, err = mcp_session(
        repo,
        [
            {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}},
            {"jsonrpc": "2.0", "method": "notifications/initialized"},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
            {
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {"name": "gesta_status", "arguments": {}},
            },
        ],
    )

    record("server answers with one JSON object per line", len(responses) == 3,
           f"got {len(responses)} responses, stderr={err[:120]}")

    by_id = {r.get("id"): r for r in responses if isinstance(r, dict)}
    init = by_id.get(1, {})
    result = init.get("result", {})
    record("initialize reports protocol version and identity",
           result.get("protocolVersion") is not None
           and result.get("serverInfo", {}).get("name") == "gesta",
           json.dumps(init)[:250])

    tools_resp = by_id.get(2, {}).get("result", {})
    tools = tools_resp.get("tools", [])
    names = [t.get("name") for t in tools]
    expected = {
        "gesta_status", "gesta_log", "gesta_read_file",
        "gesta_blame", "gesta_diff", "gesta_create_changeset",
        "gesta_commit",
    }
    record("tools/list returns the full set with schemas",
           expected.issubset(set(names)) and all("inputSchema" in t for t in tools),
           f"tools={names}")

    status_resp = by_id.get(3, {}).get("result", {})
    structured = status_resp.get("structuredContent", {})
    record("status tool returns structured content the model can read",
           structured.get("branch") == "main"
           and isinstance(structured.get("pending_changesets"), list),
           json.dumps(status_resp)[:250])

    # --- read_file without a working copy ---
    os.remove(os.path.join(repo, "src/main.rs"))
    responses, _ = mcp_session(
        repo,
        [{
            "jsonrpc": "2.0", "id": 10, "method": "tools/call",
            "params": {"name": "gesta_read_file", "arguments": {"path": "src/main.rs"}},
        }],
    )
    body = responses[0].get("result", {}).get("structuredContent", {}) if responses else {}
    record("read_file serves content that is not on disk",
           "fn main()" in body.get("content", ""), json.dumps(body)[:200])

    # A removed file means the working tree no longer matches; put it back so the
    # later checks start from a clean tree.
    write(repo, "src/main.rs", "fn main() {\n    println!(\"hi\");\n}\n")

    # --- create + commit through MCP ---
    write(repo, "src/lib.rs", "pub fn add(a: i32, b: i32) -> i32 { a + b }\n")
    responses, _ = mcp_session(
        repo,
        [{
            "jsonrpc": "2.0", "id": 20, "method": "tools/call",
            "params": {
                "name": "gesta_create_changeset",
                "arguments": {"message": "add lib", "author": "mcp-client"},
            },
        }],
    )
    created = responses[0].get("result", {}).get("structuredContent", {}) if responses else {}
    cid = created.get("id")
    record("create_changeset through MCP returns an id", bool(cid), json.dumps(created)[:200])

    if cid:
        responses, _ = mcp_session(
            repo,
            [{
                "jsonrpc": "2.0", "id": 21, "method": "tools/call",
                "params": {"name": "gesta_commit", "arguments": {"id": cid}},
            }],
        )
        committed = responses[0].get("result", {}).get("structuredContent", {}) if responses else {}
        record("commit through MCP moves the head",
               committed.get("committed") is True, json.dumps(committed)[:200])

    # --- an error is reported inside the protocol, not on stderr ---
    responses, _ = mcp_session(
        repo,
        [{
            "jsonrpc": "2.0", "id": 30, "method": "tools/call",
            "params": {"name": "gesta_read_file", "arguments": {"path": "nope.txt"}},
        }],
    )
    has_error = bool(responses) and "error" in responses[0]
    record("a failing tool returns a JSON-RPC error", has_error,
           json.dumps(responses[0])[:200] if responses else "no response")

    # --- JSON mode on the CLI ---
    rc, out = run("status", "--json", cwd=repo)
    try:
        parsed = json.loads(out)
    except json.JSONDecodeError:
        parsed = {}
    record("status --json emits valid JSON",
           rc == 0 and parsed.get("branch") == "main", out[:250])

    rc, out = run("log", "--json", "--limit", "5", cwd=repo)
    try:
        entries = json.loads(out)
    except json.JSONDecodeError:
        entries = []
    record("log --json emits an array of changesets",
           rc == 0 and isinstance(entries, list) and len(entries) >= 2
           and "id" in entries[0], out[:250])

    rc, out = run("blame", "src/lib.rs", "--json", cwd=repo)
    try:
        parsed = json.loads(out)
    except json.JSONDecodeError:
        parsed = {}
    record("blame --json emits one entry per line",
           rc == 0 and len(parsed.get("lines", [])) == 1
           and parsed["lines"][0].get("author") == "mcp-client", out[:250])

    rc, out = run("history", "src/lib.rs", "--json", cwd=repo)
    try:
        parsed = json.loads(out)
    except json.JSONDecodeError:
        parsed = {}
    record("history --json includes the rename chain field",
           rc == 0 and isinstance(parsed.get("rename_chain"), list)
           and len(parsed.get("versions", [])) == 1, out[:250])

    rc, out = run("diff", "--json", cwd=repo)
    try:
        parsed = json.loads(out)
    except json.JSONDecodeError:
        parsed = {}
    record("diff --json describes the working tree",
           rc == 0 and parsed.get("branch") == "main" and "added" in parsed, out[:200])

    rc, out = run("branch", "list", "--json", cwd=repo)
    try:
        parsed = json.loads(out)
    except json.JSONDecodeError:
        parsed = {}
    record("branch list --json names the current branch",
           rc == 0 and parsed.get("current") == "main"
           and any(b.get("name") == "main" for b in parsed.get("branches", [])), out[:200])

    passed = sum(1 for _, ok, _ in results if ok)
    total = len(results)
    print(f"\n{passed}/{total} MCP and JSON checks pass")
    if passed < total:
        print("\nfailing:")
        for name, ok, _ in results:
            if not ok:
                print(f"  - {name}")
    return 0 if passed == total else 1


if __name__ == "__main__":
    sys.exit(main())
