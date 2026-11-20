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

### 1. Disk: 1.4x git, after grouping objects into shared streams

Measured on 11,301 files and 171 MB of content, both tools at their best:

```
work tree                      171.7 MB
git, packed                     37.4 MB
gesta, after VACUUM             72.8 MB
```

The earlier figure in this document was 10.2x, and it dated from before the blame table was rewritten
as ranges. It also compared gesta against an unpacked git, and named a table as the suspect without
measuring it. `bench/disk.py` now does what a comparison has to do: pack git with `--no-detach`
(a plain `git gc` detaches and returns before it has packed anything, which made an early run of this
very script report that git never shrank), VACUUM gesta, and break the database down by table.

```
objects                  59.8 MB   82.1%
file_versions             1.7 MB    2.3%
changeset_files           1.6 MB    2.2%
worktree_cache            1.6 MB    2.1%
branch_tree               1.5 MB    2.0%
blame_ranges              0.9 MB    1.2%
every other table         5.4 MB    7.9%
```

Everything that is a row per path is small. The content is the cost, and the reason is specific:
git compresses many objects into one stream, and gesta compresses each object on its own. Measured
on this tree, joining the objects into one stream gives 1.50x, which is almost exactly the remaining
gap between 76.3 and 33.8 MB once the ratios are applied to the same content.

Raising the compression level does nothing: level 6 gives 47.3 MB and level 9 gives 47.2 MB, for
10% more time. The gap is not effort, it is that a zlib stream cannot see the next file.

That gap is closed now, and the shape of the fix came from measuring it first. Grouping objects
before compressing them, 50 at a time and sorted by size, gets 1.36x; joining everything into one
stream gets only 1.50x. Almost the whole win is available at a granularity a read can afford, so
there was no need to trade the object model for it.

`gesta gc` groups loose text objects into streams. Each object keeps its row, its hash and its own
identity; the row names the group and the byte range instead of holding the body. Measured on the
same 11,225 file tree:

```
before gc       69.2 MB
after gc        51.4 MB      (25% smaller)
```

A read decompresses one group, which is 50 objects rather than the whole store, and the group is
bounded on purpose. A repository too small to benefit forms no group at all, so a hundred small files
do not pay for a mechanism they do not need.

One thing this cost, and it is why the constant is 50: a read is now proportional to its group rather
than to one object. At 50 objects that is a few hundred kilobytes of decompression for a few kilobytes
of answer. At 1000 it measured almost no extra saving and a much worse read.

Measured on a 600 file tree, reading one file through `show`:

```
loose    1.6 ms median
packed   1.8 ms median      (1.12x)
```

So the read price is 12%, not the several-fold a pack format can cost when the group is a whole
repository. On a tree that small the pack does not pay for itself at all, which is why a repository
below the threshold forms no group: 847 KB loose against 995 KB packed, because the index entries
cost more than the shared vocabulary saved. The mechanism needs a tree with enough repeated text to
be worth having.

### 2. `.gitattributes` covers line endings and nothing else

`text`, `-text`/`binary`, `eol=lf` and `eol=crlf` are honoured, read from `.gitattributes` and from
`.gesta/attributes`, with the last matching rule winning. A text rule normalises to LF in the object
and writes the named ending to the working tree, so a Windows checkout and a Unix one agree on what
changed. `ignore attr <path>` says which rule decided, because a rule file where the last match wins
is easy to write and hard to read back.

Absent: filters, diff drivers, merge drivers, and `working-tree-encoding`. Each of those runs a
program on every file, which is a different feature with its own trust question.

### 3. A round trip now keeps every commit name

An import followed by an export used to produce a repository whose content matched and whose commit
ids did not: 0 of 97 survived on this project. git hashes the author line including its timezone, and
the export wrote `+0000` where the import had read `+0700`.

The timezone is recorded on the changeset now and written back on export. Measured after the fix:
97 of 97 commit ids survive an import and export of this project's own history, and `git fsck` accepts
the result.

The gate asserts this with a source repository written at `+0700`, because a `+0000` source matches
whether the code is right or wrong, which is how the fault survived a test that read the export back.

### 4. The server speaks enough of the git protocol to clone and to push

`gesta serve` answers the four requests a client makes: `info/refs?service=git-upload-pack`,
`POST git-upload-pack`, `info/refs?service=git-receive-pack` and `POST git-receive-pack`. A plain
git client clones, and a plain git client pushes.

That is the check, and the only one worth making: a test that reads our own response back proves the
response is well formed and nothing about whether git agrees with it.

Six details each produce a symptom that looks like a different bug:

- The pkt-line length counts its own four byte header.
- The first ref line carries the capabilities after a NUL, and only the first one.
- Under `side-band-64k` the stream ends with a band-1 packet carrying nothing, then a flush.
- `HEAD` has to be advertised as a ref of its own alongside `symref=HEAD:refs/heads/main`. Without
  the `HEAD` line git files everything under `refs/remotes/origin` and leaves the tree empty with a
  warning about a missing ref.
- A push sends a **thin pack**: the objects the server is known to hold are left out and named as
  bases. Unpacking into an empty repository fails on an object the client never sent, so the server's
  own history is exported into the scratch repository before the new objects arrive. This is the
  detail that separates a push that works from a push that reports a missing object.
- The client's `old` value has to be compared with the branch **before** the import moves it.
  Reading it afterwards compares the client's expectation with the result of applying the push, which
  always differs, and every correct push is rejected.

`index-pack` is not enough to unpack a push: it leaves the objects inside a pack, and the importer
reads loose objects. `unpack-objects` writes them loose, which is the shape the next step needs.

What is absent: the v2 protocol, `multi_ack_detailed`, ref deletion, and any delta in the packs this
server writes. The pack it sends is a full copy, about 3x a delta pack on this project, and a
negotiation that sent only what the client asked for would need its `have` list. Absent on purpose,
and named here so the next person does not read the absence as a bug.

### 5. No SSH transport

A remote is a filesystem path, an HTTP server, or S3. An SSH remote would need the protocol and key
handling. `git push` covers it in the meantime by shelling out, which is honest but depends on the
git binary being present.

### 6. The write is flat; the scan is not

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

### 100,000 files: the write holds, the scan does not

The write claim was measured to 8,000 files. At 100,000 it still holds, and the comparison to the
tool this replaces flips in one column and not the other.

| files | gesta scan | git status | gesta write | git add+commit |
|---:|---:|---:|---:|---:|
| 8,000 | 33.1 | 11.0 | 13.4 | 15.6 |
| 100,000 | 454.7 | 78.4 | 13.2 | 100.2 |

The write is flat: 13.4 to 13.2 ms while the tree grew 12x, against git going 15.6 to 100.2. Above
roughly 10,000 files a commit is several times cheaper here.

The whole path says the opposite. A commit is a scan and a write, and at 100,000 files:

| | gesta | git |
|---|---:|---:|
| scan + write, one file changed | 446.4 ms | 108.1 ms |

The scan is 5.8x slower than `git status` and it swallows the write advantage. Measuring only the
write would have reported a win on the phase nobody runs alone.

Where the 364 ms of `status` goes at 100,000 files, measured part by part rather than guessed:

| part | ms |
|---|---:|
| walk only (scandir + stat per entry) | 195.8 |
| plus a relative path per entry | 261.5 |
| cache load, 100,002 rows | 47.7 |
| hash map built from those rows | 12.3 |

There is no single culprit. The kernel floor for stat-ing 100,000 files is 186.7 ms, so the tool is
about 1.95x its floor, and the extra is work git does not do: an ignore rule per path, a relative path
string per file, a cache read, and a hash map. Each is defensible on its own. Together they cost more
than the write they were protecting.

Two things measured and rejected as the cause: sorting the 100,002 collected paths costs 8.1 ms, and
the walk order is already unsorted, so the sort is not free but it is not the problem either.

One idea was written here first and then measured and removed, which is worth recording because it
reads as obviously correct:

> Compare the directory's own mtime with the recorded one and reuse every entry under it when it has
> not changed.

It is wrong. A directory's mtime changes when an entry is created, removed or renamed inside it. It
does not change when a file inside it is rewritten. Measured on this filesystem: modifying an existing
file left the directory mtime untouched, twice, while adding and removing a file both changed it.
Adopting the idea would have made every edit to an existing file invisible to `status`, which is the
one change a person makes most often. The measurement took two minutes and would have cost a bug that
looks like data loss.

### The sort nobody asked for

`scan_files_excluding` sorted its result before returning it. No caller reads the order: the scan's
output becomes a map keyed by relative path, and the two tests ask whether a path is present. The sort
was 79 ms of a 244 ms walk at 100,000 files, because comparing paths compares components rather than
bytes.

Measured A/B on the same tree, two binaries, seven runs each:

| | status at 100,000 files | ratio to `git status` |
|---|---:|---:|
| with the sort | 388.0 ms | 5.23x |
| without it | 299.6 ms | 4.04x |

23% faster, and the removed code was doing nothing.

The walk fell to 164 ms, and the Python floor of 186.7 ms was taken to mean the stat calls were no
longer the cost. That was wrong as well, and two more measurements were needed to get it right:

| walk, same tree, best of three | ms |
|---|---:|
| `read_dir` only, no stat at all | 21.7 |
| `read_dir` plus `file_type()` per entry, stat only files | 122.2 |
| `read_dir` plus `metadata()` per entry | 118.0 to 223.3 |

So 21.7 ms is not the floor: a scan has to know each file's size and mtime, which is a stat per file,
and that is 122 ms. `file_type()` is free because readdir already reported the type, and calling
`metadata()` on a directory is a stat the tool did not need. An earlier comment in the walk said
`metadata()` "costs nothing beyond what read_dir returned", which the first row contradicts.

Directories are now typed from the directory entry and only files are stat-ed. That saved 8 ms, not
100, because this tree holds 501 directories among 100,505 entries, so there were only 501 stats to
remove. The lesson is the one this file keeps recording: the cost of a change is what the measurement
says it is, and the shape of the tree decides which of the two floors applies.

That measurement also corrected an earlier one. `sorted()` on a list of Python strings costs 8.1 ms,
and that number was taken to mean the sort was cheap. It was not: the Rust sort compares `PathBuf`
values, which walks components, and the two are not comparable. Measuring the thing in the wrong
language produced a wrong conclusion that survived two rounds of reasoning.

### What the four remaining ideas were worth

Four changes were listed here as the way to close the gap. Three were measured and kept, one was
measured and thrown away, and the measurement changed the order.

| change | status at 100,000 files | verdict |
|---|---:|---|
| walk returns `(rel, meta)` and builds the path from a prefix | 319.9 -> 303.5 ms | kept |
| reserve the cache map instead of growing it | 222 -> 173 ms | kept |
| take the hash out of the map instead of cloning it | +5.2 ms slower | reverted |

The first two together, measured against the published binary over 21 interleaved pairs:

| | median | p10 | p90 |
|---|---:|---:|---:|
| before | 302.0 ms | 288.4 | 352.2 |
| after | 274.3 ms | 262.3 | 305.5 |
| `git status` | 77.9 ms | 72.3 | 85.2 |

9% faster, 3.88x to 3.52x against `git status`.

Where the 274 ms goes: about 122 ms of stat-ing every file, which no scan can avoid, roughly 37 ms of
ignore rule and path building, 29 ms loading 100,003 cache rows, 21 ms in the loop that builds the
result, and under 1 ms pruning. The two largest items left are the stats themselves and the cache
load.

`HashMap::remove` costs more than `get` plus a clone of a forty byte string, which is the opposite of
what the count of allocations suggests. That one was measured before it was believed.

What is genuinely left:

- Avoid loading cache rows for paths the walk will not visit. The two passes are separate today, so a
  tree where most cached paths are gone pays for rows it never reads.
- Skip the ignore rule for a path whose parent was already excluded by a directory rule. The rule
  check is 14.8 ms of the 30 ms above.
- Compare a directory's listing against the cache without building 100,003 `HashMap` entries at all,
  which is the shape `git status` uses and why it is 78 ms.

None of those is done. The number above is what the tool does today.

### Many writers in one checkout

SQLite takes one writer at a time, so the question is not whether writes serialise but whether the
store survives the queue. Measured with writers started together:

| writers | landed | wall ms | median ms | p95 ms | changesets | fsck |
|---:|---:|---:|---:|---:|---:|---|
| 20 | 20 | 309 | 52 | 135 | 21 | clean |
| 100 | 100 | 1185 | 172 | 307 | 101 | clean |

Every writer landed exactly one commit and `fsck` was clean at both sizes. The drain time grew 3.83x
for 5x the writers, which is what a serialised queue costs when the work per write is small.

The measurement found a real bug before it found a number. Writers that snapshot the whole tree
record each other's files: at 100 writers one changeset held 39 files belonging to other writers, and
one writer was told `changed: false` because the file it had just written had already been committed
by somebody else. Nothing was lost, but authorship was wrong, which for an agent surface is worse
than losing a commit because it looks correct.

`changeset create --path` limits the snapshot. A path matches itself or a parent of itself, so naming
a directory covers what is under it. Without the flag the snapshot is still the whole tree.

`bench/many_writers.py` runs the measurement; `bench/gate.py writer-scope` checks the scoping.

### Long histories: measured to 10,000 commits

A commit is billed as costing what the change costs, not what the repository holds. That claim was
measured on the file axis and not on the history axis, and when it was measured the second one was
false.

| commits | write ms | scan ms | db MB |
|---:|---:|---:|---:|
| 100 | 13.6 | 12.0 | 0.3 |
| 1,000 | 19.4 | 16.7 | 1.3 |
| 10,000 | 47.1 | 17.4 | 82.7 |

The scan stayed flat. The write did not: 3.47x the cost over 100x the history.

The cause was blame. Every commit deleted the file's whole `blame_ranges` and wrote it back, so the
cost of committing one line followed the number of lines the file already had. On a ten thousand
line file that delete and rewrite measured 34 ms, which is almost all of the growth.

The fix compares the new ranges with the stored ones and rewrites only from the first difference
onwards. Appending a line leaves every earlier range untouched.

| commits | write ms | scan ms |
|---:|---:|---:|
| 100 | 14.1 | 12.0 |
| 1,000 | 19.0 | 15.0 |
| 10,000 | 29.4 | 16.4 |

2.08x over 100x history, down from 3.47x.

Two mistakes on the way, both worth keeping:

- Adding an index on `file_versions(path, version DESC)` looked like the fix and was not. The lookup
  with `LIMIT 1` was already 0.004 ms. An index that helps nothing costs a write on every insert, and
  the measurement said so before the change was kept.
- The first version compared line by line against the previous version, which is wrong the moment a
  line is deleted: every line after it shifts, so index i is a different line. The comparison has to
  be between ranges, which is what the table holds. A test that deletes a line that a previous commit
  inserted caught it.

`bench/long_history.py` runs the full measurement; `bench/gate.py long-history` runs a short version
in the gate.

### 8. No interactive conflict resolver

Conflicts are data: both sides, line numbers, context, all three whole files. An agent writes the
answer. A human is handed conflict markers. That is a deliberate design choice, not a gap, but it
is a difference from git that a person will notice.

### 9. What git has that nothing here replaces

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

For a team: disk at 1.95x, no forge of its own, and no submodules in the git sense.
Those three are the real answer to the question, and two of them are schema and format work rather
than new features.
