"""Criteria about how the tool behaved, separate from whether the job got done.

A task can succeed while the tool behaved badly. A command can print the right file and also write a
warning into the machine-readable stream, or report success while having silently skipped the work,
or leave the repository in a state the next command trips over. Scoring only the deliverable records
none of that.

The benchmark literature is blunt about the size of the gap: in one harness evaluation, 31.1% of runs
reached task success while failing at least one harness criterion. A harness that scores only the
deliverable therefore reports a number that hides a third of what it could have said.

These criteria read the same recorded steps the task checks read, so they cost one pass over
evidence that already exists. Each names the property it defends and the failure it caught.
"""

from __future__ import annotations

import re

from stats import pass_at_k, pass_pow_k


class Criterion:
    """One property of good behaviour, with the evidence that decides it."""

    def __init__(self, key: str, obligation: str, judge):
        self.key = key
        self.obligation = obligation
        self.judge = judge

    def evaluate(self, attempt) -> tuple[bool, str]:
        """True when the tool honoured this obligation on this run."""
        try:
            return self.judge(attempt)
        except Exception as exc:  # a judge that crashes must not pass by accident
            return (False, f"criterion raised {type(exc).__name__}: {exc}")


def _all_phases(attempt):
    return list(attempt.steps)


def _stderr_is_quiet_on_success(attempt) -> tuple[bool, str]:
    """A command that succeeded should not have written a warning a caller must read.

    Silence on the error stream is what lets a script treat a zero exit as a clean result.
    """
    noisy = []
    for step in _all_phases(attempt):
        if step.exit_code == 0 and (step.stderr_tail or "").strip():
            noisy.append(step.stderr_tail.strip().splitlines()[0][:120])
    if noisy:
        return (False, f"{len(noisy)} successful command(s) wrote to stderr: {noisy[0]}")
    return (True, "")


def _failure_says_why(attempt) -> tuple[bool, str]:
    """Every refusal has to explain itself. A zero-exit binary that prints nothing tells a caller
    nothing, and an agent reading an empty error cannot recover."""
    for step in _all_phases(attempt):
        if step.exit_code not in (0, None):
            text = ((step.stderr_tail or "") + (step.stdout_tail or "")).strip()
            if not text:
                return (False, f"a command failed with no message: {step.command[:80]}")
    return (True, "")


def _no_internal_state_leaks(attempt) -> tuple[bool, str]:
    """A message meant for a person must not expose a database path, a raw SQL error, or a panic.

    Those are the strings that turn a bug report into a security note.
    """
    pattern = re.compile(
        r"(panicked at|no such table:|UNIQUE constraint failed|SQLITE_|RUST_BACKTRACE|"
        r"called `Option::unwrap`|\.gesta/gesta\.db)",
        re.IGNORECASE,
    )
    for step in _all_phases(attempt):
        text = (step.stdout_tail or "") + (step.stderr_tail or "")
        m = pattern.search(text)
        if m:
            return (False, f"internal detail reached the reader: {m.group(0)}")
    return (True, "")


def _no_panic_exit(attempt) -> tuple[bool, str]:
    """A Rust binary exits 101 on a panic, and a caller that treats 101 as "just a failure" has
    lost the distinction between a refusal and a crash."""
    for step in _all_phases(attempt):
        if step.exit_code == 101:
            return (False, f"a command panicked: {step.command[:80]}")
    return (True, "")


def _json_reaches_stdout(command: str) -> bool:
    """Whether the stream this script produced belongs to a command that asked for JSON.

    Three shapes are excluded, each for the same reason: the JSON was consumed by something else.
    A pipe takes it, a redirect to a file takes it, and a command substitution takes it.
    """
    lines = [l for l in command.splitlines() if l.strip() and not l.strip().startswith("#")]
    # Only a single-line command has a stdout that belongs to it alone. In a script of several
    # lines the stream is the concatenation of every line's output, so a json document on line one
    # and a sentence on line two produce a body no parser can read while the json command itself
    # was correct. Judging that would accuse an innocent run, which is the failure mode that makes
    # a criterion useless rather than strict.
    if len(lines) != 1:
        return False
    stripped = lines[0].strip()
    if "--json" not in stripped:
        return False
    before_pipe = stripped.split("|", 1)[0]
    if "|" in stripped and "--json" in before_pipe:
        return False
    if ">" in stripped.split("--json", 1)[1].split("|")[0]:
        return False
    if "$(" in stripped or "`" in stripped:
        return False
    return True


def _json_is_one_document(attempt) -> tuple[bool, str]:
    """Every `--json` invocation must print exactly one parseable document on stdout.

    A caller that asked for JSON and got JSON plus a line of prose parses nothing and branches on
    nothing, and the run looks successful. This is checked on real output because the failure only
    appears when the two streams are mixed, which no unit test of the formatter would catch.
    """
    import json as _json

    for step in _all_phases(attempt):
        if "--json" not in step.command or step.exit_code not in (0, 1, 2, 3, 4, 5):
            continue
        # A script whose stdout is the last command in it says nothing about the command that
        # carried `--json`: `C=$(gesta ... --json | cs_id)` pipes that output away, and the
        # document never reaches stdout. Scoring the script's own output would be scoring a
        # different command, which is how a criterion ends up accusing an innocent run.
        body = (step.stdout_full or "").strip()
        if not _json_reaches_stdout(step.command):
            continue
        if not body:
            continue
        try:
            _json.loads(body)
        except Exception as exc:
            return (
                False,
                f"`--json` printed something a parser cannot read ({exc}): {body[:110]}",
            )
    return (True, "")


CRITERIA = [
    Criterion(
        "quiet-stderr",
        "a successful command writes nothing to the error stream",
        _stderr_is_quiet_on_success,
    ),
    Criterion(
        "failure-explains",
        "a refused command says why",
        _failure_says_why,
    ),
    Criterion(
        "no-internal-leak",
        "no database path, SQL error, or panic reaches the reader",
        _no_internal_state_leaks,
    ),
    Criterion(
        "no-panic",
        "no command crashes instead of refusing",
        _no_panic_exit,
    ),
    Criterion(
        "json-one-document",
        "`--json` prints exactly one document a parser can read",
        _json_is_one_document,
    ),
]


def score(attempts: list) -> dict:
    """Score every criterion over a set of attempts, and report the two ways a run can hide.

    `task_success_with_violation` is the count of runs that did the job while breaking a rule about
    how. That number is the reason this module exists: without it, a passing run reads as a clean
    run, and a third of the useful signal is discarded.
    """
    per_criterion = {}
    for c in CRITERIA:
        results = [c.evaluate(a)[0] for a in attempts]
        per_criterion[c.key] = {
            "obligation": c.obligation,
            "pass_at_1": pass_at_k(results),
            "pass_pow_1": pass_pow_k(results),
            "violations": sum(1 for r in results if not r),
            "total": len(results),
        }

    hidden = 0
    for a in attempts:
        task_ok = getattr(a, "outcome", "") == "pass"
        if not task_ok:
            continue
        if any(not c.evaluate(a)[0] for c in CRITERIA):
            hidden += 1

    return {
        "criteria": per_criterion,
        "task_success_with_violation": hidden,
        "attempts": len(attempts),
    }
