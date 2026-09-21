//! Specialized bitmap×bitmap kernels.
//!
//! The first arms to earn specialization. The M1 baseline measured dense binary
//! ops at 1.3-1.4x slower than the `roaring` crate, and the cause is structural:
//! [`super::generic`] merges *values*, so intersecting two dense containers
//! walks up to 131 072 individual `u16`s through a peekable iterator pair. The
//! same work as a word loop is 1024 iterations of `a & b`.
//!
//! # Fused popcount
//!
//! Each kernel counts set bits in the same pass that writes them. The cached
//! cardinality has to be exact ( the identities in [`super::card`] depend on it ),
//! and a second 8 KiB counting pass would give back much of what the word loop
//! wins.
//!
//! # What this module owns as of 2026-08-28
//!
//! The word loops are no longer written inline in [`try_apply`] and
//! [`super::card`]; they are five named kernels here, so that every caller gets
//! the same one and each has a scalar oracle beside it:
//!
//! * `apply_words` — the four binary ops with a fused popcount.
//! * `words_and_cardinality` — `|x ∩ y|`, no result.
//! * `words_and_cardinality_rows2` — two blocked-view filters over many
//!   fixed-width bitmap rows, with one architecture dispatch per container.
//! * `words_popcount` — set bits in one payload, for [`super::mixed`], whose
//!   interval-driven arms cannot fuse their own count.
//! * `words_disjoint` / `words_contains` — the two predicates.
//!
//! **LLVM had already vectorized three of the five, and the two it had not
//! were the two that mattered most.** `--emit asm` on the release lib
//! ( `aarch64-unknown-linux-gnu` ) showed `and_cardinality` and all four
//! `try_apply` arms getting a full `and`/`cnt`/`uaddlp`/`uzp1` ladder over 64
//! bytes per side per iteration — and `is_disjoint` / `contains_all` getting
//! **zero** vector instructions, because [`Iterator::all`] short-circuits and a
//! short-circuiting reduction cannot be widened. The arms that looked identical
//! in source were a factor of three apart in the object file. Do not read
//! "it is a word loop" as "it vectorizes".
//!
//! # The generic kernel remains the oracle
//!
//! Every arm here is differential-tested against `generic::apply` on the same
//! inputs. That is the whole reason specializing is safe: the slow path is not
//! dead code to be deleted once this exists, it is the reference implementation.

use crate::container::{BitmapContainer, Container};
use crate::ops::generic::SetOp;
use crate::BITMAP_WORDS;

/// Words tested between two checks of the running accumulator.
///
/// The two predicates below are the reason this constant exists, and it is a
/// compromise between two costs that pull in opposite directions — see
/// [`words_disjoint`].
const PREDICATE_BLOCK: usize = 16;

/// `∀ i. wx[i] & wy[i] == 0`, in blocks.
///
/// # Why this is not `zip(..).all(..)`
///
/// It was, and that cost 1.63x what counting the intersection costs.
/// [`Iterator::all`] short-circuits, so LLVM cannot widen it: the release
/// assembly for `wx.iter().zip(wy).all(|(p, q)| p & q == 0)` on
/// `aarch64-unknown-linux-gnu` contained **zero** vector instructions and walked
/// one 8-byte word per iteration, while `and_cardinality`'s bitmap arm — the
/// same traversal with the early exit removed — got a fully vectorized
/// `and`/`cnt`/`uaddlp` ladder over 64 bytes per side per iteration in the same
/// object file. Measured on two disjoint 8 KiB bitmaps: **272 ns for
/// `is_disjoint` against 167 ns for `and_cardinality(a, b) == 0`.**
///
/// That inverts the invariant `card.rs` records — a predicate that computes
/// strictly less than a count must not cost more than the count — and it is the
/// same failure the module found in 2026-08-26, in the same two functions, for
/// a different reason. It was invisible to the benchmark that existed:
/// `predicate_paths` names the pair and reports its cost, but nothing compared
/// that cost to `and_cardinality`'s, which is what the invariant is *about*.
///
/// # The block is what buys both properties
///
/// OR-accumulating a fixed 16 words has no loop-carried branch, so it widens —
/// the emitted loop is eight `and v.16b` and an `orr` tree over 128 bytes per
/// side — while checking the accumulator once per block keeps an early exit.
/// The price of the exit is at most one block of wasted work.
///
/// # The peeled first word is not redundant
///
/// Blocking alone made the *cheap* answer more expensive: two dense bitmaps
/// almost always meet in word 0, and paying a whole 16-word block to discover
/// it measured **3.85 ns against the scalar loop's 2.83 ns**. Testing word 0
/// before entering the block loop puts that back to **2.57 ns**, below where it
/// started, and costs the walking case nothing because in that case the branch
/// is perfectly predicted. Do not delete it as a special case of the loop
/// below; the loop does not have this property.
///
/// # What it is worth
///
/// On two 8 KiB bitmaps that really are disjoint, so no exit is possible
/// ( second of two consecutive runs ):
///
/// ```text
///                       before   after     x
///   is_disjoint  late   274.8    82.6    3.33
///   is_disjoint  early    2.83    2.57   1.10
///   contains_all late   274.6    83.1    3.31
///   contains_all early    1.93    1.79   1.08
///   and_cardinality      167.6    94.5    ( the bound, for scale )
/// ```
///
/// The predicates went from **1.64x the cost of the count they must not
/// exceed** to **0.87x of it**.
///
/// `wx` and `wy` are `BITMAP_WORDS` long at every call site, so the
/// `as_chunks` remainder is empty in practice. The tail is here because
/// nothing in the signature promises that, and a silently-skipped tail is a
/// wrong answer rather than a slow one.
#[inline]
pub(crate) fn words_disjoint(wx: &[u64], wy: &[u64]) -> bool {
    let n = wx.len().min(wy.len());
    let (wx, wy) = (&wx[..n], &wy[..n]);
    if let (Some(p), Some(q)) = (wx.first(), wy.first()) {
        if p & q != 0 {
            return false;
        }
    }
    let (cx, rx) = wx.as_chunks::<PREDICATE_BLOCK>();
    let (cy, ry) = wy.as_chunks::<PREDICATE_BLOCK>();
    for (bx, by) in cx.iter().zip(cy) {
        if bx.iter().zip(by).fold(0u64, |a, (p, q)| a | (p & q)) != 0 {
            return false;
        }
    }
    rx.iter().zip(ry).all(|(p, q)| p & q == 0)
}

/// `∀ i. wy[i] & !wx[i] == 0`, i.e. the `wy` bitmap is contained in the `wx`
/// one.
///
/// The dual of [`words_disjoint`] and it carries the same history: the
/// short-circuiting `all` form measured **271 ns against `and_cardinality`'s
/// 168 ns** on an 8 KiB containment that holds. Asymmetric — the operands
/// cannot be swapped — which is why it is a second function and not a flag on
/// the first.
#[inline]
pub(crate) fn words_contains(wx: &[u64], wy: &[u64]) -> bool {
    let n = wx.len().min(wy.len());
    let (wx, wy) = (&wx[..n], &wy[..n]);
    // The peeled first word, for the reason given on `words_disjoint`.
    if let (Some(p), Some(q)) = (wx.first(), wy.first()) {
        if q & !p != 0 {
            return false;
        }
    }
    let (cx, rx) = wx.as_chunks::<PREDICATE_BLOCK>();
    let (cy, ry) = wy.as_chunks::<PREDICATE_BLOCK>();
    for (bx, by) in cx.iter().zip(cy) {
        if bx.iter().zip(by).fold(0u64, |a, (p, q)| a | (q & !p)) != 0 {
            return false;
        }
    }
    rx.iter().zip(ry).all(|(p, q)| q & !p == 0)
}

/// `|x ∩ y|` over two word-aligned bitmap payloads.
///
/// # LLVM already vectorizes this, and that is most of the story
///
/// `wx.iter().zip(wy).map(|(p, q)| (p & q).count_ones()).sum()` compiles on
/// `aarch64-unknown-linux-gnu` to a loop over 64 bytes per side per iteration:
/// four `and v.16b`, four `cnt v.16b`, then LLVM's canonical widening ladder of
/// twelve `uaddlp` and two `uzp1` to get u64 lanes it can add into a `4s`
/// accumulator. That is **24 vector ALU operations per eight word-pairs**, and
/// it is the reason this arm was already competitive.
///
/// What is left is the ladder itself. `cnt` yields per-byte counts, and
/// `uadalp` ( `vpadalq_u8` ) accumulates byte pairs straight into halfword lanes
/// in one instruction — so the widening does not need to happen every iteration
/// at all, only once at the end. That is **12** operations for the same eight
/// word-pairs.
///
/// The lane bound is what makes deferring the widening legal: a `u16` lane of
/// one accumulator gains at most 16 per iteration ( one `cnt` result, each byte
/// at most 8, two bytes folded per lane ), a container is [`BITMAP_WORDS`] =
/// 1024 words so the loop runs at most 128 times, and the pairwise sums that
/// combine the four accumulators reach at most 4096. The final horizontal sum
/// widens to `u32` before reducing because a full container holds 65 536 bits,
/// which does **not** fit a `u16`.
///
/// Measured on two 8 KiB bitmaps, `aarch64-unknown-linux-gnu`, Cortex-X925,
/// second of two consecutive runs: **167.6 ns -> 94.5 ns, 1.77x**, and
/// unchanged by operand density at either end ( `d = 2^-1` and `d = 2^-3` agree
/// to 0.1% ).
///
/// **Over 64 distinct pairs the ratio is 1.39x, not 1.77x, and that gap is
/// the finding.** A resident pair costs 92.5 ns per intersection where a
/// streamed one costs 126 ns; before this change the two were 167 and 175 ns,
/// i.e. 4% apart. Making the kernel 1.8x faster did not make the *query* 1.8x
/// faster — it moved the cost from the ALU to the 16 KiB fetch, which the
/// reused-operand benchmark cannot see. Speeding up a kernel raises the share
/// of the bill that memory pays.
///
/// # Against `arrow-buffer`, which is already a dependency
///
/// ARCHITECTURE's containment policy says to reuse Arrow where it is good, so
/// this had to be measured rather than assumed. Arrow offers no fused
/// binary-op-plus-popcount: `BooleanBuffer::from_bitwise_binary_op` builds the
/// result and `count_set_bits` then walks it a **second** time. Over 64
/// distinct pairs, in one build so the comparison is internal:
///
/// ```text
///   and_cardinality        126 ns/pair    arrow bit_chunks + popcount  518
///   apply(And) + count     166 ns/pair    arrow from_bitwise_binary_op 373
/// ```
///
/// **4.1x and 2.2x.** The gap is a pass over 8 KiB plus, in Arrow's counting
/// path, a `BitChunks` iterator that carries bit-offset handling this crate's
/// payloads never need.
#[inline]
pub(crate) fn words_and_cardinality(wx: &[u64], wy: &[u64]) -> u32 {
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("neon") {
        // SAFETY: `neon` was just detected.
        return unsafe { simd::and_cardinality(wx, wy) };
    }
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: `avx2` was just detected.
        return unsafe { simd::and_cardinality(wx, wy) };
    }
    scalar_and_cardinality(wx, wy)
}

/// The word loop, kept reachable and correct as the oracle for the vector arm
/// ( QG §4.3 ). Do not delete it when the vector arm is faster.
fn scalar_and_cardinality(wx: &[u64], wy: &[u64]) -> u32 {
    wx.iter().zip(wy).map(|(p, q)| (p & q).count_ones()).sum()
}

/// Add two intersection counts for every fixed-width row in `data`.
///
/// This is deliberately a two-query kernel. The packed-view batch evaluator
/// commonly asks for sibling facet planes, and pairing them lets the vector
/// arm load each data block once. More filters are handled as independent
/// pairs by the caller; an odd final filter keeps the ordinary scalar path.
/// Dispatch is outside the row loop, because a feature-gated call per row cost
/// more than the isolated SIMD win in the first prototype.
pub(crate) fn words_and_cardinality_rows2(
    data: &[u64],
    q0: &[u64],
    q1: &[u64],
    row_words: usize,
    out0: &mut [u64],
    out1: &mut [u64],
) {
    assert!(row_words != 0, "bitmap row width must be non-zero");
    assert_eq!(out0.len(), out1.len(), "paired row outputs must align");
    assert!(q0.len() >= row_words && q1.len() >= row_words);
    let needed = row_words
        .checked_mul(out0.len())
        .expect("bitmap row span overflows usize");
    assert!(data.len() >= needed);
    let data = &data[..needed];

    #[cfg(target_arch = "aarch64")]
    if row_words >= 8 && std::arch::is_aarch64_feature_detected!("neon") {
        // SAFETY: `neon` was just detected. The assertions above establish
        // bound B8: every output row has `row_words` data and query words.
        unsafe { simd::and_cardinality_rows2(data, q0, q1, row_words, out0, out1) };
        return;
    }
    #[cfg(target_arch = "x86_64")]
    if row_words >= 16 && std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: `avx2` was just detected and bound B8 was checked above.
        unsafe { simd::and_cardinality_rows2(data, q0, q1, row_words, out0, out1) };
        return;
    }
    scalar_and_cardinality_rows2(data, q0, q1, row_words, out0, out1);
}

/// Scalar oracle for [`words_and_cardinality_rows2`].
fn scalar_and_cardinality_rows2(
    data: &[u64],
    q0: &[u64],
    q1: &[u64],
    row_words: usize,
    out0: &mut [u64],
    out1: &mut [u64],
) {
    for (row, words) in data.chunks_exact(row_words).take(out0.len()).enumerate() {
        out0[row] += u64::from(scalar_and_cardinality(words, q0));
        out1[row] += u64::from(scalar_and_cardinality(words, q1));
    }
}

/// Set bits in a payload, as one pass.
///
/// The unary form of [`words_and_cardinality`], with the same accumulator and
/// the same lane bound. It exists for [`super::mixed`], whose four bitmap×run
/// arms build the result word by word and then count it in a **second** 8 KiB
/// pass — the thing this module's header says a fused popcount exists to avoid,
/// in a module that could not fuse it because the write is driven by intervals
/// rather than by words.
#[inline]
pub(crate) fn words_popcount(w: &[u64]) -> u32 {
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("neon") {
        // SAFETY: `neon` was just detected.
        return unsafe { simd::popcount(w) };
    }
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: `avx2` was just detected.
        return unsafe { simd::popcount(w) };
    }
    scalar_popcount(w)
}

/// Oracle for [`words_popcount`] ( QG §4.3 ).
fn scalar_popcount(w: &[u64]) -> u32 {
    w.iter().map(|x| x.count_ones()).sum()
}

/// The NEON popcount arm.
///
/// One shared bound, stated once so the `SAFETY` comments can name it:
///
/// > **B3.** The block loop runs only while `i + WORDS_PER_BLOCK <= n` where
/// > `n = min(wx.len(), wy.len())`, so all four 16-byte loads from each side —
/// > covering bytes `[8i, 8i + 64)` — are inside both slices.
///
/// It is asserted with `debug_assert!` inside the loop rather than argued only
/// in prose, so a property test fails on a violated bound rather than merely on
/// a wrong answer; `cargo test` builds without optimizations, so the assertion
/// is live wherever the property runs.
#[cfg(target_arch = "aarch64")]
mod simd {
    use core::arch::aarch64::*;

    /// `u64` words consumed per iteration: four 128-bit loads per side.
    const WORDS_PER_BLOCK: usize = 8;

    /// # Safety
    ///
    /// Requires `neon`, which the caller establishes.
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn and_cardinality(wx: &[u64], wy: &[u64]) -> u32 {
        let n = wx.len().min(wy.len());
        let mut i = 0usize;
        let total;
        // SAFETY: B3. `i + 8 <= n <= wx.len()` and likewise for `wy`, so the
        // four `vld1q_u8` per side read bytes `[8i, 8i + 64)`, which is inside
        // `8 * n` bytes of each slice. `i` is not advanced inside the body
        // before the reads.
        unsafe {
            // Four independent accumulators, not one. `uadalp` is
            // read-modify-write on its destination, so a single `acc` makes the
            // four of them a serial dependency chain and the loop runs at the
            // instruction's *latency* rather than its throughput — measured 41%
            // **slower** than LLVM's ladder before this was split.
            let mut a0 = vdupq_n_u16(0);
            let mut a1 = vdupq_n_u16(0);
            let mut a2 = vdupq_n_u16(0);
            let mut a3 = vdupq_n_u16(0);
            while i + WORDS_PER_BLOCK <= n {
                debug_assert!(i + WORDS_PER_BLOCK <= wx.len(), "B3");
                debug_assert!(i + WORDS_PER_BLOCK <= wy.len(), "B3");
                let px = wx.as_ptr().add(i).cast::<u8>();
                let py = wy.as_ptr().add(i).cast::<u8>();
                a0 = vpadalq_u8(a0, vcntq_u8(vandq_u8(vld1q_u8(px), vld1q_u8(py))));
                a1 = vpadalq_u8(
                    a1,
                    vcntq_u8(vandq_u8(vld1q_u8(px.add(16)), vld1q_u8(py.add(16)))),
                );
                a2 = vpadalq_u8(
                    a2,
                    vcntq_u8(vandq_u8(vld1q_u8(px.add(32)), vld1q_u8(py.add(32)))),
                );
                a3 = vpadalq_u8(
                    a3,
                    vcntq_u8(vandq_u8(vld1q_u8(px.add(48)), vld1q_u8(py.add(48)))),
                );
                i += WORDS_PER_BLOCK;
            }
            // Widen before the horizontal add: the total can be 65 536, which
            // `vaddvq_u16` would truncate. Each lane holds at most 8192 ( bound
            // B3's iteration count times 16 ), so the `u16` adds cannot wrap.
            let s0 = vaddq_u16(a0, a1);
            let s1 = vaddq_u16(a2, a3);
            total = vaddvq_u32(vaddq_u32(vpaddlq_u16(s0), vpaddlq_u16(s1)));
        }
        total + super::scalar_and_cardinality(&wx[i..n], &wy[i..n])
    }

    /// Two AND-popcounts for every fixed-width row, loading `data` once.
    ///
    /// # Safety
    ///
    /// Requires `neon` and bound **B8**: `row_words >= 8`, both queries have
    /// at least `row_words` words, `data` has `row_words * out0.len()` words,
    /// and both output slices have the same length.
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn and_cardinality_rows2(
        data: &[u64],
        q0: &[u64],
        q1: &[u64],
        row_words: usize,
        out0: &mut [u64],
        out1: &mut [u64],
    ) {
        debug_assert!(row_words >= WORDS_PER_BLOCK, "B8");
        debug_assert!(q0.len() >= row_words && q1.len() >= row_words, "B8");
        debug_assert_eq!(out0.len(), out1.len(), "B8");
        debug_assert!(data.len() >= row_words * out0.len(), "B8");
        for row in 0..out0.len() {
            let words = &data[row * row_words..(row + 1) * row_words];
            let mut i = 0usize;
            let (mut x0, mut x1, mut x2, mut x3);
            let (mut y0, mut y1, mut y2, mut y3);
            // SAFETY: B8 and `i + 8 <= row_words` keep every four-vector
            // load within this row and both queries. The data vectors are
            // shared by the two filters before the loop advances.
            unsafe {
                x0 = vdupq_n_u16(0);
                x1 = vdupq_n_u16(0);
                x2 = vdupq_n_u16(0);
                x3 = vdupq_n_u16(0);
                y0 = vdupq_n_u16(0);
                y1 = vdupq_n_u16(0);
                y2 = vdupq_n_u16(0);
                y3 = vdupq_n_u16(0);
                while i + WORDS_PER_BLOCK <= row_words {
                    debug_assert!(i + WORDS_PER_BLOCK <= words.len(), "B8");
                    let pd = words.as_ptr().add(i).cast::<u8>();
                    let p0 = q0.as_ptr().add(i).cast::<u8>();
                    let p1 = q1.as_ptr().add(i).cast::<u8>();
                    let d0 = vld1q_u8(pd);
                    let d1 = vld1q_u8(pd.add(16));
                    let d2 = vld1q_u8(pd.add(32));
                    let d3 = vld1q_u8(pd.add(48));
                    x0 = vpadalq_u8(x0, vcntq_u8(vandq_u8(d0, vld1q_u8(p0))));
                    x1 = vpadalq_u8(x1, vcntq_u8(vandq_u8(d1, vld1q_u8(p0.add(16)))));
                    x2 = vpadalq_u8(x2, vcntq_u8(vandq_u8(d2, vld1q_u8(p0.add(32)))));
                    x3 = vpadalq_u8(x3, vcntq_u8(vandq_u8(d3, vld1q_u8(p0.add(48)))));
                    y0 = vpadalq_u8(y0, vcntq_u8(vandq_u8(d0, vld1q_u8(p1))));
                    y1 = vpadalq_u8(y1, vcntq_u8(vandq_u8(d1, vld1q_u8(p1.add(16)))));
                    y2 = vpadalq_u8(y2, vcntq_u8(vandq_u8(d2, vld1q_u8(p1.add(32)))));
                    y3 = vpadalq_u8(y3, vcntq_u8(vandq_u8(d3, vld1q_u8(p1.add(48)))));
                    i += WORDS_PER_BLOCK;
                }
                out0[row] += u64::from(vaddvq_u32(vaddq_u32(
                    vpaddlq_u16(vaddq_u16(x0, x1)),
                    vpaddlq_u16(vaddq_u16(x2, x3)),
                )));
                out1[row] += u64::from(vaddvq_u32(vaddq_u32(
                    vpaddlq_u16(vaddq_u16(y0, y1)),
                    vpaddlq_u16(vaddq_u16(y2, y3)),
                )));
            }
            out0[row] += u64::from(super::scalar_and_cardinality(
                &words[i..],
                &q0[i..row_words],
            ));
            out1[row] += u64::from(super::scalar_and_cardinality(
                &words[i..],
                &q1[i..row_words],
            ));
        }
    }

    /// # Safety
    ///
    /// Requires `neon`, which the caller establishes.
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn popcount(w: &[u64]) -> u32 {
        let n = w.len();
        let mut i = 0usize;
        let total;
        // SAFETY: B3, with one slice instead of two: `i + 8 <= n == w.len()`,
        // so the four `vld1q_u8` read bytes `[8i, 8i + 64)`, inside `8 * n`.
        unsafe {
            let mut a0 = vdupq_n_u16(0);
            let mut a1 = vdupq_n_u16(0);
            let mut a2 = vdupq_n_u16(0);
            let mut a3 = vdupq_n_u16(0);
            while i + WORDS_PER_BLOCK <= n {
                debug_assert!(i + WORDS_PER_BLOCK <= w.len(), "B3");
                let p = w.as_ptr().add(i).cast::<u8>();
                a0 = vpadalq_u8(a0, vcntq_u8(vld1q_u8(p)));
                a1 = vpadalq_u8(a1, vcntq_u8(vld1q_u8(p.add(16))));
                a2 = vpadalq_u8(a2, vcntq_u8(vld1q_u8(p.add(32))));
                a3 = vpadalq_u8(a3, vcntq_u8(vld1q_u8(p.add(48))));
                i += WORDS_PER_BLOCK;
            }
            let s0 = vaddq_u16(a0, a1);
            let s1 = vaddq_u16(a2, a3);
            total = vaddvq_u32(vaddq_u32(vpaddlq_u16(s0), vpaddlq_u16(s1)));
        }
        total + super::scalar_popcount(&w[i..])
    }

    /// `out[i] = wx[i] op wy[i]` for `i < BITMAP_WORDS`, returning the popcount
    /// of what was written.
    ///
    /// The same four-accumulator popcount as [`and_cardinality`], with the
    /// combined word stored on the way past. `OP` is a const parameter rather
    /// than a `SetOp` argument so the match folds away and the loop body is one
    /// vector operation wide again; passing it dynamically would put a branch
    /// inside the loop.
    ///
    /// # Safety
    ///
    /// Requires `neon`, which the caller establishes, and — bound **B4** —
    /// requires `wx` and `wy` to be at least [`super::BITMAP_WORDS`] long and
    /// `out` to be empty with at least that capacity. On return `out` has
    /// exactly that length and every element has been written.
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn apply_into<const OP: u8>(
        wx: &[u64],
        wy: &[u64],
        out: &mut Vec<u64>,
    ) -> u32 {
        const N: usize = super::BITMAP_WORDS;
        debug_assert!(wx.len() >= N && wy.len() >= N, "B4");
        debug_assert!(out.is_empty() && out.capacity() >= N, "B4");
        let total;
        // SAFETY: B4. Every load reads bytes `[8i, 8i + 64)` of `wx` / `wy` for
        // `i + 8 <= N <= len`, and every store writes the same range of `out`,
        // whose capacity is at least `N` words. The loop covers `0..N` exactly,
        // so `set_len(N)` leaves no element uninitialized.
        unsafe {
            let dst = out.as_mut_ptr().cast::<u8>();
            let mut a0 = vdupq_n_u16(0);
            let mut a1 = vdupq_n_u16(0);
            let mut a2 = vdupq_n_u16(0);
            let mut a3 = vdupq_n_u16(0);
            let mut i = 0usize;
            while i + WORDS_PER_BLOCK <= N {
                debug_assert!(i + WORDS_PER_BLOCK <= wx.len(), "B4");
                debug_assert!(i + WORDS_PER_BLOCK <= wy.len(), "B4");
                debug_assert!(i + WORDS_PER_BLOCK <= out.capacity(), "B4");
                let px = wx.as_ptr().add(i).cast::<u8>();
                let py = wy.as_ptr().add(i).cast::<u8>();
                let po = dst.add(i * 8);
                let w0 = combine::<OP>(vld1q_u8(px), vld1q_u8(py));
                let w1 = combine::<OP>(vld1q_u8(px.add(16)), vld1q_u8(py.add(16)));
                let w2 = combine::<OP>(vld1q_u8(px.add(32)), vld1q_u8(py.add(32)));
                let w3 = combine::<OP>(vld1q_u8(px.add(48)), vld1q_u8(py.add(48)));
                vst1q_u8(po, w0);
                vst1q_u8(po.add(16), w1);
                vst1q_u8(po.add(32), w2);
                vst1q_u8(po.add(48), w3);
                // Four accumulators is a **throughput** choice, not a
                // correctness one: folding all four `vpadalq_u8` into `a0`
                // leaves every test green, because the sum is the same and the
                // lane bound only gets looser. It measured 41% slower in
                // `and_cardinality`, where the same collapse was tried by
                // accident. Do not read the tests passing as evidence that
                // these four lines may be merged.
                a0 = vpadalq_u8(a0, vcntq_u8(w0));
                a1 = vpadalq_u8(a1, vcntq_u8(w1));
                a2 = vpadalq_u8(a2, vcntq_u8(w2));
                a3 = vpadalq_u8(a3, vcntq_u8(w3));
                i += WORDS_PER_BLOCK;
            }
            debug_assert_eq!(i, N, "the block loop must cover the payload exactly");
            let s0 = vaddq_u16(a0, a1);
            let s1 = vaddq_u16(a2, a3);
            total = vaddvq_u32(vaddq_u32(vpaddlq_u16(s0), vpaddlq_u16(s1)));
            out.set_len(N);
        }
        total
    }

    /// The one lane-wise operation, selected at compile time.
    ///
    /// `vbicq_u8(a, b)` is `a & !b`, which is `AndNot` in the operand order
    /// this crate uses. Reversing it silently computes `b \ a`, and `AndNot` is
    /// the one op whose pair order the dispatcher does not normalize.
    ///
    /// # Safety
    ///
    /// Requires `neon`, which the caller establishes.
    #[inline]
    #[target_feature(enable = "neon")]
    unsafe fn combine<const OP: u8>(x: uint8x16_t, y: uint8x16_t) -> uint8x16_t {
        // Register-to-register only, so `neon` is each intrinsic's whole
        // precondition and `#[target_feature]` discharges it.
        match OP {
            OP_AND => vandq_u8(x, y),
            OP_OR => vorrq_u8(x, y),
            OP_XOR => veorq_u8(x, y),
            _ => vbicq_u8(x, y),
        }
    }

    pub(super) const OP_AND: u8 = 0;
    pub(super) const OP_OR: u8 = 1;
    pub(super) const OP_XOR: u8 = 2;
    pub(super) const OP_ANDNOT: u8 = 3;
}

/// The AVX2 arm.
///
/// # Why this exists on x86 at all
///
/// The NEON arm exists because aarch64 has **no scalar popcount** -- there,
/// `u64::count_ones()` lowers to `fmov` + `cnt` + `addv` per word. x86 has a
/// single-cycle `popcnt`, so the obvious prediction is that the scalar path
/// already wins here and no arm is warranted.
///
/// **Measured on an Intel i9-9880H 2026-09-18, and the prediction was wrong.**
/// Isolated kernels, 1024 words, against the same loops without the arm:
///
/// ```text
///   popcount          2.70x
///   and_cardinality   2.22x
///   and_into          2.05x
/// ```
///
/// A nibble-shuffle popcount consumes 32 bytes per instruction while `popcnt`
/// consumes 8, and the `psadbw` accumulation is nearly free, so the wider path
/// wins despite the scalar instruction being good. Recorded because the
/// reasoning that predicted otherwise is sound and still wrong.
///
/// **Crate-level, aarch64, measured 2026-09-18 after two false starts:
/// `and_cardinality` on two bitmap containers is 103.5 ns with this arm and
/// 167.5 ns without -- `1.62x`.** Six repetitions per side, spread under 0.5%,
/// and the figure matches the isolated kernel ( 1.62x popcount, 1.73x
/// and_cardinality ) rather than sitting below it, because this operation is
/// almost entirely the kernel. `array x run` gains `1.27x` from the same arm.
///
/// # What the scalar side actually is
///
/// `w.iter().map( |x| x.count_ones() ).sum()` is **auto-vectorized by LLVM** on
/// aarch64. So 1.62x is this arm beating compiler-generated NEON, not a naive
/// byte loop -- which is the comparison that matters and the reason the arm
/// earns its `unsafe`.
///
/// # Two figures were published here and withdrawn; the harness was the fault
///
/// **2.51x** came from a probe whose timing loop black-boxed the *result* and
/// not the operands, leaving the call loop-invariant, pure and hoistable -- so
/// whether it was timed at all moved with inlining in *unrelated* modules. The
/// tell was on screen and missed: 96.6 ns for an AND-cardinality over 1024
/// words. **1.17x** then came from a fixed probe run at 20 000 iterations,
/// where this benchmark is bimodal ( ~102 ns or ~290 ns from the same binary );
/// the corrupted cell was the *baseline*, which made the arm look like a
/// pessimization. At 200 000 iterations it is flat to 0.5%.
///
/// The rule both failures produced, now in `QUALITY_GATE.md`: disable the
/// kernel, confirm the benchmark moves **and moves for that kernel alone**, and
/// treat an implausible number as a finding. See
/// `.agents/docs/LTM/simd-arch-arms-and-kernel-selection.md`.
///
/// # The accumulator split is load-bearing, exactly as on NEON
///
/// The NEON arm records that a *single* accumulator makes `uadalp`'s
/// read-modify-write a serial dependency chain, running the loop at latency
/// rather than throughput -- 41% slower than LLVM's own ladder. The same is
/// true of `_mm256_add_epi64` here. Four independent accumulators, not one.
#[cfg(target_arch = "x86_64")]
mod simd {
    use core::arch::x86_64::*;

    /// `u64` words consumed per iteration: four 256-bit loads per side.
    const WORDS_PER_BLOCK: usize = 16;

    /// The nibble-count table, duplicated across both 128-bit lanes because
    /// `pshufb` shuffles within lanes.
    ///
    /// # Safety
    ///
    /// Requires `avx2`, which the caller establishes.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn nibble_lut() -> __m256i {
        _mm256_setr_epi8(
            0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3, 3, 4, 0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2,
            3, 3, 4,
        )
    }

    /// Byte-wise popcount of `v`, summed into 64-bit lanes by `psadbw`.
    ///
    /// # Safety
    ///
    /// Requires `avx2`, which the caller establishes.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn popcnt_bytes(v: __m256i, lut: __m256i, lo_mask: __m256i) -> __m256i {
        let lo = _mm256_and_si256(v, lo_mask);
        let hi = _mm256_and_si256(_mm256_srli_epi16::<4>(v), lo_mask);
        let cnt = _mm256_add_epi8(_mm256_shuffle_epi8(lut, lo), _mm256_shuffle_epi8(lut, hi));
        _mm256_sad_epu8(cnt, _mm256_setzero_si256())
    }

    /// Horizontal sum of four 64-bit lanes.
    ///
    /// # Safety
    ///
    /// Requires `avx2`, which the caller establishes.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn hsum_u64(v: __m256i) -> u64 {
        let lo = _mm256_castsi256_si128(v);
        let hi = _mm256_extracti128_si256::<1>(v);
        let s = _mm_add_epi64(lo, hi);
        let t = _mm_add_epi64(s, _mm_unpackhi_epi64(s, s));
        _mm_cvtsi128_si64(t) as u64
    }

    /// # Safety
    ///
    /// Requires `avx2`, which the caller establishes.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn and_cardinality(wx: &[u64], wy: &[u64]) -> u32 {
        let n = wx.len().min(wy.len());
        let mut i = 0usize;
        let total;
        // SAFETY: B3. `i + 16 <= n <= wx.len()` and likewise for `wy`, so the
        // four `_mm256_loadu_si256` per side read bytes `[8i, 8i + 128)`, which
        // is inside `8 * n` bytes of each slice. `i` is not advanced inside the
        // body before the reads.
        unsafe {
            let (lut, lo_mask) = (nibble_lut(), _mm256_set1_epi8(0x0f));
            let mut a0 = _mm256_setzero_si256();
            let mut a1 = _mm256_setzero_si256();
            let mut a2 = _mm256_setzero_si256();
            let mut a3 = _mm256_setzero_si256();
            while i + WORDS_PER_BLOCK <= n {
                debug_assert!(i + WORDS_PER_BLOCK <= wx.len(), "B3");
                debug_assert!(i + WORDS_PER_BLOCK <= wy.len(), "B3");
                let px = wx.as_ptr().add(i).cast::<__m256i>();
                let py = wy.as_ptr().add(i).cast::<__m256i>();
                let v0 = _mm256_and_si256(_mm256_loadu_si256(px), _mm256_loadu_si256(py));
                let v1 =
                    _mm256_and_si256(_mm256_loadu_si256(px.add(1)), _mm256_loadu_si256(py.add(1)));
                let v2 =
                    _mm256_and_si256(_mm256_loadu_si256(px.add(2)), _mm256_loadu_si256(py.add(2)));
                let v3 =
                    _mm256_and_si256(_mm256_loadu_si256(px.add(3)), _mm256_loadu_si256(py.add(3)));
                a0 = _mm256_add_epi64(a0, popcnt_bytes(v0, lut, lo_mask));
                a1 = _mm256_add_epi64(a1, popcnt_bytes(v1, lut, lo_mask));
                a2 = _mm256_add_epi64(a2, popcnt_bytes(v2, lut, lo_mask));
                a3 = _mm256_add_epi64(a3, popcnt_bytes(v3, lut, lo_mask));
                i += WORDS_PER_BLOCK;
            }
            let s = _mm256_add_epi64(_mm256_add_epi64(a0, a1), _mm256_add_epi64(a2, a3));
            // Each lane counts at most 8 bits per byte times the iteration
            // count, far inside `u64`; the total is at most 65 536.
            total = hsum_u64(s) as u32;
        }
        total + super::scalar_and_cardinality(&wx[i..n], &wy[i..n])
    }

    /// Two AND-popcounts for every fixed-width row, loading `data` once.
    ///
    /// # Safety
    ///
    /// Requires `avx2` and bound **B8**: `row_words >= 16`, both queries have
    /// at least `row_words` words, `data` has `row_words * out0.len()` words,
    /// and both output slices have the same length.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn and_cardinality_rows2(
        data: &[u64],
        q0: &[u64],
        q1: &[u64],
        row_words: usize,
        out0: &mut [u64],
        out1: &mut [u64],
    ) {
        debug_assert!(row_words >= WORDS_PER_BLOCK, "B8");
        debug_assert!(q0.len() >= row_words && q1.len() >= row_words, "B8");
        debug_assert_eq!(out0.len(), out1.len(), "B8");
        debug_assert!(data.len() >= row_words * out0.len(), "B8");
        // SAFETY: B8 and `i + 16 <= row_words` keep the four vector loads
        // within each row and both queries. AVX2 permits these unaligned loads.
        unsafe {
            let (lut, lo_mask) = (nibble_lut(), _mm256_set1_epi8(0x0f));
            for row in 0..out0.len() {
                let words = &data[row * row_words..(row + 1) * row_words];
                let mut i = 0usize;
                let mut x0 = _mm256_setzero_si256();
                let mut x1 = _mm256_setzero_si256();
                let mut x2 = _mm256_setzero_si256();
                let mut x3 = _mm256_setzero_si256();
                let mut y0 = _mm256_setzero_si256();
                let mut y1 = _mm256_setzero_si256();
                let mut y2 = _mm256_setzero_si256();
                let mut y3 = _mm256_setzero_si256();
                while i + WORDS_PER_BLOCK <= row_words {
                    debug_assert!(i + WORDS_PER_BLOCK <= words.len(), "B8");
                    let pd = words.as_ptr().add(i).cast::<__m256i>();
                    let p0 = q0.as_ptr().add(i).cast::<__m256i>();
                    let p1 = q1.as_ptr().add(i).cast::<__m256i>();
                    let d0 = _mm256_loadu_si256(pd);
                    let d1 = _mm256_loadu_si256(pd.add(1));
                    let d2 = _mm256_loadu_si256(pd.add(2));
                    let d3 = _mm256_loadu_si256(pd.add(3));
                    x0 = _mm256_add_epi64(
                        x0,
                        popcnt_bytes(_mm256_and_si256(d0, _mm256_loadu_si256(p0)), lut, lo_mask),
                    );
                    x1 = _mm256_add_epi64(
                        x1,
                        popcnt_bytes(
                            _mm256_and_si256(d1, _mm256_loadu_si256(p0.add(1))),
                            lut,
                            lo_mask,
                        ),
                    );
                    x2 = _mm256_add_epi64(
                        x2,
                        popcnt_bytes(
                            _mm256_and_si256(d2, _mm256_loadu_si256(p0.add(2))),
                            lut,
                            lo_mask,
                        ),
                    );
                    x3 = _mm256_add_epi64(
                        x3,
                        popcnt_bytes(
                            _mm256_and_si256(d3, _mm256_loadu_si256(p0.add(3))),
                            lut,
                            lo_mask,
                        ),
                    );
                    y0 = _mm256_add_epi64(
                        y0,
                        popcnt_bytes(_mm256_and_si256(d0, _mm256_loadu_si256(p1)), lut, lo_mask),
                    );
                    y1 = _mm256_add_epi64(
                        y1,
                        popcnt_bytes(
                            _mm256_and_si256(d1, _mm256_loadu_si256(p1.add(1))),
                            lut,
                            lo_mask,
                        ),
                    );
                    y2 = _mm256_add_epi64(
                        y2,
                        popcnt_bytes(
                            _mm256_and_si256(d2, _mm256_loadu_si256(p1.add(2))),
                            lut,
                            lo_mask,
                        ),
                    );
                    y3 = _mm256_add_epi64(
                        y3,
                        popcnt_bytes(
                            _mm256_and_si256(d3, _mm256_loadu_si256(p1.add(3))),
                            lut,
                            lo_mask,
                        ),
                    );
                    i += WORDS_PER_BLOCK;
                }
                let sx = _mm256_add_epi64(_mm256_add_epi64(x0, x1), _mm256_add_epi64(x2, x3));
                let sy = _mm256_add_epi64(_mm256_add_epi64(y0, y1), _mm256_add_epi64(y2, y3));
                out0[row] += hsum_u64(sx);
                out1[row] += hsum_u64(sy);
                out0[row] += u64::from(super::scalar_and_cardinality(
                    &words[i..],
                    &q0[i..row_words],
                ));
                out1[row] += u64::from(super::scalar_and_cardinality(
                    &words[i..],
                    &q1[i..row_words],
                ));
            }
        }
    }

    /// # Safety
    ///
    /// Requires `avx2`, which the caller establishes.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn popcount(w: &[u64]) -> u32 {
        let n = w.len();
        let mut i = 0usize;
        let total;
        // SAFETY: B3, with one slice instead of two: `i + 16 <= n == w.len()`,
        // so the four loads read bytes `[8i, 8i + 128)`, inside `8 * n`.
        unsafe {
            let (lut, lo_mask) = (nibble_lut(), _mm256_set1_epi8(0x0f));
            let mut a0 = _mm256_setzero_si256();
            let mut a1 = _mm256_setzero_si256();
            let mut a2 = _mm256_setzero_si256();
            let mut a3 = _mm256_setzero_si256();
            while i + WORDS_PER_BLOCK <= n {
                debug_assert!(i + WORDS_PER_BLOCK <= w.len(), "B3");
                let pw = w.as_ptr().add(i).cast::<__m256i>();
                a0 = _mm256_add_epi64(a0, popcnt_bytes(_mm256_loadu_si256(pw), lut, lo_mask));
                a1 = _mm256_add_epi64(
                    a1,
                    popcnt_bytes(_mm256_loadu_si256(pw.add(1)), lut, lo_mask),
                );
                a2 = _mm256_add_epi64(
                    a2,
                    popcnt_bytes(_mm256_loadu_si256(pw.add(2)), lut, lo_mask),
                );
                a3 = _mm256_add_epi64(
                    a3,
                    popcnt_bytes(_mm256_loadu_si256(pw.add(3)), lut, lo_mask),
                );
                i += WORDS_PER_BLOCK;
            }
            let s = _mm256_add_epi64(_mm256_add_epi64(a0, a1), _mm256_add_epi64(a2, a3));
            total = hsum_u64(s) as u32;
        }
        total + super::scalar_popcount(&w[i..])
    }

    /// # Safety
    ///
    /// Requires `avx2`, which the caller establishes.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn combine<const OP: u8>(x: __m256i, y: __m256i) -> __m256i {
        // Register-to-register only, so `avx2` is each intrinsic's whole
        // precondition and `#[target_feature]` discharges it.
        //
        // **`andnot` takes its operands the other way round from NEON.**
        // `vbicq_u8( x, y )` is `x & !y`, while `_mm256_andnot_si256( a, b )` is
        // `!a & b`. Passing `( x, y )` here would compute `!x & y`, which is the
        // *other* asymmetric difference and is wrong in a way both operands
        // being bitmaps makes easy to miss.
        match OP {
            OP_AND => _mm256_and_si256(x, y),
            OP_OR => _mm256_or_si256(x, y),
            OP_XOR => _mm256_xor_si256(x, y),
            _ => _mm256_andnot_si256(y, x),
        }
    }

    /// # Safety
    ///
    /// Requires `avx2`, and bound **B4** exactly as the NEON arm states it.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn apply_into<const OP: u8>(
        wx: &[u64],
        wy: &[u64],
        out: &mut Vec<u64>,
    ) -> u32 {
        const N: usize = super::BITMAP_WORDS;
        debug_assert!(wx.len() >= N && wy.len() >= N, "B4");
        debug_assert!(out.is_empty() && out.capacity() >= N, "B4");
        let total;
        // SAFETY: B4. Every load reads bytes `[8i, 8i + 128)` of `wx` / `wy` for
        // `i + 16 <= N <= len`, and every store writes the same range of `out`,
        // whose capacity is at least `N` words. The loop covers `0..N` exactly,
        // so `set_len( N )` leaves no element uninitialized.
        unsafe {
            let (lut, lo_mask) = (nibble_lut(), _mm256_set1_epi8(0x0f));
            let dst = out.as_mut_ptr().cast::<__m256i>();
            let mut a0 = _mm256_setzero_si256();
            let mut a1 = _mm256_setzero_si256();
            let mut a2 = _mm256_setzero_si256();
            let mut a3 = _mm256_setzero_si256();
            let mut i = 0usize;
            while i + WORDS_PER_BLOCK <= N {
                debug_assert!(i + WORDS_PER_BLOCK <= wx.len(), "B4");
                debug_assert!(i + WORDS_PER_BLOCK <= wy.len(), "B4");
                debug_assert!(i + WORDS_PER_BLOCK <= out.capacity(), "B4");
                let px = wx.as_ptr().add(i).cast::<__m256i>();
                let py = wy.as_ptr().add(i).cast::<__m256i>();
                let po = dst.add(i / 4);
                let w0 = combine::<OP>(_mm256_loadu_si256(px), _mm256_loadu_si256(py));
                let w1 =
                    combine::<OP>(_mm256_loadu_si256(px.add(1)), _mm256_loadu_si256(py.add(1)));
                let w2 =
                    combine::<OP>(_mm256_loadu_si256(px.add(2)), _mm256_loadu_si256(py.add(2)));
                let w3 =
                    combine::<OP>(_mm256_loadu_si256(px.add(3)), _mm256_loadu_si256(py.add(3)));
                _mm256_storeu_si256(po, w0);
                _mm256_storeu_si256(po.add(1), w1);
                _mm256_storeu_si256(po.add(2), w2);
                _mm256_storeu_si256(po.add(3), w3);
                a0 = _mm256_add_epi64(a0, popcnt_bytes(w0, lut, lo_mask));
                a1 = _mm256_add_epi64(a1, popcnt_bytes(w1, lut, lo_mask));
                a2 = _mm256_add_epi64(a2, popcnt_bytes(w2, lut, lo_mask));
                a3 = _mm256_add_epi64(a3, popcnt_bytes(w3, lut, lo_mask));
                i += WORDS_PER_BLOCK;
            }
            out.set_len(N);
            let s = _mm256_add_epi64(_mm256_add_epi64(a0, a1), _mm256_add_epi64(a2, a3));
            total = hsum_u64(s) as u32;
        }
        total
    }

    pub(super) const OP_AND: u8 = 0;
    pub(super) const OP_OR: u8 = 1;
    pub(super) const OP_XOR: u8 = 2;
    pub(super) const OP_ANDNOT: u8 = 3;
}

/// `out[i] = wx[i] op wy[i]`, with the popcount fused into the same pass.
///
/// Returns the filled payload and its exact cardinality. The cached cardinality
/// has to be exact — the identities in [`super::card`] depend on it — and a
/// second 8 KiB counting pass would give back much of what the word loop wins.
///
/// The vector arm allocates **uninitialized** and fills every word, where the
/// scalar one starts from `vec![0u64; BITMAP_WORDS]`. That is not an oversight:
/// the zeroing pass is an 8 KiB write the kernel then overwrites in full, and it
/// is only safe to skip where the fill is known to cover the whole payload,
/// which is bound B4 and is checked by `debug_assert!` at the site.
fn apply_words(op: SetOp, wx: &[u64], wy: &[u64]) -> (Vec<u64>, u32) {
    #[cfg(target_arch = "aarch64")]
    if wx.len() >= BITMAP_WORDS
        && wy.len() >= BITMAP_WORDS
        && std::arch::is_aarch64_feature_detected!("neon")
    {
        let mut out: Vec<u64> = Vec::with_capacity(BITMAP_WORDS);
        // SAFETY: `neon` was just detected, both payloads were just checked to
        // be at least `BITMAP_WORDS` long, and `out` is empty with exactly that
        // capacity — bound B4.
        let len = unsafe {
            match op {
                SetOp::And => simd::apply_into::<{ simd::OP_AND }>(wx, wy, &mut out),
                SetOp::Or => simd::apply_into::<{ simd::OP_OR }>(wx, wy, &mut out),
                SetOp::Xor => simd::apply_into::<{ simd::OP_XOR }>(wx, wy, &mut out),
                SetOp::AndNot => simd::apply_into::<{ simd::OP_ANDNOT }>(wx, wy, &mut out),
            }
        };
        return (out, len);
    }
    #[cfg(target_arch = "x86_64")]
    if wx.len() >= BITMAP_WORDS
        && wy.len() >= BITMAP_WORDS
        && std::arch::is_x86_feature_detected!("avx2")
    {
        let mut out: Vec<u64> = Vec::with_capacity(BITMAP_WORDS);
        // SAFETY: `avx2` was just detected, both payloads were just checked to
        // be at least `BITMAP_WORDS` long, and `out` is empty with exactly that
        // capacity — bound B4.
        let len = unsafe {
            match op {
                SetOp::And => simd::apply_into::<{ simd::OP_AND }>(wx, wy, &mut out),
                SetOp::Or => simd::apply_into::<{ simd::OP_OR }>(wx, wy, &mut out),
                SetOp::Xor => simd::apply_into::<{ simd::OP_XOR }>(wx, wy, &mut out),
                SetOp::AndNot => simd::apply_into::<{ simd::OP_ANDNOT }>(wx, wy, &mut out),
            }
        };
        return (out, len);
    }
    scalar_apply_words(op, wx, wy)
}

/// The word loop, kept reachable and correct as the oracle for the vector arm
/// ( QG §4.3 ). Do not delete it when the vector arm is faster.
fn scalar_apply_words(op: SetOp, wx: &[u64], wy: &[u64]) -> (Vec<u64>, u32) {
    let mut out = vec![0u64; BITMAP_WORDS];
    let mut len = 0u32;
    match op {
        SetOp::And => {
            for i in 0..BITMAP_WORDS {
                let w = wx[i] & wy[i];
                out[i] = w;
                len += w.count_ones();
            }
        }
        SetOp::Or => {
            for i in 0..BITMAP_WORDS {
                let w = wx[i] | wy[i];
                out[i] = w;
                len += w.count_ones();
            }
        }
        SetOp::Xor => {
            for i in 0..BITMAP_WORDS {
                let w = wx[i] ^ wy[i];
                out[i] = w;
                len += w.count_ones();
            }
        }
        SetOp::AndNot => {
            for i in 0..BITMAP_WORDS {
                let w = wx[i] & !wy[i];
                out[i] = w;
                len += w.count_ones();
            }
        }
    }
    (out, len)
}

/// Apply `op` if both operands are bitmaps with word-aligned storage.
///
/// Returns `None` when this module does not handle the pair, so the caller falls
/// through to the generic kernel. The inner `Option` is the ordinary
/// empty-result signal.
#[inline]
pub fn try_apply(op: SetOp, a: &Container, b: &Container) -> Option<Option<Container>> {
    let (Container::Bitmap(x), Container::Bitmap(y)) = (a, b) else {
        return None;
    };
    // `try_words` fails only for an unaligned shared buffer, which the ladder
    // makes unreachable for extents we write — but a foreign file could produce
    // one, and falling through beats panicking.
    let (wx, wy) = (x.bits().try_words()?, y.bits().try_words()?);

    let (out, len) = apply_words(op, wx, wy);

    if len == 0 {
        // An empty container is never stored.
        return Some(None);
    }
    let mut c = Container::Bitmap(BitmapContainer::from_words(out, len));
    // A result that has fallen below the demote threshold should not stay a
    // bitmap just because its operands were: the caller may store it.
    c.ensure_demoted();
    Some(Some(c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::generic;

    fn bm(vals: &[u16]) -> Container {
        Container::Bitmap(BitmapContainer::from_sorted(vals))
    }

    const OPS: [SetOp; 4] = [SetOp::And, SetOp::Or, SetOp::Xor, SetOp::AndNot];

    // ---------------------------------------------------------------------
    // The vector word kernels, called directly.
    //
    // [`words_and_cardinality`], [`words_popcount`] and [`apply_words`] all
    /// Whether this host can reach the vector arm, so a direct-call wrapper
    /// cannot silently degrade into testing the scalar word loop.
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    pub(super) fn vector_arm_reachable() -> bool {
        #[cfg(target_arch = "aarch64")]
        {
            std::arch::is_aarch64_feature_detected!("neon")
        }
        #[cfg(target_arch = "x86_64")]
        {
            std::arch::is_x86_feature_detected!("avx2")
        }
    }

    // pick the vector arm through `is_aarch64_feature_detected!`, and fall back
    // to the scalar word loop when the host reports `neon` absent. A test that
    // reaches a kernel only through one of those dispatchers therefore degrades
    // to `scalar == scalar` on such a host, staying green for a kernel it never
    // executed.
    //
    // Measured, not argued: with the three gates forced to `false` and
    // `simd::and_cardinality`, `simd::popcount` and `simd::apply_into` each
    // sabotaged in turn, every test in this module stayed **green**. The
    // wrappers below are what make it red. Same property as `ops::run`'s
    // `vec_card`, same reason.
    //
    // [`words_disjoint`] and [`words_contains`] are **not** here and need no
    // wrapper: they are blocked *scalar* loops with no feature gate and no
    // vector arm, so the assertions on them mean what they say on every host.
    // ---------------------------------------------------------------------

    /// The fused and-popcount kernel, called directly.
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    fn vec_and_cardinality(wx: &[u64], wy: &[u64]) -> u32 {
        assert!(
            crate::ops::bitmap::tests::vector_arm_reachable(),
            "every aarch64 this crate targets has neon"
        );
        // SAFETY: `neon` was just detected.
        unsafe { simd::and_cardinality(wx, wy) }
    }

    /// The paired row kernel, called directly so a disabled dispatcher cannot
    /// turn its differential property into `scalar == scalar`.
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    fn vec_and_cardinality_rows2(
        data: &[u64],
        q0: &[u64],
        q1: &[u64],
        row_words: usize,
        out0: &mut [u64],
        out1: &mut [u64],
    ) {
        assert!(
            vector_arm_reachable(),
            "the vector feature must be available"
        );
        assert!(row_words >= 16, "satisfies B8 on both supported arches");
        // SAFETY: the feature was detected, and the property constructs equal
        // output lengths, full queries, and `rows * row_words` data ( B8 ).
        unsafe { simd::and_cardinality_rows2(data, q0, q1, row_words, out0, out1) };
    }

    /// The unary popcount kernel, called directly.
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    fn vec_popcount(w: &[u64]) -> u32 {
        assert!(
            crate::ops::bitmap::tests::vector_arm_reachable(),
            "every aarch64 this crate targets has neon"
        );
        // SAFETY: `neon` was just detected.
        unsafe { simd::popcount(w) }
    }

    /// The fused apply-and-count kernel, called directly.
    ///
    /// The `assert!` on the lengths is bound **B4**'s premise, which
    /// [`apply_words`] establishes with the same check before it dispatches. A
    /// caller that violated it would be unsound, not merely wrong, so it is a
    /// hard assertion rather than a `debug_assert!`.
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    fn vec_apply_words(op: SetOp, wx: &[u64], wy: &[u64]) -> (Vec<u64>, u32) {
        assert!(
            crate::ops::bitmap::tests::vector_arm_reachable(),
            "every aarch64 this crate targets has neon"
        );
        assert!(wx.len() >= BITMAP_WORDS && wy.len() >= BITMAP_WORDS, "B4");
        let mut out: Vec<u64> = Vec::with_capacity(BITMAP_WORDS);
        // SAFETY: `neon` was just detected, both payloads were just checked to
        // be at least `BITMAP_WORDS` long, and `out` is empty with exactly that
        // capacity — bound B4.
        let len = unsafe {
            match op {
                SetOp::And => simd::apply_into::<{ simd::OP_AND }>(wx, wy, &mut out),
                SetOp::Or => simd::apply_into::<{ simd::OP_OR }>(wx, wy, &mut out),
                SetOp::Xor => simd::apply_into::<{ simd::OP_XOR }>(wx, wy, &mut out),
                SetOp::AndNot => simd::apply_into::<{ simd::OP_ANDNOT }>(wx, wy, &mut out),
            }
        };
        (out, len)
    }

    /// A full-size payload, biased at the boundaries the word kernels have.
    ///
    /// Uniform random words would make `words_disjoint` answer `false` in the
    /// first word essentially always and `words_contains` answer `false` just as
    /// fast, so the block loop would never run past one iteration and the
    /// accumulator's lane bound would never be approached. The all-ones and
    /// all-zero arms are what make the *walking* answer reachable, and the
    /// striped one is what puts a hit in the middle of a block rather than at
    /// its start.
    fn payload() -> impl proptest::strategy::Strategy<Value = Vec<u64>> {
        use proptest::prelude::*;
        prop_oneof![
            // Saturated: every lane of every accumulator takes its maximum, so
            // a too-narrow accumulator wraps here and nowhere else.
            1 => Just(vec![!0u64; BITMAP_WORDS]),
            1 => Just(vec![0u64; BITMAP_WORDS]),
            3 => (any::<u64>(), 1usize..64).prop_map(|(seed, k)| {
                let mut s = seed | 1;
                (0..BITMAP_WORDS)
                    .map(|i| {
                        s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
                        if i % k == 0 { s } else { 0 }
                    })
                    .collect()
            }),
            3 => any::<u64>().prop_map(|seed| {
                let mut s = seed | 1;
                (0..BITMAP_WORDS)
                    .map(|_| {
                        s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
                        s
                    })
                    .collect()
            }),
            // A single bit, at the ends of the payload, of a block, and of a
            // word — where an off-by-one in a bound or a mask shows up.
            2 => prop_oneof![
                Just(0usize), Just(1), Just(63), Just(64),
                Just(8 * 64 - 1), Just(8 * 64), Just(16 * 64 - 1),
                Just(BITMAP_WORDS * 64 - 1),
            ]
            .prop_map(|b| {
                let mut w = vec![0u64; BITMAP_WORDS];
                w[b >> 6] |= 1u64 << (b & 63);
                w
            }),
        ]
    }

    /// A payload **shorter than one block**, so the scalar tails run.
    ///
    /// Every caller in the crate passes exactly `BITMAP_WORDS` words, which is
    /// a multiple of both the vector block ( 8 ) and the predicate block ( 16 ),
    /// so the `as_chunks` remainders and the `&wx[i..n]` tails are
    /// **unreachable from the crate**. They are still the difference between a
    /// right answer and a wrong one for any future caller, and a lengths sweep
    /// is the only thing that can see them.
    fn short_payload() -> impl proptest::strategy::Strategy<Value = Vec<u64>> {
        use proptest::prelude::*;
        proptest::collection::vec(
            prop_oneof![
                2 => any::<u64>(),
                1 => Just(0u64),
                1 => Just(!0u64),
                1 => (0u32..64).prop_map(|b| 1u64 << b),
            ],
            0..40,
        )
    }

    proptest::proptest! {
        /// The whole-container pair kernel against its scalar oracle.
        ///
        /// Nonzero outputs pin the additive contract used when a counter has
        /// already consumed earlier chunks. Widths cross both vector block
        /// boundaries and leave tails on either architecture.
        #[test]
        fn paired_row_counts_agree_with_the_scalar_oracle(
            seed in proptest::prelude::any::<u64>(),
            row_words in 16usize..129,
            rows in 1usize..17,
        ) {
            let mut state = seed | 1;
            let mut next = || {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                state
            };
            let data: Vec<_> = (0..row_words * rows).map(|_| next()).collect();
            let q0: Vec<_> = (0..row_words).map(|_| next()).collect();
            let q1: Vec<_> = (0..row_words).map(|_| next()).collect();
            let mut want0 = vec![7u64; rows];
            let mut want1 = vec![11u64; rows];
            scalar_and_cardinality_rows2(
                &data,
                &q0,
                &q1,
                row_words,
                &mut want0,
                &mut want1,
            );

            let mut got0 = vec![7u64; rows];
            let mut got1 = vec![11u64; rows];
            words_and_cardinality_rows2(
                &data,
                &q0,
                &q1,
                row_words,
                &mut got0,
                &mut got1,
            );
            proptest::prop_assert_eq!((&got0, &got1), (&want0, &want1));

            #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
            {
                let mut vector0 = vec![7u64; rows];
                let mut vector1 = vec![11u64; rows];
                vec_and_cardinality_rows2(
                    &data,
                    &q0,
                    &q1,
                    row_words,
                    &mut vector0,
                    &mut vector1,
                );
                proptest::prop_assert_eq!((&vector0, &vector1), (&want0, &want1));
            }
        }

        /// Every word kernel against the scalar form it replaced.
        ///
        /// The scalar functions are kept reachable precisely so this can exist
        /// ( QG §4.3 ): they are the oracle, not dead code.
        #[test]
        fn the_word_kernels_agree_with_their_scalar_oracles(
            x in payload(),
            y in payload(),
        ) {
            proptest::prop_assert_eq!(
                words_and_cardinality(&x, &y),
                scalar_and_cardinality(&x, &y)
            );
            proptest::prop_assert_eq!(words_popcount(&x), scalar_popcount(&x));
            proptest::prop_assert_eq!(
                words_disjoint(&x, &y),
                x.iter().zip(&y).all(|(p, q)| p & q == 0)
            );
            proptest::prop_assert_eq!(
                words_contains(&x, &y),
                x.iter().zip(&y).all(|(p, q)| q & !p == 0)
            );
            for op in OPS {
                let fast = apply_words(op, &x, &y);
                let slow = scalar_apply_words(op, &x, &y);
                proptest::prop_assert_eq!(&fast.0, &slow.0, "{:?} words differ", op);
                proptest::prop_assert_eq!(fast.1, slow.1, "{:?} cardinality differs", op);
                // And the kernel itself, so the comparison does not become
                // `scalar == scalar` on a host reporting `neon` absent.
                #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
                {
                    let vec = vec_apply_words(op, &x, &y);
                    proptest::prop_assert_eq!(&vec.0, &slow.0, "{:?} vector words differ", op);
                    proptest::prop_assert_eq!(
                        vec.1, slow.1,
                        "{:?} vector cardinality differs", op
                    );
                }
            }
            #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
            {
                proptest::prop_assert_eq!(
                    vec_and_cardinality(&x, &y),
                    scalar_and_cardinality(&x, &y)
                );
                proptest::prop_assert_eq!(vec_popcount(&x), scalar_popcount(&x));
            }
        }

        /// The same, on payloads shorter than a block, so the tails run.
        ///
        /// `apply_words` is **not** here: it requires exactly `BITMAP_WORDS`
        /// and asserts so, because a shorter result would not be a valid bitmap
        /// container. Only the length-agnostic kernels are swept.
        #[test]
        fn the_word_kernel_tails_agree_with_their_scalar_oracles(
            x in short_payload(),
            y in short_payload(),
        ) {
            let n = x.len().min(y.len());
            proptest::prop_assert_eq!(
                words_and_cardinality(&x, &y),
                scalar_and_cardinality(&x, &y)
            );
            proptest::prop_assert_eq!(words_popcount(&x), scalar_popcount(&x));
            proptest::prop_assert_eq!(
                words_disjoint(&x, &y),
                x[..n].iter().zip(&y[..n]).all(|(p, q)| p & q == 0)
            );
            proptest::prop_assert_eq!(
                words_contains(&x, &y),
                x[..n].iter().zip(&y[..n]).all(|(p, q)| q & !p == 0)
            );
            // The two length-agnostic kernels, called directly: the tails are
            // theirs, so the dispatchers must not be the only way in.
            #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
            {
                proptest::prop_assert_eq!(
                    vec_and_cardinality(&x, &y),
                    scalar_and_cardinality(&x, &y)
                );
                proptest::prop_assert_eq!(vec_popcount(&x), scalar_popcount(&x));
            }
        }
    }

    /// Every length from 0 to three blocks, on both sides.
    ///
    /// The deterministic companion to the property above: it guarantees that
    /// each residue class of the vector block ( 8 ) and of the predicate block
    /// ( 16 ) appears on both operands in every combination, which a random
    /// strategy only promises in expectation. This is the test a wrong loop
    /// bound fails, and the `debug_assert!`s carrying **B3** are live because
    /// `cargo test` builds without optimizations.
    #[test]
    fn the_word_kernels_agree_at_every_block_boundary() {
        let mk = |n: usize, seed: u64| -> Vec<u64> {
            let mut s = seed | 1;
            (0..n)
                .map(|_| {
                    s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
                    // Sparse enough that `words_disjoint` sometimes says yes.
                    s & (s >> 17) & (s >> 31)
                })
                .collect()
        };
        for la in 0..=48usize {
            for lb in 0..=48usize {
                let (x, y) = (mk(la, 0xA1), mk(lb, 0xB2));
                let n = la.min(lb);
                assert_eq!(
                    words_and_cardinality(&x, &y),
                    scalar_and_cardinality(&x, &y),
                    "cardinality at {la}x{lb}"
                );
                assert_eq!(words_popcount(&x), scalar_popcount(&x), "popcount at {la}");
                assert_eq!(
                    words_disjoint(&x, &y),
                    x[..n].iter().zip(&y[..n]).all(|(p, q)| p & q == 0),
                    "disjoint at {la}x{lb}"
                );
                assert_eq!(
                    words_contains(&x, &y),
                    x[..n].iter().zip(&y[..n]).all(|(p, q)| q & !p == 0),
                    "contains at {la}x{lb}"
                );
                // The kernels themselves at each residue class, for the reason
                // given above `vec_and_cardinality`. `apply_words` is not here
                // for the same reason it is absent from the tails property: it
                // requires exactly `BITMAP_WORDS`.
                #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
                {
                    assert_eq!(
                        vec_and_cardinality(&x, &y),
                        scalar_and_cardinality(&x, &y),
                        "vector cardinality at {la}x{lb}"
                    );
                    assert_eq!(
                        vec_popcount(&x),
                        scalar_popcount(&x),
                        "vector popcount at {la}"
                    );
                }
            }
        }
    }

    /// The accumulator must not wrap on the densest payload there is.
    ///
    /// A `u16` lane holds at most 8192 by the bound stated on
    /// [`words_and_cardinality`], and the only input that reaches the maximum is
    /// two full payloads. If the accumulator were `u8`-lane wide, or the final
    /// reduction stayed in `u16`, this is the case that wraps — 65 536 is one
    /// past `u16::MAX`, which is why the reduction widens first.
    #[test]
    fn a_full_payload_counts_every_bit_without_wrapping() {
        let full = vec![!0u64; BITMAP_WORDS];
        assert_eq!(words_and_cardinality(&full, &full), 65_536);
        assert_eq!(words_popcount(&full), 65_536);
        let (out, len) = apply_words(SetOp::Or, &full, &full);
        assert_eq!(len, 65_536);
        assert_eq!(out, full);
        // The accumulator that could wrap is the kernel's, so ask it directly.
        #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
        {
            assert_eq!(vec_and_cardinality(&full, &full), 65_536);
            assert_eq!(vec_popcount(&full), 65_536);
            let (vout, vlen) = vec_apply_words(SetOp::Or, &full, &full);
            assert_eq!(vlen, 65_536);
            assert_eq!(vout, full);
        }
    }

    /// `AndNot` is the one op whose operand order the dispatcher does not
    /// normalize, so the vector arm must not be symmetric in it.
    #[test]
    fn the_vector_andnot_keeps_its_operand_order() {
        let mut x = vec![0u64; BITMAP_WORDS];
        let mut y = vec![0u64; BITMAP_WORDS];
        x[0] = 0b1100;
        y[0] = 0b1010;
        assert_eq!(apply_words(SetOp::AndNot, &x, &y).0[0], 0b0100, "x \\ y");
        assert_eq!(apply_words(SetOp::AndNot, &y, &x).0[0], 0b0010, "y \\ x");
        // The operand order this test exists for is `simd::combine`'s
        // `vbicq_u8`, which lives behind the feature gate. Reaching it only
        // through `apply_words` would leave the whole test asserting the scalar
        // arm's operand order on a host reporting `neon` absent — the one thing
        // it is not about.
        #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
        {
            assert_eq!(
                vec_apply_words(SetOp::AndNot, &x, &y).0[0],
                0b0100,
                "vector x \\ y"
            );
            assert_eq!(
                vec_apply_words(SetOp::AndNot, &y, &x).0[0],
                0b0010,
                "vector y \\ x"
            );
        }
    }

    /// The specialization must be indistinguishable from the oracle.
    #[test]
    fn every_arm_agrees_with_the_generic_kernel() {
        let cases: Vec<(Vec<u16>, Vec<u16>)> = vec![
            (
                (0..5000u16).map(|i| i * 2).collect(),
                (0..5000u16).map(|i| i * 3).collect(),
            ),
            ((0..9000u16).collect(), (4000..12000u16).collect()),
            (vec![0, 65535], vec![0, 1, 65535]),
            ((0..5000u16).collect(), (0..5000u16).collect()), // identical
            ((0..5000u16).collect(), (30000..35000u16).collect()), // disjoint
        ];

        for (av, bv) in cases {
            let (a, b) = (bm(&av), bm(&bv));
            for op in OPS {
                let fast = try_apply(op, &a, &b).expect("both are bitmaps");
                let slow = generic::apply(op, &a, &b);
                let f: Vec<u16> = fast
                    .as_ref()
                    .map(|c| c.iter().collect())
                    .unwrap_or_default();
                let s: Vec<u16> = slow
                    .as_ref()
                    .map(|c| c.iter().collect())
                    .unwrap_or_default();
                assert_eq!(f, s, "{op:?} disagrees with the oracle");
                assert_eq!(
                    fast.as_ref().map(|c| c.len()),
                    slow.as_ref().map(|c| c.len()),
                    "{op:?} cardinality disagrees"
                );
            }
        }
    }

    #[test]
    fn the_cached_cardinality_is_exact() {
        // The identities in ops::card depend on this being right, and a fused
        // popcount is easy to get subtly wrong.
        let a = bm(&(0..7000u16).map(|i| i * 2).collect::<Vec<_>>());
        let b = bm(&(0..7000u16).map(|i| i * 3).collect::<Vec<_>>());
        for op in OPS {
            if let Some(Some(c)) = try_apply(op, &a, &b) {
                assert_eq!(
                    c.len(),
                    c.iter().count() as u32,
                    "{op:?}: cached length disagrees with the contents"
                );
            }
        }
    }

    #[test]
    fn an_empty_result_is_none_not_an_empty_container() {
        let a = bm(&(0..5000u16).collect::<Vec<_>>());
        assert_eq!(try_apply(SetOp::Xor, &a, &a), Some(None));
        assert_eq!(try_apply(SetOp::AndNot, &a, &a), Some(None));
    }

    #[test]
    fn a_sparse_result_is_demoted() {
        // Two dense bitmaps can intersect to something an array should hold.
        let a = bm(&(0..5000u16).collect::<Vec<_>>());
        let b = bm(&(4990..10000u16).collect::<Vec<_>>());
        let r = try_apply(SetOp::And, &a, &b).unwrap().unwrap();
        assert_eq!(r.len(), 10);
        assert_eq!(
            r.kind(),
            crate::ContainerKind::Array,
            "a 10-element result must not stay a bitmap"
        );
    }

    #[test]
    fn non_bitmap_pairs_fall_through() {
        let arr = Container::from_sorted(&[1, 2, 3]);
        let bmp = bm(&(0..5000u16).collect::<Vec<_>>());
        assert!(try_apply(SetOp::And, &arr, &bmp).is_none());
        assert!(try_apply(SetOp::And, &bmp, &arr).is_none());
        assert!(try_apply(SetOp::And, &arr, &arr).is_none());
    }
}
