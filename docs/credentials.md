# Credentials

A remote that speaks S3 needs a secret key. This is how that key is kept, and what
the tool deliberately does not do.

## Where a secret can live

Three places, in order of preference:

1. **The encrypted store.** `gesta secret set s3.secret_key` asks for the value on
   the terminal without echoing it, then writes it sealed. This is the default and
   the recommended path.
2. **An environment variable.** `GESTA_S3_SECRET_KEY`. Useful for a test, a
   container, or a CI job that injects secrets at run time. Not for a laptop that
   stays on for a week, because the variable is visible to every process that user
   starts.
3. **Nowhere.** Anonymous access to a public bucket.

The remote URL holds no credential. `s3://bucket/prefix` is the whole thing, so a
repository can be shared, pushed, or copied without carrying a key.

## A token for an HTTP remote

The same rule, which this file stated before the code obeyed it: a repository must not
carry a key. `gesta remote add <name> <url> --token <value>` writes the token to the
encrypted store under `remote.<name>.token`, never to the database. A store built
before this change has the value in `remotes.token` in clear; nothing reads it any
more, `gesta remote list` names it, and

```
gesta remote token clear origin
```

erases it and vacuums the file. Where a plaintext value and a store entry both exist,
the store entry wins.

A push takes the token from, in order:

1. `GESTA_REMOTE_TOKEN_<NAME>` - the remote name upper-cased, with anything that is not
   a letter or a digit turned into `_`. `origin` is `GESTA_REMOTE_TOKEN_ORIGIN` and
   `up-stream` is `GESTA_REMOTE_TOKEN_UP_STREAM`. This is the path for a CI job, which
   has no passphrase and should not have a key on disk.
2. The encrypted store, which needs `GESTA_SECRETS_PASSPHRASE`.
3. Nowhere. A push with no token goes anonymous, and a remote that wants one answers
   401.

With no passphrase and no environment variable, `remote add --token` refuses and writes
nothing, naming the variable to set. It does not fall back to the database, because that
fallback is the defect.

## What the store is

One file, one secret per line, sealed as a whole:

```
gesta-secrets v1 <salt> <nonce> <ciphertext>
```

- The key is derived from a passphrase with HMAC-SHA256 and a random 16 byte salt.
- The payload is sealed with ChaCha20-Poly1305 from `ring`, which authenticates as
  well as encrypts. An edited file is refused, not decrypted into rubbish.
- Every save uses a fresh salt and nonce, so two stores holding the same secrets
  are not recognisable as such.
- On Unix the file is created with mode 0600.

The file lives at `%LOCALAPPDATA%\gesta\secrets` on Windows and
`~/.config/gesta/secrets` elsewhere. Point `GESTA_SECRETS_FILE` somewhere else to
override it.

## Commands

```
gesta secret init                 # choose a passphrase, create the store
gesta secret set s3.access_key    # value read from the terminal, never an argument
gesta secret set s3.secret_key
gesta secret list                 # names only, values are never printed
gesta secret remove s3.secret_key
```

`GESTA_SECRETS_PASSPHRASE` lets a script unlock the store without a prompt. Anyone
who can read that variable can read the store, so it is for automation, not for a
desktop session.

## What this does not protect against

Being honest about the boundary is more useful than claiming more than it does.

- **A compromised user account.** The passphrase is in the environment of a process
  the attacker can inspect, or they can read the terminal. No local store survives
  this.
- **A weak passphrase.** The key derivation is a single HMAC pass, not thousands of
  rounds, because this file is opened on every push. That is fine for a passphrase
  with real entropy and useless for `12345678`. The tool refuses anything under 8
  characters and says why.
- **A secret that travelled through a chat, an issue, or a shell argument.** It is
  already out. Rotate it in the provider, then store the new one here.
- **The bucket itself.** Server side encryption protects the bytes at rest; it does
  not stop someone holding a valid key from reading everything that key can reach.
  Scope the key to one bucket, and give it read and write only if pushes need it.

## If a key leaks

1. Revoke it in the provider first. The store is not the exposure.
2. Create a new key scoped to the same bucket.
3. `gesta secret set s3.secret_key`, paste the new value.
4. `gesta remote push` to confirm, then check the object listing shows one bundle,
   not two.

Rotating the key does not touch the repository. Nothing in the history depends on
which key pushed it.

## Scoping a token to a branch

A write token may be limited to named branches, so a job that publishes to `main` cannot rewrite
`release`:

```bash
gesta remote token add write publisher --branches main
gesta remote token add write docs-bot --branches "docs-*" --scope "docs/**"
```

The auth file entry gains two optional fields at the end:

```text
<token or sha256:digest> : <role> : <name> : <path globs> : <expiry> : <branch globs>
```

Each field may be empty, and an entry written before the branch field existed grants every branch,
which is why an existing `.gesta/auth` keeps working unchanged.

## Keying the digests

A plain `sha256:` digest is `SHA-256(token)` with no secret in it, so a copy of the auth file is an
offline attack: an operator-chosen token is usually weak, and guessing it from the digest is minutes
of work. The file also happens to be the file the changeset endpoint reads, so a digest leak and a
token recovery are one step apart.

Set `GESTA_AUTH_KEY` on the server, to a value with real entropy, and give every token a keyed
digest:

```bash
export GESTA_AUTH_KEY="$(head -c 32 /dev/urandom | base64)"
gesta remote token add write ci --branches main
```

`token add` and `token rotate` then write the keyed form:

```text
sha256m:<mac> : <role> : <name> : <path globs> : <expiry> : <branch globs>
```

The mac is `HMAC-SHA256(GESTA_AUTH_KEY, "gesta-auth-v1\0" + token)`. Three properties follow, and
they are the reason for a key rather than a per-entry salt:

- **A leaked file is not a set of credentials.** Verifying a keyed entry needs the key, which lives
  in the server's environment and not in the file. A host without `GESTA_AUTH_KEY` authenticates
  nobody against a keyed file.
- **The lookup stays one hash.** The key plays the part a salt would play, but it is per deployment
  rather than per entry, so authentication is two map lookups and two HMACs rather than a scan. A
  per-entry salt would make it `O(tokens)` with a slow hash in the middle, which measures 24 ms per
  token per request with PBKDF2 at 210 000 rounds.
- **The two forms mix.** A `sha256:` entry keeps verifying while the key is set, so a deployment
  migrates one token at a time with `token rotate`. Without `GESTA_AUTH_KEY`, `token add` still
  writes the plain form and says on stderr that the digest is unkeyed.

Losing the key locks out every keyed token; that is the same trade as losing the auth file, and it
is why the key belongs wherever the other deployment secrets already live.

Two rules that are not obvious:

- **A read token is never limited by branch.** Refusing a read because a branch is not listed would
  make the field silently hide history, and reading is already covered by the path scope.
- **A refused push leaves no branch behind.** The pack is unpacked, the permitted refs move, the
  refused ref reports `ng`, and any branch the refused ref would have created is removed. Creating it
  and then reporting a refusal would put history in the repository that the caller was not allowed to
  write.

## The audit record

Every authenticated request and every refusal is one JSON line in `.gesta/audit.log`:

```json
{"action":"GET","at":"2026-11-19T16:01:28Z","decision":"no-token","reason":"missing or invalid token","target":"/v1/file","who":"anonymous"}
```

The decisions are `allowed`, `no-token`, `expired`, `wrong-role`, `path-refused`, `branch-refused`,
`bad-request` and `failed`.

The refusals are the reason the file exists. `access.log` records what succeeded, which answers "was
this token used" and never "was it tried". An audit that only holds successes shows nothing at the
moment somebody is guessing tokens.

```bash
gesta remote token audit              # everything, newest last
gesta remote token audit --refused    # only what was turned away
gesta remote token audit --limit 20   # the last 20
gesta remote token audit --json       # one document, for a script
```

A line that does not parse is kept and reported as `unreadable` rather than skipped. A log that
quietly drops the line it could not read hides the damage, which is the one thing an audit must
never do.

The file is appended to and never truncated. Rotating it is the operator's job, the same way it is
for any log.

## Rotating a token

`gesta remote token rotate <name>` prints a new secret and replaces the old one in place. The role,
the path scope, the branch scope and the name stay exactly as they were.

Rotating rather than removing and adding is not a convenience. A remove followed by an add leaves a
window where the name has no credential at all, and a person doing it by hand has to copy the scope
back from the old entry, which is the step that gets a scope wrong.

The server re-reads the auth file on every request, so a rotation takes effect immediately and no
restart is needed. The old secret is refused from that moment: `bench/token_rotate.py` checks this by
presenting both secrets to a running server rather than comparing the file, because a file that
changed says nothing about what the server accepts.

```
gesta remote token rotate buildbot
gesta remote token rotate buildbot --hashed
```
