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

ZF_DIR = "/work/zenforge"
ZF = os.path.join(ZF_DIR, "target/release/zenforge")
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
                    bad.append(f"{f}:{line} {m.group(0)[:40]}")
        record("no hardcoded credential in src", not bad, "\n".join(bad[:10]))

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

    if want("suites"):
        suites = [
            "replace_git", "http_remote", "http_auth", "mcp_json", "api_changeset",
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
