#!/usr/bin/env python3
"""Exercise the workflow's actual Gate shell with explicit exit-code test doubles."""

import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent


def gate_script(workflow):
    """Extract the literal run block of the uniquely named Gate step."""
    matches = list(re.finditer(r"^      - name: Gate\s*$", workflow, re.MULTILINE))
    if len(matches) != 1:
        raise ValueError("expected exactly one Gate step")
    lines = workflow[matches[0].end():].splitlines()
    collecting = False
    script = []
    for line in lines:
        if collecting:
            if line and not line.startswith("          "):
                break
            script.append(line[10:] if line else "")
        elif line.strip() == "run: |":
            collecting = True
        elif line.startswith("      - "):
            break
    if not script:
        raise ValueError("Gate needs a literal run block")
    return "\n".join(script)


class WorkflowGateTests(unittest.TestCase):
    def test_only_a_complete_gate_can_pass_ci(self):
        script = gate_script((ROOT / ".github/workflows/gate.yml").read_text())
        with tempfile.TemporaryDirectory(prefix="gesta-ci-") as tmp:
            directory = Path(tmp)
            python = directory / "python3"
            python.write_text('#!/bin/sh\nexit "$MOCK_GATE_EXIT"\n')
            python.chmod(0o700)
            for code in (0, 1, 2, 3, 17, 127):
                with self.subTest(gate_exit=code):
                    env = dict(os.environ)
                    env.update(
                        PATH=str(directory) + os.pathsep + env["PATH"],
                        MOCK_GATE_EXIT=str(code),
                        GITHUB_OUTPUT=str(directory / "outputs"),
                    )
                    result = subprocess.run(
                        ["bash", "-e", "-o", "pipefail", "-c", script],
                        cwd=ROOT,
                        env=env,
                        capture_output=True,
                        text=True,
                        timeout=10,
                    )
                    self.assertEqual(
                        result.returncode,
                        code,
                        f"gate exit {code} became CI exit {result.returncode}: "
                        f"{result.stdout}{result.stderr}",
                    )


if __name__ == "__main__":
    unittest.main()
