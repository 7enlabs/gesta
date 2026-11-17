# Upgrade plan: what to fix and in what order

**Status: steps 1 to 3 of this plan are implemented in v0.3.0.** The measurements
below are the v0.2.0 baseline that motivated the work; the "Result" section at the
end records what changed and what is still missing.

Every number here was measured, not estimated. The harness is `bench/bench.py`
(baseline) and `bench/replace_git.py` (20 end-to-end capability checks).

Measured 17/11/2026 in container `hermes-workspace`.

## 1. Current state

### Speed is not the bottleneck

| Measurement | Result |
|---|---|
| `changeset create`, 2,000 files | 47 ms |
| `changeset commit`, 2,000 files | 93 ms |
| `status`, scanning 2,000 files | 62 ms |
| `changeset create` when 1 of 2,000 files changed | 64 ms |
| `blame` on a 2,000-line file, history depth 1 | 22 ms |
| `blame` same file, history depth 20 | 14 ms |

Blame does not slow down as history grows: it reads the precomputed `blame` table instead of replaying every version. Speed is not worth optimising yet.

### Disk usage is the real problem

Same workload (a 500-line, 23 KB file, one line edited 30 times):

| System | Size |
|---|---|
| ZenForge | 911 KB |
| git | 102 KB |
| Ratio | **9.0x** |

Cause: every commit of a file writes the **whole file content** into `objects` as TEXT. git compresses and packs deltas. For an agent that edits large files thousands of times a day this cost is not ignorable.

Measured in isolation: 30 edits of one line in a 23 KB file grew the database by 732 KB, about 24 KB per single-line edit — exactly the file size.

## 2. Nine defects, ordered by damage

### P0: data loss, must be fixed before anything else

**Defect 1: merge neither updates the working copy nor detects conflicts**

Reproduction: branch `dev` changes `shared.txt` to "DEV VERSION", branch `main` changes the same file to "MAIN VERSION". Running `branch merge dev --target main` prints:

```
merged dev into main: 1 changeset(s) applied
```

It reports success. But:
- The file on disk still holds "MAIN VERSION", not "DEV VERSION".
- The `main` head already points at the dev changeset.
- `status` afterwards prints `M shared.txt` because the working tree no longer matches the head it just moved to.

The head and the working copy diverge with no warning. An agent that trusts the head and commits again will overwrite or drop the "DEV VERSION" content from history. This is silent data loss, the most dangerous class of bug in a source control system.

Two faults in one place:
- `merge_branch` only rewrites `parent` and `head`; it never writes files to disk.
- There is no conflict check. Two branches editing the same file still "fast-forward" cleanly.

**Defect 2: blame attributes by line content, so duplicates get the wrong author**

Reproduction: agentA writes a file containing two `return None` lines. agentB appends a third identical line. Blame output:

```
cs77dfe4bb52d7 agentA      1 | return None
cs77dfe4bb52d7 agentA      2 | other
cs77dfe4bb52d7 agentA      3 | return None
cs77dfe4bb52d7 agentA      4 | return None   <- agentB wrote this line
```

Line 4 belongs to agentB but is credited to agentA. Real code is full of `return None`, `}`, `import os` and blank lines, so the misattribution rate is high, not rare.

Cause: `recompute_blame` matches on **content** through a `HashSet`, with no notion of position or line identity. That is the direct consequence of choosing the cheap matching strategy.

### P1: wrong results, no data loss

**Defect 3: no way to undo**

`zenforge --help` lists init, status, log, changeset, blame, history, checkout, branch. There is no `undo`, `revert`, `restore`, `reset`, `gc`, or `fsck`.

An agent editing files thousands of times a day with no undo means one bad write has to be repaired by hand or by rebuilding the repository. Jujutsu solves this with an **operation log**: every operation records a snapshot of repository state, which enables `jj undo`, `jj op revert` and `jj op restore` ([jj operation log](https://docs.jj-vcs.dev/latest/operation-log/)). ZenForge has no such layer.

**Defect 4: no `fsck`, so a broken repository goes unnoticed**

Nothing verifies integrity: an object missing for a `changeset_files` row, a `parent` pointing at a changeset that does not exist, a `committed` changeset with no `file_versions`. Those states were produced by the transaction bugs that have since been fixed, so existing repositories may already be corrupt with no way to detect it.

**Defect 5: no three-way merge, merge is fast-forward only**

No merge base is computed. When two branches diverge and touch the same file there is no concept of conflict, only "apply the changeset". The user is never asked, never shown a three-way diff, and has no way to resolve anything.

### P2: operational cost

**Defect 6: objects are never reclaimed**

A changeset created and never committed still leaves its object in `objects`. Measured: 5 pending changesets that were never committed left 5 objects occupying space permanently. There is no `gc`. An agent taking snapshots and discarding them accumulates garbage without limit.

**Defect 7: stores whole content, uncompressed, no deltas**

See section 1. This is the cause of the 9.0x ratio against git.

**Defect 8: no programmatic interface**

CLI only. No HTTP API, no MCP server. Anthropic states one principle behind Scion explicitly: **every query the client uses is also exposed via API**, so CI jobs do not need a local repository ([Scion](https://www.anthropic.com/research/redesigning-source-control)). With ZenForge, a tool that wants to ask "who changed this line" has to shell out and parse text.

**Defect 9: exact path matching, no rename detection**

Renaming a file is recorded as delete-plus-add, losing the file's entire history. git handles this with similar-object detection during diff.

## 3. Four upgrade directions, weighed by cost and payoff

### A: fix the P0 defects first (mandatory)

Scope: merge writes files to disk, detect conflicts, blame by position.

Correct blame needs line matching by sequence, not by set. The cheap approach that is good enough: walk versions oldest to newest, run an LCS (longest common subsequence) between consecutive versions, keep the old owner for lines inside the LCS and assign the current changeset to new lines. Cost is O(n·m) per version pair; for a few thousand lines that is a few million comparisons, acceptable.

Conflicts: compute the merge base with a bidirectional BFS over `parent`, then compare three versions (base, ours, theirs). If both sides changed the same region, report a conflict and stop without writing.

Estimate: 400 to 600 lines of Rust, 8 to 10 new tests. This has to land before ZenForge is used on any real repository.

### B: cut disk usage with compression (best payoff per line of code)

Instead of storing full content, store an operation list against the previous version:

- Keep `objects` for full objects.
- Add `deltas(hash, base_hash, ops)` holding (position, delete, insert) tuples.
- When reading an object not present in `objects`, walk the delta chain back to a base object and reconstruct.
- Periodically fold long delta chains into full objects (like a git pack).

Or the far simpler option: **compress each object with zstd or flate2**. Text compresses 3 to 5x. One `flate2::write::ZlibEncoder` on write, `ZlibDecoder` on read, and change `content TEXT` to `content BLOB`. Estimate: 3 to 4x reduction for about 40 lines of code, with no change to schema logic.

Recommendation: compress first (40 lines, 3 to 4x), then add deltas only if that is still not enough.

### C: operation log for undo

Following jj. Add `operations(id, parent_op, kind, target_id, created_at, snapshot_json)`. Every mutating command also writes one operation row; `snapshot_json` holds pointer state (each branch head, current_branch).

With that, `undo` is just: load the previous operation and restore the pointers. Objects are untouched because they are immutable. This is exactly why jj's undo is cheap: content never moves, only pointers do.

Estimate: 250 lines, 5 tests. High value because agents write wrong things constantly.

### D: MCP server

Wrap the existing commands as MCP tools: `zenforge_status`, `zenforge_changeset_create`, `zenforge_blame`, `zenforge_history`, `zenforge_diff`, `zenforge_log`. Use the `rmcp` crate or speak JSON-RPC over stdio directly.

Payoff: agents call directly without shelling out, without parsing text, with a clear parameter schema. This is the meaningful difference from git: a git MCP server wraps a heavy system, whereas ZenForge wraps SQLite directly.

Estimate: 300 lines. Do it after P0, because wrapping a system that still loses data only spreads the loss faster.

## 4. Recommended order

| Step | Work | Why | Estimate |
|---|---|---|---|
| 1 | Safe merge: write files, detect conflicts, compute merge base | Currently loses data silently | 400 to 600 lines |
| 2 | Sequence-based blame (LCS) instead of content-set matching | Currently misattributes authors | 150 to 200 lines |
| 3 | Compress objects with flate2 | 9.0x down to roughly 2 to 3x vs git | 40 lines |
| 4 | `fsck` integrity check | Detect repositories broken by older versions | 120 lines |
| 5 | Operation log and `undo` | Agents need to reverse wrong writes | 250 lines |
| 6 | `gc` to reclaim orphan objects | Garbage grows without limit | 80 lines |
| 7 | MCP server | Agent integration | 300 lines |

Steps 1 to 3 are the condition for calling ZenForge usable at all. Steps 4 to 7 are what make it good.

## 5. Not yet verified

These are inferences, with no measurement or source behind them:

- Behaviour at 100,000 changesets. Measured only up to 3 changesets and 2,000 files.
- The exact flate2 ratio on real Rust source. Inferred from typical text compression rates.
- Whether LCS stays fast enough for a 50,000-line file. Measured only up to 2,000 lines.
- Whether `rusqlite` with a BLOB column has memory problems for objects of tens of megabytes.
- WAL behaviour with many agents writing concurrently. Tried 8 processes, not 100.

## 6. Sources

- Anthropic, "Redesigning source control at Anthropic" (Scion), https://www.anthropic.com/research/redesigning-source-control
- Jujutsu, "Operation log", https://docs.jj-vcs.dev/latest/operation-log/
- Jujutsu, "Revset language", https://docs.jj-vcs.dev/latest/revsets/
- Pijul, "Theory" (line graph, CRDT), https://pijul.org/manual/theory
- git, "git-merge-base", https://git.github.io/htmldocs/git-merge-base.html
- SQLite, "Recursive common table expressions", https://www.sqlite.org/lang_with.html


## 7. Result after v0.3.0

### Implemented

| Defect | Fix | Evidence |
|---|---|---|
| Merge lost data silently | Real three-way merge: computes the merge base, applies both sides, writes merged content to disk, records a merge changeset with two parents, and refuses to move the head when a file conflicts (their version saved as `<file>.theirs`) | `merge3` unit tests + 2 e2e checks |
| Blame misattributed duplicate lines | Blame now matches line sequences (longest common subsequence) instead of a content set, so an appended duplicate belongs to its author | `blame_does_not_credit_a_duplicate_line_to_an_earlier_author` |
| No undo | Operation log records a pointer snapshot after every ref change; `undo` restores the previous snapshot and deletes that operation so a second undo walks further back | `record_and_undo_restores_the_previous_head`, e2e check |
| No `fsck` | Checks missing objects, dangling parents, branch heads pointing nowhere, and reports orphan objects; `--prune` deletes them | e2e check with a deliberately corrupted hash |
| No `.gitignore` | Gitignore-style matcher: `*`, `**`, `?`, anchored `/`, directory-only `/`, negation `!`, plus `.zenignore` and a database-backed list | 10 glob unit tests |
| Disk usage 9.0x git | zlib compression per object, applied only when it actually shrinks the row (small files stay raw), and the redundant `working_copy` table was deleted because nothing read it | 2.63x on the same workload |
| No remote | `clone`/`push`/`pull` over a zlib bundle written through the real serialise/parse path, with per-object hash verification on receipt | 4 unit tests + 2 e2e checks |
| No `diff` / `show` / `restore` / `tag` | All implemented | e2e checks |
| Deletions not tracked | `changeset create` records deletions; `status` reports them | e2e check |

### Measured after the work

| Metric | v0.2.0 | v0.3.0 |
|---|---|---|
| End-to-end capability checks | 6 of 20 | **20 of 20** |
| Unit tests | 24 | **57** |
| Disk vs git, 21 versions of a 42 KB file | 8.9x | **2.63x** |
| Disk vs git, 1000 small files | 4.32x | **4.32x** (unchanged) |
| `changeset create` 2000 files | 47 ms | 54 ms |
| `commit` 2000 files | 93 ms | 95 ms |
| `status` 2000 files | 62 ms | 45 ms |
| aislop | 94/100 | **100/100** |
| Largest source file | 1,278 lines | **946 lines** |

### Still missing, and why it matters

1. ~~**No network transport.**~~ **Done in v0.3.1.** `zenforge serve` exposes a
   repository over HTTP and `clone`/`push`/`pull` speak it. Eleven checks in
   `bench/http_remote.py` drive a real socket, including a refused rewind, a
   merge across two clones, and a second clone seeing the pushed history.
   Still missing on top of it: TLS and authentication.
2. **Small-file overhead.** 4.32x git for a tree of 1000 tiny files. The cause is
   per-row SQLite and index overhead, not content. Fixing it needs packing
   several small objects into one row, which is a schema change.
3. **No rebase, cherry-pick, bisect, hooks, submodules, LFS, `.gitattributes`.**
4. **Conflicts are not interactively resolved.** You edit the files and commit.
5. **Blame is sequence-based.** A line moved and edited in the same commit loses
   its original author.
6. **No performance data past 3 changesets and 2000 files.** Large-history
   behaviour is still unmeasured.

### Judgement

ZenForge can now serve as the source control for a single developer or a set of
agents working on one machine: every daily operation exists, merges are safe, and
nothing is lost silently. It cannot yet replace git for a team, because there is
no way to get the repository to another machine over a network. Calling it a git
replacement today would be wrong; calling it usable for local agent work is
accurate and now backed by 20 checks.


## 8. v0.3.1: HTTP remote

### What was built

`src/http.rs` is a hand-written HTTP/1.1 server and client on `std::net`. No TLS
stack, no async runtime, no new dependency: the protocol surface is small enough
to test directly.

| Endpoint | Purpose |
|---|---|
| `GET /v1/health` | liveness, answers `ok` without touching a repository |
| `GET /v1/branches` | `name<TAB>head` per line |
| `GET /v1/bundle?branch=X&have=a,b` | zlib bundle of what the caller lacks |
| `POST /v1/bundle?branch=X` | receive a bundle; 409 on a rewind |

### Bugs the network exposed

Each of these worked locally and broke as soon as two repositories talked:

1. **Push did not update the working tree.** The served repository had the
   changeset in its database but not the file on disk, so `status` inside the
   origin directory reported the pushed file as deleted. Fixed by materialising
   the tree after both sending and receiving.
2. **`restore_worktree` only read the head changeset.** A file added by an
   earlier changeset was never written, so a clone of a two-commit repository
   produced one file instead of two. Fixed with `tree_at`, which takes the newest
   version of every path reachable from the head.
3. **The server checked ancestry before applying the bundle.** The incoming head
   does not exist in the receiving database until the bundle is applied, so every
   legitimate push was rejected with "pull first". Fixed by applying first,
   verifying after, and rolling the branch ref back on rejection.
4. **Pull overwrote the local branch head.** `apply_bundle` moved the branch to
   the fetched head, discarding local changesets that had not been pushed, and
   then the merge compared a branch against itself and did nothing. Fixed by
   separating data ingestion from ref movement: pull applies objects without
   touching the branch, points a tracking branch (`origin/main`) at the fetched
   head, and merges that into the local branch.

### Verification

| Suite | Result |
|---|---|
| `bench/http_remote.py` | 11/11 over a real socket |
| `bench/replace_git.py` | 20/20 |
| Unit tests | 67 |
| clippy | 0 warnings |
| aislop | 100/100 |

A three-machine scenario (server, clone B, clone C, then D cloning late) was run
by hand: all four ended with the same file content, clean working trees, and the
full changeset history including the merge commit.
