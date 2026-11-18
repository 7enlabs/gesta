# Whether to keep S3

The question was whether the S3 remote earns its place. This records what it costs, what it
gives, and the decision.

## What it costs

| | |
|---|---|
| Code | 670 lines, `s3.rs` and `sigv4.rs` |
| Shared with other features | `cli_remote.rs` holds the push, pull and clone paths for every remote kind, so S3 adds a branch there rather than a module of its own |
| Dependencies added | none. `ring` is present for TLS already, and the signature is written by hand against the published vectors |
| Binary size | nothing measurable. The 1.6 MB difference between 4.4 and 6.06 MB comes from rustls, which the HTTPS remote needs on its own |
| Test surface | one gate step that pushes, pushes again without uploading, clones shallow, and lists branches |

The honest reading of those numbers: the marginal cost of keeping S3 is 670 lines and one gate
step. The marginal cost of removing it is losing the only remote that works with no server.

## What it gives

A repository can be stored on object storage and shared from there without a machine running
anything. That matters for the way this project is meant to be used: an agent on a laptop, work
that has to survive the laptop, no daemon and no account beyond the bucket.

The HTTPS remote needs a process listening somewhere, which means a host, a port, a token and
somebody to keep it alive. That is the wrong shape for a single person and an agent.

Measured on a real bucket, twenty gigabytes, `ap-southeast-2`: push, a second push that uploads
nothing because the changesets are already there, a full clone, a shallow clone of 200 files
from five changesets, and branch listing. All of it works, and the bundle is compressed so a
few thousand files is a few megabytes.

## Decision

Keep it, with one defect fixed.

The defect: an `http://` endpoint was accepted, so the signature and the object bodies went over
the network in the clear. The signature is not a password, but it names the key, it covers the
payload, and it can be replayed inside its signing window. A plain endpoint is now refused,
with loopback allowed because a local test server cannot be read by anyone else. This is the
right place for the check: it fires before a single byte of a repository travels.

## What would make this decision wrong

If the S3 path were the only way to share work, a defect in it would strand every user. It is
not: the HTTPS remote covers the case of two machines, and a local path covers one machine.
S3 is the third option, not the foundation.

If it grew beyond a transport into its own storage model, the cost would stop being 670 lines.
It has not, and it should not.
