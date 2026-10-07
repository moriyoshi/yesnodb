#!/usr/bin/env python3
"""Recompute `docs/storage-format.md`'s size-class ladder from `CLASS_SIZES`.

Why this exists
---------------

The document states the ladder four times over, and every statement is pure
arithmetic on one array in `yesno-core`:

  * a twelve-row table of class index to slot bytes;
  * a *maximum standalone payload* column, which since the extent trailer moved
    to a table at the tail of the slab body equals the slot size exactly —
    `class_for` adds nothing to `payload_len`;
  * the superblock field list's entry for the persisted ladder, whose element
    count and whose byte range are both functions of the array's length;
  * the bitmap class's per-slab capacity, `floor(( 2 MiB - 8192 ) / ( 8192 + 8 ))`.

Reserving class 0 on 2026-10-07 required editing all of it by hand, and the same
drift was found **twice in one commit**: a row-numbering comment left pointing at
the wrong row, and a prose mention of "class-0" that survived until a human
asked about it. None of that is catchable by review, because a ladder that is
eleven-twelfths right reads as right.

What it checks, and what it deliberately does not
-------------------------------------------------

It recomputes rather than compares two copies: `CLASS_SIZES` in the source is the
only input, and every figure in the document is derived from it here. A second
hand-written list would just be a third place to drift.

It does **not** check the ladder's *values*. Whether 576 is the right second
class is a format decision with a recorded rationale, not arithmetic, and
`AGENTS.md` requires reading that rationale and journalling before touching it.
This only enforces that the document says what the code does.

It does **not** read the `//` comments beside each `CLASS_SIZES` row. Those
describe payload bounds from the superseded trailer-in-slot regime ( each is
`size - 8` ) and are a source-comment problem, tracked separately; a checker that
enforced them would be enforcing the stale regime.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "yesno-core/src/store/extent.rs"
DOC = ROOT / "docs/storage-format.md"

SLAB_BYTES = 2 * 1024 * 1024
SLAB_METADATA_BYTES = 8192
EXT_TRAILER_BYTES = 8
LADDER_FIELD_START = 76


def source_ladder(text):
    block = re.search(r"pub const CLASS_SIZES: \[u32; (\d+)\] = \[(.*?)\n\];", text, re.S)
    if not block:
        raise SystemExit("FAIL: could not find CLASS_SIZES in the source")
    declared = int(block.group(1))
    # One entry per line of the form `<number>,` before any `//` comment.
    sizes = [int(m.group(1)) for m in re.finditer(r"^\s*(\d+),", block.group(2), re.M)]
    if len(sizes) != declared:
        raise SystemExit(f"FAIL: CLASS_SIZES declares {declared} entries, {len(sizes)} parsed")
    return sizes


def doc_table(text):
    """The rows of the ladder table, as (class, slot_bytes_cell, payload_cell)."""
    rows = []
    seen_header = False
    for line in text.splitlines():
        if line.startswith("| class | slot bytes |"):
            seen_header = True
            continue
        if seen_header:
            if not line.startswith("|"):
                break
            cells = [c.strip() for c in line.strip().strip("|").split("|")]
            if set("".join(cells)) <= set("-: "):
                continue  # the alignment row
            rows.append(cells)
    return rows


def main():
    sizes = source_ladder(SRC.read_text())
    doc = DOC.read_text()
    packed = int(re.search(r"pub const PACKED_CLASS: u8 = (\d+);", SRC.read_text()).group(1))
    bad = []

    # 1. The table: one row per entry, slot bytes equal to the array, and the
    #    payload column equal to the slot size for every general class.
    rows = doc_table(doc)
    if len(rows) != len(sizes):
        bad.append(f"the ladder table has {len(rows)} rows, CLASS_SIZES has {len(sizes)} entries")
    else:
        for i, (size, cells) in enumerate(zip(sizes, rows)):
            if len(cells) < 3:
                bad.append(f"class {i}: table row has {len(cells)} columns, expected 3")
                continue
            idx, slot, payload = cells[0], cells[1], cells[2]
            if idx != str(i):
                bad.append(f"row {i}: first column reads {idx!r}, expected {i}")
            if size == 0:
                if slot != "-" or "reserved" not in payload:
                    bad.append(f"class {i} is zero in CLASS_SIZES, so the table must mark it "
                               f"reserved with no slot size; reads {slot!r} / {payload!r}")
            elif slot != str(size):
                bad.append(f"class {i}: table says slot bytes {slot!r}, CLASS_SIZES says {size}")
            elif i == packed:
                if "packed" not in payload:
                    bad.append(f"class {i} is PACKED_CLASS, so its payload cell must say so; "
                               f"reads {payload!r}")
            elif payload != str(size):
                bad.append(f"class {i}: maximum standalone payload reads {payload!r}, but the "
                           f"trailer is out of the slot so it is the slot size, {size}")

    # 2. The superblock field list. Both the element count and the byte range
    #    follow from the array's length.
    want_end = LADDER_FIELD_START + 4 * len(sizes)
    field = re.search(r"\| `(\d+)\.\.(\d+)` \| (\d+) x `u32` \| current persisted size-class ladder", doc)
    if not field:
        bad.append("could not find the superblock's persisted-ladder field row")
    else:
        lo, hi, count = int(field.group(1)), int(field.group(2)), int(field.group(3))
        if count != len(sizes):
            bad.append(f"the superblock field says {count} x u32, CLASS_SIZES has {len(sizes)}")
        if (lo, hi) != (LADDER_FIELD_START, want_end):
            bad.append(f"the ladder's byte range reads {lo}..{hi}, expected "
                       f"{LADDER_FIELD_START}..{want_end} for {len(sizes)} u32 entries")

    # 3. The bitmap class's stated per-slab capacity.
    bitmap = sizes[-1]
    want_cap = (SLAB_BYTES - SLAB_METADATA_BYTES) // (bitmap + EXT_TRAILER_BYTES)
    cap = re.search(r"The bitmap class holds (\d[\d,]*) adjacent ([\d,]+)-byte payloads", doc)
    if not cap:
        bad.append("could not find the bitmap class's per-slab capacity sentence")
    else:
        stated, payload = int(cap.group(1).replace(",", "")), int(cap.group(2).replace(",", ""))
        if payload != bitmap:
            bad.append(f"the bitmap sentence says {payload}-byte payloads, the last class is {bitmap}")
        if stated != want_cap:
            bad.append(f"the bitmap class's capacity reads {stated}, derived is {want_cap} "
                       f"= floor(( {SLAB_BYTES} - {SLAB_METADATA_BYTES} ) / ( {bitmap} + "
                       f"{EXT_TRAILER_BYTES} ))")

    # 4. The prose naming of the packed class.
    if not re.search(rf"Class {packed} is the packed-page class", doc):
        bad.append(f"PACKED_CLASS is {packed}, but the prose does not say "
                   f"'Class {packed} is the packed-page class'")

    if bad:
        print(f"FAIL: {len(bad)} ladder statement(s) in docs/storage-format.md "
              f"disagree with CLASS_SIZES:\n")
        for b in bad:
            print(f"  {b}")
        print("\nCLASS_SIZES is the only source of truth; every figure above is derived "
              "from it.")
        return 1

    general = [s for i, s in enumerate(sizes) if s > 0 and i != packed]
    print(f"  ladder agrees: {len(sizes)} classes ( 1 reserved, 1 packed, {len(general)} general ), "
          f"field {LADDER_FIELD_START}..{want_end}, bitmap capacity {want_cap}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
