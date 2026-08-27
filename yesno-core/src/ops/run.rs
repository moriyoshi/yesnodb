//! Specialized run×run kernels: interval arithmetic instead of value merging.
//!
//! # Why this arm is the worst one
//!
//! [`super::generic`] merges *values*. A run container holding 30 000
//! contiguous ordinals is four bytes on disk and 30 000 iterations through a
//! peekable pair — the encoding's entire advantage is thrown away at the point
//! of use. Measured against the `roaring` crate on eight such chunks:
//!
//! ```text
//!   run x run  AND   996 us  vs  2.2 us     450x
//!   run x run  OR    997 us  vs  2.2 us     458x
//! ```
//!
//! Two intervals intersect in constant time, so the whole operation is a
//! two-pointer walk over `nruns` — for that corpus, eight steps rather than a
//! quarter of a million.
//!
//! # The output is built as intervals, not as values
//!
//! Every arm emits `(start, len_minus_1)` pairs directly and hands them to
//! `RunContainer::from_pairs`, so a result that *is* a run stays one all the way
//! through. Emitting values and re-optimizing would put the cost straight back.
//!
//! # Non-adjacency is a postcondition, not an assumption
//!
//! `QUALITY_GATE.md` §2: two runs that touch must be merged into one, or every
//! kernel that assumes maximal runs is wrong. Union and difference can both
//! produce touching intervals from inputs that had none, so each arm pushes
//! through `Emit`, which coalesces on the way out.
//!
//! # The generic kernel remains the oracle
//!
//! Every arm is differential-tested against `generic::apply` on the same inputs,
//! which is what makes specializing safe: the slow path is the reference
//! implementation, not dead code.
//!
//! # The intervals are read once per advance, not four times per step
//!
//! [`RunContainer::start`] is `self.runs.as_slice()[i * 2]` and
//! [`RunContainer::end`] is `s[i * 2] + s[i * 2 + 1]`, and `runs` is a
//! copy-on-write enum, so *every* one of those accessors is a match plus a
//! bounds-checked index. The two-pointer AND written against them called
//! `x.end(i)` and `y.end(j)` twice each per step — about ten bounds-checked
//! indexes and six `as_slice()` matches to do one interval intersection.
//!
//! The payload is therefore taken as a `&[[u16; 2]]` once ( `pairs`, via
//! `bytemuck`, so no `unsafe` and no per-index bound on the *inner* access ),
//! and each side's current `(start, end)` is kept in locals that are refreshed
//! only when that side advances. Measured on the balanced sweep, ns per pair:
//!
//! `run_kernel_shape/run_x_run/card`, ns per pair, 32 distinct pairs, both sides
//! at the same interval count so the gallop below never fires:
//!
//! ```text
//!   nruns           1      8     64    512    2048
//!   before        3.4   18.1  139.8   1272   12578
//!   after         3.5   12.6   97.4    794    3942
//!                0.99x  1.44x  1.43x  1.60x   3.19x
//! ```
//!
//! The gain grows with `nruns` because the *baseline* paid per step, and at
//! `nruns = 1` there is nothing to amortize and the change is a wash. It is not
//! the kernel getting better with size.
//!
//! `nruns = 2048` is 512 KiB of operands over 32 pairs and it moves ±12%
//! between builds on code paths that did not change — a build with the gallop
//! compiled out measured 4 566 ns on the same source. Read that column as "about
//! 3x", not as 3.19.
//!
//! # Galloping on skew
//!
//! `ops::array` has probed the small side into the large past [`GALLOP_RATIO`]
//! since it was written. This module had **no** reference to that constant at
//! all, so intersecting 8 intervals with 1024 walked all 1024 — `O(n + m)` where
//! `O(min · log max)` was available. Every run x run benchmark in the file passed
//! *equal* `nruns` on both sides, so nothing could see it; `run_kernel_skew` is
//! the group that does.
//!
//! Galloping over intervals is not galloping over values, and the difference is
//! where the boundary conditions live. The probe seeks the first interval whose
//! **end** is `>= the other side's start`, because an interval that merely
//! touches that start still overlaps it; and the large-side cursor must **not**
//! advance past an interval that outlives the small one, since that interval can
//! still meet the next small interval. Both are stated at `gallop_end` and
//! `gallop_and_each`, and both are what the skewed property tests pin.
//!
//! Against the hoisted merge, on the same 32 distinct pairs ( x = the whole
//! change against the pre-2026-08-28 kernel ):
//!
//! ```text
//!                          8 vs 1024        32 vs 1024
//!                        gallop  total    gallop   total
//!   and_cardinality       2.58x  3.98x     1.62x   2.38x
//!   is_disjoint           2.79x  4.68x     1.09x   1.68x
//!   contains_all          4.65x  6.63x     1.36x   2.02x
//!   and ( materializing ) 1.46x  2.05x     1.21x   1.86x
//! ```
//!
//! **It costs nothing where it declines** — a ratio of 16, one step below the
//! threshold, and 1:1 are all within 1.01x of the merge — **except on a call too
//! short to amortize the decision.** `run_kernel_shape/run_x_run/disjoint` exits
//! on its first overlapping pair at ~3.7 ns, and there the gate is the whole
//! remaining cost: the hoist alone made that arm 1.09-1.24x faster and the gate
//! gives all of it back, leaving 0.95-1.08x against the original. On the same
//! predicate over operands that actually walk — `predicate_paths/is_disjoint/run_x_run`,
//! 54 intervals — the same code is **1.61x**. Do not read the tiny-input row
//! as the arm's cost.
//!
//! **Or, Xor and AndNot are not galloped, and the reason is not neglect.**
//! Their results are `Ω( the intervals they must emit )`: on the 8 x 1024 point
//! that is 1031 output intervals for XOR, 747 and 284 for the two ANDNOT
//! directions, against inputs of 1032 — so the boundary sweep is *output*-bound,
//! and no probe can skip work the result depends on. They keep the hoist, which
//! is worth 1.16-1.31x there, and nothing else.
//!
//! # The counting merge is vectorized; the emitting one is not
//!
//! An 8x8 block kernel replaces the two-pointer in `and_cardinality`'s
//! **merge** branch on `aarch64`. Two `vld1q_u16` take the 32 bytes of eight
//! `(start, len_minus_1)` pairs, `vuzp1q_u16` / `vuzp2q_u16` split them into a
//! starts register and a lengths register, and `starts + lens` gives the ends —
//! LLVM folds that triple into a single `addp`. Eight `vextq_u16` rotations of
//! the `y` block against the fixed `x` block then cover all 64 pairs. Two
//! intervals overlap iff `max(starts) <= min(ends)`, so one `umax`, one `umin`
//! and one compare do what the obvious `sa <= eb && sb <= ea` spends two
//! compares and an `and` on. A block retires the eight intervals of whichever
//! side ends lower, exactly as the scalar loop retires one.
//!
//! **Only the counting arm.** The rotation product yields the 64 overlaps in
//! rotation order, not in ascending order, and `Emit` coalesces on the
//! assumption that it is fed ascending intervals. Sorting them back would cost
//! more than the block saves, so [`try_apply`]'s `And` keeps the scalar merge.
//!
//! **The gallop still decides first.** Run unconditionally, the block kernel
//! is a **0.62-0.75x regression** at 8 x 1024 and 8 x 2048 — it has no probe, so
//! it walks the large side. It is reached only from the merge branch, and only
//! when both sides hold at least one full block.
//!
//! `run_kernel_shape/run_x_run/card`, ns per pair, 32 distinct pairs, both sides
//! at the same interval count so the gallop never fires. Two builds of this
//! module differing **only** in whether `merge_cardinality` reaches the block:
//!
//! ```text
//!   nruns              1      8     64    512    2048
//!   scalar merge     3.54  13.08   96.1  795.0   4017
//!   block kernel     3.75   6.88   53.1  433.8   1713
//!                   0.94x  1.90x  1.81x  1.83x   2.35x
//! ```
//!
//! Two controls, source untouched between those two builds:
//! `run_kernel_shape/bitmap_x_run/card` moved **−0.11% to +0.30%** over its five
//! points, and `bitmap_kernel_reuse/card` **−1.4% to −8.8%**. So the drift band
//! is about ±9%, the `n = 1` row sits inside it and is not a regression, and
//! nothing else in the table is near it.
//!
//! # The whole result was one dependency chain, and it was lost twice
//!
//! **What made this arm pay is not the vector arithmetic.** Every block is ~80
//! vector ops issuing at three per cycle, and the loop is *latency*-bound on one
//! chain: read the block's last end, compare the two, select which cursor
//! advances, form the next load address. Anything that lengthens that chain
//! costs more than the 64 comparisons the block exists to do.
//!
//! It was lost twice, the second time invisibly:
//!
//! 1. The first working version took the last end with `vgetq_lane_u16::<7>(xe)`
//!    — a SIMD-to-GPR move straight onto the chain. **1.03-1.09x**, inside the
//!    noise. Reading the same end with a scalar load, whose address depends only
//!    on the cursor, took it to 1.7-2.2x.
//! 2. Switching the de-interleave from `vld2q_u16` to `vld1q_u16` + `vuzp`
//!    ( see `simd::block_and_cardinality` for why ) put it **straight back**:
//!    **2.05x collapsed to 1.09x**. The source still said "scalar load" — but
//!    `vld1q_u16` loads the payload *verbatim*, so GVN could now prove that
//!    `xp[i + 7]` is lanes 6 and 7 of a register it already had, and replaced
//!    the load with `dup` + `add` + `umov`. Reinstating the lane extract is a
//!    thing the **compiler** did, from source that had been written to avoid it.
//!
//! The fix is to read the two endpoints **before** the vector loads. The values
//! are identical and GVN then has nothing to fold the scalar loads into. That
//! ordering is load-bearing and looks like style; moving those two lines below
//! the loads is a 2x regression with every test still green. Do not tidy them
//! back down, and do not replace them with a lane extract.
//!
//! # The three cardinality-only arms share these kernels
//!
//! `ops::card` is a second dispatch over the same kind pairs and does **not**
//! inherit a fix made here — the crate has twice shipped a repair to one arm and
//! left its siblings. So `and_cardinality`, `is_disjoint` and `contains_all` all
//! call into this module rather than open-coding a fourth and fifth and sixth
//! copy of the two-pointer. This is the same arrangement, and for the same
//! reason, as `ops::array::and_cardinality` living beside `ops::array::and`.

use crate::container::{Container, RunContainer};
use crate::ops::generic::SetOp;
use crate::GALLOP_RATIO;

/// One interval as it is stored: `(start, len_minus_1)`.
type Iv = [u16; 2];

/// Intervals per side consumed by one iteration of the vector merge.
///
/// Eight `(start, len_minus_1)` pairs is 32 bytes, which one `vld2q_u16`
/// de-interleaves into two 128-bit registers. The width is the register, not a
/// tuning knob.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
const SIMD_BLOCK: usize = 8;

/// The flat `u16` payload as `(start, len_minus_1)` pairs.
///
/// `bytemuck` rather than a hand-written cast, per QG §7 — `[u16; 2]` and `u16`
/// have the same alignment, so this is a length division and nothing else. The
/// odd trailing element is dropped rather than asserted away: the invariant says
/// the payload is even-length, and a kernel is not the place to panic if a
/// decode ever let one through.
#[inline]
fn pairs(flat: &[u16]) -> &[Iv] {
    bytemuck::cast_slice(&flat[..flat.len() & !1])
}

#[inline]
fn ivstart(v: Iv) -> u16 {
    v[0]
}

/// Inclusive end. Cannot overflow on a valid container: `start + len_minus_1` is
/// the interval's last value, which is `<= u16::MAX` by construction.
#[inline]
fn ivend(v: Iv) -> u16 {
    v[0] + v[1]
}

/// Skewed enough that probing beats merging, by the same ratio and the same
/// constant `ops::array` uses.
///
/// Written as a **multiply**, not as `hi / lo >= GALLOP_RATIO`. The two are
/// exactly equivalent for `lo > 0` — truncating division satisfies
/// `hi / lo >= k  ⟺  hi >= k * lo` — but an integer division is ~20 cycles on
/// this target and this test runs on *every* run x run call, including the ones
/// that cost 3 ns in total. Dividing measured a flat **1.13-1.16x** on the
/// cheapest points of the sweep ( `nruns = 1`, and every `is_disjoint` that exits
/// on its first pair ), which is the whole cost of the operation. `lo` is an
/// interval count, at most `RUN_DECODE_MAX`, so the product cannot overflow.
///
/// This is not a change to [`GALLOP_RATIO`]; the predicate it expresses is the
/// same one, at the same threshold.
///
/// **Forcing this to `true` leaves every test green**, and that is by design —
/// the gallop and the merge agree on every input, and the property tests hold
/// them against each other directly rather than through this gate. So the suite
/// passing is *not* evidence that this threshold is right; only a benchmark is.
#[inline]
fn should_gallop(n: usize, m: usize) -> bool {
    let (lo, hi) = if n <= m { (n, m) } else { (m, n) };
    lo > 0 && hi >= lo * GALLOP_RATIO
}

/// Index of the first interval at or after `lo` whose **end** is `>= target`,
/// by exponential probe then binary search.
///
/// The predicate is on `end`, not on `start`, and `>=` rather than `>`. Ends
/// are strictly increasing across a valid run container, so the predicate is
/// monotone and the search is well-defined; using `start` would skip an interval
/// that begins before `target` and continues past it, which is precisely the
/// overlap the caller is looking for. `>=` rather than `>` because an interval
/// ending exactly *on* `target` contains it.
#[inline]
fn gallop_end(p: &[Iv], lo: usize, target: u16) -> usize {
    // A fast path, not a boundary. Weakening this `>=` to `>` leaves every
    // test green because the search below then returns the same index by a
    // longer route — the `>=` that *is* load-bearing is the `<` in the
    // `partition_point` predicate, and sabotaging that one fails four tests.
    if lo >= p.len() || ivend(p[lo]) >= target {
        return lo;
    }
    let mut step = 1usize;
    while lo + step < p.len() && ivend(p[lo + step]) < target {
        step *= 2;
    }
    let hi = (lo + step + 1).min(p.len());
    let base = lo + step / 2;
    base + p[base..hi].partition_point(|&v| ivend(v) < target)
}

/// Every non-empty `a ∩ b` interval, in ascending order, by two-pointer merge.
///
/// Each side's `(start, end)` lives in a local and is re-read only when that
/// side retires, which is the whole point — see the module header.
#[inline]
fn merge_and_each(xp: &[Iv], yp: &[Iv], mut f: impl FnMut(u16, u16)) {
    let (mut xi, mut yi) = (xp.iter(), yp.iter());
    let (Some(&x0), Some(&y0)) = (xi.next(), yi.next()) else {
        return;
    };
    let (mut xs, mut xe) = (ivstart(x0), ivend(x0));
    let (mut ys, mut ye) = (ivstart(y0), ivend(y0));
    loop {
        let (s, t) = (xs.max(ys), xe.min(ye));
        if s <= t {
            f(s, t);
        }
        // Retire whichever interval ends first; the other may still overlap
        // what comes next.
        if xe < ye {
            let Some(&v) = xi.next() else { return };
            xs = ivstart(v);
            xe = ivend(v);
        } else {
            let Some(&v) = yi.next() else { return };
            ys = ivstart(v);
            ye = ivend(v);
        }
    }
}

/// The same intervals, in the same order, by probing the sparser side into the
/// denser one.
///
/// Two boundary conditions carry this, and neither is visible on balanced
/// operands:
///
/// * the probe seeks `end >= s`, so an interval that ends exactly at the small
///   side's start is found rather than skipped;
/// * the cursor stops **without advancing** on an interval that ends past `e`,
///   because that interval can still meet the *next* small interval. Advancing
///   there loses overlaps, and does so only when a large interval spans two
///   small ones — which uniform operands essentially never produce.
///
/// Emission order is ascending overall: the small side's intervals are disjoint
/// and ascending, and within one of them the large side is walked forward.
#[inline]
fn gallop_and_each(xp: &[Iv], yp: &[Iv], mut f: impl FnMut(u16, u16)) {
    let (small, large) = if xp.len() <= yp.len() {
        (xp, yp)
    } else {
        (yp, xp)
    };
    let mut j = 0usize;
    for &sv in small {
        let (s, e) = (ivstart(sv), ivend(sv));
        j = gallop_end(large, j, s);
        while j < large.len() {
            let lv = large[j];
            let ls = ivstart(lv);
            if ls > e {
                break;
            }
            let le = ivend(lv);
            f(s.max(ls), e.min(le));
            if le > e {
                break;
            }
            j += 1;
        }
        if j >= large.len() {
            return;
        }
    }
}

/// `a ∩ b` as intervals, gallop or merge by the size ratio.
///
/// **[`and_cardinality`] does not call this, and must still take the same
/// branch.** It cannot share the body, because counting has a vector merge that
/// emitting does not — the block kernel yields overlaps in rotation order and
/// [`Emit`] requires ascending ones. What the two must share is the *decision*,
/// [`should_gallop`], for the failure mode `ops::array::and_cardinality`'s doc
/// comment describes: two paths returning the same number at different costs,
/// with nothing reporting the divergence. Both call it, and — like the
/// normalization below and the tie-break in `ops::array::simd` — **no test can
/// defend that**, because both branches return the same number. Changing one
/// call site and not the other leaves the suite green.
///
/// **The sparser side is passed first, and that is a measured decision, not a
/// tidy one.** AND is commutative and the emitted sequence is identical either
/// way, but the *cost* is not: `and( large, small )` measured **1.8x**
/// `and( small, large )` on the same 32 pairs at 8 x 1024 intervals, reproducibly
/// and with a single shared copy of the code, so it is neither layout nor
/// inlining. The mechanism is register pressure in the caller's closure —
/// `and_cardinality`, whose closure is one accumulator, is symmetric to within
/// 3%, while the `Emit` closure spills one of the two cursors and the loop then
/// pays for it once per advance of whichever side advances most. Normalizing
/// makes the kernel cost a property of the operands rather than of the order the
/// caller happened to write them in.
///
/// Reversing the normalization leaves every test green — it is a throughput
/// choice, exactly like the tie-break in `ops::array::simd::merge_cardinality`,
/// and the suite cannot tell you it is right.
#[inline]
fn and_each(xp: &[Iv], yp: &[Iv], f: impl FnMut(u16, u16)) {
    if should_gallop(xp.len(), yp.len()) {
        gallop_and_each(xp, yp, f)
    } else {
        let (p, q) = if xp.len() <= yp.len() {
            (xp, yp)
        } else {
            (yp, xp)
        };
        merge_and_each(p, q, f)
    }
}

/// The NEON block arm of the counting merge.
///
/// Two bounds carry it. Both are stated here so the `SAFETY` comments below can
/// name them rather than restate them, and both are asserted with
/// `debug_assert!` **inside** the loop, which is what lets a property test fail
/// on a violated bound rather than merely on a wrong answer. `cargo test` builds
/// without optimizations, so those assertions are live wherever the properties
/// run.
///
/// > **B5** ( loads ). The block loop runs only while `i + 8 <= xp.len()` and
/// > `j + 8 <= yp.len()`, so the 32-byte de-interleaving load at `xp[i]` and the
/// > read of `xp[i + 7]` are both inside the slice; `j` likewise. Neither cursor
/// > moves inside the body until after every read.
///
/// > **B6** ( accumulator width ). Lane `k` of the `u16` sum receives
/// > `min(ends) - max(starts)` once per rotation. The eight `y` intervals of a
/// > block are disjoint — `codec::decode` rejects a payload whose runs overlap
/// > or do not ascend — so the eight overlaps with `x[i + k]` are disjoint
/// > sub-intervals of it. Their lengths sum to at most `|x[i + k]| <= 65536` and
/// > each contributes one *less* than its length, so a lane holds at most
/// > `65536 - count <= 65535`. The count lane holds at most 8. Both are widened
/// > to `u32` with `vpadalq_u16` once per block, where the running totals are
/// > bounded by `CHUNK_CARD`.
///
/// B6 is why the total is accumulated as `sum(end - start)` **plus a count of
/// overlapping pairs** rather than as `sum(end - start + 1)`. A single overlap
/// can be the whole 65 536-value chunk, which does not fit a `u16` lane.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
/// Bound **B6**, checked rather than only argued.
///
/// Recomputes one block's per-lane sum and count in `u32` and asserts they
/// fit the `u16` lanes the kernel accumulates them in. Debug-only: it is
/// 64 interval comparisons, the same work the block itself does.
fn lane_sums_fit_u16(xp: &[Iv], yp: &[Iv], i: usize, j: usize) -> bool {
    let ys = &yp[j..j + SIMD_BLOCK];
    xp[i..i + SIMD_BLOCK].iter().all(|&xv| {
        let (xs, xe) = (ivstart(xv) as u32, ivend(xv) as u32);
        let (mut sum, mut cnt) = (0u32, 0u32);
        for &yv in ys {
            let (lo, hi) = (xs.max(ivstart(yv) as u32), xe.min(ivend(yv) as u32));
            if lo <= hi {
                sum += hi - lo;
                cnt += 1;
            }
        }
        sum <= u16::MAX as u32 && cnt <= SIMD_BLOCK as u32
    })
}

#[cfg(target_arch = "aarch64")]
mod simd {
    use super::{ivend, lane_sums_fit_u16, Iv, SIMD_BLOCK};
    use core::arch::aarch64::*;

    /// `|a ∩ b|` over the merge shape, eight intervals per side at a time.
    ///
    /// # Safety
    ///
    /// Requires `neon`, which the caller establishes, and requires `xp` and `yp`
    /// to be valid run payloads — strictly ascending, non-overlapping, every
    /// `start + len_minus_1 <= u16::MAX`. That is bound **B6**'s premise and
    /// `codec::decode` enforces all three.
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn block_and_cardinality(xp: &[Iv], yp: &[Iv]) -> u32 {
        let (nx, ny) = (xp.len(), yp.len());
        let (mut i, mut j, mut n) = (0usize, 0usize, 0u32);
        if nx >= SIMD_BLOCK && ny >= SIMD_BLOCK {
            let (ix, iy) = (nx - SIMD_BLOCK + 1, ny - SIMD_BLOCK + 1);
            // SAFETY: B5, which is the only bound *memory safety* rests on.
            // `i < ix == nx - 7` gives `i + 8 <= nx`, so the de-interleaving
            // load at `xp[i]` covers intervals `i..i + 8` and the
            // `get_unchecked(i + 7)` is the last of them; `j` likewise. Neither
            // cursor moves until after every read. The two `.cast::<u16>()` go
            // from `[u16; 2]` to `u16`, which cannot reduce alignment, and
            // `vld1q_u16` requires only element alignment.
            //
            // B6 is asserted alongside but is **not** a safety bound: an
            // overflowing lane would return a wrong cardinality, not corrupt
            // memory. It is checked here because that is where its premise —
            // the `y` block being disjoint — is in hand.
            unsafe {
                let mut sum = vdupq_n_u32(0);
                let mut cnt = vdupq_n_u32(0);
                while i < ix && j < iy {
                    debug_assert!(i + SIMD_BLOCK <= nx && j + SIMD_BLOCK <= ny, "B5");
                    debug_assert!(lane_sums_fit_u16(xp, yp, i, j), "B6");
                    // **These two reads must stay above the vector loads.**
                    // They are the front of the loop-carried chain — last end,
                    // compare, select a cursor, form the next address — and the
                    // module header explains why that chain is the whole
                    // result. Below the loads, GVN proves `xp[i + 7]` is lanes
                    // 6 and 7 of a register it already has and rewrites the
                    // load as `dup` + `add` + `umov`, putting the SIMD-to-GPR
                    // move back on the chain: **2.05x becomes 1.09x, with every
                    // test still green.** Read first, and there is nothing to
                    // fold into.
                    let xl = *xp.get_unchecked(i + SIMD_BLOCK - 1);
                    let yl = *yp.get_unchecked(j + SIMD_BLOCK - 1);
                    let (amax, bmax) = (ivend(xl), ivend(yl));

                    // 2-byte aligned by construction: a run payload is
                    // `[nruns][start, len]...` so it is never 4-byte aligned,
                    // and `vld1q_u16` needs only element alignment.
                    //
                    // **Two `vld1q` and a `uzp` pair, not one `vld2q_u16`.**
                    // The de-interleaving load does this in one instruction —
                    // and MIRI does not implement it ( "unsupported operation"
                    // on `llvm.aarch64.neon.ld2` ), which would take `ops::run`
                    // out of `scripts/miri.sh`'s tier 1, where `ops::array` and
                    // `ops::bitmap`'s kernels both pass today. It is also 3-5%
                    // *slower* here once the reads above are ordered correctly,
                    // because LLVM folds `uzp1 + uzp2 + add` into one `addp`.
                    // Do not "restore" the `vld2q`.
                    let (xq, yq) = (
                        xp.as_ptr().add(i).cast::<u16>(),
                        yp.as_ptr().add(j).cast::<u16>(),
                    );
                    let (xa, xb) = (vld1q_u16(xq), vld1q_u16(xq.add(8)));
                    let (ya, yb) = (vld1q_u16(yq), vld1q_u16(yq.add(8)));
                    // `vuzp1` takes the even lanes of both registers and `vuzp2`
                    // the odd ones, so the pair splits exactly as `vld2q` would:
                    // eight starts, then eight `len_minus_1`.
                    let xs = vuzp1q_u16(xa, xb);
                    let xe = vaddq_u16(xs, vuzp2q_u16(xa, xb));
                    let ys = vuzp1q_u16(ya, yb);
                    let ye = vaddq_u16(ys, vuzp2q_u16(ya, yb));

                    let mut d16 = vdupq_n_u16(0);
                    let mut c16 = vdupq_n_u16(0);
                    // Eight rotations of `y` against a fixed `x` is the whole
                    // 8x8 product. Two intervals overlap iff
                    // `max(starts) <= min(ends)`, which is one compare where the
                    // literal `sa <= eb && sb <= ea` is two and an `and`; and
                    // the *saturating* subtract is already zero where they do
                    // not overlap, so the length needs no mask either.
                    macro_rules! rot {
                        ($r:literal) => {{
                            let rs = vextq_u16::<$r>(ys, ys);
                            let re = vextq_u16::<$r>(ye, ye);
                            let lo = vmaxq_u16(xs, rs);
                            let hi = vminq_u16(xe, re);
                            d16 = vaddq_u16(d16, vqsubq_u16(hi, lo));
                            c16 = vsubq_u16(c16, vcleq_u16(lo, hi));
                        }};
                    }
                    rot!(0);
                    rot!(1);
                    rot!(2);
                    rot!(3);
                    rot!(4);
                    rot!(5);
                    rot!(6);
                    rot!(7);
                    sum = vpadalq_u16(sum, d16);
                    cnt = vpadalq_u16(cnt, c16);

                    // **Read the block's last end from memory, not with
                    // `vgetq_lane_u16::<7>(xe)`.** The lane extract is a
                    // SIMD-to-GPR move sitting on the loop-carried dependency
                    // chain that picks the side to retire; a scalar load's
                    // address depends only on the cursor, so it issues
                    // alongside the vector work. That one change is the whole
                    // distance between 1.07x and 1.75x — see the module header.
                    // Retire whichever block ends lower; on a tie both, since
                    // neither side's next interval can reach the other's block.
                    //
                    // The tie is a **throughput** choice: weakening either
                    // `<=` to `<` leaves every test green, because the
                    // un-retired side simply spends one more block finding
                    // nothing. Same class as the tie-break in
                    // `ops::array::simd::merge_cardinality`. Do not read the
                    // suite passing as evidence these two lines are right.
                    i += usize::from(amax <= bmax) * SIMD_BLOCK;
                    j += usize::from(bmax <= amax) * SIMD_BLOCK;
                }
                n = vaddvq_u32(sum) + vaddvq_u32(cnt);
            }
        }
        // Whatever is left is shorter than a block on at least one side.
        n + super::scalar_merge_cardinality(&xp[i..], &yp[j..])
    }
}

/// The SSE block arm, carrying bounds **B5** and **B6** unchanged.
///
/// # Three places x86 needs a different instruction, not a renamed one
///
/// **De-interleaving.** NEON splits `[ start, len ]` pairs with `uzp1` / `uzp2`.
/// x86 has no lane-deinterleave, but `packus_epi32` is one: masking each 32-bit
/// lane to its low half and packing yields the eight starts, and shifting right
/// by 16 first yields the eight lengths. Two instructions each, and the
/// saturation never fires because both inputs are already 16-bit.
///
/// **Unsigned compare.** `vcleq_u16` has no SSE2 equivalent -- `cmpgt_epi16` is
/// signed and would mis-order every value at or above `0x8000`. `min_epu16( lo,
/// hi ) == lo` is the unsigned test, the same shape `ops::mixed` uses.
///
/// **Pairwise widening accumulate.** `vpadalq_u16` has no direct twin, and the
/// obvious `madd_epi16` is **wrong here**: it reads its inputs as *signed*, and
/// a `d16` lane reaching `0x8000` would be accumulated as a negative number.
/// B6 permits lanes up to `65535`, so this unpacks against zero into `u32`
/// instead -- two instructions where NEON has one, and correct for the whole
/// range the bound allows.
///
/// # The crate-level figure is 1.10x, which is inside the noise
///
/// An x86 figure of 1.10x was recorded here on 2026-09-18 from a probe with a
/// hoistable timing loop and is withdrawn along with it.
///
/// **Re-measured on aarch64 with that defect fixed and at 500 000 iterations:
/// 131.7 ns with this arm, 239.2 ns without -- `1.82x`.**
/// `run x run` is one of the two cases in that harness that is stable and
/// responds to exactly one module: disabling `ops::array`, `ops::bitmap` or
/// `ops::mixed` leaves it at 131-133 ns. That single-module response is what
/// makes the number trustworthy, and it is the check to repeat before quoting
/// any figure here. The x86 arm is unmeasured. See
/// `.agents/docs/LTM/simd-arch-arms-and-kernel-selection.md`.
#[cfg(target_arch = "x86_64")]
mod simd {
    use super::{ivend, lane_sums_fit_u16, Iv, SIMD_BLOCK};
    use core::arch::x86_64::*;

    /// # Safety
    ///
    /// Requires `sse4.1` and `ssse3`, which the caller establishes.
    #[target_feature(enable = "sse4.1,ssse3")]
    pub(super) unsafe fn block_and_cardinality(xp: &[Iv], yp: &[Iv]) -> u32 {
        let (nx, ny) = (xp.len(), yp.len());
        let (mut i, mut j, mut n) = (0usize, 0usize, 0u32);
        if nx >= SIMD_BLOCK && ny >= SIMD_BLOCK {
            let (ix, iy) = (nx - SIMD_BLOCK + 1, ny - SIMD_BLOCK + 1);
            // SAFETY: B5, which is the only bound *memory safety* rests on.
            // `i < ix == nx - 7` gives `i + 8 <= nx`, so the two 16-byte loads
            // at `xp[i]` cover intervals `i..i + 8` and the
            // `get_unchecked( i + 7 )` is the last of them; `j` likewise.
            // Neither cursor moves until after every read. The `.cast::<u16>()`
            // goes from `[u16; 2]` to `u16`, which cannot reduce alignment, and
            // `loadu` requires none.
            //
            // B6 is asserted alongside but is **not** a safety bound: an
            // overflowing lane would return a wrong cardinality, not corrupt
            // memory.
            unsafe {
                let lo32 = _mm_set1_epi32(0x0000_FFFF);
                let zero = _mm_setzero_si128();
                let mut sum = _mm_setzero_si128();
                let mut cnt = _mm_setzero_si128();
                while i < ix && j < iy {
                    debug_assert!(i + SIMD_BLOCK <= nx && j + SIMD_BLOCK <= ny, "B5");
                    debug_assert!(lane_sums_fit_u16(xp, yp, i, j), "B6");
                    // **These two reads must stay above the vector loads**, for
                    // the reason the NEON arm records at length: they are the
                    // front of the loop-carried chain that picks which side to
                    // retire, and below the loads the compiler is free to prove
                    // them redundant with lanes it already holds and rewrite
                    // them as a vector-to-GPR extract -- which puts that move
                    // back on the chain. Read first, and there is nothing to
                    // fold into.
                    let xl = *xp.get_unchecked(i + SIMD_BLOCK - 1);
                    let yl = *yp.get_unchecked(j + SIMD_BLOCK - 1);
                    let (amax, bmax) = (ivend(xl), ivend(yl));

                    let (xq, yq) = (
                        xp.as_ptr().add(i).cast::<__m128i>(),
                        yp.as_ptr().add(j).cast::<__m128i>(),
                    );
                    let (xa, xb) = (_mm_loadu_si128(xq), _mm_loadu_si128(xq.add(1)));
                    let (ya, yb) = (_mm_loadu_si128(yq), _mm_loadu_si128(yq.add(1)));
                    // Eight starts, then eight `len_minus_1`, split as `uzp`
                    // would: see the module note on `packus_epi32`.
                    let xs = _mm_packus_epi32(_mm_and_si128(xa, lo32), _mm_and_si128(xb, lo32));
                    let xlen = _mm_packus_epi32(_mm_srli_epi32::<16>(xa), _mm_srli_epi32::<16>(xb));
                    let xe = _mm_add_epi16(xs, xlen);
                    let ys = _mm_packus_epi32(_mm_and_si128(ya, lo32), _mm_and_si128(yb, lo32));
                    let ylen = _mm_packus_epi32(_mm_srli_epi32::<16>(ya), _mm_srli_epi32::<16>(yb));
                    let ye = _mm_add_epi16(ys, ylen);

                    let mut d16 = _mm_setzero_si128();
                    let mut c16 = _mm_setzero_si128();
                    // Eight rotations of `y` against a fixed `x` is the whole
                    // 8x8 product. Two intervals overlap iff
                    // `max( starts ) <= min( ends )`, and the *saturating*
                    // subtract is already zero where they do not overlap, so
                    // the length needs no mask either.
                    macro_rules! rot {
                        ($r:literal) => {{
                            let rs = _mm_alignr_epi8::<$r>(ys, ys);
                            let re = _mm_alignr_epi8::<$r>(ye, ye);
                            let lo = _mm_max_epu16(xs, rs);
                            let hi = _mm_min_epu16(xe, re);
                            d16 = _mm_add_epi16(d16, _mm_subs_epu16(hi, lo));
                            // `lo <= hi` unsigned, as a 0/-1 mask; subtracting
                            // it increments the lane.
                            let le = _mm_cmpeq_epi16(_mm_min_epu16(lo, hi), lo);
                            c16 = _mm_sub_epi16(c16, le);
                        }};
                    }
                    rot!(0);
                    rot!(2);
                    rot!(4);
                    rot!(6);
                    rot!(8);
                    rot!(10);
                    rot!(12);
                    rot!(14);
                    // Widen against zero rather than `madd_epi16`: see the
                    // module note. A `d16` lane may legitimately exceed
                    // `0x7FFF` under B6.
                    sum = _mm_add_epi32(sum, _mm_unpacklo_epi16(d16, zero));
                    sum = _mm_add_epi32(sum, _mm_unpackhi_epi16(d16, zero));
                    cnt = _mm_add_epi32(cnt, _mm_unpacklo_epi16(c16, zero));
                    cnt = _mm_add_epi32(cnt, _mm_unpackhi_epi16(c16, zero));

                    // Retire whichever block ends lower; on a tie both, since
                    // neither side's next interval can reach the other's block.
                    // The tie is a **throughput** choice: weakening either `<=`
                    // to `<` leaves every test green, because the un-retired
                    // side simply spends one more block finding nothing. Do not
                    // read the suite passing as evidence these two lines are
                    // right.
                    i += usize::from(amax <= bmax) * SIMD_BLOCK;
                    j += usize::from(bmax <= amax) * SIMD_BLOCK;
                }
                let tot = _mm_add_epi32(sum, cnt);
                let t2 = _mm_add_epi32(tot, _mm_shuffle_epi32::<0b1110>(tot));
                n = _mm_cvtsi128_si32(_mm_add_epi32(t2, _mm_shuffle_epi32::<0b0001>(t2))) as u32;
            }
        }
        // Whatever is left is shorter than a block on at least one side.
        n + super::scalar_merge_cardinality(&xp[i..], &yp[j..])
    }
}

/// The counting two-pointer over intervals.
///
/// Oracle for [`simd::block_and_cardinality`] ( QG §4.3 ) and the whole merge
/// branch on every target that has no vector arm. Do not delete it when the
/// vector arm is faster.
fn scalar_merge_cardinality(xp: &[Iv], yp: &[Iv]) -> u32 {
    // The sparser side first, exactly as `and_each`'s merge branch does.
    let (p, q) = if xp.len() <= yp.len() {
        (xp, yp)
    } else {
        (yp, xp)
    };
    let mut total = 0u32;
    merge_and_each(p, q, |s, t| {
        total += (t - s) as u32 + 1;
    });
    total
}

/// The vector merge if this build can reach one, the scalar merge otherwise.
///
/// The length test comes first and short-circuits the feature detection: below a
/// block the vector arm has nothing to do but run its scalar tail, and calling
/// it anyway measured **0.91x** at `nruns = 1`, where the whole call is 2 ns.
/// The threshold is `SIMD_BLOCK` because that is when a block can run at all —
/// it is not a tuned constant, and nothing was swept to choose it.
///
/// **Lowering it to 1 leaves every test green**, because the kernel carries
/// the same guard internally and simply runs its scalar tail. This gate is a
/// cost decision only; the suite cannot tell you it is here.
#[inline]
fn merge_cardinality(xp: &[Iv], yp: &[Iv]) -> u32 {
    #[cfg(target_arch = "aarch64")]
    if xp.len() >= SIMD_BLOCK
        && yp.len() >= SIMD_BLOCK
        && std::arch::is_aarch64_feature_detected!("neon")
    {
        // SAFETY: `neon` was just detected, and both slices came from `pairs`
        // over a decoded run payload, which is B6's premise.
        return unsafe { simd::block_and_cardinality(xp, yp) };
    }
    #[cfg(target_arch = "x86_64")]
    if xp.len() >= SIMD_BLOCK
        && yp.len() >= SIMD_BLOCK
        && std::arch::is_x86_feature_detected!("sse4.1")
    {
        // SAFETY: `sse4.1` was just detected ( and implies `ssse3` on every CPU
        // that reports it ), and both slices came from `pairs` over a decoded
        // run payload, which is B6's premise.
        return unsafe { simd::block_and_cardinality(xp, yp) };
    }
    scalar_merge_cardinality(xp, yp)
}

/// `|a ∩ b|` over two flat run payloads, without allocating.
///
/// `tests/allocation.rs` pins this arm as allocation-free; the closure is
/// monomorphized into the walk and nothing here touches the heap.
///
/// **The gallop-versus-merge decision is [`should_gallop`], the same call
/// [`and_each`] makes**, for the reason stated there: two paths that answer the
/// same question at different costs, with nothing reporting the divergence, is a
/// defect this crate has shipped before. Only the *merge* branch differs, and
/// only because counting has a vector form that emitting does not.
pub(crate) fn and_cardinality(xf: &[u16], yf: &[u16]) -> u32 {
    let (xp, yp) = (pairs(xf), pairs(yf));
    if should_gallop(xp.len(), yp.len()) {
        let mut total = 0u32;
        gallop_and_each(xp, yp, |s, t| {
            total += (t - s) as u32 + 1;
        });
        return total;
    }
    merge_cardinality(xp, yp)
}

/// `a ∩ b = ∅`, stopping at the first meeting pair.
///
/// The early exit is why this is not `and_cardinality(..) == 0`: the *false*
/// answer must stay cheap. It makes the same gallop-vs-merge decision, which is
/// the invariant that `is_disjoint` may never cost more than the count.
pub(crate) fn is_disjoint(xf: &[u16], yf: &[u16]) -> bool {
    let (xp, yp) = (pairs(xf), pairs(yf));
    if should_gallop(xp.len(), yp.len()) {
        let (small, large) = if xp.len() <= yp.len() {
            (xp, yp)
        } else {
            (yp, xp)
        };
        gallop_is_disjoint(small, large)
    } else {
        merge_is_disjoint(xp, yp)
    }
}

/// [`is_disjoint`]'s merge branch, kept as its own function so the property
/// tests can hold it against the gallop branch on identical inputs rather than
/// having to construct operands skewed enough to select one.
fn merge_is_disjoint(xp: &[Iv], yp: &[Iv]) -> bool {
    let (mut xi, mut yi) = (xp.iter(), yp.iter());
    let (Some(&x0), Some(&y0)) = (xi.next(), yi.next()) else {
        return true;
    };
    let (mut xs, mut xe) = (ivstart(x0), ivend(x0));
    let (mut ys, mut ye) = (ivstart(y0), ivend(y0));
    loop {
        if xs.max(ys) <= xe.min(ye) {
            return false;
        }
        if xe < ye {
            let Some(&v) = xi.next() else { return true };
            xs = ivstart(v);
            xe = ivend(v);
        } else {
            let Some(&v) = yi.next() else { return true };
            ys = ivstart(v);
            ye = ivend(v);
        }
    }
}

/// [`is_disjoint`]'s gallop branch. `small` must be the shorter list, but the
/// answer does not depend on that — only the cost does, which is what lets the
/// property tests run it on any pair.
fn gallop_is_disjoint(small: &[Iv], large: &[Iv]) -> bool {
    let mut j = 0usize;
    for &sv in small {
        j = gallop_end(large, j, ivstart(sv));
        if j >= large.len() {
            return true;
        }
        // The probe guarantees `end(large[j]) >= start(sv)`, so the two meet iff
        // the large interval also begins at or before `end(sv)`. `<=` and not
        // `<`: an interval starting exactly on `end(sv)` shares that value.
        if ivstart(large[j]) <= ivend(sv) {
            return false;
        }
    }
    true
}

/// `b ⊆ a`: every interval of `b` must sit inside one interval of `a`.
///
/// Asymmetric — `b` drives and the sides cannot be swapped — so the gallop
/// test is `|a| / |b|` rather than large-over-small, exactly as in
/// `ops::array::contains_all`. Probing `b` into `a` is the only legal direction.
///
/// `a`'s intervals are disjoint and ordered, so the interval that could contain
/// `b[j]` is unique: the first whose end reaches `b[j]`'s start. Everything else
/// is rejecting it.
pub(crate) fn contains_all(af: &[u16], bf: &[u16]) -> bool {
    let (ap, bp) = (pairs(af), pairs(bf));
    if bp.is_empty() {
        return true;
    }
    if ap.is_empty() {
        return false;
    }
    // Two functions rather than one loop with the decision inside it. Hoisting
    // the test into a `bool` and branching on it per interval of `b` is the
    // obvious spelling and measured **1.18x** on the balanced point, where the
    // branch never changes its answer and buys nothing at all.
    if ap.len() >= bp.len() * GALLOP_RATIO {
        gallop_contains_all(ap, bp)
    } else {
        walk_contains_all(ap, bp)
    }
}

/// [`contains_all`]'s forward-walk branch.
fn walk_contains_all(ap: &[Iv], bp: &[Iv]) -> bool {
    let mut i = 0usize;
    for &bv in bp {
        let s = ivstart(bv);
        while i < ap.len() && ivend(ap[i]) < s {
            i += 1;
        }
        if i >= ap.len() || !covers(ap[i], bv) {
            return false;
        }
    }
    true
}

/// [`contains_all`]'s gallop branch, which must agree with [`walk_contains_all`]
/// on every input and not merely on skewed ones.
fn gallop_contains_all(ap: &[Iv], bp: &[Iv]) -> bool {
    let mut i = 0usize;
    for &bv in bp {
        // Carrying `i` forward is what makes each probe cost `log` of the
        // *remaining* list rather than `log |a|` — and it is a **cost** property,
        // not a correctness one: restarting from 0 every iteration returns the
        // same index and leaves every test green.
        i = gallop_end(ap, i, ivstart(bv));
        if i >= ap.len() || !covers(ap[i], bv) {
            return false;
        }
    }
    true
}

/// `b` sits entirely inside `a`, given that `a` is the first interval whose end
/// reaches `b`'s start.
///
/// The search establishes `end(a) >= start(b)`. Containment additionally needs
/// `a` to begin no later than `b` and to run at least as far — the second is the
/// case a *counting* kernel never has to notice, a `b` interval that begins
/// inside an `a` interval and outlives it.
#[inline]
fn covers(a: Iv, b: Iv) -> bool {
    ivstart(a) <= ivstart(b) && ivend(a) >= ivend(b)
}

/// Accumulates `(start, end)` inclusive intervals, merging any that touch.
///
/// The coalescing is the point. `[0,3]` and `[4,7]` are two intervals a naive
/// union would emit and one interval the format requires, and nothing
/// downstream re-checks.
struct Emit {
    out: Vec<(u16, u16)>,
    len: u32,
}

impl Emit {
    fn new(cap: usize) -> Self {
        Emit {
            out: Vec::with_capacity(cap),
            len: 0,
        }
    }

    /// Push `[s, e]` inclusive.
    #[inline]
    fn push(&mut self, s: u16, e: u16) {
        debug_assert!(s <= e, "empty interval reached the emitter");
        if let Some(last) = self.out.last_mut() {
            let prev_end = last.1 as u32;
            // Touching counts: `prev_end + 1 == s` must merge, not append.
            if s as u32 <= prev_end + 1 {
                if e as u32 > prev_end {
                    self.len += e as u32 - prev_end;
                    last.1 = e;
                }
                return;
            }
        }
        self.out.push((s, e));
        self.len += (e - s) as u32 + 1;
    }

    fn finish(self) -> Option<Container> {
        if self.out.is_empty() {
            // An empty container is never stored.
            return None;
        }
        let mut c = Container::Run(RunContainer::from_pairs(&self.out));
        // A result may be cheaper as an array or bitmap than as runs.
        c.optimize();
        Some(c)
    }
}

/// Apply `op` when both operands are runs, else `None` to fall through.
#[inline]
pub fn try_apply(op: SetOp, a: &Container, b: &Container) -> Option<Option<Container>> {
    let (Container::Run(x), Container::Run(y)) = (a, b) else {
        return None;
    };
    let (xp, yp) = (pairs(x.as_flat()), pairs(y.as_flat()));
    let (n, m) = (xp.len(), yp.len());
    let mut e = Emit::new(n + m);
    let (mut i, mut j) = (0usize, 0usize);

    match op {
        SetOp::And => {
            and_each(xp, yp, |s, t| e.push(s, t));
        }
        SetOp::Or => {
            while i < n && j < m {
                let (xv, yv) = (xp[i], yp[j]);
                if ivstart(xv) <= ivstart(yv) {
                    e.push(ivstart(xv), ivend(xv));
                    i += 1;
                } else {
                    e.push(ivstart(yv), ivend(yv));
                    j += 1;
                }
            }
            // Not `extend`: the tail still has to go through `Emit`, which is
            // what coalesces an interval that now touches the last one emitted.
            for &v in &xp[i..] {
                e.push(ivstart(v), ivend(v));
            }
            for &v in &yp[j..] {
                e.push(ivstart(v), ivend(v));
            }
        }
        SetOp::Xor | SetOp::AndNot => {
            // A boundary sweep, not a walk over one side.
            //
            // The obvious implementation — iterate lhs and subtract whatever of
            // rhs overlaps — cannot emit a rhs-only region that begins *before*
            // lhs does, which `Xor([20,30], [0,100])` does immediately. Sweeping
            // positions and asking "which sides cover this?" makes both
            // operations the same loop with a different predicate, and there is
            // no asymmetric case left to forget.
            if n == 0 && m == 0 {
                return Some(None);
            }
            let mut pos: u32 = match (n > 0, m > 0) {
                (true, true) => (ivstart(xp[0]) as u32).min(ivstart(yp[0]) as u32),
                (true, false) => ivstart(xp[0]) as u32,
                (false, true) => ivstart(yp[0]) as u32,
                (false, false) => unreachable!("handled above"),
            };
            loop {
                // Retire intervals that end before the sweep position.
                while i < n && (ivend(xp[i]) as u32) < pos {
                    i += 1;
                }
                while j < m && (ivend(yp[j]) as u32) < pos {
                    j += 1;
                }
                if i >= n && j >= m {
                    break;
                }
                // One read per side per sweep step; the sweep asked for each
                // endpoint up to three times before.
                let (xs, xe) = if i < n {
                    (ivstart(xp[i]) as u32, ivend(xp[i]) as u32)
                } else {
                    (0, 0)
                };
                let (ys, ye) = if j < m {
                    (ivstart(yp[j]) as u32, ivend(yp[j]) as u32)
                } else {
                    (0, 0)
                };
                let in_a = i < n && xs <= pos;
                let in_b = j < m && ys <= pos;

                // The next position where membership can change: the end of an
                // interval we are inside, or the start of one we are before.
                let mut next = u32::MAX;
                if i < n {
                    next = next.min(if in_a { xe + 1 } else { xs });
                }
                if j < m {
                    next = next.min(if in_b { ye + 1 } else { ys });
                }

                let keep = match op {
                    SetOp::Xor => in_a != in_b,
                    SetOp::AndNot => in_a && !in_b,
                    _ => unreachable!("only Xor and AndNot reach this arm"),
                };
                if keep && next > pos {
                    e.push(pos as u16, (next - 1) as u16);
                }
                if next == u32::MAX {
                    break;
                }
                pos = next;
            }
        }
    }

    Some(e.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::generic;

    /// `(start, end)` inclusive, which is what `from_pairs` takes.
    fn run(pairs: &[(u16, u16)]) -> Container {
        Container::Run(RunContainer::from_pairs(pairs))
    }

    const OPS: [SetOp; 4] = [SetOp::And, SetOp::Or, SetOp::Xor, SetOp::AndNot];

    /// A pair of operands, as `(start, end)` inclusive interval lists.
    type Case = (Vec<(u16, u16)>, Vec<(u16, u16)>);

    /// The specialization must be indistinguishable from the oracle.
    #[test]
    fn every_arm_agrees_with_the_generic_kernel() {
        let cases: Vec<Case> = vec![
            // Disjoint, touching, overlapping, nested, identical, and the ends.
            (vec![(0, 3)], vec![(10, 13)]),
            (vec![(0, 3)], vec![(4, 7)]),
            (vec![(0, 10)], vec![(5, 15)]),
            (vec![(0, 100)], vec![(20, 30)]),
            (vec![(20, 30)], vec![(0, 100)]),
            (vec![(0, 5)], vec![(0, 5)]),
            (vec![(0, 0)], vec![(0, 0)]),
            (vec![(65535, 65535)], vec![(65535, 65535)]),
            (vec![(0, 65535)], vec![(100, 200)]),
            (
                vec![(0, 5), (10, 15), (20, 25)],
                vec![(3, 12), (22, 30), (40, 50)],
            ),
            (vec![(1, 2), (5, 6), (9, 10)], vec![(0, 20)]),
            (vec![(0, 20)], vec![(1, 2), (5, 6), (9, 10)]),
        ];

        for (av, bv) in cases {
            let (a, b) = (run(&av), run(&bv));
            for op in OPS {
                let fast = try_apply(op, &a, &b).expect("both are runs");
                let slow = generic::apply(op, &a, &b);
                let f: Vec<u16> = fast
                    .as_ref()
                    .map(|c| c.iter().collect())
                    .unwrap_or_default();
                let s: Vec<u16> = slow
                    .as_ref()
                    .map(|c| c.iter().collect())
                    .unwrap_or_default();
                assert_eq!(f, s, "{op:?} on {av:?} vs {bv:?} disagrees with the oracle");
                assert_eq!(
                    fast.as_ref().map(|c| c.len()),
                    slow.as_ref().map(|c| c.len()),
                    "{op:?} on {av:?} vs {bv:?}: cardinality disagrees"
                );
            }
        }
    }

    /// Results must not contain touching runs — the format forbids it and every
    /// run kernel assumes maximal intervals.
    #[test]
    fn results_never_contain_adjacent_runs() {
        let a = run(&[(0, 3), (8, 11)]);
        let b = run(&[(4, 7), (12, 15)]);
        for op in OPS {
            if let Some(Some(c)) = try_apply(op, &a, &b) {
                crate::container::codec::validate(&c)
                    .unwrap_or_else(|e| panic!("{op:?} produced an invalid container: {e:?}"));
            }
        }
    }

    #[test]
    fn an_empty_result_is_none_not_an_empty_container() {
        let a = run(&[(0, 10)]);
        assert_eq!(try_apply(SetOp::Xor, &a, &a), Some(None));
        assert_eq!(try_apply(SetOp::AndNot, &a, &a), Some(None));
        let b = run(&[(20, 30)]);
        assert_eq!(try_apply(SetOp::And, &a, &b), Some(None));
    }

    /// Sorted, non-overlapping, **non-adjacent** interval lists.
    ///
    /// Uniform random `u16`s are useless here for the reason
    /// `tests/proptest_oracle.rs` states about uniform `u64`s: they would never
    /// produce a run container at all, and here they would not even produce a
    /// *valid* one. The lists are therefore built by walking — gap of at least 2
    /// so no two intervals touch — and the four arms are biased at the two shapes
    /// the gallop lives between: few long intervals, and many short ones.
    fn intervals(max: usize) -> impl proptest::strategy::Strategy<Value = Vec<(u16, u16)>> {
        use proptest::prelude::*;
        prop_oneof![
            // Few and long: the side a probe is driven *from*.
            3 => prop::collection::vec((2u32..4000, 1u32..4000), 1..10.min(max)),
            // Many and short: the side it is driven *into*.
            3 => prop::collection::vec((2u32..48, 1u32..30), 1..max),
            // Comparable counts, so the merge branch is exercised too.
            2 => prop::collection::vec((2u32..300, 1u32..300), 1..(max / 4).max(2)),
            // Minimum legal spacing: single values two apart, where an
            // off-by-one in a `>=` shows up as an interval gained or lost.
            1 => prop::collection::vec((2u32..3, 1u32..2), 1..max),
        ]
        .prop_map(|steps| {
            let mut out: Vec<(u16, u16)> = Vec::new();
            let mut pos: u32 = 0;
            for (gap, len) in steps {
                let (s, e) = (pos + gap, pos + gap + len - 1);
                if e > 65535 {
                    break;
                }
                out.push((s as u16, e as u16));
                pos = e;
            }
            if out.is_empty() {
                out.push((0, 0));
            }
            out
        })
    }

    proptest::proptest! {
        /// The two branches of every galloped kernel, on identical inputs.
        ///
        /// This is the property that fails when the probe seeks a `start`
        /// instead of an `end`, or when the cursor advances past an interval
        /// that outlives the small one. Neither is visible on balanced operands,
        /// and neither needs the ratio gate to be reachable.
        #[test]
        fn the_gallop_branch_agrees_with_the_merge_branch(
            av in intervals(900),
            bv in intervals(900),
        ) {
            let (a, b) = (ivs(&av), ivs(&bv));
            for (l, r) in [(&a, &b), (&b, &a)] {
                proptest::prop_assert_eq!(
                    collect_and(gallop_out, l, r),
                    collect_and(merge_out, l, r),
                    "and: gallop and merge disagree"
                );
                proptest::prop_assert_eq!(
                    gallop_is_disjoint(l, r),
                    merge_is_disjoint(l, r),
                    "is_disjoint: gallop and merge disagree"
                );
                proptest::prop_assert_eq!(
                    gallop_contains_all(l, r),
                    walk_contains_all(l, r),
                    "contains_all: gallop and walk disagree"
                );
            }
        }
    }

    proptest::proptest! {
        // The oracle is the generic *value* merge, so a case costs the operands'
        // cardinality rather than their interval count; the operands are kept
        // small enough that 128 cases stay cheap in an unoptimized test build.
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(128))]

        /// The whole arm — dispatch, ratio gate and both branches — against
        /// `generic::apply`, which stays the oracle ( QG §4.2 ).
        #[test]
        fn the_run_arm_agrees_with_the_generic_kernel(
            av in intervals(120),
            bv in intervals(120),
        ) {
            let (a, b) = (run(&av), run(&bv));
            for (l, r) in [(&a, &b), (&b, &a)] {
                for op in OPS {
                    let fast = try_apply(op, l, r).expect("both are runs");
                    let slow = generic::apply(op, l, r);
                    let f: Vec<u16> = fast.as_ref().map(|c| c.iter().collect()).unwrap_or_default();
                    let s: Vec<u16> = slow.as_ref().map(|c| c.iter().collect()).unwrap_or_default();
                    proptest::prop_assert_eq!(f, s, "{:?} disagrees with the oracle", op);
                    if let Some(c) = fast {
                        // Non-adjacency is a postcondition of every arm.
                        proptest::prop_assert!(crate::container::codec::validate(&c).is_ok());
                    }
                }
            }
        }

        /// The three cardinality-only arms against the same oracle, through the
        /// `Container` dispatch in `ops::card` so the delegation is covered too.
        #[test]
        fn the_run_predicates_agree_with_the_generic_kernel(
            av in intervals(120),
            bv in intervals(120),
        ) {
            let (a, b) = (run(&av), run(&bv));
            for (l, r) in [(&a, &b), (&b, &a)] {
                let n = generic::apply(SetOp::And, l, r).map_or(0, |c| c.len());
                proptest::prop_assert_eq!(crate::ops::and_cardinality(l, r), n);
                proptest::prop_assert_eq!(crate::ops::is_disjoint(l, r), n == 0);
                proptest::prop_assert_eq!(crate::ops::contains_all(l, r), n == r.len());
            }
        }
    }

    #[test]
    fn non_run_pairs_fall_through() {
        let r = run(&[(0, 10)]);
        let arr = Container::from_sorted(&[1, 2, 3]);
        assert!(try_apply(SetOp::And, &r, &arr).is_none());
        assert!(try_apply(SetOp::And, &arr, &r).is_none());
    }

    // ---------------------------------------------------------------------
    // The gallop branch.
    //
    // The tests below hold the gallop branch against the merge branch on the
    // **same** inputs rather than against the oracle only, and they call the two
    // directly rather than through the ratio gate. That is deliberate: a bug in
    // the probe is a bug whatever the operands' size ratio happens to be, and a
    // property that can only reach the gallop through `should_gallop` is a
    // property whose coverage a later tuning change could silently delete.
    // ---------------------------------------------------------------------

    fn ivs(v: &[(u16, u16)]) -> Vec<Iv> {
        v.iter().map(|&(s, e)| [s, e - s]).collect()
    }

    /// The flat payload of a container the test just built as a run.
    fn flat(c: &Container) -> &[u16] {
        match c {
            Container::Run(r) => r.as_flat(),
            _ => panic!("the test built a run and got {:?}", c.kind()),
        }
    }

    /// One of the two AND branches, erased so a test can name either.
    type AndBranch = fn(&[Iv], &[Iv], &mut dyn FnMut(u16, u16));

    fn collect_and(f: AndBranch, a: &[Iv], b: &[Iv]) -> Vec<(u16, u16)> {
        let mut out = Vec::new();
        f(a, b, &mut |s, t| out.push((s, t)));
        out
    }

    fn merge_out(a: &[Iv], b: &[Iv], f: &mut dyn FnMut(u16, u16)) {
        merge_and_each(a, b, f)
    }
    fn gallop_out(a: &[Iv], b: &[Iv], f: &mut dyn FnMut(u16, u16)) {
        gallop_and_each(a, b, f)
    }

    #[test]
    fn gallop_end_finds_the_first_interval_whose_end_reaches_the_target() {
        // Ends are 4, 14, 24, ... — one interval per decade, none touching.
        let p = ivs(&[(0, 4), (10, 14), (20, 24), (30, 34)]);
        assert_eq!(gallop_end(&p, 0, 0), 0);
        // 4 is the *end* of interval 0, so interval 0 still reaches it.
        assert_eq!(
            gallop_end(&p, 0, 4),
            0,
            "an interval ending on the target reaches it"
        );
        assert_eq!(gallop_end(&p, 0, 5), 1, "past interval 0's end");
        assert_eq!(gallop_end(&p, 0, 10), 1);
        assert_eq!(gallop_end(&p, 0, 34), 3);
        assert_eq!(gallop_end(&p, 0, 35), 4, "past the end of the list");
        // Starting past the answer must not rewind.
        assert_eq!(gallop_end(&p, 2, 0), 2);
        assert_eq!(gallop_end(&[], 0, 7), 0);
    }

    /// The shape that a gallop written against `start` gets wrong.
    ///
    /// One long interval spans several short ones, so the probe must ( a ) find
    /// it by its **end** rather than its start and ( b ) leave the cursor on it
    /// rather than advancing, because it meets the next small interval too.
    #[test]
    fn a_spanning_interval_meets_every_small_interval_it_covers() {
        let small = ivs(&[(0, 100)]);
        let large = ivs(&[(10, 20), (30, 40), (50, 60)]);
        assert_eq!(
            collect_and(gallop_out, &small, &large),
            vec![(10, 20), (30, 40), (50, 60)]
        );
        // And the dual: the spanning interval is on the *probed* side, so the
        // cursor must stay on it across three consecutive small intervals.
        let small2 = ivs(&[(10, 20), (30, 40), (50, 60)]);
        let large2 = ivs(&[(0, 100)]);
        assert_eq!(
            collect_and(gallop_out, &small2, &large2),
            vec![(10, 20), (30, 40), (50, 60)]
        );
        // Touching, not overlapping: `[0,10]` and `[10,20]` share exactly 10.
        assert_eq!(
            collect_and(gallop_out, &ivs(&[(0, 10)]), &ivs(&[(10, 20)])),
            vec![(10, 10)]
        );
        assert!(!gallop_is_disjoint(&ivs(&[(0, 10)]), &ivs(&[(10, 20)])));
        assert!(gallop_is_disjoint(&ivs(&[(0, 10)]), &ivs(&[(11, 20)])));
    }

    /// Skew past `GALLOP_RATIO` really does select the gallop, and the whole
    /// kernel still agrees with the oracle there.
    ///
    /// Without the `should_gallop` assertions this test would keep passing if the
    /// ratio were retuned out from under it, which is how a specialization stops
    /// being covered with nothing going red.
    #[test]
    fn skewed_operands_select_the_gallop_and_still_match_the_oracle() {
        let small: Vec<(u16, u16)> = (0..8u16)
            .map(|k| (k * 8192 + 100, k * 8192 + 4000))
            .collect();
        let large: Vec<(u16, u16)> = (0..1024u16).map(|k| (k * 64 + 3, k * 64 + 40)).collect();
        assert!(
            should_gallop(small.len(), large.len()),
            "the sweep must gallop"
        );
        assert!(
            large.len() >= small.len() * GALLOP_RATIO,
            "and `contains_all`'s asymmetric test must fire too"
        );
        let (a, b) = (run(&small), run(&large));
        for (l, r) in [(&a, &b), (&b, &a)] {
            for op in OPS {
                let fast = try_apply(op, l, r).expect("both are runs");
                let slow = generic::apply(op, l, r);
                let f: Vec<u16> = fast
                    .as_ref()
                    .map(|c| c.iter().collect())
                    .unwrap_or_default();
                let s: Vec<u16> = slow
                    .as_ref()
                    .map(|c| c.iter().collect())
                    .unwrap_or_default();
                assert_eq!(f, s, "{op:?} disagrees with the oracle on skewed runs");
            }
            let want = generic::apply(SetOp::And, l, r).map_or(0, |c| c.len());
            assert_eq!(and_cardinality(flat(l), flat(r)), want);
            assert_eq!(is_disjoint(flat(l), flat(r)), want == 0);
        }
        // A genuine subset, so `contains_all` walks to the end instead of
        // rejecting on the first interval.
        let sub: Vec<(u16, u16)> = large.iter().step_by(128).copied().collect();
        assert_eq!(sub.len(), 8);
        assert!(contains_all(flat(&b), flat(&run(&sub))));
        assert!(!contains_all(flat(&run(&sub)), flat(&b)));
    }

    // ---------------------------------------------------------------------
    // The vector merge.
    //
    // Held against `scalar_merge_cardinality` on the **same** inputs and
    // called directly, not through `merge_cardinality`'s length gate — for the
    // same reason the gallop tests bypass `should_gallop`. A block-boundary bug
    // is a bug whatever the operand lengths are, and a property reachable only
    // past a threshold is one a later retune silently deletes.
    // ---------------------------------------------------------------------

    /// The vector arm, called directly.
    ///
    /// Gated on the architecture rather than stubbed to the scalar merge
    /// elsewhere: a stub would make every assertion below `scalar == scalar`,
    /// and a test that passes because both sides are the same code is worse than
    /// an absent one — it reports coverage the build does not have.
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    fn vec_card(a: &[Iv], b: &[Iv]) -> u32 {
        // The arm this calls directly must be reachable, or the test degrades
        // into comparing the scalar two-pointer against itself.
        #[cfg(target_arch = "aarch64")]
        assert!(
            std::arch::is_aarch64_feature_detected!("neon"),
            "every aarch64 this crate targets has neon"
        );
        #[cfg(target_arch = "x86_64")]
        assert!(
            std::arch::is_x86_feature_detected!("sse4.1"),
            "every x86_64 this crate targets has sse4.1"
        );
        // SAFETY: `neon` was just detected; both operands are built by the
        // helpers below, which produce ascending non-overlapping intervals
        // inside the chunk — B6's premise.
        unsafe { simd::block_and_cardinality(a, b) }
    }

    /// `n` ascending non-overlapping intervals of length `len`, starting at
    /// `base` and repeating every `stride`. Panics rather than wrapping if the
    /// ladder would leave the chunk, so a mistyped case cannot silently become a
    /// different one.
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    fn ladder(base: u32, stride: u32, len: u32, n: usize) -> Vec<Iv> {
        assert!(stride >= len, "a ladder must not overlap itself");
        (0..n as u32)
            .map(|k| {
                let s = base + stride * k;
                assert!(s + len - 1 <= 65535, "ladder leaves the chunk");
                [s as u16, (len - 1) as u16]
            })
            .collect()
    }

    /// Every combination of operand lengths across the block boundary.
    ///
    /// This is the property that fails when the block loop's `i < ix` bound is
    /// off — bound **B5**, whose `debug_assert!` is live in a test build — or
    /// when the tail picks up the walk at the wrong cursor.
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn the_block_kernel_agrees_at_every_operand_length() {
        for na in 0..40usize {
            for nb in 0..40usize {
                // Deliberately different strides, so the two ladders interleave
                // rather than lining up block for block.
                let a = ladder(0, 7, 3, na);
                let b = ladder(2, 5, 2, nb);
                assert_eq!(
                    vec_card(&a, &b),
                    scalar_merge_cardinality(&a, &b),
                    "block kernel disagrees at {na} x {nb}"
                );
            }
        }
    }

    /// The shapes an 8x8 block gets wrong if the advance rule is wrong.
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn the_block_kernel_handles_spanning_and_touching_intervals() {
        let same = |a: &[Iv], b: &[Iv], what: &str| {
            assert_eq!(vec_card(a, b), scalar_merge_cardinality(a, b), "{what}");
            assert_eq!(
                vec_card(b, a),
                scalar_merge_cardinality(b, a),
                "{what} ( reversed )"
            );
        };

        // One interval spanning several blocks of small ones. The side holding
        // it must **not** be retired while the other still has intervals inside
        // it, which is the case a per-block "advance the lower end" rule gets
        // wrong if it looks at anything but the block's *last* end.
        same(
            &[[0, 60_000]],
            &ladder(0, 4, 2, 40),
            "one spanning interval",
        );

        // Bound **B6** at its extreme. Eight `x` intervals, one of them
        // nearly the whole chunk, against a `y` block that partitions that same
        // span — so lane 0 accumulates 8 * 8188 = 65 504 in a `u16`, the most a
        // lane can ever be asked to hold.
        let mut wide: Vec<Iv> = vec![[0, 65_520]];
        wide.extend(ladder(65_522, 2, 1, 7));
        let cover = ladder(0, 8_190, 8_189, 8);
        same(&wide, &cover, "B6 at its bound");

        // Touching across operands is not overlapping: `[0,7]` and `[8,15]`
        // share nothing, `[0,7]` and `[7,14]` share exactly one value.
        let l = ladder(0, 16, 8, 16);
        same(&l, &ladder(8, 16, 8, 16), "touching, not overlapping");
        same(&l, &ladder(7, 16, 8, 16), "overlapping in one value");

        // A single interval on one side, many blocks on the other: the block
        // loop never runs at all and everything falls to the tail.
        same(
            &[[100, 20_000]],
            &ladder(0, 64, 30, 900),
            "one against many",
        );
    }

    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    proptest::proptest! {
        /// The vector merge against its scalar oracle on boundary-biased
        /// operands, both orders.
        ///
        /// The generator's four arms straddle the 8-interval block on purpose:
        /// "few and long" is mostly tail, "many and short" is mostly blocks, and
        /// the minimum-spacing arm puts an off-by-one in the advance rule where
        /// it shows up as a whole interval gained or lost.
        #[test]
        fn the_block_kernel_agrees_with_the_scalar_merge(
            av in intervals(900),
            bv in intervals(900),
        ) {
            let (a, b) = (ivs(&av), ivs(&bv));
            for (l, r) in [(&a, &b), (&b, &a)] {
                proptest::prop_assert_eq!(
                    vec_card(l, r),
                    scalar_merge_cardinality(l, r),
                    "block kernel and scalar merge disagree"
                );
            }
        }
    }

    /// A result that is no longer run-shaped must not stay a run.
    #[test]
    fn a_scattered_result_is_re_encoded() {
        // Intersecting a long run with many tiny ones yields many tiny runs.
        let a = run(&[(0, 1000)]);
        let tiny: Vec<(u16, u16)> = (0..200u16).map(|i| (i * 5, i * 5)).collect();
        let b = run(&tiny);
        let c = try_apply(SetOp::And, &a, &b).unwrap().unwrap();
        assert_eq!(c.len(), 200);
        crate::container::codec::validate(&c).unwrap();
    }
}
