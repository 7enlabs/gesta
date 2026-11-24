# Decisions

## A Prolly tree is not worth replacing the flat branch tree here

Dolt stores table data in a Prolly tree - a content-addressed B-tree - so that comparing two
versions scales with the difference instead of with the data. The argument is structural and
correct: every subtree two versions share has the same root hash, so a diff descends only into what
changed. It is also the reason Dolt could not have used a plain B-tree, whose structure depends on
insertion order and therefore cannot be shared between two versions at all.

This store keeps `branch_tree` as a flat table of `(branch, path, hash)`, and comparing two branches
reads both sets and diffs them. By the structural argument that is the wrong shape.

Measured with `bench/branch_diff_cost.py`, on trees of 200, 1000 and 4000 paths, asking what
separates `main` from a side branch:

| paths | changed paths | diff |
|---|---|---|
| 200 | 1 | 32.2 ms |
| 200 | 20 | 48.7 ms |
| 1000 | 1 | 31.1 ms |
| 1000 | 100 | 34.1 ms |
| 4000 | 1 | 87.3 ms |
| 4000 | 400 | 43.2 ms |

The cost does not track the tree. A twentyfold larger tree with a four hundredfold larger change
costs about the same, and the differences between rows are startup noise rather than a slope: one
path changed in a 4000 path tree is slower than four hundred changed in the same tree, which is not
a property of any data structure.

That measurement is the whole answer. The structural gap is real and would matter at a size this
store has not been shown to reach, and replacing the branch tree is the largest change on the list -
a new table shape, a new write path, a migration, and every reader audited. A change that large,
made for a cost that cannot be shown at a hundredth of the size where it would appear, is the kind
of work that reads as progress and delivers nothing measurable.

What would reopen this: a repository where a branch diff takes more than a second, or a measurement
showing the per-path cost rising with the tree rather than staying flat. Neither exists today. The
bench stays in the tree so the next person can re-run it rather than argue from the analogy.


Every choice that shaped this program, with the reason it was made. Each one can be revisited.
The numbers are for reference in a discussion, not an ordering.

This is not a changelog. A changelog says what changed; this says why the shape is what it is.
Where a decision was reversed or a diagnosis was wrong, that is recorded too, because a decision
without its mistakes reads as if it were obvious.

## Shape of the program

| # | Decision | Reason |
|---|---|---|
| S1 | One binary, no daemon, no service, no account. | An agent on a laptop is the target. Anything that has to be started is something that can be down. |
| S2 | SQLite as the store, bundled so no system library is needed. | One file to copy, one file to back up, and transactions for free. A directory of loose object files cannot give an atomic commit without its own locking layer. |
| S3 | Every command works without a network. | A repository is local first. Remotes are transport, not the model. |
| S4 | The cost of a commit follows the size of the change, not the size of the repository. | The rule everything else bends around. A tree rebuilt per commit is the difference between an agent committing forty times a task and not committing at all. |

## The cost rule

| # | Decision | Reason |
|---|---|---|
| C1 | `set_branch_head` updates the branch tree incrementally from the changeset's own file list. | The one operation an agent does most must not cost the size of the repository. Measured with `bench/scale.py` (median of three rounds, binary read from the container's own disk): an ordinary one file commit goes from 28.7 ms at 500 files to 54.3 ms at 8000, which is 1.89x for a repository 16 times larger, with the middle half of samples inside 5 to 12 percent. |
| G1 | Git import and export are written against the documented object format, not by calling the git binary. | A bridge that shells out cannot run where git is not installed, which is the machine an agent works on. A round trip proves the format is right: equal commit ids mean the tree, the parents, the author line and the message all match, because the id is the hash of those four. |
| G2 | An import records the git commit id it came from. | Importing the same repository twice adds nothing, and a later export can tell which changesets came from git. |
| C1b | The rebuild is kept, and costs the size of the repository. | A merge, a land and a remote apply cannot carry the tree forward, so they rebuild it from history. Measured with `bench/rebuild_cost.py`: 22.7 ms at 500 files, 57.2 ms at 2000, 187.8 ms at 8000, which is 8.26x for 16 times the files. That is the price of the correctness the fallback buys, and it is why the common path does not take it. |
| C2 | The incremental path falls back to a rebuild when the changeset's parent is not the previous head. | Merge, undo and remote apply all re-parent. Updating incrementally across a re-parent would silently write a wrong tree. |
| C3 | Deltas are capped at a chain of 16. | An unbounded chain makes a read walk the whole history. The cap bounds a read and keeps the compression that matters. |
| C4 | `status` reads a maintained table instead of rebuilding the committed tree. | Rebuilding ran on every `status` and every `diff`, so the saving lands on the most frequent command. The 2 ms figure from the original change has not been reproduced since, and a `status` on 500 files now measures about 30 ms in total, which the note below explains is not comparable. A number that cannot be reproduced is recorded as such rather than repeated. |
| C5 | The stat cache exists, and a file younger than a small window is re-read rather than trusted. | A cache hit needs size and mtime to match. A file written in the same clock tick as the last read has an mtime that proves nothing. |

## Evidence over claims

| # | Decision | Reason |
|---|---|---|
| E1 | `fsck --worktree` hashes every tracked path and compares it to the disk. | A check that reads the database and not the disk is half a check. It is opt in because it reads every file. |
| E2 | `fsck --worktree` never reports a file the user made, a path outside the sparse scope, or a branch that is not current. | A check that cries wolf is turned off. Three tests hold that line. |
| E3 | A revert raises a change's risk score and never lowers it. | A rule that can quietly hide a change someone wanted to see is worse than a noisy one. |
| E4 | `land` materialises the working tree when the target branch is the checked-out one. | Without it the head moved and the disk did not: `show main:a.txt` returned the new content while `status` called the file deleted. |
| E5 | A shallow clone carries a snapshot of the head tree inside the bundle. | Truncating the history is exactly removing the changesets that created the old files. Restoring from the chain gave 5 files out of 200. |
| E6 | Bundle completeness counts the snapshot and every object the snapshot names. | A head that had not moved left its objects out of the bundle, so the bundle contradicted itself. |

## Things that must not be lost

| # | Decision | Reason |
|---|---|---|
| K1 | A stash records a deletion as a row with no hash. | Collecting only added and modified paths reported "nothing to stash" when the only change was a removal, and the intent to delete disappeared on the next branch switch. |
| K2 | A three way merge refuses to append a replacement that is already the tail of what was emitted. | Two sides expanding adjacent lines to the same content produced the line twice with no conflict reported. |
| K3 | Merge overlap is decided by the content the regions produce, not by where they sit. | Two wrong fixes came first: skipping hunks inside an already handled region broke a merge into an empty base, and treating a partial overlap as a conflict never fired because the regions are adjacent. |
| K4 | `undo` restores the working tree and the database row together. | An undo that leaves the disk ahead of the head is a silent divergence. |
| K5 | `--fix` refuses anything a person's judgement could change, and names the command. | A fix that silently skips work is worse than no fix, because the caller believes the state is clean. |
| K6 | Setting uncommitted work aside takes an explicit flag. | Committing a snapshot is safe because the snapshot describes the disk. A stash is safe because it keeps the work. Removing someone's unfinished edit from the tree is not safe, so it is never the default. |

## Secrets and transport

| # | Decision | Reason |
|---|---|---|
| P1 | A secret never appears in `argv`, in a URL, or in the repository. | `argv` is readable through `ps` and lands in shell history. A URL is logged by every proxy on the path. |
| P2 | The secret store is one file per user, not per repository. | A repository can be copied or pushed; the credential must not travel with it. |
| P3 | Credentials are read from the environment first, then the sealed store. | The environment is what a test or a one-off job sets; the store is what a person sets once and forgets. |
| P4 | The sealed store uses ChaCha20-Poly1305 with a single pass KDF. | The threat model is a leaked backup, not an attacker holding the file and a GPU. The single pass is stated rather than hidden. |
| P5 | `S3Config` has no `Debug`. | It holds the secret key, and a derived `Debug` prints it in any panic message. The test avoids needing it rather than adding it for convenience. |
| P6 | A plain `http://` endpoint is refused except on loopback. | The signature names the key, covers the payload and can be replayed inside its signing window. Loopback is allowed because a local test server cannot be read by anyone else. |
| P7 | The S3 remote is kept. | It is the only remote that needs no listening process. It costs 719 lines across `s3.rs` and `sigv4.rs` and one gate step. Measured from two builds, the TLS feature adds 1.63 MB (6,065,400 to 7,695,864 bytes), and that cost belongs to the HTTPS remote, which needs it on its own; S3 reuses it. |

## The interface an agent meets

| # | Decision | Reason |
|---|---|---|
| A1 | `brief` returns a named state, not a paragraph. | A program branches on `commit-work`; a summary gets read once and forgotten. |
| A2 | A stopped rebase outranks everything in `brief`. | Nothing else is safe until it is finished. |
| A3 | New work beyond a snapshot outranks committing that snapshot. | The snapshot does not describe the new file, so committing it would drop the file. |
| A4 | `grep --history` reads each distinct version once. | Reading per changeset repeated the whole history, which is the difference between a search that finishes and one that does not. |
| A5 | Search and state are exposed over MCP as well as from a shell. | The protocol is how an agent reaches the tool. A capability only a shell can reach is a capability the agent effectively lacks. |
| A6 | Every JSON object carries `schema: gesta@1`, including MCP results. | A renamed field breaks a caller silently: it keeps running and reads the wrong value. |
| A7 | A top-level list stays a bare list, and `--envelope` asks for the versioned object. | A list cannot carry a key, and wrapping it would break the pipes people already write. |
| A8 | `grep --regex` is a separate feature from the default build. | A build that only needs literal search keeps a smaller binary. A pattern the engine cannot express is refused with the construct named. |
| A9 | `changeset commit` takes no id when exactly one changeset is pending. | The shortest session is create-then-commit, and requiring the id there made the second call fail on a value the program already had. Measured: four plausible attempts before the right form, each a lost turn. With several pending the id stays required, because choosing silently commits work the caller did not name. |
| A10 | A refusal names the reading the command used, and the three readings are separate sentences. | `show nope` is a path against the current branch, `show nosuch:a.txt` names a revision, and a repository with no changesets has no tree at all. One message covering all three sends the reader to look at whichever part was never in question. Collapsing the colon case into the file case is what the gate's `errors` step caught. |
| A11 | A refusal lists the names that do exist. | `rebase nosuchbranch` said `no such changeset`, an argument that accepts a branch, a tag or a changeset, and named nothing the reader could compare against. The list is what turns a typo into the next attempt. |
| A12 | `checkout` defaults to the whole tree; `restore` keeps its argument required. | The two have opposite failure modes. Writing the branch out cannot lose uncommitted work, because the branch already holds those files. Discarding cannot be undone, so a default there would fire on a typo. Neither path deletes an untracked file, checked end to end. |
| A13 | A rebase reports a removed path that another file still mentions, and the comparison base is the tree it moved onto. | A deletion and a reference to the deleted name, made in parallel, apply cleanly because they touch different files, so no conflict reports them. Comparing against the branch's own past misses it: the deletion may predate the rebase. The report is not an error, and it says so, because a reader who thinks the rebase failed reaches for `--abort`. Matching is delimited so `myutil.sh` is not a mention of `util.sh`; a warning that fires on substrings is a warning people skip. |
| A14 | A stopped rebase re-applies the paths that did not conflict before recording the resolution. | Replay returns at the first conflict, so every other path the changeset touched is still at its base value on disk, and recording the tree records a deleted file as present. A rewrite hides this because the next snapshot records whatever is on disk; a deletion is the change whose mark is an absence, so it vanished with no error. The conflict set is saved with the session so the continuation does not have to re-derive it from the state it is recording. |
| A15 | A missing argument is answered with one line the parser cannot write, added at the parse site rather than by making 37 fields optional across nine files. | clap already prints `Usage: gesta changeset create <MESSAGE>`, which names the shape to a reader who reads usage lines and nothing to a reader who does not. Three sweeps fixed the commands that came to mind and each reported no work left; the real count was 45 of 128, because the first two sweeps only looked at top-level names and the third added eleven while 60 subcommands went unchecked. The first attempt declared the arguments optional, which compiled to 37 signature errors across nine files and reached public functions for no reason: the message is one sentence in one place. `try_parse` catches the outcome and appends an example from a table of 51 entries. `bench/refusals.py` reads the command list from `--help` including one level of subcommand, and fails on the previous commit naming all 45, which is the only evidence that a check of this kind is worth having. |
| A16 | `serve` handles SIGTERM and SIGINT, with the blocking accept on its own thread and the wait on the main thread. | A blocking `accept` cannot be interrupted by a flag, and without a handler both signals were ignored outright. The visible cost was not a hung server: it was that every bench that starts a server and calls `terminate()` leaked one, because the signal went nowhere and the process outlived its parent. Two thousand had accumulated, holding twelve gigabytes and every port they had bound. Two wrong shapes came first: a non-blocking socket polled every 100 ms with the stop check before `accept` added exactly that interval to every request, measured 101.2 ms median against 100 ms, and checking after `accept` still slept while idle. The thread shape costs nothing because the process ends when the main thread returns, ending the accept thread with it: 0.2 ms median. The handler does one relaxed atomic store, which is all that is safe in signal context. |
| A17 | The wait after the accept thread has no deadline, and a timed-out wait is retried rather than read as a stop. | The first version passed `from_secs(86_400)` to `wait_for_stop` and fell through to `Ok(())` when it returned. A server up for a day therefore shut down on its own, printed nothing, and exited 0. A supervisor that reads only the exit status sees a clean stop and does not restart, so the port goes dark with no failure recorded anywhere. The wait already takes a timeout so its caller can wake periodically, which means `false` cannot be distinguished from a stop by the caller unless the caller keeps asking. `wait_until_stopped` loops until the wait reports a stop, waking once a minute rather than once a second. The decision is a separate function taking the wait as a parameter so it can be tested without waiting a day: the test makes the wait time out three times and asserts it is called four times, which fails on the single-shot shape with `left: 1, right: 4`. |
| A18 | The ancestry walk is checked by counting SQLite statements, not by timing two commands and subtracting one from the other. | The two benches this replaces (`merge_base_cost.py`, `history_slope.py`) measured a command against `status` as a startup baseline and subtracted. On this host `status` alternates between 23 ms and 120 ms inside one run of twenty-five samples, so a walk under 10 ms cannot be separated from startup at any sample count, and both steps were loosened to accept "cannot separate" as a pass, which is a step that can no longer fail on the defect it was written for. Worse, the property is already guaranteed by the type: `walk_ancestors` takes `&ParentMap`, not `&Connection`, so the walk cannot read a row however it is written, and a timer was measuring something that cannot regress. `rusqlite`'s `trace` feature gives `SQLITE_TRACE_STMT`, so the test asks the same question as a count: `merge_bases` over a fork on 8 changesets and on 400 must start the same number of statements. Shipped it is 4 and 4; with the per-step read put back, 13 and 405, so the check fails on the exact regression it exists for and passes on the code. A count has no timing in it, which is why it can be a gate step on a shared machine. |
| A19 | A file holding a credential is written 0600 at creation by one shared helper, and a token selector means one entry or a refusal. | Three places stored a secret and each answered the permission question alone: the secret store and the TLS key were 0600, `.gesta/auth` was `fs::write` and therefore 0644 under the usual umask, holding serve tokens in clear unless `--hashed` was used. Setting the mode after the write leaves the file readable for the width of the write, so the mode is passed to `OpenOptions` and applies before the content lands. The TLS key path used `let _ = set_permissions`, which would have left a key at 0644 with nothing reported; it now propagates. Separately, `token rotate` took the first entry whose secret ended with the selector while `token remove` deleted every entry matching the same text, so one selector rotated one token and deleted two, and a one-character selector matched any token ending in that character. Both commands now ask `entries_named`, which matches the whole name or the whole last four characters, and `one_entry_named` refuses an ambiguous selector naming what else it matched. Tests were checked red: removing `.mode(0o600)` reports `mode was 644`, and restoring `ends_with` makes a one-character selector match. |
| A20 | A route that touches a stored asset or a served file asks the same scope question as the JSON routes, and the question is a function so it can be asserted. | Four defects on the serving surface shared one cause: the decision about who may do what lived inside the request handler, where it could only be observed through a socket. LFS fell through to `_ => None`, which the matcher reads as no credential required, so a token-protected server served every stored asset and answered every lock request anonymously; it is now `/info/lfs/` requiring Write. `POST /v1/bundle` checked the role and not the grant, so a token scoped to one branch could push to any other, and the check now runs before the bundle is decoded so a refused push writes nothing. An empty content entry in `POST /v1/changeset` is the signal to read bytes off disk and the path came from the request body, so a client could name `/etc/passwd`, have it stored as an object and read it back through the file routes; a tree path is relative with no climbing component, and anything else is refused before a read. `route()` ended its auth chain in `Box::leak`, which put the decoded credential in the process heap on every authenticated request for the life of the process. `ApplyError` had no `Display`, so the sentence a `Rejected` carries was unreadable. The LFS test was first written against a status of 400 or more and passed with the arm deleted, because a 404 also satisfies that; it now asks `required_role` directly, and deleting the arm reports `left: None, right: Some(Write)`. |
| A21 | A ref that arrives in a push body is a name only if it could only name a ref, and the pushed paths are checked against the token's path scope before the push is called applied. | The server builds `refs/heads/<name>` from the request body and `Path::join` does not sanitise, so a name of `../../../etc/passwd` resolved outside the scratch repository and an absolute name discarded the prefix outright: an authenticated push could create or overwrite any file the server process can write. `is_ref_name` applies git's own rules and runs at the parse site, so the refusal names the offending ref and nothing is written first; the write site checks again, because that is the line that would actually escape and a second caller could appear. Separately the pushed files were never checked against the token's path scope: receive-pack asked `permits_branch` and stopped, so a token allowed to write `docs/**` could push any file on any branch it could name. The paths exist only after the import, so the check lands there and rolls the branch back the way a refused branch is rolled back, keeping the `ng` line honest. Tests were checked red: removing the parse-site guard makes a climbing ref accepted. |
| A22 | Absent, empty and changed are three states, and a merge answer that needs to tell them apart keeps them apart. | `merge_branch` read every path through `Option<Hash>` and turned `None` into `String::new()`, so a file one side deleted was indistinguishable from a file one side replaced with nothing. A merge of a deletion against an untouched file produced an empty file and reported clean; the tree kept the path, now pointing at the object for `""`, whose hash is the empty-string digest. The three sides are kept as `Option<String>` and resolved before the three-way merge: a deletion against an unchanged file is a deletion, a deletion against a changed file is a conflict, and anything else falls through. `cherry_pick` already asked the question correctly by looking at the hash; this is the same question asked of the merge. `stash::apply_hunk` had the mirror of it: the span it drained was the hunk's own length, but a hunk is recorded as the lines it *added*, so a pure insertion - no base lines at all - deleted as many unrelated lines as it inserted, and `a,b,c` plus a stashed `x,y` popped back as `a,x,y`. An insertion removes nothing, and the hunk entry does not carry what it replaced, so nothing is drained. Tests were checked red: restoring the conflation leaves the deleted path in the tree as `e3b0c442...`, and restoring the length-based span yields `a,x,y`. |
| A23 | A graph walk that produces an order must produce a topological one, and a lookup that cannot fail must not be written as if it can. | `git_bridge` imported commits in reversed depth-first preorder, which is not a topological order once two branches share an ancestor: the walk from a merge M with parents B and C is `M, C, A, B` and its reversal is `B, A, C, M`, so B asked for A before A existed. The parent was written as NULL with nothing reported, so a merge history imported with its ancestry silently linearised. The order is post-order now (`place_parents_first`), parents before children across all branches at once, and a parent missing from the map is an error rather than a NULL: the miss can only mean the ordering broke, and writing NULL is what made a broken import look successful. |
| A24 | A step that has to happen before a commit happens in one function that every commit path calls. | The secret scanner, both hooks, the undo record and the audit line existed on the text form of `changeset commit` only. `--json`, `apply`, `commit-all` and `brief --fix` called `commit_one` directly, so the form agents use was the one without a security gate. The five steps are lifted into `commit_checked`, and a test reads the source to assert no other function in the file calls `commit_one`; the compiler cannot tell the two apart, and four separate call sites is evidence that a fifth will be added later. |
| A25 | A path from data is checked against the root it must stay under, on the side that writes, not only where it is decoded. | `Bundle::decode` refused an unsafe path when it parsed one, and `restore_from_snapshot` checked only for `..`. `Path::join` with an absolute value discards the base, so `/etc/cron.d/x` passed that check and was written outside the repository with the bundle's content, and the same `root.join(path)` plus `fs::write` sat in `repo::materialise` and `repo::restore_worktree`, which `pull` and `clone` reach. Decoding is one door; the writers are the other three. All of them go through `target_inside`, which asks `support::paths::is_repo_relative`, and the deletion loops ask it too because a path the tree no longer names still came from the tree. |
| A26 | A hash names bytes, so every place that computes one from a file on disk hashes the bytes and not the result of decoding them. | Two spellings of the same question drifted apart. `store::cache` hashed binary files with `hash_bytes` and text with `hash_content`, but the readers that recompute a hash to compare against a recorded one - `brief`, `pending_out_of_date`, `fsck --worktree` - wrote `hash_content(&String::from_utf8_lossy(&bytes))`. For valid UTF-8 the two agree, which is why it went unnoticed; for any other byte they differ, and every binary file in a pending snapshot was therefore always stale, never covered, and dropped by `brief --fix` while an image was reported as differing from the head it matched. The same lossy form also made importing destructive: `git_bridge` stored every blob through `put_object(&String::from_utf8_lossy(..))`, so each invalid byte became U+FFFD and the object kept was not the file - a 20 byte PNG imported as 34. And because the parent-tree hash was lossy in the same way, the round trip compared equal and hid it. A lossy conversion is only ever a display decision. |
| A27 | When a function refuses to run, it must be observable; a failure path that returns void and prints nothing is a silent data loss. | `audit::record` opened the log with `if let Ok(mut f) = ..open(..)` and no `else`. A log that could not be opened - directory gone, permissions changed, disk full - dropped every event with nothing printed and nothing returned, so an operator reading the file saw a complete-looking history that had silently stopped. The whole value of an audit log is that it is complete, which makes a missing write the one failure it may not report by omission. Both the open and the write are reported now, and the test captures stderr from a child process, because a test cannot read its own. |
| A28 | A plaintext copy of the store may not sit anywhere another user can enter, and the sidecars count as part of the copy. | The remote backup wrote the whole repository - every object, every unhashed token, the entire history - as a plain database in `std::env::temp_dir()`. `/tmp` is 1777 and `Connection::open` creates its file with the default umask, so it was world-readable for the length of the upload, and SQLite's `-wal` and `-shm` landed beside it under the same permissions. The copy now lives in a directory of its own created 0700, which covers all three files, and the directory is removed after the read and before the upload. The mode is asserted without a network call, because it is a property of the creation and not of the transfer. |
| A29 | A retention policy may only delete what it created. A prefix is a shared namespace, not a claim of ownership. | `prune` sorted whatever `list_keys(prefix)` returned and deleted the oldest `len - keep`, so a bucket holding a log archive or another tool's snapshots under the same prefix lost them and the count called them backups. Deleting data that was never yours is not a retention policy. Candidates are now keys matching the exact shape `push` writes (`gesta-<19 char stamp>.bak`), `keep == 0` is refused rather than obeyed since it is the one value that would delete every backup including the one just uploaded, and the decision is its own function because the first test asked the shape predicate directly and passed with the filter removed from `prune`. |
| A30 | A format must be able to represent its own inputs, and the one that cannot is found by round-tripping the values people actually store, not the ones that are easy to type. | The secret store was `key = value` with no escaping and with `trim()` on both sides. A value with a newline in it - a PEM block, which is the reason the feature exists - became two lines and split the entry in half; a value with a leading or trailing space lost it, and a token with a trailing space is invisible in `secret list` and wrong in a request. Version 2 escapes `\\`, newline, carriage return and `=`, writes no spaces around the separator, and marks itself with a first line. A store with no marker is a version 1 file and is still read with the old trimming, because refusing it would strand every credential already stored. |
| A31 | A diagnostic must never quote the thing it is protecting. | The parse error for a malformed store line printed the line, and the line had just been decrypted, so the one message a broken store could give was the secret it held. The message now names the line number and nothing else. This is a rule about the error, not about trimming the log: a credential never has a safe quoted form. |
| A32 | Trailing data is an error. A format that ignores what it does not understand cannot tell a valid file from a corrupted one. | `decode` split the sealed text on whitespace and read five fields, never looking at a sixth, so anything appended to the file after the ciphertext was accepted as part of a store that verified. The ciphertext is hex and the fields are fixed, so a sixth token is not padding and not a field from a future version: it is corruption or an append, and either way the file is not what it claims. The backup format had the same shape, and so did the hunk header that read `rsplitn(4)` and silently took the wrong four. |
| A33 | Refuse a name that cannot be typed back, not only a name the format cannot store. | Escaping fixed the file. The interface around it has a second condition: `secret list` prints names and `secret get` takes one, so a name holding a control character produces an entry nobody can read - it is stored, `list` shows a broken line, and `get` cannot name it. `put` now refuses control characters and empty names. `=` stays allowed because the format escapes it and `s3.access_key` is what people type. |
| A34 | A guarantee that is described and not implemented is worse than a smaller one that is, because the next person reads the comment instead of the code. | The audit header said lines were appended under a lock and gave that as the reason the log was safe. No lock existed. None is needed - one `write_all` to an `O_APPEND` descriptor is one `write`, and the line is far below `PIPE_BUF`, so the kernel does not interleave it - but the wrong reason was load-bearing in the sense that anyone auditing the file would have checked for a lock and found the comment. The header now states the real mechanism and what it does not give (no atomicity across several lines), and a test reads the header and fails if the old claim returns. |
| A35 | A field documented as not holding a secret must not hold one, even when nothing prints it yet. | `Finding.preview` was `truncate(line, 160)` and its doc said a report is read by a person and a log is not a place to put a secret. Truncation removes nothing: a short line like `password = "hunter2"` fits whole, so the field held the credential while claiming not to. No caller printed it, which is exactly why it was latent - the first `--verbose`, JSON field or debug log would have leaked the thing the field exists to catch. The secret in the line is now replaced by its redaction before the length is considered. |
| A36 | A counter nobody reads is not a record. If the design says a loss is reported, the report has to travel with the data. | The audit forwarder counted dropped events in `Counters.dropped` and nothing read the counter outside its own test, while the module header promised the count "goes into the next line that is delivered". A collector outage therefore lost events with a gap that appeared nowhere, and a history that stops without a mark looks complete. The count now rides on the next delivered line, a failed delivery puts it back to ride again, and the test drives a real collector that fails once to read the bytes on the wire. |
| A37 | A check that can be switched off by a file that arrives with a clone must say when it is off. | `.secret-allowlist` is committed with the project, so a repository listing every rule id disables the commit-time secret scanner for everyone who works in it. Nothing about that change looks like an act - it is a text file in a diff - and the refusal that would have caught a real key simply never fires. The scan now prints how many rules are off on every run, and says plainly that the scanner is off when it is all of them. A check silently not running is worse than one that is absent, because the absent one is visible. |
| A38 | Nothing may be destroyed before its replacement exists and is verified. A backup or a restore is the last thing that may delete first. | `backup` removed `dest` before copying and `restore` deleted the live store's `-wal` and `-shm` before attempting the install. Both delete first, in the two functions whose entire purpose is surviving a failure: a copy that failed left no backup at that path, not even the previous good one, and a failed install brought the database back without its WAL, so the store opened and was silently missing its last commits - measured at 506 KB of WAL in one fixture - while the error said the repository had been "put back as it was". The backup is now built in a sibling `gesta-staging` file and renamed over the destination after it verifies; the sidecars are parked beside the copy that was moved aside and returned with it, and when one cannot be returned the message says the rollback is not exact instead of claiming it is. |
| A39 | When a failure cannot be forced portably, the test asserts the property that makes the failure safe, not a simulation of the failure. | For #82 the attempt to force an install failure failed four ways: permissions do not bind because the tests run as root, `CREATE TABLE` is always `IF NOT EXISTS` so `init_schema` does not fail, a directory in the database's place is moved away by the rename that precedes the copy, and a malformed backup is refused by `verify` before the live store is touched - the first version of the test therefore passed with the bug re-inserted, because it never reached the rollback. Deleting a sidecar is what made the failure unsafe, so that is what the test reads: the source must not contain a removal of a live sidecar, and a successful restore must park the old sidecars and leave none behind. A test that cannot fail on the wrong code is not a test, and one that pretends to force a failure it cannot force is worse, because it passes for the wrong reason. |
| A40 | A row's own flag is the evidence, not the absence of a file beside it. | Fixing #95 by treating `sidecar::read` returning `None` as "the sidecar is gone" refused every object in the repository: `None` is also the ordinary answer for every inline object on a file-backed store, so nine tests across pack, gitserver, http and git_bridge failed with `no group was formed`. The `sidecar` column on the row is the only thing that says where the body was meant to live. Absence of a file is not absence of data; the row is the claim and the file is the medium. |
| A41 | A test that reads a socket once is reading a race, not a message. | The #73 test failed 2 runs in 3, then 3 in 3 under `llvm-cov`. Two causes, both the test's: `read` on a socket returns whatever has arrived, which is often the header without the body, and the test then asserted on a truncated request; and it assumed the second request was the one carrying the gap note, when the queue held 4096 events and the note rides whichever line is delivered next. The collector now reads until `Content-Length` is satisfied and keeps serving until it has seen the note, and the test looks for the note instead of predicting its position. A timing-shaped assumption in a test is the same mistake as a timing-shaped gate step. |
| A42 | A proof-of-fix that passes once has not been run. | #73 was reported done on a single green run of a test that was racy. The rerun showed it failing 2 in 3, and the coverage step in CI made it fail 3 in 3 because the instrumented binary is slower to reach the write. The rule this leaves: a test that involves a thread, a socket or a clock is run repeatedly - five times is the cheap version - before the fix is called done, and the red proof is run repeatedly too, because a red that only shows once may be the same race pointing the other way. |
| A43 | A multi-step change that describes itself as undoable must be one transaction, including its log row. | `rewrite_authors` selected, updated row by row and only then called `ops::record`, with no transaction anywhere. Its own comment says the log row is there so the rewrite is not "the one mutation with no way back"; a kill mid-loop left a branch partly anonymised and no row, so there was nothing to step back over and no record that anything had happened. Measured with a trigger that refuses the third update: 1 of 3 changesets rewritten, 0 log rows. The rule generalises: if the last step is the record, the record is inside the same transaction as the work. |
| A44 | Deferred is not a lock, and a nested BEGIN is not a nested transaction. | Two facts decided the shape of three fixes. `unchecked_transaction` is DEFERRED, so two connections can both read `MAX(id)` under a shared lock and only then both try to write, which SQLite answers with `database is locked` rather than making one wait: 8 threads x 5 records failed on every thread until `BEGIN IMMEDIATE` took the write lock before the read. And `set_branch_head` is called both with a bare connection and with a caller's open transaction, so a second BEGIN would error: it uses SAVEPOINT, which nests in both cases and rolls the tree back with the head. |
| A45 | Prove atomicity by failing the middle, not by succeeding at the end. | A test that only shows the happy path passing says nothing about whether the steps are one unit. Each of #85, #86 and #96 is proved by making a later step fail and asserting the earlier ones did not stick: a trigger refuses the third UPDATE (`1 of 3` rewritten without the fix), a trigger refuses the tree insert (`head is cs2, was cs1`), and eight threads record at once (`DatabaseBusy` without IMMEDIATE). The obstacle is a data-level failure because the container runs as root, where a permission trick fails nothing. |
| A46 | A recursion over stored data needs a bound that survives re-entry, and the test must show the crash. | The delta walk capped each query at 16 links but finished a longer chain by calling itself with no counter, so a two-link cycle in `deltas` recursed until the stack overflowed. The existing test covered `delta_depth` only, which is why the reader kept the defect. Two rules follow: a bound that a recursive call can reset is not a bound, and the test for it must run where a regression reports a failure - a stack overflow cannot be caught, so the test runs on a large stack and the crash proof runs on a default one, where it prints `fatal runtime error: stack overflow, aborting`. |
| A47 | Skipping a line a format cannot read is how a parser turns a problem into a deletion. | The snapshot encoding wrote branch names raw into tab-separated lines and `decode` dropped anything it did not recognise. `restore` deletes every branch the snapshot does not name, so a branch called `$'a\tb'` - legal, unvalidated, reachable from a shell - was deleted by the undo the user asked for to get their work back. Both halves had to change: the format escapes (V2), and an unreadable line or a missing current branch is an error. A format is not safe because the values are expected to be plain; it is safe because it carries whatever the values are. |
| A48 | Input from outside the process gets a depth bound before it gets anything else. | A few hundred kilobytes of `[[[[` overflowed the stack in the JSON parser and aborted the process. `MAX_BODY` is 512 MiB, so size is no defence, and the MCP path needs no token at all - only a client that can write to stdin. The cap is 512 levels, which is far past any shape this program writes and comfortably inside a default stack. Making the parser refuse is the point: an error the caller can report beats a process that is gone. |

## Install and delivery

| # | Decision | Reason |
|---|---|---|
| I1 | Install is one command, and a prebuilt binary is tried before asking for a toolchain. | The machine that most needs an installer is the one without the compiler. |
| I2 | The Windows installer targets PowerShell 5.1 and writes the user PATH. | Every Windows 10 and 11 machine has 5.1. Elevation is why installs get abandoned. |
| I3 | No releases, no tags. | The project ships from the branch. A version number in the manifest tracks the interface, not a packaged artifact. |

## How a number gets here

Every figure in this document carries what it takes to judge it: the command that produced it,
the spread, and whether it can be reproduced. A number without those three is a claim, and a
claim in a decision table is worse than a blank, because it gets quoted.

Three rules came out of getting this wrong repeatedly:

- **Measure the thing, not something near it.** Four attempts at measuring the tree rebuild each
  timed a different operation: disk writes, a growing table, a conflict, a branch switch. Each
  produced a plausible curve.
- **Hold one variable.** At 500 files, `land` took 29, 20 and 30 ms with 5, 50 and 200 changesets
  of history. That settled in one run what four rounds of reasoning had not: the cost follows the
  files, not the length of the history.
- **Say when a number is old.** The 2 ms for rebuilding the tree in C4 has not been reproduced and
  is marked as such. It may well be right; nobody has shown it.

## Mistakes kept on the record

These cost more time than the decisions above, so they stay written down.

| # | What happened | What it taught |
|---|---|---|
| M1 | A fixture set `parent = NULL`, so the incremental tree path was never exercised by any test. | A test that does not walk the code path it names proves nothing. |
| M2 | `commit_one` re-parented the changeset before calling `set_branch_head`, so the condition that enables the incremental path was always false. | The bug was in the order of two lines, and every test passed. |
| M3 | Two merge "fixes" were wrong before the third was right. | Compute the hunk positions by hand before changing merge logic. A few lines of Python settled in a minute what two rounds of guessing did not. |
| M4 | Four tests shared one temporary directory because the name used the process id. | A fixture that collides across threads produces failures that look like product defects. |
| M5 | An investigation blamed a traversal and a diff function for missing files; both were innocent. | Check the fixture before believing the diagnosis. `status --json` on a clean fixture settled it in one call. |
| M6 | `--branch main` was used with `cargo install --git`, which fails because the remote has no such tracking ref. | Run the installer from the published URL, not from a checkout. Three defects only appeared that way. |
| M7 | `bench/scale.py` kept the first binary it copied to `/tmp` and reused it on every later run. | A benchmark that reuses a stale copy measures the previous build and reports it as the current one. The copy is refreshed every run now, and the numbers above are from a correct run. |
| M8 | Timing the binary in place added about 16 ms per run, because it lives on a bind mount. | Read an executable from the container's own disk before timing it. The mount cost was larger than the work being measured, and it looked exactly like slow code. |
| M9 | The benchmark printed one median per size, and ten runs of the same commit on this machine ranged from 29 ms to 50 ms. | Report the spread beside the number. A single figure from a 70 percent range cannot support a claim, and the person reading it has no way to tell. The report now prints the median of three rounds and the interquartile spread of each. |
| M10 | Measuring the tree rebuild took four attempts, each wrong in a different way. | `undo` writes every path to disk, so it timed the file system. A fresh branch per sample grew the table being read. Editing one file on both sides made the land a conflict. Leaving `branch switch` inside the timer timed an operation that writes the whole tree. Each produced a plausible curve that described something other than the rebuild. What settled it was holding one variable fixed: at 500 files, `land` took 29, 20 and 30 ms with 5, 50 and 200 changesets of history, so the cost follows the files and not the history. |

## Not done, and why

| # | Not done | Reason |
|---|---|---|
| N1 | A workspace per agent by copying the tree. | The cost of a workspace is the whole tree. Sparse checkout, cheap branches and stash cover the need without a copy. |
| N2 | A local server and a SQL backend. | Both give up "one binary, one machine, no daemon". They are a separate design, not an increment. |
| N3 | A lock file for concurrent access. | It turns a working concurrent read into a failure, and a crashed process leaves it stuck with no honest way to prove it stale. WAL plus retries behaves better. |
| N4 | Guessing at another tool's behaviour to claim compatibility. | An unverified claim is worse than a missing feature. |
| N5 | Reading packfiles. | A repository whose objects are all packed has no loose objects. `git gc` gives them back. A pack reader is its own piece of work and pretending otherwise would produce a bridge that fails on the first real repository. |
| N6 | Packing several small objects into one row. | It was built and measured, and it made the repository larger: the ratio at the time went from 6.6x git to 7.7x. A pointer into a segment costs more than the seven bytes it replaces, because it is a hash, two integers and a row header. The bodies did shrink, from 98,304 bytes to a 1,959 byte segment, but `object_packed` and its index spent 172,032 bytes finding them. Recorded in docs/storage.md so the idea is not tried again without the number. The ratio on the current build is 14.9x; the packing result stands regardless of the base it was measured against. |
| N7 | Removing the per-path tables to close the storage gap. | The cost is spread over six tables that each hold a row per tracked path, and every one of them exists to make something faster: the maintained tree, the stat cache, the blame chain. Fewer rows is the fix, not smaller ones, and it costs a capability. Not done. |

## P1. A push must be inspectable before it happens

Pushing is when content leaves the machine, and a changeset carries an author string that is an address in practice. Until this existed, the only way to learn what a push disclosed was to push and look.

`remote inspect` reports the changesets, the object count, the payload size, and every author string, then separates the ones that name a person. `--check-identity` turns that into a refusal for a caller that wants a gate. A remote that cannot be asked what it already holds is reported as an estimate over the whole branch rather than guessed at, because a wrong number that looks precise is worse than an honest one.

## P2. Rewriting an author drops the signature, and says so

A signature covers the author field, because the author is part of what a reviewer is told the signer vouched for. Rewriting the author therefore invalidates it. Keeping a stale signature would turn a verified changeset into one that fails verification while still looking signed, which is worse than an unsigned changeset: the second is honest about what it proves.

## P3. A changeset id must not depend on its author

The id is derived once, when the changeset is created, and never recomputed. Everything from parent pointers to remote refs to the branch head is expressed in terms of those ids, so a rewrite that changed an id would have to rewrite the whole graph and every remote that ever received it. This is why `anonymise` can exist at all.

## P4. A replacement that is itself an identity is refused

Swapping one address for another is not anonymising. Accepting it would let somebody believe the payload is clean while it names a person exactly as before. The refusal costs nothing and prevents a false sense of safety.

## T1. A tag stores a changeset id, never the name it was given

`tag v2 main` used to store the string "main" as the changeset. The tag existed, appeared in the list, and resolved to nothing; the fault surfaced only when somebody read through it. Every revision name must be resolved before it is written, and a name that resolves to nothing must be refused rather than stored.

## T2. A tag may point at a branch, a changeset, or another tag

Following the chain at read time is what makes re-tagging honest. The walk is bounded, so a cycle written straight into the database is reported instead of hanging the process.

## T3. Blame reads its ignore list from the repository, not from local configuration

A tree-wide reformat or a symbol rename makes every line point at one changeset. True, and useless: the reader wants the change that last touched the line in substance. A list at the repository root means every clone inherits the same answer. A list that lives in local configuration gives two people two different histories of the same file, which is the one outcome a blame cannot afford.

## I1. "Nothing found" is not "nothing to find"

A review that reports no stale reference reads as a clean bill of health. That is a stronger claim than the search can support: a name built at run time or reached through an alias is invisible to a text search. Counting the places where that could happen, and saying so only when nothing was found, turns a silent gap into a stated one without hedging a definite finding.

The count comes from the same tree the search walked, so both answers describe one pass over one set of files. A count taken from a different set would answer a different question.

## G1. A check that did not run is not a check that passed

A gate reports on work nobody did when a skipped check counts as a pass. The failure is silent by construction: the run looks green, the sentence at the bottom says releasable, and the only thing wrong is that nothing was examined. Three outcomes must stay distinct, with different exit codes: passed, failed, and did not run.

## G2. A step name that matches nothing must refuse

`bench/gate.py <name>` runs one step. A mistyped name used to select nothing, print `0/0 gate steps pass`, and exit zero with the same sentence a full pass prints. Any selector must refuse a value it cannot honour, and should suggest the nearest name: a suggestion is the difference between an error somebody fixes and one they work around.

## O1. Output that was cut says where the rest is

A cap on a list leaves the reader knowing something exists and unable to see it. Every place that prints "and N more" names the command that lists all of them, and a cut line says how long it really was. The cost is a few characters; the alternative is a report nobody can act on.

## O2. Cutting a long line is opt-in, and the default is the whole line

A caller that gets a shortened line has no other place to fetch the original, so shortening by default removes information the caller cannot recover. `--max-line` exists for a caller that has decided a transcript is worth shrinking, and it states what it did.

## H1. Two layers, scored apart

A task can succeed while the tool behaved badly. Scoring only the deliverable reports that
combination as clean, and the harness benchmark literature puts the size of the gap at 31.1% of runs.
Every attempt is scored on whether the job got done and separately on five obligations about how.

A criterion must not accuse an innocent run. That is the failure that makes it useless rather than
strict: an instrument that cries wolf gets switched off. A command whose output was piped or
redirected away is not judged, and neither is a multi-line script, whose stdout belongs to no single
command inside it.

## H2. The bootstrap resamples tasks, never runs

Variance is a between-task term over the task count plus a within-task term over tasks times runs.
Adding runs shrinks only the second, and on agent work the first is usually larger. Resampling runs
would treat repeats of one easy task as independent evidence and report an interval too narrow to be
useful.

One task reports [0, 100]. A zero-width interval from a single observation is certainty nobody
earned.

## H3. A passing score needs a mutation test to mean anything

A deterministic reference answer passes every time, so the interval is zero by construction: that is
a property of the instrument, not a fact about the tool. Planting a known defect and requiring that
something catches it is what turns a score of zero defects into evidence.

Mutation found four faults in this harness that a passing run could not: a criterion reading a
truncated tail it could never parse, a criterion judging a script's concatenated output as if it
belonged to one command, and two mutants whose planted defect never reached the path they claimed.

## M1. A tool catalog is paid for on every request

Tool definitions travel with every model call, so an agent pays for the tools it never invokes. The gesta catalog is 4,426 bytes, about 1,106 estimated tokens, and it was paid again on every call because `tools/list` carried no cache hint. It now carries `ttlMs` and `cacheScope`, which the current spec requires for a client to cache at all.

The lesson is not "fewer tools". A smaller catalog that takes more steps costs more than a larger one that answers in one, and the published rebuild that cut 15 tools to 4 moved the context meter only from 1.1% to 0.8% while halving the number of calls. What matters is the cost per completed task, not the size of the list.

## M2. An argument error is not a server fault

Every tool failure used to return -32603, which tells a client the server broke. A missing `path` is the caller's mistake and JSON-RPC has -32602 for it. A client that cannot tell the two apart either retries a request that will never succeed or reports a fault that does not exist.

## M3. Tool selection is a separate thing to test

Calling a tool directly tests whether it works. It says nothing about whether anything reading twelve descriptions can tell them apart. `gesta_status` and `gesta_brief` both answer "what is going on", and a wrong choice returns a plausible answer, so the mistake never surfaces as an error.

The eval scores selection, arguments and restraint separately. Restraint matters most: a request no tool answers must be declined, because a forced fit turns "I cannot" into an action.

## S1. Blame is stored as ranges, and the author is not stored with it

A row per line repeats the changeset, the author and the timestamp for every line of every file. This project had 62,824 such rows carrying one distinct changeset, and 90% of the database was that table and its indexes. A range says the same thing once: these lines, from this changeset.

The author and the timestamp are not columns here. They have one home on `changesets`, and a reader joins for them. Duplicating a value that has one home is how a database ends up mostly repetition, and making the duplicate smaller does not compete with not writing it.

Measured: 9,236,480 bytes to 901,120 on this project, and 550,338,560 to 73,469,952 on a 10,669-file vault. Against git, 10.16x became 1.36x.
