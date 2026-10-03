#!/usr/bin/env bash
#
# The "lean core" dependency budget for `yesno-core`.
#
# README.md sells the core as five dependencies and no async runtime, and the
# design asks for a pinned count that a change may not quietly raise. The limits
# below are **calibrated to an exact tree**, not chosen with slack: at the time of
# writing the real counts are 5 and 36, so either one growing by a single entry
# trips this.
#
# # Why this is a script rather than two copies of a pipeline
#
# It was two copies -- one inline in `.github/workflows/ci.yml` and none in
# `scripts/gate.sh`, which is a worse version of the same problem. On 2026-09-27
# `libc` was added to `yesno-core` for one `fallocate` call, taking the counts to
# 6 and 37. CI went red and stayed red for 25 commits, because the local gate
# that everyone actually runs before reporting a change had no equivalent check
# and could not see it. The repair is one implementation with two callers.
# The same drift once affected Clippy: its script was fixed to cover the whole
# workspace while the written hand-run command still covered only two crates.
#
# # Do not raise these to make a change pass
#
# Dependency creep into the core is what kills a lean core over eighteen months,
# one justified crate at a time. The established alternative is to declare the
# handful of foreign functions directly: `db::readers` does it for `kill` and
# `store::segment` for `fallocate`, each two lines of `extern "C"` next to the
# reasoning for it. `libc` remains in the tree transitively, under `memmap2` --
# this budget is about what the core reaches for on its own behalf.
set -uo pipefail
cd "$(dirname "$(readlink -f "$0")")/.." || exit 1

MAX_DIRECT=5
MAX_TOTAL=36

# `-e normal` excludes dev-dependencies, so `roaring`, `proptest` and `criterion`
# do not move the number the README's claim is about.
direct=$(cargo tree -p yesno-core --depth 1 -e normal | tail -n +2 | wc -l) || exit 1
total=$(cargo tree -p yesno-core -e normal | wc -l) || exit 1

status=0
echo "direct=$direct (max $MAX_DIRECT)  total=$total (max $MAX_TOTAL)"
if (( direct > MAX_DIRECT )); then
    echo "direct dependencies grew past $MAX_DIRECT"
    cargo tree -p yesno-core --depth 1 -e normal | tail -n +2
    status=1
fi
if (( total > MAX_TOTAL )); then
    echo "transitive tree grew past $MAX_TOTAL lines"
    status=1
fi

# The two network crates depend on the core and never the reverse.
if cargo tree -p yesno-core -e normal | grep -E 'tokio|tonic|prost|arrow-flight'; then
    echo "an async or gRPC crate reached yesno-core"
    status=1
fi
exit $status
