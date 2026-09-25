//! `BigUint` -> `OrdSet`: the inverse map.

use super::BigUint;
use crate::pack::OrdinalSink;
use crate::{CodecError, OrdSet, Result, ORDINAL_MAX};

impl OrdSet {
    /// The set of an integer's set bits.
    ///
    /// The inverse of [`OrdSet::read_int`] at full width, and a map rather than
    /// a builder: one integer is one set, so there is no index to advance, no
    /// ordering rule between placements, and nothing to accumulate.
    ///
    /// # Errors
    ///
    /// Only if the value's top bit would land on `u64::MAX`, which is not an
    /// ordinal ( invariant I8 ). Such an integer cannot be represented at all
    /// rather than being silently truncated. It needs about `2^58` limbs and is
    /// therefore unreachable in practice, but it is the one case that stops this
    /// being total -- which is why it is a `Result` and not a `From` impl, for
    /// the reason [`BigUint::sub`] returns an `Option`.
    pub fn from_int(v: &BigUint) -> Result<OrdSet> {
        let bits = v.bit_len();
        if bits > 0 && bits - 1 > ORDINAL_MAX {
            return Err(CodecError::OrdinalOutOfRange { ordinal: u64::MAX });
        }
        let mut sink = OrdinalSink::new();
        // Base zero, so the limb vector goes out as one line and every chunk
        // boundary is a limb boundary.
        sink.push_line(0, v.limbs());
        Ok(sink.build())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two directions compose to the identity, which is what makes this a
    /// change of view rather than an encoding.
    #[test]
    fn reading_back_what_was_written_is_the_identity() {
        for v in [
            BigUint::zero(),
            BigUint::one(),
            BigUint::from_u64(11),
            BigUint::from_u64(u64::MAX),
            BigUint::from_limbs_le(vec![0xDEAD_BEEF_CAFE_BABE, 0x1234_5678]),
            BigUint::one().shl(200_000),
            BigUint::one().shl(65_536).sub(&BigUint::one()).unwrap(),
        ] {
            let set = OrdSet::from_int(&v).unwrap();
            assert_eq!(set.read_int(u64::MAX), v);
        }
    }

    #[test]
    fn zero_is_the_empty_set() {
        let set = OrdSet::from_int(&BigUint::zero()).unwrap();
        assert!(set.is_empty());
    }

    /// The other composition: a set survives a round trip through the integer.
    #[test]
    fn a_set_survives_the_round_trip_through_a_value() {
        let s = OrdSet::from_iter_unsorted([0u64, 1, 3, 64, 65_535, 65_536, 200_000]);
        assert_eq!(OrdSet::from_int(&s.read_int(u64::MAX)).unwrap(), s);
    }
}
