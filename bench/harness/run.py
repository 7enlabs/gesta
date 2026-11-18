"""Run gesta tasks and record what happened, including what stopped the run.

Three properties, each learned from a silent failure somewhere else.

**The exit code decides the outcome, not the artefacts.** A run that wrote a partial file and then
hit a time limit looks like a finished run that chose to stop, and the reader blames the work
instead of the clock. Every attempt records its exit status and its stopping reason, and a non-zero
exit appends an error event to the trajectory before anything else reads it.

**Nothing skips quietly.** A check that did not run is not a check that passed. The record has three
outcomes: pass, fail, and did not run, and the summary refuses to call a run complete while any
attempt is unfinished.

**A subprocess does not inherit secrets it does not need.** An agent that runs shell commands will
eventually run `env`, so credentials reachable in its environment are disclosed by design. The
environment is scrubbed to an allowlist, exceptions are written down one by one, and the scrub is
tested rather than assumed.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import urllib.request
import uuid
from dataclasses import asdict, dataclass, field
from pathlib import Path

from tasks import TASKS, Task, by_name, names
import criteria as crit
import mutants as mut
import stats

# Variables an agent needs to run at all. Anything else is dropped, so a key that happens to be
# exported in the developer's shell does not reach a process that can print its own environment.
KEEP_ENV = {
    "PATH", "HOME", "USER", "LANG", "LC_ALL", "TERM", "TMPDIR", "TEMP", "TMP",
    "SHELL", "PWD", "SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT", "PROGRAMFILES",
    "PROGRAMDATA", "LOCALAPPDATA", "APPDATA", "NUMBER_OF_PROCESSORS", "PROCESSOR_ARCHITECTURE",
}

# Written down singly, because an allowlist with a wildcard is not an allowlist. Each entry names
# what needs it and why. Credentials do not appear here at all: a task that pushes to a real bucket
# is a task that can leak a key, and the remote tasks use a local directory instead.
KEEP_ENV_EXPLICIT = {
    # Endpoint and region describe where a bucket is, not how to enter it.
    "GESTA_S3_ENDPOINT", "GESTA_S3_REGION",
}

SECRET_SHAPED = re.compile(
    r"(KEY|TOKEN|SECRET|PASSWORD|PASSWD|CREDENTIAL|AUTH|COOKIE|SESSION)", re.IGNORECASE
)


def scrub_env(extra: dict[str, str] | None = None) -> dict[str, str]:
    """The environment a task's commands run in.

    A secret-shaped name is dropped even when it is on the keep list, because the list above is
    about function and this rule is about risk. The two are checked separately so that adding a
    functional variable can never quietly add a credential.
    """
    out = {k: v for k, v in os.environ.items() if k in KEEP_ENV and not SECRET_SHAPED.search(k)}
    out.update({k: v for k, v in os.environ.items() if k in KEEP_ENV_EXPLICIT})
    if extra:
        out.update(extra)
    return out


@dataclass
class Step:
    """One command and what came of it."""

    command: str
    exit_code: int | None
    seconds: float
    stdout_tail: str
    stderr_tail: str
    stop_reason: str = ""
    """`timeout` or `signal` when the clock or the system ended it, empty when the command did."""

    stdout_full: str = ""
    """Complete stdout, kept only for a command that asked for `--json`.

    A criterion that claims "one document a parser can read" has to be able to parse it. Reading a
    truncated tail makes that claim untestable, and an untestable claim passes forever.
    """


@dataclass
class Attempt:
    task: str
    tries: int
    outcome: str
    """pass, fail, timeout, error, or not-run. `not-run` is its own outcome, never a pass."""

    steps: list[Step] = field(default_factory=list)
    checks: list[Step] = field(default_factory=list)
    note: str = ""
    criteria: dict = field(default_factory=dict)
    """Per-obligation verdicts, scored apart from whether the job got done."""

    def unfinished(self) -> bool:
        return self.outcome == "not-run"


@dataclass
class Run:
    run_id: str
    gesta: str
    started: str
    k: int
    mode: str
    seed: int
    attempts: list[Attempt] = field(default_factory=list)

    def summary(self) -> dict:
        counts: dict[str, int] = {}
        for a in self.attempts:
            counts[a.outcome] = counts.get(a.outcome, 0) + 1
        return {
            "run_id": self.run_id,
            "gesta": self.gesta,
            "k": self.k,
            "mode": self.mode,
            "seed": self.seed,
            "tasks": len(self.attempts),
            "outcomes": counts,
            "unfinished": sum(1 for a in self.attempts if a.unfinished()),
            "complete": all(not a.unfinished() for a in self.attempts),
            "criterion_violations": sum(
                1 for a in self.attempts for c in (a.criteria or {}).values() if not c.get("ok")
            ),
        }


def sh(
    command: str,
    cwd: str,
    env: dict[str, str],
    budget: int,
) -> Step:
    """Run one shell line, and say what ended it."""
    started = time.monotonic()
    try:
        proc = subprocess.run(
            command,
            shell=True,
            cwd=cwd,
            env=env,
            capture_output=True,
            text=True,
            timeout=budget,
        )
        out = proc.stdout or ""
        return Step(
            command=command,
            exit_code=proc.returncode,
            seconds=round(time.monotonic() - started, 3),
            stdout_tail=out[-800:],
            stderr_tail=(proc.stderr or "")[-800:],
            # Only when JSON was asked for, and only up to a bound: a document larger than this is
            # a design problem the criterion should not paper over.
            stdout_full=out if ("--json" in command and len(out) <= 1_000_000) else "",
        )
    except subprocess.TimeoutExpired as exc:
        # The clock ended it. Saying `fail` here would blame the work for the budget, which is the
        # mistake a run that only looked at whether output existed made.
        return Step(
            command=command,
            exit_code=None,
            seconds=round(time.monotonic() - started, 3),
            stdout_tail=((exc.stdout or b"").decode("utf-8", "replace") if isinstance(exc.stdout, bytes) else (exc.stdout or ""))[-800:],
            stderr_tail=((exc.stderr or b"").decode("utf-8", "replace") if isinstance(exc.stderr, bytes) else (exc.stderr or ""))[-800:],
            stop_reason="timeout",
        )


def _replace_all(line: str, pairs: list[tuple[str, str]]) -> str:
    """Apply a mutant's replacements to one line, in order."""
    for find, repl in pairs:
        line = line.replace(find, repl)
    return line


def _only_expected_refusals(lines: list[str]) -> bool:
    """Whether every line that can fail is one the task expects to fail.

    A reference answer that ends in a refusal has a non-zero status on purpose. Anything else
    failing is a broken task, and the two must not be confused, because one of them means the
    measurement is wrong rather than the tool.
    """
    if not lines:
        return False
    return bool(lines) and all(
        l.lstrip().startswith("!") for l in lines if l.strip() and not l.strip().startswith("#")
    )


def _expected_to_refuse(line: str) -> bool:
    """Whether a reference line is meant to fail.

    Written as an explicit marker rather than inferred from the command, because a solver that
    guesses what "should have failed" cannot report anything.
    """
    return line.lstrip().startswith("!")


def run_task(task: Task, gesta_bin: str, tries: int, budget: int, mutant=None) -> Attempt:
    """Build a fresh repository for the task and score one attempt at it."""
    root = tempfile.mkdtemp(prefix=f"gesta-task-{task.name}-")
    env = scrub_env()
    attempt = Attempt(task=task.name, tries=tries, outcome="not-run")

    # Reachable by name inside the task's shell, so a task does not have to hardcode a path.
    bin_dir = os.path.join(root, ".bin")
    os.makedirs(bin_dir, exist_ok=True)
    link = os.path.join(bin_dir, "gesta")
    if not os.path.exists(link):
        os.symlink(gesta_bin, link)

    # Tasks need the id of the changeset they just created, and the JSON is pretty-printed across
    # several lines so a single-line text match does not find it. A helper keeps every task
    # definition readable and puts the one place that parses output in one place.
    cs_helper = os.path.join(bin_dir, "cs_id")
    with open(cs_helper, "w", encoding="utf-8") as fh:
        # A Python helper rather than sed, because a sed replacement escapes backslashes through
        # three layers and one of them turns \1 into a control character. This reads the number.
        fh.write(
            "#!/usr/bin/env python3\n"
            "import json, sys\n"
            "try:\n"
            "    doc = json.load(sys.stdin)\n"
            "except Exception:\n"
            "    sys.exit(1)\n"
            "print(doc.get('changeset', ''))\n"
        )
    os.chmod(cs_helper, 0o755)

    env["PATH"] = bin_dir + os.pathsep + env.get("PATH", "")

    work = os.path.join(root, "repo")
    os.makedirs(work, exist_ok=True)

    try:
        # Setup and the reference answer run as one shell each, with `set -e`.
        #
        # The lines of a task are a script: `C=$(...)` on one line is read by the next. Running
        # each line in its own process drops every variable between them, and the failure looks
        # like a tool refusing an empty id rather than a runner that threw the id away.
        #
        # The checks run one per process, because each one has to report its own exit code. A
        # single script would only report the last line's status and hide every earlier failure.
        for phase, lines in (("setup", task.setup), ("solve", task.solve)):
            if not lines:
                continue
            work_lines = list(lines)
            if phase == "solve" and mutant is not None:
                # The defect is planted in the answer, not in the task, so the task stays a task.
                work_lines = [
                    _replace_all(line, mutant.replaces) for line in work_lines
                ] + list(mutant.commands)
            script = "set -e\n" + "\n".join(work_lines)
            if phase == "solve":
                # `set` after the work lists every variable and its value; a check can source it.
                script += f"\nset > {os.path.join(root, 'vars.sh')}\n"
            step = sh(script, work, env, budget)
            attempt.steps.append(step)

            if step.stop_reason:
                attempt.outcome = "timeout"
                attempt.note = f"the {phase} phase stopped by the clock"
                return attempt

            # A failing line is an error in both phases, with one exception: a task may want a
            # command to be refused, and marks that line with a leading `!`. Any other failure
            # means the reference answer itself is broken, and scoring the checks after it would
            # report the tool's fault when the fault is in the task.
            if step.exit_code != 0 and not _only_expected_refusals(lines):
                attempt.outcome = "error"
                attempt.note = f"{phase} failed\n{step.stderr_tail or step.stdout_tail}"
                return attempt

        # A check may name a variable the reference answer set, so the solve phase writes its
        # variables to a file and every check sources it. Replaying the solve phase instead would
        # run the work twice, and for a task about undo that changes the answer.
        env_file = os.path.join(root, "vars.sh")
        for line in task.checks:
            step = sh(f". {env_file} 2>/dev/null || true\n{line}", work, env, budget)
            attempt.checks.append(step)
            if step.stop_reason:
                attempt.outcome = "timeout"
                attempt.note = f"check stopped by the clock: {line}"
                return attempt

        # Criteria about how the tool behaved, scored on the same recorded steps. A task can pass
        # while a criterion fails, and that combination is the one worth counting.
        attempt.criteria = {
            c.key: {"ok": ok, "why": why} for c in crit.CRITERIA for (ok, why) in [c.evaluate(attempt)]
        }

        failed = [c for c in attempt.checks if c.exit_code != 0]
        attempt.outcome = "fail" if failed else "pass"
        if failed:
            attempt.note = failed[0].command
        return attempt
    finally:
        shutil.rmtree(root, ignore_errors=True)


def main(argv: list[str]) -> int:
    task_filter = None
    selftest = "--selftest" in argv
    k = 1
    seed = 0
    runs_dir = os.environ.get("GESTA_HARNESS_RUNS", "runs")
    gesta_bin = os.environ.get("GESTA_BIN", shutil.which("gesta") or "")

    i = 1
    while i < len(argv):
        a = argv[i]
        if a == "--task":
            task_filter = argv[i + 1]
            i += 2
        elif a == "--k":
            k = int(argv[i + 1])
            i += 2
        elif a == "--seed":
            seed = int(argv[i + 1])
            i += 2
        elif a == "--gesta":
            gesta_bin = argv[i + 1]
            i += 2
        elif a == "--runs":
            runs_dir = argv[i + 1]
            i += 2
        elif a in ("--selftest", "--mutate"):
            i += 1
        elif a in ("--help", "-h"):
            print(__doc__)
            print(f"tasks: {', '.join(names())}")
            return 0
        elif a == "--list":
            for t in TASKS:
                print(f"{t.name:28} {t.teaches}")
            return 0
        else:
            print(f"unknown argument {a!r}. Known tasks: {', '.join(names())}")
            return 2

    # Mutation mode: prove the harness can see a defect before trusting a score of zero defects.
    #
    # A deterministic reference answer passes every time, so pass@k and pass^k both read 1.0 and the
    # gap between them is zero by construction. That is a property of the instrument, not a fact
    # about the tool. Planting a known defect and requiring that it is caught is what turns the
    # score into evidence.
    if "--mutate" in argv:
        if not gesta_bin or not os.path.exists(gesta_bin):
            print("no gesta binary: set GESTA_BIN or put gesta on PATH")
            return 2
        survived = []
        caught = 0
        for task in TASKS:
            for mutant in mut.for_task(task.name):
                attempt = run_task(task, gesta_bin, 1, task.budget, mutant=mutant)
                # A defect is detected when either layer notices it. The declaration says which
                # layer is expected to, and a mutant caught by the other layer is still caught:
                # what would make the score dishonest is a defect nobody sees, not a defect seen
                # in an unexpected place. The expected layer is reported, not enforced.
                violated = [k for k, v in (attempt.criteria or {}).items() if not v.get("ok")]
                by_checks = attempt.outcome != "pass"
                by_criteria = bool(violated)
                noticed = by_checks or by_criteria
                for_layer = (
                    "task checks"
                    if mutant.expect == "checks-fail"
                    else "harness criteria"
                )
                how = f"task {attempt.outcome}"
                if violated:
                    how += f", criteria violated: {', '.join(violated)}"
                if noticed and not (
                    (by_checks and mutant.expect == "checks-fail")
                    or (by_criteria and mutant.expect == "criteria-fail")
                ):
                    how += f" (caught by the other layer; expected {for_layer})"
                if noticed:
                    caught += 1
                    print(f"  [caught] {task.name} x {mutant.name}: {how}")
                else:
                    survived.append(f"{task.name} x {mutant.name}")
                    print(f"  [SURVIVED] {task.name} x {mutant.name}: {how}")
        print()
        total = caught + len(survived)
        if survived:
            print(
                f"{len(survived)} of {total} planted defect(s) were not detected, so the score "
                f"overstates what this harness can see: {', '.join(survived)}"
            )
            return 1
        print(f"every one of {total} planted defect(s) was detected")
        return 0

    # A harness whose checks pass without the work being done measures agreement with itself.
    # This mode is the test that has to fail: run the setup, do nothing, and require that not every
    # check passes. Any task that passes here is not measuring anything.
    if selftest:
        leaks = []
        for task in TASKS:
            root = tempfile.mkdtemp(prefix=f"gesta-selftest-{task.name}-")
            env = scrub_env()
            bin_dir = os.path.join(root, ".bin")
            os.makedirs(bin_dir, exist_ok=True)
            os.symlink(gesta_bin, os.path.join(bin_dir, "gesta"))
            cs_helper = os.path.join(bin_dir, "cs_id")
            with open(cs_helper, "w", encoding="utf-8") as fh:
                fh.write(
                    "#!/usr/bin/env python3\n"
                    "import json, sys\n"
                    "print(json.load(sys.stdin).get('changeset', ''))\n"
                )
            os.chmod(cs_helper, 0o755)
            env["PATH"] = bin_dir + os.pathsep + env.get("PATH", "")
            work = os.path.join(root, "repo")
            os.makedirs(work, exist_ok=True)
            setup = sh("set -e\n" + "\n".join(task.setup), work, env, task.budget)
            if setup.exit_code != 0:
                print(f"  [SKIP] {task.name}: its setup does not build a starting state")
                leaks.append(task.name)
                shutil.rmtree(root, ignore_errors=True)
                continue
            passed = 0
            for line in task.checks:
                if sh(line, work, env, task.budget).exit_code == 0:
                    passed += 1
            if passed == len(task.checks):
                print(f"  [LEAK] {task.name}: every check passes with no work done")
                leaks.append(task.name)
            else:
                print(f"  [ok]   {task.name}: {passed}/{len(task.checks)} pass with no work done")
            shutil.rmtree(root, ignore_errors=True)
        print()
        if leaks:
            print(f"{len(leaks)} task(s) measure nothing: {', '.join(leaks)}")
            return 1
        print(f"every task distinguishes work from no work, over {len(TASKS)} task(s)")
        return 0

    if not gesta_bin or not os.path.exists(gesta_bin):
        print("no gesta binary: set GESTA_BIN or put gesta on PATH")
        return 2

    # A task name that matches nothing must refuse, or a subset run reports success for a set it
    # never built.
    selected: list[Task]
    if task_filter is None:
        selected = TASKS
    else:
        picked = by_name(task_filter)
        if picked is None:
            near = [n for n in names() if task_filter.lower() in n.lower() or n.lower().startswith(task_filter.lower()[:4])]
            print(f"no task is named {task_filter!r}: nothing would run")
            if near:
                print(f"did you mean: {', '.join(near[:6])}")
            print(f"{len(TASKS)} task(s) exist: {', '.join(names())}")
            return 2
        selected = [picked]

    run = Run(
        run_id=time.strftime("%Y%m%dT%H%M%S") + "-" + uuid.uuid4().hex[:6],
        gesta=os.path.basename(gesta_bin),
        started=time.strftime("%Y-%m-%d %H:%M:%S"),
        k=k,
        mode="sequential",
        seed=seed,
    )

    for task in selected:
        print(f"\n{task.name}  ({task.teaches})")
        best: Attempt | None = None
        for tries in range(1, k + 1):
            attempt = run_task(task, gesta_bin, tries, task.budget)
            run.attempts.append(attempt)
            mark = {
                "pass": "pass", "fail": "fail", "timeout": "timeout",
                "error": "error", "not-run": "did not run",
            }[attempt.outcome]
            print(f"  try {tries}: {mark}")
            if attempt.note:
                print(f"    {attempt.note.splitlines()[0][:160]}")
            # No early exit. pass@k needs at least one success out of k, and pass^k needs all k,
            # so stopping at the first pass would make the second number true by construction and
            # the gap between them zero by accident.
            best = best or attempt

        if best is not None and best.outcome != "pass":
            # Name the check that failed rather than the whole task, because "the task failed" is
            # not something a reader can act on.
            for c in best.checks:
                if c.exit_code not in (0, None):
                    print(f"    failing check: {c.command}")
                    if c.stderr_tail.strip():
                        print(f"      {c.stderr_tail.strip().splitlines()[-1][:160]}")

    out = Path(runs_dir) / run.run_id
    out.mkdir(parents=True, exist_ok=True)
    payload = {
        "summary": run.summary(),
        "attempts": [asdict(a) for a in run.attempts],
    }
    (out / "run.json").write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")

    # Two layers, reported separately.
    #
    # The first is whether the job got done. The second is whether the tool behaved while doing it.
    # A single score over both is the number that hides a third of the signal: a run can reach the
    # right file and still write a warning into the machine-readable stream, or crash after the
    # work is written.
    by_task: dict[str, list[Attempt]] = {}
    for a in run.attempts:
        by_task.setdefault(a.task, []).append(a)

    task_pass_k: list[float] = []
    task_pass_all_k: list[float] = []
    task_lines = []
    for task in selected:
        tries = by_task.get(task.name, [])
        outcomes = [a.outcome == "pass" for a in tries]
        p1 = stats.pass_at_k(outcomes)
        pk = stats.pass_pow_k(outcomes)
        task_pass_k.append(p1)
        task_pass_all_k.append(pk)
        task_lines.append((task.name, p1, pk, outcomes))

    mean_k, lo_k, hi_k = stats.bootstrap_ci(task_pass_k, seed=seed)
    mean_all, lo_all, hi_all = stats.bootstrap_ci(task_pass_all_k, seed=seed)

    challenges = crit.score(run.attempts)

    print()
    print("task outcomes")
    for name, p1, pk, outcomes in task_lines:
        marks = "".join("P" if o else "." for o in outcomes)
        print(f"  {name:26} {marks:8}  pass@k {p1:.0f}  pass^k {pk:.0f}")

    print()
    print(f"capability   pass@k   {stats.fmt_ci(mean_k, lo_k, hi_k)}   over {len(selected)} task(s)")
    print(f"reliability  pass^k   {stats.fmt_ci(mean_all, lo_all, hi_all)}")
    gap = stats.reliability_gap(mean_k, mean_all)
    if gap > 0:
        print(
            f"             the gap is {gap * 100:.1f} points: a tool that works on a retry is not "
            f"a tool that works"
        )
    if len(selected) < 20:
        # An interval from a handful of tasks is honest but wide, and saying so is cheaper than
        # letting a reader assume the middle of it.
        print(
            f"             {len(selected)} task(s) is a small sample: the interval is honest and "
            f"wide, and only a difference outside it is visible"
        )

    print()
    print("harness criteria")
    for key, row in challenges["criteria"].items():
        mark = "ok  " if row["violations"] == 0 else "FAIL"
        print(
            f"  {mark} {key:18} {row['violations']}/{row['total']} violated   {row['obligation']}"
        )
    if challenges["task_success_with_violation"]:
        print(
            f"\n  {challenges['task_success_with_violation']} run(s) did the job while breaking a "
            f"rule about how. A score that counts only the job would report those as clean."
        )

    s = run.summary()
    print()
    for outcome, count in sorted(s["outcomes"].items()):
        print(f"  {outcome}: {count}")
    print(f"\n{s['tasks']} attempt(s) over {len(selected)} task(s), k={k}, seed={seed}")

    if not s["complete"]:
        print("the run is incomplete: some attempts did not finish, so the counts above are partial")
    print(f"written to {out / 'run.json'}")

    # A run is releasable only when every task passed on every attempt and no criterion was
    # violated. Anything less is reported, not rounded up.
    clean = (
        s["complete"]
        and mean_all == 1.0
        and challenges["task_success_with_violation"] == 0
    )
    return 0 if clean else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
