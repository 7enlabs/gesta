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
