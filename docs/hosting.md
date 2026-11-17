# Hosting

gesta writes and reads one object per branch, so any S3 compatible store works
without a server of its own. This page is about the parts of Cloudflare that fit,
what they cost, and what does not fit at all.

## R2: the useful one

R2 speaks the S3 API, so the existing remote works unchanged. Two settings differ
from a generic provider:

- **Endpoint**: `https://<ACCOUNT_ID>.r2.cloudflarestorage.com`
- **Region**: `auto`. An empty value and `us-east-1` also alias to it, for tools
  that insist on a region.

```
export GESTA_S3_ENDPOINT=https://<ACCOUNT_ID>.r2.cloudflarestorage.com
export GESTA_S3_REGION=auto
gesta secret set s3.access_key
gesta secret set s3.secret_key
gesta remote add origin s3://bucket/prefix
gesta remote push origin --branch main
gesta remote ls origin
```

Free tier, from the published pricing page: 10 GB-month of storage, 1 million
Class A operations, 10 million Class B operations, and no egress charge at any
volume.

A push is one PUT plus one GET to read the object's current head, so it is two
Class A operations. A clone is one GET. A repository of a few thousand files
bundles to a few MB, so the storage allowance holds hundreds of repositories.

`remote ls` asks the bucket with `list-type=2` and reads the keys back, which is
how a clone finds a branch without being told its name.

## Tunnel: run serve without opening a port

`gesta serve` exposes a repository over HTTP with bearer tokens and TLS. Cloudflare
Tunnel puts that behind a hostname without a public IP or a firewall rule.

A Quick Tunnel (`cloudflared tunnel --url http://localhost:8790`) needs no account
and is explicitly for testing and development, not production. For anything real,
create a named tunnel with an access policy.

Note what this does and does not buy: a tunnel removes the need to open an inbound
port, and it terminates TLS. It does not authenticate a gesta client. The token
check in `serve` is still the thing that decides who may read or write.

## D1: not a fit

D1 is SQLite, which makes it look like a match for a repository database. The
limits make it a poor one:

- **500 MB per database on the free plan**, 10 GB on paid.
- **2 MB maximum for a string, BLOB, or table row.** A bundle object stored in a
  row would hit this immediately; a large file's compressed content can exceed it.
- **100 KB maximum SQL statement**, which caps a bulk insert of changeset rows.
- **50 queries per Worker invocation on the free plan**, 1000 on paid.
- **30 second maximum query time.**

A local repository is a SQLite file with no such ceilings, and moving it into D1
would trade a fast local write for a network round trip per statement. The
asymmetry is the point of the design: the database is local and instant, the object
store is remote and rarely touched.

If a hosted read replica is ever wanted, a Worker that answers `log` and `blame`
from a D1 copy of derived tables is a reasonable shape, because those tables are
small and read only. The authoritative history would stay where it is.

## Workers: not needed for sync

A push is one PUT. A Worker in front of R2 would add a hop, a deploy step and a
limit on request duration, and change nothing about what is stored. Add one only
if a signature or an access rule has to be checked somewhere the client cannot
skip, and remember that R2 already enforces the signature.

## What to set up, in order

1. Create an R2 bucket, note the account ID, and confirm `remote push` works.
2. `remote ls` to confirm listing is allowed by the token.
3. Leave object versioning off: the bundle already contains the history, so
   versioning multiplies the same bytes.
4. If `serve` is needed outside a LAN, add a named tunnel rather than a Quick one.
