# Two processes at once

What happens when more than one gesta command runs against the same repository, what is
guaranteed, and what is not.

## What is safe

Writers serialize. The database runs in WAL mode with a thirty second busy timeout, and the
operations that change history retry while the database is busy. Verified with twenty
processes creating and committing at the same time: twenty one changesets, twenty one files,
a clean integrity check, nothing lost.

A reader is never blocked by a writer. `status`, `log` and `diff` run while another process
commits, and they see either the state before or after, never a half written one.

Committing the same changeset twice from two processes is safe. One commits it, the other
reports that it is already committed, and the history holds it once.

## What is not safe, and cannot be

Two processes editing the same file on disk at the same time. Nothing in the repository can
reconcile that, because the last writer wins at the file system level before gesta is
involved. The same is true of every version control system.

What gesta can do is notice afterwards, and it now does: a pending changeset records the
content it snapshotted, and `status` warns when a file it recorded no longer matches the
disk.

```
changesets: 3 total, 1 pending

warning: 1 pending changeset(s) no longer match the working tree:
  cs0a3499a0b06d (f.txt)
Commit or discard them before snapshotting again.
```

That warning appears when the file moved on after the snapshot. It does not appear in the
narrower case where two writers produced the same content, because then the snapshot does
describe the disk and there is nothing to report.

## What to do in practice

- One agent per repository, or one agent per branch. Coordination across processes is the
  caller's job, not the store's.
- Snapshot and commit close together. A pending changeset is a photograph, and a photograph
  of yesterday does not describe today.
- If `status` warns about a stale snapshot, discard it and snapshot again rather than
  committing content nobody checked.
- Before trusting a repository after an interrupted run, `gesta fsck --worktree`.

## Why not a lock file

A lock would turn a working concurrent read into a failure, and a crashed process would leave
the repository locked with no clean way to prove the lock is stale. WAL plus retries keeps
readers working and lets the writer wait, which is the behaviour that matches how agents run.
