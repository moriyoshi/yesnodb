//! Packing constituents in: the other half of [`OrdSet::view_select`].
//!
//! # Why a sink rather than a per-constituent encode
//!
//! Constituents interleave into the same chunks — under
//! [`ViewLayout::Interleaved`](super::ViewLayout::Interleaved) *every* chunk
//! holds slots of every constituent — so encoding after each `place` would
//! re-select the representation of the same containers once per constituent.
//! The sink accumulates and [`Container::optimize`](crate::Container::optimize)
//! runs **once**, at [`ViewSink::build`]. Same shape as
//! [`MatrixSink`](crate::matrix::MatrixSink) and
//! [`IntSink`](crate::bignum::IntSink).
//!
//! # Build memory is proportional to the packed cardinality
//!
//! [`OrdinalSink`](crate::pack::OrdinalSink) accumulates a `Vec<u64>`, so
//! building a view costs 8 bytes per ordinal placed, against the roughly 2 bytes
//! per ordinal the packed form settles at. That is a **transient 4x**, paid only
//! during construction, and it is the same bargain `MatrixSink` and `IntSink`
//! already make.
//!
//! Unlike theirs it is **not bounded by the descriptor**: a matrix and an
//! integer have a width, a view constituent is an arbitrary `OrdSet`. So placing
//! a billion-ordinal constituent really does want 8 GB here. The fix, when it
//! is needed, is the mirror of [`OrdSet::view_select`]'s specialised arm — under
//! a chunk-aligned `Blocked` stride a constituent's containers can be **shared
//! by refcount** at a shifted prefix, moving no bits at all. Not done here:
//! the generic path ships first and an arm is added on a measurement, per the
//! crate's policy.

use super::View;
use crate::pack::OrdinalSink;
use crate::{CodecError, OrdSet, Result};

/// Accumulates constituents and encodes them into one packed `OrdSet`.
///
/// ```
/// use yesno_core::view::{View, ViewSink};
/// use yesno_core::OrdSet;
///
/// let v = View::interleaved(2);
/// let mut sink = ViewSink::new(v);
/// sink.place(0, &OrdSet::from_iter_unsorted([0u64, 5])).unwrap();
/// sink.place(1, &OrdSet::from_iter_unsorted([5u64])).unwrap();
/// let packed = sink.build();
///
/// assert_eq!(packed.view_select(&v, 0).len(), 2);
/// assert_eq!(packed.view_select(&v, 1).len(), 1);
/// assert!(packed.view_contains(&v, 1, 5));
/// assert!(!packed.view_contains(&v, 1, 0));
/// ```
#[derive(Clone, Debug)]
pub struct ViewSink {
    view: View,
    ordinals: OrdinalSink,
}

impl ViewSink {
    pub fn new(view: View) -> ViewSink {
        ViewSink {
            view,
            ordinals: OrdinalSink::new(),
        }
    }

    // There is deliberately no `view()` accessor, and since 2026-09-06
    // neither `MatrixSink` nor `IntSink` has one either. All three had the same
    // shape: a caller who built a sink already holds the descriptor it was built
    // from, so handing it back is public API earning nothing — the `stats.rs`
    // precedent in `CLAUDE.md` is exactly this at larger scale. The other two
    // were removed rather than this one being added, which is what made the
    // three sinks consistent.

    /// Record `s` as constituent `set`.
    ///
    /// # Errors
    ///
    /// - the view is not self-consistent — see [`View::check`];
    /// - `set` is not a constituent of it;
    /// - some ordinal of `s` is not addressable in that constituent, which is
    ///   [`CodecError::OrdinalOutOfRange`] carrying the offending **logical**
    ///   ordinal. Under `Blocked` that means at or above the stride; under either
    ///   layout it means the slot would exceed
    ///   [`ORDINAL_MAX`](crate::ORDINAL_MAX), and `u64::MAX` is not an ordinal
    ///   ( invariant I8 ).
    ///
    /// **Every ordinal is checked before any is written**, so a failed
    /// `place` leaves the sink exactly as it was. The alternative — writing
    /// until one fails — would leave a half-placed constituent that no later
    /// `place` can undo, since a sink has no way to clear a bit.
    ///
    /// **Placing the same `set` twice unions the two**, because the underlying
    /// set has no notion of clearing a bit a later placement leaves unset. That
    /// follows [`MatrixSink::place`](crate::matrix::MatrixSink::place) rather
    /// than [`IntSink::place`](crate::bignum::IntSink::place): OR-ing two
    /// *sets* is a meaningful operation and is exactly what a caller building a
    /// constituent from parts would want, whereas OR-ing two integers is not
    /// addition and had to be refused. A caller meaning to replace builds a
    /// fresh sink.
    pub fn place(&mut self, set: u32, s: &OrdSet) -> Result<()> {
        self.view.check()?;
        if set >= self.view.sets() {
            return Err(CodecError::Invariant("no such constituent in the view"));
        }
        // Check first, write second.
        for (p, c) in s.chunks() {
            for val in c.iter() {
                let x = crate::chunk_base(p) | val as u64;
                if self.view.ordinal_of(set, x).is_none() {
                    return Err(CodecError::OrdinalOutOfRange { ordinal: x });
                }
            }
        }
        for (p, c) in s.chunks() {
            for val in c.iter() {
                let x = crate::chunk_base(p) | val as u64;
                let o = self
                    .view
                    .ordinal_of(set, x)
                    .expect("every ordinal was just checked");
                self.ordinals.push(o);
            }
        }
        Ok(())
    }

    /// Encode every placement into one packed `OrdSet`, choosing each
    /// container's representation once.
    pub fn build(self) -> OrdSet {
        self.ordinals.build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::ViewLayout;

    fn constituents() -> Vec<OrdSet> {
        vec![
            OrdSet::new(),
            OrdSet::from_iter_unsorted([0u64]),
            OrdSet::from_iter_unsorted([0u64, 1, 2, 63, 64, 999]),
            OrdSet::from_iter_unsorted((0..900u64).map(|i| i * 7)),
            OrdSet::from_iter_unsorted(0..5000u64),
        ]
    }

    /// **The property that says a container is a container.** Everything else in
    /// this module is in service of this round trip.
    #[test]
    fn every_constituent_comes_back_exactly_under_both_layouts() {
        let parts = constituents();
        let n = parts.len() as u32;
        for v in [
            View::interleaved(n),
            View::blocked(n, 10_000),
            View::blocked(n, 65_536),
            View::blocked(n, 131_072),
        ] {
            let mut sink = ViewSink::new(v);
            for (i, p) in parts.iter().enumerate() {
                sink.place(i as u32, p).expect("addressable");
            }
            let packed = sink.build();
            for (i, want) in parts.iter().enumerate() {
                let got = packed.view_select(&v, i as u32);
                assert_eq!(got.len(), want.len(), "{v:?} constituent {i}");
                assert_eq!(
                    packed.view_cardinality(&v, i as u32),
                    want.len(),
                    "{v:?} constituent {i}"
                );
                for x in want.iter() {
                    assert!(got.contains(x), "{v:?} constituent {i} lost {x}");
                    assert!(packed.view_contains(&v, i as u32, x));
                }
            }
        }
    }

    /// The packed set's cardinality is the sum of its constituents', because the
    /// packing is an injection — no slot is shared.
    #[test]
    fn packing_conserves_cardinality() {
        let parts = constituents();
        let v = View::interleaved(parts.len() as u32);
        let mut sink = ViewSink::new(v);
        for (i, p) in parts.iter().enumerate() {
            sink.place(i as u32, p).unwrap();
        }
        let packed = sink.build();
        assert_eq!(packed.len(), parts.iter().map(|p| p.len()).sum::<u64>());
    }

    #[test]
    fn an_ordinal_past_a_blocked_constituents_capacity_is_refused() {
        let v = View::blocked(2, 100);
        let mut sink = ViewSink::new(v);
        let over = OrdSet::from_iter_unsorted([5u64, 100]);
        let err = sink.place(0, &over).unwrap_err();
        assert!(
            matches!(err, CodecError::OrdinalOutOfRange { ordinal: 100 }),
            "{err:?}"
        );
        // And nothing was written, so the sink is still usable.
        sink.place(0, &OrdSet::from_iter_unsorted([5u64])).unwrap();
        assert_eq!(sink.build().len(), 1);
    }

    #[test]
    fn a_bad_descriptor_or_constituent_is_refused() {
        let mut sink = ViewSink::new(View::interleaved(0));
        assert!(sink.place(0, &OrdSet::new()).is_err(), "zero constituents");

        let mut sink = ViewSink::new(View::interleaved(2));
        assert!(
            sink.place(2, &OrdSet::new()).is_err(),
            "no such constituent"
        );
        assert!(matches!(
            ViewSink::new(View::blocked(2, 0)).place(0, &OrdSet::new()),
            Err(CodecError::Invariant(_))
        ));
    }

    /// Diverges from `IntSink`, which refuses a repeat, and follows
    /// `MatrixSink`, which unions one. Unioning two *sets* is meaningful.
    #[test]
    fn placing_a_constituent_twice_unions_it() {
        let v = View::interleaved(2);
        let mut sink = ViewSink::new(v);
        sink.place(0, &OrdSet::from_iter_unsorted([1u64, 2]))
            .unwrap();
        sink.place(0, &OrdSet::from_iter_unsorted([2u64, 3]))
            .unwrap();
        let packed = sink.build();
        let got = packed.view_select(&v, 0);
        assert_eq!(got.len(), 3);
        for x in [1u64, 2, 3] {
            assert!(got.contains(x));
        }
    }

    /// §15.2 Proposition 23, asserted rather than assumed: under a dense
    /// packing the map to the constituent stack is a bijection, so an
    /// elementwise operation on the packed sets **is** the `n`-wise operation on
    /// the constituents — with no view-aware code involved.
    #[test]
    fn set_algebra_on_packed_sets_is_elementwise_across_constituents() {
        let v = View::interleaved(3);
        let a_parts = [
            OrdSet::from_iter_unsorted([0u64, 1, 2, 500]),
            OrdSet::from_iter_unsorted((0..400u64).map(|i| i * 3)),
            OrdSet::from_iter_unsorted(0..1000u64),
        ];
        let b_parts = [
            OrdSet::from_iter_unsorted([1u64, 2, 3, 501]),
            OrdSet::from_iter_unsorted((0..400u64).map(|i| i * 5)),
            OrdSet::from_iter_unsorted(500..1500u64),
        ];
        let pack = |parts: &[OrdSet; 3]| {
            let mut s = ViewSink::new(v);
            for (i, p) in parts.iter().enumerate() {
                s.place(i as u32, p).unwrap();
            }
            s.build()
        };
        let (a, b) = (pack(&a_parts), pack(&b_parts));

        for i in 0..3u32 {
            let want_and = a_parts[i as usize].and(&b_parts[i as usize]);
            let want_or = a_parts[i as usize].or(&b_parts[i as usize]);
            let want_xor = a_parts[i as usize].xor(&b_parts[i as usize]);
            assert_eq!(
                a.and(&b).view_select(&v, i).len(),
                want_and.len(),
                "and {i}"
            );
            assert_eq!(a.or(&b).view_select(&v, i).len(), want_or.len(), "or {i}");
            assert_eq!(
                a.xor(&b).view_select(&v, i).len(),
                want_xor.len(),
                "xor {i}"
            );
            for x in want_and.iter() {
                assert!(a.and(&b).view_select(&v, i).contains(x));
            }
        }
        assert!(matches!(v.layout(), ViewLayout::Interleaved));
    }
}
