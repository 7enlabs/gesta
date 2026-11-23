# Corrections to an external review

A review of this project circulated on 23 November 2026. Most of it holds up, and the storage,
speed and parity figures it quotes are the ones this repository publishes. Three of its statements
were out of date, and one number it repeats is a stale entry in a file here. Recording the
corrections where the code is, rather than in the review, because the next reader of either will
arrive from a search.

## Out of date in the review

**"Chưa verify push GitHub.com" and "Forge push chỉ hỗ trợ một ref" and "Chưa hỗ trợ multi-ref
forge push".** All three were true before `a07250e`. Pushing to github.com now runs against the real
forge and is read back by `git clone` on a machine that shares no code with this one; `--all` exists
and is wired to `push_all`.

Three defects made the first real push fail, and all three were in how the server answers rather
than in how git is spoken: GitHub advertises `report-status-v2` and not plain `report-status`, it
chunks both responses so a pkt-line reader takes `14C\r\n` for a packet length, and a push of a ref
already at that id is refused rather than skipped. `docs/forge.md` says so and names them.

The review's own list is careful enough to have caught this if it had read `docs/forge.md`, which
says "Pushing to github.com" and no longer contains the phrase it quotes. That is the failure mode
of quoting a document from memory instead of from the file.

**"Chưa có worktree" in the parity list.** `gesta worktree add`, `list` and `remove` exist and
work; the bench for them writes two trees from one store and reads both. The review is looking at
`docs/git-parity.md`, where worktrees appear under what is missing for *interoperability with git*,
which is a different claim: a git client cannot see this repository's worktrees, and this
repository's worktrees are not git worktrees.

**"13 cửa ải kiểm soát" for `bench/gate.py`.** There are 114 named steps, each holding several
checks, and the run reports 365 individual checks. The number 13 appears nowhere in the file.

## Wrong here, and the review repeated it

The review's disk and speed tables are accurate: `19.9 ms` against `4.8 ms`, `214.5 ms` against
`58.1 ms`, `11,301` files at `37.4 MB` of pack against `51.4 MB` of database. Those numbers come
from `docs/speed.md` and `docs/git-parity.md` and they are current.

The conflict is inside `docs/git-parity.md`. Its 100,000-file table says `scan` is `454.7 ms`, and
thirty sections later the same measurement is `214.5 ms` in a run recorded twice an hour apart. One
of the two is wrong. The second is the one with the reasoning attached: it names what the first run
measured by hand and why the hand-assembled figures disagreed with each other.

Which of them is right has not been established here. Rebuilding the 100,000-file fixture and
running `bench/status_vs_git.py` against it is the way to settle it, and that has not been done. The
honest state is that the document contradicts itself and the contradiction is known.

## What the review got right that this project had not written down

The review puts the gap in the right place. It is not the git protocol, and it is not the storage
engine: it is that this store has a single writer process and no distributed metadata, so nothing
here scales by adding machines. That is a design choice with a written refusal in `decisions.md`,
and the review states it as a limitation rather than as a defect, which is the accurate reading.

Two of its other findings were worth acting on and were:

- **Seven commands refused with the argument parser's message.** `gesta worktree add` said
  `required arguments were not provided: <PATH>` and stopped there, which names a token and not the
  shape. All seven now name the form: `gesta worktree add ../other feature`. The pattern the project
  already had in `gesta tag` was applied to `worktree add`, `worktree remove` and the five `branch`
  subcommands that take a name, and the `main.rs` arm that duplicated the branch dispatch was
  deleted rather than edited in two places.
- **`gesta worktree add` with one argument.** It reported the missing `<BRANCH>` and nothing else,
  so a reader who had guessed the order correctly was still left to guess the second half. It now
  says `worktree add needs a branch as well as a path: gesta worktree add ../x <branch>`, carrying
  the path they typed.

Seven was the number this pass checked. The full command surface has not been swept, so the count
of commands still refusing with a bare parser message is unknown and probably not zero.

## What was found while checking the review

The review's claim that this project is agent-native and single-node is right, and both are already
written down. What neither the review nor this project had noticed is a defect with the same origin
as the single-node design: **`serve` ignored SIGTERM and SIGINT entirely.**

There was no signal handler and the accept loop blocked in `accept`, so a `kill -TERM` on a running
server did nothing. In a terminal that is invisible, because Ctrl-C then closes the session and the
process goes with it. Under a harness it is a leak: every bench that starts a server and calls
`terminate()` leaves it running, because the signal is ignored and the process outlives its parent.

The container had accumulated **2,209** of them from 20 November onward, holding **12.1 GB** resident
and every port each had bound. The first symptom noticed was a bench failing to bind a port that
nothing should have been using.

Two changes, both verified by observation rather than by a passing test:

- `src/transport/signal.rs` installs `sigaction` for `SIGTERM` and `SIGINT`; the handler does one
  relaxed atomic store and nothing else, because that is all that is safe in signal context.
- The accept loop sets the listener non-blocking and polls the flag every 100 ms. A blocking `accept`
  cannot be interrupted by a flag, and a 100 ms poll costs an idle server one wakeup per tick.

Measured: before, `kill -TERM` on a running server left it alive and a bench that starts two servers
left two behind; after, the server exits within the poll interval, prints
`stopping: a stop signal was received`, and the same bench leaves zero. There is now a gate step,
`serve stops on SIGTERM`, which starts a real server, sends a real signal and waits, because a check
that asserts a flag exists would pass with the handler never installed.

The earlier bug in the same area was in the benches: nine of them started the server with
`Popen(..., shell=True)`, so `terminate()` killed the shell and orphaned the server. That was fixed
too, but fixing only that would have hidden the real cause, since a server that cannot be stopped is
broken whether or not its parent is polite.

## What was not done

The review's first recommendation, a distributed metadata and object plane, is refused in
`decisions.md` for the reason that review gives: the store is one SQLite file by design. The second,
a mid-size monorepo measurement beyond 10,000 files, would settle the contradiction above and has
not been run.
