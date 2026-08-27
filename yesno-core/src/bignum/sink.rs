//! [`BigUint`] -> `OrdSet`: the scatter, and the other half of the seam.
//!
//! # Why a sink rather than a per-integer encode
//!
//! Turning one integer into container bytes means picking an array / bitmap /
//! run representation and running
//! [`Container::optimize`](crate::Container::optimize), which re-selects the
//! encoding by serialized size. Doing that per integer would pay it once per
//! value in a series that shares its containers — at `IntLayout::dense(64)` a
//! single chunk holds 1024 integers, so a per-integer encode would re-encode the
//! same chunk 1024 times.
//!
//! So chained work — read, add, multiply, reduce — stays in [`BigUint`], and the
//! sink encodes **once**, at [`IntSink::build`].
//!
//! # Indices must strictly increase, and this diverges from `MatrixSink`
//!
//! [`MatrixSink::place`](crate::matrix::MatrixSink::place) unions a repeated
//! index, and that is defensible there: OR-ing two boolean matrices *is*
//! addition under [`Semiring::Boolean`](crate::matrix::Semiring). OR-ing two
//! integers is not addition, not maximum, and not anything a caller could have
//! meant — placing 5 and then 3 at one index yields 7. So a repeat has no
//! correct interpretation and must be refused rather than given one.
//!
//! Requiring strictly increasing indices makes a repeat impossible to express,
//! costs one comparison, and needs no memory of what has been placed. The
//! cost is that out-of-order writes are refused; a caller with unordered values
//! sorts them first, which is what `build` would do to the ordinals anyway.

use super::{BigUint, IntLayout};
use crate::pack::OrdinalSink;
use crate::{CodecError, OrdSet, Result, ORDINAL_MAX};

/// Accumulates integer placements and encodes them into an `OrdSet` in one pass.
///
/// ```
/// use yesno_core::bignum::{BigUint, IntLayout, IntSink};
///
/// let layout = IntLayout::dense(64);
/// let mut sink = IntSink::new(layout);
/// sink.place(0, &BigUint::from_u64(7)).unwrap();
/// sink.place(1, &BigUint::from_u64(1 << 40)).unwrap();
/// let set = sink.build();
///
/// assert_eq!(set.read_int(0, &layout).unwrap(), BigUint::from_u64(7));
/// assert_eq!(set.read_int(1, &layout).unwrap(), BigUint::from_u64(1 << 40));
/// // Three bits for 7, one for the power of two.
/// assert_eq!(set.len(), 4);
/// ```
#[derive(Clone, Debug)]
pub struct IntSink {
    layout: IntLayout,
    ordinals: OrdinalSink,
    last: Option<u64>,
}

impl IntSink {
    pub fn new(layout: IntLayout) -> IntSink {
        IntSink {
            layout,
            ordinals: OrdinalSink::new(),
            last: None,
        }
    }

    /// Record `v` at index `k`.
    ///
    /// # Errors
    ///
    /// - the layout is not self-consistent — see [`IntLayout::check`];
    /// - `k` is not strictly greater than the previous index — see the module
    ///   header for why a repeat cannot simply be unioned;
    /// - `v` needs more than `width_bits` bits, which is
    ///   [`CodecError::Invariant`]. It is neither truncated nor clamped: a
    ///   value that does not fit is not the same value with its top bits
    ///   removed, and it is not `2^width_bits - 1` either. A caller who *wants*
    ///   the cyclic reading spells it —
    ///   `place( k, &v.truncate( layout.width_bits as u64 ) )` — so the width
    ///   appears at the call site rather than as a hidden policy. See
    ///   [`BigUint::truncate`] for why saturation is not offered at all;
    /// - the integer would reach above [`ORDINAL_MAX`], which is
    ///   [`CodecError::OrdinalOutOfRange`]. `u64::MAX` is not an ordinal
    ///   ( invariant I8 ), so such an integer cannot be stored at all rather
    ///   than being silently truncated. The reported ordinal saturates at
    ///   `u64::MAX`: an absurd `k` can put the true value past what a `u64`
    ///   holds, and that value is out of range either way.
    ///
    /// Every check is made **before** anything is written, so a failed `place`
    /// leaves the sink exactly as it was.
    ///
    /// The ceiling test is on the integer's **span**, not on "the last bit
    /// written". A sparse value's top set bit can be far below `width_bits`, and
    /// admitting it on that basis would make whether a placement is accepted
    /// depend on the value rather than on the layout — so the same index would
    /// succeed for 1 and fail for `2^(width_bits-1)`.
    pub fn place(&mut self, k: u64, v: &BigUint) -> Result<()> {
        self.layout.check()?;
        if let Some(prev) = self.last {
            if k <= prev {
                return Err(CodecError::Invariant(
                    "integer sink indices must strictly increase",
                ));
            }
        }
        if v.bit_len() > self.layout.width_bits as u64 {
            return Err(CodecError::Invariant(
                "integer is wider than the sink's layout",
            ));
        }
        let addressable = self.layout.base_of(k).filter(|b| {
            b.checked_add(self.layout.span_bits() - 1)
                .is_some_and(|t| t <= ORDINAL_MAX)
        });
        let Some(base) = addressable else {
            // Computed in u128 so the report survives an overflowing `k`.
            let reported =
                k as u128 * self.layout.stride as u128 + (self.layout.width_bits as u128 - 1);
            return Err(CodecError::OrdinalOutOfRange {
                ordinal: reported.min(u64::MAX as u128) as u64,
            });
        };
        // An integer is a packing with one line, so its bits go out as one:
        // `bit < width_bits` follows from the bit-length check, and the span
        // check put `base + width_bits - 1` inside the universe.
        self.ordinals.push_line(base, v.limbs());
        self.last = Some(k);
        Ok(())
    }

    /// Encode every placement into an `OrdSet`, choosing each container's
    /// representation once.
    pub fn build(self) -> OrdSet {
        self.ordinals.build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_series_round_trips_through_the_boundary() {
        let l = IntLayout::dense(128);
        let vals = [
            BigUint::zero(),
            BigUint::one(),
            BigUint::from_u64(u64::MAX),
            BigUint::from_limbs_le(vec![0x0123_4567_89ab_cdef, 0xfedc_ba98]),
            BigUint::from_limbs_le(vec![u64::MAX, u64::MAX]),
        ];
        let mut sink = IntSink::new(l);
        for (k, v) in vals.iter().enumerate() {
            sink.place(k as u64, v).unwrap();
        }
        let s = sink.build();
        for (k, v) in vals.iter().enumerate() {
            assert_eq!(s.read_int(k as u64, &l).as_ref(), Some(v), "index {k}");
        }
        assert_eq!(s.int_count(&l), 5);
    }

    #[test]
    fn a_repeated_index_is_refused_rather_than_unioned() {
        let mut sink = IntSink::new(IntLayout::dense(64));
        sink.place(3, &BigUint::from_u64(5)).unwrap();
        // 5 | 3 == 7, which is neither 5 nor 3 nor their sum.
        assert!(sink.place(3, &BigUint::from_u64(3)).is_err());
        assert!(sink.place(2, &BigUint::from_u64(3)).is_err());
        assert!(sink.place(4, &BigUint::from_u64(3)).is_ok());
        let s = sink.build();
        assert_eq!(
            s.read_int(3, &IntLayout::dense(64)).unwrap(),
            BigUint::from_u64(5),
            "the refused placement must not have written anything"
        );
    }

    #[test]
    fn a_value_wider_than_the_layout_is_rejected_not_truncated() {
        let l = IntLayout::dense(8);
        let mut sink = IntSink::new(l);
        assert!(sink.place(0, &BigUint::from_u64(255)).is_ok());
        let mut sink = IntSink::new(l);
        assert!(sink.place(0, &BigUint::from_u64(256)).is_err());
        // And nothing was written on the way to deciding that.
        assert_eq!(sink.build().len(), 0);
    }

    #[test]
    fn a_placement_reaching_past_the_ordinal_ceiling_reports_the_ordinal() {
        let l = IntLayout::dense(64);
        let mut sink = IntSink::new(l);
        let last = ORDINAL_MAX / 64;
        match sink.place(last, &BigUint::one()) {
            Err(CodecError::OrdinalOutOfRange { ordinal }) => {
                assert_eq!(ordinal, u64::MAX);
            }
            other => panic!("expected OrdinalOutOfRange, got {other:?}"),
        }
        // The value is irrelevant to the decision: a one-bit value at the
        // same index is refused for the same reason a full-width one is.
        let mut sink = IntSink::new(l);
        assert!(sink.place(last, &BigUint::zero()).is_err());
    }

    #[test]
    fn an_absurd_index_saturates_the_report_rather_than_overflowing() {
        let mut sink = IntSink::new(IntLayout::dense(64));
        match sink.place(u64::MAX, &BigUint::one()) {
            Err(CodecError::OrdinalOutOfRange { ordinal }) => assert_eq!(ordinal, u64::MAX),
            other => panic!("expected OrdinalOutOfRange, got {other:?}"),
        }
    }

    #[test]
    fn a_padded_stride_keeps_the_integers_apart() {
        let l = IntLayout {
            width_bits: 8,
            stride: 64,
        };
        let mut sink = IntSink::new(l);
        sink.place(0, &BigUint::from_u64(0xff)).unwrap();
        sink.place(1, &BigUint::from_u64(0xff)).unwrap();
        let s = sink.build();
        assert_eq!(s.read_int(0, &l).unwrap(), BigUint::from_u64(0xff));
        assert_eq!(s.read_int(1, &l).unwrap(), BigUint::from_u64(0xff));
        assert_eq!(s.len(), 16);
    }

    #[test]
    fn a_straddling_index_round_trips() {
        let l = IntLayout::dense(10_000);
        let v = BigUint::from_limbs_le(vec![u64::MAX; 100]);
        let mut sink = IntSink::new(l);
        // Integer 6 spans bits 60 000..70 000 and crosses the chunk boundary.
        sink.place(6, &v).unwrap();
        let s = sink.build();
        assert_eq!(s.read_int(6, &l).unwrap(), v);
        assert_eq!(s.read_int(5, &l).unwrap(), BigUint::zero());
        assert_eq!(s.read_int(7, &l).unwrap(), BigUint::zero());
    }
}
