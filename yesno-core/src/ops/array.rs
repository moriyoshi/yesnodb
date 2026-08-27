//! Specialized array×array kernels.
//!
//! # These are the arms that actually mattered
//!
//! The M1 baseline measured dense binary ops 1.3-1.4x slower than the `roaring`
//! crate, and the first guess was bitmap×bitmap. It was wrong: the benchmark's
//! operands are 200 000 values over 64 chunks, which is ~3 100 per chunk —
//! comfortably under `ARRAY_MAX`, so **every container is an array** and the
//! bitmap specialization never fired. Measuring the assumption rather than
//! trusting it is what surfaced that.
//!
//! The cost is indirection, not algorithm. [`super::generic`] walks values
//! through a pair of peekable [`ContainerIter`](crate::container::ContainerIter)s,
//! so every element pays an enum dispatch and an `Option` dance. These kernels
//! work directly on `&[u16]`, which is the same merge with none of that.
//!
//! # Galloping on skew
//!
//! Intersecting a 10-element array with a 4000-element one should cost
//! `O(small · log large)`, not `O(small + large)`. Above [`GALLOP_RATIO`] the
//! smaller side drives an exponential-probe search into the larger.
//!
//! # The merge is branch-bound, and that is what the vector arm buys
//!
//! The scalar merge below **does not auto-vectorize, and could not**: `i` and
//! `j` advance by a data-dependent amount, so there is no loop-carried recurrence
//! LLVM can widen. Checked, not assumed — `--emit asm` on the release lib shows
//! zero vector instructions in `ops::array::{and, and_cardinality, is_disjoint,
//! contains_all}` on `aarch64-unknown-linux-gnu`, against the fully-vectorized
//! `cnt`/`uaddlp` popcount ladder the bitmap arm gets in the same object file.
//!
//! What the loop actually costs is **branch mispredicts**, one per element
//! consumed. That is not a fixed price: it is what the branch predictor can
//! still hold. Measured per element of `|a| + |b|`, the scalar merge runs
//! 0.66 ns at `m = 128` and 2.86 ns at `m = 4096` — the *same* code getting 4.3x
//! worse as the history outgrows the predictor. Rewriting it "branchlessly" does
//! not help either: LLVM re-materializes three conditional branches out of
//! `i += usize::from(x <= y)`, and it measured **0.9x**.
//!
//! The vector arm removes the branch rather than the compare. It takes eight
//! elements from each side, computes the 8×8 all-pairs match mask, and advances
//! the side with the smaller maximum — so the loop takes one branch per *eight*
//! elements and the advance is a `csel` pair. It costs a **flat 0.48 ns per
//! element from `m = 256` to `m = 4096`**, which is the property that matters:
//! the cost stops depending on what else the machine has been doing.
//!
//! Against the merge it replaced, on `intersect_vector_arm` ( ns per pair,
//! 64 distinct pairs per sweep, `aarch64-unknown-linux-gnu` ):
//!
//! ```text
//!   m      and_cardinality        and
//!          scalar  vector   x    scalar  vector   x
//!    32      53.4    18.4  2.9     47.4    35.8  1.3
//!   128     168.1   112.5  1.5    155.9   134.9  1.2
//!   256     437.4   240.5  1.8    474.5   275.1  1.7
//!   512    1819.0   484.5  3.8   1909.0   562.5  3.4
//!  1024    4779.0   996.0  4.8   4941.0  1091.0  4.5
//!  4096   23455.0  3939.0  6.0  24402.0  4253.0  5.7
//! ```
//!
//! The ratio grows with `m` because the **baseline** degrades, not because
//! the arm improves. Quoting the small-`m` ratios as "SIMD is only worth 1.5x
//! here" would be reading the predictor's capacity as a property of the kernel.
//!
//! **Only the merge arm is replaced. The gallop decision is untouched**, and
//! that is deliberate: on skewed operands past [`GALLOP_RATIO`] a vector merge
//! measured **0.84x** against the scalar gallop. `O(small · log large)` beats
//! `O(small + large)` however wide the lanes are — vector width does not change
//! an asymptote, and this is the one place a "vectorize the array kernel" task
//! can quietly make things worse.
//!
//! A kernel benchmark that intersects the **same two containers** on every
//! iteration cannot see any of this, and `intersect_crossover` is one. The
//! predictor memorizes the merge's decision sequence, and at `m = 1024` the
//! scalar loop then reports 1 087 ns per pair against the 4 760 ns it costs on
//! a fresh pair — **4.4x optimistic**. `intersect_branch_history` in
//! `benches/setops.rs` is the control that pins this down: it holds layout,
//! footprint and stride byte-identical and varies only the values, so the gap is
//! attributable to branch predictability and to nothing else. The vector arm
//! moves 952 -> 975 ns across the same control, i.e. not at all.
//!
//! Do not quote a scalar `array x array` figure from a group that reuses its
//! operands, and do not compare a candidate representation against one.
//!
//! ## What is *not* here
//!
//! **AArch64 NEON only.** Every other target — x86_64 included — runs the scalar
//! merge, and that is a deliberate gap rather than an oversight: the equivalent
//! SSE4.1 kernel is `_mm_cmpestrm`-shaped rather than a rotate ladder, so it is a
//! different kernel with a different cost, and there is no x86_64 machine here to
//! measure it on. Writing it unmeasured would put a number in this table that
//! nobody had seen. Tracked as `array-intersect-simd-x86`.
//!
//! The arm is selected by `is_aarch64_feature_detected!`, so the crate builds and
//! is correct anywhere. On `aarch64-unknown-linux-gnu` `neon` is a **baseline**
//! target feature, so that check constant-folds away entirely — verified in the
//! release assembly, where `and_cardinality` falls straight into the vector loop
//! with no atomic load and no branch. The runtime check costs nothing here and
//! is what keeps a `neon`-less AArch64 target correct.

use crate::container::{ArrayContainer, Container};
use crate::ops::generic::SetOp;
use crate::{ARRAY_MAX, GALLOP_RATIO};

/// Elements per side consumed by one iteration of the vector merge.
///
/// Eight `u16` is one 128-bit NEON register, which is the whole reason the arm
/// exists at that width and not another.
#[cfg(target_arch = "aarch64")]
const SIMD_BLOCK: usize = 8;

/// Apply `op` if both operands are arrays. `None` falls through to generic.
#[inline]
pub fn try_apply(op: SetOp, a: &Container, b: &Container) -> Option<Option<Container>> {
    let (Container::Array(x), Container::Array(y)) = (a, b) else {
        return None;
    };
    let (xs, ys) = (x.as_slice(), y.as_slice());
    let out = match op {
        SetOp::And => and(xs, ys),
        SetOp::Or => or(xs, ys),
        SetOp::Xor => xor(xs, ys),
        SetOp::AndNot => and_not(xs, ys),
    };
    if out.is_empty() {
        return Some(None);
    }
    // OR and XOR can exceed the array bound, in which case the result must
    // promote exactly as an insert would have.
    Some(Some(if out.len() > ARRAY_MAX {
        Container::from_sorted_vec(out)
    } else {
        Container::Array(ArrayContainer::from_sorted_vec(out))
    }))
}

/// Index of the first element `>= target`, searching from `lo` by exponential
/// probe then binary search.
#[inline]
fn gallop(s: &[u16], lo: usize, target: u16) -> usize {
    if lo >= s.len() || s[lo] >= target {
        return lo;
    }
    let mut step = 1usize;
    while lo + step < s.len() && s[lo + step] < target {
        step *= 2;
    }
    let hi = (lo + step + 1).min(s.len());
    let start = lo + step / 2;
    start + s[start..hi].partition_point(|&v| v < target)
}

/// The NEON merge arm.
///
/// Everything here relies on one shared bound, stated once so that the `SAFETY`
/// comments below can name it rather than restate it:
///
/// > **B1.** The block loop runs only while `i + 8 <= a.len()` and
/// > `j + 8 <= b.len()`, so every 128-bit load and every `get_unchecked(_ + 7)`
/// > is inside its slice.
///
/// > **B2.** `out` has capacity for `min(a.len(), b.len()) + 8` and `k`, the
/// > count written so far, never exceeds `min(a.len(), b.len())` — it counts
/// > matched elements, and a match consumes one element of each side. So the
/// > 16-byte compaction store at `out[k]` is inside the allocation.
///
/// Both are asserted with `debug_assert!` **inside** the loop rather than argued
/// only in prose, which is what lets a property test fail on a violated bound
/// instead of merely on a wrong answer. `cargo test` builds without
/// optimizations, so those assertions are live wherever the property runs.
/// `pub(crate)` for one item only: [`ops::mixed`](crate::ops::mixed)'s
/// array x run compaction needs [`SHUFFLE`](simd::SHUFFLE). Sharing the table
/// rather than duplicating it is a size decision, not a style one — it is 4 KiB
/// of static, and two copies would also be two things to keep in step with
/// `SIMD_BLOCK`. Nothing else here is for outside use.
#[cfg(target_arch = "aarch64")]
pub(crate) mod simd {
    use super::SIMD_BLOCK;
    use core::arch::aarch64::*;

    /// Lane `k` of the result is `0xFFFF` iff `va[k]` occurs anywhere in `vb`.
    ///
    /// Seven rotations of `vb` plus eight compares is the whole 8×8 all-pairs
    /// product. There is no `pcmpestrm` on AArch64 and no cheaper formulation:
    /// 64 comparisons in 8 SIMD compares is already the minimum at this width.
    ///
    /// # Safety
    ///
    /// Requires `neon`, which the caller establishes.
    #[inline]
    #[target_feature(enable = "neon")]
    unsafe fn match_mask(va: uint16x8_t, vb: uint16x8_t) -> uint16x8_t {
        // No `unsafe` block, and that is not an oversight: every operation here
        // is register-to-register and touches no memory, so each intrinsic's
        // only precondition is the `neon` feature — which `#[target_feature]`
        // establishes for this body, making the calls safe ones.
        let m0 = vceqq_u16(va, vb);
        let m1 = vceqq_u16(va, vextq_u16::<1>(vb, vb));
        let m2 = vceqq_u16(va, vextq_u16::<2>(vb, vb));
        let m3 = vceqq_u16(va, vextq_u16::<3>(vb, vb));
        let m4 = vceqq_u16(va, vextq_u16::<4>(vb, vb));
        let m5 = vceqq_u16(va, vextq_u16::<5>(vb, vb));
        let m6 = vceqq_u16(va, vextq_u16::<6>(vb, vb));
        let m7 = vceqq_u16(va, vextq_u16::<7>(vb, vb));
        let a01 = vorrq_u16(m0, m1);
        let a23 = vorrq_u16(m2, m3);
        let a45 = vorrq_u16(m4, m5);
        let a67 = vorrq_u16(m6, m7);
        vorrq_u16(vorrq_u16(a01, a23), vorrq_u16(a45, a67))
    }

    /// `|a ∩ b|` over the merge shape.
    ///
    /// A matching lane is `0xFFFF`, i.e. `-1` read as `i16`, so `acc - mask`
    /// increments the lane's counter with a single instruction and no masking
    /// step. It cannot overflow: a strictly ascending `[u16]` holds at most
    /// 65536 elements, so the loop runs at most `(65536 + 65536) / 8 = 16384`
    /// times and a lane counts at most one per iteration.
    ///
    /// # Safety
    ///
    /// Requires `neon`, which the caller establishes.
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn merge_cardinality(a: &[u16], b: &[u16]) -> u32 {
        let (na, nb) = (a.len(), b.len());
        let (mut i, mut j, mut n) = (0usize, 0usize, 0u32);
        if na >= SIMD_BLOCK && nb >= SIMD_BLOCK {
            let (ia, ib) = (na - SIMD_BLOCK + 1, nb - SIMD_BLOCK + 1);
            // SAFETY: B1. `i < ia == na - 7` gives `i + 8 <= na`, so the 8-lane
            // load at `a[i]` and the read of `a[i + 7]` are both in bounds; `j`
            // likewise. Neither index is advanced inside the body after the
            // reads, so the bound holds for the whole iteration.
            unsafe {
                let mut acc = vdupq_n_u16(0);
                while i < ia && j < ib {
                    debug_assert!(i + SIMD_BLOCK <= na && j + SIMD_BLOCK <= nb, "B1");
                    let va = vld1q_u16(a.as_ptr().add(i));
                    let vb = vld1q_u16(b.as_ptr().add(j));
                    acc = vsubq_u16(acc, match_mask(va, vb));
                    // Advance the side whose block ends lower; on a tie both,
                    // since the shared maximum has already been counted. This is
                    // a `csel` pair, not a branch, and that is the point.
                    //
                    // The tie is a **throughput** choice, not a correctness
                    // one: sabotaging `bmax <= amax` to `bmax < amax` leaves
                    // every test green, because the un-advanced side simply
                    // spends one extra iteration before it advances. Do not read
                    // the tests passing as evidence that this line is right.
                    let amax = *a.get_unchecked(i + SIMD_BLOCK - 1);
                    let bmax = *b.get_unchecked(j + SIMD_BLOCK - 1);
                    i += usize::from(amax <= bmax) * SIMD_BLOCK;
                    j += usize::from(bmax <= amax) * SIMD_BLOCK;
                }
                n = u32::from(vaddvq_u16(acc));
            }
        }
        // Whatever is left is shorter than a block on at least one side.
        n + super::scalar_merge_cardinality(&a[i..], &b[j..])
    }

    /// `a ∩ b = ∅` over the same merge shape, exiting at the first block that
    /// shares a value.
    ///
    /// # Why this exists rather than `merge_cardinality( .. ) == 0`
    ///
    /// The count and the predicate want the *same* `match_mask`; only what they
    /// do with it differs, and the difference is the whole point. The count
    /// accumulates and must therefore visit every block. This tests the mask and
    /// stops, so a pair that shares an early value never reads the rest.
    ///
    /// **The exit is per block, not per element** — coarser than the scalar
    /// merge it replaces, and deliberately so. That is the same trade
    /// `ops::bitmap`'s `PREDICATE_BLOCK` and `ops::mixed`'s `INTERSECT_BLOCK`
    /// make, for the same reason: a short-circuiting reduction cannot be
    /// widened, so an exit that is coarse beats one that costs the vector arm.
    ///
    /// Landed 2026-09-07 because the scalar merge **lost to the count it is
    /// weaker than** — 1.08x to 1.91x on disjoint operands, violating the rule
    /// `ops::card` states in prose. See JOURNAL.
    ///
    /// # Safety
    ///
    /// Requires `neon`, which the caller establishes.
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn is_disjoint(a: &[u16], b: &[u16]) -> bool {
        let (na, nb) = (a.len(), b.len());
        let (mut i, mut j) = (0usize, 0usize);
        if na >= SIMD_BLOCK && nb >= SIMD_BLOCK {
            let (ia, ib) = (na - SIMD_BLOCK + 1, nb - SIMD_BLOCK + 1);
            // SAFETY: B1, exactly as in `merge_cardinality`. `i < ia == na - 7`
            // gives `i + 8 <= na`, so the 8-lane load at `a[i]` and the read of
            // `a[i + 7]` are both in bounds; `j` likewise. Neither index is
            // advanced inside the body after the reads.
            unsafe {
                while i < ia && j < ib {
                    debug_assert!(i + SIMD_BLOCK <= na && j + SIMD_BLOCK <= nb, "B1");
                    let va = vld1q_u16(a.as_ptr().add(i));
                    let vb = vld1q_u16(b.as_ptr().add(j));
                    // A matching lane is `0xFFFF`, so the horizontal max is
                    // `0xFFFF` iff the two blocks share a value. Not
                    // `vaddvq_u16`: eight matching lanes sum past `u16` and the
                    // wrap would have to be argued rather than read.
                    if vmaxvq_u16(match_mask(va, vb)) != 0 {
                        return false;
                    }
                    // Same advance as the count, and it is safe for the same
                    // reason: `amax <= bmax` means every value in `a`'s block is
                    // at or below `bmax`, so nothing beyond `b`'s block can equal
                    // one of them. Dropping `a`'s block therefore drops no match.
                    let amax = *a.get_unchecked(i + SIMD_BLOCK - 1);
                    let bmax = *b.get_unchecked(j + SIMD_BLOCK - 1);
                    i += usize::from(amax <= bmax) * SIMD_BLOCK;
                    j += usize::from(bmax <= amax) * SIMD_BLOCK;
                }
            }
        }
        super::scalar_is_disjoint(&a[i..], &b[j..])
    }

    /// `b ⊆ a`, over the same block merge, deciding one block of `b` at a time.
    ///
    /// # Why this cannot be `is_disjoint` with the test inverted
    ///
    /// Containment is **asymmetric** — `b` drives, and probing `b` into `a` is
    /// the only legal direction — so a block of `b` is not decided by a single
    /// block of `a`. Its values may be spread across several, and a lane that
    /// matched two `a`-blocks ago is still found. So `found` **accumulates**
    /// across `a`-block advances and is reset only when `b` moves on. Testing
    /// one block against one block would report a value absent because it was
    /// looked for in the wrong place.
    ///
    /// A block of `b` is fully decided once `a`'s block reaches `b`'s maximum:
    /// every value in it is then at or below something `a` has been scanned
    /// through, so an unmatched lane is genuinely absent.
    ///
    /// # The invariant the scalar tail depends on
    ///
    /// `i` is advanced only when `a`'s block ends **strictly below** a value
    /// that bounds every `b` value still undecided. So `a[..i]` can never hold
    /// one of them, and handing `( &a[i..], &b[j..] )` to the scalar merge is
    /// sound. Advancing `i` anywhere else breaks that and the failure is
    /// silent — a `false` for a value that was present, in a block already
    /// passed.
    ///
    /// # Safety
    ///
    /// Requires `neon`, which the caller establishes.
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn contains_all(a: &[u16], b: &[u16]) -> bool {
        let (na, nb) = (a.len(), b.len());
        let (mut i, mut j) = (0usize, 0usize);
        if na >= SIMD_BLOCK && nb >= SIMD_BLOCK {
            let (ia, ib) = (na - SIMD_BLOCK + 1, nb - SIMD_BLOCK + 1);
            // SAFETY: B1. `i < ia == na - 7` gives `i + 8 <= na`, so the 8-lane
            // load at `a[i]` and the read of `a[i + 7]` are in bounds; `j`
            // likewise. Every advance below re-tests its bound before loading.
            unsafe {
                'blocks: while j < ib {
                    // Skip whole `a` blocks that end below this block's first
                    // value. This is what establishes `a[..i] < b[j]`.
                    while i < ia && *a.get_unchecked(i + SIMD_BLOCK - 1) < *b.get_unchecked(j) {
                        i += SIMD_BLOCK;
                    }
                    if i >= ia {
                        break;
                    }
                    let i_start = i;
                    let vb = vld1q_u16(b.as_ptr().add(j));
                    let bmax = *b.get_unchecked(j + SIMD_BLOCK - 1);
                    let mut found = vdupq_n_u16(0);
                    loop {
                        debug_assert!(i + SIMD_BLOCK <= na && j + SIMD_BLOCK <= nb, "B1");
                        let va = vld1q_u16(a.as_ptr().add(i));
                        found = vorrq_u16(found, match_mask(vb, va));
                        if *a.get_unchecked(i + SIMD_BLOCK - 1) >= bmax {
                            break;
                        }
                        i += SIMD_BLOCK;
                        if i >= ia {
                            // `b`'s block is still undecided, so rewind to the
                            // index whose invariant the tail needs.
                            i = i_start;
                            break 'blocks;
                        }
                    }
                    // A lane is `0xFFFF` when found, so the horizontal minimum
                    // is `0xFFFF` only when every lane was.
                    if vminvq_u16(found) != u16::MAX {
                        return false;
                    }
                    j += SIMD_BLOCK;
                }
            }
        }
        super::scalar_contains_all(&a[i..], &b[j..])
    }

    /// For each 8-bit lane mask, the byte indices that compact the matching
    /// `u16` lanes of `va` to the front of the register.
    ///
    /// `vqtbl1q_u8` yields zero for an out-of-range index, so the `0xFF` filler
    /// makes the unmatched tail deterministic garbage rather than undefined —
    /// and it is overwritten by the next block's store regardless.
    pub(crate) static SHUFFLE: [[u8; 16]; 256] = build_shuffle();

    const fn build_shuffle() -> [[u8; 16]; 256] {
        let mut t = [[0xFFu8; 16]; 256];
        let mut m = 0usize;
        while m < 256 {
            let (mut k, mut lane) = (0usize, 0usize);
            while lane < SIMD_BLOCK {
                if (m >> lane) & 1 == 1 {
                    t[m][2 * k] = (2 * lane) as u8;
                    t[m][2 * k + 1] = (2 * lane + 1) as u8;
                    k += 1;
                }
                lane += 1;
            }
            m += 1;
        }
        t
    }

    /// Narrow eight `0x0000`/`0xFFFF` lanes to one byte, one bit per lane.
    ///
    /// # Safety
    ///
    /// Requires `neon`, which the caller establishes.
    #[inline]
    #[target_feature(enable = "neon")]
    unsafe fn lane_bits(m: uint16x8_t) -> u8 {
        // SAFETY: `BITS` is a local `[u8; 8]` and `vld1_u8` reads exactly 8
        // bytes from it. The rest is register-to-register.
        unsafe {
            const BITS: [u8; 8] = [1, 2, 4, 8, 16, 32, 64, 128];
            vaddv_u8(vand_u8(vmovn_u16(m), vld1_u8(BITS.as_ptr())))
        }
    }

    /// `a ∩ b` over the merge shape, appended to `out`.
    ///
    /// # Safety
    ///
    /// Requires `neon`, and requires `out` to be empty with capacity at least
    /// `min(a.len(), b.len()) + SIMD_BLOCK` — bound **B2**.
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn merge_and_into(a: &[u16], b: &[u16], out: &mut Vec<u16>) {
        let (na, nb) = (a.len(), b.len());
        debug_assert!(out.is_empty());
        debug_assert!(out.capacity() >= na.min(nb) + SIMD_BLOCK, "B2");
        let (mut i, mut j, mut k) = (0usize, 0usize, 0usize);
        if na >= SIMD_BLOCK && nb >= SIMD_BLOCK {
            let (ia, ib) = (na - SIMD_BLOCK + 1, nb - SIMD_BLOCK + 1);
            let dst = out.as_mut_ptr();
            // SAFETY: B1 for the loads, exactly as in `merge_cardinality`. B2
            // for the store: `k` only ever grows by the number of matched lanes,
            // a match consumes one element from each side, so
            // `k <= min(na, nb)` and the 16-byte store at `dst.add(k)` stays
            // within the `min(na, nb) + 8` elements the caller reserved.
            unsafe {
                while i < ia && j < ib {
                    debug_assert!(i + SIMD_BLOCK <= na && j + SIMD_BLOCK <= nb, "B1");
                    debug_assert!(k + SIMD_BLOCK <= out.capacity(), "B2");
                    let va = vld1q_u16(a.as_ptr().add(i));
                    let vb = vld1q_u16(b.as_ptr().add(j));
                    let bits = lane_bits(match_mask(va, vb));
                    let shuf = vld1q_u8(SHUFFLE[bits as usize].as_ptr());
                    vst1q_u8(
                        dst.add(k).cast::<u8>(),
                        vqtbl1q_u8(vreinterpretq_u8_u16(va), shuf),
                    );
                    k += bits.count_ones() as usize;
                    let amax = *a.get_unchecked(i + SIMD_BLOCK - 1);
                    let bmax = *b.get_unchecked(j + SIMD_BLOCK - 1);
                    i += usize::from(amax <= bmax) * SIMD_BLOCK;
                    j += usize::from(bmax <= amax) * SIMD_BLOCK;
                }
                // SAFETY: every element of `out[..k]` was written by one of the
                // compaction stores above; the lanes past a store's match count
                // are overwritten by the following store or left outside `k`.
                out.set_len(k);
            }
        }
        // Whatever is left is shorter than a block on at least one side.
        super::scalar_merge_and_into(&a[i..], &b[j..], out);
    }
}

/// The vector merge if this build can reach one, the scalar merge otherwise.
///
/// Called **only** from the merge branch of [`and`]. The gallop branch keeps
/// the scalar probe: see the module header for why widening it loses.
#[inline]
fn merge_and(a: &[u16], b: &[u16]) -> Vec<u16> {
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("neon") {
        // `+ SIMD_BLOCK` is bound B2: the compaction store writes a whole
        // register whatever the match count, so the last block may write up to
        // seven elements past the true result length.
        let mut out = Vec::with_capacity(a.len().min(b.len()) + SIMD_BLOCK);
        // SAFETY: `neon` was just detected, and `out` is empty with the capacity
        // B2 requires.
        unsafe { simd::merge_and_into(a, b, &mut out) };
        return out;
    }
    let mut out = Vec::with_capacity(a.len().min(b.len()));
    scalar_merge_and_into(a, b, &mut out);
    out
}

/// The vector merge if this build can reach one, the scalar merge otherwise.
#[inline]
fn merge_cardinality(a: &[u16], b: &[u16]) -> u32 {
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("neon") {
        // SAFETY: `neon` was just detected.
        return unsafe { simd::merge_cardinality(a, b) };
    }
    scalar_merge_cardinality(a, b)
}

/// The two-pointer merge, kept reachable and correct as the oracle for the
/// vector arm ( QG §4.3 ). Do not delete it when the vector arm is faster.
fn scalar_merge_and_into(a: &[u16], b: &[u16], out: &mut Vec<u16>) {
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
}

/// The two-pointer containment merge. Oracle for [`simd::contains_all`], and the
/// tail its block loop cannot cover. Do not delete it when the vector arm is
/// faster ( QG §4.3 ).
fn scalar_contains_all(a: &[u16], b: &[u16]) -> bool {
    let (mut i, mut j) = (0usize, 0usize);
    while j < b.len() {
        if i >= a.len() {
            return false;
        }
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => return false,
            std::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
            }
        }
    }
    true
}

/// The two-pointer disjointness merge. Oracle for [`simd::is_disjoint`], and the
/// tail its block loop cannot cover. Do not delete it when the vector arm is
/// faster ( QG §4.3 ).
fn scalar_is_disjoint(a: &[u16], b: &[u16]) -> bool {
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => return false,
        }
    }
    true
}

/// The counting two-pointer merge. Oracle for [`simd::merge_cardinality`].
fn scalar_merge_cardinality(a: &[u16], b: &[u16]) -> u32 {
    let (mut i, mut j, mut n) = (0usize, 0usize, 0u32);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
                n += 1;
            }
        }
    }
    n
}

fn and(a: &[u16], b: &[u16]) -> Vec<u16> {
    // Skewed sizes: probe the small side into the large one.
    let (small, large, _) = if a.len() <= b.len() {
        (a, b, false)
    } else {
        (b, a, true)
    };
    if !small.is_empty() && large.len() / small.len() >= GALLOP_RATIO {
        let mut out = Vec::with_capacity(small.len());
        let mut j = 0usize;
        for &v in small {
            j = gallop(large, j, v);
            if j >= large.len() {
                break;
            }
            if large[j] == v {
                out.push(v);
                j += 1;
            }
        }
        return out;
    }
    merge_and(a, b)
}

/// `|a ∩ b|` without building the intersection.
///
/// # Why it lives here rather than in `card.rs`
///
/// It makes the **same shape decision** as [`and`] — gallop the small side into
/// the large past [`GALLOP_RATIO`], merge otherwise — and it uses the same
/// [`gallop`]. A cardinality path that picked a different algorithm from the
/// materializing one would be a second implementation whose divergence nothing
/// reports, since both still return the right number.
/// `cardinality_identities_agree_for_every_kind_pair` pins the *answer*; keeping
/// the two side by side is what keeps the *cost* honest too. Since 2026-08-27
/// they share the vector merge as well, through [`merge_cardinality`] and
/// [`merge_and`], so the two cannot drift apart in kernel either.
///
/// Until 2026-08-26 there was no array × array arm at all and this pair fell
/// through to the generic `Peekable<ContainerIter>` merge at the bottom of
/// `card::and_cardinality`, which matches on the container kind once per element
/// on **both** sides. That is a flat 2.5x over the same algorithm on `&[u16]`,
/// from m = 32 to m = 4096 — on what is the most common shape in a sparse
/// posting-list workload. Measured by `intersect_crossover` in
/// `benches/setops.rs`; the arms it does *not* have are as much a performance
/// property as the ones it does.
pub(crate) fn and_cardinality(a: &[u16], b: &[u16]) -> u32 {
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if !small.is_empty() && large.len() / small.len() >= GALLOP_RATIO {
        let (mut j, mut n) = (0usize, 0u32);
        for &v in small {
            j = gallop(large, j, v);
            if j >= large.len() {
                break;
            }
            if large[j] == v {
                n += 1;
                j += 1;
            }
        }
        return n;
    }
    merge_cardinality(a, b)
}

/// `a ∩ b = ∅`, stopping at the first block that shares a value.
///
/// Same gallop-vs-merge decision as [`and`], for the reason given on
/// [`and_cardinality`]. The early exit is what distinguishes this from
/// `and_cardinality(..) == 0`: the *false* answer must stay cheap, and it is the
/// only reason not to simply delegate.
///
/// **Until 2026-09-07 keeping that exit cost the vector arm, and the trade
/// was losing.** The merge here was scalar — a data-dependent `cmp` per element
/// — while the count it is weaker than reached `simd::merge_cardinality`, eight
/// lanes per iteration with a branchless advance. Measured on disjoint operands,
/// where neither can exit: **1.27x at m = 64, 1.91x at 256, 1.12x at 1024, 1.08x
/// at 4000**, the predicate losing every time. That is a direct violation of the
/// rule `ops::card` states in prose and `bitmap_predicate_vs_count` proves for
/// the bitmap arm.
///
/// Both are now vector, because the two want the *same* mask and differ only
/// in what they do with it: the count accumulates, the predicate tests and
/// stops. The exit is per block instead of per element — coarser, and the same
/// compromise `PREDICATE_BLOCK` and `INTERSECT_BLOCK` already make.
pub(crate) fn is_disjoint(a: &[u16], b: &[u16]) -> bool {
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if !small.is_empty() && large.len() / small.len() >= GALLOP_RATIO {
        let mut j = 0usize;
        for &v in small {
            j = gallop(large, j, v);
            if j >= large.len() {
                return true;
            }
            if large[j] == v {
                return false;
            }
        }
        return true;
    }
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("neon") {
        // SAFETY: `neon` was just detected.
        return unsafe { simd::is_disjoint(a, b) };
    }
    scalar_is_disjoint(a, b)
}

/// `b ⊆ a`, stopping at the first value of `b` that `a` lacks.
///
/// Asymmetric, unlike the other three: `b` drives, and the gallop test is
/// `a.len() / b.len()` rather than large-over-small, because there is no freedom
/// to swap the sides. Probing `b` into `a` is the only legal direction.
pub(crate) fn contains_all(a: &[u16], b: &[u16]) -> bool {
    if b.len() > a.len() {
        return false;
    }
    if !b.is_empty() && a.len() / b.len() >= GALLOP_RATIO {
        let mut j = 0usize;
        for &v in b {
            j = gallop(a, j, v);
            if j >= a.len() || a[j] != v {
                return false;
            }
            j += 1;
        }
        return true;
    }
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("neon") {
        // SAFETY: `neon` was just detected.
        return unsafe { simd::contains_all(a, b) };
    }
    scalar_contains_all(a, b)
}

fn or(a: &[u16], b: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(b[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

fn xor(a: &[u16], b: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(b[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

fn and_not(a: &[u16], b: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(a.len());
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::generic;

    const OPS: [SetOp; 4] = [SetOp::And, SetOp::Or, SetOp::Xor, SetOp::AndNot];

    fn arr(v: &[u16]) -> Container {
        Container::from_sorted(v)
    }

    // ---------------------------------------------------------------------
    // The vector arms, called directly.
    //
    // [`merge_and`] and [`merge_cardinality`] pick the vector arm through
    // `is_aarch64_feature_detected!`, and return the **scalar** merge when the
    // host reports `neon` absent. A test that reaches a kernel only through one
    // of those dispatchers therefore degrades to comparing the scalar merge
    // against itself on such a host — it stays green for a kernel it never
    // executed, which is coverage the build does not have.
    //
    // Measured, not argued: with both gates forced to `false` and
    // `simd::merge_cardinality` and `simd::merge_and_into` each sabotaged in
    // turn, every test in this module that goes through a dispatcher stayed
    // **green**. The wrappers below are what make it red. This is the same
    // property `ops::run`'s `vec_card` has, and for the same reason.
    // ---------------------------------------------------------------------

    /// The vector intersection, called directly.
    #[cfg(target_arch = "aarch64")]
    fn vec_and(a: &[u16], b: &[u16]) -> Vec<u16> {
        assert!(
            std::arch::is_aarch64_feature_detected!("neon"),
            "every aarch64 this crate targets has neon"
        );
        // `+ SIMD_BLOCK` is bound B2, reserved exactly as `merge_and` does.
        let mut out = Vec::with_capacity(a.len().min(b.len()) + SIMD_BLOCK);
        // SAFETY: `neon` was just detected, and `out` is empty with the capacity
        // B2 requires.
        unsafe { simd::merge_and_into(a, b, &mut out) };
        out
    }

    /// The vector cardinality, called directly.
    #[cfg(target_arch = "aarch64")]
    fn vec_cardinality(a: &[u16], b: &[u16]) -> u32 {
        assert!(
            std::arch::is_aarch64_feature_detected!("neon"),
            "every aarch64 this crate targets has neon"
        );
        // SAFETY: `neon` was just detected.
        unsafe { simd::merge_cardinality(a, b) }
    }

    /// The vector disjointness predicate, called directly.
    ///
    /// Direct rather than through [`is_disjoint`], and the assertion is the
    /// point: reached through the dispatcher on a host reporting `neon` absent,
    /// every comparison below would be `scalar == scalar` and the suite would
    /// report coverage this build does not have.
    #[cfg(target_arch = "aarch64")]
    fn vec_is_disjoint(a: &[u16], b: &[u16]) -> bool {
        assert!(
            std::arch::is_aarch64_feature_detected!("neon"),
            "every aarch64 this crate targets has neon"
        );
        // SAFETY: `neon` was just detected.
        unsafe { simd::is_disjoint(a, b) }
    }

    /// The vector disjointness arm against its scalar oracle, over the shapes
    /// that decide it.
    ///
    /// **Block boundaries are what this is for.** The vector arm advances a
    /// whole 8-lane block at a time and exits per block, so the cases that can
    /// break it are a shared value sitting at the first or last lane of a block,
    /// a shared value in the scalar tail the block loop never reaches, and
    /// operand lengths either side of `SIMD_BLOCK`.
    #[cfg(target_arch = "aarch64")]
    #[test]
    fn the_vector_disjointness_arm_agrees_with_the_scalar_oracle() {
        let mut cases: Vec<(Vec<u16>, Vec<u16>)> = vec![
            // Structurally disjoint, no exit anywhere: the shape the benchmark
            // measures and the one the whole change is about.
            (
                (0..4000u16).map(|i| i * 8).collect(),
                (0..4000u16).map(|i| 32768 + i * 8).collect(),
            ),
            // Shorter than one block on each side, so only the tail runs.
            (vec![1, 3, 5], vec![2, 4, 6]),
            (vec![1, 3, 5], vec![2, 3, 6]),
            // Exactly one block.
            ((0..8u16).collect(), (8..16u16).collect()),
            ((0..8u16).collect(), (7..15u16).collect()),
            // Empty sides.
            (vec![], vec![1, 2, 3]),
            (vec![1, 2, 3], vec![]),
        ];
        // **Asymmetric advance, and this family is not optional.** Every
        // case above has equal-length, evenly interleaved operands, so both
        // sides step in lockstep and a sabotage that advances *both* blocks
        // unconditionally coincides with the correct advance and survives —
        // verified, it did. What separates them is a pair where one side must
        // advance while the other holds: here `a`'s first block is entirely
        // below `b`'s, so only `a` may move, and the match lives in `a`'s
        // second block against `b`'s first. Advancing both walks `j` past the
        // end and reports disjoint.
        for blocks in 2..6usize {
            let n = (blocks * SIMD_BLOCK) as u16;
            let a: Vec<u16> = (0..n).collect();
            for k in 1..blocks {
                let lo = (k * SIMD_BLOCK) as u16;
                let b: Vec<u16> = (lo..lo + SIMD_BLOCK as u16).collect();
                cases.push((a.clone(), b));
            }
        }
        // A shared value walked across every position of a multi-block operand,
        // including both lanes of every block boundary and the scalar tail.
        let base: Vec<u16> = (0..40u16).map(|i| i * 4).collect();
        for k in 0..base.len() {
            let mut other: Vec<u16> = (0..40u16).map(|i| i * 4 + 2).collect();
            other[k] = base[k];
            other.sort_unstable();
            other.dedup();
            cases.push((base.clone(), other));
        }
        for (a, b) in &cases {
            let want = scalar_is_disjoint(a, b);
            assert_eq!(
                vec_is_disjoint(a, b),
                want,
                "vector arm disagrees on a={:?}.. b={:?}..",
                &a[..a.len().min(6)],
                &b[..b.len().min(6)]
            );
            // The dispatcher must agree too, or the arm is right and unreachable.
            assert_eq!(is_disjoint(a, b), want, "dispatcher disagrees");
            // And the predicate must agree with the count it is weaker than.
            assert_eq!(
                want,
                merge_cardinality(a, b) == 0,
                "disjointness disagrees with |a ∩ b| == 0"
            );
        }
    }

    /// The vector containment arm, called directly.
    #[cfg(target_arch = "aarch64")]
    fn vec_contains_all(a: &[u16], b: &[u16]) -> bool {
        assert!(
            std::arch::is_aarch64_feature_detected!("neon"),
            "every aarch64 this crate targets has neon"
        );
        // SAFETY: `neon` was just detected.
        unsafe { simd::contains_all(a, b) }
    }

    /// The vector containment arm against its scalar oracle.
    ///
    /// **Two shapes here can only fail one way — a present value reported
    /// absent — and neither is reachable by uniform operands.**
    ///
    /// 1. A block of `b` whose values are spread across several blocks of `a`.
    ///    `found` has to accumulate; testing one block against one block reports
    ///    the early lanes missing because it looked in the wrong place.
    /// 2. Exhausting `a`'s blocks mid-decision, so the scalar tail takes over
    ///    with a `b` block partly matched. If `i` is not rewound to a point
    ///    below every undecided `b` value, the tail searches `a` from past where
    ///    the match lives.
    #[cfg(target_arch = "aarch64")]
    #[test]
    fn the_vector_containment_arm_agrees_with_the_scalar_oracle() {
        let mut cases: Vec<(Vec<u16>, Vec<u16>)> = Vec::new();

        // ( 1 ) One `b` block drawn from many `a` blocks: `b` takes every 9th
        // value of a 200-element `a`, so consecutive `b` lanes sit in different
        // `a` blocks and a non-accumulating test fails on the first check.
        let wide: Vec<u16> = (0..200u16).map(|v| v * 3).collect();
        cases.push((wide.clone(), wide.iter().step_by(9).copied().collect()));
        cases.push((wide.clone(), wide.iter().step_by(17).copied().collect()));
        // The same, with one value removed so the answer is `false`.
        let mut miss: Vec<u16> = wide.iter().step_by(9).copied().collect();
        let dropped = miss[3];
        let holed: Vec<u16> = wide.iter().copied().filter(|v| *v != dropped).collect();
        cases.push((holed, miss.clone()));
        miss.clear();

        // ( 2 ) `b`'s last value beyond `a`'s range, forcing the tail with a
        // partly-matched block, and its subset counterpart.
        let short: Vec<u16> = (0..40u16).map(|v| v * 2).collect();
        let mut past: Vec<u16> = short.iter().step_by(3).copied().collect();
        past.push(60000);
        cases.push((short.clone(), past));
        cases.push((short.clone(), short.iter().step_by(3).copied().collect()));

        // Sizes either side of one block, empties, and equality.
        cases.push((vec![], vec![]));
        cases.push((vec![], vec![1]));
        cases.push((vec![1, 2, 3], vec![]));
        cases.push(((0..8u16).collect(), (0..8u16).collect()));
        cases.push(((0..8u16).collect(), vec![7]));
        cases.push(((0..9u16).collect(), vec![8]));
        cases.push(((0..64u16).collect(), (0..64u16).step_by(7).collect()));

        // A deterministic pseudo-random sweep: every subset shape at several
        // sizes, half of them holed so both answers are exercised.
        let mut seed = 0x9E3779B9u32;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for n in [9usize, 16, 33, 64, 129, 260] {
            for trial in 0..24 {
                let a: Vec<u16> = (0..n as u16).map(|v| v.wrapping_mul(5) / 2 + v).collect();
                let mut a: Vec<u16> = a;
                a.sort_unstable();
                a.dedup();
                let mut b: Vec<u16> = a.iter().copied().filter(|_| rnd() % 3 != 0).collect();
                if trial % 2 == 1 && !b.is_empty() && a.len() > 1 {
                    // Hole `a` at one of `b`'s values, so the answer is `false`.
                    let v = b[(rnd() as usize) % b.len()];
                    let a2: Vec<u16> = a.iter().copied().filter(|x| *x != v).collect();
                    cases.push((a2, b.clone()));
                }
                b.dedup();
                cases.push((a, b));
            }
        }

        for (a, b) in &cases {
            let want = scalar_contains_all(a, b);
            assert_eq!(
                vec_contains_all(a, b),
                want,
                "vector arm disagrees: |a|={} |b|={}",
                a.len(),
                b.len()
            );
            assert_eq!(contains_all(a, b), want, "dispatcher disagrees");
        }
    }

    #[test]
    fn every_arm_agrees_with_the_generic_kernel() {
        let cases: Vec<(Vec<u16>, Vec<u16>)> = vec![
            (
                (0..3000u16).map(|i| i * 3).collect(),
                (0..3000u16).map(|i| i * 5).collect(),
            ),
            (vec![1, 2, 3], vec![2, 3, 4]),
            (vec![], vec![1, 2, 3]),
            (vec![1, 2, 3], vec![]),
            ((0..100u16).collect(), (0..100u16).collect()),
            ((0..10u16).collect(), (0..4000u16).collect()), // skewed: gallops
            ((0..4000u16).collect(), (0..10u16).collect()), // skewed, other order
            (vec![0, 65535], vec![65535]),
        ];
        for (av, bv) in cases {
            let (a, b) = (arr(&av), arr(&bv));
            for op in OPS {
                let fast = try_apply(op, &a, &b).expect("both are arrays");
                let slow = generic::apply(op, &a, &b);
                let f: Vec<u16> = fast
                    .as_ref()
                    .map(|c| c.iter().collect())
                    .unwrap_or_default();
                let s: Vec<u16> = slow
                    .as_ref()
                    .map(|c| c.iter().collect())
                    .unwrap_or_default();
                assert_eq!(f, s, "{op:?} disagrees for {av:?} vs {bv:?}");
            }
        }
    }

    #[test]
    fn the_gallop_path_matches_the_merge_path() {
        // Same inputs, both strategies, must agree exactly.
        let small: Vec<u16> = (0..8u16).map(|i| i * 500).collect();
        let large: Vec<u16> = (0..4000u16).collect();
        assert!(
            large.len() / small.len() >= GALLOP_RATIO,
            "test must exercise gallop"
        );
        let galloped = and(&small, &large);
        // Force the merge path by comparing against the generic oracle.
        let oracle = generic::apply(SetOp::And, &arr(&small), &arr(&large));
        let o: Vec<u16> = oracle.map(|c| c.iter().collect()).unwrap_or_default();
        assert_eq!(galloped, o);
    }

    #[test]
    fn gallop_finds_the_first_element_at_or_after_the_target() {
        let s: Vec<u16> = (0..100u16).map(|i| i * 10).collect();
        assert_eq!(gallop(&s, 0, 0), 0);
        assert_eq!(gallop(&s, 0, 1), 1, "first >= 1 is index 1 (value 10)");
        assert_eq!(gallop(&s, 0, 500), 50);
        assert_eq!(gallop(&s, 0, 9999), s.len(), "past the end");
        // Starting past the target must not rewind.
        assert_eq!(gallop(&s, 60, 100), 60);
    }

    #[test]
    fn a_union_exceeding_the_array_bound_promotes() {
        let a: Vec<u16> = (0..3000u16).collect();
        let b: Vec<u16> = (3000..7000u16).collect();
        let r = try_apply(SetOp::Or, &arr(&a), &arr(&b)).unwrap().unwrap();
        assert_eq!(r.len(), 7000);
        assert_eq!(
            r.kind(),
            crate::ContainerKind::Bitmap,
            "7000 exceeds ARRAY_MAX"
        );
    }

    #[test]
    fn an_empty_result_is_none() {
        let a = arr(&[1, 2, 3]);
        assert_eq!(try_apply(SetOp::Xor, &a, &a), Some(None));
        assert_eq!(try_apply(SetOp::And, &a, &arr(&[9, 10])), Some(None));
    }

    /// Every length in `0..=24` against every other, so all three residue
    /// classes that matter — below one block, exactly one block, and a block
    /// plus a tail — appear on both sides in every combination.
    ///
    /// This is the test that a wrong loop bound fails. `merge_and` /
    /// `merge_cardinality` reach the vector arm for 289 of these pairs, and the
    /// `debug_assert!`s carrying **B1** and **B2** are live because `cargo test`
    /// builds without optimizations — so an off-by-one in `na - SIMD_BLOCK + 1`
    /// panics on the bound rather than merely returning a wrong number.
    #[test]
    fn the_merge_agrees_with_the_scalar_merge_at_every_block_boundary() {
        // Interleaved strides, so the intersection is neither empty nor total
        // and matches land at every lane position within a block.
        let mk = |n: usize, off: u16, step: u16| -> Vec<u16> {
            (0..n as u16).map(|i| off + i * step).collect::<Vec<u16>>()
        };
        let mut reached = 0usize;
        for la in 0..=24usize {
            for lb in 0..=24usize {
                for (oa, sa, ob, sb) in [(0u16, 1u16, 0u16, 1u16), (0, 2, 1, 2), (0, 3, 0, 2)] {
                    let (av, bv) = (mk(la, oa, sa), mk(lb, ob, sb));
                    let mut want = Vec::new();
                    scalar_merge_and_into(&av, &bv, &mut want);
                    assert_eq!(merge_and(&av, &bv), want, "and at {la}x{lb}");
                    assert_eq!(
                        merge_cardinality(&av, &bv),
                        scalar_merge_cardinality(&av, &bv),
                        "cardinality at {la}x{lb}"
                    );
                    // The same property held against the kernels themselves, so
                    // that it does not evaporate on a host reporting `neon`
                    // absent. See the note above `vec_and`.
                    #[cfg(target_arch = "aarch64")]
                    {
                        assert_eq!(vec_and(&av, &bv), want, "vector and at {la}x{lb}");
                        assert_eq!(
                            vec_cardinality(&av, &bv),
                            scalar_merge_cardinality(&av, &bv),
                            "vector cardinality at {la}x{lb}"
                        );
                    }
                    if la >= 8 && lb >= 8 {
                        reached += 1;
                    }
                }
            }
        }
        // Without this the property would pass vacuously on a build where the
        // vector arm is never entered, which is exactly how a specialization
        // stops being tested without anything going red.
        //
        // It counts the pairs that clear the *length* threshold, which is all
        // it can see: `merge_and` and `merge_cardinality` also consult
        // `is_aarch64_feature_detected!`, and a host reporting `neon` absent
        // would satisfy this assertion while running the scalar merge on both
        // sides of every comparison above. Only the `vec_*` calls close that,
        // which is why they are not redundant with these two.
        assert_eq!(reached, 289 * 3, "the vector arm must actually be reached");
    }

    /// The intersection of two full blocks, at every possible match count.
    ///
    /// The compaction store writes a whole register whatever the match count,
    /// so bound **B2** is tightest when `k` is largest. This walks `k` from 0 to
    /// 8 matches per block with the operands sized so `min(a, b)` is exactly the
    /// result length — the case where the reserved slack is entirely consumed.
    #[test]
    fn the_compaction_store_stays_inside_the_reservation() {
        for matches in 0..=8usize {
            let a: Vec<u16> = (0..8u16).collect();
            let mut b: Vec<u16> = (0..matches as u16).collect();
            b.extend((0..8 - matches as u16).map(|i| 1000 + i));
            b.sort_unstable();
            let mut want = Vec::new();
            scalar_merge_and_into(&a, &b, &mut want);
            assert_eq!(merge_and(&a, &b), want, "{matches} matches");
            // B2 is the compaction store's bound, so it is the kernel that must
            // be asked, not the dispatcher that may decline to call it.
            #[cfg(target_arch = "aarch64")]
            assert_eq!(vec_and(&a, &b), want, "{matches} matches, vector arm");
        }
    }

    proptest::proptest! {
        /// The vector arm against the scalar merge it replaced, on inputs shaped
        /// like real containers.
        ///
        /// Uniform random `u16` would make almost every intersection empty and
        /// the compaction path would never run with more than one lane set, so
        /// the strategy is boundary-biased in the sense `tests/proptest_oracle.rs`
        /// means it: strided operands that really do overlap, contiguous
        /// stretches where whole blocks match, and the ends of the chunk.
        #[test]
        fn the_vector_merge_agrees_with_the_scalar_merge(
            av in array_values(),
            bv in array_values(),
        ) {
            let mut want = Vec::new();
            scalar_merge_and_into(&av, &bv, &mut want);
            proptest::prop_assert_eq!(merge_and(&av, &bv), want.as_slice());
            proptest::prop_assert_eq!(
                merge_cardinality(&av, &bv),
                scalar_merge_cardinality(&av, &bv)
            );
            // And against the kernels directly, so the property survives a host
            // that reports `neon` absent. See the note above `vec_and`.
            #[cfg(target_arch = "aarch64")]
            {
                proptest::prop_assert_eq!(vec_and(&av, &bv), want.as_slice());
                proptest::prop_assert_eq!(
                    vec_cardinality(&av, &bv),
                    scalar_merge_cardinality(&av, &bv)
                );
            }
        }

        /// And the whole arm — gallop decision included — against the generic
        /// kernel, which stays the oracle ( QG §4.2 ).
        #[test]
        fn the_array_arm_agrees_with_the_generic_kernel(
            av in array_values(),
            bv in array_values(),
        ) {
            let (a, b) = (arr(&av), arr(&bv));
            for op in OPS {
                let fast = try_apply(op, &a, &b).expect("both are arrays");
                let slow = generic::apply(op, &a, &b);
                let f: Vec<u16> = fast.as_ref().map(|c| c.iter().collect()).unwrap_or_default();
                let s: Vec<u16> = slow.as_ref().map(|c| c.iter().collect()).unwrap_or_default();
                proptest::prop_assert_eq!(f, s, "{:?} disagrees", op);
            }
        }
    }

    fn array_values() -> impl proptest::strategy::Strategy<Value = Vec<u16>> {
        use proptest::prelude::*;
        prop_oneof![
            // Short, so every length residue mod the block width appears.
            3 => prop::collection::vec(any::<u16>(), 0..25),
            // Scattered over the chunk with a stride: the shape the corpus's
            // leading waste cell is made of, and the one the vector arm is for.
            3 => (200usize..1200, 1u16..64)
                .prop_map(|(n, k)| (0..n as u16).map(|i| i.wrapping_mul(k)).collect::<Vec<u16>>()),
            // One contiguous stretch, so whole blocks match and every lane of
            // the compaction shuffle is exercised.
            2 => (0u16..60000, 1u16..600)
                .prop_map(|(s, l)| (s..=s.saturating_add(l)).collect::<Vec<u16>>()),
            // The ends of the chunk and the first block, where an off-by-one in
            // the loop bound shows up.
            1 => prop::collection::vec(
                prop_oneof![
                    Just(0u16), Just(1), Just(7), Just(8), Just(9),
                    Just(65527), Just(65534), Just(65535)
                ],
                1..9,
            ),
        ]
        .prop_map(|mut v| {
            v.sort_unstable();
            v.dedup();
            v.truncate(ARRAY_MAX);
            v
        })
    }

    #[test]
    fn non_array_pairs_fall_through() {
        let bmp = Container::Bitmap(crate::container::BitmapContainer::from_sorted(
            &(0..5000u16).collect::<Vec<_>>(),
        ));
        assert!(try_apply(SetOp::And, &bmp, &bmp).is_none());
        assert!(try_apply(SetOp::And, &arr(&[1]), &bmp).is_none());
    }
}
