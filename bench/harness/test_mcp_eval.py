"""Tests for the MCP evaluation itself.

An evaluation that has never produced a different number for a different catalog measures nothing.
These tests build a good selection run and a deliberately bad one and require that the scores
separate them. Where the score is identical for both, the case set is not asking a question.

The bad runs are not noise: each names a specific defect a catalog can have, so a failure here says
which question the case set cannot answer.
"""

from __future__ import annotations

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import mcp_eval


def _perfect() -> list[dict]:
    return [{"tool": c.tool, "args": dict(c.args)} for c in mcp_eval.CASES]


def test_a_perfect_run_scores_one():
    s = mcp_eval.score(_perfect())
    assert s["selection_accuracy"] == 1.0, s
    assert s["argument_accuracy"] == 1.0, s
    assert s["forced_a_fit_where_nothing_fits"] == 0, s
    assert s["confusions"] == [], s


def test_confusing_two_neighbours_lowers_the_score():
    """The failure this case set exists for: status picked when brief was meant."""
    run = _perfect()
    for sel in run:
        if sel["tool"] == "gesta_brief":
            sel["tool"] = "gesta_status"
    s = mcp_eval.score(run)
    assert s["selection_accuracy"] < 1.0, "swapping a neighbour did not change the score"
    assert s["recall"]["gesta_brief"] == 0.0, s["recall"]
    assert any("nearly fits" in c for c in s["confusions"]), s["confusions"]


def test_a_forced_fit_is_scored_as_a_defect_not_a_choice():
    """A request no tool answers must be declined. Any choice here turns "I cannot" into an action."""
    run = _perfect()
    for sel in run:
        if sel["tool"] == "":
            sel["tool"] = "gesta_status"
    s = mcp_eval.score(run)
    assert s["forced_a_fit_where_nothing_fits"] == 1, s
    assert s["declined_when_nothing_fits"] == 0, s


def test_wrong_arguments_are_scored_apart_from_wrong_selection():
    """A right tool with a wrong limit is a different defect from picking the wrong tool."""
    run = _perfect()
    for sel in run:
        if sel["args"].get("limit") == 5:
            sel["args"]["limit"] = 50
    s = mcp_eval.score(run)
    assert s["selection_accuracy"] == 1.0, "argument error leaked into selection accuracy"
    assert s["argument_accuracy"] < 1.0, "a wrong argument was not noticed"


def test_an_unnamed_argument_is_not_asserted():
    """A sensible default for a field the request never mentioned is not an error."""
    run = [{"tool": c.tool, "args": {}} for c in mcp_eval.CASES]
    for sel, case in zip(run, mcp_eval.CASES):
        sel["args"] = {}
    s = mcp_eval.score(run)
    # Only cases that name arguments are scored, and they fail; the rest must not.
    assert s["selection_accuracy"] == 1.0, "a missing argument broke selection scoring"


def test_a_run_with_the_wrong_length_is_refused():
    """A short list must not score as if the missing cases passed."""
    try:
        mcp_eval.score(_perfect()[:-1])
    except ValueError as exc:
        assert "one selection per case" in str(exc)
        return
    raise AssertionError("a short run was scored instead of refused")


def test_the_catalogue_cost_is_measured_from_the_catalog():
    """The cost is what travels with every request, so it is counted from the real definitions."""
    tools = [
        {"name": "a", "description": "x" * 400, "inputSchema": {"type": "object"}},
        {"name": "b", "description": "y", "inputSchema": {"type": "object"}},
    ]
    cost = mcp_eval.catalogue_cost(tools)
    assert cost["tools"] == 2
    assert cost["bytes"] > 400
    assert cost["estimated_tokens"] == round(cost["bytes"] / 4)
    assert cost["largest"][1] == "a", "the largest tool was not identified"


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
        print(f"{failures} mcp eval test(s) failed")
        sys.exit(1)
    print("every mcp eval test passes")
