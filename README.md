# ZenForge

[![repo](https://img.shields.io/badge/github-zencli%2Fzenforge-181717?logo=github)](https://github.com/zencli/zenforge)

Source control for AI agents. Changesets live in SQLite, objects are content addressed and compressed, blame and history are derived on commit. One Rust binary, no daemon.

## Why

Git assumes a human working copy. You clone everything, stage, commit, push. An agent reads one file, writes one file, and does that thousands of times a day, so a full clone is wasted work and a full round trip per edit is wasted time.

ZenForge keeps the repository server side and answers questions about it directly. An agent can commit without ever holding the whole tree.

Anthropic's Scion follows the same idea at monorepo scale.

## Install

```bash
cd zenforge && cargo build --release
# Binary: target/release/zenforge (~3.3 MB)
```

Needs Rust. Dependencies: rusqlite (bundled SQLite), clap, sha2, hex, chrono, flate2.

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
| `init [--bare]` | Create `.zenforge/` with the SQLite database |
| `status` | Branch, HEAD, pending changesets, added/modified/deleted files |
| `diff [--changeset <id>] [--name-only]` | Working tree diff, or one changeset's diff |
| `log [--limit N] [--oneline]` | Changesets, newest first |
| `changeset create <msg> --author <a>` | Snapshot the working tree |
| `changeset commit <id>` | Commit one changeset, move the branch head |
| `changeset commit-all` | Commit every pending changeset in order |
| `changeset apply <id>` | Commit this and all earlier pending changesets |
| `changeset diff <id>` | Line diff for one changeset |
| `changeset list` | Pending changesets |
| `blame <file>` | Which changeset introduced each line |
| `history <file>` | Version chain with hash, time, author |
| `show [<rev>:]<file>` | File content at a changeset or branch |
| `checkout <file \| .> [--changeset <id>]` | Materialise files from history |
| `restore <file \| .>` | Discard uncommitted changes |
| `undo` | Undo the last operation |
| `ops [--limit N]` | Operation log |
| `tag <name> [--list]` | Named pointer to a changeset |
| `branch create/list/switch/delete/merge/info` | Branch operations |
| `remote add/list/push/pull/clone` | Remote sync, filesystem path or `http://` |
| `remote token add <role> <name> [--hashed]` | Generate an access token for serving |
| `remote token list` | Show configured tokens, last four characters only |
| `serve [--bind ADDR] [--port N]` | Serve this repository over HTTP |
| `fsck [--prune]` | Verify integrity, optionally drop orphan objects |
| `stats` | Object counts and compression ratio |
| `ignore add/list/remove` | Ignore patterns |

## Storage

One file: `.zenforge/zenforge.db` (SQLite, WAL mode). No loose objects, no packfiles, no index to rebuild.

| Table | Purpose |
|---|---|
| `objects` | Content addressed blobs, zlib compressed when that shrinks the row |
| `changesets` | id, message, author, created_at, status, parent, parent2 (merges), branch |
| `changeset_files` | Old and new hash per file inside a changeset |
| `file_versions` | Version chain per path (v1, v2, ...) |
| `blame` | Line ownership, recomputed on commit |
| `branches` | name, head changeset, origin, created_at |
| `tags` | name, changeset |
| `operations` | Pointer snapshots for `undo` |
| `ignore_rules` | Patterns added with `ignore add` |
| `remotes`, `remote_refs` | Remote urls and the last head seen per remote branch |
| `repo_state` | Key-value state such as current_branch |

Dedup comes free from the hash: identical content is one row, however many files share it.

Compression is not applied blindly. A zlib stream carries a fixed header, so a short file grows when packed. `put_object` compares the packed size to the raw size and keeps whichever is smaller.

## Design notes

Changesets are explicit. `changeset create` snapshots the tree and `changeset commit` moves the head. An agent can look at what it is about to commit before it lands.

Blame is derived, not written per edit. On commit, `recompute_blame` walks the version chain oldest to newest and matches lines by sequence (longest common subsequence), not by content set. A line that duplicates another line keeps its own author.

Branches are pointers plus reachability. `status` and `latest_hash` walk the parent chain, so a file's current content is the newest version reachable from the head, not simply the newest version in the table.

Merge is real three way. It computes the merge base, applies both sides, writes the merged result to disk, records a changeset with two parents, and refuses to move the head if any file conflicts.

`stdout` is the API. Commands print one human readable line per fact and exit 0 on success, non-zero with `zenforge: <error>`. There is no JSON mode.

## Remote over HTTP

A repository can serve itself and another machine can clone, push and pull over plain HTTP:

```bash
# machine holding the repository
zenforge serve --bind 0.0.0.0 --port 8787

# another machine
zenforge remote clone http://server:8787 path/to/project
zenforge remote add origin http://server:8787
zenforge remote push origin
zenforge remote pull origin
```

Endpoints: `GET /v1/health`, `GET /v1/branches`, `GET /v1/bundle?branch=X&have=...`, `POST /v1/bundle?branch=X`.

A push that would move the server's branch to a head not containing its current head gets a 409 and the branch stays where it was. `https://` is refused with an explanation: terminate TLS in a proxy in front of it.

## Access control

The server has two roles. A read token may clone and pull. A write token may also push. `/v1/health` is open so a monitor can check liveness without a credential.

```bash
# on the serving machine
zenforge remote token add write alice --hashed
# prints the token once; only its digest is stored

# on a client
zenforge remote clone http://server:8787 project --token <token>
zenforge remote add origin http://server:8787 --token <token>
```

Tokens live in `.zenforge/auth`, one entry per line, in either form:

```text
<token>:<role>:<name>              token in clear, hashed when loaded
sha256:<digest>:<role>:<name>      only the digest on disk
```

`--hashed` writes the second form. A leaked file then cannot be replayed, because presenting the digest does not authenticate; only its preimage does. A test pins that, and another pins that a malformed digest line never matches anything.

`.zenforge/` is never tracked by ZenForge, so tokens cannot reach a changeset.

With no tokens configured the server refuses to bind to a non-loopback address. `0.0.0.0` without tokens exits with an error instead of serving an open port. A failed authentication waits 200 ms before answering, which blunts online guessing.

## Verification

```bash
cargo test --release              # 82 unit tests
python3 bench/replace_git.py      # 20 end to end capability checks
python3 bench/http_remote.py      # 11 checks over a real socket
python3 bench/http_auth.py        # 18 checks on tokens and roles
python3 bench/size_compare.py     # disk usage against git
python3 bench/bench.py            # speed baseline
```

What the suites cover, and why each check exists:

| Suite | Covers |
|---|---|
| unit | hashing, compressed object store, id allocation under concurrency, graph reachability across both parents of a merge, merge base, blame by sequence, glob matching, bundle serialisation, HTTP parsing, undo |
| `replace_git.py` | commit, diff, restore, show, deletion tracking, gitignore, disjoint merge, conflicted merge, branch switch, undo, tag, clone, push, fsck, compression, duplicate line blame, empty commit, binary reporting, large history, 8 concurrent snapshots |
| `http_remote.py` | server health, branch listing, clone, ignored files, push, landing on disk, refused rewind, pull with merge, second clone, 404 on unknown branch, https refusal |
| `http_auth.py` | token generation, hashed storage, anonymous refusal, read token cannot push, write token pushes, public bind without tokens refused |

## Performance

Measured in the workspace container, release build.

| Operation | 2,000 files | 5,000 files, 200 changesets |
|---|---|---|
| `changeset create` + `commit` | 54 ms | 117 ms |
| `status` | 45 ms | 54 ms |
| `blame` (201 versions of one file) | 12 ms | 26 ms |
| `log --limit 5` | 14 ms | 14 ms |
| `remote clone` over HTTP | 0.3 s | 0.3 s (5,000 files) |

An earlier version of `status` and `changeset create` asked for the committed hash of each file separately, and each lookup re-walked the whole parent chain. That made both operations quadratic in history depth: on 3,000 files, `status` took 55 ms at one changeset, 878 ms at fifty, and 6.8 s at two hundred. They now fetch the committed tree in one ranked query, so the cost no longer depends on how deep the history is.

Disk usage, 21 versions of a 42 KB file edited one line at a time: 267 KB against git's 101 KB, so 2.63x. Before compression it was 8.9x. A tree of 1,000 tiny files costs 4.32x git, because per-row overhead dominates when each object is a few dozen bytes.

`blame` does not slow down as history grows. It reads the precomputed table instead of replaying versions.

## Limitations

- **No TLS.** Plain HTTP. Put a TLS terminating proxy in front of it before sending credentials over an untrusted network.
- **Tokens are all or nothing per role.** There is no per-branch or per-path permission, and no expiry.
- **No SSH transport.**
- **Small files cost more than git.** 4.32x for a tree of tiny files. Fixing it means packing several objects per row, which is a schema change.
- **Blame is line-sequence based.** A line moved and edited in the same commit loses its original author.
- **No rebase, cherry-pick, bisect, hooks, submodules, LFS, or `.gitattributes`.**
- **Conflicts are resolved by editing files**, then committing the result. There is no interactive resolver.
- **Large histories are still lightly measured.** Behaviour up to 5,000 files and 200 changesets is known. Beyond that, unmeasured.

## Layout

```
src/main.rs        CLI definitions and the dispatcher
src/cli.rs         command handlers
src/cli_remote.rs  remote, fsck, stats, ignore, serve handlers
src/repo.rs        changeset lifecycle, blame, merge, checkout
src/store.rs       schema, compressed object store, graph queries
src/merge.rs       line diff and three way merge
src/remote.rs      bundle format and application
src/http.rs        HTTP server and client
src/ops.rs         operation log and undo
src/worktree.rs    ignore matching and file scanning
```

Build environment: Docker container `hermes-workspace`, Rust 1.100.0, image `hermes-workspace-rust:latest`.

The source lives on the host at `D:/Hermes/zenforge/` and is mounted into the container at `/work/zenforge`, so recreating the container does not lose it.

## Correctness guarantees

Each of these is pinned by a test, and each one exists because the first implementation got it wrong.

- **Ids are unique.** Allocation mixes a SQLite-side counter (`RETURNING` on an upsert) with the message, author and a sequence, so concurrent agents writing the same message in the same second still get distinct ids.
- **Commits are atomic.** File versions, blame, the changeset status and the branch head move inside one transaction. A missing object aborts the whole commit.
- **History does not inflate.** A snapshot matching the recorded content adds no version, and version numbers stay contiguous.
- **No changeset is orphaned.** A pending changeset is re-parented onto the live head at commit time, so two snapshots taken while the branch was empty still chain instead of forking.
- **Diffs keep order and repeats.** `line_diff` works on an alignment, so duplicates are reported where they occur.
- **Binary files are reported, not dropped.** They show up in `status` and in `changeset create` output.
- **SQL errors propagate.** No `unwrap_or` masks a failed query.
- **Unattributed lines are marked.** `blame` prints `?` and a warning instead of inventing an owner.
- **A push cannot rewind a branch.** The server applies the bundle, checks that the incoming head contains the previous one, and rolls the ref back if it does not.
- **A pull cannot delete local work.** Fetching applies objects without touching the branch, then merges a tracking branch into the local one.

## Status

Version 0.3.1. Usable for one developer or a set of agents on a machine or a trusted network. Not yet a full git replacement: no TLS, no auth, and small files cost more disk.

`docs/upgrade-plan.md` holds the measurements behind each decision and the list of what is still missing.
