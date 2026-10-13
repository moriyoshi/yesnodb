#!/usr/bin/env python3
"""Fail when a script stops being invoked or a `pub fn` loses its last caller.

Why this is a script and not a procedure
----------------------------------------

This sweep has been run by hand at least six times. It works -- it found
`Db::open_reader_with_events` within a day of the symbol landing, and it found
`KeyStream::chunks_remaining`, whose body production was already writing out by
hand three screens below it. The backlog entry that owns it records its own
construction, both of its controls, and two earlier ruler corrections, and ends
with a process note saying the author rebuilt the ruler from scratch anyway.

So did the next one. On 2026-10-14 a rebuilt ruler stripped `#`-prefixed lines
as comments from `.rs` files, which removes every Rust **attribute** --
including `#[cfg(test)]`. That collapsed the production/test split and reported
`Tree::build_reusing`, the differential oracle for `build_updating`, as dead
code. Third error of the same family.

A recorded construction is a convention. This is the mechanism.

What it checks
--------------

**Scripts.** Every `scripts/*.py`, `scripts/*.sh` and `*/gate.sh` must be
invoked somewhere. The invocation search excludes Markdown and comment lines,
because a path-shaped mention is not a call: `gate.sh` explains in a `#` comment
that it no longer runs `miri.sh`. Every other file type stays in, which is what
keeps the recorded false positive false -- `build-database-artifacts.sh` is
called from `e2e/Dockerfile` and from Rust, not from a shell script.

**`pub fn` in `yesno-core/src`.** A declaration with no call outside its own
declaration, anywhere in the workspace, is a hit. References are matched as
`name(` or `name::<`; the turbofish alternative is load-bearing and removing it
re-flags real symbols.

What it deliberately does not do
--------------------------------

It does not fail on a symbol called only from tests. That is 115 names here,
most of them legitimate: a differential oracle, a constructor a property test
needs, an accessor an invariant check reads. Treating them as defects would
make the check unusable, and the backlog entry's own reading of `merge_set`
shows why -- uncalled because nothing needs it yet is a different thing from
uncalled while the cost it addresses persists.

It is a **triage lead, not a verdict**. Removing public API is a semver
decision. What this guarantees is only that a *new* orphan is noticed the day
it lands rather than at the next hand sweep.

The baselines shrink and never grow
-----------------------------------

Both lists fail on an entry that no longer applies, exactly like
`check-r1.py` and `check-docs-selfcontained.py`. Do not add an entry to make a
change pass: if a symbol has lost its last caller, either give it one or decide
to remove it.
"""

import collections
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent

# Scripts that exist without being invoked, each for a recorded reason.
BASELINE_SCRIPTS = {
    "scripts/fmt-scoped.py": "deliberately retained 2026-09-18; the bulk-format "
    "baseline it managed was closed and the tool was kept runnable",
    "scripts/gate-mount-propagation.sh": "opt-in by its own header; needs Docker "
    "privileges the routine gate does not take",
    "scripts/miri.sh": "withdrawn 2026-08-29; kept runnable, gated by nothing",
}

# `pub fn` in `yesno-core/src` with no caller outside its own declaration.
BASELINE_DEAD = {
    "into_magnitude": "an uncalled consuming accessor whose borrowing sibling "
    "serves every caller; `neg` and `abs` take `&self` and must clone, and the "
    "one `magnitude().clone()` is inside a test. A semver-decision diff, read "
    "twice on 2026-10-11 and confirmed to have no open-coded equivalent.",
}

RUST_LIKE = (".rs", ".go", ".java", ".c", ".h", ".cc", ".hpp")
SEARCHED = (".rs", ".py", ".go", ".java", ".c", ".h", ".cc", ".toml")
DECL = re.compile(r"^\s*pub fn ([a-z_][a-z0-9_]*)", re.M)
CALL = re.compile(r"\b([a-z_][a-z0-9_]*)\s*(?:\(|::<)")


def strip_comments(text: str, suffix: str) -> str:
    """Drop comment lines. The marker is per language, and that matters.

    `#` opens a comment in shell, Python, YAML and TOML, and opens an
    **attribute** in Rust. Dropping `#` lines from a `.rs` file deletes every
    `#[cfg(test)]`, which is how a rebuilt ruler once reported a differential
    oracle as dead code.
    """
    markers = ("//", "*", "/*") if suffix in RUST_LIKE else ("#", "//", "--", "*", "/*")
    return "\n".join(
        line for line in text.split("\n") if not line.lstrip().startswith(markers)
    )


def read(path: pathlib.Path) -> str | None:
    try:
        return strip_comments(path.read_text(errors="ignore"), path.suffix)
    except OSError:
        return None


def tracked() -> list[pathlib.Path]:
    out = subprocess.run(
        ["git", "ls-files", "-z"], cwd=ROOT, capture_output=True, text=True, check=True
    ).stdout.split("\0")
    return [ROOT / p for p in out if p]


def scripts_half(files: list[pathlib.Path]) -> list[str]:
    scripts = sorted(
        f
        for f in files
        if (f.parent.name == "scripts" and f.suffix in (".py", ".sh"))
        or f.name == "gate.sh"
    )
    # **This file is not part of its own corpus.** `BASELINE_SCRIPTS` names
    # every script it baselines, in string literals that no comment stripper
    # removes, so leaving it in makes each baseline look invoked by the very
    # statement that it is not. A baseline is a claim *about* invocation, not
    # an invocation.
    #
    # Found the hard way: this was verified while the file was still untracked,
    # so `git ls-files` excluded it and the defect could not appear. It
    # appeared on the first gate run after the commit. A checker whose corpus
    # is the tracked tree has to be validated from inside the tracked tree.
    me = pathlib.Path(__file__).resolve()
    blob = "\n".join(
        t
        for f in files
        if f.suffix != ".md" and f.resolve() != me and (t := read(f))
    )
    if not scripts:
        sys.exit("check-unwired: no scripts found; the ruler is broken")

    problems = []
    uninvoked = set()
    for s in scripts:
        rel = s.relative_to(ROOT).as_posix()
        own = (read(s) or "").count(s.name)
        if blob.count(rel) + blob.count(s.name) - own <= 0:
            uninvoked.add(rel)
            if rel not in BASELINE_SCRIPTS:
                problems.append(f"{rel} is never invoked, and is not baselined")
    for rel in sorted(BASELINE_SCRIPTS):
        if rel not in uninvoked:
            problems.append(
                f"{rel} is baselined as uninvoked but now has a caller; "
                f"remove it from BASELINE_SCRIPTS"
            )
    return problems


def pubfn_half(files: list[pathlib.Path]) -> tuple[list[str], int, int, int]:
    declared: collections.Counter[str] = collections.Counter()
    core = [f for f in files if f.suffix == ".rs" and "yesno-core/src/" in f.as_posix()]
    if not core:
        sys.exit("check-unwired: no yesno-core sources found; the ruler is broken")
    for f in core:
        declared.update(DECL.findall(read(f) or ""))

    # One tokenizing pass over the corpus rather than one regex scan per name:
    # 800 names against a multi-megabyte blob took 38 seconds, which is not a
    # gate step.
    calls: collections.Counter[str] = collections.Counter()
    test_calls: collections.Counter[str] = collections.Counter()
    for f in files:
        if f.suffix not in SEARCHED or (text := read(f)) is None:
            continue
        is_test_file = "/tests/" in f.as_posix() or "/benches/" in f.as_posix()
        cut = text.find("#[cfg(test)]")
        if is_test_file:
            test_calls.update(CALL.findall(text))
        elif cut == -1:
            calls.update(CALL.findall(text))
        else:
            calls.update(CALL.findall(text[:cut]))
            test_calls.update(CALL.findall(text[cut:]))

    dead, test_only = [], 0
    for name, count in declared.items():
        # The declaration is itself a `name(` match in the production half.
        if calls[name] > count:
            continue
        if test_calls[name] > 0:
            test_only += 1
        else:
            dead.append(name)

    problems = []
    for name in sorted(dead):
        if name not in BASELINE_DEAD:
            problems.append(
                f"`pub fn {name}` in yesno-core/src has no caller anywhere, "
                f"and is not baselined"
            )
    for name in sorted(BASELINE_DEAD):
        if name not in dead:
            problems.append(
                f"`{name}` is baselined as having no caller but now has one, "
                f"or is gone; remove it from BASELINE_DEAD"
            )
    return problems, len(core), sum(declared.values()), test_only


def main() -> int:
    files = tracked()
    problems = scripts_half(files)
    more, nfiles, ndecl, test_only = pubfn_half(files)
    problems += more

    if problems:
        print("check-unwired: the tree has an unwired symbol or script", file=sys.stderr)
        for p in problems:
            print(f"  {p}", file=sys.stderr)
        print(
            "\n  Read a hit twice: once as `should this exist`, and once as\n"
            "  `is production doing this already` -- the second reading is how\n"
            "  `chunks_remaining` turned out to be a duplicate rather than an\n"
            "  orphan. Removing public API is a semver decision; see\n"
            "  `rerun-the-unwired-sweep` in .agents/docs/TODO.md.",
            file=sys.stderr,
        )
        return 1

    print(
        f"  nothing newly unwired ( {nfiles} core files, {ndecl} `pub fn`, "
        f"{test_only} test-only, {len(BASELINE_DEAD)} baselined dead, "
        f"{len(BASELINE_SCRIPTS)} baselined scripts )"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
