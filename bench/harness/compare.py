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
        # The path comes from the shape rather than being written here. A fixed name made this job
        # fail on any shape that does not have that file, and a job both tools fail is not a
        # comparison, it is two errors.
        "gesta": ["{G} show {REV}:{FILE}"],
        "git": ["git show HEAD:{FILE}"],
    },
    {
        "name": "search",
        "ask": "where does this word appear",
        # A word every shape contains. Pinning this to one shape's content made the job fail on the
        # others and the ratio was then computed over a job one side never answered.
        "gesta": ["{G} grep {WORD}"],
        "git": ["git grep {WORD}"],
    },
]


# The three states both tools walk, in order. Each entry is the message its commit carries and the
# files that commit writes. Sharing the list is what makes "what did the last change do" the same
# question in both tools.
# Shapes the same six jobs are asked on. One repository shape is one sample, and a cost ratio
# measured on one sample is a statement about that repository rather than about the tool: the same
# mistake the split advice in any evaluation text warns about, where a row-level split claims
# generalization it cannot support. The shapes differ in what they stress.
SHAPES = {
    # The original: a small tree, a few edits, one new file.
    "small": [
        ("first", {"notes.txt": "alpha\nbeta\n"}),
        ("second", {"notes.txt": "first\nalpha\nbeta\ngamma\n"}),
        ("third", {"other.txt": "alpha again\n"}),
    ],
    # Deep paths and many files, where a tree walk and a path listing cost more.
    "wide": [
        ("first", {f"d{i}//f{i}.txt": f"line {i}\n" for i in range(40)}),
        ("second", {"d39//f39.txt": "line 39 changed\n"}),
        ("third", {"d0//f0.txt": "line 0 changed\n"}),
    ],
    # Deletions, which a summary has to report and a name list has to omit.
    "deleting": [
        ("first", {"a.txt": "first\n", "b.txt": "two\n", "c.txt": "three\n"}),
        ("second", {"b.txt": "two changed\n"}),
        ("third", {}),
    ],
}

# The files a shape's deletions remove, keyed by the step that removes them.
DELETIONS = {"deleting": {2: ["c.txt"]}}

# The file a per-shape job reads, and the word it searches for. A job written with one shape's
# names fails on the others, and a job both tools fail is two errors rather than a comparison.
SHAPE_FACTS = {
    "small": {"file": "notes.txt", "word": "alpha"},
    "wide": {"file": "d0/f0.txt", "word": "line"},
    "deleting": {"file": "a.txt", "word": "first"},
}

STEPS = SHAPES["small"]


def apply_step(root: str, writes: dict[str, str]) -> None:
    """Write one state to disk."""
    for name, body in writes.items():
        target = os.path.join(root, name.replace("//", "/"))
        os.makedirs(os.path.dirname(target) or root, exist_ok=True)
        with open(target, "w") as fh:
            fh.write(body)


def apply_deletions(root: str, names: list[str]) -> None:
    """Remove the files a step deletes, so both tools see the same tree."""
    for name in names:
        p = os.path.join(root, name)
        if os.path.exists(p):
            os.remove(p)


def setup_git(root: str) -> None:
    """Initialise git, so both tools answer about the same files."""
    os.makedirs(root, exist_ok=True)
    subprocess.run(
        "git init -q . && git config user.email t@t && git config user.name t",
        shell=True, cwd=root, capture_output=True,
    )


def measure_shape(gesta: str, shape: str):
    """Run every job on one repository shape. Returns (ledgers, rows) or None when setup fails."""
    steps = SHAPES[shape]
    deletes = DELETIONS.get(shape, {})
    root = tempfile.mkdtemp(prefix=f"compare-{shape}-")
    try:
        # gesta is initialised before the tree exists, so its first changeset sees only the first
        # state. Initialising after the files are written makes that changeset hold every later file
        # too, and every history question then compares two histories with different shapes.
        g = run(f"{gesta} init", root)
        if g.code != 0:
            print(f"gesta init failed: {g.err[:200]}")
            return None
        setup_git(root)

        import json as _js

        def changeset(message: str) -> str:
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

        # One changeset per commit, walking the same states git walks. Only the files a step changes
        # are written: rewriting a file with its existing content leaves the tree unchanged, so the
        # changeset records nothing and the step vanishes from the history it was meant to build.
        for i, (message, writes) in enumerate(steps):
            apply_step(root, writes)
            for name in deletes.get(i, []):
                apply_deletions(root, [name])
            subprocess.run(
                f"git add -A && git commit -qm {message} --allow-empty",
                shell=True, cwd=root, capture_output=True,
            )
            if not changeset(message):
                return None

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
            print(f"gesta has no tree after setup on {shape}: {check.err[:200]}")
            return None

        facts = SHAPE_FACTS[shape]
        ledgers = {"gesta": eff.Ledger(), "git": eff.Ledger()}
        rows = []
        for job in JOBS:
            for tool in ("gesta", "git"):
                key = tool if parent else tool + "_first"
                commands = job.get(key) or job[tool]
                runs = [
                    run(
                        cmd.replace("{G}", gesta)
                        .replace("{PARENT}", parent)
                        .replace("{REV}", "main")
                        .replace("{FILE}", facts["file"])
                        .replace("{WORD}", facts["word"]),
                        root,
                    )
                    for cmd in commands
                ]
                ok = all(r.code == 0 for r in runs) and any(r.bytes_read for r in runs)
                ledgers[tool].add(price(job["name"], runs, ok))
            rows.append((job["name"], job["ask"]))
        return ledgers, rows
    finally:
        shutil.rmtree(root, ignore_errors=True)


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

    # Every shape, then the pooled result. One shape is one sample, and a ratio measured on one
    # sample is a statement about that repository rather than about the tool, which is the same
    # mistake as claiming generalization from a split that did not respect the unit of deployment.
    pooled = {"gesta": eff.Ledger(), "git": eff.Ledger()}
    per_shape = {}
    counted = 0
    for shape in SHAPES:
        got = measure_shape(gesta, shape)
        if got is None:
            print(f"shape {shape}: setup failed, so it is not counted")
            continue
        ledgers, rows = got
        counted += 1
        a1 = ledgers["gesta"].summary()
        b1 = ledgers["git"].summary()
        c1 = eff.compare(a1, b1, "gesta", "git")
        per_shape[shape] = (a1, b1, c1)
        # The ledger's own prices are carried over, not a row rebuilt from `by_task`. That row
        # reports combined bytes and no attempt count, so rebuilding a Price from it silently
        # changed what the pooled number means: a job run on three shapes counted once.
        for price_row in ledgers["gesta"].prices:
            pooled["gesta"].add(price_row)
        for price_row in ledgers["git"].prices:
            pooled["git"].add(price_row)

    if not per_shape:
        print("no repository shape produced a result, so there is nothing to compare")
        return 2

    # One row per shape. A pooled figure that hides one shape pointing the other way is the finding,
    # and it only shows up when the shapes are reported before the total.
    print(f"{len(JOBS)} job(s) per shape, {counted} shape(s)\n")
    print(f"{'shape':14} {'gesta/pass':>11} {'git/pass':>10} {'ratio':>7}  quality")
    for shape, (sa, sb, sc) in per_shape.items():
        print(
            f"{shape:14} {sa['cost_per_pass_tokens']:>11} {sb['cost_per_pass_tokens']:>10} "
            f"{sc['cost_ratio']:>6.2f}x  {sc['quality_gap_points']:+.1f}"
        )
    print()

    a = pooled["gesta"].summary()
    b = pooled["git"].summary()
    cmp = eff.compare(a, b, "gesta", "git")

    print(f"{'job':14} {'gesta':>10} {'git':>10}   ask")
    gs = {r["task"]: r for r in pooled["gesta"].by_task()}
    gis = {r["task"]: r for r in pooled["git"].by_task()}
    for job in JOBS:
        name = job["name"]
        print(f"{name:14} {gs[name]['bytes']:>10} {gis[name]['bytes']:>10}   {job['ask']}")

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

    if a["passed"] == b["passed"] == len(JOBS) * counted:
        print(
            "\nboth tools answered every job on every shape, so the difference is entirely cost: "
            f"{a['cost_per_pass_tokens']} estimated tokens against {b['cost_per_pass_tokens']}"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
