#!/usr/bin/env bash
#
# **NOT A GATE. Withdrawn 2026-08-29.** This script was the MIRI half of the
# M2 UB gate ( "MIRI clean on all casts" ). Nothing invokes it any more:
# `scripts/gate.sh --deep` and `.github/workflows/ci.yml`'s `deep` job both
# dropped their MIRI step on that date, and `--component miri` came out of the
# CI toolchain install with it. It is kept runnable because a *targeted* run is
# still occasionally the cheapest way to answer a question about one cast.
#
# **Why it was withdrawn.** Not unsoundness in the tool — cost, and a failure
# mode the cost produces. MIRI interprets, so the price is set by the *fixtures*
# a test builds rather than by the kernel under test, and this suite's fixtures
# outgrew it: `ops::run::tests` exceeded 900 s, and `matrix::seek` sat on a
# single test that builds 200 000-ordinal sets for **42 min 43 s** before being
# killed. A run that never ends never reports, and `scripts/gate.sh` counts
# steps that *ran* rather than steps that *passed* precisely because a step
# which hangs is indistinguishable from one that succeeded. See JOURNAL.md,
# "MIRI dropped as a verification method".
#
# Do not re-add this to a gate on the strength of a fast targeted run. The
# thing that made it unaffordable is the proptests and the large fixtures, and
# any selector broad enough to be worth gating on pulls those back in.
#
#   ./scripts/miri.sh            # both tiers
#   ./scripts/miri.sh symbolic   # tier 1 only
#   ./scripts/miri.sh standard   # tier 2 only
#
# ---------------------------------------------------------------------------
# Why there are two tiers rather than one command
# ---------------------------------------------------------------------------
#
# `-Zmiri-symbolic-alignment-check` is the flag worth having. By default MIRI
# checks alignment against the *concrete* address it happened to hand out, so a
# cast that is wrong in principle passes whenever the allocator got lucky. We
# rely on alignment as a **format** constraint — `ScalarBuffer::<T>::new` and
# `typed_data::<T>()` panic rather than error on misalignment — so "passed by
# luck" is exactly the failure we need to exclude. Tier 1 runs it.
#
# But that flag has a documented false-positive mode, quoting MIRI's own README:
# it "incurs some false positives when the code does the pointer-integer-cast-
# based alignment check itself". The symbolic check tracks alignment from the
# allocation's declared alignment and offset, so it cannot see a library align
# a pointer by rounding its address up.
#
# The `crc32c` crate does exactly that — `util::split` rounds the base address
# up to a multiple of 8 and splits there, which is correct — so under tier 1 it
# reports UB constructing `&[U64Le]` from a `&[u8]`. **That is not a bug in
# `crc32c` and not a bug in us.** It was investigated once ( JOURNAL,
# 2026-08-25 ); do not re-report it upstream.
#
# So: modules that checksum run under tier 2, without the symbolic flag. They
# still get full Stacked-Borrows and validity checking, just concrete alignment.
#
# ---------------------------------------------------------------------------
# Why `store::segment` appears in neither tier
# ---------------------------------------------------------------------------
#
# Not slowness. MIRI physically cannot execute it, and says so in as many words:
#
#     error: unsupported operation: Miri does not support file-backed memory
#            mappings
#
# `store::segment`'s two blocks live there ( `MmapOptions::map` and
# `Buffer::from_custom_allocation` ), so the sites with the most need of a UB
# checker are among the exact sites MIRI cannot reach. That is a real gap, not a
# technicality, and `.agents/docs/TODO.md` tracks it as
# `miri-cannot-reach-the-mmap-unsafe-sites`.
#
# **Do not read the deep gate's sanitizers as covering this.** That substitution
# was asserted here for weeks and measured false on 2026-09-14: an overread 1000
# bytes past a 16-byte slice, still inside the mapping, is **silent under both
# AddressSanitizer and Valgrind**, while a heap overread in the same binary is
# caught by both. ASan instruments allocations it makes and a mapping is not one;
# Valgrind treats the mapping as valid addressable memory. The mmap blocks have
# no UB-checker coverage at all -- what protects them is `SegmentedMmap`'s
# explicit containment check and the I2/I6 invariants.
#
# **This paragraph said "the two `unsafe` blocks in the workspace" until
# 2026-09-14. There are twenty-five**, across six files, and the count was
# presumably right when written -- the NEON arms landed 2026-08-27 and
# 2026-09-07, after this comment. Counted with `#[cfg(test)]` regions stripped:
#
#   ops/array.rs    9 blocks,  6 unsafe fn   SIMD intrinsics
#   ops/bitmap.rs   6 blocks,  4 unsafe fn   SIMD intrinsics
#   db/readers.rs   4 blocks,  0 unsafe fn   2 mmap + 2 libc FFI ( kill, errno )
#   ops/mixed.rs    2 blocks,  1 unsafe fn   SIMD intrinsics
#   ops/run.rs      2 blocks,  1 unsafe fn   SIMD intrinsics
#   store/segment.rs 2 blocks, 0 unsafe fn   the mmap pair named above
#
# The conclusion survives the correction and arguably strengthens: MIRI reaches
# none of the four mmap blocks, neither libc call, and not the target-specific
# intrinsics either. What changes is the *size* of what is uncovered -- one
# module was named where six have unsafe, and 2 blocks where there are 25.
# Do not quote "the two unsafe blocks" from anywhere; it has been copied once
# already, into TODO.md, on the day it was found false.
#
# Do not "fix" a failure by moving `store::segment` into a tier and finding it
# errors. Reporting this gate as met by running MIRI over code that never
# touches a mapping is precisely what that TODO item warns against.
#
set -euo pipefail

if ! rustup component list --toolchain nightly --installed 2>/dev/null | grep -q '^miri'; then
    echo "miri is not installed. Run:" >&2
    echo "    rustup component add miri --toolchain nightly" >&2
    exit 1
fi

# Tier 1: cast-dense and checksum-free, so the strict flag applies cleanly.
# libtest treats several filters as OR and matches by substring, so `ops::`
# also picks up `stream::ops::` — intended.
SYMBOLIC=(buffer:: container:: ops::)

# Tier 2: reaches `store::checksum::crc32c`, directly or via a node/page seal.
STANDARD=(index:: store::extent store::packed store::superblock store::checksum)

# Isolation is off in both tiers: some helpers touch the filesystem, and without
# it MIRI stops at `unlink` long before evaluating anything interesting.
run_symbolic() {
    echo "==> tier 1 (symbolic alignment): ${SYMBOLIC[*]}"
    MIRIFLAGS="-Zmiri-symbolic-alignment-check -Zmiri-disable-isolation" \
        cargo +nightly miri test -p yesno-core --lib -- "${SYMBOLIC[@]}"
}

run_standard() {
    echo "==> tier 2 (standard alignment): ${STANDARD[*]}"
    MIRIFLAGS="-Zmiri-disable-isolation" \
        cargo +nightly miri test -p yesno-core --lib -- "${STANDARD[@]}"
}

case "${1:-all}" in
    symbolic) run_symbolic ;;
    standard) run_standard ;;
    all)      run_symbolic; run_standard ;;
    *)        echo "usage: $0 [symbolic|standard|all]" >&2; exit 2 ;;
esac
