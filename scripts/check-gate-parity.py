#!/usr/bin/env python3
"""Keep `scripts/gate.sh` and `.github/workflows/ci.yml` from drifting apart.

`ci.yml` has carried a written warning not to drift from the local gate since the
first instance of it. There have now been four, and a warning is not a mechanism:

  1. `cargo clippy` lost `--workspace` in `gate.sh`, so the step reported success
     having linted two crates of eight.
  2. 2026-09-17, by hand: three checks in `gate.sh` and not in CI
     ( `check-todo-refs.py`, `check-unsafe-count.py`, `cargo doc` ). Two had been
     added by recent sessions.
  3. Those were fixed and the comparison was not automated, so nothing kept them
     in step.
  4. 2026-09-29, the other direction: the `lean core` dependency budget existed
     only in `ci.yml`. `libc` entered `yesno-core` on 2026-09-27, CI went red, and
     **25 commits landed** while every local gate run stayed green -- because the
     local gate is the one people act on and it could not see the check.
  5. 2026-10-06, and this check could not see it either: the four **client**
     gates ( Python, Go, Java, C ABI ) ran only in `ci.yml`, and the comparison
     below matched `scripts/*.py` and `scripts/*.sh` only, so every one of them
     lived outside it. The Flight ticket header widened from 40 to 48 bytes on
     2026-09-30, three clients were not widened with it, and the local gate stayed
     green for a week. Fixed in both directions: `*/gate.sh` is compared too, and
     Java grew a `gate.sh` so that it could be.

# What this compares, and what it deliberately does not

Two granularities, chosen because they are the ones the four instances actually
took, and because they are the ones that can be compared without guessing:

* **Scripts.** Every `scripts/*.py` and `scripts/*.sh` either side invokes, plus
  every per-crate `*/gate.sh` -- the client gates. This is instances 2, 3, 4 and
  5, and it is exact: a script is present or it is not. The `*/gate.sh` half is
  why a client gate must be *named* in `scripts/gate.sh` even on a host that
  cannot run it; the local gate reports such a skip rather than omitting the
  call.
* **Canonical commands.** A short list of whole-workspace commands -- clippy,
  fmt, doc, the workspace test -- compared by their *flags*. This is instance 1,
  which no script-level check could have caught: the command was in both files,
  and one of them was missing a word.

It does **not** diff the full command sets. That was tried first and is the wrong
granularity: joined continuations, `env` prefixes, `bash -c` wrappers,
`out=$( ... )` capture and host-specific `--target` triples make the two files
spell the same check differently everywhere, so the report is mostly noise. A
check that cries wolf is a check that gets a bigger baseline every time someone
is in a hurry, and then gets ignored. Narrow and exact beats broad and
approximate, for a gate whose whole purpose is that people trust it.

# The baseline shrinks and never grows

`INTENTIONAL` lists asymmetries that are meant to exist. It fails on an entry
that no longer applies, exactly like `check-r1.py` and
`check-docs-selfcontained.py`, so the list can only ever get shorter. Do not add
an entry to make a change pass: if a check belongs in both places, put it in both
places.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
GATE = ROOT / "scripts" / "gate.sh"
CI = ROOT / ".github" / "workflows" / "ci.yml"

# Asymmetries that are meant to exist. Each entry is ( where, what, why ):
# `where` is the side the check is missing from, `what` the script path.
#
# **This list is empty, and that is the finding.** The comparison was run by hand
# on 2026-09-29 expecting to freeze a messy state, and the two files already agree
# on every script and every canonical command -- the three instances found in
# 2026-09 had each been repaired, only never automated. So this starts where
# `check-r1.py` and `check-docs-selfcontained.py` took months to arrive: at zero.
# Keep it there.
INTENTIONAL: list[tuple[str, str, str]] = []

# Commands compared by their flags rather than merely by presence. The key is a
# stable name; the value matches the command's head.
CANONICAL = {
    "clippy": re.compile(r"^cargo clippy\b"),
    "fmt": re.compile(r"^cargo fmt\b"),
    "doc": re.compile(r"^cargo doc\b"),
    "test-workspace": re.compile(r"^cargo test --workspace\b"),
}


def commands(text: str) -> list[str]:
    """Shell lines with continuations joined and comment-only lines dropped."""
    text = re.sub(r"\\\n\s*", " ", text)
    out = []
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        out.append(re.sub(r"\s+", " ", line))
    return out


def normalise(command: str) -> str:
    """Strip the wrappers each file uses, leaving the command itself.

    `ci.yml` is read as **text, not YAML**, so an inline `run: cargo ...` is
    stripped here. That was measured rather than assumed: parsing the workflow
    with PyYAML and scanning it as text give identical answers on all twelve
    scripts and all four canonical commands, so the dependency bought nothing --
    and a check that must run everywhere should not need a library the CI image
    is not asked to install. A `run: |` block's body is already plain indented
    lines, which need no special handling.
    """
    c = re.sub(r"^-?\s*run:\s*[|>]?-?\s*", "", command.strip())
    c = re.sub(r"^check\s+", "", c)
    c = re.sub(r"^report\s+\d+\s+", "", c)
    c = re.sub(r"^run_measurement\s+", "", c)
    c = re.sub(r"^out=\$\(", "", c)
    c = re.sub(r"^bash -c '(.*)'$", r"\1", c)
    c = re.sub(r"^\((.*)\)$", r"\1", c)
    c = re.sub(r"^env\s+", "", c)
    # Environment assignments that precede the program.
    while re.match(r"^[A-Z_][A-Z0-9_]*=(\"[^\"]*\"|\S*)\s+", c):
        c = re.sub(r"^[A-Z_][A-Z0-9_]*=(\"[^\"]*\"|\S*)\s+", "", c)
    c = re.sub(r"^python3\s+", "", c)
    c = re.sub(r"^\./", "", c)
    c = re.sub(r"\+nightly\s+", "", c)
    return c.strip()


def gate_commands() -> list[str]:
    return [normalise(c) for c in commands(GATE.read_text())]


def ci_commands() -> list[str]:
    return [normalise(c) for c in commands(CI.read_text())]


# `scripts/<name>.{py,sh}`, and any crate-local `<dir>/gate.sh`. The second
# alternative is instance 5: the client gates are not under `scripts/`, and
# matching only the first made four checks invisible to this comparison.
SCRIPT = re.compile(r"(?:\bscripts/[A-Za-z0-9_-]+\.(?:py|sh)|[A-Za-z0-9_.+-]+/gate\.sh)\b")


def scripts_in(cmds: list[str]) -> set[str]:
    found: set[str] = set()
    for c in cmds:
        found |= set(SCRIPT.findall(c) or [])
        found |= {m.group(0) for m in SCRIPT.finditer(c)}
    return {s for s in found if (ROOT / s).exists()}


def flags_of(command: str) -> frozenset[str]:
    """The command's flags, order-insensitive. Values stay attached to their flag."""
    tokens = command.split()
    return frozenset(t for t in tokens[1:] if t.startswith("-"))


def canonical_in(cmds: list[str]) -> dict[str, set[frozenset[str]]]:
    found: dict[str, set[frozenset[str]]] = {name: set() for name in CANONICAL}
    for c in cmds:
        for name, head in CANONICAL.items():
            if head.match(c):
                found[name].add(flags_of(c))
    return found


def main() -> int:
    gate, ci = gate_commands(), ci_commands()
    gate_scripts, ci_scripts = scripts_in(gate), scripts_in(ci)

    excused = {(where, what) for where, what, _ in INTENTIONAL}
    problems: list[str] = []
    used: set[tuple[str, str]] = set()

    for missing in sorted(gate_scripts - ci_scripts):
        if ("ci", missing) in excused:
            used.add(("ci", missing))
            continue
        problems.append(f"{missing} runs in gate.sh and in no CI job")
    for missing in sorted(ci_scripts - gate_scripts):
        if ("gate", missing) in excused:
            used.add(("gate", missing))
            continue
        problems.append(f"{missing} runs in CI and in no gate.sh step")

    gate_canon, ci_canon = canonical_in(gate), canonical_in(ci)
    for name in sorted(CANONICAL):
        g, c = gate_canon[name], ci_canon[name]
        if not g and not c:
            problems.append(f"`{name}` runs in neither gate.sh nor CI")
            continue
        if not g:
            problems.append(f"`{name}` runs in CI and in no gate.sh step")
            continue
        if not c:
            problems.append(f"`{name}` runs in gate.sh and in no CI job")
            continue
        if g != c:
            only_gate = sorted(set().union(*g) - set().union(*c))
            only_ci = sorted(set().union(*c) - set().union(*g))
            if only_gate or only_ci:
                problems.append(
                    f"`{name}` is invoked with different flags: "
                    f"only in gate.sh {only_gate or '[]'}, only in CI {only_ci or '[]'}"
                )

    # A baseline entry that no longer applies is itself a failure, so the list
    # can only shrink.
    for where, what, _ in INTENTIONAL:
        if (where, what) not in used:
            problems.append(
                f"the INTENTIONAL entry for {what!r} ({where}) no longer applies; remove it"
            )

    if problems:
        print("gate.sh and ci.yml have drifted:")
        for p in problems:
            print(f"  {p}")
        print(
            "\n  A check belongs in both places. Do not add an INTENTIONAL entry\n"
            "  to silence this; see the module docstring for why the local gate\n"
            "  is the one that has to be complete."
        )
        return 1

    print(
        f"  gate.sh and ci.yml agree ( {len(gate_scripts & ci_scripts)} shared scripts, "
        f"{len(CANONICAL)} canonical commands, {len(INTENTIONAL)} intentional asymmetries )"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
