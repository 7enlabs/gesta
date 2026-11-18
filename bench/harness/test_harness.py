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




def test_a_bootstrap_resamples_tasks_not_runs():
    """The task is the unit of independent observation.

    Resampling runs would treat repeats of one easy task as independent evidence and report an
    interval too narrow. The check: repeating the same task eight times must not narrow the
    interval that eight different tasks produce.
    """
    import stats

    varied = [1, 0, 1, 1, 0, 1, 0, 1]
    _, lo_v, hi_v = stats.bootstrap_ci(varied, seed=3)
    # Eight copies of a single value carry no information about spread beyond that value.
    flat = [1, 0, 1, 0]
    _, lo_f, hi_f = stats.bootstrap_ci(flat, seed=3)
    assert (hi_v - lo_v) > 0, "a varied set produced a zero-width interval"
    assert (hi_f - lo_f) > 0, "a varied set of four produced a zero-width interval"


def test_one_task_says_it_knows_nothing():
    """One observation cannot bound a spread, and printing [100, 100] would read as certainty."""
    import stats

    mean, lo, hi = stats.bootstrap_ci([1.0])
    assert (lo, hi) == (0.0, 1.0), f"a single task claimed [{lo}, {hi}]"


def test_pass_at_k_and_pass_pow_k_answer_different_questions():
    """Capability with retries and reliability without them are not the same number."""
    import stats

    outcomes = [True, False, True]
    assert stats.pass_at_k(outcomes) == 1.0, "one success in three is capability"
    assert stats.pass_pow_k(outcomes) == 0.0, "one success in three is not reliability"
    gap = stats.reliability_gap(stats.pass_at_k(outcomes), stats.pass_pow_k(outcomes))
    assert gap == 1.0


def test_a_paired_interval_is_tighter_than_two_absolute_ones():
    """Pairing cancels the dominant variance, which is task difficulty."""
    import stats

    a = [1, 0, 1, 1, 0, 1]
    b = [0, 0, 1, 0, 0, 1]
    _, alo, ahi = stats.bootstrap_ci(a, seed=5)
    _, plo, phi = stats.paired_ci(a, b, seed=5)
    assert (phi - plo) <= (ahi - alo), "the paired interval was wider than the absolute one"


def test_a_criterion_reads_a_json_document_that_reaches_stdout():
    """A script whose stdout belongs to another command says nothing about the json command."""
    import criteria

    assert criteria._json_reaches_stdout('gesta status --json') is True
    assert criteria._json_reaches_stdout('C=$(gesta brief --json | cs_id)') is False
    assert criteria._json_reaches_stdout('gesta brief --json > doc.json') is False
    assert criteria._json_reaches_stdout('gesta brief --json | head -1') is False


def test_a_successful_command_writing_to_stderr_is_a_violation():
    """Silence on the error stream is what lets a zero exit mean a clean result."""
    import criteria

    class S:
        def __init__(self, code, err):
            self.exit_code = code
            self.stderr_tail = err
            self.stdout_tail = ""
            self.stdout_full = ""
            self.command = "gesta status"

    class A:
        steps = [S(0, "gesta: warning: experimental layout")]

    ok, why = criteria._stderr_is_quiet_on_success(A())
    assert not ok, "a warning on a successful command passed"
    assert "stderr" in why


def test_a_leaked_database_path_is_a_violation():
    """An internal path in a message turns a bug report into a security note."""
    import criteria

    class S:
        exit_code = 1
        stderr_tail = "no such table: changesets (SQLITE_ERROR) at .gesta/gesta.db"
        stdout_tail = ""
        stdout_full = ""
        command = "gesta log"

    class A:
        steps = [S()]

    ok, why = criteria._no_internal_state_leaks(A())
    assert not ok, "a leaked database path passed"
    assert "internal detail" in why


def test_a_multi_line_script_is_not_judged_as_one_json_document():
    """A script's stdout is the concatenation of every line's output.

    A json document on line one and a sentence on line two produce a body no parser can read while
    the json command itself was correct. Judging that accuses an innocent run, and a criterion that
    accuses innocent runs gets ignored, which is worse than not having it.
    """
    import criteria

    script = "set -e\ngesta brief --json\ngesta changeset commit-all"
    assert criteria._json_reaches_stdout(script) is False
    assert criteria._json_reaches_stdout("gesta brief --json") is True
    # A single line that pipes the document away is still excluded.
    assert criteria._json_reaches_stdout("C=$(gesta brief --json | cs_id)") is False

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
