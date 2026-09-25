//! Arbitrary-precision unsigned arithmetic: an [`OrdSet`](crate::OrdSet) read as
//! a big integer.
//!
//! An ordinal set is a bit vector over `[0, ORDINAL_MAX]`, and that bit vector
//! **is** an unsigned integer: the ordinal `j` carries the `2^j` term. One set
//! is one number. It is the same reinterpretation
//! [`matrix`](crate::matrix) performs, at a different shape -- there a chunk is
//! a 256x256 matrix, here a set is a number as wide as its highest member.
//!
//! # One set, one integer, and no layout
//!
//! An earlier form carried an `IntLayout` of `width_bits` and `stride`, so that
//! **several** integers shared a set and `k * stride + j` addressed bit `j` of
//! integer `k`. That is gone. Nothing ever packed several: every layout the
//! workspace constructed was dense, the padded constructors' only consumers were
//! tests asserting their own arithmetic, and a descriptor that can be
//! self-inconsistent -- `stride < width_bits` -- had to be checked everywhere it
//! was accepted. Several integers are now several **keys**, which is what a key
//! is for.
//!
//! Three things fall out, and the third is the one that paid for the change:
//!
//! - **Width is an argument, not a type.** It appears only where a *choice* is
//!   being made -- reading fewer bits than are there -- and
//!   [`OrdSet::read_int`](crate::OrdSet::read_int) is total, so there is no
//!   layout to validate and no unaddressable index to report.
//! - **Reading narrower is exactly `x mod 2^width`.** That identity was always
//!   the payoff of least-significant-bit-first ordering; with the base pinned to
//!   zero it is now the *only* thing a width does.
//! - **Nothing straddles a limb.** The hazard this header used to open with --
//!   an integer, or a single limb, crossing the 65 536-bit chunk boundary --
//!   existed because an arbitrary stride put an integer's base at a
//!   non-multiple of 64. With the base always zero and 65 536 bits being exactly
//!   1 024 limbs, every chunk maps onto a whole limb window. The shift-and-carry
//!   seam it required is gone rather than centralized.
//!
//! # Least significant bit first, and why there is no other order
//!
//! The ordinal set and the integer are the *same object*, and the arithmetic
//! reading falls out of the set reading for free: two integers with no bit in
//! common satisfy `a + b == a | b`, so [`OrdSet::or`](crate::OrdSet::or) **is**
//! their sum, and [`OrdSet::xor`](crate::OrdSet::xor) is addition without carry
//! -- that is, addition in `GF(2)[x]`. A most-significant-bit-first option would
//! reverse every read and write and buy none of that back, and under it the
//! value of every stored bit would depend on the width. So there is deliberately
//! no `Order` knob here as there is on [`Layout`](crate::matrix::Layout).
//!
//! # The canonical form has no trailing zero limb
//!
//! [`BigUint`] derives `PartialEq`, which compares limbs, and its ordering
//! compares lengths first. A trailing zero limb is therefore not "don't care" —
//! it makes two equal integers compare unequal and makes `bit_len` wrong. Every
//! operation that writes limbs must trim; [`BigUint::is_normalized`] is the
//! debug-time guard, and it is the direct analogue of
//! [`BitMatrix::tail_is_clear`](crate::matrix::BitMatrix::tail_is_clear).
//!
//! There is no cached bit length. [`BitMatrix`](crate::matrix::BitMatrix)
//! carries `ones` because a population count is `O(n)`; `bit_len` here is `O(1)`
//! from the top limb's `leading_zeros`, so a cache would be state to invalidate
//! in exchange for nothing.
//!
//! # What this module is not
//!
//! **Not a cryptographic library.** Nothing here is constant-time, and no
//! effort is made to avoid data-dependent branches or memory access. `divrem`
//! short-circuits on operand size and the shifts branch on the bit offset. This
//! is an index; do not use these operations on secret material and do not
//! add a "constant-time" variant without also adding the test layer that could
//! catch it regressing, which this crate does not have.
//!
//! **Unsigned only.** No sign, no rationals, no floats. A signed layer would
//! have to decide truncating versus Euclidean division, and nothing here asks
//! the question. A *signed reading* of a stored value is a different thing and
//! belongs to the reader, not to this type: two's complement needs a width to
//! be negative in, and a width is an argument here rather than a property of
//! anything stored.
//!
//! **No `std::ops` implementations.** `Sub`, `Div` and `Rem` cannot be total
//! on an unsigned arbitrary-precision type — underflow and a zero divisor have
//! no value to return — so they would have to panic, next to a
//! [`BigUint::sub`] that returns `None`. Two spellings of one operation, one of
//! which aborts the process, is worse than one honest spelling.

mod addsub;
mod div;
mod modular;
mod mul;
mod query;
mod read;
mod signed;
mod write;

pub use modular::Barrett;
pub use mul::KARATSUBA_MIN;
pub use signed::BigInt;

/// An owned arbitrary-precision unsigned integer, little-endian by `u64` limb.
///
/// `limbs[0]` carries `2^0..2^63`, `limbs[1]` carries `2^64..2^127`, and so on.
/// There is **never** a trailing zero limb — see the module header for why that
/// is an invariant rather than a convention — so zero is the empty limb vector
/// and `limbs.len()` is exactly `bit_len().div_ceil(64)`.
///
/// # Ordering is by magnitude, and is not derived
///
/// `#[derive(PartialOrd)]` on a little-endian limb vector would compare
/// `limbs[0]` first, which orders by the *low* word. `Ord` is therefore
/// hand-written: longer is greater, and equal lengths compare from the top limb
/// down. Normalization is what makes the length comparison sound, which is the
/// second reason it is an invariant.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct BigUint {
    limbs: Vec<u64>,
}

impl BigUint {
    /// Zero.
    #[inline]
    pub fn zero() -> BigUint {
        BigUint { limbs: Vec::new() }
    }

    /// One.
    #[inline]
    pub fn one() -> BigUint {
        BigUint { limbs: vec![1] }
    }

    /// A single-limb value.
    #[inline]
    pub fn from_u64(v: u64) -> BigUint {
        if v == 0 {
            BigUint::zero()
        } else {
            BigUint { limbs: vec![v] }
        }
    }

    /// Adopt a little-endian limb vector, trimming any trailing zero limbs.
    ///
    /// This is the only constructor that takes raw limbs, and it normalizes, so
    /// no caller can create an unnormalized value.
    pub fn from_limbs_le(mut limbs: Vec<u64>) -> BigUint {
        while limbs.last() == Some(&0) {
            limbs.pop();
        }
        BigUint { limbs }
    }

    /// The limbs, little-endian, with no trailing zero.
    #[inline]
    pub fn limbs(&self) -> &[u64] {
        &self.limbs
    }

    /// Does this value satisfy the no-trailing-zero-limb invariant?
    ///
    /// The debug-time guard every limb-writing operation asserts, and the
    /// analogue of
    /// [`BitMatrix::tail_is_clear`](crate::matrix::BitMatrix::tail_is_clear).
    #[inline]
    pub fn is_normalized(&self) -> bool {
        self.limbs.last() != Some(&0)
    }

    /// Trim trailing zero limbs, restoring the invariant.
    #[inline]
    pub(crate) fn normalize(&mut self) {
        while self.limbs.last() == Some(&0) {
            self.limbs.pop();
        }
    }

    /// Mutable access to the limbs for a kernel that will re-normalize itself.
    #[inline]
    pub(crate) fn limbs_mut(&mut self) -> &mut Vec<u64> {
        &mut self.limbs
    }
}

impl Ord for BigUint {
    fn cmp(&self, other: &BigUint) -> std::cmp::Ordering {
        debug_assert!(self.is_normalized() && other.is_normalized());
        // Normalized, so a longer limb vector is strictly larger. Without that
        // invariant this comparison is simply wrong, not merely slower.
        self.limbs
            .len()
            .cmp(&other.limbs.len())
            .then_with(|| self.limbs.iter().rev().cmp(other.limbs.iter().rev()))
    }
}

impl PartialOrd for BigUint {
    #[inline]
    fn partial_cmp(&self, other: &BigUint) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl From<u64> for BigUint {
    #[inline]
    fn from(v: u64) -> BigUint {
        BigUint::from_u64(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_trailing_zero_limb_never_survives_construction() {
        let a = BigUint::from_limbs_le(vec![7, 0, 0]);
        assert_eq!(a.limbs(), &[7]);
        assert!(a.is_normalized());
        assert_eq!(a, BigUint::from_u64(7));
    }
    #[test]
    fn zero_is_the_empty_limb_vector() {
        assert_eq!(BigUint::zero().limbs(), &[] as &[u64]);
        assert_eq!(BigUint::from_u64(0), BigUint::zero());
        assert_eq!(BigUint::from_limbs_le(vec![0, 0]), BigUint::zero());
    }
    #[test]
    fn ordering_is_by_magnitude_not_by_the_low_limb() {
        let small = BigUint::from_limbs_le(vec![u64::MAX]);
        let large = BigUint::from_limbs_le(vec![0, 1]);
        assert!(small < large);
        let a = BigUint::from_limbs_le(vec![1, 5]);
        let b = BigUint::from_limbs_le(vec![9, 5]);
        assert!(a < b);
    }
    #[test]
    fn is_normalized_notices_a_raw_limb_write() {
        let mut a = BigUint::from_u64(5);
        assert!(a.is_normalized());
        a.limbs_mut().push(0);
        assert!(!a.is_normalized());
        a.normalize();
        assert!(a.is_normalized());
        assert_eq!(a, BigUint::from_u64(5));
    }
}
