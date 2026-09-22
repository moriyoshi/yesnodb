//! Multiplication: the schoolbook kernel, and Karatsuba above a measured
//! crossover.
//!
//! # Schoolbook is the oracle
//!
//! `O(n*m)`, operand-scanning, correct for every pair of operands. It is never
//! deleted when a faster arm lands, and the dispatch never becomes unable to
//! reach it — same contract as [`ops::generic`](crate::ops::generic) and
//! [`matrix::read`](crate::matrix). QG §4 makes that a requirement: a specialized
//! arm is admitted only with a benchmark, and the arm it is differential-tested
//! against has to still exist.
//!
//! # Karatsuba, additive rather than subtractive
//!
//! Split both operands at `h` limbs. With `a = a1*B^h + a0` and
//! `b = b1*B^h + b0`,
//!
//! ```text
//!   z0 = a0*b0
//!   z2 = a1*b1
//!   z1 = (a0+a1)*(b0+b1) - z0 - z2
//!   r  = z2*B^2h + z1*B^h + z0
//! ```
//!
//! Three multiplications instead of four, so `O(n^1.585)`.
//!
//! **The additive form is chosen because this module is unsigned by
//! contract.** The subtractive variant computes `|a0-a1| * |b0-b1|` and needs a
//! sign flag to recombine; the additive middle term is
//! `z0 + z2 + (a0*b1 + a1*b0)` minus `z0` and `z2`, which is provably
//! non-negative, so no intermediate anywhere is signed. The cost is one extra
//! carry limb on each sum, which the scratch layout accounts for.
//!
//! **Strongly unbalanced operands do not recurse.** With `n > 2m` the balanced
//! split leaves `b1` empty, and the formula degenerates to computing
//! `(a0+a1)*b0` — strictly *more* work than the schoolbook it replaced. Such a
//! product is chopped into blocks of `m` limbs instead, so a 1 x 4096 multiply
//! stays `O(n)`. `an_unbalanced_product_does_not_allocate_per_block` pins it.
//!
//! # One scratch allocation, not one per recursion node
//!
//! The recursion tree has `3^log2(n)` nodes, so allocating per node is
//! `Theta(n^1.585)` allocations for one product — the same shape `ops::nary`
//! rejects for k-way union ( "one accumulator, not k-1 intermediates" ). Instead
//! [`scratch_needed`] sizes a single buffer once and every level takes its
//! working space from the front, passing the tail down.
//!
//! The buffer is allocated **after** the dispatch, so the schoolbook path
//! still allocates exactly once. `ops::nary`'s header records that allocating its
//! accumulator up front made the common case slower, and that is the same trap.

use super::addsub::{adc, sbb};
use super::BigUint;

/// Limb count at or above which [`BigUint::mul`] switches to Karatsuba, measured
/// against schoolbook on the same operands.
///
/// # How this was fixed
///
/// **Not by a bench-local comparison.** The in-crate arms are `pub(crate)`, so
/// `benches/` cannot call them, and `benches/setops.rs` records that a
/// bench-local copy measured 7-21% faster than the identical source. Instead this
/// value was measured by timing the **real** [`BigUint::mul`] twice, from two
/// builds of the same source with this constant patched high ( every product
/// schoolbook ) and low ( every product Karatsuba ). Both curves then come from
/// the same compilation of the same code and the bias does not exist rather than
/// merely cancelling. The harness and the full sweep are in `JOURNAL.md`.
///
/// Measured 2026-08-29, time relative to pure schoolbook, balanced operands:
///
/// ```text
///   limbs    18     20     22     24     28     40     64    128
///   ratio  1.05   0.98   0.97   0.90   0.88   0.78   0.66   0.49
/// ```
///
/// So the arm **loses** 5% at 18 limbs, breaks even at 20, and is ahead
/// thereafter.
///
/// **The 20-to-22 numbers are within this harness's noise and should not be
/// read as locating the crossover to the limb.** In the same sweep two
/// configurations that both dispatch to schoolbook at 16 limbs differed by 0.7%,
/// and a coarser earlier run put two identical schoolbook configurations 2x
/// apart at 8 limbs. What the data supports is "somewhere around 20", not "20".
///
/// **What actually decides 20 over 24 is a mechanism, not a tie-break.** At 40
/// limbs `T = 24` measures 0.831 against `T = 20`'s 0.779 — 6%, well clear of the
/// noise — because at `T = 24` a 40-limb product splits to 20 and stops, where at
/// `T = 20` it takes one more level. The crossover and the best threshold are not
/// the same question, and only sweeping both answered it.
///
/// **A wrong value here is a performance bug and can never be a correctness
/// bug**, because both arms compute the same function and
/// `every_multiplication_arm_agrees_with_the_schoolbook_oracle` says so on the
/// same operands. That makes re-measuring on another machine a zero-risk change,
/// and it is the difference between this constant and `ARRAY_MAX`, where a wrong
/// value violates a payload-size invariant.
///
/// **No hysteresis, unlike [`BITMAP_DEMOTE`](crate::BITMAP_DEMOTE).** That
/// constant's 512-value gap exists because a container's encoding is *persistent
/// state* that would otherwise flip on every commit. A multiply arm is chosen
/// from the operand sizes, stores nothing, and is recomputed per call, so there
/// is no state to oscillate. The shape invites a reader to add a gap it does not
/// need.
pub const KARATSUBA_MIN: usize = 20;

/// Below four limbs the recursion does not shrink: `sa = a0 + a1` has
/// `ceil(n/2) + 1` limbs, which is `n` again at `n = 3`. Any threshold below this
/// recurses forever.
const KARATSUBA_FLOOR: usize = 4;

// Checked at compile time rather than by a test: both operands are constants, so
// a runtime assertion is one clippy calls out as always-true and a reader cannot
// distinguish from a real check. Dropping `KARATSUBA_MIN` below the floor stops
// the build instead of hanging the suite.
const _: () = assert!(KARATSUBA_MIN >= KARATSUBA_FLOOR);

/// Which kernel a product of these limb counts takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MulArm {
    Schoolbook,
    Karatsuba,
}

/// Keyed on the **shorter** operand: Karatsuba's saving comes from the balanced
/// split, and a product whose short side is tiny has none to make.
///
/// A pure predicate, so the dispatch is testable without instrumenting the
/// kernel — the role [`BitMatrix::transpose_prefers_scatter`](crate::matrix::BitMatrix::transpose_prefers_scatter)
/// plays for its own arm choice.
pub(crate) fn mul_arm(len_a: usize, len_b: usize, min: usize) -> MulArm {
    if len_a.min(len_b) < min {
        MulArm::Schoolbook
    } else {
        MulArm::Karatsuba
    }
}

/// `acc + a*b + carry`, returning ( low, high ).
///
/// **It cannot overflow, and the margin is exactly zero**:
/// `(2^64-1)^2 + 2*(2^64-1) == 2^128 - 1`. That is why the four-input form
/// exists rather than a bare widening multiply followed by two [`adc`]s — the
/// accumulator and the carry both fit in the same `u128` for free, and pinning
/// the identity is one test.
///
/// `u64::carrying_mul` and `u64::widening_mul` are unstable and the crate's
/// MSRV is 1.95, so this is the `u128` spelling. Keep the casts **inline at the
/// multiply**: LLVM recognizes `(a as u128) * (b as u128)` as a 64x64 widening
/// multiply and emits `mulx` / `umulh`, and hoisting a cast into a variable used
/// twice can defeat the pattern match and produce a call to `__multi3`.
#[inline]
pub(crate) fn mac(acc: u64, a: u64, b: u64, carry: u64) -> (u64, u64) {
    let t = (a as u128) * (b as u128) + acc as u128 + carry as u128;
    (t as u64, (t >> 64) as u64)
}

/// `acc += x`, returning the carry out of `acc`'s top limb.
fn add_assign_slice(acc: &mut [u64], x: &[u64]) -> u64 {
    debug_assert!(acc.len() >= x.len());
    let mut carry = 0u64;
    for (i, a) in acc.iter_mut().enumerate() {
        if i >= x.len() && carry == 0 {
            return 0;
        }
        let b = x.get(i).copied().unwrap_or(0);
        let (s, c) = adc(*a, b, carry);
        *a = s;
        carry = c;
    }
    carry
}

/// `acc -= x`, returning the borrow out of `acc`'s top limb.
fn sub_assign_slice(acc: &mut [u64], x: &[u64]) -> u64 {
    debug_assert!(acc.len() >= x.len());
    let mut borrow = 0u64;
    for (i, a) in acc.iter_mut().enumerate() {
        if i >= x.len() && borrow == 0 {
            return 0;
        }
        let b = x.get(i).copied().unwrap_or(0);
        let (d, bo) = sbb(*a, b, borrow);
        *a = d;
        borrow = bo;
    }
    borrow
}

/// Trailing zero limbs carry no value and only inflate the arm dispatch, so the
/// recursion trims before deciding.
fn trim(x: &[u64]) -> &[u64] {
    let mut n = x.len();
    while n > 0 && x[n - 1] == 0 {
        n -= 1;
    }
    &x[..n]
}

/// `out += a*b`, schoolbook. `out` must be **zeroed** and at least
/// `a.len() + b.len()` long.
///
/// Row `j`'s carry is **assigned** to `out[j + a.len()]`, not added, and that
/// is correct rather than lucky: row `j` writes `out[j ..= j + a.len() - 1]` and
/// row `j-1` put its own carry at `out[j + a.len() - 1]`, which row `j` reads as
/// an accumulator input. So `out[j + a.len()]` is still zero when row `j` reaches
/// it — **which is exactly why `out` must arrive zeroed**. Do not call this on
/// a buffer holding a partial result.
fn school_into(a: &[u64], b: &[u64], out: &mut [u64]) {
    debug_assert!(out.len() >= a.len() + b.len());
    debug_assert!(
        out.iter().all(|&w| w == 0),
        "school_into needs a zeroed out"
    );
    for (j, &bj) in b.iter().enumerate() {
        if bj == 0 {
            // The row contributes nothing and carries nothing, so the position
            // its carry would occupy is correctly left at zero.
            continue;
        }
        let mut carry = 0u64;
        for (i, &ai) in a.iter().enumerate() {
            let (lo, hi) = mac(out[i + j], ai, bj, carry);
            out[i + j] = lo;
            carry = hi;
        }
        out[j + a.len()] = carry;
    }
}

/// Limbs of scratch [`kara_into`] needs for these operand sizes.
///
/// **This mirrors `kara_into`'s dispatch and the two must change together.**
/// `kara_into` carries a `debug_assert` on the slice it is handed, which is what
/// catches the drift.
///
/// Only the `sa * sb` sub-product is recursed into here. The other two have
/// operands of at most `h` limbs against its `h + 1`, and the requirement is
/// monotone, so this is an upper bound — and it keeps the computation `O(log n)`
/// rather than walking all `n^1.585` nodes of the recursion tree.
fn scratch_needed(len_a: usize, len_b: usize, min: usize) -> usize {
    let (n, m) = (len_a.max(len_b), len_a.min(len_b));
    if m < min {
        return 0;
    }
    if n > 2 * m {
        return 2 * m + 2 + scratch_needed(m, m, min);
    }
    let h = n.div_ceil(2);
    4 * h + 4 + scratch_needed(h + 1, h + 1, min)
}

/// `out = a*b`. `out` must be zeroed and at least `a.len() + b.len()` long;
/// `scratch` at least [`scratch_needed`].
fn kara_into(a: &[u64], b: &[u64], out: &mut [u64], scratch: &mut [u64], min: usize) {
    debug_assert!(min >= KARATSUBA_FLOOR);
    let (a, b) = (trim(a), trim(b));
    if a.is_empty() || b.is_empty() {
        return;
    }
    let (n, m) = (a.len(), b.len());
    if n.min(m) < min {
        school_into(a, b, &mut out[..n + m]);
        return;
    }
    debug_assert!(
        scratch.len() >= scratch_needed(n, m, min),
        "scratch too small"
    );

    // Strongly unbalanced: block the longer operand rather than splitting it.
    if n > 2 * m {
        return blocked_into(a, b, out, scratch, min);
    }
    if m > 2 * n {
        return blocked_into(b, a, out, scratch, min);
    }

    // `h` is half the **longer** operand, not half `a`. Deriving it from
    // `a.len()` is correct only while `a` is the longer one, and silently
    // oversizes `b1` past its `h+1` sum buffer the moment the caller passes the
    // operands the other way round — which `b.mul(&a)` does.
    //
    // Past the two guards above, `n <= 2m` and `m <= 2n`, so
    // `h = ceil(max/2) <= min(n, m)` and both low halves are exactly `h` limbs.
    let h = n.max(m).div_ceil(2);
    debug_assert!(
        h <= n.min(m),
        "the unbalanced guards should have caught this"
    );
    let (a0, a1) = a.split_at(h);
    let (b0, b1) = b.split_at(h);

    // z0 into out[0..], z2 into out[2h..]. Both use the whole scratch, one after
    // the other, so their working space is shared rather than summed.
    let lz0 = a0.len() + b0.len();
    kara_into(a0, b0, &mut out[..lz0], scratch, min);
    let lz2 = if a1.is_empty() || b1.is_empty() {
        0
    } else {
        let l = a1.len() + b1.len();
        kara_into(a1, b1, &mut out[2 * h..2 * h + l], scratch, min);
        l
    };

    // t = (a0+a1) * (b0+b1), then t -= z0, t -= z2.
    let (t, rest) = scratch.split_at_mut(2 * h + 2);
    let (sa, rest) = rest.split_at_mut(h + 1);
    let (sb, rest) = rest.split_at_mut(h + 1);
    t.fill(0);
    sa.fill(0);
    sb.fill(0);
    sa[..a0.len()].copy_from_slice(a0);
    let c = add_assign_slice(sa, a1);
    debug_assert_eq!(c, 0, "sa has a spare limb for the carry");
    sb[..b0.len()].copy_from_slice(b0);
    let c = add_assign_slice(sb, b1);
    debug_assert_eq!(c, 0, "sb has a spare limb for the carry");

    kara_into(sa, sb, t, rest, min);
    let bo = sub_assign_slice(t, &out[..lz0]);
    debug_assert_eq!(bo, 0, "(a0+a1)(b0+b1) >= a0*b0");
    if lz2 > 0 {
        let bo = sub_assign_slice(t, &out[2 * h..2 * h + lz2]);
        debug_assert_eq!(bo, 0, "the middle term is non-negative");
    }

    // `t` is sized for the *worst case* `(a0+a1)*(b0+b1)`, which is wider than
    // `out[h..]` whenever the operands are unbalanced within the balanced arm —
    // at n = 33, m = 17 the buffer is 53 limbs against 33 available. After the
    // two subtractions the remaining value always fits, because `z1 * B^h` is
    // part of a product that fits in `n + m` limbs, so the excess is leading
    // zeros. Trim to the value rather than the buffer.
    let tl = trim(t).len();
    debug_assert!(
        h + tl <= out.len(),
        "z1 must fit above the low h limbs of the product"
    );
    let c = add_assign_slice(&mut out[h..], &t[..tl]);
    debug_assert_eq!(c, 0, "the product fits in n+m limbs");
}

/// `out = long * short` for `long.len() > 2 * short.len()`, by blocking.
///
/// One block product buffer, reused; not one allocation per block.
fn blocked_into(long: &[u64], short: &[u64], out: &mut [u64], scratch: &mut [u64], min: usize) {
    let k = short.len();
    let (tmp, rest) = scratch.split_at_mut(2 * k + 2);
    let mut off = 0;
    while off < long.len() {
        let end = (off + k).min(long.len());
        let blk = &long[off..end];
        let l = blk.len() + k;
        tmp[..l].fill(0);
        kara_into(blk, short, &mut tmp[..l], rest, min);
        let c = add_assign_slice(&mut out[off..], &tmp[..l]);
        debug_assert_eq!(c, 0, "the product fits in n+m limbs");
        off += k;
    }
}

impl BigUint {
    /// `self * rhs`.
    ///
    /// Dispatches on the shorter operand's limb count; see [`KARATSUBA_MIN`].
    pub fn mul(&self, rhs: &BigUint) -> BigUint {
        self.mul_with_min(rhs, KARATSUBA_MIN)
    }

    /// The dispatch, with the crossover as a parameter.
    ///
    /// Exists so a differential test can force deep recursion on small operands —
    /// at `min = 4` a 40-limb product recurses four levels, where at the shipped
    /// threshold it would not recurse at all. That is the only way to exercise
    /// the recombination without building operands too large to test quickly.
    pub(crate) fn mul_with_min(&self, rhs: &BigUint, min: usize) -> BigUint {
        debug_assert!(self.is_normalized() && rhs.is_normalized());
        let a = self.limbs();
        let b = rhs.limbs();
        if a.is_empty() || b.is_empty() {
            return BigUint::zero();
        }
        match mul_arm(a.len(), b.len(), min) {
            // The generic kernel is the *production* path below the crossover,
            // not merely a test oracle kept alive on principle. QG §4 requires it
            // to stay reachable; routing through it is what makes that structural
            // rather than a promise. Clippy's dead-code lint caught the first
            // version, where `school_into` was inlined here and `mul_schoolbook`
            // had no caller outside `#[cfg(test)]` — so `-D warnings` is what
            // keeps the oracle wired, and it is the check that actually runs.
            MulArm::Schoolbook => self.mul_schoolbook(rhs),
            MulArm::Karatsuba => {
                let mut out = vec![0u64; a.len() + b.len()];
                // Allocated only on the recursive path. Hoisting this above
                // the branch would put an allocation on every small multiply,
                // which is the mistake `ops::nary` records making with its
                // accumulator.
                let mut scratch = vec![0u64; scratch_needed(a.len(), b.len(), min)];
                kara_into(a, b, &mut out, &mut scratch, min);
                BigUint::from_limbs_le(out)
            }
        }
    }

    /// `O(n*m)` operand scanning. The generic kernel; see the module header.
    pub(crate) fn mul_schoolbook(&self, rhs: &BigUint) -> BigUint {
        debug_assert!(self.is_normalized() && rhs.is_normalized());
        let a = self.limbs();
        let b = rhs.limbs();
        if a.is_empty() || b.is_empty() {
            return BigUint::zero();
        }
        let mut out = vec![0u64; a.len() + b.len()];
        school_into(a, b, &mut out);
        BigUint::from_limbs_le(out)
    }

    /// `self * rhs`, for a single-limb multiplier.
    ///
    /// Saves wrapping the multiplier in a [`BigUint`], which would allocate.
    ///
    /// Nothing outside the tests calls it. An earlier version of this comment
    /// claimed it was "the shape division and base conversion actually want";
    /// that was false in both halves — [`BigUint::divrem`] uses
    /// [`BigUint::divrem_u64`], and there is no base conversion, because decimal
    /// formatting is deferred.
    pub fn mul_u64(&self, rhs: u64) -> BigUint {
        debug_assert!(self.is_normalized());
        let a = self.limbs();
        if a.is_empty() || rhs == 0 {
            return BigUint::zero();
        }
        let mut out = vec![0u64; a.len() + 1];
        let mut carry = 0u64;
        for (i, &ai) in a.iter().enumerate() {
            let (lo, hi) = mac(0, ai, rhs, carry);
            out[i] = lo;
            carry = hi;
        }
        out[a.len()] = carry;
        BigUint::from_limbs_le(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big(limbs: &[u64]) -> BigUint {
        BigUint::from_limbs_le(limbs.to_vec())
    }

    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *state
    }

    fn random_big(state: &mut u64, limbs: usize) -> BigUint {
        BigUint::from_limbs_le((0..limbs).map(|_| lcg(state)).collect())
    }

    /// The margin is exactly zero, so this is the identity the whole `u128`
    /// approach rests on.
    #[test]
    fn mac_cannot_overflow_even_at_every_input_maximum() {
        assert_eq!(
            mac(u64::MAX, u64::MAX, u64::MAX, u64::MAX),
            (u64::MAX, u64::MAX)
        );
        let t = (u64::MAX as u128) * (u64::MAX as u128) + (u64::MAX as u128) + (u64::MAX as u128);
        assert_eq!(t, u128::MAX);
    }

    #[test]
    fn multiplication_by_zero_and_one_behaves() {
        let a = big(&[7, 8, 9]);
        assert!(a.mul(&BigUint::zero()).is_zero());
        assert!(BigUint::zero().mul(&a).is_zero());
        assert_eq!(a.mul(&BigUint::one()), a);
        assert_eq!(BigUint::one().mul(&a), a);
        assert_eq!(a.mul_u64(0), BigUint::zero());
        assert_eq!(a.mul_u64(1), a);
    }

    #[test]
    fn a_full_width_product_carries_into_the_top_limb() {
        let m = big(&[u64::MAX]);
        assert_eq!(m.mul(&m), big(&[1, u64::MAX - 1]));
        let two = big(&[0, 0, 1]);
        assert_eq!(m.mul(&two), big(&[0, 0, u64::MAX]));
    }

    #[test]
    fn a_zero_limb_inside_the_multiplier_does_not_corrupt_the_row_above_it() {
        let a = big(&[u64::MAX, u64::MAX]);
        let b = big(&[1, 0, 1]);
        assert_eq!(a.mul(&b), a.shl(128).add(&a));
    }

    #[test]
    fn multiplication_is_commutative_and_associative() {
        let a = big(&[0xdead_beef, 0, 7]);
        let b = big(&[u64::MAX, 3]);
        let c = big(&[0x1234_5678_9abc_def0]);
        assert_eq!(a.mul(&b), b.mul(&a));
        assert_eq!(a.mul(&b).mul(&c), a.mul(&b.mul(&c)));
    }

    #[test]
    fn multiplication_distributes_over_addition() {
        let a = big(&[u64::MAX, u64::MAX, 1]);
        let b = big(&[9, 0, 0, 5]);
        let c = big(&[0x8000_0000_0000_0000]);
        assert_eq!(a.mul(&b.add(&c)), a.mul(&b).add(&a.mul(&c)));
    }

    #[test]
    fn multiplying_by_a_power_of_two_equals_a_left_shift() {
        let a = big(&[0x0123_4567_89ab_cdef, 0xfedc_ba98, 3]);
        for k in [0u64, 1, 63, 64, 65, 127, 128, 200] {
            assert_eq!(a.mul(&BigUint::one().shl(k)), a.shl(k), "k = {k}");
        }
    }

    #[test]
    fn mul_u64_agrees_with_the_general_kernel() {
        let a = big(&[u64::MAX, 0, 0x1234, u64::MAX]);
        for m in [1u64, 2, 0xffff, u64::MAX, u64::MAX - 1, 1 << 63] {
            assert_eq!(a.mul_u64(m), a.mul(&BigUint::from_u64(m)), "m = {m}");
        }
    }

    #[test]
    fn a_product_never_carries_a_trailing_zero_limb() {
        let p = big(&[2]).mul(&big(&[3]));
        assert_eq!(p, BigUint::from_u64(6));
        assert_eq!(p.limbs().len(), 1);
        assert!(p.is_normalized());
    }

    /// **QG §4's requirement**: the specialized arm and the generic kernel must
    /// agree on the *same* operands. Forced down to `min = 4` so the recursion is
    /// genuinely deep at sizes a test can afford, and swept across every limb
    /// count either side of the shipped threshold.
    #[test]
    fn every_multiplication_arm_agrees_with_the_schoolbook_oracle() {
        let mut st = 0x9e37_79b9_7f4a_7c15u64;
        for min in [KARATSUBA_FLOOR, 5, 8, KARATSUBA_MIN] {
            for n in 0usize..80 {
                for m in [0usize, 1, 2, 3, 4, 7, 8, 15, 16, 17, 31, 32, 33, 63, 64] {
                    if m > n + 40 {
                        continue;
                    }
                    let a = random_big(&mut st, n);
                    let b = random_big(&mut st, m);
                    assert_eq!(
                        a.mul_with_min(&b, min),
                        a.mul_schoolbook(&b),
                        "min {min}, n {n}, m {m}"
                    );
                }
            }
        }
    }

    /// Limb counts exactly at the crossover and either side of it, and the
    /// odd/even split Karatsuba's `h` depends on. Derived from the constant
    /// rather than hardcoded, so the boundary follows a re-measurement.
    #[test]
    fn operands_at_the_crossover_agree_with_the_oracle() {
        let mut st = 0x0123_4567_89ab_cdefu64;
        let t = KARATSUBA_MIN;
        for n in [t - 1, t, t + 1, 2 * t - 1, 2 * t, 2 * t + 1] {
            for m in [t - 1, t, t + 1, 2 * t + 1] {
                let a = random_big(&mut st, n);
                let b = random_big(&mut st, m);
                assert_eq!(a.mul(&b), a.mul_schoolbook(&b), "n {n}, m {m}");
                assert_eq!(a.mul(&b), b.mul(&a), "n {n}, m {m}");
            }
        }
    }

    /// All-ones operands make every sum in the additive split carry into its
    /// spare limb, which is the shape the subtractive variant would have needed a
    /// sign for.
    #[test]
    fn all_ones_operands_carry_through_every_split() {
        for n in [KARATSUBA_MIN, KARATSUBA_MIN + 1, 2 * KARATSUBA_MIN + 3, 100] {
            let a = big(&vec![u64::MAX; n]);
            assert_eq!(a.mul(&a), a.mul_schoolbook(&a), "n = {n}");
            let b = big(&vec![u64::MAX; n - 1]);
            assert_eq!(a.mul(&b), a.mul_schoolbook(&b), "n = {n}");
        }
    }

    #[test]
    fn the_dispatch_keys_on_the_shorter_operand() {
        let t = KARATSUBA_MIN;
        assert_eq!(mul_arm(1, 4096, t), MulArm::Schoolbook);
        assert_eq!(mul_arm(4096, 1, t), MulArm::Schoolbook);
        assert_eq!(mul_arm(t - 1, 4096, t), MulArm::Schoolbook);
        assert_eq!(mul_arm(4096, t - 1, t), MulArm::Schoolbook);
        assert_eq!(mul_arm(t, t, t), MulArm::Karatsuba);
    }

    /// The blocked path, which exists so a very unbalanced product does not pay
    /// for a split that leaves one half empty.
    #[test]
    fn a_strongly_unbalanced_product_agrees_with_the_oracle() {
        let mut st = 0xfeed_face_dead_beefu64;
        for (n, m) in [
            (200usize, 24usize),
            (200, 25),
            (4096, 30),
            (97, 32),
            (256, 128),
        ] {
            let a = random_big(&mut st, n);
            let b = random_big(&mut st, m);
            assert_eq!(a.mul(&b), a.mul_schoolbook(&b), "n {n}, m {m}");
            assert_eq!(b.mul(&a), a.mul_schoolbook(&b), "n {n}, m {m} reversed");
        }
    }

    /// `scratch_needed` mirrors `kara_into`'s dispatch by hand, so the thing that
    /// catches drift is the `debug_assert` inside the recursion — this exercises
    /// it across enough shapes for that assertion to mean something.
    #[test]
    fn the_scratch_estimate_covers_every_shape_the_recursion_takes() {
        let mut st = 0x2468_ace0_1357_9bdfu64;
        for min in [KARATSUBA_FLOOR, 6, 11] {
            for n in [4usize, 5, 9, 16, 17, 33, 64, 65, 129] {
                for m in [4usize, 5, 8, 17, 64, 129] {
                    let a = random_big(&mut st, n);
                    let b = random_big(&mut st, m);
                    // Panics inside `kara_into` if the estimate is short.
                    assert_eq!(a.mul_with_min(&b, min), a.mul_schoolbook(&b));
                }
            }
        }
    }
}
