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


## Two layers, because one hides a third of the signal

A task can succeed while the tool behaved badly: a command prints the right file and also writes a
warning into the machine-readable stream, or crashes after the work is written. Scoring only the
deliverable reports none of that. The harness benchmark literature puts the size of the gap at
31.1% of runs: task success while failing at least one criterion about how.

So every attempt is scored twice. The first layer asks whether the job got done. The second asks
whether the tool honoured five obligations while doing it:

| Criterion | Rejects |
|---|---|
| quiet-stderr | a successful command writing to the error stream |
| failure-explains | a refusal with no message |
| no-internal-leak | a database path, a raw SQL error, or a panic reaching the reader |
| no-panic | exit 101, which is a crash rather than a refusal |
| json-one-document | `--json` output a parser cannot read |

The criteria read the same recorded steps the checks read, so they cost one pass over evidence that
already exists. They are also easy to get wrong in the direction of accusing innocent runs, and a
criterion that accuses innocent runs gets ignored. Two exclusions keep it honest: a command whose
output was piped or redirected away is not judged, and neither is a multi-line script, whose stdout
is the concatenation of every line rather than the property of one command.

## Numbers carry intervals, and the interval comes from tasks

`pass@k` is the chance at least one of k attempts succeeds, which is capability with retries.
`pass^k` is the chance every attempt succeeds, which is reliability without one. A consumer who gets
one attempt needs the second, and a report showing only the first measures something nobody buys.
The distance between them is the size of the reliability problem still unsolved, and the report
prints it.

Variance decomposes into a between-task term divided by the task count and a within-task term
divided by tasks times runs. Adding runs shrinks only the second, and on agent work the first is
usually larger, because a task set holds a few trivial instances, a long middle, and some nothing
solves. The bootstrap therefore resamples tasks and never runs: resampling runs would treat repeats
of one easy task as independent evidence and report an interval too narrow to be useful.

A single task reports `[0, 100]` rather than a zero-width interval, because one observation carries
no information about spread and printing certainty from it is a lie with a decimal point.

## A passing score is evidence only if the instrument can see a defect

`--selftest` proves the checks can fail. That is necessary and not sufficient: it says the checks
notice when the work is missing, not that the instrument notices anything finer.

`--mutate` plants a known defect in the reference answer and requires that something catches it.
Nineteen are planted across the task set: a wrong commit message, a warning on a successful command,
a leaked database path, a `--json` stream no parser can read. Every one must be caught, by the task
checks or by a criterion.

This is the part that found real faults rather than confirming the obvious. It found a criterion
that read a truncated tail and therefore could not parse anything, one that judged a script's
concatenated output as if it belonged to the JSON command inside it, and two mutants whose planted
defect never reached the code path they claimed to test. Without mutation those four would have
looked like a clean bill.

## Known limits

The task set is eight. A day of version control has more gestures, and a set this size finds a tool
that cannot do the common ones, not one that cannot do the rare ones. The intervals say so rather
than hiding it.

No task drives a language model. The harness scores a fixed reference answer and mutants of it, so
`pass^k` over attempts measures the nondeterminism of the tool and the filesystem, not of a model.
Measuring a model through the same interface is possible and is not what the runs in `runs/`
contain.
