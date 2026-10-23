#!/usr/bin/env python3
"""Keep the Flight capability bitmask identical across all four implementations.

Why this exists
---------------

`ServerStats.features` is a bitmask a client is *required* to consult before
sending a new `do_put` command: a server predating a bit reports zero and treats
an unrecognised command as insert, so an unchecked mixed batch would have its
removals applied as insertions with no error anywhere. The whole mechanism rests
on the client and the server agreeing about which bit means what.

They did not. `FEATURE_CONTAINER_PUT` ( bit 2 ) landed in `yesno-flight` on
2026-10-03 with a 64.8x write-wire reduction, and the Python, Go and Java
clients were never told. For ten days three of the four implementations could
not name a capability the server advertises on every `stats` call -- and nothing
anywhere said so, because a bitmask that is two-thirds right reads as right.

That is the same failure mode as the Flight ticket header widening from 40 to 48
bytes on 2026-09-30: a shared constant, four copies, and a convention that
someone remembers to update them together. The repair there was a literal hex
vector mirrored in four test files, which works and has the same weakness --
it only holds while someone remembers all four. This holds without anyone
remembering.

What it checks, and what it deliberately does not
-------------------------------------------------

It checks that the four sources declare the **same set of names at the same bit
positions**. `yesno-flight/src/lib.rs` is the authority: it is where the server
decides what it implements, and `FEATURES` there is what goes on the wire.

Names are compared after normalizing case and separators, because Go spells
them `FeatureMixedPut` and the others `FEATURE_MIXED_PUT`. That is a real
convention difference and not drift.

It also checks that `FEATURES` -- the union the server actually puts on the
wire -- contains every bit declared beside it. A bit that is declared and not
advertised is invisible from the client side: `supports()` returns false
forever and the caller concludes it is talking to an old server.

It does **not** check that a client can *use* a capability. Naming a bit is the
client's claim about the protocol, not about itself. When this was written, all
three non-Rust clients could ask whether the server accepts container batches
and none of them could send one; Python closed that on 2026-10-22 and Go on
2026-10-23, and Java remains open in `TODO.md`. That gap was never visible here
and still is not.

It also does not check the C ABI or the C++ client, which expose no stats
surface at all. Add them here when they grow one.
"""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent

# The authority first; every other source is compared against it.
SOURCES = [
    (
        "yesno-flight/src/lib.rs",
        re.compile(r"^pub const (FEATURE_[A-Z_]+): u64 = 1 << (\d+);", re.M),
    ),
    (
        "yesno-flight-python/src/yesnodb/client/flight.py",
        re.compile(r"^(FEATURE_[A-Z_]+) = 1 << (\d+)$", re.M),
    ),
    (
        "yesno-flight-go/schema.go",
        re.compile(r"^\s*(Feature[A-Za-z]+)\s+uint64 = 1 << (\d+)$", re.M),
    ),
    (
        "yesno-flight-java/src/main/java/dev/yesnodb/client/ServerStats.java",
        re.compile(r"^\s*public static final long (FEATURE_[A-Z_]+) = 1L << (\d+);", re.M),
    ),
]


def normalize(name: str) -> str:
    """`FEATURE_MIXED_PUT` and `FeatureMixedPut` are the same bit."""
    return re.sub(r"[^a-z0-9]", "", name.lower())


def read(path: str, pattern: re.Pattern[str]) -> dict[str, int]:
    text = (ROOT / path).read_text()
    found = {normalize(name): int(bit) for name, bit in pattern.findall(text)}
    if not found:
        # A pattern that matches nothing would otherwise make every other
        # source "agree" with an empty set, which is how a checker passes by
        # finding nothing.
        sys.exit(f"check-feature-bits: no capability bits found in {path}")
    return found


def advertised(path: str) -> set[str]:
    """The bits `FEATURES` actually puts on the wire.

    A bit can be declared and not advertised, and that failure is invisible
    from the client side: `supports()` returns false forever and the caller
    concludes the server is old. Checking the union closes it.
    """
    text = (ROOT / path).read_text()
    match = re.search(r"^pub const FEATURES: u64 = ([^;]+);", text, re.M)
    if not match:
        sys.exit(f"check-feature-bits: no FEATURES union found in {path}")
    return {normalize(name) for name in re.findall(r"FEATURE_[A-Z_]+", match.group(1))}


def main() -> int:
    authority_path, authority_pattern = SOURCES[0]
    authority = read(authority_path, authority_pattern)

    problems = []
    union = advertised(authority_path)
    for name, bit in sorted(authority.items()):
        if name not in union:
            problems.append(
                f"{authority_path} declares bit {bit} ( {name} ) but leaves it "
                f"out of FEATURES, so the server never advertises it"
            )
    for name in sorted(union - set(authority)):
        problems.append(f"{authority_path} puts {name} in FEATURES without declaring it")

    for path, pattern in SOURCES[1:]:
        bits = read(path, pattern)
        for name, bit in sorted(authority.items()):
            if name not in bits:
                problems.append(
                    f"{path} does not declare bit {bit} ( {name} ), which "
                    f"{authority_path} advertises"
                )
            elif bits[name] != bit:
                problems.append(
                    f"{path} puts {name} at bit {bits[name]}, "
                    f"{authority_path} at bit {bit}"
                )
        for name, bit in sorted(bits.items()):
            if name not in authority:
                problems.append(
                    f"{path} declares bit {bit} ( {name} ), which "
                    f"{authority_path} does not"
                )

    if problems:
        print("check-feature-bits: the capability bitmask has drifted", file=sys.stderr)
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        print(
            "\n  A client must be able to name every bit the server advertises; "
            "see the\n  module docstring for what this costs when it is wrong.",
            file=sys.stderr,
        )
        return 1

    print(
        f"  the capability bitmask agrees across {len(SOURCES)} implementations "
        f"( {len(authority)} bits )"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
