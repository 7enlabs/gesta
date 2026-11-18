# The JSON is a contract, so it carries a version

An agent reads this program's output and branches on field names. Renaming a field without
saying so breaks a caller silently: it keeps running and does the wrong thing. That failure is
worse than an error, because nothing reports it.

Every object the program prints now carries `"schema": "gesta@1"`, and so does every MCP tool
result.

```sh
$ gesta brief --json
{
  "advice": "...",
  "branch": "main",
  "next": "commit-work",
  "schema": "gesta@1"
}
```

## Why a bare array stays a bare array

`log --json` and `changeset list --json` print a list at the top level. A list cannot carry a
key, and wrapping it would break the pipes people already write (`jq '.[0]'`).

The bare shape is therefore the default, and `--envelope` asks for the versioned form:

```sh
$ gesta --json log --limit 1
[ { "id": "cs21d284fd9850", ... } ]

$ gesta --json log --limit 1 --envelope
{
  "changesets": [ { "id": "cs21d284fd9850", ... } ],
  "count": 1,
  "schema": "gesta@1"
}
```

An agent that needs to refuse an unknown shape should pass `--envelope`. A shell pipeline that
wants the list should not.

## What the version promises

A change to a field's name or meaning changes the number. Adding a field does not, since a
reader that ignores unknown keys is unaffected.

The number is not a stability promise about the program, only about the shape of this document.
A version bump says "read the release notes before you trust your parser", nothing more.

## Where the idea came from

Reading a Rust rewrite toolkit that is checked against recorded behaviour, the same rule appeared
there: its JSON carries `schema: rust-anything/analyze@1` because agents read it and it is a wire
contract. The reasoning transfers exactly. A document a machine parses needs a version more than
a document a person reads, because a person notices a missing field and a parser does not.
