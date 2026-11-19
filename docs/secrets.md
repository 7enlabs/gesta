# Secrets

A version control tool is where a leaked credential is most expensive. The commit is the record, and
a record that has to be rewritten is a record nobody can trust. So the check runs at commit time, on
the bytes that are about to become that record.

## What runs

`gesta changeset commit <id>` scans the content the changeset would store and refuses when it finds
something shaped like a credential. Nothing is written by a refused commit.

```
$ gesta changeset commit cs18bc00db4c47
gesta: 1 possible secret(s) in this changeset. Nothing was written.
  real.py  aws-access-key-id  AKIA****************

If one of these is a fixture rather than a credential, add it to .gesta/secret-allowlist:
  path real.py
```

`gesta secret scan` answers the same question for the tree, and `--history` for every stored version,
which is the finding a commit-time check cannot produce: a key that was committed and then deleted is
still in the record.

```
$ gesta secret scan --history
1 possible secret(s) in 1 object(s):
  leak.txt:1  github-pat  ghp_************************************
      entropy 4.25   fingerprint github-pat:leak.txt:6e7c8363c906f395
```

## Three decisions

**A regex plus an entropy check, and both are stated per rule.** A pattern recognises the shapes
already known, which is most of the value. Entropy is what separates `AKIAIOSFODNN7EXAMPLE` from a
real key of the same shape without calling a service. Each rule declares whether it needs entropy and
what floor, because a rule whose secret is short should not ask for it at all.

**The fingerprint does not contain the line number.** A finding keyed on `file:line` moves every time
somebody edits above it, and the allowlist entry that suppressed it yesterday stops matching today.
The fingerprint is the rule, the path, and a digest of the secret, so the same secret keeps one
identity across edits.

**Fixtures are the false positives that matter.** This repository's own tests hold a PEM header and an
example key. A scanner that refuses the project's own test suite is a scanner nobody keeps, and a
project that switches the scanner off to commit its tests has no scanner. The allowlist is part of the
format rather than an afterthought:

```
# .gesta/secret-allowlist
path src/tls.rs
rule generic-secret-assignment
stopword placeholder
```

One line per entry, the shape of an ignore file, no parser required.

The default stopword list covers the placeholders providers ship in their own documentation
(`example`, `sample`, `changeme`, `dummy`, `fake`, `000000`). A check that fires on the key from a
provider's own tutorial teaches a person to ignore it on the first day.

## What is deliberately absent

Network validation. A tool that makes a real key authenticate has sent the key somewhere, from a
machine that did not ask for it, and the request itself is the leak it was trying to prevent.

Archive traversal and decoding chains. Both are a program doing work on content on behalf of a
commit, and a build step that walks a zip file has its own trust question.

A scanner can be wrong in both directions, so it reports what it found and names the two-line fix
rather than deciding for the person reading it. A credential that is already committed has to be
reverted and then rotated, in that order: reverting alone leaves a live key in a public record.
