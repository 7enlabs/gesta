# Reviewing a stack

A change that arrives as a series of small steps should be read as a series of small
steps. Comparing only the endpoints loses the order, and when an agent wrote the change
the order carries most of the reasoning.

```
gesta review                      # every step from the start of the branch to the head
gesta review --from cs1a2b3c      # only what came after that step
gesta review --min-risk medium    # hide the steps that need no attention
```

Each step prints its id, message, author, the risk band with its score, the reasons
behind the score, and then the lines that changed for that step alone.

## Why a risk read at all

When more changes arrive than a person can read, attention has to be allocated
somehow. The alternative to a stated rule is an unstated one, which is worse, because
nobody can audit it or argue with it.

The score is built from what a reviewer looks at first:

| Signal | Points |
| --- | --- |
| More than 400 lines changed | 4 |
| More than 100 lines changed | 2 |
| More than 20 paths touched | 3 |
| More than 5 paths touched | 1 |
| Sizeable change with no test touched | 2 |
| Dependency manifest changed | 2 |
| Vendored or generated path touched | 1 |
| A binary file changed | 3 |

Bands: 0 to 3 low, 4 to 8 medium, 9 and above high.

Every point names the rule that awarded it. A score whose reason cannot be stated is
not usable for routing, because a wrong routing decision becomes unauditable.

## What the score learns from

The rules are a guess, so the score is adjusted by evidence the repository already holds:
a revert in the operation log. If a change matches the shape of something this repository
has taken back before, it gets up to three extra points, and the reason names the count.

The adjustment only ever raises a score. A rule that can quietly lower one would be able
to hide a change a person should have seen, and that is the wrong direction to be wrong in.

The match is coarse on purpose: a size band, whether a test was touched. A finer match
finds nothing in a small repository and would claim to know more than the data supports.

## What this is not

It is not a prediction. The rules encode a guess about what deserves attention, not a
measurement of what has caused incidents in this repository. A team that has that
measurement should replace the rules with it; the shape of the output stays the same.

It also does not decide anything. `review` shows a band and a threshold filters the
list. Landing a change without a person reading it is a policy decision that belongs to
whoever owns the repository, not to this tool.

## A protected branch needs a second credential

A review that a reviewer cannot enforce is advice. `gesta branch protect main` makes it a rule the
server applies to a push:

```
gesta branch protect main              # from now on a push to main needs a sign-off
gesta changeset approve cs1a2b3c --by reviewer
gesta changeset approvals cs1a2b3c     # who signed, in order
gesta branch protect main --unset      # back to normal
```

The role `approve` may read and sign off and may **not** write. That separation is the point. A
repository where the reviewer holds a write token turns the gate into a formality, because the same
credential can push the thing it just approved.

An approval from the pusher does not count. `check_push` compares the signer against the name behind
the pushing token and refuses when they match, so a change needs a reader who is somebody else.

A push that fails the gate leaves the branch where it was. This was the first thing the test found:
the import moves the branch as it creates changesets, so refusing only the `ok` line produced a push
that git reported as rejected and that had landed anyway. The refusal now puts the previous head back,
and `bench/branch_protect.py` reads the branch from the server after the refusal rather than trusting
the message.

A server running without tokens calls every pusher `anonymous`, and an anonymous pusher cannot be told
apart from a reviewer. A protected branch therefore refuses such a push and says to add a token, which
is the honest answer rather than accepting an approval nobody can check.

`bench/branch_protect.py` drives a real git client against a real server for six checks: refused with
no approval, the branch did not move, refused when the pusher approved its own change, allowed when
another name did, and an unprotected branch needs none of it.
