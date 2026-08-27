//! `OrdSet` -> [`BigUint`]: the integer half of the gather.
//!
//! # The transfer itself lives in [`pack`](crate::pack)
//!
//! An integer occupies a contiguous run of `width_bits` ordinals, which is a
//! [`Packing`](crate::pack::Packing) with **one line** — and a one-line
//! packing's canonical words *are* a limb vector. So the walk that fills them is
//! [`pack::gather`](crate::pack), shared with [`matrix`](crate::matrix), and
//! what is left here is establishing the canonical invariant and deciding what
//! is addressable.
//!
//! # The seeking arm now applies here too, and that is why the two were merged
//!
//! A `Container` iterates from its first value and cannot seek, so gathering an
//! integer that does **not** start on a chunk boundary used to discard every
//! value in that chunk below it — `O(values before base)` on every read. That
//! was the standing gap this module carried and the matrix module did not.
//! `pack::try_gather` closes it: [`OrdSet::read_int`] tries the seeking arm
//! first and falls back to the generic walk, which remains the oracle and is
//! never deleted because the arm is faster ( same contract as
//! [`ops::generic`](crate::ops::generic) ).
//!
//! # An unwritten index reads as zero
//!
//! Absence is not `None`. A set that no writer ever filled at index `k` has
//! no bits in `k`'s span, and the integer there is zero — which is a perfectly
//! good value. `None` is reserved for "this index is not addressable at all",
//! which is a statement about the layout and the universe, not about the data.

use super::{BigUint, IntLayout};
use crate::pack;
use crate::{OrdSet, ORDINAL_MAX};

impl OrdSet {
    /// Gather integer `k` into the canonical limb form.
    ///
    /// `None` if `layout` is not self-consistent ( [`IntLayout::check`] ) or if
    /// the integer would reach past [`ORDINAL_MAX`] — `u64::MAX` is not an
    /// ordinal ( invariant I8 ), so such an integer is not addressable at all.
    ///
    /// Reading at a **narrower** `width_bits` than the value was written
    /// under yields exactly `x mod 2^width_bits`. That is a consequence of the
    /// least-significant-bit-first ordering and it is a deliberate, testable
    /// identity rather than an accident — see [`IntLayout`].
    pub fn read_int(&self, k: u64, layout: &IntLayout) -> Option<BigUint> {
        layout.check().ok()?;
        // The whole integer must be addressable, which is exactly the condition
        // its top bit imposes.
        let base = layout.base_of(k)?;
        let last = base.checked_add(layout.span_bits() - 1)?;
        if last > ORDINAL_MAX {
            return None;
        }
        let packing = layout.packing();
        let words = layout.limbs();
        let mut limbs = vec![0u64; words];
        // The seeking arm decides before it writes, so a decline leaves `limbs`
        // untouched and the generic path starts from a clean zero vector.
        if !pack::try_gather(self, base, &packing, &mut limbs, words) {
            pack::gather(self, base, &packing, &mut limbs, words);
        }
        // The one place the canonical invariant is established, so no kernel
        // ever has to wonder.
        Some(BigUint::from_limbs_le(limbs))
    }

    /// How many integers this set's occupied span reaches into under `layout`.
    ///
    /// Counts addressable positions, not non-zero integers: a zero below the
    /// highest set ordinal is still counted, because zero is a legitimate value
    /// and absence cannot distinguish it from one.
    pub fn int_count(&self, layout: &IntLayout) -> u64 {
        layout.packing().count_below(self.max())
    }

    /// Is integer `k` zero — answered without decoding a payload?
    ///
    /// [`OrdSet::range_summary`] resolves the span from container metadata and
    /// popcounts, and `Container::len()` is `O(1)` on every representation ( QG
    /// §2 ), so the answer comes from the *stored* form before a limb is ever
    /// gathered.
    ///
    /// It is also the *cheap* half of that answer, which the wording above
    /// implied and the code did not deliver until 2026-09-06: `range_summary`
    /// used to count the whole span and compare, so this predicate cost exactly
    /// what `len_in_range` over the same span costs. It now returns at the first
    /// set bit — [`Container::is_range_empty`](crate::container::Container::is_range_empty)
    /// per chunk — so a non-zero integer is refuted by its lowest set bit and a
    /// zero one is confirmed by container metadata alone.
    ///
    /// Nothing outside the tests calls this. An earlier version of this
    /// comment claimed it was "the divisor test every division starts with",
    /// which was false — [`BigUint::divrem`] tests its own operand after the
    /// gather and never consults a layout.
    ///
    /// `true` for an index that is not addressable: nothing is there.
    pub fn int_is_zero(&self, k: u64, layout: &IntLayout) -> bool {
        if layout.check().is_err() {
            return true;
        }
        let Some(base) = layout.base_of(k) else {
            return true;
        };
        let Some(hi) = base.checked_add(layout.span_bits()) else {
            return true;
        };
        matches!(self.range_summary(base, hi), crate::RangeSummary::Empty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bignum::IntSink;
    use crate::CHUNK_CARD;

    fn set_of(ordinals: &[u64]) -> OrdSet {
        OrdSet::from_iter_unsorted(ordinals.iter().copied())
    }

    #[test]
    fn a_bit_at_ordinal_base_plus_j_carries_two_to_the_j() {
        let l = IntLayout::dense(64);
        // Integer 1 spans ordinals 64..128; bits 0 and 3 of it are set.
        let s = set_of(&[64, 67]);
        assert_eq!(s.read_int(1, &l).unwrap(), BigUint::from_u64(0b1001));
        assert_eq!(s.read_int(0, &l).unwrap(), BigUint::zero());
    }

    #[test]
    fn an_unwritten_index_reads_as_zero_not_as_none() {
        let l = IntLayout::dense(64);
        let s = set_of(&[64]);
        assert_eq!(s.read_int(0, &l), Some(BigUint::zero()));
        assert_eq!(s.read_int(9_999, &l), Some(BigUint::zero()));
        assert!(s.int_is_zero(0, &l));
        assert!(!s.int_is_zero(1, &l));
    }

    /// The seam. `width_bits < 65536` does not imply chunk containment, and
    /// this index is the one that proves it.
    #[test]
    fn an_integer_straddling_a_chunk_boundary_reads_correctly() {
        let l = IntLayout::dense(10_000);
        // Integer 6 spans bits 60 000..70 000, crossing 65 536.
        let base = 6 * 10_000;
        assert!(base < CHUNK_CARD as u64 && base + 10_000 > CHUNK_CARD as u64);
        // Bits 0, 5535 ( still in the first chunk ) and 5536 ( the first bit of
        // the second chunk ), and the top bit.
        let s = set_of(&[base, base + 5_535, base + 5_536, base + 9_999]);
        let v = s.read_int(6, &l).unwrap();
        assert!(v.bit(0));
        assert!(v.bit(5_535));
        assert!(v.bit(5_536));
        assert!(v.bit(9_999));
        assert_eq!(v.count_ones(), 4);
        assert_eq!(v.bit_len(), 10_000);
    }

    /// The limb-level sharpening of the same hazard: 65 536 is a multiple of 64,
    /// so under a stride that is not, a single **limb** can cross the chunk
    /// boundary. A reader that copied whole limbs per chunk would be wrong for
    /// exactly one limb, and every test whose width is a multiple of 64 would
    /// still pass.
    #[test]
    fn a_single_limb_crossing_the_chunk_boundary_reads_correctly() {
        let l = IntLayout::dense(100);
        // Integer 655 begins at bit 65 500; its first limb spans 65 500..65 564.
        let base = 655 * 100;
        assert_eq!(base, 65_500);
        let s = set_of(&[base + 35, base + 36, base + 99]);
        let v = s.read_int(655, &l).unwrap();
        assert!(v.bit(35), "the last bit below the chunk boundary");
        assert!(v.bit(36), "the first bit above it");
        assert!(v.bit(99));
        assert_eq!(v.count_ones(), 3);
    }

    #[test]
    fn reading_at_a_narrower_width_is_a_reduction_modulo_a_power_of_two() {
        let wide = IntLayout {
            width_bits: 128,
            stride: 128,
        };
        let narrow = IntLayout {
            width_bits: 8,
            stride: 128,
        };
        let mut sink = IntSink::new(wide);
        let v = BigUint::from_limbs_le(vec![0xdead_beef_1234_5678, 7]);
        sink.place(0, &v).unwrap();
        let s = sink.build();
        assert_eq!(s.read_int(0, &wide).unwrap(), v);
        assert_eq!(
            s.read_int(0, &narrow).unwrap(),
            BigUint::from_u64(0x78),
            "the low 8 bits, and nothing else"
        );
    }

    /// **The property the overflow decision rests on**, and the only one that
    /// discriminates against a saturating write. Truncating before the write must
    /// give the same answer as writing wide and reading narrow — and it must,
    /// because the reader has no clamp to apply: it gathers `W` ordinals and the
    /// bits above them are simply outside the range, so a narrow read *is*
    /// `x mod 2^W`. Verified against a saturating implementation of
    /// `truncate_assign`, which fails here.
    ///
    /// Nesting and the ring-homomorphism identity in `addsub.rs` do **not**
    /// discriminate — saturation satisfies both. Do not treat them as the reason.
    #[test]
    fn truncating_before_the_write_agrees_with_reading_at_that_width() {
        let stride = 256;
        let wide = IntLayout {
            width_bits: 200,
            stride,
        };
        let x = BigUint::from_limbs_le(vec![0xdead_beef_1234_5678, u64::MAX, 0x3f]);
        assert!(x.bit_len() <= 200);

        for narrow_bits in [1u32, 7, 63, 64, 65, 100, 128, 129, 199] {
            let narrow = IntLayout {
                width_bits: narrow_bits,
                stride,
            };
            let expected = x.truncate(narrow_bits as u64);

            // Written wide, read narrow.
            let mut wide_sink = IntSink::new(wide);
            wide_sink.place(0, &x).unwrap();
            let read_narrow = wide_sink.build().read_int(0, &narrow).unwrap();

            // Truncated, then written and read at the narrow width.
            let mut narrow_sink = IntSink::new(narrow);
            narrow_sink.place(0, &expected).unwrap();
            let round_tripped = narrow_sink.build().read_int(0, &narrow).unwrap();

            assert_eq!(read_narrow, expected, "width {narrow_bits}");
            assert_eq!(round_tripped, expected, "width {narrow_bits}");
        }
    }

    #[test]
    fn padding_between_integers_belongs_to_neither() {
        let l = IntLayout {
            width_bits: 8,
            stride: 16,
        };
        // Ordinal 8 is in the gap above integer 0 and below integer 1.
        let s = set_of(&[8]);
        assert_eq!(s.read_int(0, &l).unwrap(), BigUint::zero());
        assert_eq!(s.read_int(1, &l).unwrap(), BigUint::zero());
        assert!(s.read_int(0, &IntLayout::dense(16)).unwrap().bit(8));
    }

    #[test]
    fn an_index_reaching_past_the_ordinal_ceiling_is_not_addressable() {
        let l = IntLayout::dense(64);
        let s = OrdSet::new();
        let last = ORDINAL_MAX / 64;
        // The final integer's top bit would land on `u64::MAX`, which is not an
        // ordinal, so the whole integer is refused rather than truncated.
        assert_eq!(s.read_int(last, &l), None);
        assert!(s.read_int(last - 1, &l).is_some());
    }

    #[test]
    fn int_count_reports_addressable_positions_not_non_zero_ones() {
        let l = IntLayout::dense(64);
        assert_eq!(OrdSet::new().int_count(&l), 0);
        // One bit inside integer 3 makes 0..=3 addressable, zeros included.
        assert_eq!(set_of(&[3 * 64 + 1]).int_count(&l), 4);
        // A layout that does not check answers zero rather than a wrong number.
        assert_eq!(
            set_of(&[1]).int_count(&IntLayout {
                width_bits: 0,
                stride: 8
            }),
            0
        );
    }
}
