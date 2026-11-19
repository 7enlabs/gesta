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
