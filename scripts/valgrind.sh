#!/usr/bin/env bash
#
# The UB gate for `store/segment.rs`, which holds both of the crate's `unsafe`
# blocks.
#
# **Since MIRI was withdrawn on 2026-08-29, this is the whole UB gate rather
# than half of it.** That is less of a loss than it sounds: MIRI refused
# `store/segment.rs` outright — "Miri does not support file-backed memory
# mappings" — so the two `unsafe` blocks were never covered by it in the first
# place. Valgrind has no such limitation, because it runs the real binary
# against the real mapping.
#
# What did go with MIRI is cast checking under
# `-Zmiri-symbolic-alignment-check`. Valgrind does not replace that: it sees
# concrete addresses, so it passes whenever the allocator happened to align
# things. The crate's casts go through `bytemuck`, whose checked entry points
# ( `try_cast_slice` and friends ) refuse a misaligned slice at runtime rather
# than reinterpreting it, which is what makes that gap survivable.
#
# ---------------------------------------------------------------------------
# What this does and does not prove
# ---------------------------------------------------------------------------
#
# Proves: no invalid read or write, no use of a page after its mapping is gone,
# no branch on uninitialised bytes. That is the memory-safety half of the
# zero-copy design, including the `ExtentGuard` keeping a mapping alive for a
# `Buffer` that outlived the `Db`.
#
# Does **not** prove: that the allocator's three reclamation conditions are
# correct. Logical reuse of a slot inside a still-valid mapping is not a memcheck
# error — the memory is validly mapped, it just holds someone else's bytes. That
# is what `tests/zero_copy_mvcc.rs` is for, and why its assertions are on
# *contents*. Do not report reclamation as verified on the strength of a clean
# run here.
#
set -euo pipefail

command -v valgrind >/dev/null || { echo "valgrind is not installed" >&2; exit 1; }

echo "building test binaries..."
mapfile -t BINS < <(
    cargo test -p yesno-core --no-run --message-format=json 2>/dev/null | python3 -c "
import sys, json
for line in sys.stdin:
    try: m = json.loads(line)
    except ValueError: continue
    if m.get('profile', {}).get('test') and m.get('executable'):
        print(m['executable'])
" | sort -u
)

# Valgrind serialises threads onto one core, so a stress suite that takes ~30s
# natively takes many minutes under it — and it is measuring interleavings that
# valgrind has just removed. Memory errors in that code are reachable from the
# single-threaded suites, which do cover the same paths.
SKIP="concurrency"

status=0
for bin in "${BINS[@]}"; do
    name=$(basename "$bin" | sed 's/-[0-9a-f]*$//')
    if [[ " $SKIP " == *" $name "* ]]; then
        printf '%-20s skipped (see the note above)\n' "$name"
        continue
    fi
    printf '%-20s ' "$name"
    if out=$(valgrind --error-exitcode=42 --leak-check=no "$bin" 2>&1); then
        echo "$out" | grep -oE 'ERROR SUMMARY: [0-9]+ errors' | head -1
    else
        echo "FAILED"
        # **Print the memory diagnosis *and* the test output.** This used to
        # grep for `Invalid |uninitialised|ERROR SUMMARY` alone, which is only
        # right when Valgrind is what failed. On 2026-09-06 a binary exited
        # non-zero with `ERROR SUMMARY: 0 errors` — a *test* had failed, under
        # Valgrind, and the one line naming it was filtered away. Recovering it
        # meant re-running the binary by hand. Do not narrow this again: the
        # exit code says "something failed" and cannot say which, so both
        # possible causes have to be shown.
        echo "$out" | grep -E 'Invalid |uninitialised|ERROR SUMMARY' | head -5
        echo "$out" | grep -E '^test .* FAILED|^test result:|panicked at|^assertion' | head -10
        status=1
    fi
done
exit $status
