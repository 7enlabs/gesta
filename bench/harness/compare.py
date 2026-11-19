"""Compare gesta against git on the same jobs, in the same units.

The published harness benchmark held the model fixed and found cost moving 17.5x while quality
stayed within about 17 points. The lesson for a version-control tool is not that one harness is
better: it is that a tool has to be priced the way its consumer pays. An agent pays for what it
reads and for how many round trips an answer takes.

So every job here is written twice, once for each tool, and both are priced by the same ledger.
Nothing calls a model: the measurement is the tool's own output, which is the part a model would
read.

Run: python3 bench/harness/compare.py --gesta /path/to/gesta
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import efficiency as eff


class Run:
    """One command as a consumer sees it: bytes read, and whether it worked."""

    def __init__(self, command: str, code: int, out: str, err: str, seconds: float):
        self.command = command
        self.code = code
        self.out = out
        self.err = err
        self.seconds = seconds

    @property
    def bytes_read(self) -> int:
        return len(self.out.encode("utf-8", "replace"))

    @property
    def bytes_written(self) -> int:
        return len(self.err.encode("utf-8", "replace"))


def run(command: str, cwd: str, budget: int = 120) -> Run:
    import time

    started = time.monotonic()
    try:
        p = subprocess.run(
            command, shell=True, cwd=cwd, capture_output=True, text=True, timeout=budget
        )
        return Run(command, p.returncode, p.stdout or "", p.stderr or "", time.monotonic() - started)
    except subprocess.TimeoutExpired:
        return Run(command, None, "", "timed out", time.monotonic() - started)


def price(name: str, runs: list[Run], ok: bool) -> eff.Price:
    """Price a job from the commands a consumer would run for it."""
    step = type(
        "Step",
        (),
        {
            "phase": "solve",
            "stdout_bytes": sum(r.bytes_read for r in runs),
            "stderr_bytes": sum(r.bytes_written for r in runs),
            "seconds": sum(r.seconds for r in runs),
            "command": "\n".join(r.command for r in runs),
        },
    )()
    return eff.price_attempt(name, "pass" if ok else "fail", [step])


# Each job answers the same question in both tools. The question is the unit of comparison, because
# that is what a caller has: a thing it wants to know, not a command it wants to run.
JOBS = [
    {
        "name": "state",
        "ask": "what is the state of this tree",
        "gesta": ["{G} status"],
        "git": ["git status"],
    },
    {
        "name": "history",
        "ask": "what changed recently",
        # Both sides use the one-line form. Comparing gesta's full record with git's one-line form
        # measures a format choice rather than a cost, and the finding would be about the wrong
        # thing.
        "gesta": ["{G} log --limit 5 --oneline"],
        "git": ["git log --oneline -5"],
    },
    {
        "name": "diff",
        "ask": "what did the last change do",
        # The working tree is clean, so an unstaged diff answers nothing in either tool. The
        # question is about the last change, and gesta answers it without being handed an id:
        # `review` walks the stack and reports what each step did.
        # A diffstat against a diffstat. Comparing gesta's full JSON review with git's summary
        # measures how much detail each prints rather than what an answer costs, and the number
        # would be about the format instead of the tool.
        # The closest thing each tool has to a summary: names on both sides, so the comparison is
        # about the cost of an answer rather than the amount of detail a format prints.
        #
        # Both tools take the same question without being handed a parent: gesta resolves the
        # parent of `--to` itself, and git's first-commit form falls back for the same reason.
        "gesta": ["{G} diff --name-only --to main"],
        "git": ["git diff --name-only HEAD~1 HEAD"],
        "git_first": ["git show --name-only --pretty=format: HEAD"],
    },
    {
        "name": "list-files",
        "ask": "what files exist at the head",
        "gesta": ["{G} ls"],
        "git": ["git ls-tree -r --name-only HEAD"],
    },
    {
        "name": "read-one-file",
        "ask": "show one file as committed",
        # gesta names the branch, not a Git synonym for it. `HEAD` is not its vocabulary, and a job
        # written in one tool's words measures the vocabulary rather than the cost.
        "gesta": ["{G} show main:notes.txt"],
        "git": ["git show HEAD:notes.txt"],
    },
    {
        "name": "search",
        "ask": "where does this word appear",
        "gesta": ["{G} grep alpha"],
        "git": ["git grep alpha"],
    },
]


# The three states both tools walk, in order. Each entry is the message its commit carries and the
# files that commit writes. Sharing the list is what makes "what did the last change do" the same
# question in both tools.
STEPS = [
    ("first", {"notes.txt": "alpha\nbeta\n"}),
    ("second", {"notes.txt": "alpha\nbeta\ngamma\n"}),
    ("third", {"other.txt": "alpha again\n"}),
]


def apply_step(root: str, writes: dict[str, str]) -> None:
    """Write one state to disk."""
    for name, body in writes.items():
        with open(os.path.join(root, name), "w") as fh:
            fh.write(body)


def setup_git(root: str) -> None:
    """Initialise git, so both tools answer about the same files."""
    os.makedirs(root, exist_ok=True)
    subprocess.run(
        "git init -q . && git config user.email t@t && git config user.name t",
        shell=True, cwd=root, capture_output=True,
    )


def main(argv: list[str]) -> int:
    gesta = os.environ.get("GESTA_BIN") or "gesta"
    if "--gesta" in argv:
        gesta = argv[argv.index("--gesta") + 1]
    if not shutil.which("git"):
        print("git is not on PATH, so there is nothing to compare against")
        return 2
    if not os.path.exists(gesta) and not shutil.which(gesta):
        print(f"no gesta binary at {gesta}. Pass --gesta PATH or set GESTA_BIN")
        return 2

    root = tempfile.mkdtemp(prefix="harness-compare-")
    try:
        # gesta is initialised before the tree exists, so its first changeset sees only the first
        # state. Initialising after the files are written makes that changeset hold every later file
        # too, and every history question then compares two histories with different shapes.
        g = run(f"{gesta} init", root)
        if g.code != 0:
            print(f"gesta init failed: {g.err[:200]}")
            return 2
        setup_git(root)

        # The gesta repository has to hold the same tree git does, or the comparison is between a
        # populated tool and an empty one. A setup failure here is fatal and says so, because an
        # empty repository makes every gesta job fail for a reason that has nothing to do with
        # gesta.
        import json as _js

        def changeset(message: str) -> str:
            """Record the current tree as one changeset, and return its id."""
            r = run(f'{gesta} changeset create "{message}" --author cmp --json', root)
            try:
                cs = _js.loads(r.out).get("changeset", "")
            except Exception:
                print(f"could not read the changeset id gesta returned: {r.out[:160]}")
                return ""
            if not cs:
                print(f"gesta created no changeset: {r.out[:160]} {r.err[:160]}")
                return ""
            k = run(f"{gesta} changeset commit {cs}", root)
            if k.code != 0:
                print(f"gesta could not commit the tree: {k.err[:200]}")
                return ""
            return cs

        # One changeset per commit, walking the same three states git walked, so "what did the last
        # change do" asks the same question in both tools. A single changeset holding the whole tree
        # makes gesta's last change the entire repository, and the comparison then measures the
        # shape of the history rather than the cost of an answer.
        #
        # Only the files a step actually changes are written. Rewriting a file with its existing
        # content leaves the tree unchanged, so the changeset records nothing and the step vanishes
        # from the history it was meant to build.
        for message, writes in STEPS:
            apply_step(root, writes)
            # git commits the same state, so the two histories stay in step. The setup cost of
            # measuring is charged to neither side.
            subprocess.run(
                f"git add -A && git commit -qm {message}", shell=True, cwd=root, capture_output=True
            )
            if not changeset(message):
                return 2

        # The parent of the head, so a "last change" job has two revisions to compare. gesta names
        # revisions by changeset id rather than by HEAD~1, and inventing a Git spelling for it would
        # measure the vocabulary.
        parent = ""
        log = run(f"{gesta} log --json", root)
        try:
            entries = _js.loads(log.out)
            if isinstance(entries, dict):
                entries = entries.get("changesets", [])
            if entries:
                parent = entries[0].get("parent") or ""
        except Exception:
            parent = ""

        check = run(f"{gesta} ls", root)
        if check.code != 0:
            print(f"gesta has no tree after setup: {check.err[:200]}")
            return 2

        ledgers = {"gesta": eff.Ledger(), "git": eff.Ledger()}
        rows = []
        for job in JOBS:
            for tool in ("gesta", "git"):
                # A job whose first form needs a parent falls back to its first-commit form when the
                # repository has only one changeset. Both tools take the same fallback, so neither
                # is charged for a shape the other did not have to use.
                key = tool if parent else tool + "_first"
                commands = job.get(key) or job[tool]
                # The binary is named by the caller, so a job is a command line rather than an
                # assumption about PATH. A tool measured through a missing name exits 127 and every
                # job fails for a reason that has nothing to do with the tool.
                runs = [
                    run(cmd.replace("{G}", gesta).replace("{PARENT}", parent), root)
                    for cmd in commands
                ]
                # A job counts as answered when every command in it succeeded and said something.
                ok = all(r.code == 0 for r in runs) and any(r.bytes_read for r in runs)
                ledgers[tool].add(price(job["name"], runs, ok))
            rows.append((job["name"], job["ask"]))

        a = ledgers["gesta"].summary()
        b = ledgers["git"].summary()
        cmp = eff.compare(a, b, "gesta", "git")

        print(f"{len(JOBS)} job(s), both tools answering the same questions\n")
        print(f"{'job':14} {'gesta':>10} {'git':>10}   ask")
        gs = {r["task"]: r for r in ledgers["gesta"].by_task()}
        gis = {r["task"]: r for r in ledgers["git"].by_task()}
        for name, ask in rows:
            print(
                f"{name:14} {gs[name]['bytes']:>10} {gis[name]['bytes']:>10}   {ask}"
            )

        print()
        print("gesta")
        for line in eff.fmt(a).splitlines():
            print(line)
        print("\ngit")
        for line in eff.fmt(b).splitlines():
            print(line)

        print()
        if "cost_ratio" in cmp:
            print(
                f"cost ratio      {cmp['cost_ratio']}x  (gesta cost per pass over git's; "
                f"below 1.0 means gesta answers with fewer bytes)"
            )
        if "turns_ratio" in cmp:
            print(f"turn ratio      {cmp['turns_ratio']}x")
        print(f"quality gap     {cmp['quality_gap_points']:+.1f} points")

        # The figure this whole exercise exists to produce: what a caller pays to get one answer.
        if a["passed"] == b["passed"] == len(JOBS):
            print(
                "\nboth tools answered every job, so the difference is entirely cost: "
                f"{a['cost_per_pass_tokens']} estimated tokens against {b['cost_per_pass_tokens']}"
            )
        return 0
    finally:
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main(sys.argv))
