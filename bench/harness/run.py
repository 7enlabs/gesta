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


@dataclass
class Attempt:
    task: str
    tries: int
    outcome: str
    """pass, fail, timeout, error, or not-run. `not-run` is its own outcome, never a pass."""

    steps: list[Step] = field(default_factory=list)
    checks: list[Step] = field(default_factory=list)
    note: str = ""

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
        return Step(
            command=command,
            exit_code=proc.returncode,
            seconds=round(time.monotonic() - started, 3),
            stdout_tail=(proc.stdout or "")[-800:],
            stderr_tail=(proc.stderr or "")[-800:],
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


def run_task(task: Task, gesta_bin: str, tries: int, budget: int) -> Attempt:
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
            script = "set -e\n" + "\n".join(lines)
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
        elif a == "--selftest":
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
            if attempt.outcome == "pass":
                best = attempt
                break
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

    s = run.summary()
    print()
    for outcome, count in sorted(s["outcomes"].items()):
        print(f"  {outcome}: {count}")
    print(f"\n{s['tasks']} attempt(s) over {len(selected)} task(s), k={k}, seed={seed}")

    if not s["complete"]:
        print("the run is incomplete: some attempts did not finish, so the counts above are partial")
    print(f"written to {out / 'run.json'}")

    return 0 if s["complete"] and s["outcomes"].get("pass", 0) == s["tasks"] else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
