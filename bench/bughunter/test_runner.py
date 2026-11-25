#!/usr/bin/env python3
"""Check the scenario runner itself, independently of product correctness."""

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


HERE = Path(__file__).resolve().parent
CHECKOUT = HERE.parents[1]


class RunnerContract(unittest.TestCase):
    def run_runner(self, *args, binary=None, runner=None):
        env = os.environ.copy()
        if binary is not None:
            env["GESTA_BIN"] = binary
        return subprocess.run(
            ["bash", str(runner or HERE / "run.sh"), *map(str, args)],
            env=env,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=120,
            check=False,
        )

    def test_missing_argument_is_usage_error(self):
        result = self.run_runner()
        self.assertEqual(result.returncode, 2)
        self.assertIn("usage:", result.stderr)

    def test_missing_binary_is_usage_error(self):
        result = self.run_runner(CHECKOUT, binary="/nonexistent/gesta-bughunter")
        self.assertEqual(result.returncode, 2)
        self.assertIn("binary not found", result.stderr)

    def test_zero_exit_without_valid_output_cannot_pass(self):
        result = self.run_runner(CHECKOUT, binary="/bin/true")
        self.assertEqual(result.returncode, 1)
        self.assertIn("Ran 12 tests", result.stdout)
        self.assertIn("0/12 bughunter scenarios pass; skipped=0", result.stdout)

    def test_failing_binary_cannot_pass(self):
        result = self.run_runner(CHECKOUT, binary="/bin/false")
        self.assertEqual(result.returncode, 1)
        self.assertIn("Ran 12 tests", result.stdout)
        self.assertIn("0/12 bughunter scenarios pass; skipped=0", result.stdout)

    def test_paths_with_spaces_and_shell_tokens_are_literal(self):
        with tempfile.TemporaryDirectory(prefix="gesta-runner-contract-") as tmp:
            parent = Path(tmp)
            folder = parent / "suite space; touch INJECTED;"
            folder.mkdir()
            for name in ("run.sh", "scenarios.py"):
                shutil.copyfile(HERE / name, folder / name)
            direct = self.run_runner(CHECKOUT, binary="/bin/false", runner=folder / "run.sh")
            self.assertEqual(direct.returncode, 1, direct.stdout + direct.stderr)
            self.assertIn("0/12 bughunter scenarios pass; skipped=0", direct.stdout)
            env = os.environ.copy()
            env["BUGHUNTER_DIR"] = str(folder)
            env["GESTA_BIN"] = "/bin/false"
            gate = subprocess.run(
                [sys.executable, str(CHECKOUT / "bench/gate.py"), "bughunter"],
                cwd=parent,
                env=env,
                stdin=subprocess.DEVNULL,
                capture_output=True,
                text=True,
                timeout=120,
                check=False,
            )
            self.assertEqual(gate.returncode, 1, gate.stdout + gate.stderr)
            self.assertIn("[FAIL] bughunter scenarios", gate.stdout)
            # record() deliberately prints only eight detail lines. The
            # traceback path proves the quoted override reached this runner.
            self.assertIn(str(folder / "scenarios.py"), gate.stdout)
            self.assertFalse((CHECKOUT / "INJECTED").exists())
            self.assertFalse((parent / "INJECTED").exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
