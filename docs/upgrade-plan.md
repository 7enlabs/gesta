# Upgrade plan: what to fix and in what order

Every number here was measured on v0.2.0 (24 tests passing, clippy clean, aislop 100/100). The harness that produced them is `bench/bench.py`; run it again after a change to confirm the metric actually moved.

Measured 17/11/2026 in container `hermes-workspace`, binary `target/release/zenforge` sha256 `279dbfa7…`.

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
