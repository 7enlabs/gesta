# Backup and restore

A backup is the whole store in one file: every branch, every tag, blame ranges, file versions and the
branch tree. `gesta backup` writes it, `gesta verify-backup` reads it without touching a repository,
and `gesta restore-repo` puts it back.

## Why not a bundle

`gesta remote push` sends a bundle, and a bundle is not a backup. It carries one branch, the
changesets reachable from that branch, and the objects they name. Everything else stays behind:

| In a backup | Not in a bundle |
|---|---|
| every branch | the branches other than the one named |
| tags | all tags |
| blame ranges | all blame |
| file versions for every path | only those reachable from the named branch |
| branch tree cache | absent |
| repository state: sparse scope, current branch | absent |

Calling a bundle a backup is the mistake that costs a repository. A person who backs up their work
and loses the tag that marks a release has lost the release.

## Taking a backup

```bash
gesta backup /backups/project-2026-11-19.db
gesta backup --json /backups/x.db        # the same, as one document
```

The destination is replaced if it exists. A backup that appended to an older file would produce a
file that is neither snapshot.

The copy goes through SQLite's backup API rather than a file copy. A file copy of a store in WAL mode
misses whatever is still in the write-ahead log, and those are the commits made in the last minute,
which are the ones a person notices missing.

**A backup works while the repository is being written.** The copy is a consistent snapshot as of
when it starts; a writer that commits during the copy leaves the file holding a coherent point in
time and fewer rows than the live store holds at the end. That is correct, and the count is compared
against the source as it was when the copy began.

## Checking a backup

```bash
gesta verify-backup /backups/x.db
```

Reads the file only, so it is safe against a repository in use, and safe on a machine that has no
repository at all. It answers three questions: is this a readable database, does it pass SQLite's
integrity check, and does it have a `changesets` table. A database that is not a repository is
refused by name rather than restored into place.

Run this on a backup you have not restored. A backup nobody has read is a file with a hopeful name.

## Restoring

```bash
gesta restore-repo /backups/x.db          # refuses: says what would be replaced
gesta restore-repo /backups/x.db --yes    # does it
```

The order matters and it is deliberate:

1. The backup is verified first. Every reason a restore could fail after the live store has moved is
   a reason to check before it moves.
2. The live store is renamed to `gesta.db.before-restore`, not deleted.
3. The WAL and shared-memory files beside it are removed. They belong to the store that moved, and
   leaving them next to a new database is how a restore produces a store that opens and reports no
   commits.
4. The new file is installed and reopened through the ordinary path, so the schema migrates.
5. On success the old file is deleted. On failure it is put back and the error says so.

A failed restore leaves a working repository. A restore that destroys the only copy is worse than the
problem it was meant to fix.

## What this is not

- **Not a schedule.** Nothing here takes a backup for you. A cron entry that runs `gesta backup` to a
  directory that is itself backed up is the part you have to add.
- **Not off-site.** The file lands where you put it. Copying it somewhere else is your responsibility.
- **Not point-in-time recovery.** There is one file per backup and no log to replay, so the resolution
  is however often you run it.
- **Not verified against corruption after the fact.** `verify-backup` checks structure and integrity,
  not that the contents are what you expected. Keep more than one, and check them.
- **Not encrypted.** The store is a SQLite file; the secret store is sealed, the repository is not.
  Encrypt the file if the destination is not trusted.

## Related

- `gesta fsck` checks the live repository, and `fsck --prune` removes unreachable objects.
- `gesta git export` writes a git repository with matching commit ids, which is a different kind of
  portability: it is for moving work into git, not for keeping a copy of this store.
