#!/usr/bin/env python3
"""Verify that documents under `docs/` do not depend on source code paths.

`docs/` holds standing, human-facing documents. The tree moves underneath them,
so a source path written into prose goes stale silently — there is nothing that
recompiles a sentence. This is the opposite of the rule for `.agents/docs/`,
which is *supposed* to name source: ARCHITECTURE.md carries a module diagram
that `check-layout.py` verifies against the tree in both directions. Agent docs
track the code; `docs/` must stand on its own.

The concrete failure this exists to prevent happened on 2026-08-28: §13.9 of
`formal-model.md` said "The instrument exists ( `yesno-core/src/stats.rs` )" and
that file was deleted, leaving a published document asserting something false
with no check able to notice.

BASELINE below is **empty**, and `docs/` is clean. It started with one entry —
§6.2's citation of the plan-measure checker by filename — which was rephrased
away on 2026-08-28. The mechanism is kept because it is what lets the list only
ever shrink: the check fails on references that are **not** in the baseline, and
on baseline entries that have been fixed but not removed. Do not add entries
to make a change pass.

What this can and cannot see
---------------------------
It matches **file paths**. It does not match Rust symbol paths such as
`stream::plan::pass_b`, which are a weaker form of the same coupling — a symbol
can be renamed as silently as a file can be moved, but matching identifiers
against prose produces too many false positives to gate on. That is left to
review, deliberately.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DOCS = ROOT / "docs"

# `document::path` entries tolerated for now. Empty, and meant to stay that way.
BASELINE: set[str] = set()

# Source-bearing directories a `docs/` file must not name.
PATTERN = re.compile(
    r"""(?:yesno-[a-z0-9-]+/(?:src|benches|tests|examples)|scripts|e2e/scenarios)"""
    r"""/[A-Za-z0-9_./-]+\.(?:rs|py|sh|toml|py3)""",
)


def main() -> int:
    if not DOCS.is_dir():
        print(f"no {DOCS.relative_to(ROOT)}/ directory; nothing to check")
        return 0

    found: set[str] = set()
    where: dict[str, str] = {}
    for path in sorted(DOCS.rglob("*.md")):
        rel = path.relative_to(DOCS).as_posix()
        for lineno, line in enumerate(path.read_text().splitlines(), 1):
            for hit in PATTERN.findall(line):
                key = f"{rel}::{hit}"
                found.add(key)
                where.setdefault(key, f"{path.relative_to(ROOT)}:{lineno}")

    new = sorted(found - BASELINE)
    stale = sorted(BASELINE - found)

    for key in new:
        print(f"NEW: {where[key]}: `{key.split('::', 1)[1]}`")
    for key in stale:
        print(f"FIXED, remove from BASELINE: {key}")

    if new:
        print(
            f"\n{len(new)} new source reference(s) under docs/. State the result "
            f"rather than the location — see AGENTS.md, 'Self-Contained `docs/`'."
        )
    if stale:
        print(f"\n{len(stale)} baseline entr(y/ies) no longer present; shrink BASELINE.")
    if not new and not stale:
        print(f"docs/ self-contained; {len(BASELINE)} baselined reference(s) remain")
    return 1 if (new or stale) else 0


if __name__ == "__main__":
    sys.exit(main())
