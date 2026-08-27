#!/usr/bin/env python3
"""Verify the `unsafe` counts quoted in the backlog against the tree.

Why this is a gate step when two neighbouring detectors were not
--------------------------------------------------------------
Three detectors were written across 2026-09-16. Two were retired as strictly
worse than reading: one for doc comments orphaned by an insertion ( 16 flagged,
8 false ), one for backlog entries naming vanished identifiers ( 14 flagged, 14
false, and none of the real cases ). This one is kept, and the difference is not
that it was written more carefully.

**A count is arithmetic.** It has exactly one correct value, re-deriving it is
cheaper than reading the sentence that states it, and a disagreement is a fact
rather than a judgement. "Does this claim still follow from its premise" has
none of those properties, which is why the other two could only ever generate
leads. Mechanize the arithmetic; read the judgement.

What rotted, and why a correction was not enough
-----------------------------------------------
`miri-cannot-reach-the-mmap-unsafe-sites` has quoted a wrong count **twice**.
Restored on 2026-09-14 claiming two sites, corrected the same day to 25, and
found on 2026-09-16 to be 37. Its *conclusion* was never wrong -- MIRI cannot
execute file-backed mappings, arbitrary FFI or target-specific intrinsics, which
are properties of MIRI and not of this tree -- and its file list and `unsafe fn`
count were right throughout. Only the arithmetic decayed, and it decays every
time a SIMD arm is filled in, with nothing linking the two.

That is what makes correcting it insufficient: the entry had already been
corrected once, by someone who was paying attention, and it rotted again inside
two days. A date stamp records when a number was taken but does not notice when
it stops being true.

Why `finditer` and not `search`
-------------------------------
A consumer's equivalent check used `re.search` -- match *zero or one* -- and was
correct only because exactly one matching phrase existed in the file it read. A
second appearing anywhere earlier would have silently redirected it at the wrong
sentence and still reported confidently: an instrument checking something
*adjacent* to its subject, with no outward sign. This collects every match and
fails on ambiguity, naming the lines, because a check that cannot tell which
sentence it is verifying is not verifying anything.

What this does not check
------------------------
That the entry's *reasoning* is sound, that the files it names are the right
ones, or that MIRI's limitations are as described. Those are judgement and stay
with the reader. This checks two integers.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TODO = ROOT / ".agents" / "docs" / "TODO.md"
SRC = ROOT / "yesno-core" / "src"

# The sentence this verifies. Deliberately anchored on the entry's slug so a
# rewrite that drops the numbers fails loudly rather than silently passing.
SLUG = "miri-cannot-reach-the-mmap-unsafe-sites"
CLAIM = re.compile(
    r"\*\*The uncovered surface is (\d+) `unsafe` blocks and (\d+) `unsafe fn`"
)

BLOCK = re.compile(r"\bunsafe\s*\{")
FN = re.compile(r"\bunsafe fn\b")


def count() -> tuple[int, int, dict[str, int]]:
    """Non-comment `unsafe` blocks and `unsafe fn`, with a per-file breakdown."""
    blocks = 0
    fns = 0
    per_file: dict[str, int] = {}
    for path in sorted(SRC.rglob("*.rs")):
        n = 0
        for line in path.read_text().splitlines():
            if line.lstrip().startswith("//"):
                continue
            n += len(BLOCK.findall(line))
            fns += len(FN.findall(line))
        if n:
            per_file[str(path.relative_to(ROOT))] = n
            blocks += n
    return blocks, fns, per_file


def main() -> int:
    if not TODO.is_file():
        print(f"no {TODO.relative_to(ROOT)}; nothing to check")
        return 0
    text = TODO.read_text()
    if SLUG not in text:
        print(f"`{SLUG}` is not in TODO.md; nothing to check")
        return 0

    # `findall`, not `search`. A consumer's equivalent check used `search` and
    # worked only because exactly one such phrase existed; a second appearing
    # anywhere earlier would have silently redirected it at the wrong sentence
    # and still reported confidently. Ambiguity is a failure, not a tiebreak.
    matches = list(CLAIM.finditer(text))
    if len(matches) > 1:
        lines = [text[: m.start()].count("\n") + 1 for m in matches]
        print(
            f"FAIL: {len(matches)} counted claims match, at lines "
            f"{', '.join(str(n) for n in lines)}.\n"
            f"  This check verifies exactly one sentence and cannot choose between\n"
            f"  them. Give the others a different wording, or scope this check."
        )
        return 1
    m = matches[0] if matches else None
    if not m:
        print(
            f"FAIL: `{SLUG}` is present but its counted claim was not found.\n"
            f"  Expected a sentence of the form:\n"
            f'    **The uncovered surface is N `unsafe` blocks and M `unsafe fn`\n'
            f"  Rewording the entry is fine; dropping the numbers silently is not,\n"
            f"  because the numbers are the half of it that rots."
        )
        return 1

    want_blocks, want_fns = int(m.group(1)), int(m.group(2))
    blocks, fns, per_file = count()

    if (blocks, fns) == (want_blocks, want_fns):
        # Say what was counted, not a rounder thing that reads the same. The
        # file count is of files holding *blocks*; `unsafe fn` are counted over
        # the whole tree and live in four of them.
        print(
            f"  `unsafe` counts agree ( {blocks} blocks across {len(per_file)} "
            f"files, {fns} `unsafe fn` tree-wide )"
        )
        return 0

    print(f"FAIL: `{SLUG}` disagrees with the tree.")
    print(f"  entry says: {want_blocks} blocks, {want_fns} unsafe fn")
    print(f"  tree has:   {blocks} blocks, {fns} unsafe fn")
    for f, n in per_file.items():
        print(f"    {f}: {n}")
    print(
        "\n  Update the entry. Its conclusion is probably still right -- MIRI's\n"
        "  limits are properties of MIRI -- so this is very likely arithmetic\n"
        "  rather than a finding. Stamp the new count with today's date."
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
