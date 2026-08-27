//! Addition, subtraction, the bit shifts and the bit window: the carry and
//! borrow kernels.
//!
//! # Overflow is a boundary question, not an arithmetic one
//!
//! Nothing here can overflow. `add`, `shl` and later `mul` grow the limb vector,
//! and the only failure available is allocation — which is what it means for the
//! value type to carry no width. Overflow exists at exactly one place, where a
//! value meets a fixed `width_bits`: [`IntSink::place`](super::IntSink::place),
//! which **refuses** rather than clamping.
//!
//! **There is no saturating arithmetic and there will not be**, for three
//! reasons in increasing force. QG §6 already rules out the silent clamp.
//! Saturating needs `2^W - 1`, so it would put a width into `add` and from there
//! into every kernel — the coupling the seam doctrine exists to prevent. And,
//! decisively: **the reader cannot saturate.**
//!
//! Reading integer `k` at `width_bits = W` gathers exactly the ordinals
//! `[k*stride, k*stride + W)`. The bits above `W` are not clamped, they are
//! simply not in the range being read, so the read is `x mod 2^W` *by
//! construction* — there is no clamp available to it and no way to give it one.
//! Truncation is therefore the only write rule that agrees with the read rule.
//! Under a saturating write, storing `x` wide and reading it narrow would
//! disagree with storing `x` narrow, and the two would drift apart silently.
//! `truncating_before_the_write_agrees_with_reading_at_that_width` is the
//! property, and it is the one a saturating implementation fails.
//!
//! **Two tempting arguments against saturation are wrong, and were checked
//! rather than assumed.** Saturating addition and multiplication *are*
//! associative on unsigned values, because `min( ., M )` commutes with a monotone
//! operation. And saturation *does* nest — `sat_W' ( sat_W ( x ) )` is
//! `sat_W' ( x )` for `W' < W`, exactly as truncation does. Do not reach for a
//! broken-algebra or a non-composability argument here; both are false. What
//! fails is agreement with the reader, and nothing else.
//!
//! [`BigUint::truncate`] is how a caller opts into the cyclic reading. It is
//! spelled at the call site rather than configured on the layout, so the width
//! appears once, at the boundary where it means something.
//!
//! # In-place first
//!
//! [`BigUint::add_assign`] and [`BigUint::sub_assign`] are the primitives and
//! the owned forms clone into them, which is the arrangement
//! [`matrix::elem`](crate::matrix) uses for the same reason: a chained
//! computation should not allocate a fresh result per step, and the one place
//! that decides to allocate should be the caller.
//!
//! # The limb primitives, and why they are written this way
//!
//! `u64::carrying_add`, `u64::borrowing_sub` and `u64::widening_mul` are
//! **unstable**. This crate's MSRV of 1.89 is a promise to its users ( see the
//! rationale in the workspace manifest ), and an arithmetic convenience is not a
//! reason to move it. [`adc`] and [`sbb`] are therefore written against
//! `overflowing_add` / `overflowing_sub`, which LLVM folds into the same
//! `adc` / `sbb` instruction pair on x86-64 and into `adcs` / `sbcs` on
//! AArch64.
//!
//! Two `overflowing_add`s are needed, not one. `a + b + carry` can overflow
//! at either step — `u64::MAX + 0 + 1` overflows only on the second — but it can
//! never overflow at *both*, because the first sum being `u64::MAX` ( the only
//! value that makes the second overflow ) means the first did not. So the two
//! carry-out flags are disjoint and `c0 | c1` is exact rather than a truncation
//! of a two-bit sum.

use super::BigUint;

/// Add with carry: `a + b + carry`, returning the low word and the carry out.
#[inline]
pub(crate) fn adc(a: u64, b: u64, carry: u64) -> (u64, u64) {
    let (s, c0) = a.overflowing_add(b);
    let (s, c1) = s.overflowing_add(carry);
    (s, (c0 | c1) as u64)
}

/// Subtract with borrow: `a - b - borrow`, returning the low word and the
/// borrow out.
#[inline]
pub(crate) fn sbb(a: u64, b: u64, borrow: u64) -> (u64, u64) {
    let (d, b0) = a.overflowing_sub(b);
    let (d, b1) = d.overflowing_sub(borrow);
    (d, (b0 | b1) as u64)
}

impl BigUint {
    /// `self += rhs`.
    pub fn add_assign(&mut self, rhs: &BigUint) {
        debug_assert!(self.is_normalized() && rhs.is_normalized());
        let n = rhs.limbs().len();
        if self.limbs().len() < n {
            self.limbs_mut().resize(n, 0);
        }
        let mut carry = 0u64;
        for (i, x) in self.limbs.iter_mut().enumerate() {
            // The exit test is `i >= n`, not `b == 0`. A zero limb *inside*
            // `rhs` says nothing about the limbs above it, so stopping on one
            // would drop the rest of the addend.
            if i >= n && carry == 0 {
                break;
            }
            let b = rhs.limbs().get(i).copied().unwrap_or(0);
            let (s, c) = adc(*x, b, carry);
            *x = s;
            carry = c;
        }
        if carry != 0 {
            self.limbs_mut().push(carry);
        }
        debug_assert!(self.is_normalized());
    }

    /// `self + rhs`.
    pub fn add(&self, rhs: &BigUint) -> BigUint {
        // Clone the longer operand so the in-place form never has to grow.
        let (mut acc, other) = if self.limbs().len() >= rhs.limbs().len() {
            (self.clone(), rhs)
        } else {
            (rhs.clone(), self)
        };
        acc.add_assign(other);
        acc
    }

    /// `self -= rhs`, or `false` and **no change** if `rhs > self`.
    ///
    /// The comparison happens before any limb is written, so a declined
    /// subtraction leaves the value exactly as it was — the same contract
    /// [`IntSink::place`](super::IntSink::place) has for a rejected placement.
    pub fn sub_assign(&mut self, rhs: &BigUint) -> bool {
        debug_assert!(self.is_normalized() && rhs.is_normalized());
        if &*self < rhs {
            return false;
        }
        let n = rhs.limbs().len();
        let mut borrow = 0u64;
        for (i, x) in self.limbs.iter_mut().enumerate() {
            if i >= n && borrow == 0 {
                break;
            }
            let b = rhs.limbs().get(i).copied().unwrap_or(0);
            let (d, bo) = sbb(*x, b, borrow);
            *x = d;
            borrow = bo;
        }
        debug_assert_eq!(borrow, 0, "the magnitude comparison ruled this out");
        // Unlike addition, subtraction can vacate the top limbs, so this is the
        // one place normalization is load-bearing rather than defensive.
        self.normalize();
        true
    }

    /// `self - rhs`, or `None` if it would go below zero.
    ///
    /// Not a wrapping subtraction. There is no width to wrap to: an
    /// arbitrary-precision unsigned type has no `u64::MAX` to land on, so the
    /// only two honest answers are a value and no value. `None` follows
    /// [`BitMatrix::invert_gf2`](crate::matrix::BitMatrix::invert_gf2), which
    /// says the same thing about a singular matrix.
    pub fn sub(&self, rhs: &BigUint) -> Option<BigUint> {
        let mut acc = self.clone();
        acc.sub_assign(rhs).then_some(acc)
    }

    /// `self << n`.
    ///
    /// Allocates proportionally to `n`, not to `self`: shifting by a billion
    /// bits produces a billion-bit number, and nothing here caps that. The
    /// [`IntSink`](super::IntSink) boundary is where a width is enforced.
    ///
    /// # Panics
    ///
    /// If `n / 64` exceeds a `usize`, which can happen only on a 32-bit target.
    /// The result could not be allocated there in any case, and a panic is what
    /// `Vec` itself raises on a capacity overflow — the alternative is a
    /// truncated shift distance and a plausible wrong answer.
    pub fn shl(&self, n: u64) -> BigUint {
        debug_assert!(self.is_normalized());
        if self.is_zero() {
            return BigUint::zero();
        }
        let whole =
            usize::try_from(n / 64).expect("left shift distance exceeds this target's usize");
        let bits = (n % 64) as u32;
        let len = self.limbs().len();
        let mut out = vec![0u64; whole + len + 1];
        if bits == 0 {
            out[whole..whole + len].copy_from_slice(self.limbs());
        } else {
            let mut carry = 0u64;
            for (i, &w) in self.limbs().iter().enumerate() {
                out[whole + i] = (w << bits) | carry;
                // `bits` is 1..=63 here, so `64 - bits` is 1..=63 and this is
                // never the undefined full-width shift.
                carry = w >> (64 - bits);
            }
            out[whole + len] = carry;
        }
        BigUint::from_limbs_le(out)
    }

    /// `self >> n`. Bits shifted past the bottom are discarded.
    pub fn shr(&self, n: u64) -> BigUint {
        debug_assert!(self.is_normalized());
        // Compared as `u64` before the cast. `n / 64` reaches `2^58`, which is
        // exact in a 64-bit `usize` and wraps in a 32-bit one — and a wrapped
        // shift distance returns a plausible wrong answer rather than failing.
        let whole = n / 64;
        if whole >= self.limbs().len() as u64 {
            return BigUint::zero();
        }
        let whole = whole as usize;
        let bits = (n % 64) as u32;
        let src = &self.limbs()[whole..];
        let mut out = vec![0u64; src.len()];
        if bits == 0 {
            out.copy_from_slice(src);
        } else {
            for (i, x) in out.iter_mut().enumerate() {
                let hi = src.get(i + 1).map_or(0, |&w| w << (64 - bits));
                *x = (src[i] >> bits) | hi;
            }
        }
        BigUint::from_limbs_le(out)
    }

    /// Keep the low `bits` bits: exactly `self mod 2^bits`.
    ///
    /// This is the module's entire overflow story, and it is spelled at the call
    /// site rather than configured anywhere:
    ///
    /// ```
    /// use yesno_core::bignum::{BigUint, IntLayout, IntSink};
    ///
    /// let layout = IntLayout::dense(8);
    /// let wide = BigUint::from_u64(0x1_23);
    /// let mut sink = IntSink::new(layout);
    ///
    /// // A value that does not fit is refused, never clamped.
    /// assert!(sink.place(0, &wide).is_err());
    /// // Opting into the cyclic reading is one call, and it names the width.
    /// sink.place(0, &wide.truncate(layout.width_bits as u64)).unwrap();
    /// assert_eq!(sink.build().read_int(0, &layout).unwrap(), BigUint::from_u64(0x23));
    /// ```
    ///
    /// # Why this and not a saturating form
    ///
    /// [`OrdSet::read_int`](crate::OrdSet::read_int) at a narrower `width_bits`
    /// yields `x mod 2^width_bits`, and not by choice: reading `W` ordinals out
    /// of the store cannot see the bits above them, so the reduction is what the
    /// bit layout *is*. Truncation is the only write rule that agrees with it.
    ///
    /// It is **not** true that saturation fails to compose — `sat_W'` after
    /// `sat_W` is `sat_W'`, just as truncation nests — and saturating addition is
    /// associative. Those arguments were checked and are false; the reader is the
    /// whole case. See the module header.
    pub fn truncate(&self, bits: u64) -> BigUint {
        let mut out = self.clone();
        out.truncate_assign(bits);
        out
    }

    /// `self = self mod 2^bits`. The kernel [`BigUint::truncate`] wraps.
    pub fn truncate_assign(&mut self, bits: u64) {
        debug_assert!(self.is_normalized());
        // The guard is on `bit_len`, not on the limb count. A value of 96
        // bits truncated to 65 occupies two limbs either way, so a
        // `keep >= limbs.len()` test would return it unchanged and silently skip
        // the mask on the partial top limb — wrong for every width that is not a
        // multiple of 64, which is the majority of them.
        if self.bit_len() <= bits {
            return;
        }
        // `bits < bit_len() <= limbs.len() * 64`, so `keep <= limbs.len()` and
        // the cast cannot lose anything on any target.
        let keep = bits.div_ceil(64) as usize;
        self.limbs.truncate(keep);
        let rem = (bits % 64) as u32;
        if rem != 0 {
            // `keep` rounded up, so the last kept limb is the partial one.
            if let Some(last) = self.limbs.last_mut() {
                *last &= (1u64 << rem) - 1;
            }
        }
        // Masking can clear the top limb outright, so this is load-bearing
        // rather than defensive — the same reason `sub_assign` normalizes.
        self.normalize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big(limbs: &[u64]) -> BigUint {
        BigUint::from_limbs_le(limbs.to_vec())
    }

    #[test]
    fn a_carry_propagates_the_whole_length_and_grows_the_value() {
        let mut a = big(&[u64::MAX, u64::MAX, u64::MAX]);
        a.add_assign(&BigUint::one());
        assert_eq!(a, big(&[0, 0, 0, 1]));
        assert!(a.is_normalized());
    }

    /// The sabotage this pins: exiting the loop on a zero limb of `rhs` rather
    /// than on running past its length drops every limb above the zero.
    #[test]
    fn a_zero_limb_inside_the_addend_does_not_end_the_addition() {
        let a = big(&[1]);
        let b = big(&[0, 0, 7]);
        assert_eq!(a.add(&b), big(&[1, 0, 7]));
    }

    #[test]
    fn addition_is_commutative_regardless_of_which_operand_is_longer() {
        let a = big(&[3, 4, 5]);
        let b = big(&[9]);
        assert_eq!(a.add(&b), b.add(&a));
        assert_eq!(a.add(&b), big(&[12, 4, 5]));
    }

    #[test]
    fn adding_zero_changes_nothing_in_either_direction() {
        let a = big(&[7, 8]);
        assert_eq!(a.add(&BigUint::zero()), a);
        assert_eq!(BigUint::zero().add(&a), a);
        assert_eq!(BigUint::zero().add(&BigUint::zero()), BigUint::zero());
    }

    #[test]
    fn a_borrow_vacates_the_top_limbs_and_the_result_is_normalized() {
        let a = big(&[0, 0, 1]);
        let d = a.sub(&BigUint::one()).unwrap();
        assert_eq!(d, big(&[u64::MAX, u64::MAX]));
        assert!(d.is_normalized());
        assert_eq!(d.limbs().len(), 2);
    }

    #[test]
    fn subtraction_below_zero_is_none_and_leaves_the_value_untouched() {
        let mut a = big(&[5]);
        assert!(!a.sub_assign(&big(&[6])));
        assert_eq!(a, big(&[5]), "a declined subtraction must not write");
        assert_eq!(big(&[5]).sub(&big(&[6])), None);
        assert_eq!(big(&[5]).sub(&big(&[5])), Some(BigUint::zero()));
    }

    #[test]
    fn add_and_sub_are_mutually_inverse() {
        let a = big(&[0xdead_beef, 0, 0xfeed]);
        let b = big(&[u64::MAX, 3]);
        assert_eq!(a.add(&b).sub(&b), Some(a.clone()));
        assert_eq!(a.add(&b).sub(&a), Some(b));
    }

    #[test]
    fn shifting_left_then_right_recovers_the_value() {
        let a = big(&[0x1234_5678_9abc_def0, 0xff]);
        for n in [0u64, 1, 63, 64, 65, 127, 128, 200] {
            assert_eq!(a.shl(n).shr(n), a, "n = {n}");
        }
    }

    #[test]
    fn a_left_shift_is_a_multiplication_by_a_power_of_two() {
        let a = big(&[3]);
        assert_eq!(a.shl(1), big(&[6]));
        assert_eq!(a.shl(64), big(&[0, 3]));
        assert_eq!(a.shl(63), big(&[1 << 63, 1]));
        assert_eq!(BigUint::zero().shl(1000), BigUint::zero());
    }

    #[test]
    fn a_right_shift_discards_the_bits_below_and_can_reach_zero() {
        let a = big(&[0, 1]);
        assert_eq!(a.shr(64), BigUint::one());
        assert_eq!(a.shr(65), BigUint::zero());
        assert_eq!(a.shr(1_000_000), BigUint::zero());
        assert_eq!(big(&[0b1011]).shr(2), big(&[0b10]));
    }

    #[test]
    fn truncate_keeps_exactly_the_low_bits() {
        let a = big(&[0x0123_4567_89ab_cdef, 0xfedc_ba98]);
        assert_eq!(a.truncate(0), BigUint::zero());
        assert_eq!(a.truncate(4), BigUint::from_u64(0xf));
        assert_eq!(a.truncate(64), BigUint::from_u64(0x0123_4567_89ab_cdef));
        assert_eq!(a.truncate(65), big(&[0x0123_4567_89ab_cdef, 0]));
        assert_eq!(a.truncate(66), big(&[0x0123_4567_89ab_cdef, 0]));
        // A width at or above the value's own leaves it alone.
        assert_eq!(a.truncate(a.bit_len()), a);
        assert_eq!(a.truncate(10_000), a);
    }

    /// This does **not** discriminate against saturation — `sat_W'` after
    /// `sat_W` is also `sat_W'` — and a saturating sabotage passes it. It is kept
    /// because nesting is a real property of `truncate` worth pinning, not
    /// because it argues for it. The property that discriminates is
    /// `truncating_before_the_write_agrees_with_reading_at_that_width`, in
    /// `read.rs`.
    #[test]
    fn truncation_nests() {
        let a = big(&[u64::MAX, 0x0f0f_0f0f_0f0f_0f0f, 7]);
        for &wide in &[200u64, 129, 128, 127, 65, 64] {
            for &narrow in &[0u64, 1, 63, 64, 65, 100, 127] {
                if narrow > wide {
                    continue;
                }
                assert_eq!(
                    a.truncate(wide).truncate(narrow),
                    a.truncate(narrow),
                    "wide = {wide}, narrow = {narrow}"
                );
            }
        }
    }

    /// `( a + b ) mod 2^w == ( ( a mod 2^w ) + ( b mod 2^w ) ) mod 2^w` — the
    /// ring homomorphism, which is what lets a caller truncate early or late.
    ///
    /// Also not a discriminator: saturating addition satisfies the analogous
    /// identity, because `min( ., M )` commutes with a monotone operation.
    #[test]
    fn truncate_is_a_reduction_and_therefore_commutes_with_addition() {
        let a = big(&[u64::MAX, 3]);
        let b = big(&[0x8000_0000_0000_0001, 0xff]);
        for &w in &[1u64, 7, 63, 64, 65, 96, 128, 200] {
            let lhs = a.add(&b).truncate(w);
            let rhs = a.truncate(w).add(&b.truncate(w)).truncate(w);
            assert_eq!(lhs, rhs, "w = {w}");
        }
    }

    #[test]
    fn truncate_leaves_no_trailing_zero_limb() {
        // The mask can clear the top kept limb outright.
        let a = big(&[7, 1 << 40]);
        let t = a.truncate(100);
        assert!(t.is_normalized());
        assert_eq!(t, BigUint::from_u64(7));
        assert_eq!(t.limbs().len(), 1);
        // And all the way to zero.
        assert!(big(&[0, 5]).truncate(64).is_zero());
    }

    #[test]
    fn a_shift_result_never_carries_a_trailing_zero_limb() {
        // The `whole + len + 1` scratch is one limb longer than the answer
        // whenever nothing carries out of the top.
        let a = big(&[1]);
        assert_eq!(a.shl(1).limbs().len(), 1);
        assert_eq!(a.shl(64).limbs().len(), 2);
        assert!(a.shl(63).is_normalized());
    }
}
