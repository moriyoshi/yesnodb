//! Canonical words -> `OrdSet`: the scatter's accumulator, and the other half
//! of the seam.
//!
//! # Why a sink rather than a per-object encode
//!
//! Turning one object into container bytes means picking an array / bitmap / run
//! representation and running
//! [`Container::optimize`](crate::Container::optimize), which re-selects the
//! encoding by serialized size. Doing that per object would pay it once per
//! value in a series that shares its containers — under `Packing::dense(1, 64)`
//! a single chunk holds 1024 objects, so a per-object encode would re-encode the
//! same chunk 1024 times.
//!
//! So chained work stays in the lens's canonical form and the sink encodes
//! **once**, at [`OrdinalSink::build`].
//!
//! # Placement policy is the lens's, not this module's
//!
//! This type owns *accumulate and encode once*. It deliberately owns nothing
//! about what placing an index twice means, because the two lenses answer that
//! differently and both are right:
//! [`MatrixSink`](crate::matrix::MatrixSink) unions a repeat, since OR-ing two
//! boolean matrices *is* addition under
//! [`Semiring::Boolean`](crate::matrix::Semiring);
//! [`IntSink`](crate::bignum::IntSink) refuses one, since OR-ing 5 and 3 yields
//! 7 and that is nothing a caller could have meant. Do not lift either policy
//! in here.

use crate::OrdSet;

/// Accumulates ordinals and encodes them into an `OrdSet` in one pass.
///
/// ```
/// use yesno_core::pack::{OrdinalSink, Packing};
///
/// let p = Packing::dense(1, 64);
/// let mut sink = OrdinalSink::new();
/// // Bits 0 and 3 of object 1.
/// sink.push(p.ordinal_at(1, 0, 0).unwrap());
/// sink.push(p.ordinal_at(1, 0, 3).unwrap());
/// let set = sink.build();
/// assert_eq!(set.len(), 2);
/// ```
#[derive(Clone, Debug, Default)]
pub struct OrdinalSink {
    ordinals: Vec<u64>,
}

impl OrdinalSink {
    pub fn new() -> OrdinalSink {
        OrdinalSink::default()
    }

    /// Record one set ordinal.
    ///
    /// Order does not matter: `build` sorts. The caller is responsible for the
    /// ordinal being addressable — every check belongs at `place` time, before
    /// anything is pushed, so that a failed placement leaves the sink exactly as
    /// it was.
    #[inline]
    pub fn push(&mut self, ordinal: u64) {
        self.ordinals.push(ordinal);
    }

    /// Record every set bit of one line, at `base` plus the bit's index.
    ///
    /// The inner loop is `x &= x - 1` over each word, so the cost is one step
    /// per *set* bit rather than one per bit.
    #[inline]
    pub fn push_line(&mut self, base: u64, words: &[u64]) {
        for (i, &w) in words.iter().enumerate() {
            let mut x = w;
            while x != 0 {
                let bit = (i as u64) * 64 + x.trailing_zeros() as u64;
                x &= x - 1;
                self.ordinals.push(base + bit);
            }
        }
    }

    /// How many ordinals have been recorded.
    ///
    /// **Test scaffolding, not API**, and `#[cfg(test)]` so that it cannot
    /// quietly become one. Its only callers are the sinks' "a failed `place`
    /// must not write" tests, which have to observe that the accumulator did not
    /// grow — a real obligation, but one no shipped caller has. Made `pub` it
    /// would also oblige a `pub is_empty` ( clippy's `len_without_is_empty` ),
    /// which nothing wants either.
    #[cfg(test)]
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.ordinals.len()
    }

    /// Encode every placement into an `OrdSet`, choosing each container's
    /// representation once.
    pub fn build(self) -> OrdSet {
        let mut s = OrdSet::from_iter_unsorted(self.ordinals);
        s.optimize();
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::Packing;

    #[test]
    fn push_line_records_one_ordinal_per_set_bit() {
        let mut sink = OrdinalSink::new();
        sink.push_line(100, &[0b1001, 0, 1]);
        assert_eq!(sink.len(), 3);
        let s = sink.build();
        assert!(s.contains(100));
        assert!(s.contains(103));
        assert!(s.contains(100 + 128));
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn build_encodes_once_and_optimizes() {
        // A long contiguous run must come back as a run container, which is what
        // `optimize` at `build` is for.
        let mut sink = OrdinalSink::new();
        for o in 0..20_000u64 {
            sink.push(o);
        }
        let s = sink.build();
        assert_eq!(s.len(), 20_000);
        assert_eq!(s.chunk_at(0).unwrap().1.kind(), crate::ContainerKind::Run);
    }

    #[test]
    fn out_of_order_pushes_are_sorted_by_build() {
        let p = Packing::dense(2, 8);
        let mut sink = OrdinalSink::new();
        sink.push(p.ordinal_at(5, 1, 7).unwrap());
        sink.push(p.ordinal_at(0, 0, 0).unwrap());
        let s = sink.build();
        assert_eq!(s.min(), Some(0));
        assert_eq!(s.max(), Some(5 * 16 + 8 + 7));
    }
}
