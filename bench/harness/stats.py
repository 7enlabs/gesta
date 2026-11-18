"""Statistics for a harness report.

A score with no interval is a point estimate of unknown precision. This module supplies the parts
that make a report readable, and each part exists because of a specific way an agent benchmark
misleads a reader who takes the headline number at face value.

The variance of a mean score over T tasks with R runs each decomposes as

    Var(score) ~ sigma_between^2 / T  +  sigma_within^2 / (T * R)

Adding runs shrinks only the second term. On agent benchmarks the first term is usually the larger
one, because a task set contains a few trivial instances, a long middle, and some nothing solves.
So the honest lever is more tasks, and a bootstrap that resamples runs reports an interval far too
narrow: the task is the unit of independent observation.

`pass@k` and `pass^k` are one character apart and answer opposite questions. pass@k is the chance
at least one of k attempts succeeds, which is capability with retries. pass^k is the chance every
attempt succeeds, which is reliability without a retry. A consumer that gets one attempt needs the
second number, and a dashboard showing only the first measures something nobody is buying.
"""

from __future__ import annotations

import random


def bootstrap_ci(
    per_task: list[float],
    iters: int = 2000,
    alpha: float = 0.05,
    seed: int = 0,
) -> tuple[float, float, float]:
    """Percentile bootstrap over tasks.

    Resamples TASKS, never runs. A task contributes one number, and that number is what varies
    between a one-line config change and a cross-module refactor. Resampling runs would treat
    repeats of the same easy task as independent evidence and report a interval too narrow to be
    useful.

    Returns (mean, low, high).
    """
    if not per_task:
        return (0.0, 0.0, 0.0)
    rng = random.Random(seed)
    n = len(per_task)
    mean = sum(per_task) / n
    if n == 1:
        # One task carries no information about spread, and pretending otherwise would print a
        # interval of zero width that reads as certainty.
        return (mean, 0.0, 1.0)
    means = []
    for _ in range(iters):
        total = 0.0
        for _ in range(n):
            total += per_task[rng.randrange(n)]
        means.append(total / n)
    means.sort()
    lo = means[int((alpha / 2) * iters)]
    hi = means[min(iters - 1, int((1 - alpha / 2) * iters))]
    return (mean, lo, hi)


def paired_ci(
    a: list[float],
    b: list[float],
    iters: int = 2000,
    alpha: float = 0.05,
    seed: int = 0,
) -> tuple[float, float, float]:
    """Bootstrap the per-task difference, which is far tighter than two independent intervals.

    A hard task is hard for both systems, so its difficulty cancels out of the difference. What
    remains is the spread of per-task differences, and that is much smaller than the spread of
    either score. This is why a comparison can resolve an effect that neither absolute number can.
    """
    if len(a) != len(b):
        raise ValueError("a paired comparison needs one value from each side per task")
    if not a:
        return (0.0, 0.0, 0.0)
    diffs = [x - y for x, y in zip(a, b)]
    return bootstrap_ci(diffs, iters=iters, alpha=alpha, seed=seed)


def pass_at_k(outcomes: list[bool]) -> float:
    """Capability: did any attempt succeed."""
    return 1.0 if any(outcomes) else 0.0


def pass_pow_k(outcomes: list[bool]) -> float:
    """Reliability: did every attempt succeed."""
    return 1.0 if outcomes and all(outcomes) else 0.0


def reliability_gap(pass_k: float, pass_all_k: float) -> float:
    """The distance between what the tool can do and what it can be trusted to do.

    Reporting only pass@k measures a tool nobody gets to use twice. The gap is the size of the
    reliability problem still unsolved, and it is the number a person feels.
    """
    return pass_k - pass_all_k


def fmt_ci(mean: float, lo: float, hi: float) -> str:
    """A number and its interval, in the form a reader can act on."""
    return f"{mean * 100:5.1f}%  [{lo * 100:5.1f}, {hi * 100:5.1f}]"


def separates(lo: float, hi: float) -> bool:
    """Whether an interval on a difference excludes zero, which is the only claim a difference
    supports on its own. A gap whose interval contains zero is a gap the data cannot see."""
    return hi < 0 or lo > 0
