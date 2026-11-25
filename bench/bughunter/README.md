# Local bughunter regression scenarios

## Scope and provenance

This is a newly authored, repository-local regression suite. It is not a recovered
copy of the unavailable external `hermes-health/sec/bughunter` bundle, and does not
claim to implement an unspecified upstream certification standard.

The target is the team's local gesta binary. Every scenario creates its own
repository, isolated home/config directories, and synthetic outside-boundary
canary under one temporary directory. The supplied checkout is used only to locate
the binary. No remote targets, network requests, actual credentials, or user
repositories are part of these scenarios. Temporary fixtures are removed even
when assertions fail.

## Runner contract

Requirements: Bash, Python 3 standard library, a POSIX container with symlink and
hardlink support, and a built release binary. No extra packages are installed.

```bash
source "$HOME/.cargo/env"
cargo build --release
bash bench/bughunter/run.sh /work/gesta
python3 bench/bughunter/test_runner.py
python3 bench/gate.py bughunter
```

`run.sh` requires exactly one checkout path. The default executable is
`<checkout>/target/release/gesta`; `GESTA_BIN` is an explicit operator override.
Each product subprocess uses an argument array, closed stdin, a restricted
environment, and a 60-second timeout. It does not inherit user credentials/config.
The runner prints the tested executable's SHA256.

Exit codes:

- 0: all discovered scenarios ran and passed, with no skipped cases.
- 1: at least one assertion, process timeout, setup error, or scenario error.
- 2: invalid invocation or unavailable prerequisites.

Zero discovered scenarios cannot pass. The separate runner-contract tests verify
that always-successful and always-failing dummy executables cannot masquerade as
a passing product, and that shell tokens in suite paths remain literal.

The gate defaults to this directory. `BUGHUNTER_DIR` still overrides the suite
location. A missing override remains an explicit SKIP and blocks release under the
existing gate policy; an existing but broken runner fails rather than passing.

## Covered invariants

1. A file symlink snapshot stores the link, not outside file contents.
2. A directory symlink snapshot does not walk outside the repository.
3. File checkout does not overwrite an external symlink target.
4. File checkout does not overwrite a file sharing an external hardlink.
5. A symlink in a checkout parent directory cannot redirect writes outside.
6. Parent traversal in restore cannot discard unrelated edits or alter a canary.
7. A scoped snapshot cannot include a parent path or stage unrelated edits.
8. An unknown checkout revision cannot silently restore the current branch.
9. Repository metadata is excluded from snapshots, with a positive file control.
10. An invalid backup is rejected without replacing the current store.
11. A valid backup round trip restores the original history and file contents.
12. Shell metacharacters in filenames remain literal through snapshot and checkout.

`--path` is documented in `create_changeset_scoped` as a repository-path filter.
An unmatched outside filter may safely return a no-op. That is not classified as
a path-traversal vulnerability. Assertions check unchanged history, pending
changesets, unrelated edits, and outside content instead of requiring an
undocumented nonzero exit.

## Verified baseline and open findings

Baseline source: main at `84e3692`, with pre-existing local changes. This is a
working-tree measurement, not a claim about a pristine commit or remote release.
Tested release binary SHA256:

```text
8d2dc7328802f87783860cd043500393da87f8783cccbce8446d25285a781eec
```

Observed results: 8/12 product scenarios pass, 4 fail, 0 skip. The focused gate
returns 1 and reports `bughunter scenarios` as FAIL. The runner-contract suite
passes 5/5. Rust release tests pass 644/644; formatting and strict release clippy
checks return 0. The full release gate has not been run for this addition.

### Outside-boundary overwrite: three failing scenarios

Reproduce by running the suite. Each failing checkout scenario first commits a
normal tracked file, then substitutes either an external file symlink, an external
hardlink, or a symlinked parent directory, and calls `checkout` on the tracked
path. The external canary changes from `OUTSIDE-BOUNDARY-CANARY` to the committed
file contents. The assertion compares bytes, not just exit status.

Impact: with the fixture's filesystem permissions and substituted link in place,
checkout overwrites a file outside the repository. These tests do not establish a
remote exploit, privilege escalation, or a CVSS score. The cases expose related
write-boundary failures, not three proven independent root causes.

Repair direction: validate path components and prevent link-following escapes;
replace the destination file safely instead of truncating an existing inode.
A leaf-only symlink check is insufficient for symlinked parents and hardlinks.
Keep these assertions unchanged when repairing the product.

### Unknown whole-tree checkout revision: one failing scenario

The fixture edits `tracked.txt`, then runs:

```bash
gesta checkout . --changeset missing-bughunter-revision
```

The command returns 0 and reports a restored file. `src/cli/tree.rs::cmd_checkout`
handles `.` before consulting the supplied changeset, so the requested revision is
ignored. The regression requires a refusal that leaves the edit and history
intact. This is a CLI correctness/data-preservation finding, not a demonstrated
remote security vulnerability.

Repair direction: validate and honor the revision for whole-tree checkout, or
explicitly refuse the unsupported argument combination before restoring files.

Product repairs are intentionally not bundled into this runner addition. A red
result is evidence to retain, not an invitation to skip cases or relax assertions.
