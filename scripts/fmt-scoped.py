#!/usr/bin/env python3
"""Format or check *exactly* the named files, despite rustfmt's module recursion.

The hazard
----------

`rustfmt <file>` does not format one file. It parses the file and follows every
`mod x;` declaration into `x.rs` / `x/mod.rs`, recursively. So:

    rustfmt yesno-core/src/lib.rs

rewrites **seven** files in this tree, most of which the caller never opened.
`AGENTS.md` tells agents to "format the files you edited -- `rustfmt <file>`",
and forbids bulk-reformatting two lines later; on any module root, following the
first instruction violates the second.

`--skip-children` would confine it, but it is nightly-only ( it needs
`--unstable-features`, which stable rustfmt 1.9.0 rejects outright ). There is no
stable flag that limits rustfmt to a single file, so the containment has to be
done here.

What this fixes, concretely
---------------------------

Two real defects this script exists to remove:

  * `scripts/gate.sh` and the CI PR job both ran
    `rustfmt --check --edition 2021 $changed`. Change a `mod.rs` and the check
    reports hunks in untouched children, so the gate fails for a diff that is
    clean -- the exact "red for the wrong reason" failure mode that the comment
    above that step warns about.
  * The `fmt-baseline` counts in `JOURNAL.md`, `gate.sh` and `ci.yml`
    ( "330 hunks", later "192 across 35 files" ) were produced by passing every
    `.rs` file to rustfmt at once, which counts each child once per ancestor that
    reaches it. The real workspace figure is 89 hunks across 15 files.

Modes
-----

  check <files...>   exit non-zero if any *named* file needs formatting;
                     hunks in files reached only by recursion are ignored.
  write <files...>   format the named files, and restore any other file rustfmt
                     touched on the way. Restoration is from a copy this script
                     makes itself under the scratch dir -- never `git restore`,
                     which AGENTS.md forbids because a second agent may be
                     working in the same checkout.

Both modes print the recursion victims they suppressed, so the containment is
visible rather than silent.
"""

import pathlib
import re
import shutil
import subprocess
import sys
import tempfile

EDITION = "2021"
DIFF_IN = re.compile(r"^Diff in (.+?):\d+:")


def run_rustfmt(args: list[str]) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["rustfmt", "--edition", EDITION, *args],
        capture_output=True,
        text=True,
    )


def resolve(files: list[str]) -> list[pathlib.Path]:
    out = []
    for f in files:
        p = pathlib.Path(f).resolve()
        if not p.exists():
            print(f"fmt-scoped: no such file: {f}", file=sys.stderr)
            sys.exit(2)
        out.append(p)
    return out


def do_check(files: list[str]) -> int:
    targets = resolve(files)
    named = {str(p) for p in targets}
    proc = run_rustfmt(["--check", *[str(p) for p in targets]])

    mine, theirs = set(), set()
    for line in proc.stdout.splitlines():
        m = DIFF_IN.match(line)
        if not m:
            continue
        path = str(pathlib.Path(m.group(1)).resolve())
        (mine if path in named else theirs).add(path)

    # A rustfmt that failed to parse says so on stderr and prints no diff; that
    # must not read as "clean".
    if proc.returncode not in (0, 1) or (proc.stderr.strip() and not proc.stdout.strip()):
        sys.stderr.write(proc.stderr)
        print("fmt-scoped: rustfmt failed", file=sys.stderr)
        return 2

    if theirs:
        print(f"   ( ignored {len(theirs)} pre-existing file(s) reached by module recursion )")

    if mine:
        for p in sorted(mine):
            print(f"   needs formatting: {pathlib.Path(p).name}")
        print(f"\n{len(mine)} of the {len(named)} named file(s) need formatting.")
        return 1

    print(f"   {len(named)} named file(s) formatted correctly")
    return 0


def do_write(files: list[str]) -> int:
    targets = resolve(files)
    named = {str(p) for p in targets}

    # Ask rustfmt what it intends to rewrite rather than enumerating the tree
    # ourselves. An earlier version snapshotted `root.rglob("*.rs")` minus a few
    # excluded directories, and when a target fell in an excluded directory the
    # snapshot came back empty -- so the script rewrote a file, restored nothing,
    # and printed "nothing to change". A safety net that reports success while
    # silently doing the damage is worse than no net, and it failed exactly the
    # way the tools this session has been fixing failed: blind to its own subject.
    probe = run_rustfmt(["--check", *[str(p) for p in targets]])
    if probe.returncode not in (0, 1):
        sys.stderr.write(probe.stderr)
        print("fmt-scoped: rustfmt failed", file=sys.stderr)
        return 2

    affected = set()
    for line in probe.stdout.splitlines():
        m = DIFF_IN.match(line)
        if m:
            affected.add(pathlib.Path(m.group(1)).resolve())

    if not affected:
        print("   nothing to change")
        return 0

    backup = pathlib.Path(tempfile.mkdtemp(prefix="fmt-scoped-"))
    try:
        saved = {}
        for i, p in enumerate(sorted(affected)):
            dst = backup / f"{i}-{p.name}"
            shutil.copy2(p, dst)
            saved[p] = dst

        proc = run_rustfmt([str(p) for p in targets])
        if proc.returncode != 0:
            sys.stderr.write(proc.stderr)
            print("fmt-scoped: rustfmt failed; restoring", file=sys.stderr)
            for p, dst in saved.items():
                shutil.copy2(dst, p)
            return 2

        formatted, restored = [], []
        for p, dst in saved.items():
            if str(p) in named:
                formatted.append(p)
            else:
                shutil.copy2(dst, p)
                restored.append(p)

        for p in sorted(formatted):
            print(f"   formatted: {p.name}")
        for p in sorted(restored):
            print(f"   restored ( reached by recursion, not yours ): {p.name}")
        if not formatted:
            print("   nothing to change in the named file(s)")
        return 0
    finally:
        shutil.rmtree(backup, ignore_errors=True)


def main() -> int:
    if len(sys.argv) < 3 or sys.argv[1] not in ("check", "write"):
        print(__doc__.strip().split("\n\n")[0], file=sys.stderr)
        print("\nusage: fmt-scoped.py {check|write} <file.rs> [...]", file=sys.stderr)
        return 2
    return (do_check if sys.argv[1] == "check" else do_write)(sys.argv[2:])


if __name__ == "__main__":
    sys.exit(main())
