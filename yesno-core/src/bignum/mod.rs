//! Arbitrary-precision unsigned arithmetic: an [`OrdSet`](crate::OrdSet) read as
//! a series of big integers.
//!
//! An ordinal set is a bit vector over `[0, ORDINAL_MAX]`. Fix an [`IntLayout`]
//! and that bit vector becomes a series of unsigned integers, one per index,
//! with the ordinal `k*stride + j` carrying the `2^j` term of integer `k`. It is
//! the same reinterpretation [`matrix`](crate::matrix) performs, at a different
//! shape: there a chunk is a 256x256 matrix, here it is a 65 536-bit number.
//!
//! # The seam is paid at the boundary, never in a kernel
//!
//! `width_bits` and `stride` are arbitrary, so an integer can begin at any bit
//! offset and can straddle the 65 536-bit chunk boundary. If every kernel
//! handled that, the shift-and-carry path would live in the addition, the
//! multiplication and the division at once — and those already have carry logic
//! of their own, which is exactly the confusion worth preventing.
//!
//! Instead the reader normalizes. [`OrdSet::read_int`](crate::OrdSet::read_int)
//! gathers an arbitrary-layout integer into the **canonical form** — a
//! little-endian `u64` limb vector with no trailing zero limb — every kernel
//! works only on that, and [`IntSink`] scatters back. One shift path in, one
//! out, and the limb loops in between carry only arithmetic carries.
//!
//! **That shift path is [`pack`](crate::pack) and is shared with
//! [`matrix`](crate::matrix).** An integer is a [`Packing`](crate::pack::Packing)
//! with **one line**, and a one-line packing's canonical words *are* a limb
//! vector — so the same gather serves both lenses, and the **seeking arm this
//! module used to lack now applies here too**. Before it did, reading an integer
//! that did not begin on a chunk boundary discarded every value below it.
//!
//! `width_bits < 65536` does **not** by itself keep an integer inside one
//! chunk. At `stride = 10 000`, integer 6 spans bits 60 000..70 000 and crosses
//! the boundary. Straddling is a property of `stride`, not of width, and any
//! fast path must *test* for chunk containment rather than infer it. A test
//! generator that omits a straddling index leaves the seam untested while every
//! property still passes.
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
//! the question.
//!
//! **No `std::ops` implementations.** `Sub`, `Div` and `Rem` cannot be total
//! on an unsigned arbitrary-precision type — underflow and a zero divisor have
//! no value to return — so they would have to panic, next to a
//! [`BigUint::sub`] that returns `None`. Two spellings of one operation, one of
//! which aborts the process, is worse than one honest spelling.

use crate::pack::Packing;
use crate::{CodecError, Result, ORDINAL_MAX};

mod addsub;
mod div;
mod modular;
mod mul;
mod query;
mod read;
mod sink;

pub use modular::Barrett;
pub use mul::KARATSUBA_MIN;
pub use sink::IntSink;

/// Where the bits of an integer live in the ordinal space.
///
/// Bit `j` of integer `k` sits at ordinal
///
/// ```text
/// k * stride + j
/// ```
///
/// so integer `k` occupies the half-open ordinal range
/// `[k*stride, k*stride + width_bits)`.
///
/// | what you want | how to say it |
/// |---|---|
/// | densely packed, 1:1 with ordinals | [`IntLayout::dense`] |
/// | every integer starting on a `u64` boundary | [`IntLayout::word_aligned`] |
/// | one integer per chunk, so none ever straddles | `stride = 65536` |
///
/// # Least significant bit first, and why there is no other order
///
/// Under [`IntLayout::dense`] the ordinal set and the integer series are the
/// *same object*, and the arithmetic reading falls out of the set reading for
/// free: two integers with no bit in common satisfy `a + b == a | b`, so
/// [`OrdSet::or`](crate::OrdSet::or) **is** their sum, and
/// [`OrdSet::xor`](crate::OrdSet::xor) is addition without carry — that is,
/// addition in `GF(2)[x]`. A most-significant-bit-first option would reverse
/// every gather and scatter and buy none of that back, so there is deliberately
/// no `Order` knob here as there is on
/// [`Layout`](crate::matrix::Layout).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IntLayout {
    /// Bits addressable in one integer. A value needing more is rejected rather
    /// than truncated.
    pub width_bits: u32,
    /// Ordinals between the start of consecutive integers. Must be at least
    /// `width_bits`.
    pub stride: u64,
}

impl IntLayout {
    /// Densely packed: no padding anywhere, so the ordinal space and the integer
    /// series coincide exactly.
    pub fn dense(width_bits: u32) -> IntLayout {
        IntLayout {
            width_bits,
            stride: width_bits as u64,
        }
    }

    /// Each integer starting on a `u64` boundary.
    ///
    /// Wastes up to 63 bits per integer and gives up the identity described on
    /// [`IntLayout`], in exchange for a gather that can copy whole words.
    pub fn word_aligned(width_bits: u32) -> IntLayout {
        IntLayout {
            width_bits,
            stride: (width_bits as u64).div_ceil(64) * 64,
        }
    }

    /// Each integer starting on a 65 536-bit chunk boundary, so **none ever
    /// straddles**.
    ///
    /// `None` if `width_bits` exceeds a chunk, where the promise is
    /// unachievable. This is the spelling of the row the [`IntLayout`] table
    /// names, given a constructor because the hazard it avoids is the one the
    /// module header warns about — and a caller who has just read that warning
    /// should not have to re-derive the stride.
    pub fn chunk_aligned(width_bits: u32) -> Option<IntLayout> {
        (width_bits <= crate::CHUNK_CARD).then_some(IntLayout {
            width_bits,
            stride: crate::CHUNK_CARD as u64,
        })
    }

    /// Limbs in the canonical form of a full-width value.
    #[inline]
    pub fn limbs(&self) -> usize {
        (self.width_bits as usize).div_ceil(64)
    }

    /// Ordinals from the first to the last bit of one integer, inclusive.
    ///
    /// This is `width_bits`, not `stride`: the padding above the last bit is
    /// addressed by nothing.
    #[inline]
    pub fn span_bits(&self) -> u64 {
        self.width_bits as u64
    }

    /// Is this layout self-consistent?
    ///
    /// Rejects a zero width and a `stride` that would overlap consecutive
    /// integers. A deliberately over-large stride is fine — that is a caller
    /// choice, not an error.
    pub fn check(&self) -> Result<()> {
        if self.width_bits == 0 {
            return Err(CodecError::Invariant("integer layout has zero width"));
        }
        if self.stride < self.width_bits as u64 {
            return Err(CodecError::Invariant(
                "integer layout stride is shorter than an integer",
            ));
        }
        Ok(())
    }

    /// First ordinal of integer `k`, or `None` if it is out of the universe.
    #[inline]
    pub fn base_of(&self, k: u64) -> Option<u64> {
        let base = k.checked_mul(self.stride)?;
        (base <= ORDINAL_MAX).then_some(base)
    }

    /// This layout as the shared [`Packing`](crate::pack::Packing) the transfer
    /// kernels are defined on.
    ///
    /// An integer is a packing with **one line**: `line_bits` is the width and
    /// the line stride never applies, because there is no second line for it to
    /// reach. That is not a convenient encoding of an unrelated thing — a
    /// one-line packing's canonical words *are* a limb vector, which is why the
    /// same gather serves both lenses.
    #[inline]
    pub fn packing(&self) -> Packing {
        Packing {
            line_bits: self.width_bits,
            lines: 1,
            line_stride: self.width_bits,
            object_stride: self.stride,
        }
    }

    /// Does integer `k` cross a 65 536-bit chunk boundary? `None` if `k` is not
    /// addressable at all.
    ///
    /// Exposed because the module header names straddling as *the* hazard, and a
    /// caller choosing a `stride` needs a way to check the claim rather than
    /// re-deriving the arithmetic. [`IntLayout::chunk_aligned`] is the answer
    /// for a caller who would rather not think about it.
    pub fn straddles(&self, k: u64) -> Option<bool> {
        let base = self.base_of(k)?;
        let last = base.checked_add(self.span_bits() - 1)?;
        if last > ORDINAL_MAX {
            return None;
        }
        let chunk = crate::CHUNK_CARD as u64;
        Some(base / chunk != last / chunk)
    }

    /// Ordinal holding bit `bit` of integer `k`.
    ///
    /// `None` if `bit` is out of range for the layout, or if the ordinal would
    /// exceed [`ORDINAL_MAX`] — `u64::MAX` is not an ordinal ( invariant I8 ),
    /// so an integer that would reach it cannot be addressed at all rather than
    /// being silently truncated.
    #[inline]
    pub fn ordinal_at(&self, k: u64, bit: u32) -> Option<u64> {
        if bit >= self.width_bits {
            return None;
        }
        let ord = self.base_of(k)?.checked_add(bit as u64)?;
        (ord <= ORDINAL_MAX).then_some(ord)
    }
}

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

    /// A derived `PartialOrd` would compare `limbs[0]` first and get this
    /// backwards, which is why `Ord` is hand-written.
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
    fn a_dense_layout_puts_bit_j_of_integer_k_where_arithmetic_expects_it() {
        let l = IntLayout::dense(100);
        assert_eq!(l.stride, 100);
        assert_eq!(l.limbs(), 2);
        assert_eq!(l.ordinal_at(0, 0), Some(0));
        assert_eq!(l.ordinal_at(0, 99), Some(99));
        assert_eq!(l.ordinal_at(1, 0), Some(100));
        assert_eq!(l.ordinal_at(0, 100), None);
    }

    #[test]
    fn word_aligned_rounds_the_stride_up_and_dense_does_not() {
        assert_eq!(IntLayout::word_aligned(100).stride, 128);
        assert_eq!(IntLayout::word_aligned(128).stride, 128);
        assert_eq!(IntLayout::dense(100).stride, 100);
    }

    #[test]
    fn addressing_stops_at_the_ordinal_ceiling() {
        let l = IntLayout::dense(64);
        // The last integer that fits ends exactly at ORDINAL_MAX.
        let k = ORDINAL_MAX / 64;
        assert!(l.base_of(k).is_some());
        // `u64::MAX` is not an ordinal, so the bit that would land there is
        // unaddressable rather than wrapping.
        assert_eq!(l.ordinal_at(k, 63), None);
        assert_eq!(l.base_of(u64::MAX), None);
    }

    /// Without this the guard could return `true` unconditionally and every
    /// normalization assertion in the module would be vacuous. The direct
    /// analogue of `tail_is_clear_notices_a_raw_word_write`.
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

    #[test]
    fn straddling_follows_from_the_stride_and_not_from_the_width() {
        // 10 000 bits is far short of a chunk, and integer 6 crosses one anyway.
        let dense = IntLayout::dense(10_000);
        assert_eq!(dense.straddles(0), Some(false));
        assert_eq!(dense.straddles(6), Some(true));
        // The chunk-aligned spelling cannot straddle at any index.
        let aligned = IntLayout::chunk_aligned(10_000).unwrap();
        for k in 0..8 {
            assert_eq!(aligned.straddles(k), Some(false), "k = {k}");
        }
        assert_eq!(IntLayout::chunk_aligned(crate::CHUNK_CARD + 1), None);
    }

    #[test]
    fn a_layout_with_an_overlapping_stride_is_rejected() {
        assert!(IntLayout::dense(64).check().is_ok());
        assert!(IntLayout {
            width_bits: 0,
            stride: 8
        }
        .check()
        .is_err());
        assert!(IntLayout {
            width_bits: 64,
            stride: 63
        }
        .check()
        .is_err());
        // An over-large stride is a caller's choice, not an error.
        assert!(IntLayout {
            width_bits: 64,
            stride: 1 << 20
        }
        .check()
        .is_ok());
    }
}
