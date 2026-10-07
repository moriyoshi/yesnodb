#!/usr/bin/env python3
"""Verify every declared Rust floor in the tree agrees with the workspace's.

Why this exists
---------------

On 2026-10-07 the floor moved from 1.95 to 1.98 for the AArch64 `dotprod`
intrinsics. It is written in **fourteen** places and nothing checked that they
agree, so the bump was a hand search: five manifests, four Dockerfiles, four
lines of the `msrv` CI job, and `MODULE.bazel`.

`MODULE.bazel` is the dangerous one, and is why this is a script rather than a
convention. Bazel compiles `yesno-core` and `yesno-wire` and **does not read
`rust-version`**, so a toolchain left below the floor builds nothing while
`cargo` reports success — the failure surfaces only in `gate-pg.sh`, as a
compile error inside first-party code, long after the manifests look right.

Two of the fourteen are deliberately a *different* number from the other
twelve, which is exactly why a reader cannot tell a stale site from an
intentional one by eye. Those two are listed in `INDEPENDENT` below with their
reason, and their value is asserted too: if pgrx's floor moves, this fails and
someone updates the expectation on purpose instead of discovering it later.

What it deliberately does not check
-----------------------------------

**Host state.** `rustup default` and `RUSTUP_TOOLCHAIN` decide which compiler
actually runs, and neither is in the tree. On 2026-10-07 the machine default
was 1.97.1 while the tree asked for 1.98, so a bare `cargo` — what
`scripts/gate.sh` runs, deliberately — failed outright. A pass here says the
tree is self-consistent, not that this host can build it.

**That the CI `msrv` job tests the floor's exact patch.** It runs `rustup
toolchain install 1.98`, which resolves to the channel's *latest* patch, so it
tests 1.98.1 rather than the 1.98.0 the floor literally promises. Something
depending on a 1.98.1 stabilization would keep that job green. This checks that
the job names the right floor, not that the floor is the minimum that works.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Manifests that deliberately carry a floor of their own, and why.
INDEPENDENT = {
    "yesno-pg/Cargo.toml": ("1.96", "pgrx 0.19 declares 1.96; separate workspace, built by Bazel"),
    "yesno-pg/pg18/Cargo.toml": ("1.96", "the same pgrx floor as its parent"),
}

# Manifests with no floor at all, and why that is acceptable.
NO_FLOOR = {
    "yesno-core/fuzz/Cargo.toml": "outside [workspace] members, and needs nightly regardless",
}

SKIP_DIRS = {"target", ".agents-workspace", ".git"}


def version_tuple(text):
    return tuple(int(p) for p in text.split("."))


def at_least(found, floor):
    """Is `found` >= `floor`, comparing on however many parts `floor` names?"""
    f, g = version_tuple(found), version_tuple(floor)
    width = max(len(f), len(g))
    f += (0,) * (width - len(f))
    g += (0,) * (width - len(g))
    return f >= g


def read(rel):
    return (ROOT / rel).read_text()


def main():
    floor_match = re.search(r'^rust-version = "([\d.]+)"', read("Cargo.toml"), re.M)
    if not floor_match:
        print("FAIL: Cargo.toml declares no [workspace.package] rust-version")
        return 1
    floor = floor_match.group(1)

    bad = []
    checked = 0

    # 1. Every manifest in the tree.
    for path in sorted(ROOT.rglob("Cargo.toml")):
        if any(part in SKIP_DIRS for part in path.relative_to(ROOT).parts):
            continue
        rel = str(path.relative_to(ROOT))
        text = path.read_text()
        literal = re.search(r'^rust-version = "([\d.]+)"', text, re.M)
        inherits = re.search(r"^rust-version\.workspace = true", text, re.M)

        if rel in INDEPENDENT:
            want, why = INDEPENDENT[rel]
            checked += 1
            if not literal:
                bad.append((rel, f'rust-version = "{want}"', "absent or inherited"))
            elif literal.group(1) != want:
                bad.append((rel, f"{want} ( {why} )", literal.group(1)))
        elif rel in NO_FLOOR:
            continue
        elif literal:
            checked += 1
            if literal.group(1) != floor:
                bad.append((rel, floor, literal.group(1)))
        elif inherits:
            continue  # inheriting the workspace floor is correct by construction
        else:
            bad.append((rel, f"{floor}, or rust-version.workspace = true", "no floor declared"))

    # 2. The release and E2E images.
    for rel in sorted(
        str(p.relative_to(ROOT))
        for p in ROOT.rglob("Dockerfile")
        if not any(part in SKIP_DIRS for part in p.relative_to(ROOT).parts)
    ):
        found = re.search(r"^ARG RUST_VERSION=([\d.]+)", read(rel), re.M)
        if found:
            checked += 1
            if found.group(1) != floor:
                bad.append((rel, floor, found.group(1)))

    # 3. The MODULE.bazel toolchain. This one must be at or ABOVE the floor
    #    rather than equal to it, because rules_rust needs a full patch version
    #    where `rust-version` names only major.minor.
    for found in re.finditer(r'versions = \["([\d.]+)"\]', read("MODULE.bazel")):
        checked += 1
        if not at_least(found.group(1), floor):
            bad.append(("MODULE.bazel", f">= {floor}", found.group(1)))

    # 4. The `msrv` CI job, which names the floor in four places.
    ci = read(".github/workflows/ci.yml")
    for pattern, what in (
        (r"name: msrv \(workspace builds at ([\d.]+)\)", "job name"),
        (r"rustup toolchain install ([\d.]+)", "toolchain install"),
        (r"rustc \+([\d.]+)", "rustc invocation"),
        (r"cargo \+([\d.]+) check", "cargo check invocation"),
    ):
        for found in re.finditer(pattern, ci):
            checked += 1
            if found.group(1) != floor:
                bad.append((f".github/workflows/ci.yml ( {what} )", floor, found.group(1)))

    if bad:
        print(f"FAIL: the workspace floor is {floor}, and {len(bad)} site(s) disagree:\n")
        for site, want, got in bad:
            print(f"  {site}\n      expected {want}\n      found    {got}")
        print(
            "\nEvery site must move together. Note that MODULE.bazel is not optional: "
            "Bazel\ncompiles yesno-core and ignores rust-version, so a stale toolchain "
            "there fails to\nbuild while cargo reports success."
        )
        return 1

    print(f"  Rust floor {floor} agrees across {checked} declaration site(s)")
    for rel, (want, why) in sorted(INDEPENDENT.items()):
        print(f"  {rel} independently at {want} ( {why} )")
    return 0


if __name__ == "__main__":
    sys.exit(main())
