"""Tests for the harness itself.

A harness is a measuring instrument, so the instrument needs checking before its readings mean
anything. These are the three properties that decide whether a number it produces can be trusted.

Run with: python3 -m pytest bench/harness/test_harness.py -q
Or without pytest: python3 bench/harness/test_harness.py
"""

from __future__ import annotations

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import run as harness
import tasks


def test_a_secret_shaped_variable_is_dropped_even_when_named_for_function():
    """The allowlist is about function and the scrub is about risk; they are separate checks.

    An on-list variable whose name says "key" is still a credential, and adding a functional
    variable must never quietly add one of those.
    """
    saved = dict(os.environ)
    try:
        os.environ["AWS_ACCESS_KEY_ID"] = "AKIA-not-a-real-key"
        os.environ["GESTA_S3_ENDPOINT"] = "https://example.invalid"
        os.environ["AWS_SECRET_ACCESS_KEY"] = "secret"
        os.environ["PATH"] = "/usr/bin:/bin"
        env = harness.scrub_env()
        assert "AWS_SECRET_ACCESS_KEY" not in env, "an off-list credential reached the task"
        assert env.get("GESTA_S3_ENDPOINT") == "https://example.invalid", (
            "a functional variable the tasks need was dropped"
        )
    finally:
        os.environ.clear()
        os.environ.update(saved)


def test_the_explicit_list_does_not_carry_a_secret_name():
    """Every explicitly kept variable is checked, because the list grows by hand."""
    for name in harness.KEEP_ENV_EXPLICIT:
        assert not harness.SECRET_SHAPED.search(name), (
            f"{name} is kept explicitly and its name says it is a credential"
        )


def test_a_task_id_is_unique():
    """Two tasks with one id make a run count the same work twice."""
    seen = set()
    for t in tasks.TASKS:
        assert t.name not in seen, f"duplicate task id: {t.name}"
        seen.add(t.name)


def test_every_task_has_a_reference_answer_and_a_check():
    """A task without either measures nothing or cannot be passed."""
    for t in tasks.TASKS:
        assert t.setup, f"{t.name} has no setup, so it starts from an unknown state"
        assert t.solve, f"{t.name} has no reference answer, so its checks were never passed"
        assert t.checks, f"{t.name} has no checks, so it cannot fail"


def test_an_unknown_task_name_is_refused():
    """A subset run that matches nothing must not report success for an empty set."""
    rc = harness.main(["run.py", "--task", "no-such-task", "--gesta", sys.executable])
    assert rc == 2, f"an unknown task name returned {rc}, which reads as success"


def test_the_timeout_is_its_own_outcome():
    """A run stopped by the clock must not be recorded as a wrong answer.

    A timeout says nothing about the work, and recording it as a failure blames the tool for the
    budget. This is the same fault as a runner that kept an attempt because output existed.
    """
    step = harness.sh("sleep 5", cwd="/tmp", env=harness.scrub_env(), budget=1)
    assert step.stop_reason == "timeout", f"expected a timeout, got {step.stop_reason!r}"
    assert step.exit_code is None, "a timed-out command has no exit code to report"


def test_a_command_that_ran_reports_its_own_exit_code():
    """The counterpart: a command that finished must carry its status, not a guess."""
    step = harness.sh("exit 7", cwd="/tmp", env=harness.scrub_env(), budget=10)
    assert step.exit_code == 7, f"expected 7, got {step.exit_code}"
    assert step.stop_reason == "", "a finished command was marked as stopped"


def test_an_incomplete_run_does_not_report_success():
    """A summary with an unfinished attempt must not read as a complete run."""
    run = harness.Run(
        run_id="t", gesta="gesta", started="now", k=1, mode="sequential", seed=0,
        attempts=[
            harness.Attempt(task="a", tries=1, outcome="pass"),
            harness.Attempt(task="b", tries=1, outcome="not-run"),
        ],
    )
    s = run.summary()
    assert s["complete"] is False, "a run with an unfinished attempt claimed to be complete"
    assert s["unfinished"] == 1


if __name__ == "__main__":
    failures = 0
    for name, fn in sorted(globals().items()):
        if name.startswith("test_") and callable(fn):
            try:
                fn()
                print(f"  [PASS] {name}")
            except AssertionError as exc:
                failures += 1
                print(f"  [FAIL] {name}: {exc}")
    print()
    if failures:
        print(f"{failures} harness test(s) failed")
        sys.exit(1)
    print("every harness test passes")
