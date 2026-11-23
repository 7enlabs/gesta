# Pushing to a forge

GitHub, GitLab and Gitea speak git's protocol. Until this release, reaching any of them meant having
the git binary installed: the tool exported a bare repository to a temporary directory and ran
`git push` in it. That works, and it costs a process plus a full copy of the tree on every push.

`gesta forge push` makes the same three requests git makes, and needs no git anywhere on the machine.

## The command

```
GESTA_FORGE_TOKEN=... gesta forge push https://github.com/owner/repo main
```

```
pushed main to https://github.com/owner/repo
  refs/heads/main now at the commit the remote accepted
```

Flags:

- `--remote-ref refs/heads/main` sets the ref name on the remote, when it should differ from the local
  branch name.
- `--token` takes the token directly. Prefer `GESTA_FORGE_TOKEN`: a token in an argument is a token in
  the shell history and in `ps`.
- `--user` sets the username paired with the token. GitHub and Gitea ignore it. GitLab wants `oauth2`.
- `--json` prints the ref and whether the remote took it.
- `--all` pushes every branch, each to a ref of the same name, in **one** request and one pack. A
  branch the remote already holds at the same commit is skipped rather than sent again, and a push with
  nothing to send says so instead of printing a success message. Two branches need two invocations
  without this, and the second one re-sends every object the first already delivered.

```
GESTA_FORGE_TOKEN=... gesta forge push https://github.com/owner/repo --all
```

Every command in a push is acknowledged separately, and one refused ref fails the whole push naming
that ref. A client that reported the first name as if it spoke for all of them would turn a partial
push into a success message; `bench/forge_push_all.py` is what checks this, by pushing two branches and
cloning both back with git.

## What it sends, in order

1. `GET <base>/info/refs?service=git-receive-pack`: the refs the remote holds and the capabilities it
   offers. This is also where a bad token fails, before anything is sent.
2. `POST <base>/git-receive-pack`: a pkt-line command list (`<old-id> <new-id> <ref>`, with the
   capabilities after a NUL on the first line), a flush, then a pack.
3. Read the `report-status` that comes back. `ok <ref>` is a push; `ng <ref> <reason>` is a refusal and
   is reported with the remote's own words.

The pack writer is the one the server half already used to answer a clone, so there is one
implementation of what an object looks like on the wire rather than two.

## What is verified, and what is not

Verified against this tool's own server, end to end:

```
$ gesta forge push http://127.0.0.1:8791/git/fremote main
pushed main to http://127.0.0.1:8791/git/fremote

$ git clone http://127.0.0.1:8792/git/fremote gc
$ ls gc
a.txt
$ cat gc/a.txt
alpha
$ git log --oneline
f01d692 seed
```

A real git client cloned what the forge client pushed, and got the file contents back. Nine unit tests
cover the parts that decide right from wrong: an advertisement is parsed into refs and capabilities, a
ref the remote does not have answers `None` so the command carries zeroes, an HTML login page is
reported rather than read as a repository with no branches, the capabilities land on the first command
only, a rejected ref keeps its reason, a failed unpack is not overturned by an `ok` line that follows
it, and a silent remote is a failure rather than a success.

## Pushing to github.com

Verified against the real forge, with a real token, and read back by a third machine:

```
$ gesta forge push https://github.com/zencli/gesta-forge-test main
pushed main to https://github.com/zencli/gesta-forge-test

# from a separate cloud box, a git client that shares no code with this one:
$ git clone https://github.com/zencli/gesta-forge-test v && cd v
$ git log --oneline
6277039 add b.txt
63a728c add a.txt
$ git fsck --no-progress ; echo $?
0
```

Three defects had to be fixed before that worked, and all three share a cause: this client was
developed against its own server, which answers in one HTTP body and negotiates capabilities the way
the original spec describes. GitHub does neither.

- **`report-status-v2`.** GitHub advertises the v2 name and does *not* advertise plain
  `report-status`. A client that only asks for v1 negotiates nothing, the push lands, and the answer
  carries no report at all - so a correct push was reported as "the remote sent no report-status".
  v2 is a superset (`unpack`, per-ref `ok`/`ng`, plus optional `option` lines) and the existing parser
  reads it once it is negotiated. v1 stays the fallback.
- **Chunked responses.** GitHub sends both the advertisement and the report with
  `Transfer-Encoding: chunked`. A reader that takes the body as-is sees `14C\r\n001f# service=...` and
  parses `14C` as a pkt-line length, so the ref list comes back empty. The consequences chain: an
  empty advertisement makes every ref look new, the push carries `old` as forty zeroes, and GitHub
  refuses it as a non-fast-forward while the real fault is the missing de-chunk. `dechunk` runs on
  both paths now.
- **An unchanged ref was sent as `old == new`.** `push_all` filtered those, the single-branch path
  did not, and GitHub answered `ng refs/heads/main failed`. It is skipped now, which is what git does
  and what the caller meant: nothing to do.

`bench/forge_push.py` checks the local-server path always and the forge path when
`GESTA_FORGE_TOKEN` and `GESTA_FORGE_TEST_REPO` are set, skipping rather than failing when they are
absent. Every check ends in `git clone` or `git log`, never in this tool's own output, because the
question is what a git client reads, not what this client believes it sent.

## Limits

- **No force push.** The command carries the old id the remote advertised, so a non-fast-forward
  update is refused by the remote. Sending `--force` is not implemented, and adding it means sending
  the old id the client *wants*, which is a different decision from reading the one the remote gave.
- **No fetch from a forge.** This pushes. Bringing a GitHub repository down still uses the remote
  bundle protocol or `git clone`.
- **Only github.com is verified.** GitLab and Gitea take the same request shape with a different URL
  path and, for GitLab, `oauth2` as the username paired with the token; those are parameters here
  rather than assumptions, but no push has been run against either. The three defects found against
  GitHub were all in how the *server* answers rather than how git is spoken, so a forge that chunks
  differently or omits `report-status-v2` takes the v1 fallback path, which has no live test.
