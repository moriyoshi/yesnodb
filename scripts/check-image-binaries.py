#!/usr/bin/env python3
"""Verify the unified image's binary set against the crates that produce it.

Why this exists
---------------

The unified image ( `dist/Dockerfile` ) has the binary list written down three
times, and nothing but this connects them:

  * the `[[bin]]` targets of `yesno-server`, `yesno-server-utils` and
    `yesno-operator` -- what the workspace actually builds;
  * the `for binary in ...` loop in `dist/build.Dockerfile` -- what the
    cross-compiling stage exports;
  * the `case` arm in `dist/entrypoint.sh` -- what can be selected by name.

A new binary added to a crate and to the builder but not to the entrypoint
produces an image that carries it and cannot run it: `docker run <image>
yesno-newthing` falls through to the `*` arm and silently starts `yesnod` with
`yesno-newthing` as an argument, which `yesnod` rejects as an unexpected
argument. That is a confusing failure a long way from its cause, and it is
exactly the drift a three-way copy invites.

The reverse direction matters as much: an entrypoint arm naming a binary the
image does not contain is a dispatch to a path that does not exist.

EXCLUDED is **empty**: every binary the three crates build is in the image,
`yesno-snapshot-agent` included. It was excluded until 2026-09-06, on the
grounds that it is the only binary needing LVM userspace and the only one that
runs as uid 0 -- both still true, and neither turned out to be a packaging
argument. The privilege is granted by the *deployment* to one container, not by
the image, and the LVM userspace is unpacked for the target architecture in a
native stage rather than installed under emulation.

The mechanism is kept for the next such decision. An entry here must carry
the reason, and this file then asserts the omission in both directions -- so an
excluded binary that quietly reappears in the image is a failure, not a shrug.
Do not silence a failure by adding an entry. The list is what the image
promises, not a place to park a mismatch.
"""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent

# Crates whose `[[bin]]` targets the unified image is expected to carry.
CRATES = ["yesno-server", "yesno-server-utils", "yesno-operator"]

# Binaries built by those crates that the unified image deliberately omits,
# each with the reason it is omitted.
EXCLUDED: dict[str, str] = {}


def crate_binaries() -> set[str]:
    """Every `[[bin]]` name declared by the crates the image builds from."""
    found: set[str] = set()
    for crate in CRATES:
        manifest = ROOT / crate / "Cargo.toml"
        if not manifest.is_file():
            print(f"check-image-binaries: {crate}/Cargo.toml is missing", file=sys.stderr)
            sys.exit(1)
        text = manifest.read_text(encoding="utf-8")
        for block in re.split(r"^\[\[bin\]\]\s*$", text, flags=re.MULTILINE)[1:]:
            match = re.search(r'^\s*name\s*=\s*"([^"]+)"', block, flags=re.MULTILINE)
            if match:
                found.add(match.group(1))
        # `src/main.rs` is auto-discovered as a binary named after the package,
        # which is how `yesno-operator` is written -- but only when no explicit
        # `[[bin]]` has already claimed that path, which is how `yesno-server`
        # gets `yesnod` and no `yesno-server` binary.
        claims_main = re.search(r'^\s*path\s*=\s*"src/main\.rs"', text, flags=re.MULTILINE)
        if (ROOT / crate / "src" / "main.rs").is_file() and not claims_main:
            name = re.search(r'^\s*name\s*=\s*"([^"]+)"', text, flags=re.MULTILINE)
            if name:
                found.add(name.group(1))
    return found


def dockerfile_binaries() -> set[str]:
    """The names in the builder's `for binary in ... ; do` export loop."""
    text = (ROOT / "dist" / "build.Dockerfile").read_text(encoding="utf-8")
    match = re.search(r"for binary in ([^;]+); do", text)
    if not match:
        # A rename must not make this check pass by checking nothing.
        print(
            "check-image-binaries: dist/build.Dockerfile has no `for binary in "
            "...; do` loop; either the export step changed shape or this check "
            "has stopped seeing its subject",
            file=sys.stderr,
        )
        sys.exit(1)
    return set(match.group(1).split())


def entrypoint_binaries() -> set[str]:
    """The names in the entrypoint dispatcher's binary-name `case` arm."""
    text = (ROOT / "dist" / "entrypoint.sh").read_text(encoding="utf-8")
    # Join shell line continuations first. The case arm is long enough to wrap,
    # and matching the raw text captured a literal backslash-newline as part of a
    # binary name -- which failed in both directions at once and read as two
    # unrelated problems.
    text = re.sub(r"\\\n\s*", " ", text)
    match = re.search(r"^\s*(yesnod \|[^)]*)\)\s*$", text, flags=re.MULTILINE)
    if not match:
        print(
            "check-image-binaries: dist/entrypoint.sh has no `yesnod | ...)` case "
            "arm; either the dispatcher changed shape or this check has stopped "
            "seeing its subject",
            file=sys.stderr,
        )
        sys.exit(1)
    return {name.strip() for name in match.group(1).split("|") if name.strip()}


def main() -> int:
    crate = crate_binaries()
    docker = dockerfile_binaries()
    entry = entrypoint_binaries()
    expected = crate - set(EXCLUDED)

    failures: list[str] = []

    for name in sorted(EXCLUDED):
        if name not in crate:
            failures.append(
                f"EXCLUDED names `{name}`, which no longer exists; remove the entry"
            )
        if name in docker or name in entry:
            failures.append(
                f"`{name}` is excluded from the unified image "
                f"( {EXCLUDED[name]} ) but the image references it"
            )

    for missing in sorted(expected - docker):
        failures.append(f"`{missing}` is built by the workspace but not exported by "
            f"dist/build.Dockerfile")
    for extra in sorted(docker - expected):
        failures.append(
            f"dist/build.Dockerfile exports `{extra}`, which no crate builds"
        )
    for missing in sorted(docker - entry):
        failures.append(
            f"`{missing}` is exported into the image but not dispatchable: "
            f"add it to the case arm in dist/entrypoint.sh"
        )
    for extra in sorted(entry - docker):
        failures.append(
            f"dist/entrypoint.sh dispatches `{extra}`, which the image does not contain"
        )

    if failures:
        for failure in failures:
            print(f"check-image-binaries: {failure}", file=sys.stderr)
        return 1

    print(
        f"{len(docker)} binaries agree across the workspace, "
        f"dist/build.Dockerfile and dist/entrypoint.sh "
        f"( {len(EXCLUDED)} deliberately excluded )"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
