"""Tool-selection evaluation for the gesta MCP server.

A tool catalog has two jobs, and only one of them is tested by calling the tool directly. The first
is whether a tool does what it says. The second is whether anything looking at twelve descriptions
can tell them apart and pick the right one with the right arguments. The second job fails silently:
every tool works, the catalog is fine, and the agent calls `gesta_status` when it wanted
`gesta_brief`, gets an answer that is plausible, and never learns it asked the wrong question.

So each case here is a request in ordinary language, the tool that answers it, and the arguments
that request implies. The catalogue is scored on three things, and they fail for different reasons:

- **selection**: did it pick the right tool, or did it pick a neighbour that nearly fits
- **arguments**: did it fill the fields the request actually names, and leave the others alone
- **restraint**: for a request no tool answers, did it say so instead of forcing a fit

Precision and recall are separate because they fail apart. A catalog where `status` and `brief` both
look right produces low precision on both: each is chosen when the other was meant. A catalog where
one tool's description is vague produces low recall on that tool alone.

The scoring is deterministic and needs no model. It reads a selection record, which this module can
also produce by asking a caller's classifier. That separation is the point: the same cases can be
run against a model, a heuristic, or a person, and the numbers are comparable.

Run: python3 bench/harness/mcp_eval.py
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
from dataclasses import dataclass, field


@dataclass
class Case:
    """One request, the tool that answers it, and the arguments it implies."""

    request: str
    """How a person or an agent would phrase it."""

    tool: str
    """The tool that answers it."""

    args: dict = field(default_factory=dict)
    """Arguments the request names outright. Anything not listed is not asserted."""

    forbids: list[str] = field(default_factory=list)
    """Tools that nearly fit and would be the wrong answer.

    Naming them makes a failure legible: "chose status instead of brief" is a defect, while "chose
    something else" is a survey result.
    """

    why: str = ""
    """What separates the right answer from the neighbours, in one line."""


CASES: list[Case] = [
    Case(
        request="What is going on in this repository right now?",
        tool="gesta_status",
        forbids=["gesta_brief"],
        why="status reports state and the files that differ; brief reports what a session should do next",
    ),
    Case(
        request="I am starting work here. What do I need to know before my first edit?",
        tool="gesta_brief",
        forbids=["gesta_status"],
        why="the question is about what to do next, which is brief",
    ),
    Case(
        request="Show me the last five changesets.",
        tool="gesta_log",
        args={"limit": 5},
        why="log lists them newest first",
    ),
    Case(
        request="What files are on the main branch?",
        tool="gesta_ls",
        args={"rev": "main"},
        why="ls lists the files a revision holds, without checking anything out",
    ),
    Case(
        request="Read notes.txt as it was in changeset csabc123.",
        tool="gesta_read_file",
        args={"path": "notes.txt"},
        why="read_file reads one file at a revision",
    ),
    Case(
        request="Who last touched line 12 of src/main.rs?",
        tool="gesta_blame",
        args={"path": "src/main.rs"},
        why="blame answers per-line ownership",
    ),
    Case(
        request="What did changeset csabc123 change?",
        tool="gesta_diff",
        why="diff shows what one changeset changed",
    ),
    Case(
        request="If I merge feature into main, what will conflict?",
        tool="gesta_merge_conflicts",
        why="merge_conflicts reports conflicts without touching the tree",
    ),
    Case(
        request="Find every place that calls parse_config, including in files that were deleted.",
        tool="gesta_grep",
        args={"pattern": "parse_config"},
        why="grep with history searches every recorded version",
    ),
    Case(
        request="Save what I have on disk so I can commit it in a moment.",
        tool="gesta_create_changeset",
        forbids=["gesta_commit"],
        why="create snapshots as pending; commit is the second step and needs an id",
    ),
    Case(
        request="Commit the pending changeset.",
        tool="gesta_commit",
        forbids=["gesta_create_changeset"],
        why="the snapshot already exists, so the missing half is the commit",
    ),
    Case(
        request="Do the safe part of whatever the repository is waiting for.",
        tool="gesta_fix",
        forbids=["gesta_brief"],
        why="fix acts on what brief reports",
    ),
    Case(
        request="Delete the database file and rebuild the repository from scratch.",
        tool="",
        why="no tool deletes the store; a catalog that picks something here is dangerous",
    ),
]


def score(selections: list[dict]) -> dict:
    """Score a run of selections against the cases.

    `selections` is one entry per case, in order, each `{"tool": str, "args": {...}}`. An entry with
    an empty tool means the caller declined to choose.
    """
    if len(selections) != len(CASES):
        raise ValueError(
            f"expected one selection per case ({len(CASES)}), got {len(selections)}"
        )

    correct_tool = 0
    correct_args = 0
    args_asked = 0
    declined_where_none = 0
    forced_where_none = 0
    per_tool_hits: dict[str, list[int]] = {}
    confusions: list[str] = []

    for case, sel in zip(CASES, selections):
        chosen = sel.get("tool") or ""
        given = sel.get("args") or {}

        if case.tool:
            per_tool_hits.setdefault(case.tool, [0, 0])
            per_tool_hits[case.tool][1] += 1
            if chosen == case.tool:
                correct_tool += 1
                per_tool_hits[case.tool][0] += 1
            else:
                confusions.append(f"{case.request!r}: wanted {case.tool}, got {chosen or 'nothing'}")
                if chosen in case.forbids:
                    confusions[-1] += " (a neighbour that nearly fits)"

            if case.args:
                args_asked += 1
                # Only the fields the request names are asserted. A caller that fills a sensible
                # default for a field the request never mentioned is not wrong.
                if all(str(given.get(k)).lower() == str(v).lower() for k, v in case.args.items()):
                    correct_args += 1
        else:
            # The request no tool answers. Declining is the only correct behaviour, and inventing a
            # call here is the failure that matters most: it turns "I cannot" into an action.
            if chosen == "":
                declined_where_none += 1
            else:
                forced_where_none += 1
                confusions.append(f"{case.request!r}: nothing fits, yet {chosen} was chosen")

    answerable = sum(1 for c in CASES if c.tool)
    selection_acc = correct_tool / answerable if answerable else 0.0
    arg_acc = correct_args / args_asked if args_asked else 1.0

    # Recall per tool: of the requests that this tool answers, how many chose it.
    recall = {
        t: (hit / total if total else 0.0) for t, (hit, total) in per_tool_hits.items()
    }
    # Precision needs the times a tool was chosen, which the cases give for the ones that appear.
    chosen_counts: dict[str, int] = {}
    for sel in selections:
        t = sel.get("tool") or ""
        if t:
            chosen_counts[t] = chosen_counts.get(t, 0) + 1
    precision = {
        t: (per_tool_hits.get(t, [0, 0])[0] / chosen_counts[t] if chosen_counts.get(t) else 0.0)
        for t in chosen_counts
    }

    return {
        "cases": len(CASES),
        "answerable": answerable,
        "selection_accuracy": selection_acc,
        "argument_accuracy": arg_acc,
        "declined_when_nothing_fits": declined_where_none,
        "forced_a_fit_where_nothing_fits": forced_where_none,
        "recall": recall,
        "precision": precision,
        "confusions": confusions,
    }


def catalogue_cost(tools: list[dict]) -> dict:
    """What the catalog costs before a single tool is called.

    Tool definitions travel with every request, so an agent pays for the ones it never invokes. The
    estimate is bytes / 4, which is what the measurement literature uses: reliable for comparison
    and approximate in absolute terms.
    """
    total = sum(len(json.dumps(t)) for t in tools)
    return {
        "tools": len(tools),
        "bytes": total,
        "estimated_tokens": round(total / 4),
        "largest": max((len(json.dumps(t)), t.get("name", "")) for t in tools) if tools else (0, ""),
    }


def rpc(root: str, gesta: str, method: str, params: dict | None = None) -> dict:
    """One JSON-RPC call over stdio, the way a client would make it."""
    req = {"jsonrpc": "2.0", "id": 1, "method": method, "params": params or {}}
    proc = subprocess.run(
        [gesta, "mcp"],
        cwd=root,
        input=json.dumps(req) + "\n",
        capture_output=True,
        text=True,
        timeout=60,
    )
    for line in (proc.stdout or "").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            return json.loads(line)
        except json.JSONDecodeError:
            continue
    raise RuntimeError(f"no response to {method}: {(proc.stderr or proc.stdout or '')[:300]}")


def main(argv: list[str]) -> int:
    gesta = os.environ.get("GESTA_BIN") or "gesta"
    if len(argv) < 2:
        print(__doc__)
        print(f"cases: {len(CASES)}")
        return 0
    if argv[1] == "--cases":
        for c in CASES:
            print(f"{c.tool or '(nothing)':24} {c.request}")
        return 0
    if argv[1] == "--catalog":
        root = argv[2] if len(argv) > 2 else "."
        try:
            r = rpc(root, gesta, "tools/list")
        except Exception as exc:
            print(f"could not read the catalog: {exc}")
            return 2
        tools = r["result"]["tools"]
        cost = catalogue_cost(tools)
        print(f"tools            {cost['tools']}")
        print(f"bytes            {cost['bytes']}")
        print(f"estimated tokens {cost['estimated_tokens']}")
        print(f"largest          {cost['largest'][0]} bytes  {cost['largest'][1]}")
        print(f"ttlMs            {r['result'].get('ttlMs')}")
        print(f"cacheScope       {r['result'].get('cacheScope')}")
        if r["result"].get("ttlMs") is None:
            print(
                "\nno cache hint: a client that follows the spec cannot cache this catalog, so the "
                "cost above is paid on every call"
            )
            return 1
        return 0
    print(f"unknown argument {argv[1]!r}. Use --cases or --catalog <repo>")
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
