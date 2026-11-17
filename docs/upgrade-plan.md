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

Measured in isolation: 30 edits of one line in a 23 KB file grew the database by 732 KB, about 24 KB per single-line edit, which is exactly the file size.

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
   Access control landed in v0.4.0: read and write tokens, hashed storage
   option, refusal to serve a public address without tokens. Still missing: TLS.
2. **Small-file overhead.** 4.32x git for a tree of 1000 tiny files. The cause is
   per-row SQLite and index overhead, not content. Fixing it needs packing
   several small objects into one row, which is a schema change.
3. **No rebase, cherry-pick, bisect, hooks, submodules, LFS, `.gitattributes`.**
4. **Conflicts are not interactively resolved.** You edit the files and commit.
5. **Blame is sequence-based.** A line moved and edited in the same commit loses
   its original author.
6. **Performance is measured to 5,000 files and 200 changesets.** Beyond that it
   is unmeasured. The quadratic path in `status` and `changeset create` was found
   and fixed at this size: 3,166 ms per commit dropped to 117 ms.

### Judgement

ZenForge can serve as the source control for a single developer or a set of
agents on one machine or on a trusted network. Every daily operation exists,
merges are safe, and nothing is lost silently. A second machine can clone, push
and pull over HTTP.

It still cannot replace git for a team outside a trusted network. There is no
TLS and no authentication, so anyone who can reach the port can push. Small file
heavy trees cost 4.32x git in disk. Both are real blockers for shared use, and
neither is a small change.

Calling it a git replacement today would be wrong. Calling it usable for agent
work on a trusted network is accurate, and that claim now rests on 67 unit tests,
20 end-to-end checks and 11 checks over a real socket.


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


## 9. v0.4.0: access control

### What was built

`src/auth.rs` holds a token store and two roles. `remote token add <role> <name>`
generates 32 random bytes from the OS entropy source and writes the entry to
`.zenforge/auth`. Without a token file the server refuses to bind anywhere except
loopback, so an open port is never the accidental default.

### The design choice that mattered

Storing tokens in clear in the auth file is convenient and wrong: whoever reads
the file gets working credentials. The first version did exactly that, and a test
proved it by presenting the file's contents and getting a 200.

The file now accepts a second form, `sha256:<digest>:<role>:<name>`, written by
`--hashed`. Verifying it is the same comparison the clear form already needed,
since entries are hashed at load either way. The difference is that a leaked file
contains no usable credential: presenting the digest itself is rejected, and only
the preimage authenticates. Two tests pin this, one on the happy path and one that
the digest cannot be used as a token.

### Verification

| Check | Result |
|---|---|
| `bench/http_auth.py` over a real socket | 18/18 |
| `bench/http_remote.py` | 11/11 |
| `bench/replace_git.py` | 20/20 |
| Unit tests | 82 |
| clippy, aislop | clean |

The auth suite covers anonymous refusal, a wrong token, a read token cloning but
not pushing, a write token pushing and the file landing on disk, a public bind
without tokens being refused, and a loopback bind without tokens still working.

### Still missing

TLS. Credentials travel in clear over plain HTTP, so the proxy is not optional
beyond a trusted network. Beyond that: no per-branch permissions, no token
expiry, no revocation command, and no audit log of who pushed what.

## 10. v0.4.1: agent interface, stat cache, concurrent writes

The Scion comparison named five gaps. Three are now closed, one is half closed.

### Closed

| Gap | What was built |
|---|---|
| stdout as the only interface | `--json` on every read command, plus `zenforge mcp` speaking JSON-RPC |
| no way to write without a working copy | `POST /v1/changeset`, blobs written straight into the store |
| I/O cost grows with tree size | `worktree_cache`, keyed on `(mtime, size)` |
| write contention under parallel agents | `with_retry` around every mutating command |

### Why the stat cache is shaped that way

The first version had the right columns and the wrong loop: it deleted the whole cache on every scan, so it never served a hit. The second version cached, but still hashed every file, which is the cost the cache exists to avoid. The scan now compares `(mtime, size)` before reading a file, and re-hashes anything it does read, so a stale row cannot hide a change. On 8,000 files with one file edited, 7,999 paths come from the cache and one is re-read.

The residue is honest: a file rewritten within the same second at the same size looks unchanged to the stat check. git has the same weakness in its index, and a changeset creation re-hashes everything it records, so the window only affects what `status` reports, never what a commit contains.

### Why concurrent commits failed

`busy_timeout` was set to 10 s and looked sufficient. It is not, for this case. A reader waiting on a writer is queued by the timeout, but two writes starting at once return `SQLITE_BUSY` immediately once a transaction has begun reading. Twenty parallel `changeset commit` calls produced one success and nineteen `database is locked`.

The fix is not a longer timeout. `store::with_retry` catches only `DatabaseBusy` and `DatabaseLocked`, pauses 5 ms, doubles the pause up to 250 ms, and gives up after 60 s. Anything else propagates at once, so a real SQL error is never retried into a loop. Twenty parallel commits now all succeed, and `fsck` reports a consistent repository afterwards.

### The defect that came out of the remote changeset work

`hash_at_point` read `file_versions`. A deletion writes a row in `changeset_files` with no hash and no row in `file_versions` at all, so the lookup walked past the deletion and returned the last version before it. A deleted file came back. `tree_at` already read `changeset_files` for exactly this reason; `hash_at_point` now does too, and a regression test pins it.

The second defect was the same shape. `changeset_from_content` compared new content against `committed_tree`, the head tree, so a file written into a pending changeset was invisible to the next call. Deleting it answered "nothing to delete". `effective_tree` projects the pending changesets onto the head in creation order, and that projection is now the base for both edits and deletes.

### Verification

| Check | Result |
|---|---|
| `cargo test --release` | 129 |
| `bench/replace_git.py` | 20/20 |
| `bench/http_remote.py` | 11/11 |
| `bench/http_auth.py` | 18/18 |
| `bench/mcp_json.py` | 14/14 |
| `bench/api_changeset.py` | 13/13 |
| `bench/concurrency.py` | 8/8 |
| clippy | 0 warnings |
| aislop | 88/100, 3 warnings, all `.unwrap()` inside a `#[cfg(test)]` fixture |

### The third defect, found by the concurrency suite

`show main:f19.txt` answered "f19.txt does not exist at main" for a file that was
in the head tree, on a repository where the database plainly listed all twenty
paths. The revision argument was passed into a function that only walks changeset
ids, and a branch name is not one. `resolve_rev` now accepts a branch, a tag or a
changeset, and the CLI, the HTTP file endpoint and the MCP read tool all use it.

The lesson generalises: every caller that takes a revision was written against
whichever kind the author had in mind that day. One resolver, used everywhere,
removes the class.

### Still missing

TLS. Delta compression, so a 500 KB file edited fifty times still stores fifty compressed copies. Sparse checkout, so a monorepo can expose one directory. Conflict output as structured JSON with line numbers, so an agent can resolve without parsing markers. Token expiry and revocation.
