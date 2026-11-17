# ZenForge

Source control for AI agents. Server-side changesets, content-addressed objects, derived blame/history. Rust + SQLite, single binary.

## Why

Git assumes a human working copy: clone the whole repo, stage, commit, push. Agents don't work that way — they read a file, write a file, repeat, thousands of times a day. ZenForge stores snapshots server-side and derives blame/history on commit, so an agent never needs a full clone.

Inspired by Anthropic's Scion design.

## Install

```bash
# Requires Rust (build once)
cd zenforge && cargo build --release
# Binary: target/release/zenforge  (~2.8 MB)
```

## Quick start

```bash
cd my-project
zenforge init
zenforge changeset create "Add feature" --author agent
zenforge changeset commit cs1a2b3c4
zenforge log
zenforge blame src/main.rs
```

## Commands

| Command | Description |
|---|---|
| `init` | Create `.zenforge/` with SQLite database |
| `status` | Branch, HEAD, pending changesets, working-tree diff |
| `log [--limit N]` | Changesets newest first |
| `changeset create <msg> --author <a>` | Snapshot working tree as pending changeset |
| `changeset commit <id>` | Commit one changeset, update branch head |
| `changeset apply <id>` | Commit this and all earlier pending changesets |
| `changeset diff <id>` | Line-level diff for a changeset |
| `changeset list` | Pending changesets |
| `blame <file>` | Which changeset introduced each line |
| `history <file>` | Version chain with hash, time, author, message |
| `checkout <file> [--changeset <id>]` | Write a file back from a changeset |
| `branch create <name> [--from <b>]` | Create branch at another branch's head |
| `branch list` | All branches, current marked `*` |
| `branch switch <name>` | Change current branch |
| `branch merge <src> --target <dst>` | Fast-forward target with source's unreached changesets |
| `branch info <name>` | Head, origin, reachable changeset count |

## Storage

Everything lives in `.zenforge/zenforge.db` (SQLite, WAL mode). No `.git`-style loose objects, no packfiles.

| Table | Purpose |
|---|---|
| `objects` | Content-addressed blobs (SHA-256 → text) |
| `changesets` | id, message, author, created_at, status, parent, branch |
| `changeset_files` | Per-file old/new hash within a changeset |
| `file_versions` | Version chain per path (v1, v2, …) |
| `blame` | Line ownership, recomputed on commit |
| `working_copy` | Last committed content per path |
| `branches` | name, head changeset, origin, created_at |
| `repo_state` | Key-value (current_branch, created_at) |

Dedup is free: identical content produces the same SHA-256, so only one object row exists no matter how many files or versions share it.

## Design notes

**Changesets are explicit.** `changeset create` snapshots the working tree; nothing is committed until `changeset commit`. This lets an agent inspect what it is about to commit before it lands.

**Blame is derived, not stored per write.** On commit, `recompute_blame` walks the version chain oldest → newest; the first version containing a line owns it. O(versions × lines) per commit, negligible for agent-scale repos.

**Branches are pointers with reachability.** A branch is a head changeset plus a parent chain. `status` and `latest_hash` walk that chain, so a file's "current" content is the newest version reachable from the branch head — not simply the newest version overall.

**stdout is the API.** Every command prints a human-readable line and exits 0 on success, non-zero with `zenforge: <error>` on failure. Agents parse text; no JSON mode yet.

## Tests

```bash
cargo test --release
# 11 tests: hashing, object store, idempotent commit, branch reachability,
#           blame creation, file scan exclusions, path normalisation
```

There is also a 13-step end-to-end script covering init → changeset → commit → blame → history → diff → branch → checkout → merge → status.

## Limitations

- Text files only. Binary content is skipped silently.
- Merge is fast-forward only; no three-way merge, no conflict resolution.
- Single-node. SQLite is the server; no replication.
- No authentication or authorization. Assume a trusted local filesystem.
- `unwrap_or(1)` on the next-version query means a clustered failure would silently restart version numbering; not hit in practice.
- Blame matches identical lines, not line positions — a moved-but-unchanged line stays attributed to its original changeset.

## Layout

```
zenforge/
├── Cargo.toml
├── src/main.rs        # ~1100 lines incl. tests
├── target/release/zenforge
└── README.md
```

Build environment: Docker container `hermes-workspace` (`D:/Hermes/docker/hermes-workspace/`), Rust 1.100.0, image `hermes-workspace-rust:latest`.

The code lives on the host at `D:/Hermes/zenforge/` and is mounted into the container at `/work/zenforge`, so destroying the container does not destroy the project.

## Correctness guarantees

These are the behaviours the test suite pins down. Each one exists because the
first implementation got it wrong.

- **Ids are unique.** Allocation mixes a SQLite-side monotonic counter
  (`RETURNING` on an upsert), so concurrent agents creating the same message in
  the same second still get distinct ids.
- **Commits are atomic.** File versions, the working copy, blame, the changeset
  status and the branch head all move inside one transaction. A missing object
  aborts the whole commit; nothing is left half written.
- **History does not inflate.** Committing a snapshot that matches the recorded
  content adds no version. Version numbers stay contiguous (1, 2, 3), not spaced
  by failed writes.
- **No changeset is ever orphaned.** A pending changeset is re-parented onto the
  live branch head at commit time, so two snapshots taken while the branch was
  empty still chain instead of forking.
- **Diffs keep order and repeats.** `line_diff` is multiset-based: it reports
  duplicates and preserves the original line order rather than collapsing them
  through a `HashSet`.
- **Binary files are reported, not dropped.** A file that is not UTF-8 shows up
  in `status` and `changeset create` output, so nothing disappears silently.
- **SQL errors propagate.** No `unwrap_or` masks a failed query; a broken object
  store surfaces as an error instead of an empty file or a restarted counter.
- **Every output has a source.** `blame` prints `?` and a warning for lines it
  cannot attribute instead of inventing an owner.

## Layout

```
src/main.rs       CLI surface and command handlers
src/repo.rs       changeset lifecycle, blame, line diff, merge
src/store.rs      SQLite schema, object store, branch reachability
src/worktree.rs   file scanning and UTF-8 reads
```

24 unit tests, `cargo clippy` clean, `aislop` 100/100.

## Known gaps

`docs/nang-cap.md` holds the measured upgrade plan. `bench/bench.py` reproduces every number in it. The headline findings on v0.2.0:

| Gap | Measured |
|---|---|
| `branch merge` moves the head but does not write files to disk, and never raises a conflict | head and working copy diverge silently |
| `blame` attributes by line content, so duplicates get the wrong author | a line added by agentB is credited to agentA |
| Stores full file content per version, no compression | 9.0x the disk of git on the same history |
| No `undo`, `revert`, `gc`, `fsck` | none of them exist |
| Pending changesets never release their objects | no garbage collection |

Fix order: safe merge, then sequence-based blame, then compression, then `fsck`, then an operation log for `undo`, then `gc`, then an MCP server.

## Measuring

```bash
docker exec -i hermes-workspace bash -c 'cd /work/zenforge && python3 bench/bench.py'
```

Run it after a change to confirm the metric actually moved.
