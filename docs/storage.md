# Why packing small objects did not help

A tree of 1000 tiny files costs 6.6x git in disk. The obvious fix is to pack several
small bodies into one row, so the row header and the index entry are paid once per
thousand objects rather than once per object. That fix was built and measured, and it
made the repository **larger**: 6.6x became 7.7x.

This records the measurement, because the reasoning was sound and the result was not.

## What the bytes actually are

Measured with `dbstat` on 1000 files of 7 bytes each, one changeset, before any change:

| Table and index | Bytes | Share |
|---|---:|---:|
| `file_versions` | 114,688 | 11.2% |
| `changeset_files` | 106,496 | 10.4% |
| `worktree_cache` | 102,400 | 10.0% |
| `objects` | 98,304 | 9.6% |
| `branch_tree` | 98,304 | 9.6% |
| `sqlite_autoindex_objects_1` | 86,016 | 8.4% |
| `blame` | 49,152 | 4.8% |
| (nine more, each under 4%) | | |
| **total** | **1,024,000** | |

The `objects` table is 9.6 percent of the file, not the whole problem. Its content column
holds 7 bytes per row and the row costs about 98, so the packing idea addresses roughly
91 kilobytes of a megabyte.

## What the packing did

The design added `object_packed(hash, seg_id, seg_offset, seg_len)` and
`object_segments(seg_id, body)`, moved each small body into a shared segment, and blanked
its `content`. The bodies really did shrink: 1000 contents that occupied 98,304 bytes
became one 1,959 byte compressed segment.

But a pointer is not free. `object_packed` costs 86,016 bytes for its rows plus 86,016 for
its primary key index, and `object_segments` adds its own. Net effect on the file:

| | Before | After |
|---|---:|---:|
| `objects` rows | 98,304 | 90,112 |
| `object_packed` + its index | 0 | 172,032 |
| `object_segments` | 0 | small, one row |
| **total file** | **1,024,000** | **1,196,032** |

Eight kilobytes saved on the content, 172 kilobytes spent pointing at it.

## Why the idea failed here

A pointer into a segment is a hash, two integers and a row header, which is more than the
seven bytes it replaces. Packing wins when the bodies are large enough that the row
overhead is small against them, and that is exactly when compression has already made the
saving. The workload where packing would pay, bodies of a few hundred bytes, is not the
workload that costs 6.6x.

## What the numbers say to do instead

The cost is spread across six tables that each hold one row per tracked path:
`file_versions`, `changeset_files`, `worktree_cache`, `branch_tree`, `blame`, and
`objects`. No single one dominates, and each exists for a reason:

- `branch_tree` is the maintained tree that keeps `status` from rebuilding history.
- `worktree_cache` is the stat cache that keeps `status` from re-reading every file.
- `blame` is one row per line per version.
- `file_versions` is the rename and history chain.
- `changeset_files` is the change itself.

Packing `objects` while leaving five other per-path tables alone cannot move the ratio far.
A real fix is fewer rows, not smaller ones. Two candidates, neither implemented:

1. **One row per path instead of per version.** `file_versions` and `blame` both grow with
   history, so a repository edited a thousand times holds a thousand rows per path. Keeping
   the newest and a bounded chain would cut those two tables; it costs the ability to ask
   about any older version, which is what `history` does today.
2. **A denser row format.** SQLite's per-row overhead is what makes 7 bytes cost 98. Storing
   the tree as one serialized value per branch, the way git does, trades the ability to
   query a single path for a much smaller file.

Both are schema changes and both lose a capability. Neither has been done, and the honest
number stays 6.6x until one of them is.
