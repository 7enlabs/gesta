# Speed, measured

Every number here is wall clock on this machine, measured by `bench/throughput.py` against git on the
same tree at the same moment. Nothing is estimated. Re-run that file on your own machine before
believing a figure, because the absolute numbers depend on disk and on whether the tree is in the page
cache.

## The short version

A commit costs about **4x** what git costs. A status costs about **1.5x** at 500 files and **2.4x** at
2000. `blame` and `log` are level with git, and `blame` is often faster.

The commit gap is a design cost, not a defect, and it is worth knowing what buys it:

| What happens on a commit | Why it is there |
|---|---|
| Read the previous version's object | to compute a delta and to attribute lines |
| Line diff against the previous version | so `blame` answers without walking history later |
| Write blame ranges for the changed lines | the same work, done at commit time rather than at read time |
| Store a delta, not a full body | to keep a long editing history small |

Git defers all of that: `blame` walks history when asked, and packs are built later by `gc`. So git
pays at read time and at collection time, and this tool pays at write time. Neither number alone says
which is better; a session that writes a thousand times and never reads blame is a session where this
tool's choice is the wrong one.

## The numbers

500 files, one file edited per operation:

```
status, 500 files clean            gesta  3.4 ms | git  2.1 ms |  1.58x slower
status, 1 file edited              gesta  3.6 ms | git  2.7 ms |  1.32x slower
diff, 1 file                       gesta  3.3 ms | git  2.1 ms |  1.54x slower
commit, 1 file                     gesta 19.9 ms | git  4.8 ms |  4.18x slower
log, last 10                       gesta  1.8 ms | git  1.7 ms |  1.05x slower
blame, small file                  gesta  2.1 ms | git  2.2 ms |  0.94x faster
```

2000 files:

```
status, 2000 files clean           gesta  8.3 ms | git  3.5 ms |  2.36x slower
status, 1 file edited              gesta  8.0 ms | git  3.2 ms |  2.48x slower
diff, 1 file                       gesta  8.2 ms | git  3.1 ms |  2.62x slower
commit, 1 file                     gesta 23.2 ms | git  7.8 ms |  2.99x slower
log, last 10                       gesta  2.1 ms | git  1.9 ms |  1.11x slower
blame, small file                  gesta  2.1 ms | git  3.1 ms |  0.67x faster
```

## What the commit cost depends on

Measured separately, so the shape is visible:

| Tree | Commit |
|---|---|
| 1 file | 30 ms |
| 100 files | 28 ms |
| 500 files | 28 ms |

The cost does not follow the size of the repository. That is the property worth having: a commit in a
repository of 10000 files costs what a commit in a repository of one file costs, which is the reason
the branch tree is stored rather than rebuilt.

It does follow the size of the file being committed, because blame is a line diff:

| Lines in the changed file | Commit |
|---|---|
| 10 | 30 ms |
| 500 | 28 ms |
| 2000 | 43 ms |

A commit that touches one line of a 2000 line file pays for a diff of 2000 lines. That is the one
place this design is quadratic where git is not, and it is worth knowing before committing a generated
file.

That shape also decides what `bench/long_history.py` can measure. It appends one line per commit, so
the file grows with the history and the blame diff grows with it: the gate reads a commit getting
more expensive over a longer history even though the repository holds one file.

Four things were rising with the history for reasons that had nothing to do with blame, and they are
fixed: the delta base comparison walked the whole of both bodies (now a 4096 byte window), the chain
depth was asked before the cheap prefix probe rejected a candidate, the size band covered 495 objects
at 1000 commits against 50 at 100 (now a narrow band ordered by `created_at`), and the index that
serves that query orders by `(size, created_at)` rather than size alone.

What is left is not the line diff, and saying it was would be wrong. A file of 400 lines and one of
4000 lines, each committed with a one line change in a repository of the same size, cost 45 ms and
49 ms: ten times the lines bought 4 ms, so the per-line work is not what the gate is reading.

The number of changesets that touched the path is not it either: the same path committed once cost
44 ms, and committed a hundred and one times cost 55 ms, so a hundredfold history on one path bought
11 ms.

So the growth is real, is measured on a clean runner (6.2 to 36.8 ms over a tenfold history), and is
not explained by the size of the file or the length of the path's history. What is known is that it
lives in the base search: turning `find_delta_base` off brought the same bench from 5.98x to 1.58x.
The four causes found there are fixed. The remainder is not attributed, and the honest reading of a
red gate here is that the bound is not yet found rather than that the cost is inherent.

## Startup

A command that does no work at all costs about 2 ms: open the database, read the schema version, close.
`log --limit 1` is 3 ms and `status` on an unchanged tree is 3 ms, which is the floor. Nothing in the
figures above is process startup; all of it is work.

## History questions

A question about ancestry - is this an ancestor of that, what is the merge base, which changesets
separate these two points - is answered by walking parents. Wren, Jujutsu, Sapling and Dolt all
answer it from an index instead, and Wren writes the rule down as an invariant: a history query may
not scan the commit log. That is a design this store has not adopted, and `docs/git-parity.md` records
it as an open gap rather than a closed one.

What changed here is the shape of the walk, not the walk. `ancestors` used to ask the database for a
row's parents every time it visited one, and `merge_bases` calls it once per input and again for every
common ancestor, so a merge base on a forked history was a query per step per candidate. The whole
parent table is now read once and used for every walk inside one answer.

Measured with `bench/merge_base_cost.py` on 300 changesets with a fork and a re-merge:

| | |
|---|---|
| `status` (startup and one read) | 32.8 ms |
| `bisect start`, which resolves two ancestries | 45.6 ms |
| walk after removing startup | 12.8 ms, or 42.8 us per changeset |

The per-changeset number is the one to watch. At a few hundred changesets the walk is inside the
noise of process startup and is not a problem; the figure is linear, so it does not go away as a
history grows. `bench/history_slope.py` tries to fit the slope and reports honestly when it cannot:
at 600 changesets the walk is under 10 ms against a 38 ms startup, so the slope is not
distinguishable from noise, and that bench cannot say what a walk would cost at 85,545 commits.

Each committed changeset now carries a `generation` number - one more than its highest parent, the
number git, Jujutsu, Sapling and Wren all keep. It is written at commit time and a migration fills it
for a repository that predates the column. Nothing reads it yet: pruning a walk by generation is the
next step, and recording the number without using it is a schema change, not an optimisation.

## What is not measured here

- Concurrent writers: `bench/many_writers.py` covers the shape, not the absolute cost.
- Network operations: `bench/http_remote.py` and `bench/tls.py` cover correctness, not speed.
- Rebuilding blame for a whole imported history: `bench/rebuild_cost.py` measures the slope and the
  numbers are printed rather than asserted, because the cost depends on how many versions a path has
  and there is no single threshold that would be true for every repository.
