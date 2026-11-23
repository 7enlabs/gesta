# What scion teaches a version-control tool

Scion is Google's orchestration platform for teams of AI agents: 7,862 commits, Go, Apache-2.0, and
it reached 2,100 stars in the eight months after March 2026. It is not a version-control system and
its storage model shares nothing with this store, so a feature-by-feature comparison would produce a
table and no work. What transfers is smaller and more useful: four documented refusals the project
already paid for, and one design principle about how an agent learns a CLI.

Read against this repository's code, one of the five turned out to be a real defect here, three were
cheap to adopt, and one was already done better.

## The principle worth taking: the CLI is the documentation

`.design/` carries the project's own words: agents learn the tool through `scion --help`, and the
document names the progression "MCP → SKILL.md → `<cli> --help`" as a technique that gets cheaper as
models get better. The stated goal is that a tool should be learnable by a capable reader with no
side channel.

Measured here before any change, on a fresh repository, by making the four most plausible attempts
at the shortest session a person has, which is write a file and commit it:

| attempt | result |
|---|---|
| `gesta commit -m hi` | `error: unrecognized subcommand 'commit'` |
| `gesta changeset commit -m hi` | `error: unexpected argument '-m' found` |
| `gesta changeset commit` | `error: the following required arguments were not provided: <ID>` |
| `gesta changeset create && changeset commit` | `error: unexpected argument 'changeset' found` |

Four turns, four different refusals, and none of them named the command that would have worked. The
reference answer needs an id the program already knew, and a message argument in a position nobody
guesses because every sibling tool spells it `-m`.

That is the gap this report closes first: not a missing capability, a missing sentence.

## The four operational refusals, and what happened to each

### 1. A rebase after a deletion leaves a dangling reference and reports success

Scion's `git-operations` skill states it plainly: when a PR deletes a file, a concurrent edit that
adds a *reference* to that file from a different file produces no conflict at all. Git sees two
disjoint changes, prints "Successfully rebased", and leaves the reference pointing at nothing. Only
an edit *to* the deleted file raises a conflict, because only that case forces a decision.

**Why this is not a gap here.** This store has the whole tree in an index, so the question "does
anything still mention this name" is answerable per changeset rather than by grepping. What is
missing is not the ability but the check, and the check belongs in `rebase` rather than in a
document telling agents to grep afterwards. Recorded as work, not done in this pass; the honest
statement is that a rebase here can currently land the same dangling reference and say nothing.

### 2. `git clean -fdx` is not a stronger clean

Scion separates `-fd`, which respects the ignore file, from `-fdx`, which defeats it and removes
agent state along with the work. The distinction is worth stating because the tools look like a
strength setting.

**Adopted, and the asymmetry is now explicit.** Neither `checkout` nor `restore` deletes an
untracked file, verified end to end. What changed is the argument each one requires, because the
two commands have opposite failure modes:

- `checkout` writes the branch out. It cannot lose work that was never committed, because the
  branch already holds every file it writes. Its argument is now optional and defaults to `.`.
- `restore` throws work away. A default that discards the whole tree would fire on a typo, so its
  argument stays required, and the refusal names `gesta restore .` as the form that discards
  everything. Before this pass the message was `required arguments were not provided: <FILE>` from
  the argument parser, which says nothing about the choice.

### 3. A shallow history breaks the questions that need a merge base

Scion documents the exact symptoms: `git merge-base` failing with "no merge base", two-dot ranges
walking to the graft and listing hundreds of unrelated commits, `blame` and `log -S` blind past the
boundary.

**Already better here, and it was measurable.** `clone --depth` does not exist in this store, so the
class of failure does not arise; every clone carries full history and `landed`, `merge_bases` and
`blame` read it directly. That is a refusal to add a feature rather than a feature, and it is worth
recording because "we do not have shallow clones" reads as a missing capability until the failure
mode behind it is named.

### 4. An editor variable fights the tool that sets it

Scion records that `GIT_EDITOR=true git rebase --continue` is required, and that
`git -c core.editor=true` is not enough because harnesses export `GIT_EDITOR=false` in subshells
where it overrides the config key.

**Not applicable, and the reason is structural.** No command here opens an editor, and no command
reads an editor variable. A rebase that stops takes its continuation from what is on disk. The
failure mode scion documents cannot occur because the mechanism it works around does not exist.

## What scion does better, measured, and not adopted

**Multi-agent coordination.** Scion runs separate agent processes in containers, per-agent
credentials, shared or separate worktrees, and a Hub for a team. This store has `worktree` and
`session` on a changeset, which covers the same ground at single-machine scale. Adopting a Hub means
adopting a distributed object plane, and the store is deliberately one SQLite file. The refusal is
the same one recorded for a Prolly tree in `decisions.md`: the structural change is not the first
item however large the gap looks.

**Progressive disclosure of skills as published artefacts.** Scion versions skills and resolves them
from GitHub or a registry. This store's equivalent is `--help` and the JSON contract, both of which
ship in the binary and cannot be stale. A registry adds a network dependency to reading
documentation.

## What this pass changed

Three defects, each found by running a plausible wrong argument rather than by reading code:

- `changeset commit` no longer requires an id when exactly one changeset is pending. The
  create-then-commit session is now two commands. With several pending, the id stays required and
  the refusal lists them, because picking silently commits work the caller did not name.
- `show nope` in a repository with no changesets said `no such branch, tag or changeset: main`. The
  reader typed `nope`; the message named `main`, which exists. The name had been read as a path, and
  the message now picks the reading the command actually used. There are three such questions and
  each gets its own sentence: a colon means the revision is missing, no colon with changesets means
  the file is missing at the current branch, no colon with no changesets means there is no tree to
  look in yet. The first version of this change collapsed the colon form into the file case, and the
  gate's `errors` step refused it - the message said `was read as a file` for `show nosuch:a.txt`,
  pointing the reader at a file when the name before the colon was the whole problem.
- `rebase nosuchbranch` said `no such changeset`, on an argument that accepts a branch, a tag or a
  changeset, and listed nothing to compare against. It now names all three forms and the branches
  that exist.
- `checkout` requires no argument and defaults to the whole tree; `restore` keeps its argument
  required and now refuses with the form that discards everything. The two commands have opposite
  failure modes, so they get opposite defaults, and the reason is written where the argument is
  declared.

`bench/refusals.py` pins all twenty-three behaviours and runs as gate step `refusals`.

## What was not verified

- Scion's own benchmark figures were not read or repeated. Nothing in this report depends on a
  performance number from that project.
- The dangling-reference check for rebase was reasoned from the index, not implemented. It is
  recorded as work with no measurement, because a check that has not been written has no cost to
  report.
- The four-attempt table was measured on this machine against the release binary at `a07250e`. A
  different binary will produce different messages, and the count is a property of those messages
  rather than of the tool.
