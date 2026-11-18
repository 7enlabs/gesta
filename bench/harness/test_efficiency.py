"""Tests for the cost model.

A cost figure is only useful if it moves. These tests build two runs that differ in one way and
require that the number separates them, because a cost model that reports the same figure for a
quiet tool and a chatty one measures nothing.

Each test names the confusion it rules out, since the ways to get this wrong are specific: counting
the harness's own work as the tool's, dividing by attempts instead of passes, and reporting a ratio
against zero.
"""

from __future__ import annotations

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import efficiency as eff


class FakeStep:
    def __init__(self, phase, stdout_bytes, stderr_bytes, seconds=0.1, command="gesta status"):
        self.phase = phase
        self.stdout_bytes = stdout_bytes
        self.stderr_bytes = stderr_bytes
        self.seconds = seconds
        self.command = command


def test_setup_and_checks_are_not_charged_to_the_tool():
    """The harness's own work is the cost of measuring, not the cost of the tool.

    A tool that says nothing would otherwise look expensive because the harness scanned a large
    tree to build its starting state.
    """
    quiet = [
        FakeStep("setup", 100_000, 0),
        FakeStep("solve", 20, 0),
        FakeStep("checks", 50_000, 0),
    ]
    p = eff.price_attempt("t", "pass", quiet)
    assert p.bytes_read == 20, f"charged {p.bytes_read} bytes for a 20-byte answer"


def test_a_chatty_tool_costs_more_than_a_quiet_one():
    """The number has to move, or it is not measuring anything."""
    quiet = eff.price_attempt("t", "pass", [FakeStep("solve", 100, 0)])
    chatty = eff.price_attempt(
        "t", "pass", [FakeStep("solve", 100, 0), FakeStep("solve", 4_000, 2_000)]
    )
    assert chatty.estimated_tokens > quiet.estimated_tokens


def test_cost_per_pass_divides_by_passes_not_attempts():
    """A harness that fails half its tasks cheaply has not earned a low cost per pass."""
    ledger = eff.Ledger()
    ledger.add(eff.price_attempt("a", "pass", [FakeStep("solve", 400, 0)]))
    ledger.add(eff.price_attempt("b", "fail", [FakeStep("solve", 40, 0)]))
    ledger.add(eff.price_attempt("c", "fail", [FakeStep("solve", 40, 0)]))
    s = ledger.summary()
    assert s["passed"] == 1
    # Only the passing task's bytes count, so the figure is 400/4 = 100 rather than 480/12 = 40.
    assert s["cost_per_pass_tokens"] == 100, s


def test_no_passes_reports_no_cost_rather_than_zero():
    """Zero would read as free. Having no pass is the absence of a figure, not a figure."""
    ledger = eff.Ledger()
    ledger.add(eff.price_attempt("a", "fail", [FakeStep("solve", 400, 0)]))
    s = ledger.summary()
    assert s["no_passes"] is True
    assert s["cost_per_pass_tokens"] == 0
    text = eff.fmt(s)
    assert "no cost per pass can be computed" in text, text


def test_turns_count_the_commands_the_solver_ran():
    """A turn is a command the solver chose, not a shell that ran it."""
    step = FakeStep("solve", 10, 0)
    step.command = "set -e\ngesta init\ngesta status\n\ngesta log"
    p = eff.price_attempt("t", "pass", [step])
    assert p.turns == 3, f"counted {p.turns} turns for three commands"


def test_setup_lines_are_not_turns():
    """Setup is the harness building a starting state, which no caller chose to watch."""
    setup = FakeStep("setup", 100, 0)
    setup.command = "gesta init\nprintf 'x' > a.txt\ngesta changeset create m\n"
    solve = FakeStep("solve", 10, 0)
    solve.command = "gesta status"
    p = eff.price_attempt("t", "pass", [setup, solve])
    assert p.turns == 1, f"counted {p.turns} turns, expected only the solver's one"


def test_a_ratio_is_only_reported_when_both_sides_have_passes():
    """Dividing by zero passes produces a number that looks like a saving."""
    a = {"passed": 1, "cost_per_pass_tokens": 100, "turns_per_pass": 2, "pass_rate": 1.0}
    b = {"passed": 0, "cost_per_pass_tokens": 0, "turns_per_pass": 0, "pass_rate": 0.0}
    out = eff.compare(a, b, "quiet", "broken")
    assert "cost_ratio" not in out, out
    assert "turns_ratio" not in out, out
    assert out["quality_gap_points"] == 100.0


def test_the_quality_gap_is_reported_alongside_the_cost_ratio():
    """A cost win with a quality loss is not a win, and the two numbers must travel together."""
    a = {"passed": 8, "cost_per_pass_tokens": 100, "turns_per_pass": 2, "pass_rate": 1.0}
    b = {"passed": 4, "cost_per_pass_tokens": 50, "turns_per_pass": 1, "pass_rate": 0.5}
    out = eff.compare(a, b, "cheap", "expensive")
    assert out["cost_ratio"] == 2.0
    assert out["quality_gap_points"] == 50.0


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
        print(f"{failures} cost test(s) failed")
        sys.exit(1)
    print("every cost test passes")
