#!/usr/bin/env python3
"""Check policy R1: no Arrow types in `yesno-core`'s public API.

The policy and the payoff
-------------------------

R1 says `arrow-buffer` is a private implementation detail of `yesno-core`, so
that an `arrow-buffer` **major** bump is a *patch* release of this crate. The
handoff points are supposed to live in `unstable_arrow`, which is documented as
semver-exempt.

`ARCHITECTURE.md` asserted the policy held. It did not, and had not for as long
as it had existed, because nothing checked it — the design named the mechanism
that would have ( R7, `cargo semver-checks` ) and there is no published baseline
to run it against.

Baselined, and the baseline reached empty
-----------------------------------------

BASELINE below is **empty**: R1 is now true rather than aspirational. It started
at six on 2026-08-25, and every entry left for a different reason — three were
never violations ( `pub fn` inside a `pub(crate) mod` is not public API, and the
checker could not see that until `module_is_public` walked the ancestors ), one
was dead code, one was closed by giving `NodeReader::node` an opaque `Page`, and
`store::segment::buffer_at` by making the module `pub(crate)` on 2026-08-28.

The baseline mechanism is kept because it is what lets the list only ever shrink:
this fails on violations **not** in the baseline, and on baseline entries that
have been fixed but not removed. Do not add entries to make a change pass.

What this can and cannot see
----------------------------

It is a signature scan, not a type resolution. It sees:

  * `pub fn` whose signature names an arrow type;
  * method declarations inside a `pub trait` — which have no `pub` keyword of
    their own and are the reason a first version of this script reported five
    violations when the answer was six. `NodeReader::node` is the violation that
    prompted writing it, and the script missed it.
  * `pub struct` / `pub type` aliases naming one.

It does not see a type re-exported under an alias, a public field of a public
struct, or an arrow type reached through an associated type. A pass means these
shapes are clean, not that R1 holds.
"""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
SRC = ROOT / "yesno-core/src"

ARROW = re.compile(r"\b(arrow_buffer|BooleanBuffer|ScalarBuffer|MutableBuffer|Buffer)\b")
FN = re.compile(r"^(\s*)(pub(?:\([^)]*\))?\s+)?(unsafe\s+)?(async\s+)?fn\s+(\w+)")
PUB_TRAIT = re.compile(r"^\s*pub(?:\([^)]*\))?\s+trait\s+(\w+)")
PUB_TY = re.compile(r"^\s*pub(?:\([^)]*\))?\s+(struct|type|enum)\s+")

# Known violations, as `module.rs::item`. This list may only shrink.
# The policy and the story of this baseline reaching zero are in
# ARCHITECTURE.md, 'Buffer Containment Policy ( R1 )'. This comment used to
# cite `r1-is-not-enforced` in JOURNAL.md, which stopped existing when that
# entry was consolidated into LTM; corrected 2026-09-14.
BASELINE: set[str] = set()


MOD_DECL = re.compile(r"^\s*(pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;")


def module_is_public(rel: str) -> bool:
    """Is the module holding `rel` reachable from outside the crate?

**The item's own `pub` is not enough, and assuming it was is what put
    three false entries in `BASELINE`.** A `pub fn` inside `pub(crate) mod
    buffer` is not public API — nothing outside the crate can name it. R1 is a
    statement about the *public API*, so an unreachable item is not a violation
    however it is spelled. Verified by compilation: `use yesno_core::buffer::..`
    from a satellite crate fails with "module `buffer` is private", while
    `SegmentedMmap::buffer_at` and `NodeReader::node` both resolve.

    Walks every ancestor: `store/segment.rs` needs `mod store` in `lib.rs`
    *and* `mod segment` in `store/mod.rs` to both be `pub`.
    """
    parts = rel[:-3].split("/")  # strip `.rs`
    if parts[-1] == "mod":
        parts = parts[:-1]
    parent = SRC
    for i, comp in enumerate(parts):
        decl = (parent / "lib.rs") if i == 0 else (parent / "mod.rs")
        if not decl.exists():
            decl = parent / "lib.rs"
        if not decl.exists():
            return True  # cannot tell; report rather than hide
        vis = None
        for line in decl.read_text().split("\n"):
            m = MOD_DECL.match(line)
            if m and m.group(2) == comp:
                vis = (m.group(1) or "").strip()
                break
        if vis is None:
            return True  # declared some other way; report rather than hide
        if not vis.startswith("pub") or vis.startswith("pub(crate)"):
            return False
        parent = parent / comp
    return True


def scan() -> list[tuple[str, int, str, str]]:
    found = []
    for f in sorted(SRC.rglob("*.rs")):
        rel = str(f.relative_to(SRC))
        if rel == "unstable_arrow.rs":
            continue
        # An item in a crate-private module is not public API, whatever its own
        # visibility says. See `module_is_public`.
        if not module_is_public(rel):
            continue
        text = f.read_text()
        cut = text.find("#[cfg(test)]")
        prod = text[:cut] if cut >= 0 else text

        trait_depth = None
        depth = 0
        for lineno, line in enumerate(prod.split("\n"), 1):
            stripped = line.strip()
            if not stripped.startswith("//"):
                if PUB_TRAIT.match(line) and trait_depth is None:
                    trait_depth = depth
                opens = line.count("{") - line.count("}")
                prev = depth
                depth += opens
                if trait_depth is not None and depth <= trait_depth and prev > trait_depth:
                    trait_depth = None

            if stripped.startswith("//"):
                continue
            if not ARROW.search(line):
                continue

            m = FN.match(line)
            if m:
                vis = (m.group(2) or "").strip()
                # Inside a `pub trait`, a method needs no `pub` to be public.
                public = vis.startswith("pub") and not vis.startswith("pub(crate)")
                if public or (trait_depth is not None and not vis):
                    found.append((rel, lineno, m.group(5), stripped))
            elif PUB_TY.match(line):
                name = line.split()[2].split("(")[0].split("<")[0].rstrip("{").strip()
                found.append((rel, lineno, name, stripped))
    return found


def main() -> int:
    found = scan()
    keys = {f"{rel}::{name}" for rel, _, name, _ in found}
    new = sorted(k for k in keys if k not in BASELINE)
    fixed = sorted(BASELINE - keys)

    for rel, lineno, name, text in found:
        if f"{rel}::{name}" in new:
            print(f"  NEW R1 violation  {rel}:{lineno}\n      {text}")

    if fixed:
        print("  baseline entries no longer present ( remove them from BASELINE ):")
        for k in fixed:
            print(f"      {k}")

    if new:
        print(
            f"\n{len(new)} public signature(s) name an arrow type outside "
            f"`unstable_arrow`. R1 says they must not."
        )
        return 1
    if fixed:
        print("\nR1 improved — shrink BASELINE to match.")
        return 1
    # Not "tracked in TODO.md": there is no R1 entry there, and with an empty
    # BASELINE this line used to print "0 known, tracked in TODO.md" -- a
    # citation of a record that does not exist, for a list that is empty.
    if BASELINE:
        print(f"  no new R1 violations ( {len(BASELINE)} baselined )")
    else:
        print("  no R1 violations; the baseline is empty")
    return 0


if __name__ == "__main__":
    sys.exit(main())
