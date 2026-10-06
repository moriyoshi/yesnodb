#!/usr/bin/env bash
#
# The Java client's gate: compile, tests, javadoc.
#
# This existed only as a `gradlew` line inside `.github/workflows/ci.yml` while
# the Python, Go and C ABI clients each had a script. A check that lives in one
# file cannot be compared against another, so `scripts/check-gate-parity.py`
# could not see the Java gate at all, and `scripts/gate.sh` had nothing to call.
# Being a script is what makes it reachable from both.
set -euo pipefail

project_dir="$(dirname "$(readlink -f "$0")")"
cd "$project_dir"

# `build` runs `check`, which the module's own `build.gradle.kts` extends with
# `javadoc` -- doclint is part of the gate rather than a release-time surprise.
./gradlew -p . --no-daemon build
