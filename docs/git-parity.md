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

### 2. `.gitattributes` covers line endings and nothing else

`text`, `-text`/`binary`, `eol=lf` and `eol=crlf` are honoured, read from `.gitattributes` and from
`.gesta/attributes`, with the last matching rule winning. A text rule normalises to LF in the object
and writes the named ending to the working tree, so a Windows checkout and a Unix one agree on what
changed. `ignore attr <path>` says which rule decided, because a rule file where the last match wins
is easy to write and hard to read back.

Absent: filters, diff drivers, merge drivers, and `working-tree-encoding`. Each of those runs a
program on every file, which is a different feature with its own trust question.

### 3. No SSH transport

A remote is a filesystem path, an HTTP server, or S3. An SSH remote would need the protocol and key
handling. `git push` covers it in the meantime by shelling out, which is honest but depends on the
git binary being present.

### 4. The write is flat; the scan is not

Measured on one file edit at three repository sizes, `create` and `commit` apart:

```
  files   gesta create   gesta commit   git add+commit  write ratio
    500      13.2ms       11.4ms           6.2ms        1.86x
   2000      19.7ms       12.8ms          13.8ms        0.92x
   8000      48.2ms       14.7ms          21.6ms        0.68x
```

The write costs 1.28x more in a repository 16x larger, which is the property the project claims: the
cost follows the change. At 8000 files the write is cheaper than `git add && git commit` (14.7
against 21.6 ms).

The read costs 3.65x more over the same range, and that number is real rather than a bug to hide.
Answering "what changed" means stat-ing every tracked file, some 14 ms of the 48 at 8000 files, and
comparing the rest against a cache. Git pays the same shape of cost in `git status` (7.9 ms at 8000
files on the same machine, because its index stores the stat of every path).

A single figure for the two phases said "2.12x worse" about a floor that did not move. Both numbers
are reported now, and the gate asserts the write is flat and the scan was seen growing, so neither
can be made to look good at the other's expense.

### 9. Long histories are lightly measured

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
