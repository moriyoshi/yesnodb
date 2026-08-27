#!/usr/bin/env python3
"""Verify that backlog slugs cited from source are actually recorded somewhere.

Source and scripts cite backlog entries by slug -- "see some-backlog-item in
TODO.md", with the slug in backticks. A citation is a promise that the reasoning
is written down somewhere a reader can find it.

( That example is deliberately written *without* backticks. This file is inside
the globs it scans, so a backticked example slug is a real unresolved citation
and fails this check -- which it did, on the first draft of this line. An
instrument exempt from its own rule is how every sweep in this repository has
managed to report a clean tree while blind. ) Nothing checked
that promise, and on 2026-09-14 a sweep found **25 slugs cited from source that
appear nowhere in `.agents/docs/` at all**.

Why this is worth a gate step rather than review
------------------------------------------------
This repository's own backlog states the failure mode, in
`array-intersect-simd-x86`: *a backlog entry that misstates what is tracked does
not merely fail to prompt work, it actively suppresses the question.* A citation
pointing at nothing is the limiting case. Two of the ones found were load-bearing
rationale for a design gap -- `yesno-pg`'s `Transport::Local` is unimplementable
without `multiprocess-read-only-reader`, and two module headers named that slug as
the tracking record for a backlog entry that did not exist.

The cause is mechanical rather than careless, which is why a human pass will not
hold the line: `reconcile-journal-ltm` may remove JOURNAL entries once they are
consolidated into `LTM/`, and a slug dies with its entry while every citation of
it in `src/` survives untouched. Nothing links the two.

What counts as resolved
-----------------------
A slug resolves if it appears **anywhere** under `.agents/docs/` -- TODO.md,
JOURNAL.md, or any LTM document. That is deliberately weak. A stronger rule
( "must be an entry heading in TODO.md" ) would reject the many legitimate
citations of closed JOURNAL findings, and the question worth gating is not
"is this still open" but "can a reader find what this refers to". Whether a
resolved citation points at an *accurate* entry is left to review, like
`check-docs-selfcontained.py`'s symbol-path case.

The preferred repair is not the one this check measures
-------------------------------------------------------
Making the slug resolve is the *weakest* of the three available fixes, and a
gate that only says "unresolved" will steer a reader to it. **Prefer restating
the reasoning at the citation site and dropping the pointer**: prose that states
the thing cannot go stale the way a pointer does, and it removes the citation
from this check's domain entirely rather than satisfying it.

Every site repaired on 2026-09-14 already had self-sufficient prose -- the
`~0.25%` in `index/tree.rs`, the detected-never-estimated paragraph in
`stream/plan.rs`, the `flock`-and-N-backends argument in `yesno-pg`. In each the
slug was pure navigation, and the reasoning survived the pointer dying. So the
question at a failure is not "where do I record this" but "does this comment
still say what it means without the pointer".

**The ranking does not rest on those four, and keeping the two apart matters.**
Restating ranks first because of what it *does* -- it takes the citation out of
this check's domain instead of satisfying the check -- and that argument holds
however rare or common self-sufficient prose turns out to be. The four speak to
something else: how often restating is *available*. A wrong sample would move
the second and leave the first standing.

**And the sample is four, with a selection bias.** It is drawn from citations
written by people who happened to explain themselves, so it cannot say what the
general rate is. The case it is silent on is likely the harder one: a citation
that is *only* a pointer ( "see `some-slug`", with no argument beside it ), where
the reasoning has to be recovered from the entry before it can be restated, and
may not survive the entry being gone. Read "restate it" as the first thing to
check, not as advice that always applies.

Recording the entry is right when the work is genuinely open and belongs in the
backlog ( `multiprocess-read-only-reader` was ); repointing is right when the
work closed and only the reference rotted
( `crash-matrix-has-no-partial-multi-shard-commit` was ). Neither is a substitute
for the comment standing on its own.

BASELINE, and how it reached empty
----------------------------------
It started at **16** on 2026-09-14, because the debt predated the check, and
reached **zero** the same day as the remaining citations were triaged one at a
time. Three named genuinely open work and were recorded
( `planner-cost-is-o-chunks`, `follower-retention-floor-is-not-enforced`,
`pitr-retention-restores-an-empty-window-intermittently` ); the rest named work
that had closed, and were restated in place so the pointer could go.

The mechanism is kept for the same reason `check-docs-selfcontained.py` keeps
its: it is what lets the list only ever **shrink**. The check fails on new
unresolved citations, and on baseline entries resolved but not removed. Do not
add an entry to make a change pass -- restate the reasoning, or record the entry.

`NOT_SLUGS` is separate and permanent: crate names ( `aws-lc-rs` ), target
triples ( `aarch64-unknown-linux-gnu` ), tokio flavours ( `rt-multi-thread` ),
image names. Keeping the two lists apart is what let the debt list shrink to
empty without the permanent exclusions being mistaken for remaining debt.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
AGENT_DOCS = ROOT / ".agents" / "docs"

# Kebab-case tokens that are not backlog slugs and never will be. Permanent.
NOT_SLUGS: set[str] = {
    "aarch64-unknown-linux-gnu",
    "aws-lc-rs",
    "aws-sdk-ecr",
    "aws-sdk-ssm",
    "rt-multi-thread",
    "x86-64-unknown-linux-gnu",
    "yesno-operator-e2e",
    "yesno-snapshot-agent",
    "yesno-snapshot-stage",
}

# Cited slugs that resolve nowhere under `.agents/docs/`. Debt, not policy.
# Every removal from this set is a citation repaired. Additions are not allowed.
BASELINE: set[str] = set()

# Three or more hyphen-separated lowercase segments, inside backticks. Two-segment
# names are excluded: they are overwhelmingly ordinary hyphenated words and crate
# names, and the slugs in this tree are all longer.
SLUG = re.compile(r"`([a-z0-9]+(?:-[a-z0-9]+){2,})`")

SEARCH_GLOBS = (
    "yesno-*/src/**/*.rs",
    "yesno-*/tests/**/*.rs",
    "yesno-*/benches/**/*.rs",
    "scripts/*.py",
    "scripts/*.sh",
    "e2e/**/*.py",
)


def cited() -> dict[str, str]:
    """Every slug cited from source, mapped to its first site."""
    out: dict[str, str] = {}
    for glob in SEARCH_GLOBS:
        for path in sorted(ROOT.glob(glob)):
            if "target" in path.parts:
                continue
            try:
                text = path.read_text()
            except (OSError, UnicodeDecodeError):
                continue
            for lineno, line in enumerate(text.splitlines(), 1):
                for slug in SLUG.findall(line):
                    # A file path is not a slug, however kebab-shaped.
                    if "/" in slug or slug.endswith((".rs", ".py", ".sh", ".md")):
                        continue
                    out.setdefault(slug, f"{path.relative_to(ROOT)}:{lineno}")
    return out


def main() -> int:
    if not AGENT_DOCS.is_dir():
        print(f"no {AGENT_DOCS.relative_to(ROOT)}/ directory; nothing to check")
        return 0

    corpus = "\n".join(
        p.read_text(errors="replace") for p in sorted(AGENT_DOCS.rglob("*.md"))
    )

    sites = cited()
    unresolved = {s: w for s, w in sites.items() if s not in corpus and s not in NOT_SLUGS}

    new = sorted(set(unresolved) - BASELINE)
    fixed = sorted(BASELINE - set(unresolved))

    for slug in new:
        print(f"UNRECORDED: {unresolved[slug]}: `{slug}`")
    for slug in fixed:
        print(f"RESOLVED, remove from BASELINE: {slug}")

    if new:
        print(
            f"\n{len(new)} slug(s) cited from source are recorded nowhere under "
            f".agents/docs/. In preference order: (1) restate the reasoning at the "
            f"site and drop the pointer -- prose cannot dangle; (2) record the "
            f"entry, if the work is open; (3) repoint at where the reasoning lives, "
            f"if it closed. Adding to BASELINE is not the fix."
        )
    if fixed:
        print(f"\n{len(fixed)} baseline entr(y/ies) now resolve; shrink BASELINE.")
    if not new and not fixed:
        print(
            f"  every cited slug is recorded "
            f"( {len(sites)} cited, {len(BASELINE)} baselined, "
            f"{len(NOT_SLUGS)} known non-slugs )"
        )
    return 1 if (new or fixed) else 0


if __name__ == "__main__":
    sys.exit(main())
