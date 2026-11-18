# What still separates this from git

Written after replacing git in daily work on this machine, with each claim checked by running
something rather than by reading the code. The order is by what blocks the most.

## Closed in this round

- **Binary files.** They were refused outright, so any repository with an image was unusable.
  Stored raw now, carried in bundles, restored byte for byte.
- **Packfiles.** `git gc` moves every object into a pack and every branch into `packed-refs`.
  Both are read now, so importing a real repository works.
- **Signing.** Ed25519 over the changeset identity, with the private key refusing to be Debug so a
  panic cannot print it.
- **Listing the tree.** `ls`, which did not exist and is the first thing any caller needs.

## Not closed, in the order they block

### 1. Disk: 10.2x git on a markdown vault

Measured on 11,062 files: 550,338,560 bytes against git's 54,179,187. This is the worst number in
the project and the one that decides whether a large repository is practical.

The measurement and the failed fix are in `docs/storage.md`. The short version: packing small
objects into one row cost more than it saved, because a pointer is a hash, two integers and a row
header. The cost sits in six tables that each hold a row per tracked path, so the fix is **fewer
rows**, not smaller ones, and every option costs a capability:

- One row per path instead of per version: loses the ability to ask about any older version.
- A serialized tree per branch, the way git does: loses per-path queries.

Neither is done. Until one is, the honest number stays 10.2x.

### 2. `.gitattributes` is ignored

A `*.txt text eol=lf` file has no effect. On a mixed team this is not cosmetic: a Windows checkout
and a Unix checkout of the same file differ in every line ending, so every commit looks like it
touched the whole file. A minimal `text` and `eol` is a few hundred lines and a large reduction in
false diffs.

### 3. No SSH transport

A remote is a filesystem path, an HTTP server, or S3. An SSH remote would need the protocol and key
handling. `git push` covers it in the meantime by shelling out, which is honest but depends on the
git binary being present.

### 4. Long histories are lightly measured

Behaviour is known to 5,000 files and 200 changesets. The vault run reached 11,000 files and one
changeset. Nothing has been tested at 50,000 commits, and the per-query `ancestors_bfs` is the
first thing that would hurt.

### 5. No interactive conflict resolver

Conflicts are data: both sides, line numbers, context, all three whole files. An agent writes the
answer. A human is handed conflict markers. That is a deliberate design choice, not a gap, but it
is a difference from git that a person will notice.

### 6. What git has that nothing here replaces

- **Hooks ecosystem.** There are pre and post commit hooks, not the full set, and no `.git/hooks`
  compatibility.
- **Submodules** in git's sense. `sub` records a pointer to a child repository, which covers the
  common case with different semantics.
- **`git bisect run`**, `git worktree`, `git notes`, `git stash --patch`.
- **The forge ecosystem.** GitHub, GitLab and the rest speak git's protocol. `git push` is the
  bridge, and it is the one place a repository still needs the git binary.

## What "100% replacement" would mean, honestly

For one person on one machine, on text and binary files, with a remote over HTTP or object
storage: nothing above blocks it today, and the daily cycle is measured and working.

For a team: disk at 10.2x, no line-ending normalisation, and a forge still reached through git.
Those three are the real answer to the question, and two of them are schema and format work rather
than new features.
