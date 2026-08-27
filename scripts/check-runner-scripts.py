#!/usr/bin/env python3
"""Shell-syntax-check the runner scripts the AWS gate ships to EC2.

`e2e/aws/gate.py` builds each remote step as a Python string and hands it to
Systems Manager, which runs it through `/bin/sh` on the instance. Nothing
between here and there parses it: the scenario compiles fine with a missing
`fi` inside a string literal, `terraform apply` succeeds, an EKS cluster comes
up, and the first thing that notices is `sh` on the far side of a round trip --
roughly twenty billable minutes and a provisioned stack into the run.

This parses `gate.py` as Python ( it is a Monty subset, so `ast` reads it ),
pulls out every `*_BODY` constant without executing anything, prepends the
`set -eu` its prologue always adds, and asks `sh -n`.

Syntax only. A script that runs the wrong command parses perfectly; only a
live run can say otherwise. What this removes is the failure with nothing to do
with the subject and the highest cost to discover.

Do not "fix" a failure here by relaxing the check. A body that `sh` cannot
parse is a body the runner cannot run.
"""

import ast
import pathlib
import subprocess
import sys
import tempfile

# The prologue every body is concatenated onto. Checking the body alone
# would accept one that depends on a shell option the real script does not set.
PROLOGUE = "set -eu\n"
SOURCES = ["e2e/aws/gate.py"]


def bodies(path: pathlib.Path):
    """Every module-level `NAME_BODY = "..."` assignment, unevaluated."""
    tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    for node in tree.body:
        if not isinstance(node, ast.Assign) or len(node.targets) != 1:
            continue
        target = node.targets[0]
        if not isinstance(target, ast.Name) or not target.id.endswith("_BODY"):
            continue
        if not isinstance(node.value, ast.Constant) or not isinstance(
            node.value.value, str
        ):
            continue
        yield target.id, node.value.value


def main() -> int:
    root = pathlib.Path(__file__).resolve().parent.parent
    failures = []
    checked = 0
    for source in SOURCES:
        path = root / source
        if not path.is_file():
            print(f"check-runner-scripts: {source} does not exist", file=sys.stderr)
            return 1
        found = list(bodies(path))
        if not found:
            # A rename would otherwise make this script pass by checking
            # nothing, which is the one result a checker must never produce.
            print(
                f"check-runner-scripts: {source} declares no *_BODY constant; "
                "either the naming changed or this check has stopped seeing its subject",
                file=sys.stderr,
            )
            return 1
        for name, body in found:
            checked += 1
            with tempfile.NamedTemporaryFile(
                "w", suffix=".sh", encoding="utf-8"
            ) as handle:
                handle.write(PROLOGUE + body)
                handle.flush()
                result = subprocess.run(
                    ["sh", "-n", handle.name],
                    capture_output=True,
                    text=True,
                    check=False,
                )
            if result.returncode != 0:
                failures.append(f"{source}:{name}\n{result.stderr.strip()}")

    if failures:
        for failure in failures:
            print(f"check-runner-scripts: {failure}", file=sys.stderr)
        return 1
    print(f"{checked} runner script(s) parse as POSIX sh")
    return 0


if __name__ == "__main__":
    sys.exit(main())
