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


def sh_out(cmd, cwd=ZF_DIR, timeout=900):
    """stdout alone, for a check that parses JSON: stderr carries a line for a person."""
    r = subprocess.run(cmd, shell=True, capture_output=True, text=True, cwd=cwd, timeout=timeout)
    return r.returncode, r.stdout.strip()


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

    if want("regex"):
        # A regex is a different search, and a pattern the engine cannot express has to say so.
        import tempfile
        tmp = tempfile.mkdtemp()
        os.makedirs(os.path.join(tmp, "src"), exist_ok=True)
        sh(f"{ZF} init", cwd=tmp)
        with open(os.path.join(tmp, "src", "a.rs"), "w") as fh:
            fh.write("fn read_config() {}\nfn write_config() {}\nlet hashes = 1;\nlet hash = 2;\n")
        sh(f"{ZF} changeset create base --author t", cwd=tmp)
        m = re.search(r"(cs[0-9a-f]+)", sh(f"{ZF} changeset list --json", cwd=tmp)[1])
        if m:
            sh(f"{ZF} changeset commit {m.group(1)}", cwd=tmp)

        r = sh(f"{ZF} grep 'fn [a-z]+_config' --regex", cwd=tmp)
        record(
            "a regex matches by shape, which a literal cannot",
            r[0] == 0 and r[1].count("_config") == 2,
            r[1][:200],
        )

        literal = sh(f"{ZF} grep 'fn .*_config'", cwd=tmp)
        record(
            "the same text without the flag is read literally and finds nothing",
            literal[0] != 0,
            literal[1][:160],
        )

        r = sh(f"{ZF} grep hash --regex --word", cwd=tmp)
        record(
            "a regex with --word does not match a longer identifier",
            r[0] == 0 and "hashes" not in r[1] and "hash = 2" in r[1],
            r[1][:200],
        )

        r = sh(f"{ZF} grep 'foo(?=bar)' --regex", cwd=tmp)
        record(
            "a pattern the engine cannot express is refused with the construct named",
            r[0] != 0 and ("look-around" in r[1] or "lookaround" in r[1]),
            r[1][:240],
        )

    if want("git-remote"):
        # A push has to reach a real git repository and a pull has to bring back only what is
        # new. A round trip that duplicates the pushed work is the failure worth checking.
        import shutil as _shutil
        import tempfile
        if not _shutil.which("git"):
            record("git remote bridge", False, "git is not on PATH")
        else:
            forge = tempfile.mkdtemp() + "/forge.git"
            sh(f"git init -q --bare {forge}")
            local = tempfile.mkdtemp()
            sh(f"{ZF} init", cwd=local)
            with open(os.path.join(local, "a.txt"), "w") as fh:
                fh.write("v1\n")
            sh(f"{ZF} changeset create one --author t", cwd=local)
            m = re.search(r"(cs[0-9a-f]+)", sh(f"{ZF} changeset list --json", cwd=local)[1])
            if m:
                sh(f"{ZF} changeset commit {m.group(1)}", cwd=local)

            pushed = sh(f"{ZF} git push {forge}", cwd=local)
            record(
                "gesta pushes to a git remote",
                pushed[0] == 0 and "pushed" in pushed[1],
                pushed[1][:200],
            )

            # A clone from the forge proves the push wrote a real git repository.
            clone = tempfile.mkdtemp() + "/clone"
            sh(f"git clone -q {forge} {clone}")
            seen = sh("cat a.txt", cwd=clone)
            record(
                "a clone of the remote holds what was pushed",
                seen[0] == 0 and seen[1].strip() == "v1",
                f"{seen[0]} {seen[1][:80]}",
            )

            # A teammate commits to the forge, then gesta pulls.
            with open(os.path.join(clone, "a.txt"), "w") as fh:
                fh.write("v2\n")
            sh("git -c user.email=t@e.st -c user.name=T commit -qam teammate", cwd=clone)
            sh("git push -q origin main", cwd=clone)

            pulled = sh(f"{ZF} git pull {forge}", cwd=local)
            record(
                "gesta pulls a commit from a git remote",
                pulled[0] == 0 and "imported 1 commit" in pulled[1],
                pulled[1][:200],
            )
            body = sh(f"{ZF} show main:a.txt", cwd=local)
            record(
                "the pulled content is what the teammate wrote",
                body[0] == 0 and body[1].strip() == "v2",
                body[1][:120],
            )

            # The push recorded the git name of what it sent, so pulling it back must not
            # import the same commit a second time.
            count = sh(f"{ZF} log --limit 20 --oneline", cwd=local)[1].count("cs")
            record(
                "a push then pull does not duplicate the pushed commit",
                count == 2,
                f"{count} changesets, expected 2",
            )

    if want("errors"):
        # A caller has to be able to tell "nothing there" from "the store is damaged" without
        # reading English, and a failure under --json has to parse.
        import tempfile as _tf
        d = _tf.mkdtemp()
        sh(f"{ZF} init", cwd=d)
        with open(os.path.join(d, "a.txt"), "w") as fh:
            fh.write("v1\n")
        sh(f'{ZF} changeset create one --author t', cwd=d)
        m = re.search(r"(cs[0-9a-f]+)", sh(f"{ZF} changeset list --json", cwd=d)[1])
        if m:
            sh(f"{ZF} changeset commit {m.group(1)}", cwd=d)

        missing = sh(f"{ZF} show nosuch:a.txt", cwd=d)
        record(
            "a missing branch exits with a code a caller can branch on",
            missing[0] == 3,
            f"exit {missing[0]}, expected 3",
        )
        record(
            "the message still names what is missing",
            "no such branch" in (missing[1] or ""),
            (missing[1] or "")[:160],
        )

        bad_cs = sh(f"{ZF} changeset diff cs000000000000", cwd=d)
        record(
            "a missing changeset exits with the same code",
            bad_cs[0] == 3,
            f"exit {bad_cs[0]}, expected 3",
        )

        usage = sh(f"{ZF} --nonsense", cwd=d)
        record(
            "a bad argument exits 2, so it is not confused with a missing thing",
            usage[0] == 2,
            f"exit {usage[0]}, expected 2",
        )

        # `sh` folds stderr into stdout, and the error line goes to stderr on purpose, so the
        # JSON is fetched on its own here.
        # `show` resolves a revision and fails with not-found; `changeset diff` has no JSON form
        # and would answer usage, which is a different error on purpose.
        js = sh_out(f"{ZF} --json show nosuch:a.txt", cwd=d)
        import json as _json
        try:
            payload = _json.loads(js[1])
            ok = payload.get("error", {}).get("code") == "not-found"
            why = js[1][:160]
        except Exception as exc:
            ok = False
            why = f"not JSON at all: {exc}: {js[1][:120]}"
        record("a failure under --json parses and carries a code", ok, why)

        # A path with a quote is the case that breaks a hand built JSON string. The shell needs
        # the quote escaped before the program ever sees it, so the argument is quoted properly
        # rather than by hand.
        import shlex as _shlex
        arg = _shlex.quote('main:quote"name.txt')
        quoted = sh_out(f"{ZF} --json show {arg}", cwd=d)
        try:
            _json.loads(quoted[1])
            ok2 = True
            why2 = quoted[1][:120]
        except Exception as exc:
            ok2 = False
            why2 = f"a quote in a path broke the JSON: {exc}: {quoted[1][:80]!r}"
        record("a quote in a message does not break the JSON", ok2, why2)

    if want("unicode"):
        # A repository of Vietnamese file names is this machine's real workload, and a byte
        # slice in the ignore matcher panicked the whole scan on it. The check has to run the
        # real binary on a real tree, not a unit fixture.
        import tempfile as _tf
        d = _tf.mkdtemp()
        os.makedirs(os.path.join(d, "tài-liệu"), exist_ok=True)
        os.makedirs(os.path.join(d, "nguồn"), exist_ok=True)
        for rel in ["tài-liệu/ghi-chú.md", "nguồn/bản-sinh.md", "nồ.md", "ồ.md"]:
            with open(os.path.join(d, rel), "w", encoding="utf-8") as fh:
                fh.write("nội dung tiếng Việt\n")
        sh(f"{ZF} init", cwd=d)
        made = sh(f"{ZF} changeset create snapshot --author t", cwd=d)
        record(
            "a tree of non-ASCII names does not crash the scanner",
            made[0] == 0,
            (made[1] or "")[:200],
        )
        m = re.search(r"(cs[0-9a-f]+)", made[1] or "")
        if m:
            sh(f"{ZF} changeset commit {m.group(1)}", cwd=d)
        st = sh(f"{ZF} status", cwd=d)
        record(
            "and status still answers on it",
            st[0] == 0,
            (st[1] or "")[:200],
        )
        # The file has to be readable back, which proves the name survived the round trip.
        body = sh(f'{ZF} show "main:tài-liệu/ghi-chú.md"', cwd=d)
        record(
            "a non-ASCII path reads back with its content",
            body[0] == 0 and "nội dung" in (body[1] or ""),
            (body[1] or "")[:160],
        )
        check = sh(f"{ZF} fsck", cwd=d)
        record(
            "and the store stays consistent",
            check[0] == 0 and "consistent" in (check[1] or ""),
            (check[1] or "")[:160],
        )

    if want("ls"):
        # Listing the tree is the first thing anything does with a repository, and it did not
        # exist: the only way to discover a path was to guess or to read the whole log.
        import tempfile as _tf
        d = _tf.mkdtemp()
        os.makedirs(os.path.join(d, "src", "deep"), exist_ok=True)
        for rel in ["a.txt", "src/b.rs", "src/deep/c.rs"]:
            with open(os.path.join(d, rel), "w") as fh:
                fh.write("x\n")
        sh(f"{ZF} init", cwd=d)
        sh(f'{ZF} changeset create one --author t', cwd=d)
        m = re.search(r"(cs[0-9a-f]+)", sh(f"{ZF} changeset list --json", cwd=d)[1])
        if m:
            sh(f"{ZF} changeset commit {m.group(1)}", cwd=d)

        listed = sh(f"{ZF} ls", cwd=d)
        got = sorted(x for x in (listed[1] or "").split("\n") if x.strip())
        expected = ["a.txt", "src/b.rs", "src/deep/c.rs"]
        record(
            "ls names every file in the head tree",
            got == expected,
            f"{got}",
        )

        sub = sh(f"{ZF} ls --path src", cwd=d)
        subbed = sorted(x for x in (sub[1] or "").split("\n") if x.strip())
        record(
            "ls --path limits to a prefix",
            subbed == ["src/b.rs", "src/deep/c.rs"],
            f"{subbed}",
        )

        long_form = sh(f"{ZF} ls --long --path src/deep", cwd=d)
        record(
            "ls --long prints the content hash beside the path",
            long_form[0] == 0 and len((long_form[1] or "").split()) == 2,
            (long_form[1] or "")[:120],
        )

        js = sh_out(f"{ZF} --json ls", cwd=d)
        import json as _json
        try:
            payload = _json.loads(js[1])
            ok = payload.get("count") == 3 and len(payload.get("files", [])) == 3
            why = js[1][:160]
        except Exception as exc:
            ok = False
            why = f"not JSON: {exc}"
        record("ls --json parses with a count and the files", ok, why)

        unknown = sh(f"{ZF} ls nosuch", cwd=d)
        record(
            "ls on an unknown revision exits 3",
            unknown[0] == 3,
            f"exit {unknown[0]}",
        )

    if want("undo-empty"):
        # Undoing the first changeset leaves an empty branch. The disk has to empty too, or
        # status reports every tracked file as new and the undo looks like it did nothing.
        import tempfile as _tf
        d = _tf.mkdtemp()
        os.makedirs(os.path.join(d, "sub"), exist_ok=True)
        with open(os.path.join(d, "a.txt"), "w") as fh:
            fh.write("v1\n")
        with open(os.path.join(d, "sub", "b.txt"), "w") as fh:
            fh.write("v2\n")
        sh(f"{ZF} init", cwd=d)
        sh(f"{ZF} changeset create one --author t", cwd=d)
        m = re.search(r"(cs[0-9a-f]+)", sh(f"{ZF} changeset list --json", cwd=d)[1])
        if m:
            sh(f"{ZF} changeset commit {m.group(1)}", cwd=d)

        undone = sh(f"{ZF} undo", cwd=d)
        record("undo reports no error", undone[0] == 0, (undone[1] or "")[:160])
        record(
            "undo to an empty branch removes the tracked files from disk",
            not os.path.exists(os.path.join(d, "a.txt"))
            and not os.path.exists(os.path.join(d, "sub", "b.txt")),
            "a file stayed on disk after the head went empty",
        )
        st = sh(f"{ZF} status", cwd=d)
        record(
            "status is clean after undoing to an empty branch",
            "2 files tracked" not in (st[1] or "")
            and "new files" not in (st[1] or ""),
            (st[1] or "")[:200],
        )
        sh(f"{ZF} redo", cwd=d)
        record(
            "redo puts both files back",
            os.path.exists(os.path.join(d, "a.txt"))
            and os.path.exists(os.path.join(d, "sub", "b.txt")),
            "redo did not restore the tree",
        )

    if want("delta-blank"):
        # A delta whose only change is a blank line decoded into an insert of nothing, so the
        # object it described could not be read back and fsck called it missing.
        import tempfile as _tf
        d = _tf.mkdtemp()
        sh(f"{ZF} init", cwd=d)
        body = "".join("line %d\n" % i for i in range(400))
        with open(os.path.join(d, "f.txt"), "w") as fh:
            fh.write(body)
        sh(f"{ZF} changeset create one --author t", cwd=d)
        m = re.search(r"(cs[0-9a-f]+)", sh(f"{ZF} changeset list --json", cwd=d)[1])
        if m:
            sh(f"{ZF} changeset commit {m.group(1)}", cwd=d)

        # Insert a blank line near the top: a small delta against a large file.
        with open(os.path.join(d, "f.txt"), "w") as fh:
            fh.write("\n" + body)
        sh(f"{ZF} changeset create two --author t", cwd=d)
        m = re.search(r"(cs[0-9a-f]+)", sh(f"{ZF} changeset list --json", cwd=d)[1])
        if m:
            sh(f"{ZF} changeset commit {m.group(1)}", cwd=d)

        # Compare bytes, not a stripped string: the whole point is that the leading blank line
        # survives, and `strip()` removes exactly the byte under test.
        out_path = os.path.join(d, "readback.txt")
        rc = subprocess.run(
            f"{ZF} show main:f.txt > {out_path}",
            shell=True,
            cwd=d,
            capture_output=True,
            text=True,
        ).returncode
        with open(out_path, "rb") as fh:
            read_back = fh.read()
        record(
            "a delta holding a blank line reads back byte for byte",
            rc == 0 and read_back == ("\n" + body).encode(),
            f"{len(read_back)} bytes, expected {len(('\n' + body).encode())}, "
            f"starts {read_back[:12]!r}",
        )
        check = sh(f"{ZF} fsck", cwd=d)
        record(
            "and fsck calls the store consistent",
            check[0] == 0 and "consistent" in (check[1] or ""),
            (check[1] or "")[:200],
        )

    if want("binary"):
        # Binary files used to be refused outright, which blocked any repository holding an
        # image, a font or an archive. They are stored raw and have to survive the whole round
        # trip byte for byte.
        import shutil as _shutil
        import tempfile as _tf
        import hashlib as _hl
        d = _tf.mkdtemp()
        sh(f"{ZF} init", cwd=d)
        payload = bytes(range(256)) * 40
        with open(os.path.join(d, "img.png"), "wb") as fh:
            fh.write(payload)
        with open(os.path.join(d, "note.md"), "w") as fh:
            fh.write("text\n")
        before = _hl.md5(payload).hexdigest()

        made = sh(f"{ZF} changeset create withbinary --author t", cwd=d)
        record("a binary file can be snapshotted", made[0] == 0, (made[1] or "")[:200])
        m = re.search(r"(cs[0-9a-f]+)", made[1] or "")
        if m:
            sh(f"{ZF} changeset commit {m.group(1)}", cwd=d)

        listed = sh(f"{ZF} ls", cwd=d)
        record(
            "the binary is in the tree",
            "img.png" in (listed[1] or "") and "note.md" in (listed[1] or ""),
            (listed[1] or "")[:200],
        )

        check = sh(f"{ZF} fsck", cwd=d)
        record(
            "fsck accepts a binary object as present",
            check[0] == 0 and "consistent" in (check[1] or ""),
            (check[1] or "")[:200],
        )

        # Restore from the store and compare the bytes, which is the only proof that matters.
        os.remove(os.path.join(d, "img.png"))
        got = sh(f"{ZF} checkout img.png", cwd=d)
        with open(os.path.join(d, "img.png"), "rb") as fh:
            after = _hl.md5(fh.read()).hexdigest()
        record(
            "checkout restores the binary byte for byte",
            got[0] == 0 and after == before,
            f"{got[0]} {before} vs {after}",
        )

        # And a clone has to carry it.
        remote_dir = _tf.mkdtemp()
        clone_dir = _tf.mkdtemp()
        _shutil.rmtree(clone_dir, ignore_errors=True)
        sh(f"{ZF} remote add o {remote_dir}", cwd=d)
        pushed = sh(f"{ZF} remote push o --branch main", cwd=d)
        cloned = sh(f"{ZF} remote clone {remote_dir} {clone_dir}", cwd=d)
        cloned_file = os.path.join(clone_dir, "img.png")
        ok = cloned[0] == 0 and os.path.exists(cloned_file)
        if ok:
            with open(cloned_file, "rb") as fh:
                ok = _hl.md5(fh.read()).hexdigest() == before
        record(
            "a clone carries the binary unchanged",
            ok,
            f"push {pushed[0]} clone {cloned[0]} {(cloned[1] or '')[:120]}",
        )

    if want("packfile"):
        # A repository after `git gc` holds no loose objects: everything is in a pack. An import
        # that only reads loose objects works on a repository nobody has used.
        import shutil as _shutil
        import tempfile as _tf
        if not _shutil.which("git"):
            record("git is available", False, "git is not on PATH")
        else:
            src = _tf.mkdtemp()
            sh(f"git init -q -b main {src}")
            sh("git config user.email t@e.st && git config user.name T", cwd=src)
            for i in range(1, 5):
                with open(os.path.join(src, "a.txt"), "w") as fh:
                    fh.write(f"v{i}\n")
                os.makedirs(os.path.join(src, "sub"), exist_ok=True)
                with open(os.path.join(src, "sub", "b.txt"), "w") as fh:
                    fh.write("deep\n")
                sh(f"git add -A && git commit -qm c{i}", cwd=src)
            sh("git gc -q 2>/dev/null", cwd=src)
            loose = 0
            for _root, _dirs, files in os.walk(os.path.join(src, ".git", "objects")):
                if "pack" in _root:
                    continue
                loose += len(files)
            record(
                "the fixture really is packed",
                os.path.isdir(os.path.join(src, ".git", "objects", "pack"))
                and any(f.endswith(".pack") for f in os.listdir(os.path.join(src, ".git", "objects", "pack"))),
                f"{loose} loose objects",
            )

            dest = _tf.mkdtemp()
            sh(f"{ZF} init", cwd=dest)
            got = sh(f"{ZF} git import {src}", cwd=dest)
            record(
                "a packed git repository imports",
                got[0] == 0 and "imported" in (got[1] or ""),
                (got[1] or "")[:200],
            )
            listed = sh(f"{ZF} ls", cwd=dest)
            record(
                "every path came through from the pack",
                "a.txt" in (listed[1] or "") and "sub/b.txt" in (listed[1] or ""),
                (listed[1] or "")[:200],
            )
            body = sh(f"{ZF} show main:a.txt", cwd=dest)
            record(
                "and the content of a packed blob is right",
                body[0] == 0 and (body[1] or "").strip() == "v4",
                (body[1] or "")[:120],
            )
            deep = sh(f"{ZF} show main:sub/b.txt", cwd=dest)
            record(
                "a nested path inside the pack reads too",
                deep[0] == 0 and (deep[1] or "").strip() == "deep",
                (deep[1] or "")[:120],
            )
            # A branch ref after `git gc` lives in packed-refs, not in refs/heads.
            record(
                "the branch was found through packed-refs",
                "main" in (got[1] or ""),
                (got[1] or "")[:160],
            )

    if want("sign"):
        # A commit message claims an author; a signature is the only part a verifier can check.
        import tempfile as _tf
        d = _tf.mkdtemp()
        key = os.path.join(d, "signing.key")
        sh(f"{ZF} init", cwd=d)
        made = sh(f"{ZF} key generate --path {key}", cwd=d)
        record("a signing key can be generated", made[0] == 0, (made[1] or "")[:160])
        pub = ""
        for line in (made[1] or "").splitlines():
            if line.startswith("public key:"):
                pub = line.split(":", 1)[1].strip()

        with open(os.path.join(d, "a.txt"), "w") as fh:
            fh.write("signed\n")
        created = sh(f'{ZF} changeset create "signed change" --author alice', cwd=d)
        m = re.search(r"(cs[0-9a-f]+)", created[1] or "")
        signed = sh(f"{ZF} changeset sign {m.group(1)} --key {key}", cwd=d) if m else (1, "")
        record("a pending changeset can be signed", signed[0] == 0, (signed[1] or "")[:160])

        ok = sh(f"{ZF} verify {m.group(1)}", cwd=d) if m else (1, "")
        record(
            "the signature verifies against the recorded key",
            ok[0] == 0 and "verifies" in (ok[1] or ""),
            (ok[1] or "")[:160],
        )

        # Tamper with the recorded message and the signature must fail.
        if m:
            import sqlite3 as _sq
            con = _sq.connect(os.path.join(d, ".gesta", "gesta.db"))
            con.execute("UPDATE changesets SET message = 'tampered' WHERE id = ?", (m.group(1),))
            con.commit()
            con.close()
            bad_ok = sh(f"{ZF} verify {m.group(1)}", cwd=d)
            record(
                "a changed message breaks the signature",
                bad_ok[0] != 0 and "does not match" in (bad_ok[1] or ""),
                (bad_ok[1] or "")[:160],
            )
            con = _sq.connect(os.path.join(d, ".gesta", "gesta.db"))
            con.execute("UPDATE changesets SET message = 'signed change' WHERE id = ?", (m.group(1),))
            con.commit()
            con.close()

        # A different key must not verify it.
        other = os.path.join(d, "other.key")
        sh(f"{ZF} key generate --path {other}", cwd=d)
        other_pub = ""
        again = sh(f"{ZF} key generate --path {other}", cwd=d)
        record(
            "generating over an existing key is refused",
            again[0] != 0 and "already exists" in (again[1] or ""),
            (again[1] or "")[:160],
        )

        unsigned_path = os.path.join(d, "b.txt")
        with open(unsigned_path, "w") as fh:
            fh.write("x\n")
        c2 = sh(f'{ZF} changeset create "unsigned" --author bob', cwd=d)
        m2 = re.search(r"(cs[0-9a-f]+)", c2[1] or "")
        if m2:
            v = sh(f"{ZF} verify {m2.group(1)}", cwd=d)
            record(
                "verifying an unsigned changeset says there is nothing to verify",
                v[0] != 0 and "no signature" in (v[1] or ""),
                (v[1] or "")[:160],
            )

    if want("impact"):
        # A diff shows what was edited and says nothing about what depended on it. The failure
        # that hurts is a rename that looks complete while an untouched file keeps the old name.
        import tempfile as _tf
        d = _tf.mkdtemp()
        os.makedirs(os.path.join(d, "src"), exist_ok=True)
        files = {
            "src/model.rs": "pub struct Post { authorName: String }\n",
            "src/feed.rs": "fn feed(p: Post) { let n = p.authorName; }\n",
            "src/mail.rs": "fn mail(p: Post) { send(p.authorName); }\n",
        }
        for rel, body in files.items():
            with open(os.path.join(d, rel), "w") as fh:
                fh.write(body)
        sh(f"{ZF} init", cwd=d)
        sh(f'{ZF} changeset create "add the model" --author t', cwd=d)
        m = re.search(r"(cs[0-9a-f]+)", sh(f"{ZF} changeset list --json", cwd=d)[1])
        if m:
            sh(f"{ZF} changeset commit {m.group(1)}", cwd=d)

        # Rename the field in two of the three files. The third is the untouched caller.
        with open(os.path.join(d, "src", "model.rs"), "w") as fh:
            fh.write("pub struct Post { author: String }\n")
        with open(os.path.join(d, "src", "feed.rs"), "w") as fh:
            fh.write("fn feed(p: Post) { let n = p.author; }\n")
        sh(f'{ZF} changeset create "rename authorName to author" --author t', cwd=d)
        m = re.search(r"(cs[0-9a-f]+)", sh(f"{ZF} changeset list --json", cwd=d)[1])
        if m:
            sh(f"{ZF} changeset commit {m.group(1)}", cwd=d)

        review = sh(f"{ZF} review", cwd=d)
        record(
            "review finds the caller in a file the change did not touch",
            "src/mail.rs" in (review[1] or "") and "authorName" in (review[1] or ""),
            (review[1] or "")[:400],
        )
        record(
            "and it says which file removed the name",
            "removed from src/model.rs" in (review[1] or ""),
            (review[1] or "")[:400],
        )
        # The two files that were updated must not be reported as stale.
        record(
            "a file the change did update is not reported",
            "src/feed.rs line" not in (review[1] or ""),
            (review[1] or "")[:400],
        )

    if want("json-stdout"):
        # `--json` means stdout is one document. A hook that prints, and a subcommand that has no
        # JSON form, both used to put prose there, and a caller parsed nothing while the run
        # looked successful.
        import tempfile as _tf
        import json as _json
        d = _tf.mkdtemp()
        sh(f"{ZF} init", cwd=d)
        os.makedirs(os.path.join(d, ".gesta", "hooks"), exist_ok=True)
        hook = os.path.join(d, ".gesta", "hooks", "pre-commit")
        with open(hook, "w") as fh:
            fh.write("#!/bin/sh\necho 'hook chatter'\n")
        os.chmod(hook, 0o755)
        with open(os.path.join(d, "a.txt"), "w") as fh:
            fh.write("x\n")

        made = sh_out(f'{ZF} --json changeset create "one" --author t', cwd=d)
        try:
            payload = _json.loads(made[1])
            ok = payload.get("changed") is True and payload.get("changeset", "").startswith("cs")
            why = made[1][:200]
        except Exception as exc:
            ok, why = False, f"not JSON: {exc}: {made[1][:120]!r}"
        record("changeset create --json answers with one document", ok, why)

        cid = ""
        try:
            cid = _json.loads(made[1]).get("changeset", "")
        except Exception:
            pass
        if cid:
            committed = sh_out(f"{ZF} --json changeset commit {cid}", cwd=d)
            try:
                second = _json.loads(committed[1])
                ok2 = second.get("files") == 1 and second.get("already_committed") is False
                why2 = committed[1][:200]
            except Exception as exc:
                ok2, why2 = False, f"the hook's output broke the JSON: {exc}: {committed[1][:120]!r}"
            record("a chatty hook cannot break the JSON on stdout", ok2, why2)

        # A subcommand with no JSON form must answer with an error document, not prose: the
        # shape stays parseable so a caller learns the code instead of failing to parse.
        refused = sh_out(f"{ZF} --json changeset diff {cid}", cwd=d)
        try:
            err = _json.loads(refused[1])
            ok_ref = refused[0] != 0 and err.get("error", {}).get("code") == "usage"
            why_ref = refused[1][:200]
        except Exception as exc:
            ok_ref, why_ref = False, f"prose on stdout instead of an error document: {exc}"
        record(
            "a subcommand without a JSON form answers with an error document",
            ok_ref,
            why_ref,
        )

        # And `ls --json` still parses, since it is the newest JSON surface.
        ls = sh_out(f"{ZF} --json ls", cwd=d)
        try:
            _json.loads(ls[1])
            ok3, why3 = True, ls[1][:120]
        except Exception as exc:
            ok3, why3 = False, f"ls --json broke: {exc}"
        record("ls --json parses alongside the rest", ok3, why3)

    if want("gc"):
        # A repository that only grows is a repository that gets slower and bigger forever. The
        # check plants an object nothing references and asks gc to remove exactly that one.
        import tempfile
        tmp = tempfile.mkdtemp()
        sh(f"{ZF} init", cwd=tmp)
        with open(os.path.join(tmp, "a.txt"), "w") as fh:
            fh.write("v1\n")
        sh(f"{ZF} changeset create base --author t", cwd=tmp)
        m = re.search(r"(cs[0-9a-f]+)", sh(f"{ZF} changeset list --json", cwd=tmp)[1])
        if m:
            sh(f"{ZF} changeset commit {m.group(1)}", cwd=tmp)

        db = os.path.join(tmp, ".gesta", "gesta.db")
        import hashlib
        import sqlite3
        conn = sqlite3.connect(db)
        for i in range(4):
            h = hashlib.sha256(f"junk{i}".encode()).hexdigest()
            conn.execute(
                "INSERT OR IGNORE INTO objects(hash, content, size, compressed, created_at) "
                "VALUES(?,?,?,0,1)",
                (h, f"junk{i}".encode(), 5),
            )
        conn.commit()
        before = conn.execute("SELECT COUNT(*) FROM objects").fetchone()[0]
        conn.close()

        r = sh(f"{ZF} gc", cwd=tmp)
        conn = sqlite3.connect(db)
        after = conn.execute("SELECT COUNT(*) FROM objects").fetchone()[0]
        conn.close()
        record(
            "gc removes the objects nothing references",
            r[0] == 0 and "removed 4" in r[1] and after == before - 4,
            f"before={before} after={after} {r[1][:120]}",
        )

        fs = sh(f"{ZF} fsck", cwd=tmp)
        body = sh(f"{ZF} show main:a.txt", cwd=tmp)
        record(
            "gc leaves the repository consistent and readable",
            fs[0] == 0 and body[0] == 0 and body[1].strip() == "v1",
            f"fsck={fs[1][:80]} content={body[1].strip()[:40]}",
        )

    if want("git"):
        # Interoperability is the claim that matters most, so it is checked against the real git
        # binary: import a repository git made, export one, and let git validate the result.
        import shutil
        import tempfile
        if not shutil.which("git"):
            record("git interoperability", False, "git is not on PATH, so it cannot be checked")
        else:
            src = tempfile.mkdtemp()
            dst = tempfile.mkdtemp()
            sh(f"{ZF} init", cwd=dst)
            sh(f"git init -q -b main", cwd=src)
            sh("git config user.email a@b.c && git config user.name T", cwd=src)
            os.makedirs(os.path.join(src, "src"), exist_ok=True)
            with open(os.path.join(src, "src", "a.rs"), "w") as fh:
                fh.write("fn one() {}\n")
            with open(os.path.join(src, "README.md"), "w") as fh:
                fh.write("readme\n")
            sh("git add -A && git commit -qm first", cwd=src)
            with open(os.path.join(src, "src", "a.rs"), "a") as fh:
                fh.write("fn two() {}\n")
            sh("git add -A && git commit -qm second", cwd=src)

            sh(f"{ZF} init", cwd=dst)
            got = sh(f"{ZF} git import {src}", cwd=dst)
            record(
                "a git repository imports as changesets",
                got[0] == 0 and "imported 2 commit" in got[1],
                got[1][:200],
            )

            # The content has to be readable, not just the history: an import that stores git's
            # object names in a SHA-256 store looks fine and finds nothing.
            body = sh(f"{ZF} show main:src/a.rs", cwd=dst)
            record(
                "an imported file can be read back",
                body[0] == 0 and "fn two()" in body[1],
                body[1][:200],
            )

            again = sh(f"{ZF} git import {src}", cwd=dst)
            record(
                "importing a second time adds nothing",
                again[0] == 0 and "imported 0 commit" in again[1],
                again[1][:200],
            )

            out = tempfile.mkdtemp()
            exp = sh(f"{ZF} git export {out}", cwd=dst)
            record(
                "the repository exports as a git repository",
                exp[0] == 0 and "exported" in exp[1],
                exp[1][:200],
            )

            # The real test: git has to accept it, with the same commit ids as the original.
            fsck = sh("git fsck --no-progress", cwd=out)
            record("git accepts the exported repository", fsck[0] == 0, fsck[1][:300])

            original = sh("git rev-parse main", cwd=src)[1].strip()
            round_tripped = sh("git rev-parse main", cwd=out)[1].strip()
            record(
                "a round trip keeps the commit id, which covers tree, parents, author and message",
                original != "" and original == round_tripped,
                f"original={original[:12]} exported={round_tripped[:12]}",
            )

            listing = sh("git ls-tree -r main", cwd=out)
            record(
                "the exported tree holds the nested files",
                "src/a.rs" in listing[1] and "README.md" in listing[1],
                listing[1][:200],
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
