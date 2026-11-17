# Provenance

A change records who wrote it. That is enough when a person writes the code, because the
person remembers why. When an agent writes most of it, the reason is not in anyone's head,
and a diff shows what changed without saying why.

`session` and `intent` keep the reasoning attached to the change:

```
gesta changeset create "retry the request twice" \
  --author agent \
  --session sess-42 \
  --intent "the endpoint returns 503 under load and the caller gave up too early"
```

`--session` defaults to `GESTA_SESSION` when that variable is set, so a harness exports it
once and every change carries it.

## Where it shows up

`log` prints the intent on the one line form and both fields in the long form:

```
cs1eb9815577b0 2026-11-17 16:18 add alpha  (needed a fixture for the parser test)
```

`blame` lists the reasons behind the lines it attributed, once each rather than repeated
per line:

```
cs1eb9815577b0 agent         1 | alpha

why these lines exist:
  cs1eb9815577b0: needed a fixture for the parser test
```

## What it survives

Provenance travels in the bundle, so a clone keeps the reasoning as well as the result.
A bundle written before these fields existed still decodes, with the fields empty, which is
why an archive made by an earlier build is not refused.

An existing repository gains the two columns on first open. The migration only adds
columns, so no row is rewritten and nothing is lost.

## What it does not do

It does not enforce anything. An empty intent is allowed, because a rule that rejects a
change for missing prose teaches people to write prose that means nothing.

It does not link to the conversation itself. A session identifier is whatever the caller
puts there, and this tool does not know how to fetch a transcript; a system that stores
the conversation has to resolve the identifier.

It does not version the conversation. Storing the chat is a different problem with a much
larger footprint, and a repository that carried every transcript would be unreadable.
