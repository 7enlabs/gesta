# Contributing

## Before you start

This project is used daily by its author before it is offered to anyone else, so the bar is
"does it survive a working day", not "does it compile". Read `docs/git-parity.md` for what works
and what is deliberately absent; a pull request that adds a feature listed as absent is welcome,
and a pull request that claims a feature works without a check behind it is not.

## Building

Rust stable. The build needs a C toolchain for the bundled SQLite.

```bash
cargo build --release
cargo test --release
cargo clippy --release --all-targets -- -D warnings
```

`cargo clippy -- -D warnings` is stricter than the gate's own warning count, and it is what CI runs
on Windows. A variable that is only used inside a `#[cfg(unix)]` block fails there and nowhere else.

There is a second build without the optional features, and it has to keep compiling:

```bash
cargo build --release --no-default-features
```

## The gate

One command decides whether a change is acceptable. Run it, do not run the steps you remember:

```bash
python3 bench/gate.py            # every step
python3 bench/gate.py git-push   # one step, by name
```

Exit codes are not two-way: `0` pass, `1` fail, `2` a step name that does not exist (nothing was
checked, so nothing passed), `3` the checks that ran passed and at least one did not run. A `3` is
not a pass.

Run a new step on its own before adding it to the full run. A step that raises inside itself aborts
the run after every earlier step has already spent its minutes, and the traceback points at a line
in the harness rather than at the defect.

## What a change needs

- **A check that fails without the change.** For anything a user or a client can observe, prove it
  by running the real thing. `git clone` from the server is checked by a real `git clone`, not by
  reading our own response back.
- **A measurement, if the change is about cost.** Comparisons are only meaningful when both sides are
  at their best and produce the same shape of output. Two rounds of this project's own history were
  spent on comparisons that measured the measurement.
- **English.** Code, comments, commit messages, documentation and issue text. Vietnamese is for
  conversation, not for the repository.
- **No other product by name.** Describe the behaviour rather than who does it.

## Commits

One change per commit, with a message that says what was wrong and why the fix is the right one. A
message that only names the file does not help the person reading the history in a year.

Do not create a release or a tag. Commits and pushes are fine; a release is a claim that the
software is ready for other people, and that claim is not true yet.

## Secrets

Nothing that looks like a credential goes into a commit. A scanner refuses the commit and writes
nothing, which is the intended behaviour. If a fixture is genuinely not a credential, add it to
`.secret-allowlist` at the top level with a comment saying what it is and why it is not one.

Do not add an entry that covers a real key. An allowlist that hides a credential is worse than no
scanner, because the project then believes it is being checked.

## Licence of a contribution

The project is MIT, held by 7EN Labs. By sending a change you agree it is licensed under the same
terms, and that you have the right to license it.

## Reporting a problem

Open an issue with the command you ran, what you expected and what happened. For anything that looks
like a security defect, read `SECURITY.md` first and do not open a public issue.
