#!/usr/bin/env python3
"""Verify the repository-layout diagram in ARCHITECTURE.md against the tree.

Why this exists
---------------

The layout block is the first thing anyone reads to orient — a human or an
agent — and on 2026-08-25 it had drifted far enough to mislead: five of the
eight files in `ops/` were absent, so was `stream/nary.rs`, three of the six
crates were missing, two whole test layers were missing, and `store/mod.rs`
carried the annotation `ShardFile`, a type that does not exist.

None of that is catchable by review, because a diagram that is 90% right reads
as right. It is trivially catchable mechanically, which is what this does.

What it checks, and what it deliberately does not
-------------------------------------------------

Checks **both directions**, because only one of them is the interesting one:

  * every path named in the diagram exists — catches deletions and renames;
  * every `src/*.rs` under the workspace crates appears in the diagram —
    catches *additions*, which is how the drift above actually happened. A
    one-directional check would have passed the entire time.

It does not check the annotations. A comment that describes the wrong thing is
still a real failure mode ( `ShardFile` was one ) and this cannot see it, so do
not read a pass here as "the diagram is accurate".
"""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
DOC = ROOT / ".agents/docs/ARCHITECTURE.md"

# Crates whose `src` tree the diagram is expected to enumerate file by file.
# The satellites are listed as single lines on purpose — they are separate
# crates with their own docs, and expanding them here would duplicate rather
# than orient.
ENUMERATED = ["yesno-core"]


def diagram_paths(text: str) -> set[str]:
    start = text.index("```text")
    end = text.index("```", start + 7)
    out, stack = set(), {}
    for line in text[start:end].split("\n")[1:]:
        m = re.match(r"^(\s*)([A-Za-z0-9_.\-]+)(/?)\s*(#.*)?$", line)
        if not m:
            continue
        indent, name = len(m.group(1)), m.group(2)
        stack = {k: v for k, v in stack.items() if k < indent}
        stack[indent] = name
        parts = [stack[i] for i in sorted(stack)]
        if parts and parts[0] == "yesno":
            parts = parts[1:]
        if parts:
            out.add("/".join(parts))
    return out


def main() -> int:
    text = DOC.read_text()
    named = diagram_paths(text)

    missing = sorted(p for p in named if not (ROOT / p).exists())

    undocumented = []
    for crate in ENUMERATED:
        for f in sorted((ROOT / crate / "src").rglob("*.rs")):
            rel = str(f.relative_to(ROOT))
            if rel not in named:
                undocumented.append(rel)

    for p in missing:
        print(f"  in the diagram but not on disk: {p}")
    for p in undocumented:
        print(f"  on disk but not in the diagram: {p}")

    if missing or undocumented:
        print(
            f"\n{len(missing)} stale and {len(undocumented)} undocumented path(s) "
            f"in {DOC.relative_to(ROOT)}"
        )
        return 1
    print(f"  {len(named)} paths, all present; every yesno-core source file is listed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
