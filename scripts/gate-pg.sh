#!/usr/bin/env bash
#
# The PostgreSQL gate: everything `scripts/gate.sh` structurally cannot reach.
# Its public entrypoint needs only Docker; the internal invocation runs every
# build and fixture inside the database-builder container.
#
# Why there are two gates
# ----------------------
#
# `scripts/gate.sh` is unchanged and remains the authority on the cargo
# workspace. It cannot cover `yesno-pg`, because that crate produces a `cdylib`
# PostgreSQL `dlopen`s, and such a library is only meaningful against one
# specific server ABI — same major, same BLCKSZ, same configure flags. Cargo has
# no way to pin that; `cargo pgrx init` reaches it by building PostgreSQL into
# `~/.pgrx`, which is machine state, not a pinned input.
#
# **Neither gate subsumes the other.** A change to `yesno-core` or
# `yesno-flight` must run both: Bazel builds those crates too, so a change that
# satisfies cargo can still break this build through a stale `Cargo.lock`
# resolution.
#
# Do not add cargo steps here or Bazel steps to `gate.sh`. The split is the
# point; merging them puts the whole PostgreSQL toolchain in the path of every
# routine `yesno-core` change.

set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1
. "$PWD/scripts/scratch.sh"

if [[ "${1:-}" != "--container-internal" ]]; then
    exec ./scripts/run-database-gate.sh postgresql
fi
shift

# The image installs the checksum-pinned Bazel binary matching `.bazelversion`.
# The candidate scan also keeps the internal gate usable for repository debugging.
BAZEL=""
for candidate in bazelisk bazel "$YESNO_SCRATCH_DIR/bin/bazelisk"; do
    if command -v "$candidate" >/dev/null 2>&1 || [[ -x "$candidate" ]]; then
        BAZEL="$candidate"
        break
    fi
done
if [[ -z "$BAZEL" ]]; then
    printf 'gate-pg: database-builder image contains no bazel binary\n' >&2
    exit 1
fi

fail=0
steps_run=0
# **Update this when adding or removing a step.** Same reasoning as
# `gate.sh`: a gate that can report success without having run is worse than no
# gate, and counting is the cheapest check that catches it however it is caused.
EXPECT_STEPS=6

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
    [[ $fail -eq 0 ]] && printf '\n\033[32mgate-pg passed\033[0m\n' || printf '\n\033[31mgate-pg failed\033[0m\n'
    exit $fail
}

printf '\033[1mgate-pg\033[0m  bazel=%s  pinned=%s\n' "$BAZEL" "$(cat .bazelversion)"

step "build the extension (default PostgreSQL major)"
check "$BAZEL" build //yesno-pg:yesno_pg

step "unit tests (pure functions; no backend)"
check "$BAZEL" test //yesno-pg:unit

step "regression fixtures against a hermetic cluster"
check "$BAZEL" test //e2e/postgresql:regress

step "resolver manifests differ only by PostgreSQL major"
check python3 scripts/check-pg-manifests.py

step "unit and regression fixtures against PostgreSQL 18"
check "$BAZEL" test //yesno-pg:unit //e2e/postgresql:regress --//:pg_version=18

# `yesno-pg` is outside `[workspace] members`, so `cargo fmt --all` never sees
# it — the same reason `gate.sh` checks `yesno-core/fuzz` separately.
step "rustfmt (yesno-pg is outside the cargo workspace)"
check bash -c 'cd yesno-pg && cargo fmt --all -- --check'

verdict
