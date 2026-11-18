"""Deliberately imperfect solvers, to prove the harness can see a difference.

A deterministic reference answer passes every time, so `pass@k` and `pass^k` both read 1.0 and the
gap between them is zero by construction. That is not evidence of quality: it is evidence that the
measurement has no variance to measure. A thermometer reading 20 degrees in a freezer and 20 in an
oven is a broken thermometer, not a stable climate.

Each mutant makes one specific thing worse in a way a person would notice, and the harness has to
catch it. A mutant that survives is a defect in the measurement rather than in the tool, and
finding those before trusting a score is the whole point.

Two kinds, because there are two things to detect.

A **behavioural** mutant changes what the solver does, so the task's checks should fail. If they
pass, the task is not measuring its own goal.

A **criterion** mutant leaves the goal reached and spoils the manner: a warning on a successful
command, a message that leaks a database path. The task's checks still pass and a criterion fails.
That combination is what a single score reports as clean, and the benchmark literature puts it at
roughly a third of runs.
"""

from __future__ import annotations

from dataclasses import dataclass, field


@dataclass
class Mutant:
    """One way to be wrong, and what should notice."""

    name: str
    """Stable id."""

    plants: str
    """The defect, in the words of somebody who would complain about it."""

    applies_to: list[str] = field(default_factory=list)
    """Task ids this mutant is meaningful for. Empty means every task."""

    commands: list[str] = field(default_factory=list)
    """Shell lines appended to the solver's answer, to plant the defect."""

    expect: str = "checks-fail"
    """`checks-fail` or `criteria-fail`.

    The first says the task's own checks must notice, so a survivor means the task does not measure
    its goal. The second says the goal is still reached and a criterion must notice, so a survivor
    means the criterion layer is not reading what it claims to read.
    """

    replaces: list[tuple[str, str]] = field(default_factory=list)
    """Pairs applied to the solver's script before it runs, when a defect needs a change in place."""


MUTANTS: list[Mutant] = [
    Mutant(
        name="wrong-commit-message",
        plants="the changeset is committed under a different message than the one asked for",
        applies_to=["first-commit"],
        replaces=[("add notes", "misc changes")],
        expect="checks-fail",
    ),
    Mutant(
        name="writes-a-warning",
        plants="every command succeeds and one also writes a warning to the error stream",
        commands=["echo 'gesta: warning: repository layout is experimental' >&2"],
        expect="criteria-fail",
    ),
    Mutant(
        name="leaks-a-database-path",
        plants="the reader is shown a database path and a raw SQL error",
        commands=["echo 'no such table: changesets (SQLITE_ERROR) at .gesta/gesta.db' >&2"],
        expect="criteria-fail",
    ),
    Mutant(
        name="breaks-the-json",
        plants="`--json` output is followed by a line of prose, so no parser can read it",
        applies_to=["first-commit", "recover-with-brief"],
        # The prose has to sit inside the same command that asked for JSON. Appending it as a
        # separate line puts it in a different process, where nothing reads it and nothing notices.
        # Appended to the same shell line that asked for JSON, whatever else that line does. A
        # replacement keyed on a pipe only works for a solver that happens to pipe.
        replaces=[
            (
                "--json",
                "--json | tee /dev/stderr | sed 's|}$|}\nnote: written by gesta|' ; true #",
            )
        ],
        expect="checks-fail",
    ),
]


def for_task(task_name: str) -> list[Mutant]:
    """The mutants that carry a verdict for this task.

    A mutant that does not apply is not a pass: counting it as one would inflate the score with
    tests that were never meaningful, which is the fault this file exists to avoid.
    """
    out = []
    for m in MUTANTS:
        if m.applies_to and task_name not in m.applies_to:
            continue
        out.append(m)
    return out
