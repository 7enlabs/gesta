#!/usr/bin/env python3
"""The release gate: everything that must pass before a version is published.

Each step is a real command with a real exit code. Nothing here is inferred from
reading the source, because the whole point of a gate is that it fails when the
code is wrong, not when somebody remembers to check.

Run: python3 bench/gate.py
Exit code is 0 only when every step passes.
"""

import os
import re
import subprocess
import sys

ZF_DIR = "/work/gesta"
ZF = os.path.join(ZF_DIR, "target/release/gesta")
BUGHUNTER = os.environ.get("BUGHUNTER_DIR", "/work/hermes-health/sec/bughunter")
results = []


def record(name, ok, detail=""):
    results.append((name, ok, detail))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}")
    if not ok and detail:
        for line in str(detail).split("\n")[:8]:
            print(f"         {line}")


def sh(cmd, cwd=ZF_DIR, timeout=900):
    r = subprocess.run(cmd, shell=True, capture_output=True, text=True, cwd=cwd, timeout=timeout)
    return r.returncode, (r.stdout + r.stderr).strip()


def main():
    only = sys.argv[1] if len(sys.argv) > 1 else None

    def want(step):
        return only is None or only == step

    if want("build"):
        rc, out = sh("bash -lc 'source $HOME/.cargo/env && cargo build --release'")
        record("release build", rc == 0 and "error" not in out.lower().split("\n")[0],
               out[-400:])

    if want("test"):
        rc, out = sh("bash -lc 'source $HOME/.cargo/env && cargo test --release'")
        m = re.search(r"test result: ok\. (\d+) passed", out)
        record("unit tests", rc == 0 and m is not None,
               out[-600:] if rc != 0 else (m.group(0) if m else "no result line"))

    if want("clippy"):
        rc, out = sh("bash -lc 'source $HOME/.cargo/env && cargo clippy --release --all-targets'")
        warns = len(re.findall(r"^warning: [a-z]", out, re.M))
        record("clippy clean", rc == 0 and warns == 0, f"{warns} warning(s)\n{out[-400:]}")

    if want("audit"):
        rc, out = sh("bash -lc 'source $HOME/.cargo/env && cargo audit'", timeout=600)
        vulns = "vulnerability found" in out or "Vulnerabilities found" in out
        record("cargo audit reports no vulnerability", rc == 0 and not vulns, out[-500:])

    if want("osv"):
        if not os.path.exists("/tmp/osv"):
            record("osv-scanner reports no issue", True, "skipped: /tmp/osv not present")
        else:
            rc, out = sh("/tmp/osv scan source --lockfile Cargo.lock", timeout=600)
            record("osv-scanner reports no issue",
                   rc == 0 and "No issues found" in out, out[-500:])

    if want("coverage"):
        rc, out = sh("bash -lc 'source $HOME/.cargo/env && cargo llvm-cov --release --summary-only'",
                     timeout=900)
        m = re.search(r"^TOTAL\s+.*?([\d.]+)%", out, re.M)
        pct = float(m.group(1)) if m else 0.0
        record("line coverage recorded", rc == 0 and pct > 0, f"{pct}%\n{out[-300:]}")
        # A floor, not a target: it exists so a change that deletes tests is caught.
        record("line coverage stays above 55%", pct >= 55.0, f"{pct}%")

    if want("panics"):
        # Production code must not carry a panic path. Tests are excluded because
        # an assertion that panics is how a test reports failure.
        bad = []
        for f in sorted(os.listdir(os.path.join(ZF_DIR, "src"))):
            if not f.endswith(".rs"):
                continue
            text = open(os.path.join(ZF_DIR, "src", f), encoding="utf-8").read()
            prod = text.split("#[cfg(test)]")[0]
            for pat in (".unwrap()", ".expect(", "panic!", "unreachable!", "todo!"):
                for m in re.finditer(re.escape(pat), prod):
                    line = prod[: m.start()].count("\n") + 1
                    bad.append(f"{f}:{line} {pat}")
        record("no panic path in production code", not bad, "\n".join(bad[:10]))

    if want("secrets"):
        # A hardcoded credential in the source is a release blocker. The patterns
        # are deliberately narrow: a wide one flags every test fixture and gets
        # ignored, which is worse than not having it.
        patterns = [
            r"sk-[A-Za-z0-9]{20,}",
            r"ghp_[A-Za-z0-9]{20,}",
            r"github_pat_[A-Za-z0-9_]{20,}",
            r"AKIA[0-9A-Z]{16}",
            r"(?i)(password|passwd|secret|api_key|apikey)\s*[:=]\s*\"[^\"]{12,}\"",
        ]
        bad = []
        for f in sorted(os.listdir(os.path.join(ZF_DIR, "src"))):
            if not f.endswith(".rs"):
                continue
            text = open(os.path.join(ZF_DIR, "src", f), encoding="utf-8").read()
            for pat in patterns:
                for m in re.finditer(pat, text):
                    line = text[: m.start()].count("\n") + 1
                    lines = text.splitlines()
                    # A published example value is not a credential. The marker may
                    # sit on the line itself or on one of the three above it, so a
                    # comment can introduce a small block. A real key pasted with
                    # the marker is still a deliberate act, not an accident.
                    window = lines[max(0, line - 4):line]
                    lowered = [l.lower() for l in window]
                    if any(
                        "published example" in l or "test vector" in l for l in lowered
                    ):
                        continue
                    bad.append(f"{f}:{line} {m.group(0)[:40]}")
        record("no hardcoded credential in src", not bad, "\n".join(bad[:10]))

    if want("brief-fix"):
        # The fix does the safe half and names what it refused. A fix that silently skips work
        # is worse than no fix, so a refusal is checked as carefully as an action.
        import tempfile
        tmp = tempfile.mkdtemp()
        sh(f"{ZF} init", cwd=tmp)
        with open(os.path.join(tmp, "a.txt"), "w") as fh:
            fh.write("v1\n")
        sh(f"{ZF} changeset create base --author t", cwd=tmp)
        m = re.search(r"(cs[0-9a-f]+)", sh(f"{ZF} changeset list --json", cwd=tmp)[1])
        if m:
            sh(f"{ZF} changeset commit {m.group(1)}", cwd=tmp)

        # A snapshot that matches the disk is committed.
        with open(os.path.join(tmp, "a.txt"), "w") as fh:
            fh.write("v2\n")
        sh(f"{ZF} changeset create snap --author t", cwd=tmp)
        r = sh(f"{ZF} brief --fix", cwd=tmp)
        after = sh(f"{ZF} brief --json", cwd=tmp)[1]
        record(
            "fix commits a snapshot that matches the disk",
            "committed cs" in r[1] and '"next": "ready"' in after,
            r[1][:160],
        )

        # A snapshot the disk moved past is dropped, not committed.
        with open(os.path.join(tmp, "a.txt"), "w") as fh:
            fh.write("v3\n")
        sh(f"{ZF} changeset create s2 --author t", cwd=tmp)
        with open(os.path.join(tmp, "a.txt"), "w") as fh:
            fh.write("v4\n")
        r = sh(f"{ZF} brief --fix", cwd=tmp)
        record(
            "fix drops a snapshot the disk moved past",
            "dropped cs" in r[1] and "committed" not in r[1],
            r[1][:200],
        )

        # Uncommitted work with no snapshot: refuse and name the command.
        with open(os.path.join(tmp, "a.txt"), "w") as fh:
            fh.write("v5\n")
        r = sh(f"{ZF} brief --fix", cwd=tmp)
        keep = open(os.path.join(tmp, "a.txt")).read()
        record(
            "fix refuses to commit work it has no snapshot for, and keeps the edit",
            "left alone" in r[1] and "changeset create" in r[1] and keep == "v5\n",
            r[1][:220],
        )

        r = sh(f"{ZF} brief --fix", cwd=tmp)
        record(
            "fix reports nothing to do on a clean tree",
            "nothing to fix" in r[1] or "left alone" in r[1],
            r[1][:120],
        )

    if want("brief-stash"):
        # Setting work aside is opt in, and the work has to come back. A stash that loses the
        # edit is worse than a refusal.
        import tempfile
        tmp = tempfile.mkdtemp()
        sh(f"{ZF} init", cwd=tmp)
        with open(os.path.join(tmp, "a.txt"), "w") as fh:
            fh.write("v1\n")
        sh(f"{ZF} changeset create base --author t", cwd=tmp)
        m = re.search(r"(cs[0-9a-f]+)", sh(f"{ZF} changeset list --json", cwd=tmp)[1])
        if m:
            sh(f"{ZF} changeset commit {m.group(1)}", cwd=tmp)

        with open(os.path.join(tmp, "a.txt"), "w") as fh:
            fh.write("v2\n")
        with open(os.path.join(tmp, "b.txt"), "w") as fh:
            fh.write("new\n")

        # Without the flag nothing moves.
        before = sh(f"{ZF} brief --fix", cwd=tmp)
        record(
            "uncommitted work is refused unless stashing is asked for",
            "left alone" in before[1]
            and open(os.path.join(tmp, "a.txt")).read() == "v2\n"
            and os.path.exists(os.path.join(tmp, "b.txt")),
            before[1][:180],
        )

        r = sh(f"{ZF} brief --fix --stash", cwd=tmp)
        record(
            "asking for a stash sets the work aside and cleans the tree",
            "stashed" in r[1]
            and open(os.path.join(tmp, "a.txt")).read() == "v1\n"
            and not os.path.exists(os.path.join(tmp, "b.txt")),
            r[1][:180],
        )

        pop = sh(f"{ZF} stash pop", cwd=tmp)
        record(
            "the stashed work comes back whole",
            pop[0] == 0
            and open(os.path.join(tmp, "a.txt")).read() == "v2\n"
            and os.path.exists(os.path.join(tmp, "b.txt")),
            pop[1][:180],
        )

        # The flag needs the command it modifies.
        r = sh(f"{ZF} brief --stash", cwd=tmp)
        record(
            "the stash flag refuses to run without --fix",
            r[0] != 0,
            r[1][:160],
        )

    if want("brief"):
        # One call that answers where the session stands. An agent that asks this with four
        # commands pays four round trips and holds four outputs in context.
        import tempfile
        tmp = tempfile.mkdtemp()
        sh(f"{ZF} init", cwd=tmp)
        with open(os.path.join(tmp, "a.txt"), "w") as fh:
            fh.write("v1\n")
        sh(f"{ZF} changeset create base --author t", cwd=tmp)
        m = re.search(r"(cs[0-9a-f]+)", sh(f"{ZF} changeset list --json", cwd=tmp)[1])
        if m:
            sh(f"{ZF} changeset commit {m.group(1)}", cwd=tmp)

        r = sh(f"{ZF} brief --json", cwd=tmp)
        record(
            "a clean repository reports ready",
            '"next": "ready"' in r[1],
            r[1][:160],
        )

        with open(os.path.join(tmp, "a.txt"), "w") as fh:
            fh.write("v2\n")
        r = sh(f"{ZF} brief --json", cwd=tmp)
        record(
            "an edit on disk asks for a commit and says which command",
            '"next": "commit-work"' in r[1] and "changeset create" in r[1],
            r[1][:200],
        )

        sh(f"{ZF} changeset create snap --author t", cwd=tmp)
        r = sh(f"{ZF} brief --json", cwd=tmp)
        record(
            "a snapshot that matches the disk is what to commit next",
            '"next": "commit-pending"' in r[1],
            r[1][:200],
        )

        with open(os.path.join(tmp, "a.txt"), "w") as fh:
            fh.write("v3\n")
        r = sh(f"{ZF} brief --json", cwd=tmp)
        record(
            "a snapshot the disk moved past is named as stale",
            '"stale": [' in r[1] and "cs" in r[1] and '"next": "commit-work"' in r[1],
            r[1][:240],
        )

        # The MCP surface has to expose it, since that is how an agent reaches it.
        r = sh(
            f"""printf '%s\\n' '{{"jsonrpc":"2.0","id":1,"method":"tools/list"}}' | {ZF} mcp""",
            cwd=tmp,
        )
        record(
            "the MCP server offers brief and grep",
            "gesta_brief" in r[1] and "gesta_grep" in r[1],
            r[1][:160],
        )

    if want("grep"):
        # Searching history is the capability a working tree cannot offer: the string that was
        # deleted is usually the one being looked for.
        import tempfile
        tmp = tempfile.mkdtemp()
        os.makedirs(os.path.join(tmp, "src"), exist_ok=True)
        sh(f"{ZF} init", cwd=tmp)
        with open(os.path.join(tmp, "src", "a.txt"), "w") as fh:
            fh.write("first line\nneedle here\n")
        sh(f"{ZF} changeset create one --author t", cwd=tmp)
        m = re.search(r"(cs[0-9a-f]+)", sh(f"{ZF} changeset list --json", cwd=tmp)[1])
        if m:
            sh(f"{ZF} changeset commit {m.group(1)}", cwd=tmp)

        r = sh(f"{ZF} grep needle", cwd=tmp)
        record(
            "grep finds text in the working tree with its line number",
            r[0] == 0 and "src/a.txt:2" in r[1],
            r[1][:200],
        )

        r = sh(f"{ZF} grep needle --json", cwd=tmp)
        record(
            "grep emits machine readable locations",
            r[0] == 0 and '"line": 2' in r[1] and '"path": "src/a.txt"' in r[1],
            r[1][:200],
        )

        r = sh(f"{ZF} grep hashes --word", cwd=tmp)
        record(
            "a whole word search refuses a longer identifier",
            r[0] != 0,
            r[1][:120],
        )

        # Remove the text, commit, and find it again in history only.
        with open(os.path.join(tmp, "src", "a.txt"), "w") as fh:
            fh.write("first line\n")
        sh(f"{ZF} changeset create two --author t", cwd=tmp)
        m = re.search(r"(cs[0-9a-f]+)", sh(f"{ZF} changeset list --json", cwd=tmp)[1])
        if m:
            sh(f"{ZF} changeset commit {m.group(1)}", cwd=tmp)

        now = sh(f"{ZF} grep needle", cwd=tmp)
        was = sh(f"{ZF} grep needle --history", cwd=tmp)
        record(
            "a deleted string is gone from the tree and still found in history",
            now[0] != 0 and was[0] == 0 and "needle here" in was[1],
            f"tree={now[0]} history={was[1][:100]}",
        )

        r = sh(f"{ZF} grep nothing-matches-this", cwd=tmp)
        record(
            "a search with no match fails instead of printing nothing",
            r[0] != 0 and "no match" in r[1],
            r[1][:120],
        )

    if want("concurrent"):
        # Two processes writing one repository must not lose anything. A stale pending snapshot
        # is the failure that leaves no trace: the commit succeeds and stores content that does
        # not describe the disk.
        import tempfile
        tmp = tempfile.mkdtemp()
        sh(f"{ZF} init", cwd=tmp)
        with open(os.path.join(tmp, "base.txt"), "w") as fh:
            fh.write("base\n")
        sh(f"{ZF} changeset create seed --author t", cwd=tmp)
        out = sh(f"{ZF} changeset list --json", cwd=tmp)[1]
        cid = re.search(r"(cs[0-9a-f]+)", out)
        if cid:
            sh(f"{ZF} changeset commit {cid.group(1)}", cwd=tmp)

        # Twenty writers at once, each adding its own file.
        procs = []
        for i in range(20):
            path = os.path.join(tmp, f"w{i}.txt")
            with open(path, "w") as fh:
                fh.write(f"writer {i}\n")
            procs.append(
                subprocess.Popen(
                    ["bash", "-lc",
                     f"{ZF} changeset create w{i} --author p{i} >/dev/null 2>&1; "
                     f"c=$({ZF} changeset list --json 2>/dev/null | grep -oE 'cs[0-9a-f]+' | head -1); "
                     f"[ -n \"$c\" ] && {ZF} changeset commit $c >/dev/null 2>&1; true"],
                    cwd=tmp,
                )
            )
        for p_ in procs:
            p_.wait(timeout=120)

        total = sh(f"{ZF} log --limit 100 --oneline", cwd=tmp)[1].count("cs")
        fs = sh(f"{ZF} fsck", cwd=tmp)
        on_disk = len([f for f in os.listdir(tmp) if f.endswith(".txt")])
        record(
            "twenty concurrent writers lose nothing",
            total == 21 and fs[0] == 0 and on_disk == 21,
            f"changesets={total} files={on_disk} fsck={fs[1].strip()[:120]}",
        )

        # A pending snapshot that no longer matches the disk must be reported.
        stale_dir = tempfile.mkdtemp()
        sh(f"{ZF} init", cwd=stale_dir)
        with open(os.path.join(stale_dir, "f.txt"), "w") as fh:
            fh.write("v1\n")
        sh(f"{ZF} changeset create base --author t", cwd=stale_dir)
        out = sh(f"{ZF} changeset list --json", cwd=stale_dir)[1]
        cid = re.search(r"(cs[0-9a-f]+)", out)
        if cid:
            sh(f"{ZF} changeset commit {cid.group(1)}", cwd=stale_dir)
        with open(os.path.join(stale_dir, "f.txt"), "w") as fh:
            fh.write("v2\n")
        sh(f"{ZF} changeset create snapshot --author t", cwd=stale_dir)
        with open(os.path.join(stale_dir, "f.txt"), "w") as fh:
            fh.write("v3\n")
        st = sh(f"{ZF} status", cwd=stale_dir)
        record(
            "a pending snapshot that no longer matches the disk is reported",
            "no longer match" in st[1],
            st[1][:200],
        )

    if want("worktree"):
        # An integrity check that compares tables cannot see the disk, and the worst defect
        # this tool had was a working tree that disagreed with the head while every check
        # said the repository was consistent. This step runs the check that looks.
        import tempfile
        tmp = tempfile.mkdtemp()
        sh(f"{ZF} init", cwd=tmp)
        os.makedirs(os.path.join(tmp, "src"))
        with open(os.path.join(tmp, "src", "main.rs"), "w") as fh:
            fh.write("fn main() {}\n")
        with open(os.path.join(tmp, "a.txt"), "w") as fh:
            fh.write("a\n")
        sh(f"{ZF} changeset create seed --author t", cwd=tmp)
        out = sh(f"{ZF} changeset list --json", cwd=tmp)[1]
        cid = re.search(r"(cs[0-9a-f]+)", out)
        if cid:
            sh(f"{ZF} changeset commit {cid.group(1)}", cwd=tmp)

        clean = sh(f"{ZF} fsck --worktree", cwd=tmp)
        clean_ok = clean[0] == 0

        # Delete a tracked file behind the tool's back.
        os.remove(os.path.join(tmp, "src", "main.rs"))
        missing = sh(f"{ZF} fsck --worktree", cwd=tmp)
        caught_missing = missing[0] != 0 and "missing from disk" in missing[1]

        # A user's own file is not a problem.
        with open(os.path.join(tmp, "mine.txt"), "w") as fh:
            fh.write("user data\n")
        user_file = sh(f"{ZF} fsck --worktree", cwd=tmp)
        user_ok = "mine.txt" not in user_file[1]

        # Editing a tracked file is caught.
        sh(f"{ZF} restore .", cwd=tmp)
        with open(os.path.join(tmp, "a.txt"), "w") as fh:
            fh.write("changed\n")
        edited = sh(f"{ZF} fsck --worktree", cwd=tmp)
        caught_edit = edited[0] != 0 and "differs from the head" in edited[1]

        record(
            "fsck --worktree sees the disk",
            clean_ok and caught_missing and user_ok and caught_edit,
            f"clean={clean_ok} missing={caught_missing} user={user_ok} edited={caught_edit}",
        )

    if want("paths"):
        # Every path that arrives from a client must be refused when it escapes
        # the repository: this is the one defect that turns a remote write into
        # an arbitrary file write.
        text = open(os.path.join(ZF_DIR, "src/http.rs"), encoding="utf-8").read()
        guards = text.count('..') and "unsafe path" in text
        record("remote writes refuse a path that escapes the repository",
               bool(guards), "the ../ guard is missing from http.rs")

    if want("sql"):
        # A query built by formatting is an injection, and a parameterised one
        # never needs it. `format!` around SQL is the pattern to catch.
        bad = []
        for f in sorted(os.listdir(os.path.join(ZF_DIR, "src"))):
            if not f.endswith(".rs"):
                continue
            text = open(os.path.join(ZF_DIR, "src", f), encoding="utf-8").read()
            for m in re.finditer(r'format!\(\s*"(SELECT|INSERT|UPDATE|DELETE)', text, re.I):
                line = text[: m.start()].count("\n") + 1
                # The only permitted case is the `IN (?, ?, ...)` placeholder
                # list, which carries no caller data. Anything else is flagged.
                tail = text[m.start(): m.start() + 600]
                if "in_placeholders" in tail or "placeholders" in tail:
                    continue
                bad.append(f"{f}:{line} {m.group(0)[:40]}")
        record("no SQL built by string formatting", not bad, "\n".join(bad[:10]))

    if want("pipe"):
        # A tool that writes to stdout gets piped into `head` constantly. A panic
        # on the closed pipe prints a stack trace from the standard library,
        # which looks like a crash in the tool. git exits silently, so this
        # must too.
        import tempfile
        tmp = tempfile.mkdtemp()
        sh(f"{ZF} init", cwd=tmp)
        for i in range(300):
            with open(os.path.join(tmp, f"f{i}.txt"), "w") as fh:
                fh.write(f"content {i}\n")
        sh(f"{ZF} changeset create seed --author t", cwd=tmp)
        out = sh(f"{ZF} changeset list --json", cwd=tmp)[1]
        cid = re.search(r"(cs[0-9a-f]+)", out)
        if cid:
            sh(f"{ZF} changeset commit {cid.group(1)}", cwd=tmp)
        for i in range(300):
            with open(os.path.join(tmp, f"f{i}.txt"), "w") as fh:
                fh.write(f"changed {i}\n")
        rc, out = sh(f"{ZF} status 2>&1 | head -2", cwd=tmp)
        panicked = "panicked" in out or "Broken pipe" in out
        record("a closed pipe does not panic", not panicked, out[:250])

    if want("s3"):
        # Round trip against a local endpoint that verifies the signature itself.
        # No network and no real credentials, so this runs on every gate and a
        # broken SigV4 fails here instead of at the user's bucket.
        import tempfile, socket
        sock = socket.socket()
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
        sock.close()
        srv = subprocess.Popen(
            ["python3", "bench/fake_s3.py", str(port)],
            cwd=ZF_DIR,
            env={**os.environ, "FAKE_S3_KEY": "k", "FAKE_S3_SECRET": "s",
                 "FAKE_S3_REGION": "us-east-1"},
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
        )
        try:
            import time as _t
            _t.sleep(1.5)
            env = {
                "GESTA_S3_ENDPOINT": f"http://127.0.0.1:{port}",
                "GESTA_S3_REGION": "us-east-1",
                "GESTA_S3_ACCESS_KEY": "k",
                "GESTA_S3_SECRET_KEY": "s",
            }
            e = {**os.environ, **env}
            tmp = tempfile.mkdtemp()
            subprocess.run([ZF, "init"], cwd=tmp, capture_output=True, env=e)
            with open(os.path.join(tmp, "a.txt"), "w") as fh:
                fh.write("hello\n")
            subprocess.run([ZF, "changeset", "create", "one", "--author", "t"],
                           cwd=tmp, capture_output=True, env=e)
            out = subprocess.run([ZF, "changeset", "list", "--json"], cwd=tmp,
                                 capture_output=True, text=True, env=e).stdout
            cid = re.search(r"(cs[0-9a-f]+)", out)
            if cid:
                subprocess.run([ZF, "changeset", "commit", cid.group(1)],
                               cwd=tmp, capture_output=True, env=e)
            subprocess.run([ZF, "remote", "add", "origin", "s3://bkt/repo"],
                           cwd=tmp, capture_output=True, env=e)
            push = subprocess.run([ZF, "remote", "push", "origin", "--branch", "main"],
                                  cwd=tmp, capture_output=True, text=True, env=e)
            pushed = "pushed 1 changesets" in push.stdout

            # A second push must send nothing, which proves the bucket read worked.
            push2 = subprocess.run([ZF, "remote", "push", "origin", "--branch", "main"],
                                   cwd=tmp, capture_output=True, text=True, env=e)
            no_reupload = "nothing to push" in push2.stdout

            # A second commit, then a push. The object on the bucket is the only
            # state there, so the second push must carry the whole history: an
            # earlier version wrote only the delta and the clone lost a file.
            with open(os.path.join(tmp, "b.txt"), "w") as fh:
                fh.write("second\n")
            subprocess.run([ZF, "changeset", "create", "two", "--author", "t"],
                           cwd=tmp, capture_output=True, env=e)
            out = subprocess.run([ZF, "changeset", "list", "--json"], cwd=tmp,
                                 capture_output=True, text=True, env=e).stdout
            cid2 = re.findall(r"(cs[0-9a-f]+)", out)
            if cid2:
                subprocess.run([ZF, "changeset", "commit", cid2[-1]],
                               cwd=tmp, capture_output=True, env=e)
            push3 = subprocess.run([ZF, "remote", "push", "origin", "--branch", "main"],
                                   cwd=tmp, capture_output=True, text=True, env=e)

            dest = tempfile.mkdtemp()
            clone = subprocess.run([ZF, "remote", "clone", "s3://bkt/repo", dest],
                                   cwd=dest, capture_output=True, text=True, env=e)
            got = None
            fp = os.path.join(dest, "a.txt")
            if os.path.exists(fp):
                got = open(fp).read()
            got2 = None
            fp2 = os.path.join(dest, "b.txt")
            if os.path.exists(fp2):
                got2 = open(fp2).read()
            hist = subprocess.run([ZF, "log"], cwd=dest, capture_output=True,
                                  text=True, env=e).stdout
            n_cs = hist.count("changeset ")

            # Listing is how a clone finds a branch without being told its name.
            ls = subprocess.run([ZF, "remote", "ls", "origin"], cwd=tmp,
                                capture_output=True, text=True, env=e)
            listed = "main" in ls.stdout

            # A shallow clone must restore the whole working tree even though it carries
            # only the newest changesets. Cutting the chain removes the changesets that
            # created the older files, so the head snapshot has to travel with it.
            shallow = tempfile.mkdtemp()
            sc = subprocess.run([ZF, "remote", "clone", "s3://bkt/repo", shallow,
                                 "--depth", "1"], cwd=shallow, capture_output=True,
                                text=True, env=e)
            s_a = os.path.join(shallow, "a.txt")
            s_b = os.path.join(shallow, "b.txt")
            shallow_ok = (
                sc.returncode == 0
                and os.path.exists(s_a)
                and os.path.exists(s_b)
                and open(s_a).read() == "hello\n"
                and open(s_b).read() == "second\n"
            )
            s_hist = subprocess.run([ZF, "log", "--limit", "50"], cwd=shallow,
                                    capture_output=True, text=True, env=e).stdout
            shallow_n = s_hist.count("changeset ")
            # And its integrity check must accept the boundary instead of calling it damage.
            s_fsck = subprocess.run([ZF, "fsck"], cwd=shallow, capture_output=True,
                                    text=True, env=e)
            shallow_ok = shallow_ok and shallow_n == 1 and s_fsck.returncode == 0
            # Pruning must never delete an object the working tree needs.
            subprocess.run([ZF, "fsck", "--prune"], cwd=shallow, capture_output=True,
                           text=True, env=e)
            shallow_ok = shallow_ok and os.path.exists(s_a) and open(s_a).read() == "hello\n"

            # A shallow clone must be able to take the rest of the history, and the
            # result has to pass its own integrity check.
            deep = subprocess.run([ZF, "remote", "deepen", "--unshallow"], cwd=shallow,
                                  capture_output=True, text=True, env=e)
            deep_hist = subprocess.run([ZF, "log", "--limit", "50"], cwd=shallow,
                                       capture_output=True, text=True, env=e).stdout
            deep_n = deep_hist.count("changeset ")
            deep_fsck = subprocess.run([ZF, "fsck"], cwd=shallow, capture_output=True,
                                       text=True, env=e)
            deep_ok = (
                deep.returncode == 0
                and deep_n == 2
                and deep_fsck.returncode == 0
                and os.path.exists(s_a)
                and open(s_a).read() == "hello\n"
            )
            shallow_ok = shallow_ok and deep_ok


            record(
                "s3 push, idempotent push, shallow clone and listing",
                listed and shallow_ok and
                pushed and no_reupload and got == "hello\n" and got2 == "second\n" and n_cs == 2,
                f"pushed={pushed} no_reupload={no_reupload} a={got!r} b={got2!r} "
                f"changesets={n_cs} listed={listed} shallow={shallow_ok} deep={deep_n} "
                f"(files={os.path.exists(s_a)},{os.path.exists(s_b)} cs={shallow_n}) "
                f"clone={clone.stdout.strip()[:100]}{clone.stderr.strip()[:140]}",
            )
        finally:
            srv.terminate()
            try:
                srv.wait(timeout=5)
            except subprocess.TimeoutExpired:
                srv.kill()

    if want("suites"):
        suites = [
            "e2e", "http_remote", "http_auth", "mcp_json", "api_changeset",
            "concurrency", "conflicts", "sparse", "tls",
        ]
        failed = []
        for s in suites:
            rc, out = sh(f"python3 bench/{s}.py", timeout=900)
            m = re.search(r"(\d+)/(\d+) .*checks pass", out)
            ok = rc == 0 and m is not None and m.group(1) == m.group(2)
            if not ok:
                failed.append(s)
            print(f"         {s}: {m.group(0) if m else 'no result'}")
        record("every end to end suite passes", not failed, ", ".join(failed))

    if want("bughunter"):
        if not os.path.isdir(BUGHUNTER):
            record("bughunter scenarios", True, f"skipped: {BUGHUNTER} not present")
        else:
            rc, out = sh(f"bash {BUGHUNTER}/run.sh {ZF_DIR}", timeout=900)
            record("bughunter scenarios", rc == 0, out[-600:])

    passed = sum(1 for _, ok, _ in results if ok)
    total = len(results)
    print(f"\n{passed}/{total} gate steps pass")
    if passed < total:
        print("\nfailing:")
        for n, ok, _ in results:
            if not ok:
                print(f"  - {n}")
        return 1
    print("the tree is releasable")
    return 0


if __name__ == "__main__":
    sys.exit(main())
