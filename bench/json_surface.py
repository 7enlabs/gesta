import json
import os
import re
import shutil
import subprocess
import tempfile

# The JSON surface of tag, diff and blame, checked as a caller reads it: every document parsed,
# every flag honoured.
#
# Written because the tests that existed asserted on functions, and these three bugs lived in the
# seam between a parsed flag and the handler it reached: `--json tag v1.0` created nothing,
# `--json diff --name-only` ignored the flag, and `--json blame --ignore-rev` skipped the list that
# changes which changeset a line belongs to. A unit test on `write_tag` passes while the dispatch
# above it throws the arguments away.
ZF = os.environ.get("ZF_BIN", os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "target", "release", "gesta"))


def run(args, cwd):
    return subprocess.run([ZF] + args, cwd=cwd, capture_output=True, text=True)


def doc(args, cwd):
    r = run(args, cwd)
    try:
        return json.loads(r.stdout), r
    except Exception:
        return None, r


d = tempfile.mkdtemp(prefix="jsonsurface-")
checks = []


def check(name, ok, detail=""):
    checks.append((name, ok, detail))


try:
    run(["init"], d)
    with open(os.path.join(d, "a.txt"), "w") as fh:
        fh.write("line one\nline two\n")
    with open(os.path.join(d, "b.txt"), "w") as fh:
        fh.write("b\n")
    run(["changeset", "create", "first"], d)
    run(["changeset", "commit"], d)

    with open(os.path.join(d, "a.txt"), "w") as fh:
        fh.write("line one\nline TWO changed\n")
    run(["changeset", "create", "second"], d)
    run(["changeset", "commit"], d)

    # Which changeset is first is read from the parent link, not from the order `log` prints. Two
    # commits in the same second sort by a timestamp that cannot tell them apart, so an earlier
    # version of this bench passed when the two runs happened to straddle a second boundary and
    # failed inside the gate, where they do not.
    log, _ = doc(["log", "--json"], d)
    first_rev = second_rev = None
    if isinstance(log, list):
        for row in log:
            if row.get("parent") is None:
                first_rev = row["id"]
            elif row.get("parent") == first_rev:
                second_rev = row["id"]
        if second_rev is None:
            for row in log:
                if row["id"] != first_rev:
                    second_rev = row["id"]
    check("the bench found both changesets", first_rev is not None and second_rev is not None,
          f"first={first_rev} second={second_rev}")

    # --- #188: a tag named on the command line is created, not listed -------------------------
    tag, r = doc(["tag", "v1.0", "--json"], d)
    check("#188 tag --json answers with an object", isinstance(tag, dict), r.stdout[:120])
    check("#188 the object says it created the tag",
          isinstance(tag, dict) and tag.get("action") == "created",
          str(tag)[:160] if tag else r.stdout[:120])
    check("#188 it names the changeset it points at",
          isinstance(tag, dict) and tag.get("changeset") == second_rev,
          f"{tag.get('changeset') if tag else None} vs {second_rev}")
    listed, _ = run(["tag", "--list"], d).stdout, None
    check("#188 the tag is readable afterwards", "v1.0" in listed, listed[:120])

    # A second run must move nothing and say so rather than claiming a create.
    again, _ = doc(["tag", "v1.0", "--json"], d)
    check("#188 re-tagging the same point reports unchanged",
          isinstance(again, dict) and again.get("action") == "unchanged",
          str(again)[:160] if again else "")

    # --- #191: --name-only is honoured ----------------------------------------------------------
    full, _ = doc(["diff", "--changeset", second_rev, "--json"], d)
    check("#191 without the flag a file is an object with its lines",
          isinstance(full, dict) and isinstance((full.get("files") or [None])[0], dict),
          str(full)[:140] if full else "")
    names, _ = doc(["diff", "--changeset", second_rev, "--json", "--name-only"], d)
    check("#191 with the flag a file is just its path",
          isinstance(names, dict) and (names.get("files") or [None])[0] == "a.txt",
          str(names)[:140] if names else "")
    check("#191 the two shapes really differ", full != names, "flag changed nothing")

    # --- #200: --ignore-rev changes the attribution and is reported -----------------------------
    plain, _ = doc(["blame", "a.txt", "--json"], d)
    plain_lines = plain.get("lines") if isinstance(plain, dict) else None
    check("#200 blame answers one entry per line",
          isinstance(plain_lines, list) and len(plain_lines) == 2,
          str(plain)[:140] if plain else "")
    line1_before = plain_lines[0]["changeset"] if plain_lines else None
    check("#200 the first line belongs to the first changeset",
          line1_before == first_rev, f"{line1_before} vs {first_rev}")

    ignored, _ = doc(["blame", "a.txt", "--json", "--ignore-rev", first_rev], d)
    ignored_lines = ignored.get("lines") if isinstance(ignored, dict) else None
    line1_after = ignored_lines[0]["changeset"] if ignored_lines else None
    check("#200 the ignored revision is named in the document",
          isinstance(ignored, dict) and first_rev in (ignored.get("ignored") or []),
          str(ignored.get("ignored") if ignored else None))
    check("#200 the ignored line is attributed to the change before it",
          line1_after == second_rev, f"{line1_after} vs {second_rev}")
    check("#200 the answer really changed", line1_before != line1_after,
          f"{line1_before} == {line1_after}")

    # --- every document carries the schema ------------------------------------------------------
    for label, args in [
        ("tag", ["tag", "v1.0", "--json"]),
        ("diff", ["diff", "--changeset", second_rev, "--json"]),
        ("blame", ["blame", "a.txt", "--json"]),
    ]:
        got, _ = doc(args, d)
        check(f"the {label} document carries the schema",
              isinstance(got, (dict, list)) and (got.get("schema") == "gesta@1"
                                                 if isinstance(got, dict) else True),
              str(got)[:100] if got else "")

    # --- #209: a failure under --json parses, and carries the schema ----------------------------
    bad, _ = doc(["show", "--json", "no-such-file-anywhere"], d)
    check("#209 a failure under --json is still JSON", isinstance(bad, dict), "")
    check("#209 the error document carries the schema",
          isinstance(bad, dict) and bad.get("schema") == "gesta@1", str(bad)[:140] if bad else "")
    check("#209 it names a code and an advice",
          isinstance(bad, dict) and "error" in bad
          and bad["error"].get("code") and bad["error"].get("advice"),
          str(bad)[:160] if bad else "")

    # --- #183: a message carrying JSON-hostile characters survives the round trip ---------------
    hostile = 'back\\slash "quote"\ttab'
    run(["changeset", "create", hostile], d)
    saved, _ = doc(["changeset", "list", "--json"], d)
    check("#183 a changeset message with a backslash and a quote parses", saved is not None, "")
    run(["changeset", "commit"], d)

    for name, ok, detail in checks:
        print(("  [PASS] " if ok else "  [FAIL] ") + name + ("" if ok else f"  ({detail})"))
    print()
    n = sum(1 for _, ok, _ in checks if ok)
    print(f"{n}/{len(checks)} json surface checks pass")
finally:
    shutil.rmtree(d, ignore_errors=True)
