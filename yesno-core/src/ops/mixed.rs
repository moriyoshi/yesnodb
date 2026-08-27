//! Cross-kind kernels: every pair of unlike container types, in either order.
//!
//! # Why this arm
//!
//! With run×run specialized, bitmap×run was the worst remaining pair by a wide
//! margin — measured against the `roaring` crate on eight chunks:
//!
//! ```text
//!   bitmap x run  AND   716 us  vs  2.24 us    320x
//!   bitmap x run  OR   1057 us  vs  2.23 us    475x
//! ```
//!
//! The cause is the same one that made run×run pathological: [`super::generic`]
//! merges *values*, so a run of 30 000 contiguous ordinals costs 30 000
//! iterations against a structure that could have answered the whole interval
//! with a handful of word operations.
//!
//! # The shape
//!
//! Every arm is "walk the run's intervals, apply a word-masked operation to the
//! bitmap". That is `O(nruns + BITMAP_WORDS)` rather than `O(cardinality)`, and
//! the interval count is what a run container is small *because of*.
//!
//! Operand order matters for the asymmetric ops, and both directions are
//! handled: `bitmap \ run` clears the intervals, while `run \ bitmap` keeps the
//! intervals minus whatever the bitmap holds.
//!
//! # The generic kernel remains the oracle
//!
//! Differential-tested against `generic::apply` over overlapping, nested,
//! disjoint and boundary-touching inputs in both orders.

use crate::container::{BitmapContainer, Container};
use crate::ops::generic::SetOp;
use crate::BITMAP_WORDS;

/// Call `f(word_index, mask)` for each word the inclusive range `[s, e]` touches.
///
/// Separate from `container::bitmap::apply_range`, which hands the closure a
/// `&mut u64` and no index — enough for "set these bits", not for the intersect
/// arm, which has to read the *other* operand's word at the same position.
#[inline]
pub(crate) fn for_each_masked_word(s: u16, e: u16, mut f: impl FnMut(usize, u64)) {
    debug_assert!(s <= e);
    let (sw, ew) = (s as usize >> 6, e as usize >> 6);
    let (sb, eb) = ((s & 63) as u32, (e & 63) as u32);
    if sw == ew {
        let mask = if eb == 63 {
            !0u64 << sb
        } else {
            ((1u64 << (eb + 1)) - 1) & (!0u64 << sb)
        };
        f(sw, mask);
        return;
    }
    f(sw, !0u64 << sb);
    for w in sw + 1..ew {
        f(w, !0u64);
    }
    f(
        ew,
        if eb == 63 {
            !0u64
        } else {
            (1u64 << (eb + 1)) - 1
        },
    );
}

/// `|bitmap ∩ [s, e]|`, without going through [`for_each_masked_word`].
///
/// # Why this is not the closure
///
/// The bitmap×run cardinality arm is a scalar walk over intervals and **cannot
/// be widened**: the word index it touches comes from the interval, so the
/// loads are a gather. AArch64 NEON has no gather; SVE2 does and this machine
/// has it, but SVE intrinsics are not on stable Rust at the crate's MSRV. So
/// the only headroom in that arm is the per-interval constant, which is what
/// this removes:
///
/// * the end mask is `!0 >> (63 - eb)`, which needs no `eb == 63` special case
///   — the closure's version branches on it once per interval;
/// * the whole-word middle is a plain slice sum, which LLVM widens into the
///   same `cnt`/`uaddlp` ladder the bitmap arm gets, where a per-word closure
///   call does not always survive inlining into one.
///
/// Measured over 32 distinct bitmap × run pairs, ns per pair, second of two
/// consecutive runs, `taskset` to the big cores:
///
/// ```text
///   intervals      1      8     64    512   2048
///   closure     35.6   51.2  179.0  743.8 2663.7
///   this        35.8   48.8  175.1  627.7 1993.9
///   speedup     0.99   1.05   1.02   1.19   1.34
/// ```
///
/// The win is entirely in the interval-count-dominated rows, and the first
/// three columns are **inside this machine's build-to-build drift** ( up to 8%
/// on kernels that were not touched at all — see the `run_x_run` control column
/// in `benches/setops.rs` ). So the honest claim is 1.2-1.3x at 512 intervals
/// and above, and nothing below that. One interval per chunk — the first
/// column — is the *only* shape this file's other groups ever measured.
///
/// It is still `O(nruns)` and the ratio against the reference gets *worse* with
/// interval count regardless: see the note on [`try_apply`].
#[inline]
pub(crate) fn masked_and_popcount(bw: &[u64], s: u16, e: u16) -> u32 {
    debug_assert!(s <= e);
    let (sw, ew) = (s as usize >> 6, e as usize >> 6);
    let (sb, eb) = ((s & 63) as u32, (e & 63) as u32);
    let head = !0u64 << sb;
    let tail = !0u64 >> (63 - eb);
    if sw == ew {
        return (bw[sw] & head & tail).count_ones();
    }
    let mut n = (bw[sw] & head).count_ones() + (bw[ew] & tail).count_ones();
    n += bw[sw + 1..ew].iter().map(|w| w.count_ones()).sum::<u32>();
    n
}

/// `bitmap ∩ [s, e] != ∅`, stopping at the first word that meets the interval.
///
/// Not `masked_and_popcount(..) != 0`. `ops::card` records that a predicate
/// must never cost more than the count it is weaker than, and the arm this
/// replaces could not exit *within* an interval at all — its closure has no
/// return value, so a 1000-word run was walked to the end even when its first
/// word already answered the question.
#[inline]
pub(crate) fn masked_intersects(bw: &[u64], s: u16, e: u16) -> bool {
    debug_assert!(s <= e);
    let (sw, ew) = (s as usize >> 6, e as usize >> 6);
    let (sb, eb) = ((s & 63) as u32, (e & 63) as u32);
    let head = !0u64 << sb;
    let tail = !0u64 >> (63 - eb);
    if sw == ew {
        return bw[sw] & head & tail != 0;
    }
    if bw[sw] & head != 0 || bw[ew] & tail != 0 {
        return true;
    }
    words_any_set(&bw[sw + 1..ew])
}

/// Words OR-accumulated between two checks of the running value.
///
/// The same compromise, for the same reason, as `ops::bitmap`'s
/// `PREDICATE_BLOCK`, and deliberately the same size: a block wide enough to
/// widen, narrow enough that the early exit still means something. Named
/// separately rather than shared because the two are independent decisions about
/// two different loops, not one constant with two callers.
const INTERSECT_BLOCK: usize = 16;

/// `∃ i. w[i] != 0`, in blocks.
///
/// # Why this is not `w.iter().any(|x| *x != 0)`
///
/// It was, and `ops::bitmap` records exactly what that costs: [`Iterator::any`]
/// short-circuits, a short-circuiting reduction cannot be widened, and the
/// release assembly for the equivalent `all` walked **one 8-byte word per
/// iteration with zero vector instructions** while the counting loop beside it
/// got a full `cnt`/`uaddlp` ladder over 64 bytes.
///
/// That matters here because [`masked_intersects`] is the arm
/// `Container::is_range_empty` uses, and its own rule is that the predicate must
/// not cost more than the count it is weaker than. The count on the same window
/// is `Container::rank` twice — vectorized popcount over the whole prefix below
/// `hi` — so a scalar word-at-a-time scan of the window can lose to it even
/// though it reads strictly fewer words. On a 25 000-bit empty window of an 8 KiB
/// bitmap that was **154 ns against the count's 141 ns**; blocked, it is 24 ns.
///
/// OR-accumulating a fixed block has no loop-carried branch, so it widens, and
/// checking once per block keeps an exit that is coarse rather than absent.
/// **The block loop is skipped below one block, and that is measured, not
/// assumed.** Blocking was justified on a *wide* window — 25 000 bits, ~390
/// words — where it takes 154 ns to 24 ns. The other caller of
/// [`masked_intersects`] is `ops::card::is_disjoint`'s bitmap x run arm, which
/// feeds it **one interval at a time**: a 300-value run is ~5 words, so the
/// middle slice here is ~3 and `chunks_exact` yields no full chunk at all. The
/// blocking apparatus was pure overhead on exactly that path.
///
/// A/B/A on `predicate_paths/is_disjoint/bitmap_x_run`, 2026-09-07:
/// 163.92 ns baseline, **160.99 ns** with this guard, 163.82 ns on revert —
/// 1.8%, reversing with the code and reproducing the baseline to 0.06%.
/// That machine's one-way run-to-run drift was itself ~1.8%, so the
/// magnitude alone proves nothing; **the reversal is the evidence.** Do not
/// re-check this with a single before/after pair.
#[inline]
fn words_any_set(w: &[u64]) -> bool {
    if w.len() < INTERSECT_BLOCK {
        return w.iter().any(|x| *x != 0);
    }
    // `as_chunks` rather than `chunks_exact`, which is what clippy 1.98's
    // `chunks_exact_to_as_chunks` asks for on a constant block size. Identical
    // work: the blocks are `&[[u64; INTERSECT_BLOCK]]` and the tail is the same
    // remainder, so the early exit per block and the final fold are unchanged.
    let (blocks, rest) = w.as_chunks::<INTERSECT_BLOCK>();
    for c in blocks {
        let mut acc = 0u64;
        for &x in c {
            acc |= x;
        }
        if acc != 0 {
            return true;
        }
    }
    let mut acc = 0u64;
    for &x in rest {
        acc |= x;
    }
    acc != 0
}

/// `[s, e] ⊆ bitmap`, stopping at the first word that is not fully set.
///
/// The dual of [`masked_intersects`], testing `== mask` rather than `!= 0`, and
/// it carries the same note: the arm it replaces could not exit within an
/// interval.
#[inline]
pub(crate) fn masked_covers(bw: &[u64], s: u16, e: u16) -> bool {
    debug_assert!(s <= e);
    let (sw, ew) = (s as usize >> 6, e as usize >> 6);
    let (sb, eb) = ((s & 63) as u32, (e & 63) as u32);
    let head = !0u64 << sb;
    let tail = !0u64 >> (63 - eb);
    if sw == ew {
        let m = head & tail;
        return bw[sw] & m == m;
    }
    bw[sw] & head == head && bw[ew] & tail == tail && bw[sw + 1..ew].iter().all(|w| *w == !0u64)
}

/// Apply `op` when the pair is bitmap×run in either order, else `None`.
#[inline]
pub fn try_apply(op: SetOp, a: &Container, b: &Container) -> Option<Option<Container>> {
    let (bm, rn, bitmap_is_lhs) = match (a, b) {
        (Container::Bitmap(x), Container::Run(y)) => (x, y, true),
        (Container::Run(y), Container::Bitmap(x)) => (x, y, false),
        _ => return None,
    };
    // Unaligned shared storage is unreachable for extents this crate writes, but
    // a foreign file could produce one; falling through beats panicking.
    let bw = bm.bits().try_words()?;

    let mut out = vec![0u64; BITMAP_WORDS];
    let n = rn.nruns();

    match op {
        SetOp::And => {
            for i in 0..n {
                for_each_masked_word(rn.start(i), rn.end(i), |w, m| out[w] |= bw[w] & m);
            }
        }
        SetOp::Or => {
            out.copy_from_slice(bw);
            for i in 0..n {
                for_each_masked_word(rn.start(i), rn.end(i), |w, m| out[w] |= m);
            }
        }
        SetOp::Xor => {
            out.copy_from_slice(bw);
            for i in 0..n {
                for_each_masked_word(rn.start(i), rn.end(i), |w, m| out[w] ^= m);
            }
        }
        SetOp::AndNot if bitmap_is_lhs => {
            // bitmap \ run: clear the intervals.
            out.copy_from_slice(bw);
            for i in 0..n {
                for_each_masked_word(rn.start(i), rn.end(i), |w, m| out[w] &= !m);
            }
        }
        SetOp::AndNot => {
            // run \ bitmap: keep the intervals, minus what the bitmap holds.
            for i in 0..n {
                for_each_masked_word(rn.start(i), rn.end(i), |w, m| out[w] |= m & !bw[w]);
            }
        }
    }

    let len = crate::ops::bitmap::words_popcount(&out);
    if len == 0 {
        // An empty container is never stored.
        return Some(None);
    }
    let mut c = Container::Bitmap(BitmapContainer::from_words(out, len));
    // The result may be far sparser than a bitmap, or run-shaped again.
    c.ensure_demoted();
    Some(Some(c))
}

/// Apply `op` when the pair is array×bitmap in either order, else `None`.
///
/// Probing, not merging. The array side is the small one by construction — at
/// most `ARRAY_MAX` values against a bitmap that answers membership in a shift
/// and a mask — so the cost is the array's cardinality, never the bitmap's.
///
/// This is the most common real pair: a sparse posting list intersected with a
/// dense one. It was measured at **29x slower than the `roaring` crate** while
/// routing through the value-merging generic kernel.
#[inline]
pub fn try_apply_array_bitmap(
    op: SetOp,
    a: &Container,
    b: &Container,
) -> Option<Option<Container>> {
    let (arr, bm, array_is_lhs) = match (a, b) {
        (Container::Array(x), Container::Bitmap(y)) => (x, y, true),
        (Container::Bitmap(y), Container::Array(x)) => (x, y, false),
        _ => return None,
    };
    let bw = bm.bits().try_words()?;
    let vals = arr.as_slice();

    // And and the array-side difference both yield a *subset of the array*, so
    // they stay arrays and never touch a bitmap's worth of memory.
    match op {
        SetOp::And => {
            let keep: Vec<u16> = vals
                .iter()
                .copied()
                .filter(|v| bw[*v as usize >> 6] & (1u64 << (*v & 63)) != 0)
                .collect();
            return Some(finish_array(keep));
        }
        SetOp::AndNot if array_is_lhs => {
            let keep: Vec<u16> = vals
                .iter()
                .copied()
                .filter(|v| bw[*v as usize >> 6] & (1u64 << (*v & 63)) == 0)
                .collect();
            return Some(finish_array(keep));
        }
        _ => {}
    }

    // The rest start from the bitmap and poke the array's bits into it.
    let mut out = bw.to_vec();
    match op {
        SetOp::Or => {
            for &v in vals {
                out[v as usize >> 6] |= 1u64 << (v & 63);
            }
        }
        SetOp::Xor => {
            for &v in vals {
                out[v as usize >> 6] ^= 1u64 << (v & 63);
            }
        }
        SetOp::AndNot => {
            // bitmap \ array.
            for &v in vals {
                out[v as usize >> 6] &= !(1u64 << (v & 63));
            }
        }
        SetOp::And => unreachable!("handled above"),
    }

    let len = crate::ops::bitmap::words_popcount(&out);
    if len == 0 {
        return Some(None);
    }
    let mut c = Container::Bitmap(BitmapContainer::from_words(out, len));
    c.ensure_demoted();
    Some(Some(c))
}

/// Array values kept by their membership of a run, in one merge pass.
///
/// Both sides are sorted, so a single walk answers the whole join in
/// O(n + nruns). Probing `RunContainer::contains` per value costs
/// O(n log nruns) instead, and pays that logarithm on every element even when
/// the two are interleaved — which is the shape that actually occurs.
/// The array x run compaction, one 8-lane block at a time.
///
/// # Why a block can be decided without a per-lane interval index
///
/// Both sides are sorted, so for a block of eight values only the intervals
/// overlapping `[ v0, v7 ]` can matter: everything ending below `v0` was passed,
/// and everything starting above `v7` is out of reach. OR-ing those intervals'
/// range masks therefore gives a **complete** membership mask for the block, and
/// the surviving lanes are shuffled to the front and stored in one go.
///
/// That completeness is also what makes `AndNot` fall out for free: a lane
/// missed by every interval that could contain it is genuinely outside the run,
/// so the same mask inverted is the other answer. It would *not* be free if
/// the scan were truncated early — an interval skipped for speed would read as
/// an absence.
#[cfg(target_arch = "aarch64")]
mod simd {
    use crate::ops::array::SHUFFLE;
    use core::arch::aarch64::*;

    /// Lanes per block. The same 8 as `ops::array`, and for the same reason:
    /// `u16` lanes in a 128-bit register.
    pub(super) const BLOCK: usize = 8;

    /// `vals` filtered by membership of the run, into `out`.
    ///
    /// `flat` is the run's `( start, len )` pairs, so interval `k` is
    /// `[ flat[2k], flat[2k] + flat[2k+1] ]`.
    ///
    /// # Safety
    ///
    /// Requires `neon`, which the caller establishes. `out` must be empty with
    /// capacity at least `vals.len() + BLOCK` — bound **B3**: the store writes a
    /// whole 16-byte register even when fewer lanes survive, so the reservation
    /// has to cover one block past the largest possible result. That is the same
    /// bound, for the same reason, as `ops::array`'s B2.
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn filter_by_run(
        vals: &[u16],
        flat: &[u16],
        in_run: bool,
        out: &mut Vec<u16>,
    ) {
        debug_assert!(out.is_empty() && out.capacity() >= vals.len() + BLOCK, "B3");
        let n = vals.len();
        let m = flat.len() / 2;
        let (mut i, mut j, mut w) = (0usize, 0usize, 0usize);
        // SAFETY: B3, and the two index bounds the loops maintain.
        //
        // - Every `get_unchecked` on `vals` is under `i + BLOCK <= n`, re-tested
        //   at the top of each iteration and never advanced between the test and
        //   the reads; the tail loop indexes `i..n` directly.
        // - Every `get_unchecked` on `flat` is under `j < m` or `k < m`, where
        //   `m = flat.len() / 2`, so `2k + 1 <= 2m - 1 < flat.len()`.
        // - The store writes a whole 16-byte register at `p.add( w )`. `w` only
        //   ever advances by the popcount of an 8-bit mask, so `w <= i <= n`,
        //   and B3 reserved `n + BLOCK` — hence `w + BLOCK <= capacity`. That
        //   reservation is the caller's obligation, asserted above in debug.
        unsafe {
            let p = out.as_mut_ptr();
            let lanebit = vld1q_u16([1u16, 2, 4, 8, 16, 32, 64, 128].as_ptr());
            while i + BLOCK <= n {
                let first = *vals.get_unchecked(i);
                let last = *vals.get_unchecked(i + BLOCK - 1);
                // Intervals ending below the block cannot matter again, and `j`
                // only advances, which keeps the whole walk linear in `m`.
                while j < m && *flat.get_unchecked(j * 2) + *flat.get_unchecked(j * 2 + 1) < first {
                    j += 1;
                }
                let v = vld1q_u16(vals.as_ptr().add(i));
                let mut hit = vdupq_n_u16(0);
                let mut k = j;
                while k < m && *flat.get_unchecked(k * 2) <= last {
                    let s = *flat.get_unchecked(k * 2);
                    let e = s + *flat.get_unchecked(k * 2 + 1);
                    hit = vorrq_u16(
                        hit,
                        vandq_u16(vcgeq_u16(v, vdupq_n_u16(s)), vcleq_u16(v, vdupq_n_u16(e))),
                    );
                    if e >= last {
                        break;
                    }
                    k += 1;
                }
                // A hit lane is `0xFFFF`, so masking by the lane weights and
                // summing gives the 8-bit selector. It cannot overflow: the
                // weights total 255.
                let mut bits = vaddvq_u16(vandq_u16(hit, lanebit)) as usize & 0xFF;
                if !in_run {
                    bits = !bits & 0xFF;
                }
                let shuf = vld1q_u8(SHUFFLE[bits].as_ptr());
                let packed = vqtbl1q_u8(vreinterpretq_u8_u16(v), shuf);
                // B3: `w <= i` always, so `w + BLOCK <= n + BLOCK <= capacity`.
                vst1q_u16(p.add(w), vreinterpretq_u16_u8(packed));
                w += (bits as u32).count_ones() as usize;
                i += BLOCK;
            }
            // Fewer than a block remains; finish it the scalar way.
            for idx in i..n {
                let val = *vals.get_unchecked(idx);
                while j < m && *flat.get_unchecked(j * 2) + *flat.get_unchecked(j * 2 + 1) < val {
                    j += 1;
                }
                if (j < m && *flat.get_unchecked(j * 2) <= val) == in_run {
                    *p.add(w) = val;
                    w += 1;
                }
            }
            out.set_len(w);
        }
    }
}

/// The SSE arm, the structural twin of the NEON one above.
///
/// # One real difference: x86 has no unsigned 16-bit compare
///
/// NEON's `vcgeq_u16` / `vcleq_u16` compare unsigned directly. SSE2's
/// `_mm_cmpgt_epi16` is **signed**, so a naive port silently mis-orders every
/// value at or above `0x8000` -- which is half the chunk space, and exactly the
/// half a small test corpus is least likely to contain. SSE4.1's unsigned
/// min/max give the comparison back without a bias trick:
/// `v >= s` is `max_epu16( v, s ) == v`, and `v <= e` is `min_epu16( v, e ) == v`.
///
/// Everything else is the NEON arm's shape, bound for bound, including B3.
///
/// # No crate-level speedup is claimed, and the first attempt measured the
/// wrong path
///
/// An Intel i9-9880H run on 2026-09-18 found that disabling this arm moved
/// `and_cardinality( array, run )` by `1.00x`, and that was briefly written up
/// as the kernel win vanishing into the container path. **It is not. The arm is
/// not on that path at all.** `ops::card`'s array x run case is a scalar
/// two-pointer written in `card.rs`; `filter_by_run` is reached only through
/// [`merge_array_run`], which returns a `Vec<u16>` and therefore serves
/// **apply**, not cardinality.
///
/// Two gates stand before this arm runs, and any measurement has to clear both:
/// the operation must be the container-producing one, and `vals / nruns` must
/// be **below** `RUN_SEEK_RATIO` or the seek path answers it instead. A probe
/// that clears neither reports `1.00x` and looks like a finding.
///
/// So this arm's crate-level value is **unmeasured**, not measured-as-zero. It
/// is kept because it is correct, carries B3 exactly as NEON does, and costs
/// nothing when the dispatcher skips it.
///
/// A number for `array x run` *was* produced and is deliberately not recorded
/// for a second reason: it moved by a fifth depending on which **unrelated**
/// module had been rebuilt, so it measures code layout rather than this arm.
/// See `.agents/docs/LTM/simd-arch-arms-and-kernel-selection.md`.
#[cfg(target_arch = "x86_64")]
mod simd {
    use crate::ops::array::SHUFFLE;
    use core::arch::x86_64::*;

    /// Lanes per block. The same 8 as `ops::array`, and for the same reason:
    /// `u16` lanes in a 128-bit register.
    pub(super) const BLOCK: usize = 8;

    /// `vals` filtered by membership of the run, into `out`.
    ///
    /// `flat` is the run's `( start, len )` pairs, so interval `k` is
    /// `[ flat[2k], flat[2k] + flat[2k+1] ]`.
    ///
    /// # Safety
    ///
    /// Requires `sse4.1` and `ssse3`, which the caller establishes. `out` must
    /// be empty with capacity at least `vals.len() + BLOCK` -- bound **B3**,
    /// exactly as the NEON arm states it.
    #[target_feature(enable = "sse4.1,ssse3")]
    pub(super) unsafe fn filter_by_run(
        vals: &[u16],
        flat: &[u16],
        in_run: bool,
        out: &mut Vec<u16>,
    ) {
        debug_assert!(out.is_empty() && out.capacity() >= vals.len() + BLOCK, "B3");
        let n = vals.len();
        let m = flat.len() / 2;
        let (mut i, mut j, mut w) = (0usize, 0usize, 0usize);
        // SAFETY: B3, and the two index bounds the loops maintain -- identical
        // to the NEON arm's, which states them at length.
        //
        // - Every `get_unchecked` on `vals` is under `i + BLOCK <= n`, re-tested
        //   at the top of each iteration and never advanced between the test and
        //   the reads; the tail loop indexes `i..n` directly.
        // - Every `get_unchecked` on `flat` is under `j < m` or `k < m`, where
        //   `m = flat.len() / 2`, so `2k + 1 <= 2m - 1 < flat.len()`.
        // - The store writes a whole 16-byte register at `p.add( w )`. `w` only
        //   ever advances by the popcount of an 8-bit mask, so `w <= i <= n`,
        //   and B3 reserved `n + BLOCK`.
        unsafe {
            let p = out.as_mut_ptr();
            while i + BLOCK <= n {
                let first = *vals.get_unchecked(i);
                let last = *vals.get_unchecked(i + BLOCK - 1);
                // Intervals ending below the block cannot matter again, and `j`
                // only advances, which keeps the whole walk linear in `m`.
                while j < m && *flat.get_unchecked(j * 2) + *flat.get_unchecked(j * 2 + 1) < first {
                    j += 1;
                }
                let v = _mm_loadu_si128(vals.as_ptr().add(i).cast());
                let mut hit = _mm_setzero_si128();
                let mut k = j;
                while k < m && *flat.get_unchecked(k * 2) <= last {
                    let s = *flat.get_unchecked(k * 2);
                    let e = s + *flat.get_unchecked(k * 2 + 1);
                    let vs = _mm_set1_epi16(s as i16);
                    let ve = _mm_set1_epi16(e as i16);
                    // Unsigned, via min/max: see the module note above.
                    let ge = _mm_cmpeq_epi16(_mm_max_epu16(v, vs), v);
                    let le = _mm_cmpeq_epi16(_mm_min_epu16(v, ve), v);
                    hit = _mm_or_si128(hit, _mm_and_si128(ge, le));
                    if e >= last {
                        break;
                    }
                    k += 1;
                }
                // A hit lane is `0xFFFF`, so `packs` saturates it to `0xFF` and
                // the low byte of the byte mask is the selector.
                let mut bits = (_mm_movemask_epi8(_mm_packs_epi16(hit, hit)) as usize) & 0xFF;
                if !in_run {
                    bits = !bits & 0xFF;
                }
                let shuf = _mm_loadu_si128(SHUFFLE[bits].as_ptr().cast());
                // B3: `w <= i` always, so `w + BLOCK <= n + BLOCK <= capacity`.
                _mm_storeu_si128(p.add(w).cast(), _mm_shuffle_epi8(v, shuf));
                w += (bits as u32).count_ones() as usize;
                i += BLOCK;
            }
            // Fewer than a block remains; finish it the scalar way.
            for idx in i..n {
                let val = *vals.get_unchecked(idx);
                while j < m && *flat.get_unchecked(j * 2) + *flat.get_unchecked(j * 2 + 1) < val {
                    j += 1;
                }
                if (j < m && *flat.get_unchecked(j * 2) <= val) == in_run {
                    *p.add(w) = val;
                    w += 1;
                }
            }
            out.set_len(w);
        }
    }
}

/// Values per interval at which seeking each interval beats compacting every
/// block.
///
/// **Not `GALLOP_RATIO`, and the difference is measured, not stylistic.** That
/// constant is 32 and governs array x array, where the probe is one binary
/// search per *element*. Here the probe is two searches plus an
/// `extend_from_slice` per *interval*, so the fixed cost lands on a different
/// quantity and the crossover is elsewhere. Named separately for the reason
/// `INTERSECT_BLOCK` is named apart from `PREDICATE_BLOCK`: two decisions about
/// two loops, not one constant with two callers.
///
/// # It was 64, and the compaction arm invalidated that
///
/// **A threshold is a claim about the alternative, so replacing the
/// alternative rebuts it.** 64 was correct against the *scalar merge*, measured
/// by forcing each path ( at `GALLOP_RATIO`'s own 32 the seek path was 1.7x
/// **worse** than that merge, so reusing it would have shipped a regression ).
/// Then `simd::filter_by_run` replaced the merge, the losing side got roughly
/// three times faster, and the crossing point moved with it — leaving 64 wrong
/// in the other direction, taking the seek path where compaction is 1.8x better.
///
/// Re-measured 2026-09-07 at m = 4000 by forcing each path, `roaring` alongside
/// at ~1.7 us throughout:
///
/// ```text
/// nruns   vals/interval     seek   compact   winner
///     1            4000    764 ns   2118 ns   seek     2.8x
///     2            2000    335 ns    914 ns   seek     2.7x
///     4            1000    424 ns    922 ns   seek     2.2x
///     8             500    617 ns    961 ns   seek     1.6x
///    16             250   1002 ns    986 ns   wash
///    32             125   1779 ns    983 ns   COMPACT  1.8x
///    64            62.5   3311 ns   1054 ns   COMPACT  3.1x
/// ```
///
/// Do not re-tune this against the scalar merge; it is no longer what runs.
/// And if a faster seek or a wider compaction lands, this number moves again
/// — it belongs to the pair, not to either side.
const RUN_SEEK_RATIO: usize = 256;

#[inline]
fn merge_array_run(vals: &[u16], rn: &crate::container::RunContainer, in_run: bool) -> Vec<u16> {
    let m = rn.nruns();
    let mut out = Vec::with_capacity(vals.len());

    // **Few intervals: seek each one's span and copy it whole.** The merge
    // below is linear in `vals` and pays a test per *element*; when there are
    // far fewer intervals than values, the same answer is a handful of binary
    // searches and a `memcpy` each. Same gallop-versus-merge decision, and the
    // same `GALLOP_RATIO`, that `ops::array` already applies to array x array —
    // this pair simply never made it.
    //
    // Measured at m = 4000 against one interval: the element loop was **61%**
    // of `ops::apply`'s whole cost, against 7% for building and optimizing the
    // resulting container. So this is where the pair's time was.
    //
    // `And` only. `AndNot` keeps the values *between* intervals, which is a
    // walk over the gaps rather than the spans, and it is not the same routine
    // with a flag flipped.
    if in_run && m > 0 && vals.len() / m as usize >= RUN_SEEK_RATIO {
        let mut lo = 0usize;
        for k in 0..m {
            let (s, e) = (rn.start(k), rn.end(k));
            // Both searches run on the remaining tail, so `lo` only advances and
            // the whole loop stays `O( nruns * log n )` rather than restarting.
            lo += vals[lo..].partition_point(|&v| v < s);
            let hi = lo + vals[lo..].partition_point(|&v| v <= e);
            out.extend_from_slice(&vals[lo..hi]);
            lo = hi;
            if lo == vals.len() {
                break;
            }
        }
        return out;
    }

    // Many intervals: no seek can help, and the element loop really is the work.
    // Decide eight values at a time instead.
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("neon") {
        // B3 reserves one block past the largest possible result.
        let mut wide = Vec::with_capacity(vals.len() + simd::BLOCK);
        // SAFETY: `neon` was just detected, and `wide` is empty with the
        // capacity B3 requires.
        unsafe { simd::filter_by_run(vals, rn.as_flat(), in_run, &mut wide) };
        return wide;
    }
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("sse4.1") {
        // B3 reserves one block past the largest possible result.
        let mut wide = Vec::with_capacity(vals.len() + simd::BLOCK);
        // SAFETY: `sse4.1` was just detected ( and implies `ssse3` on every
        // CPU that reports it ), and `wide` is empty with the capacity B3
        // requires.
        unsafe { simd::filter_by_run(vals, rn.as_flat(), in_run, &mut wide) };
        return wide;
    }

    let mut j = 0u32;
    for &v in vals {
        // `j` only ever advances, which is what makes this linear.
        while j < m && rn.end(j) < v {
            j += 1;
        }
        if (j < m && rn.start(j) <= v) == in_run {
            out.push(v);
        }
    }
    out
}

/// Apply `op` when the pair is array×run in either order, else `None`.
///
/// The last pair still routing through the value-merging kernel, measured at
/// **46x slower than the `roaring` crate**. `RunContainer::contains` is a binary
/// search over intervals, so intersection costs `card * log nruns` rather than
/// the run's cardinality — which is the whole point of an interval encoding and
/// exactly what merging values threw away.
///
/// `And` and lhs-`AndNot` then go through `merge_array_run` rather than
/// probing per value, which took AND from ~5x the reference to 2.07x. `Or` and
/// `Xor` still expand the run into words: producing them as intervals would
/// need split/merge on the result, and keeping that out of these paths is a
/// deliberate v1 simplification, not an oversight. `Or` measures 5.0x against
/// the reference and is the arm to revisit if this pair ever matters more.
#[inline]
pub fn try_apply_array_run(op: SetOp, a: &Container, b: &Container) -> Option<Option<Container>> {
    let (arr, rn, array_is_lhs) = match (a, b) {
        (Container::Array(x), Container::Run(y)) => (x, y, true),
        (Container::Run(y), Container::Array(x)) => (x, y, false),
        _ => return None,
    };
    let vals = arr.as_slice();

    // Subsets of the array stay arrays, touching neither a bitmap's memory nor
    // the run's cardinality.
    match op {
        SetOp::And => return Some(finish_array(merge_array_run(vals, rn, true))),
        SetOp::AndNot if array_is_lhs => {
            return Some(finish_array(merge_array_run(vals, rn, false)))
        }
        _ => {}
    }

    // The rest need the run expanded, so build in words and let the result
    // re-encode itself.
    let mut out = vec![0u64; BITMAP_WORDS];
    for i in 0..rn.nruns() {
        for_each_masked_word(rn.start(i), rn.end(i), |w, m| out[w] |= m);
    }
    match op {
        SetOp::Or => {
            for &v in vals {
                out[v as usize >> 6] |= 1u64 << (v & 63);
            }
        }
        SetOp::Xor => {
            for &v in vals {
                out[v as usize >> 6] ^= 1u64 << (v & 63);
            }
        }
        SetOp::AndNot => {
            // run \ array.
            for &v in vals {
                out[v as usize >> 6] &= !(1u64 << (v & 63));
            }
        }
        SetOp::And => unreachable!("handled above"),
    }

    let len = crate::ops::bitmap::words_popcount(&out);
    if len == 0 {
        return Some(None);
    }
    let mut c = Container::Bitmap(BitmapContainer::from_words(out, len));
    // A union of runs is usually still run-shaped; `optimize` picks the encoding
    // rather than leaving an 8 KiB bitmap holding a handful of intervals.
    c.optimize();
    Some(Some(c))
}

/// Wrap a filtered value list, re-encoding if it is no longer array-shaped.
fn finish_array(vals: Vec<u16>) -> Option<Container> {
    if vals.is_empty() {
        return None;
    }
    let mut c = Container::from_sorted(&vals);
    c.optimize();
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::RunContainer;
    use crate::ops::generic;

    fn bm(vals: &[u16]) -> Container {
        Container::Bitmap(BitmapContainer::from_sorted(vals))
    }
    /// `(start, end)` inclusive.
    fn run(pairs: &[(u16, u16)]) -> Container {
        Container::Run(RunContainer::from_pairs(pairs))
    }

    const OPS: [SetOp; 4] = [SetOp::And, SetOp::Or, SetOp::Xor, SetOp::AndNot];

    /// The three interval kernels against [`for_each_masked_word`], which is
    /// the form they replaced and is therefore their oracle.
    ///
    /// The boundaries are enumerated rather than sampled. Every one of the
    /// three computes its end mask as `!0 >> (63 - eb)` where the closure
    /// branched on `eb == 63`, and re-derives the single-word case separately
    /// from the multi-word one — so the cases that can break them are exactly
    /// `s` and `e` at word boundaries, at the ends of the chunk, and inside the
    /// same word versus across two versus across many. A uniform `(s, e)` pair
    /// lands on none of those.
    #[test]
    fn the_interval_kernels_agree_with_the_masked_word_walk() {
        // A payload with a bit pattern that is neither all-set nor all-clear at
        // any word boundary, plus deliberately saturated and empty words so
        // `masked_covers` can answer both ways.
        let mut mixed = vec![0u64; BITMAP_WORDS];
        let mut s = 0x1234_5678_9ABC_DEF1u64;
        for (i, w) in mixed.iter_mut().enumerate() {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            *w = match i % 7 {
                0 => !0u64,
                1 => 0,
                _ => s,
            };
        }
        // **The second payload is not redundant, and it was added because a
        // sabotage survived without it.** `masked_covers` tests head, then
        // tail, then the whole-word middle, and it short-circuits — so on the
        // payload above the head test almost always answers first and the
        // middle `all(|w| *w == !0)` is never reached with a word that is
        // non-zero but not saturated. Weakening that comparison to `*w != 0`
        // left every test green. Here the ends are saturated and only word 100
        // is partial, so the middle is reachable and the distinction is live.
        let mut nearly_full = vec![!0u64; BITMAP_WORDS];
        nearly_full[100] = !0u64 ^ 1;
        nearly_full[200] = 0;
        nearly_full[300] = 1;
        // **The third payload is not redundant either, and it was also added
        // because a sabotage survived.** `masked_intersects` has to apply the
        // head and tail *masks*, not merely test the words: dropping the head
        // mask ( `bw[sw] & head != 0` -> `bw[sw] != 0` ) is only visible when
        // the head word carries bits **outside** the interval and nothing
        // inside it. Word 100 holds bits 6400..6408 and word 300 holds bits
        // 19256..19264, and the edge list below names intervals that start
        // after the first and end before the second.
        let mut edge_only = vec![0u64; BITMAP_WORDS];
        edge_only[100] = 0xFF;
        edge_only[300] = 0xFF << 56;

        let edges: Vec<u32> = vec![
            0, 1, 62, 63, 64, 65, 127, 128, 129, 191, 192, 447, 448, 449, 3200, 6399, 6400, 6407,
            6408, 6463, 6464, 9600, 12799, 12800, 19100, 19199, 19200, 19250, 19255, 19256, 19263,
            65471, 65472, 65534, 65535,
        ];
        let mut checked = 0usize;
        let mut covers_said_yes = 0usize;
        let mut intersects_said_no = 0usize;
        for bw in [&mixed, &nearly_full, &edge_only] {
            for &a in &edges {
                for &b in &edges {
                    if a > b {
                        continue;
                    }
                    let (lo, hi) = (a as u16, b as u16);
                    // The oracle: the closure walk, summing / testing per word.
                    let (mut want_n, mut want_any, mut want_all) = (0u32, false, true);
                    for_each_masked_word(lo, hi, |w, m| {
                        want_n += (bw[w] & m).count_ones();
                        want_any |= bw[w] & m != 0;
                        want_all &= bw[w] & m == m;
                    });
                    assert_eq!(
                        masked_and_popcount(bw, lo, hi),
                        want_n,
                        "popcount over [{lo}, {hi}]"
                    );
                    assert_eq!(
                        masked_intersects(bw, lo, hi),
                        want_any,
                        "intersects over [{lo}, {hi}]"
                    );
                    assert_eq!(
                        masked_covers(bw, lo, hi),
                        want_all,
                        "covers over [{lo}, {hi}]"
                    );
                    covers_said_yes += usize::from(want_all);
                    intersects_said_no += usize::from(!want_any);
                    checked += 1;
                }
            }
        }
        // Guards against the `a > b` filter silently skipping everything, and
        // against a corpus on which a predicate only ever answers one way —
        // which is how both middle-word comparisons stopped being tested.
        assert!(checked > 400, "only {checked} intervals exercised");
        assert!(
            covers_said_yes > 50,
            "only {covers_said_yes} containments held; the covering branch is barely exercised"
        );
        assert!(
            intersects_said_no > 50,
            "only {intersects_said_no} intervals missed; the empty branch is barely exercised"
        );
    }

    proptest::proptest! {
        /// The same three, on random intervals over a random payload.
        ///
        /// The enumerated test above covers the boundaries; this covers the
        /// interiors, and in particular intervals whose whole-word middle is
        /// long enough that the slice sum and the closure loop can disagree.
        #[test]
        fn the_interval_kernels_agree_on_random_intervals(
            seed in proptest::prelude::any::<u64>(),
            a in 0u16..=65535,
            b in 0u16..=65535,
            fill in 0u8..4,
        ) {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            let mut s = seed | 1;
            let bw: Vec<u64> = (0..BITMAP_WORDS)
                .map(|_| {
                    s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
                    match fill {
                        0 => !0u64,
                        1 => s,
                        2 => s & (s >> 13),
                        // Nearly saturated: a containment can hold across many
                        // words and still fail on one partial word in the
                        // middle, which is the only shape that distinguishes
                        // `masked_covers` from "the middle is non-empty".
                        _ => if s % 32 == 0 { s | (s << 1) } else { !0u64 },
                    }
                })
                .collect();
            let (mut want_n, mut want_any, mut want_all) = (0u32, false, true);
            for_each_masked_word(lo, hi, |w, m| {
                want_n += (bw[w] & m).count_ones();
                want_any |= bw[w] & m != 0;
                want_all &= bw[w] & m == m;
            });
            proptest::prop_assert_eq!(masked_and_popcount(&bw, lo, hi), want_n);
            proptest::prop_assert_eq!(masked_intersects(&bw, lo, hi), want_any);
            proptest::prop_assert_eq!(masked_covers(&bw, lo, hi), want_all);
        }
    }

    /// Every arm, in **both operand orders**, must match the oracle.
    ///
    /// Order matters: `AndNot` is asymmetric, and it is the arm where a kernel
    /// that quietly normalized the pair would return the wrong set rather than
    /// failing.
    #[test]
    fn every_arm_agrees_with_the_generic_kernel_in_both_orders() {
        let bitmaps: Vec<Vec<u16>> = vec![
            (0..5000u16).map(|i| i * 2).collect(),
            (0..300u16).collect(),
            vec![0, 1, 65534, 65535],
            (1000..2000u16).collect(),
        ];
        let runs: Vec<Vec<(u16, u16)>> = vec![
            vec![(0, 100)],
            vec![(500, 1500)],
            vec![(0, 65535)],
            vec![(0, 0), (65535, 65535)],
            vec![(10, 20), (30, 40), (5000, 6000)],
            vec![(2001, 3000)],
        ];

        for bv in &bitmaps {
            for rv in &runs {
                let (b, r) = (bm(bv), run(rv));
                for op in OPS {
                    for (x, y) in [(&b, &r), (&r, &b)] {
                        let fast = try_apply(op, x, y).expect("bitmap x run");
                        let slow = generic::apply(op, x, y);
                        let f: Vec<u16> = fast
                            .as_ref()
                            .map(|c| c.iter().collect())
                            .unwrap_or_default();
                        let s: Vec<u16> = slow
                            .as_ref()
                            .map(|c| c.iter().collect())
                            .unwrap_or_default();
                        assert_eq!(
                            f,
                            s,
                            "{op:?} disagrees on run {rv:?} ( lhs is {} )",
                            if matches!(x, Container::Bitmap(_)) {
                                "bitmap"
                            } else {
                                "run"
                            }
                        );
                        assert_eq!(
                            fast.as_ref().map(|c| c.len()),
                            slow.as_ref().map(|c| c.len()),
                            "{op:?}: cardinality disagrees on run {rv:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn results_are_structurally_valid() {
        let b = bm(&(0..4000u16).map(|i| i * 3).collect::<Vec<_>>());
        let r = run(&[(0, 500), (600, 700), (20000, 30000)]);
        for op in OPS {
            for (x, y) in [(&b, &r), (&r, &b)] {
                if let Some(Some(c)) = try_apply(op, x, y) {
                    crate::container::codec::validate(&c)
                        .unwrap_or_else(|e| panic!("{op:?} produced an invalid container: {e:?}"));
                }
            }
        }
    }

    #[test]
    fn an_empty_result_is_none_not_an_empty_container() {
        let b = bm(&(0..100u16).collect::<Vec<_>>());
        let r = run(&[(0, 99)]);
        assert_eq!(try_apply(SetOp::AndNot, &b, &r), Some(None));
        assert_eq!(try_apply(SetOp::Xor, &b, &r), Some(None));
        let far = run(&[(50000, 50100)]);
        assert_eq!(try_apply(SetOp::And, &b, &far), Some(None));
    }

    /// The array×bitmap arm, in both orders, against the oracle.
    ///
    /// Order is what makes this worth its own test: `AndNot` returns an *array*
    /// one way round and a *bitmap* the other, so a kernel that normalized the
    /// pair would silently return the complement.
    #[test]
    fn array_bitmap_agrees_with_the_generic_kernel_in_both_orders() {
        let arrays: Vec<Vec<u16>> = vec![
            vec![0],
            vec![0, 65535],
            (0..500u16).map(|i| i * 100).collect(),
            (0..4096u16).map(|i| i * 16).collect(),
            (1000..1100u16).collect(),
        ];
        let bitmaps: Vec<Vec<u16>> = vec![
            (0..5000u16).map(|i| i * 2).collect(),
            (0..300u16).collect(),
            vec![0, 1, 65534, 65535],
            (0..65535u16).collect(),
        ];

        for av in &arrays {
            for bv in &bitmaps {
                let (a, b) = (Container::from_sorted(av), bm(bv));
                assert_eq!(
                    a.kind(),
                    crate::ContainerKind::Array,
                    "want an array operand"
                );
                for op in OPS {
                    for (x, y) in [(&a, &b), (&b, &a)] {
                        let fast = try_apply_array_bitmap(op, x, y).expect("array x bitmap");
                        let slow = generic::apply(op, x, y);
                        let f: Vec<u16> = fast
                            .as_ref()
                            .map(|c| c.iter().collect())
                            .unwrap_or_default();
                        let s: Vec<u16> = slow
                            .as_ref()
                            .map(|c| c.iter().collect())
                            .unwrap_or_default();
                        assert_eq!(
                            f,
                            s,
                            "{op:?} disagrees ( lhs is {} )",
                            if matches!(x, Container::Array(_)) {
                                "array"
                            } else {
                                "bitmap"
                            }
                        );
                        assert_eq!(
                            fast.as_ref().map(|c| c.len()),
                            slow.as_ref().map(|c| c.len()),
                            "{op:?}: cardinality disagrees"
                        );
                        if let Some(c) = fast.as_ref() {
                            crate::container::codec::validate(c).unwrap();
                        }
                    }
                }
            }
        }
    }

    /// The array×run arm, both orders, against the oracle.
    /// The compaction arm, called directly, against the scalar merge.
    ///
    /// Direct rather than through `merge_array_run`, and the feature
    /// assertion is the point: on a host reporting `neon` absent the dispatcher
    /// would run the scalar path and every comparison here would be
    /// `scalar == scalar`.
    ///
    /// **The shapes that decide it are block boundaries and interval
    /// density.** A block whose eight values straddle several intervals, a block
    /// entirely inside one, a block entirely outside, intervals narrower than a
    /// block, and a scalar tail shorter than eight — plus `AndNot`, which is the
    /// same mask inverted and is only correct because the interval scan is
    /// complete for the block.
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn the_compaction_arm_agrees_with_the_scalar_merge() {
        fn scalar(vals: &[u16], flat: &[u16], in_run: bool) -> Vec<u16> {
            let m = flat.len() / 2;
            let mut out = Vec::new();
            let mut j = 0usize;
            for &v in vals {
                while j < m && flat[j * 2] + flat[j * 2 + 1] < v {
                    j += 1;
                }
                if (j < m && flat[j * 2] <= v) == in_run {
                    out.push(v);
                }
            }
            out
        }
        // The arm this test calls directly must actually be reachable, or the
        // test degrades into comparing the scalar walk against itself.
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

        // ( vals, flat as (start, len) pairs )
        let mut cases: Vec<(Vec<u16>, Vec<u16>)> = vec![
            (Vec::new(), vec![0, 5]),
            ((0..40u16).collect(), Vec::new()),
            ((0..40u16).collect(), vec![0, 39]),
            ((0..40u16).collect(), vec![100, 5]),
        ];
        // Intervals narrower than a block, so one block touches several.
        cases.push((
            (0..64u16).map(|v| v * 2).collect(),
            (0..16u16).flat_map(|k| [k * 8, 1]).collect(),
        ));
        // One wide interval covering part of the array.
        cases.push(((0..200u16).map(|v| v * 3).collect(), vec![100, 200]));
        // Tails of every length either side of a block.
        for n in 1..20u16 {
            cases.push(((0..n).map(|v| v * 5).collect(), vec![7, 20]));
        }
        // Interval edges landing exactly on lane boundaries.
        let vals: Vec<u16> = (0..64u16).collect();
        for s in 0..16u16 {
            for w in 0..4u16 {
                cases.push((vals.clone(), vec![s * 4, w]));
            }
        }
        for (vals, flat) in &cases {
            for in_run in [true, false] {
                let want = scalar(vals, flat, in_run);
                let mut got = Vec::with_capacity(vals.len() + simd::BLOCK);
                // SAFETY: `neon` asserted above; `got` is empty with B3 capacity.
                unsafe { simd::filter_by_run(vals, flat, in_run, &mut got) };
                assert_eq!(got, want, "in_run={in_run} vals={vals:?} flat={flat:?}");
                // B3: the store writes a whole register, so the reservation must
                // still hold afterwards or it was never large enough.
                assert!(got.len() <= vals.len(), "B3 reservation exceeded");
            }
        }
    }

    #[test]
    fn array_run_agrees_with_the_generic_kernel_in_both_orders() {
        let arrays: Vec<Vec<u16>> = vec![
            vec![0],
            vec![0, 65535],
            (0..500u16).map(|i| i * 100).collect(),
            (95..205u16).collect(),
            (0..4000u16).map(|i| i * 16).collect(),
        ];
        let runs: Vec<Vec<(u16, u16)>> = vec![
            vec![(0, 100)],
            vec![(100, 200)],
            vec![(0, 65535)],
            vec![(0, 0), (65535, 65535)],
            vec![(10, 20), (30, 40), (5000, 6000)],
        ];

        for av in &arrays {
            for rv in &runs {
                let (a, r) = (Container::from_sorted(av), run(rv));
                assert_eq!(
                    a.kind(),
                    crate::ContainerKind::Array,
                    "want an array operand"
                );
                for op in OPS {
                    for (x, y) in [(&a, &r), (&r, &a)] {
                        let fast = try_apply_array_run(op, x, y).expect("array x run");
                        let slow = generic::apply(op, x, y);
                        let f: Vec<u16> = fast
                            .as_ref()
                            .map(|c| c.iter().collect())
                            .unwrap_or_default();
                        let s: Vec<u16> = slow
                            .as_ref()
                            .map(|c| c.iter().collect())
                            .unwrap_or_default();
                        assert_eq!(
                            f,
                            s,
                            "{op:?} disagrees on run {rv:?} ( lhs is {} )",
                            if matches!(x, Container::Array(_)) {
                                "array"
                            } else {
                                "run"
                            }
                        );
                        if let Some(c) = fast.as_ref() {
                            crate::container::codec::validate(c).unwrap();
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn other_pairs_fall_through() {
        let b = bm(&(0..100u16).collect::<Vec<_>>());
        let arr = Container::from_sorted(&[1, 2, 3]);
        assert!(try_apply(SetOp::And, &b, &b).is_none());
        assert!(try_apply(SetOp::And, &arr, &b).is_none());
        assert!(try_apply_array_bitmap(SetOp::And, &b, &b).is_none());
        assert!(try_apply_array_bitmap(SetOp::And, &arr, &arr).is_none());
        assert!(try_apply_array_run(SetOp::And, &arr, &b).is_none());
    }
}
