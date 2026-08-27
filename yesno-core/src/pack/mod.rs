//! Where a strided object's bits live in the ordinal space — the facility both
//! [`matrix`](crate::matrix) and [`bignum`](crate::bignum) are built on.
//!
//! An `OrdSet` is a bit vector over `[0, ORDINAL_MAX]`. Fix a [`Packing`] and
//! that bit vector becomes a series of objects, each a stack of *lines* of bits.
//! A matrix reads one line as a row; an integer has a single line and reads it
//! as limbs. The two lenses differ in what they *do* with the bits; they do not
//! differ in where the bits are, and this module is that agreement.
//!
//! # Three axes of `u64`, and they are independent
//!
//! The crate has a db **key** ( which set ) and an **ordinal** ( a member of
//! that set ). A packing adds a third: the **object index** `k`, which addresses
//! a strided range of ordinals *within* one set. It is not a key and it is not
//! an ordinal, and confusing it with either is the mistake this type exists to
//! make hard.
//!
//! # The seam is paid here, and nowhere else
//!
//! `line_bits`, `line_stride` and `object_stride` are all arbitrary, so a line
//! can begin at any bit offset and an object can straddle the 65 536-bit chunk
//! boundary. If every kernel handled that, the shift-and-carry path would live
//! in the matrix product, the transpose, the inverse, the addition, the
//! multiplication and the division at once — and the arithmetic kernels already
//! have carry logic of their own, which is exactly the confusion worth
//! preventing.
//!
//! So `gather` normalizes on the way in and each lens's sink scatters on the
//! way out. One shift path inbound, one outbound, and every kernel in between is
//! defined only on its lens's canonical form.
//!
//! # Straddling is a property of the stride, not of the size
//!
//! `span_bits() < 65536` does **not** by itself keep an object inside one chunk.
//! At `object_stride = 10 000`, object 6 spans bits 60 000..70 000 and crosses
//! the boundary. Any fast path must *test* containment — [`Packing::straddles`]
//! — rather than infer it from the size, and a test generator that omits a
//! straddling index leaves the seam untested while every property still passes.
//!
//! The sharpening that costs a bug if missed: **the straddling unit is the
//! word, not the object.** 65 536 is a multiple of 64, so under a stride that is
//! not, a single destination *word* can cross the boundary. A reader that copied
//! whole words per chunk would be wrong for exactly one word, and every test
//! whose `line_bits` is a multiple of 64 would still pass.
//!
//! # What this module is not
//!
//! **Not a lens.** There is no canonical value type here and no algebra —
//! [`BitMatrix`](crate::matrix::BitMatrix) and [`BigUint`](crate::bignum::BigUint)
//! own those, and their canonical-form invariants differ on purpose
//! ( a masked padding tail against no trailing zero limb ). This module owns
//! addressing and the two transfers, and stops there.
//!
//! **Not a projection.** A packing is an *injection* of the object index
//! space into the ordinal space. Nothing here maps several ordinals onto one.

use crate::{CodecError, Result, CHUNK_CARD, ORDINAL_MAX};

mod gather;
mod seek;
mod sink;

pub use sink::OrdinalSink;

pub(crate) use gather::gather;
pub(crate) use seek::try_gather;

/// Where the bits of a strided object live in the ordinal space.
///
/// Bit `b` of line `l` of object `k` sits at ordinal
///
/// ```text
/// k * object_stride + l * line_stride + b
/// ```
///
/// The strides are separate knobs on purpose — every layout worth naming is an
/// instance of this one struct:
///
/// | what you want | how to say it |
/// |---|---|
/// | densely packed, 1:1 with ordinals | [`Packing::dense`] |
/// | lines padded to a word, so every load is aligned | [`Packing::word_aligned`] |
/// | one object per chunk, so none ever straddles | [`Packing::chunk_aligned`] |
///
/// # The dense packing has a property worth protecting
///
/// When `line_stride == line_bits` and `object_stride == lines * line_bits`, the
/// ordinal set and the object series are the *same object*. So
/// [`OrdSet::and`](crate::OrdSet::and) / `or` / `xor` already **are** the
/// elementwise algebra of whichever lens is reading, for free. Padding buys
/// aligned loads and gives that up; both are legitimate and the choice is the
/// caller's.
///
/// # This is the shape, not the meaning
///
/// A `Packing` says nothing about how the bits are *read*. Under `lines == 1` it
/// is an integer to [`bignum`](crate::bignum) and a single-row matrix to
/// [`matrix`](crate::matrix), and those two readings agree bit for bit —
/// deliberately, since a one-row matrix's row words are exactly an integer's
/// limb vector.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Packing {
    /// Bits addressed within one line. Bits between `line_bits` and
    /// `line_stride` are padding, addressed by nothing.
    pub line_bits: u32,
    /// Lines in one object.
    pub lines: u32,
    /// Ordinals between the start of consecutive lines. Must be `>= line_bits`.
    pub line_stride: u32,
    /// Ordinals between the start of consecutive objects. Must be at least
    /// `lines * line_stride`.
    pub object_stride: u64,
}

impl Packing {
    /// Densely packed: no padding anywhere, so the ordinal space and the object
    /// series coincide exactly.
    pub fn dense(lines: u32, line_bits: u32) -> Packing {
        Packing {
            line_bits,
            lines,
            line_stride: line_bits,
            object_stride: lines as u64 * line_bits as u64,
        }
    }

    /// Each line starting on a `u64` boundary.
    ///
    /// Wastes up to 63 bits per line and gives up the identity described on
    /// [`Packing`], in exchange for every line transfer being word-aligned.
    pub fn word_aligned(lines: u32, line_bits: u32) -> Packing {
        let stride = line_bits.div_ceil(64).saturating_mul(64);
        Packing {
            line_bits,
            lines,
            line_stride: stride,
            object_stride: lines as u64 * stride as u64,
        }
    }

    /// Each object starting on a 65 536-bit chunk boundary, so **none ever
    /// straddles**.
    ///
    /// `None` when the object does not fit in a chunk, where the promise is
    /// unachievable. This is given a constructor because the hazard it avoids is
    /// the one the module header warns about, and a caller who has just read
    /// that warning should not have to re-derive the stride.
    pub fn chunk_aligned(lines: u32, line_bits: u32) -> Option<Packing> {
        let dense = Packing::dense(lines, line_bits);
        (dense.span_bits() <= CHUNK_CARD as u64).then_some(Packing {
            object_stride: CHUNK_CARD as u64,
            ..dense
        })
    }

    /// Words needed to hold one line of the canonical form.
    #[inline]
    pub fn line_words(&self) -> usize {
        (self.line_bits as usize).div_ceil(64)
    }

    /// Ordinals from the first to the last bit of one object, inclusive.
    ///
    /// Not `lines * line_bits`: a padded `line_stride` makes an object occupy
    /// more of the ordinal space than it has bits. The final line's padding is
    /// not counted, because nothing addresses it.
    #[inline]
    pub fn span_bits(&self) -> u64 {
        let lines = self.lines as u64;
        if lines == 0 {
            return 0;
        }
        (lines - 1) * self.line_stride as u64 + self.line_bits as u64
    }

    /// Is this packing self-consistent?
    ///
    /// Rejects a zero dimension, a `line_stride` that would overlap consecutive
    /// lines, and an `object_stride` that would overlap consecutive objects. A
    /// deliberately over-large stride is fine — that is a caller choice, not an
    /// error.
    ///
    /// Each lens keeps its **own** `check`, because the message a caller
    /// needs names the caller's vocabulary — "integer layout has zero width"
    /// rather than "packing has a zero dimension". This one is what the transfer
    /// kernels rely on.
    pub fn check(&self) -> Result<()> {
        if self.lines == 0 || self.line_bits == 0 {
            return Err(CodecError::Invariant("packing has a zero dimension"));
        }
        if self.line_stride < self.line_bits {
            return Err(CodecError::Invariant(
                "packing line_stride is shorter than a line",
            ));
        }
        if self.object_stride < self.lines as u64 * self.line_stride as u64 {
            return Err(CodecError::Invariant(
                "packing object_stride overlaps consecutive objects",
            ));
        }
        Ok(())
    }

    /// First ordinal of object `k`, or `None` if it is out of the universe.
    #[inline]
    pub fn base_of(&self, k: u64) -> Option<u64> {
        let base = k.checked_mul(self.object_stride)?;
        (base <= ORDINAL_MAX).then_some(base)
    }

    /// Last ordinal of object `k`, or `None` if the object is not addressable.
    ///
    /// The whole object must fit, which is exactly the condition its top bit
    /// imposes — `u64::MAX` is not an ordinal ( invariant I8 ), so an object
    /// reaching it is refused rather than truncated.
    #[inline]
    pub fn last_of(&self, k: u64) -> Option<u64> {
        let last = self.base_of(k)?.checked_add(self.span_bits() - 1)?;
        (last <= ORDINAL_MAX).then_some(last)
    }

    /// Ordinal holding bit `b` of line `l` of object `k`.
    ///
    /// `None` if the indices are out of range for the packing, or if the ordinal
    /// would exceed [`ORDINAL_MAX`].
    #[inline]
    pub fn ordinal_at(&self, k: u64, l: u32, b: u32) -> Option<u64> {
        if l >= self.lines || b >= self.line_bits {
            return None;
        }
        let off = (l as u64).checked_mul(self.line_stride as u64)?;
        let ord = self.base_of(k)?.checked_add(off)?.checked_add(b as u64)?;
        (ord <= ORDINAL_MAX).then_some(ord)
    }

    /// Does object `k` cross a 65 536-bit chunk boundary? `None` if `k` is not
    /// addressable at all.
    ///
    /// Exposed because the module header names straddling as *the* hazard, and a
    /// caller choosing an `object_stride` needs a way to check the claim rather
    /// than re-deriving the arithmetic. [`Packing::chunk_aligned`] is the answer
    /// for a caller who would rather not think about it.
    pub fn straddles(&self, k: u64) -> Option<bool> {
        let base = self.base_of(k)?;
        let last = self.last_of(k)?;
        Some(crate::split(base).0 != crate::split(last).0)
    }

    /// How many objects `max_ordinal` reaches into.
    ///
    /// Counts addressable positions, not non-empty objects: an all-zero object
    /// below the highest set ordinal is still counted, because zero is a
    /// legitimate value and absence cannot distinguish it from one.
    pub fn count_below(&self, max_ordinal: Option<u64>) -> u64 {
        if self.check().is_err() || self.object_stride == 0 {
            return 0;
        }
        match max_ordinal {
            None => 0,
            Some(m) => m / self.object_stride + 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dense_packing_is_the_identity_on_the_ordinal_space() {
        let p = Packing::dense(4, 8);
        assert_eq!(p.object_stride, 32);
        assert_eq!(p.span_bits(), 32);
        // Object 3, line 2, bit 5 is ordinal 3*32 + 2*8 + 5.
        assert_eq!(p.ordinal_at(3, 2, 5), Some(96 + 16 + 5));
    }

    #[test]
    fn one_line_reproduces_the_integer_shape() {
        let p = Packing::dense(1, 100);
        assert_eq!(p.span_bits(), 100);
        assert_eq!(p.object_stride, 100);
        assert_eq!(p.line_words(), 2);
        // Bit j of object k is at k*100 + j, which is exactly IntLayout's rule.
        assert_eq!(p.ordinal_at(7, 0, 3), Some(703));
    }

    #[test]
    fn padding_above_the_last_line_is_not_counted_in_the_span() {
        let p = Packing::word_aligned(3, 100);
        assert_eq!(p.line_stride, 128);
        // Two full strides plus the last line's real bits, not a third stride.
        assert_eq!(p.span_bits(), 256 + 100);
        assert_eq!(p.object_stride, 384);
    }

    /// The seam. `span_bits() < 65536` does not imply chunk containment, and
    /// this is the index that proves it.
    #[test]
    fn a_small_object_can_still_straddle_a_chunk_boundary() {
        let p = Packing::dense(1, 10_000);
        assert!(p.span_bits() < CHUNK_CARD as u64);
        // Object 6 spans bits 60 000..70 000, crossing 65 536.
        assert_eq!(p.straddles(6), Some(true));
        assert_eq!(p.straddles(0), Some(false));
        assert_eq!(p.straddles(5), Some(false));
    }

    #[test]
    fn a_chunk_aligned_packing_never_straddles() {
        let p = Packing::chunk_aligned(4, 100).unwrap();
        assert_eq!(p.object_stride, CHUNK_CARD as u64);
        for k in [0u64, 1, 6, 7, 1000] {
            assert_eq!(p.straddles(k), Some(false), "object {k}");
        }
        // An object that does not fit in a chunk cannot be promised this.
        assert!(Packing::chunk_aligned(2, 65_536).is_none());
        assert!(Packing::chunk_aligned(1, 65_536).is_some());
    }

    #[test]
    fn check_rejects_overlap_and_zero_but_allows_a_generous_stride() {
        assert!(Packing::dense(4, 8).check().is_ok());
        assert!(Packing::dense(0, 8).check().is_err());
        assert!(Packing::dense(4, 0).check().is_err());
        assert!(Packing {
            line_stride: 4,
            ..Packing::dense(4, 8)
        }
        .check()
        .is_err());
        assert!(Packing {
            object_stride: 8,
            ..Packing::dense(4, 8)
        }
        .check()
        .is_err());
        // Deliberately over-large is a caller choice, not an error.
        assert!(Packing {
            object_stride: 1 << 20,
            ..Packing::dense(4, 8)
        }
        .check()
        .is_ok());
    }

    #[test]
    fn an_object_reaching_past_the_ordinal_ceiling_is_not_addressable() {
        let p = Packing::dense(1, 64);
        let last = ORDINAL_MAX / 64;
        // The final object's top bit would land on `u64::MAX`, which is not an
        // ordinal, so the whole object is refused rather than truncated.
        assert_eq!(p.last_of(last), None);
        assert!(p.last_of(last - 1).is_some());
        assert_eq!(p.straddles(last), None);
    }

    /// **The property that says the shared type is real rather than a forced
    /// factoring**, and it could not be written while the two lenses each owned
    /// a copy of the walk.
    ///
    /// A one-line packing is an integer to `bignum` and a single-row matrix to
    /// `matrix`. Those are two independent public entry points reading the same
    /// ordinals under the same addressing, so they must agree bit for bit — a
    /// `BitMatrix`'s row words and a `BigUint`'s limbs are the same canonical
    /// form at `lines == 1`.
    ///
    /// The indices are chosen to include **straddling** ones. Restricting
    /// them to chunk-contained objects would leave the seam untested while every
    /// assertion here still passed.
    #[test]
    fn a_one_line_packing_reads_the_same_as_a_single_row_matrix() {
        use crate::bignum::IntLayout;
        use crate::matrix::Layout;
        use crate::OrdSet;

        let mut checked = 0u32;
        let mut nonzero = 0u32;
        let mut straddling = 0u32;
        for width in [1u32, 7, 64, 100, 128, 1000, 10_000] {
            for stride in [width as u64, width as u64 + 3, 65_536] {
                if stride < width as u64 {
                    continue;
                }
                let int = IntLayout {
                    width_bits: width,
                    stride,
                };
                let mat = Layout {
                    line_stride: width,
                    matrix_stride: stride,
                    ..Layout::dense(1, width)
                };
                assert_eq!(int.packing(), mat.packing(), "w={width} s={stride}");

                // Boundary-biased source: values around every chunk edge the
                // strides can reach, plus a sparse spread.
                let s = OrdSet::from_iter_unsorted(
                    (0..400u64)
                        .map(|i| i * 977)
                        .chain([0, 1, 65_535, 65_536, 65_537, 131_071, 131_072]),
                );

                for k in [0u64, 1, 5, 6, 7, 13, 655, 1000] {
                    let (Some(v), Some(m)) = (s.read_int(k, &int), s.read_matrix(k, &mat)) else {
                        continue;
                    };
                    assert_eq!(m.rows(), 1);
                    for b in 0..width {
                        assert_eq!(
                            v.bit(b as u64),
                            m.get(0, b),
                            "w={width} s={stride} k={k} bit={b}"
                        );
                    }
                    checked += 1;
                    nonzero += u32::from(m.count_ones() > 0);
                    straddling += u32::from(int.straddles(k) == Some(true));
                }
            }
        }
        // Without these the whole test passes on pairs of zeros.
        assert!(checked > 50, "only {checked} comparisons");
        assert!(nonzero > 20, "only {nonzero} of {checked} had any bits");
        assert!(straddling > 5, "only {straddling} straddling objects");
    }

    /// The two lenses' own `check`s and this one must agree on what is legal, or
    /// a layout a lens accepts could reach a kernel that rejects it.
    #[test]
    fn each_lens_check_agrees_with_the_packing_check() {
        use crate::bignum::IntLayout;
        use crate::matrix::Layout;

        for width in [0u32, 1, 8, 64] {
            for stride in [0u64, 1, 8, 64, 128] {
                let l = IntLayout {
                    width_bits: width,
                    stride,
                };
                assert_eq!(
                    l.check().is_ok(),
                    l.packing().check().is_ok(),
                    "IntLayout w={width} s={stride}"
                );
            }
        }
        for rows in [0u32, 1, 4] {
            for cols in [0u32, 1, 7] {
                for line_stride in [0u32, 1, 7, 16] {
                    for matrix_stride in [0u64, 16, 1 << 12] {
                        let l = Layout {
                            line_stride,
                            matrix_stride,
                            ..Layout::dense(rows, cols)
                        };
                        assert_eq!(
                            l.check().is_ok(),
                            l.packing().check().is_ok(),
                            "Layout {l:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn count_below_reports_addressable_positions_not_non_empty_ones() {
        let p = Packing::dense(1, 64);
        assert_eq!(p.count_below(None), 0);
        // One bit inside object 3 makes 0..=3 addressable, zeros included.
        assert_eq!(p.count_below(Some(3 * 64 + 1)), 4);
        // A packing that does not check answers zero rather than a wrong number.
        assert_eq!(Packing::dense(1, 0).count_below(Some(1)), 0);
    }
}
