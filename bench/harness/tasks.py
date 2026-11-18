"""Task definitions for the gesta harness.

A task is a small, checkable piece of version-control work with a stated goal, the commands a
correct answer needs, and the checks that decide whether the goal was reached. Everything here is
data: the runner reads it, the grader runs the checks, and the improver reads the failures.

Four rules shape the fields.

`checks` run against a fresh repository the runner builds, so a task cannot be passed by a state
somebody left behind.

`solve` is the reference answer. A harness whose checks have never been passed measures nothing,
because a check that always fails looks exactly like a check with a bug in it. The runner proves
every check passes along this path before any other solver is scored.

`budget` is seconds. Reaching it is recorded as its own outcome rather than as a failure, since a
run stopped by a clock says nothing about the work.

Ids never change. A rename makes two runs incomparable, and comparing runs is the point.
"""

from __future__ import annotations

from dataclasses import dataclass, field

# A newline as printf understands it: two characters, backslash then n. Written once so the escape
# is not repeated, and read by anyone editing a task as "a newline in the file being written".
NL = "\\n"


def cs(message: str, author: str = "setup") -> str:
    """Create a changeset and keep its id in $C.

    The id comes from a helper the runner writes, because the JSON is printed across several lines
    and a single-line text match does not find it. One place parses output, so a formatting change
    breaks one helper instead of every task.
    """
    return f'C=$(gesta changeset create "{message}" --author {author} --json | cs_id)'


@dataclass
class Task:
    """One job, with the commands that do it and the checks that prove it."""

    name: str
    """Stable id. A rename makes two runs incomparable, so this never changes."""

    goal: str
    """What the agent is asked to do, in the words it will be given."""

    setup: list[str]
    """Shell lines that build the starting repository, run in a fresh directory."""

    checks: list[str]
    """Shell lines that must all exit zero. Each one is a claim about the result."""

    solve: list[str]
    """Commands that accomplish the goal, used as the reference answer."""

    teaches: str
    """Which gesture of the tool this exercises, so a failure names the area."""

    budget: int = 120
    """Seconds before an attempt is stopped. Reported as a timeout, never as a wrong answer."""

    tags: list[str] = field(default_factory=list)


# The task set walks the gestures a person does in a working day, in the order they appear in one.
# It is deliberately small: a harness that measures eight things honestly beats one that measures
# forty by accident, and every task here has a check that can fail.
TASKS: list[Task] = [
    Task(
        name="first-commit",
        goal=(
            "Create a repository here, write notes.txt containing the single line alpha, record "
            "it in a changeset with the message 'add notes', and commit it."
        ),
        setup=["gesta init"],
        checks=[
            "gesta log --json | grep -q 'add notes'",
            "gesta show main:notes.txt | grep -qx alpha",
            "gesta fsck",
        ],
        solve=[
            f"printf 'alpha{NL}' > notes.txt",
            cs("add notes", "solver"),
            "gesta changeset commit $C",
        ],
        teaches="init, changeset create, changeset commit",
        tags=["core"],
    ),
    Task(
        name="branch-and-land",
        goal=(
            "On a branch called tidy, change the second line of notes.txt to beta, commit it with "
            "the message 'tidy notes', then land tidy onto main."
        ),
        setup=[
            "gesta init",
            f"printf 'one{NL}two{NL}' > notes.txt",
            cs("seed"),
            "gesta changeset commit $C",
            "gesta branch create tidy",
            "gesta branch switch tidy",
        ],
        checks=[
            "gesta show main:notes.txt | sed -n 2p | grep -qx beta",
            "gesta fsck",
        ],
        solve=[
            f"printf 'one{NL}beta{NL}' > notes.txt",
            cs("tidy notes", "solver"),
            "gesta changeset commit $C",
            "gesta land tidy --onto main",
        ],
        teaches="branch create, branch switch, land",
        tags=["core", "branch"],
    ),
    Task(
        name="review-before-landing",
        goal=(
            "The branch risky has one changeset that removes a function another file still calls. "
            "Run a review and report the stale reference it finds."
        ),
        setup=[
            "gesta init",
            "mkdir -p src",
            "printf 'export function parseConfig(raw) {%s  return raw;%s}%s' > src/config.js"
            % (NL, NL, NL),
            "printf 'const c = require(\"./config\");%sconst v = c.parseConfig(x);%s' > src/main.js"
            % (NL, NL),
            cs("add config"),
            "gesta changeset commit $C",
            "gesta branch create risky",
            "gesta branch switch risky",
            "printf 'export function readConfig(raw) {%s  return raw;%s}%s' > src/config.js"
            % (NL, NL, NL),
            cs("rename parseConfig"),
            "gesta changeset commit $C",
        ],
        # The check reads a report the solver wrote. Running the review inside the check would
        # perform the goal instead of verifying it, and then the task measures nothing.
        checks=[
            "test -f report.json",
            "grep -q parseConfig report.json",
            "grep -q 'src/main.js' report.json",
        ],
        solve=["gesta review --json > report.json"],
        teaches="review, impact analysis",
        tags=["review"],
    ),
    Task(
        name="undo-and-redo",
        goal=(
            "Commit a changeset that adds second.txt, then undo it so the file is gone from disk, "
            "then redo it so the file is back."
        ),
        setup=["gesta init", f"printf 'one{NL}' > first.txt"],
        checks=["test -f second.txt", "gesta fsck"],
        solve=[
            f"printf 'two{NL}' > second.txt",
            cs("add second", "solver"),
            "gesta changeset commit $C",
            "gesta undo",
            "gesta redo",
        ],
        teaches="undo, redo, materialise",
        tags=["core", "safety"],
    ),
    Task(
        name="stash-a-deletion",
        goal=(
            "The working tree has one added file and one deleted file. Save both with stash, "
            "confirm the tree is clean, then pop the stash and confirm both are back."
        ),
        setup=[
            "gesta init",
            f"printf 'keep{NL}' > keep.txt",
            f"printf 'gone{NL}' > gone.txt",
            cs("seed"),
            "gesta changeset commit $C",
            "rm gone.txt",
            f"printf 'new{NL}' > added.txt",
        ],
        # The stash holds the state the tree was in, and that state has gone.txt deleted. Pop puts
        # the tree back, so the added file returns and the deleted one stays deleted. A check that
        # demanded both files back was wrong about what a stash is.
        # Popping restores the state the tree was in, which is the state the task starts in, so
        # every observation of the files alone is the same before and after. What distinguishes the
        # work is that a stash was made and then dropped.
        checks=[
            "test -f added.txt",
            "test ! -f gone.txt",
            "gesta show main:gone.txt | grep -qx gone",
            # A stash was made and then dropped. The evidence cannot live in the working tree,
            # because saving a stash resets that tree and removes untracked files, so the log goes
            # one directory up where the reset cannot reach it.
            "test \"$(gesta stash list 2>&1 | grep -c '^stash')\" -eq 0",
            "test -s ../stash-log.txt",
            "grep -q 'saved stash' ../stash-log.txt",
            "gesta fsck",
        ],
        solve=[
            "gesta stash save > ../stash-log.txt 2>&1",
            "gesta stash pop",
        ],
        teaches="stash save, stash pop",
        tags=["safety"],
    ),
    Task(
        name="recover-with-brief",
        goal=(
            "A previous session left a pending changeset. Ask one command what state the "
            "repository is in, and finish the work it reports as pending."
        ),
        setup=[
            "gesta init",
            f"printf 'work{NL}' > work.txt",
            'gesta changeset create "half done" --author previous --json > /dev/null',
        ],
        # The pending changeset exists before the work starts, so a check that only asks whether
        # it is in the log passes for free. The goal is that it is no longer pending.
        checks=[
            "! gesta changeset list 2>/dev/null | grep -q 'half done'",
            "gesta log --json | grep -q 'half done'",
            "gesta fsck",
        ],
        # `--json` because the task is about what an agent learns from one call, and an agent reads
        # the document rather than the prose.
        solve=["gesta brief --json", "gesta changeset commit-all"],
        teaches="brief, changeset commit",
        tags=["agent"],
    ),
    Task(
        name="refuse-a-stale-landing",
        goal=(
            "main moved after the branch was read. Try to land it with an expectation that no "
            "longer holds, and confirm the tool refuses instead of overwriting the newer work."
        ),
        setup=[
            "gesta init",
            f"printf 'one{NL}' > a.txt",
            cs("seed"),
            "gesta changeset commit $C",
            "gesta branch create side",
            "gesta branch switch side",
            f"printf 'side{NL}' > b.txt",
            cs("side work"),
            "gesta changeset commit $C",
            "gesta branch switch main",
            f"printf 'moved{NL}' > c.txt",
            cs("main moved"),
            "gesta changeset commit $C",
        ],
        checks=[
            "test -f attempt.txt",
            "grep -qi 'refus\\|moved\\|expect\\|no such' attempt.txt",
            "gesta show main:c.txt | grep -qx moved",
            "test ! -f b.txt",
        ],
        solve=["! gesta land side --onto main --expect deadbeef 2>attempt.txt"],
        teaches="land --expect, refusing a stale landing",
        tags=["safety", "agent"],
    ),
    Task(
        name="binary-survives-a-clone",
        goal=(
            "Commit a 256-byte binary file, then confirm that the object stored for it holds the "
            "same bytes the file has on disk."
        ),
        setup=[
            "gesta init",
            "python3 -c \"open('blob.bin','wb').write(bytes(range(256)))\"",
        ],
        # Hashing the file on disk proves nothing about the store: the file is already there.
        # The object has to survive a round trip, so the check clones the repository and compares
        # the clone's bytes with the original's.
        checks=[
            "md5sum blob.bin | cut -d' ' -f1 > want.txt",
            "gesta remote add origin $PWD/../mirror",
            "gesta remote push origin --branch main",
            "gesta remote clone $PWD/../mirror $PWD/../back",
            "md5sum ../back/blob.bin | cut -d' ' -f1 > got.txt",
            "cmp -s want.txt got.txt",
            "gesta fsck",
        ],
        solve=[
            cs("add blob", "solver"),
            "gesta changeset commit $C",
        ],
        teaches="binary objects",
        tags=["remote"],
    ),
]


def by_name(name: str) -> Task | None:
    """Look a task up by id, so a subset run cannot silently match nothing."""
    for t in TASKS:
        if t.name == name:
            return t
    return None


def names() -> list[str]:
    return [t.name for t in TASKS]
