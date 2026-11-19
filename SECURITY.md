# Security

## Reporting a vulnerability

Send a private report to the maintainer rather than opening a public issue. If the repository has a
private vulnerability reporting form enabled, use it; that keeps the report, the discussion and the
fix in one place.

A useful report has: the version or commit, the command or request that triggers the problem, what it
does, and what it should have done. A proof of concept is welcome. Do not include a real credential;
a screenshot with the value masked is enough.

There is no bounty and no response-time promise. This is a project one person maintains, and saying
so is better than writing a service-level agreement nobody can meet.

## What is in scope

- The `gesta` binary and its HTTP server.
- The token model: roles, path scope, expiry, hashing.
- TLS setup and certificate pinning.
- The secret store and the commit-time scanner.
- The git protocol endpoints served by `gesta serve`.

## What is not in scope

- A repository you serve on a reachable address with no token file and no TLS. `gesta serve` refuses
  before it opens the socket: `refusing to serve 0.0.0.0 without tokens: add entries to .gesta/auth,
  or bind 127.0.0.1`. Removing a token later and leaving the server up is the case that gets people,
  because the check runs at startup and the running process keeps what it read.
- Denial of service by a client that holds a write token. A write token can already change the
  repository; making it slow is not a new capability.
- Findings that require an attacker to already have file-system access to the repository store. At
  that point the store is readable directly and the server is not the boundary.

## Design decisions that look like weaknesses

These are deliberate. Report them if you believe the reasoning is wrong, and say why.

**A push is not authenticated beyond its token.** A write token can push to any branch the token's
path scope allows. There is no per-branch policy and no signing requirement on a push. Do not offer a
write token to someone you would not let rewrite a branch.

**Signatures prove the key, not the person.** A signed changeset proves that whoever held the private
key signed these bytes. It does not prove the author named in the changeset wrote the content, and a
signature over an author field is only as honest as the signer.

**Anonymising an author does not recall history.** Rewriting an author changes this repository. Any
copy made before the rewrite keeps the original, and nothing here can reach it.

**The scanner is a tripwire, not a vault.** It refuses a commit that looks like it carries a
credential, using patterns and an entropy floor. It cannot recognise every credential, it never sends
anything to a vendor to validate a key, and it does not rotate or store secrets. A clean scan means
"nothing matched", not "there is no secret".

**Certificates are trusted on first use when pinned.** `--pin` compares the fingerprint you supply.
That protects against a party who is not the server you expected. It does not protect the first
connection, where the fingerprint came from somewhere.

**The store is a single SQLite file.** Confidentiality of the file is the file system's job. The
secret store is sealed with a passphrase; the repository content is not encrypted.

## Handling a leaked token

`gesta remote token remove <selector>` takes an entry out of `.gesta/auth`, and the command prints
`Restart 'gesta serve' for the change to take effect.` Do that: remove, then restart the server. Until
the restart the old token is still accepted, because the server read the file when it started and
does not watch it.

Rotate anything the token could reach. The access log shows what a token asked for and cannot show
whether the person holding it was the one you gave it to, so a token that was used before removal and
one that was not look the same afterwards.
