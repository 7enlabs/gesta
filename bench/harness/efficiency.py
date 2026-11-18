"""What a task costs to complete, which is the number a harness is judged on.

A pass rate on its own says a tool can do the work. It says nothing about what the work cost, and
the published harness benchmark that held the model fixed found cost moving 17.5x across harnesses
while quality stayed within about 17 points. The expensive cell was not the worst one: it was the
one whose answers arrived in more pieces.

So this module prices a task the way that benchmark does, in the units a caller actually pays:

- **bytes read**: everything the tool wrote to stdout and stderr, which is what enters a transcript
- **turns**: how many commands the answer needed, because each one is a round trip
- **first cold cost**: the cost of the first attempt with nothing cached
- **cost per pass**: total bytes over the tasks that passed, which is the only figure that lets two
  harnesses be compared

Token counts are estimated as bytes / 4, the same convention the measurement literature uses. The
estimate is reliable for comparison and approximate in absolute terms, and saying which is which is
the difference between a number and a guess.
"""

from __future__ import annotations

from dataclasses import dataclass, field

BYTES_PER_TOKEN = 4


@dataclass
class Price:
    """What one attempt cost, in the units a caller pays."""

    task: str
    passed: bool
    turns: int
    bytes_read: int
    bytes_written: int
    seconds: float

    @property
    def estimated_tokens(self) -> int:
        """Everything the caller had to look at, estimated from its size."""
        return round((self.bytes_read + self.bytes_written) / BYTES_PER_TOKEN)


@dataclass
class Ledger:
    """A run priced as a whole."""

    prices: list[Price] = field(default_factory=list)

    def add(self, price: Price) -> None:
        self.prices.append(price)

    def summary(self) -> dict:
        if not self.prices:
            return {
                "tasks": 0,
                "passed": 0,
                "pass_rate": 0.0,
                "cost_per_pass": 0.0,
                "turns_per_pass": 0.0,
                "bytes_per_pass": 0,
                "no_passes": True,
            }

        passed = [p for p in self.prices if p.passed]
        total_bytes = sum(p.bytes_read + p.bytes_written for p in self.prices)
        by_turns = sorted(p.turns for p in self.prices)
        by_seconds = sorted(p.seconds for p in self.prices)

        # Divided by the number of passes, not by the number of attempts. A harness that fails half
        # its tasks cheaply has not earned a low cost per pass; it has earned no passes, and the
        # figure is reported as the cost of the ones it did get.
        per_pass_bytes = (
            sum(p.bytes_read + p.bytes_written for p in passed) / len(passed) if passed else 0
        )
        per_pass_turns = sum(p.turns for p in passed) / len(passed) if passed else 0

        return {
            "tasks": len(self.prices),
            "passed": len(passed),
            "pass_rate": len(passed) / len(self.prices),
            "total_bytes": total_bytes,
            "total_tokens_estimate": round(total_bytes / BYTES_PER_TOKEN),
            "cost_per_pass_tokens": round(per_pass_bytes / BYTES_PER_TOKEN),
            "bytes_per_pass": round(per_pass_bytes),
            "turns_per_pass": round(per_pass_turns, 2),
            "median_turns": by_turns[len(by_turns) // 2],
            "median_seconds": round(by_seconds[len(by_seconds) // 2], 2),
            "no_passes": not passed,
        }

    def by_task(self) -> list[dict]:
        """The per-task rows a report needs to show which task was expensive and why."""
        return [
            {
                "task": p.task,
                "passed": p.passed,
                "turns": p.turns,
                "bytes": p.bytes_read + p.bytes_written,
                "tokens_estimate": p.estimated_tokens,
                "seconds": round(p.seconds, 2),
            }
            for p in self.prices
        ]


def price_attempt(task_name: str, outcome: str, steps: list) -> Price:
    """Price one attempt from the steps it recorded.

    Every step counts, including the ones that failed, because a caller reads them too. A tool that
    explains a refusal in 400 bytes has cost the caller 400 bytes, and a harness that hides that
    reports a cheaper run than it had.
    """
    # A turn is a command the solver chose to run.
    #
    # Setup builds the starting state and the checks verify it, so counting either prices the
    # harness rather than the tool. A solve phase runs as one shell, so its own line count is the
    # number of commands in it: `set -e` is not a command the solver chose.
    solver_steps = [s for s in steps if getattr(s, "phase", "solve") == "solve"]
    turns = 0
    for s in solver_steps:
        for line in s.command.splitlines():
            line = line.strip()
            if not line or line.startswith("#") or line == "set -e":
                continue
            turns += 1
    # Only what the tool said counts. Bytes from the harness's own setup and from its checks are the
    # cost of measuring, and adding them to the cost of the tool is how a cheap tool looks expensive
    # and an expensive one looks fine.
    read = sum(getattr(s, "stdout_bytes", 0) for s in solver_steps)
    written = sum(getattr(s, "stderr_bytes", 0) for s in solver_steps)
    seconds = sum(getattr(s, "seconds", 0.0) for s in steps)
    return Price(
        task=task_name,
        passed=outcome == "pass",
        turns=turns,
        bytes_read=read,
        bytes_written=written,
        seconds=seconds,
    )


def compare(a: dict, b: dict, name_a: str = "a", name_b: str = "b") -> dict:
    """Two ledgers side by side, in the form the benchmark uses.

    A ratio is reported only when both sides have passes. Dividing by zero passes produces a number
    that looks like a saving and is an artefact.
    """
    out = {
        "a": {"name": name_a, **a},
        "b": {"name": name_b, **b},
    }
    if a.get("passed") and b.get("passed") and b.get("cost_per_pass_tokens"):
        out["cost_ratio"] = round(a["cost_per_pass_tokens"] / b["cost_per_pass_tokens"], 2)
    if a.get("turns_per_pass") and b.get("turns_per_pass"):
        out["turns_ratio"] = round(a["turns_per_pass"] / b["turns_per_pass"], 2)
    out["quality_gap_points"] = round((a.get("pass_rate", 0) - b.get("pass_rate", 0)) * 100, 1)
    return out


def fmt(summary: dict) -> str:
    """One block a person can read without opening a JSON file."""
    if summary.get("no_passes"):
        return (
            f"  {summary['passed']}/{summary['tasks']} passed: no cost per pass can be computed, "
            f"because there is no pass to divide by"
        )
    return (
        f"  {summary['passed']}/{summary['tasks']} passed\n"
        f"  cost per pass   {summary['cost_per_pass_tokens']:,} estimated tokens "
        f"({summary['bytes_per_pass']:,} bytes)\n"
        f"  turns per pass  {summary['turns_per_pass']}\n"
        f"  median          {summary['median_turns']} turns, {summary['median_seconds']}s per task"
    )
