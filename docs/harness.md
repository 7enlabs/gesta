# The harness

`bench/harness` measures whether a person or an agent can do a day of version-control work with
gesta. The other benchmarks in `bench/` call the binary and check a number; this one asks what
happens when somebody is handed a job and a shell.

## Running it

```
python3 bench/harness/run.py                  # every task, one attempt each
python3 bench/harness/run.py --task tags      # one task, and nothing if the name is wrong
python3 bench/harness/run.py --selftest       # does any check pass without the work?
python3 bench/harness/test_harness.py         # the instrument's own tests
```

`GESTA_BIN` chooses the binary. `GESTA_HARNESS_RUNS` chooses where the run record lands, default
`runs/`. Every run writes `run.json` with each attempt, each command, its exit code, its duration,
and what ended it.

## The three outcomes

A task ends as `pass`, `fail`, `timeout`, or `error`, and the last two are not failures.

`timeout` means the clock ended the attempt. Recording that as a wrong answer blames the work for
the budget, and it is a mistake worth naming because a runner that keeps an attempt whenever its
output directory is non-empty makes exactly that mistake: a rollout stopped at a time limit looks
identical to an agent that decided to stop, and the reader then attributes the stop to the last
edit.

`error` means the task itself is broken, either because its setup did not build a starting state or
because the reference answer did not run. Scoring the checks after that measures the task, not the
tool.

`not-run` exists for a check that never executed. A check that did not run is not a check that
passed, and a harness that cannot tell those apart reports on work nobody did.

## Why every task carries a reference answer

A check that has never been passed measures nothing: a broken check and a broken tool look exactly
the same from outside. `solve` is the known-good path, the runner proves every check passes along
it, and only then does any other attempt get scored.

## Why the checks are tested against doing nothing

`solve` proves the checks can pass. It does not prove they can fail. `--selftest` runs each task's
setup, skips the reference answer, and requires that not every check passes. A task that passes is
a task that measures agreement with itself.

This found three faults that a passing run could not:

- `review-before-landing` ran the review inside the check. The check performed the goal instead of
  verifying it, so the task would have passed with no work done.
- `refuse-a-stale-landing` ran the landing inside the check, for the same reason.
- `stash-a-deletion` restored the state the task starts in, so observing the files alone could not
  tell the work from the starting state. It now requires that a stash was made and then dropped.

## What the environment carries

A task's commands run in a scrubbed environment: an allowlist of variables needed to execute at
all, plus an explicit list checked one entry at a time. A secret-shaped name is dropped even when
it is on the list, because the list is about function and that rule is about risk.

Credentials do not appear in the explicit list. A task that pushes to a real bucket is a task that
can leak a key, and both remote tasks use a local directory.

The scrub is tested, not assumed: one test exports an off-list credential and requires that it does
not reach the task, and another requires that no explicitly kept name looks like a credential.

## Known limits, stated rather than implied

The tasks are eight. A day of version control has more gestures than eight, and a task set this
size finds a tool that cannot do the common ones, not one that cannot do the rare ones.

No task drives an agent. The harness scores a fixed reference answer and whatever a caller provides
through the same interface, so measuring a model's behaviour is possible but is not what the runs in
`runs/` contain.

A check on file contents cannot tell a stash from a saved copy unless it also asks about the stash
list. Where a task needs evidence that a state passed through, that evidence is written outside the
working tree, because saving a stash resets the tree and takes untracked files with it.
