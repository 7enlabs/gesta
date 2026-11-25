#!/usr/bin/env python3
"""Measure peak RSS of `gesta status` on a repository holding one large binary.

The bug (#115) is that `read_file` called `fs::read` and then looked at the first
8000 bytes, so a read-only status paid for the whole file. This drives the real
CLI and reads the kernel's peak-RSS counter, so the number is measured rather
than argued. It runs against whatever binary it is given, which is what makes it
usable both as a gate step and as proof that reverting the fix makes it fail.

Usage: walk_memory.py <path-to-gesta-binary> [size_mib]
Exit 0 when peak stays well under the file size, 2 when it does not.
"""
import os
import re
import shutil
import subprocess
import sys
import tempfile

DEFAULT_MIB = 256


def build_repo(root, size_mib):
    """A repository with one large binary whose first byte is NUL."""
    repo = os.path.join(root, "repo")
    os.makedirs(repo, exist_ok=True)
    big = os.path.join(repo, "big.bin")
    with open(big, "wb") as f:
        f.write(b"\x00\x01\x02\x03")
        chunk = b"A" * (1 << 20)
        for _ in range(size_mib):
            f.write(chunk)
    with open(os.path.join(repo, "small.txt"), "w") as f:
        f.write("hello\n")
    return repo


def peak_rss_kib(binary, repo):
    """Run `status` and return its peak RSS in KiB, from the child's own rusage.

    `/usr/bin/time` is not installed in this image, so the number comes from
    `os.wait4`, which reports `ru_maxrss` for the child - the kernel's high-water
    mark for that process, in KiB on Linux.
    """
    pid = os.fork()
    if pid == 0:  # child
        try:
            os.chdir(repo)
            devnull = os.open(os.devnull, os.O_WRONLY)
            os.dup2(devnull, 1)
            os.dup2(devnull, 2)
            os.execv(binary, [binary, "status"])
        finally:
            os._exit(127)
    _, status, usage = os.wait4(pid, 0)
    if not os.WIFEXITED(status):
        return None, f"child did not exit cleanly: status {status}"
    return usage.ru_maxrss, None


def main():
    if len(sys.argv) < 2:
        print("usage: walk_memory.py <gesta-binary> [size_mib]")
        return 1
    binary = sys.argv[1]
    size_mib = int(sys.argv[2]) if len(sys.argv) > 2 else DEFAULT_MIB
    if not os.path.exists(binary):
        print(f"no binary at {binary}")
        return 1
    root = tempfile.mkdtemp(prefix="walkmem-")
    try:
        repo = build_repo(root, size_mib)
        subprocess.run([binary, "init"], cwd=repo, capture_output=True, timeout=180)
        peak, err = peak_rss_kib(binary, repo)
        if peak is None:
            print(f"FAIL no measurement: {err}")
            return 1
        peak_mib = peak / 1024
        print(f"peak_mib={peak_mib:.1f}")
        print(f"file_mib={size_mib}")
        # A whole-file read is at least the file size resident. A prefix read is
        # bounded by the binary's own footprint. 40% is a generous line: well
        # below the file and well above any prefix-only run.
        limit = size_mib * 0.4
        if peak_mib > limit:
            print(f"VERDICT=reads_whole_file ({peak_mib:.1f} MiB > {limit:.0f} MiB)")
            return 2
        print(f"VERDICT=prefix_only ({peak_mib:.1f} MiB <= {limit:.0f} MiB)")
        return 0
    finally:
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
