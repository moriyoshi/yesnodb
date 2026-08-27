#!/usr/bin/env bash
# Build yesno-c, native Arrow Flight, MySQL 8.4, and ha_yesno.so from pinned
# source, then exercise the storage engine in a throwaway server. Its public
# entrypoint needs only Docker; the internal invocation runs every artifact and
# fixture inside the database-builder container. This gate is independent of
# both scripts/gate.sh and scripts/gate-pg.sh; none subsumes either of the
# others.

set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1
. "$PWD/scripts/scratch.sh"

if [[ "${1:-}" != "--container-internal" ]]; then
    exec ./scripts/run-database-gate.sh mysql
fi
shift

BAZEL=""
for candidate in bazelisk bazel "$YESNO_SCRATCH_DIR/bin/bazelisk"; do
    if command -v "$candidate" >/dev/null 2>&1 || [[ -x "$candidate" ]]; then
        BAZEL="$candidate"
        break
    fi
done
if [[ -z "$BAZEL" ]]; then
    printf 'gate-mysql: database-builder image contains no bazel binary\n' >&2
    exit 1
fi

fail=0
steps_run=0
EXPECT_STEPS=4

step() {
    steps_run=$((steps_run + 1))
    printf '\n\033[1m== %s\033[0m\n' "$1"
}

check() {
    if "$@"; then
        printf '   ok\n'
    else
        printf '   FAILED\n'
        fail=1
    fi
}

verdict() {
    if [[ $steps_run -ne $EXPECT_STEPS ]]; then
        printf '\n\033[31m   INCOMPLETE: ran %d of %d steps\033[0m\n' "$steps_run" "$EXPECT_STEPS"
        printf '   The run stopped early. Its verdict below covers only what ran.\n'
        fail=1
    fi
    [[ $fail -eq 0 ]] && printf '\n\033[32mgate-mysql passed\033[0m\n' || printf '\n\033[31mgate-mysql failed\033[0m\n'
    exit $fail
}
printf '\033[1mgate-mysql\033[0m  bazel=%s  pinned=%s  mysql=8.4.0\n' \
    "$BAZEL" "$(cat .bazelversion)"

step "build the standalone C ABI with Bazel"
check "$BAZEL" build //yesno-c:yesno_c

step "build and unit-test the native Arrow Flight client with Bazel"
check "$BAZEL" test //yesno-flight-c++:unit_tests --test_output=errors

step "build pinned MySQL and ha_yesno.so"
check "$BAZEL" build //third_party/mysql:mysql

step "mysqltest fixture against a hermetic server"
check "$BAZEL" test //e2e/mysql:regress --test_output=errors

verdict
