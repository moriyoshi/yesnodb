//! Several `OrdSet`s packed into one ordinal space.
//!
//! A [`View`] says how `n` constituent sets share one physical `OrdSet`. Each
//! constituent has its own **logical** ordinals; the view maps
//! `( constituent, logical ordinal )` to a physical ordinal, and back. Nothing
//! is stored — a `View` is a descriptor the caller constructs, exactly as
//! [`Layout`](crate::matrix::Layout) and [`IntLayout`](crate::bignum::IntLayout)
//! are.
//!
//! # A view is an `n × W` boolean matrix whose rows are the sets
//!
//! That is the whole model, and the two layouts are that matrix's two orders:
//!
//! ```text
//! Interleaved:  constituent i, logical x  ->  x * n + i     ( a "line" is one logical ordinal )
//! Blocked:      constituent i, logical x  ->  i * stride + x ( a "line" is one constituent )
//! ```
//!
//! They are transposes of each other, which is why the same three questions
//! have opposite costs under them — see [`ViewLayout`].
//!
//! **This is deliberately not [`matrix`](crate::matrix).** That module reads a
//! *dense value* out of a set: bounded, materialised, in memory. A view over 8
//! constituents of `2^40` ordinals each is `2^43` bits — a terabyte dense — so
//! every operation here works on the **sparse** representation and never
//! materialises the matrix. `matrix/` remains useful as a differential oracle at
//! sizes that do fit, and the tests use it that way.
//!
//! # Elementwise algebra across two views is already free
//!
//! `docs/formal-model.md` §15.2 proves that under a dense layout the map from a
//! set to its object stack is a **bijection**, and that every elementwise
//! operation on the stack *is* the corresponding set operation. So for two sets
//! packed under the same view, [`OrdSet::and`](crate::OrdSet::and) / `or` / `xor`
//! **are** the `n`-wise elementwise operations on all `n` pairs at once, at no
//! cost and through no new code.
//!
//! There is deliberately no `view_and`. What this module provides instead is
//! [`View::compatible_with`], so that combining two differently-packed sets is a
//! question a caller can ask rather than a silent wrong answer.
//!
//! # What is not here
//!
//! **No descriptor catalog.** The packed bits are persisted as an ordinary
//! keyed set, but the [`View`] is caller-owned metadata. Network clients carry
//! the descriptor in each query; a stored `view_id -> descriptor` map would be a
//! separate catalog format and is deliberately not implied by these bits.
//!
//! **No lazy expression node.** Selection, folding, and expansion are set
//! transforms. Flight normally evaluates them eagerly and then re-enters its
//! Boolean expression as a set leaf. Direct persisted identity counts and
//! interleaved folds may instead consume a checked chunk stream when exact
//! source statistics prove the input has one value in each of many occupied
//! chunks. This terminal-only arm is not an expression node; adding one remains
//! a separately measured change rather than an accidental API promise. Direct
//! interleaved identity ranks have a different bound: their caller may stream
//! only the physical prefixes below the requested logical endpoint, including
//! one clipped final chunk, without any sparsity admission heuristic.

use crate::pack::Packing;
use crate::{CodecError, Container, Prefix48, Result, ORDINAL_MAX};

mod fold;
mod select;
mod sink;

pub use fold::{stream_interleaved_view_fold, Reduce};
pub use select::{
    stream_view_cardinalities, stream_view_ranks, IntersectionCountStrategy,
    ViewIntersectionCounter,
};
pub use sink::ViewSink;

/// Validate the ordering and ordinal invariants promised by a `ChunkStream`.
///
/// Public stream terminals cannot trust a caller-defined stream as an `OrdSet`
/// can trust its own parallel vectors. Keep this check shared so cardinality and
/// fold paths reject the same malformed input.
fn validate_stream_chunk(
    last_prefix: &mut Option<Prefix48>,
    prefix: Prefix48,
    container: &Container,
) -> Result<()> {
    if container.is_empty() {
        return Err(CodecError::Invariant(
            "view stream yielded an empty container",
        ));
    }
    if prefix >= (1u64 << 48) {
        return Err(CodecError::Invariant(
            "view stream yielded a prefix outside 48 bits",
        ));
    }
    if last_prefix.is_some_and(|last| prefix <= last) {
        return Err(CodecError::Invariant(
            "view stream prefixes are not strictly ascending",
        ));
    }
    if prefix == (1u64 << 48) - 1 && container.contains(u16::MAX) {
        return Err(CodecError::OrdinalOutOfRange {
            ordinal: ORDINAL_MAX.saturating_add(1),
        });
    }
    *last_prefix = Some(prefix);
    Ok(())
}

/// How constituents share the ordinal space.
///
/// The choice is a genuine cost trade and not a preference — it is
/// array-of-structs against struct-of-arrays, and each layout makes the other's
/// cheap operation expensive:
///
/// | operation | `Interleaved` | `Blocked` |
/// |---|---|---|
/// | extract one constituent | `O(nnz)`, a strided filter | a range window plus an offset |
/// | cardinality of one constituent | `O(nnz)` | `O(chunks touched)`, payload at ≤ 2 |
/// | membership in one constituent | `O(log)` | `O(log)` |
/// | reaching all `n` slots of one logical ordinal | one cacheline | `n` distant regions |
///
/// **`Blocked` with a stride that is a multiple of 65 536 is close to free in
/// both directions**, because a constituent is then a whole number of chunks and
/// extracting it is a prefix relabel with the container payloads shared rather
/// than rebuilt. That is the case [`crate::OrdSet::view_select`] specialises.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ViewLayout {
    /// Constituent `i`'s logical ordinal `x` sits at `x * sets + i`.
    ///
    /// All `n` slots of one logical ordinal are adjacent, so a fold across
    /// constituents is chunk-local.
    Interleaved,
    /// Constituent `i`'s logical ordinal `x` sits at `i * stride + x`.
    ///
    /// A constituent is one contiguous region, so extracting it is a window.
    /// `stride` bounds each constituent's logical universe.
    Blocked {
        /// Ordinals between the start of consecutive constituents. Also the
        /// exclusive upper bound on a constituent's logical ordinals.
        stride: u64,
    },
}

/// How `n` constituent `OrdSet`s share one physical `OrdSet`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct View {
    sets: u32,
    layout: ViewLayout,
}

impl View {
    /// Constituent `i`'s logical ordinal `x` at `x * sets + i`.
    pub fn interleaved(sets: u32) -> View {
        View {
            sets,
            layout: ViewLayout::Interleaved,
        }
    }

    /// Constituent `i`'s logical ordinal `x` at `i * stride + x`.
    ///
    /// `stride` is also each constituent's logical capacity: a logical ordinal
    /// at or above it is not addressable, because it would land in the next
    /// constituent. [`View::check`] does not test that — it is a property of a
    /// value, not of the descriptor — so [`View::ordinal_of`] returns `None`.
    pub fn blocked(sets: u32, stride: u64) -> View {
        View {
            sets,
            layout: ViewLayout::Blocked { stride },
        }
    }

    /// How many constituents this view packs.
    #[inline]
    pub fn sets(&self) -> u32 {
        self.sets
    }

    /// How they share the space.
    #[inline]
    pub fn layout(&self) -> ViewLayout {
        self.layout
    }

    /// Is this view self-consistent?
    ///
    /// Rejects a zero constituent count and a zero `Blocked` stride, which would
    /// map every constituent onto the same ordinals. It does **not** bound
    /// `sets` or `stride` against [`ORDINAL_MAX`] — a view can be legally
    /// declared whose upper constituents are unaddressable, and
    /// [`View::ordinal_of`] reports that per ordinal rather than per descriptor.
    /// That mirrors [`Packing::check`], which likewise accepts a deliberately
    /// over-large stride as a caller choice.
    pub fn check(&self) -> Result<()> {
        if self.sets == 0 {
            return Err(CodecError::Invariant("view packs zero constituents"));
        }
        if matches!(self.layout, ViewLayout::Blocked { stride: 0 }) {
            return Err(CodecError::Invariant("view blocked stride is zero"));
        }
        Ok(())
    }

    /// Physical ordinal holding logical ordinal `x` of constituent `set`.
    ///
    /// `None` if the view does not check, if `set` is out of range, if `x`
    /// exceeds a `Blocked` constituent's capacity, or if the result would exceed
    /// [`ORDINAL_MAX`] — `u64::MAX` is not an ordinal ( invariant I8 ), so such a
    /// slot is unaddressable rather than silently wrapped.
    #[inline]
    pub fn ordinal_of(&self, set: u32, x: u64) -> Option<u64> {
        if self.check().is_err() || set >= self.sets {
            return None;
        }
        let o = match self.layout {
            ViewLayout::Interleaved => x.checked_mul(self.sets as u64)?.checked_add(set as u64)?,
            ViewLayout::Blocked { stride } => {
                if x >= stride {
                    return None;
                }
                (set as u64).checked_mul(stride)?.checked_add(x)?
            }
        };
        (o <= ORDINAL_MAX).then_some(o)
    }

    /// How many leading constituents can address logical ordinal `x` at all.
    ///
    /// [`View::ordinal_of`] is **monotone in `set`** under both layouts —
    /// `x·sets + i` and `i·stride + x` both increase with `i` — so the
    /// constituents that address `x` are always a prefix `0..k`, and this
    /// returns that `k`.
    ///
    /// # Why this exists: work must never be proportional to the descriptor
    ///
    /// `sets` is a `u32` a caller may legally declare far larger than the
    /// ordinal space can hold, and this type's [`View::check`] deliberately
    /// allows that — an over-large descriptor is a caller's choice, reported per
    /// ordinal rather than refused per descriptor. What is *not* acceptable is a
    /// loop that visits four billion constituents to discover that all but a
    /// handful are unaddressable. Callers iterating constituents bound
    /// themselves with this instead of with `sets`, so the cost follows what can
    /// actually be addressed.
    #[inline]
    pub fn addressable_sets(&self, x: u64) -> u32 {
        if self.check().is_err() {
            return 0;
        }
        let room = match self.layout {
            ViewLayout::Interleaved => {
                let Some(base) = x.checked_mul(self.sets as u64) else {
                    return 0;
                };
                if base > ORDINAL_MAX {
                    return 0;
                }
                ORDINAL_MAX - base
            }
            ViewLayout::Blocked { stride } => {
                if x >= stride {
                    return 0;
                }
                (ORDINAL_MAX - x) / stride
            }
        };
        // `room` is the largest addressable index, so the count is one more.
        let k = room.saturating_add(1).min(self.sets as u64);
        k as u32
    }

    /// The inverse of [`View::ordinal_of`]: which constituent and which logical
    /// ordinal a physical ordinal carries.
    ///
    /// `None` when the ordinal belongs to no constituent — under `Blocked` that
    /// is everything at or above `sets * stride`, which is addressed by nothing.
    /// Under `Interleaved` **every** ordinal belongs to some constituent, so
    /// this is total there; the two layouts differ in whether the packing has
    /// gaps, and a caller filtering on `Some` must not assume it is filtering
    /// anything out.
    #[inline]
    pub fn logical_of(&self, ordinal: u64) -> Option<(u32, u64)> {
        if self.check().is_err() {
            return None;
        }
        match self.layout {
            ViewLayout::Interleaved => {
                let n = self.sets as u64;
                Some(((ordinal % n) as u32, ordinal / n))
            }
            ViewLayout::Blocked { stride } => {
                let set = ordinal / stride;
                (set < self.sets as u64).then_some((set as u32, ordinal % stride))
            }
        }
    }

    /// May two sets packed under `self` and `other` be combined elementwise?
    ///
    /// Equality of the descriptor is the whole condition, and the reason it is
    /// worth a method is that the operation it guards has **no** code of its own:
    /// §15.2's bijection makes `a.and(b)` already the `n`-wise elementwise AND,
    /// so a mismatched pair produces a well-formed wrong answer rather than an
    /// error. Nothing enforces this — it is a question a caller can ask, not a
    /// guard the type system applies.
    #[inline]
    pub fn compatible_with(&self, other: &View) -> bool {
        self == other
    }

    /// This view as a [`Packing`], when it is expressible as one.
    ///
    /// `Interleaved` is exactly `Packing::dense( 1, sets )` — one object per
    /// logical ordinal, `sets` bits wide — which is what lets the shared gather
    /// address it.
    ///
    /// `Blocked` is `None` whenever `stride` exceeds `u32::MAX`, because
    /// `Packing::line_bits` is a `u32` and a blocked constituent's width is the
    /// stride. That is a real limit rather than an oversight: a blocked view's
    /// constituents are deliberately allowed to be wider than any object
    /// `matrix/` or `bignum/` would address, and the operations that need it do
    /// not go through `Packing` at all — they are range windows.
    pub fn packing(&self) -> Option<Packing> {
        self.check().ok()?;
        match self.layout {
            ViewLayout::Interleaved => Some(Packing::dense(1, self.sets)),
            ViewLayout::Blocked { stride } => {
                let width = u32::try_from(stride).ok()?;
                Some(Packing {
                    line_bits: width,
                    lines: 1,
                    line_stride: width,
                    object_stride: stride,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleaved_places_constituents_side_by_side() {
        let v = View::interleaved(3);
        assert_eq!(v.ordinal_of(0, 0), Some(0));
        assert_eq!(v.ordinal_of(1, 0), Some(1));
        assert_eq!(v.ordinal_of(2, 0), Some(2));
        assert_eq!(v.ordinal_of(0, 1), Some(3));
        assert_eq!(v.ordinal_of(3, 0), None, "no such constituent");
    }

    #[test]
    fn blocked_gives_each_constituent_a_region() {
        let v = View::blocked(3, 100);
        assert_eq!(v.ordinal_of(0, 0), Some(0));
        assert_eq!(v.ordinal_of(1, 0), Some(100));
        assert_eq!(v.ordinal_of(2, 99), Some(299));
        assert_eq!(v.ordinal_of(0, 100), None, "past the constituent capacity");
    }

    #[test]
    fn logical_of_inverts_ordinal_of_on_both_layouts() {
        for v in [View::interleaved(5), View::blocked(5, 64)] {
            for set in 0..5u32 {
                for x in [0u64, 1, 7, 63] {
                    let Some(o) = v.ordinal_of(set, x) else {
                        continue;
                    };
                    assert_eq!(v.logical_of(o), Some((set, x)), "{v:?} set={set} x={x}");
                }
            }
        }
    }

    /// The two layouts differ in whether the packing has gaps, and a caller
    /// that filters on `Some` must know which it is holding.
    #[test]
    fn only_blocked_has_ordinals_belonging_to_no_constituent() {
        let b = View::blocked(2, 10);
        assert_eq!(b.logical_of(19), Some((1, 9)));
        assert_eq!(b.logical_of(20), None, "above sets * stride");

        let i = View::interleaved(2);
        for o in [0u64, 1, 2, 3, 1_000_000, ORDINAL_MAX] {
            assert!(i.logical_of(o).is_some(), "interleaved is total");
        }
    }

    #[test]
    fn check_rejects_a_degenerate_descriptor() {
        assert!(View::interleaved(1).check().is_ok());
        assert!(View::interleaved(0).check().is_err());
        assert!(View::blocked(2, 8).check().is_ok());
        assert!(View::blocked(2, 0).check().is_err());
        // A deliberately over-large stride is a caller choice, as on `Packing`.
        assert!(View::blocked(2, 1 << 40).check().is_ok());
    }

    #[test]
    fn a_slot_past_the_ordinal_ceiling_is_not_addressable() {
        let v = View::interleaved(2);
        // The top logical ordinal whose slot 1 still fits.
        let last = (ORDINAL_MAX - 1) / 2;
        assert!(v.ordinal_of(1, last).is_some());
        assert_eq!(v.ordinal_of(1, last + 1), None);
    }

    #[test]
    fn interleaved_is_a_dense_packing_of_one_object_per_logical_ordinal() {
        let v = View::interleaved(7);
        assert_eq!(v.packing(), Some(Packing::dense(1, 7)));
        // Bit i of object x is the physical ordinal of constituent i at x.
        let p = v.packing().unwrap();
        for set in 0..7u32 {
            assert_eq!(p.ordinal_at(5, 0, set), v.ordinal_of(set, 5));
        }
    }

    /// A blocked constituent may be wider than a `Packing` can express, and
    /// declining is correct — the operations that need a wide blocked view are
    /// range windows and do not go through `Packing`.
    #[test]
    fn a_blocked_view_wider_than_a_u32_has_no_packing() {
        assert!(View::blocked(2, 1 << 20).packing().is_some());
        assert_eq!(View::blocked(2, 1 << 40).packing(), None);
    }

    /// **`matrix/` as an independent oracle**, and the statement of the model
    /// this module is built on: an interleaved view *is* a column-major matrix
    /// whose rows are the constituents, and a blocked view is the row-major one.
    ///
    /// This only works at sizes that fit a `BitMatrix`. It is a cross-check
    /// on small inputs, not a way to implement views — the whole reason this
    /// module exists is that a real view does not fit densely.
    #[test]
    fn a_small_view_is_a_bit_matrix_whose_rows_are_the_constituents() {
        use crate::matrix::{Layout, Order};
        use crate::view::ViewSink;
        use crate::OrdSet;

        let n = 5u32;
        let w = 100u32;
        let parts: Vec<OrdSet> = (0..n)
            .map(|i| {
                OrdSet::from_iter_unsorted(
                    (0..w as u64).filter(move |x| (x * 7 + i as u64).is_multiple_of(3)),
                )
            })
            .collect();

        let cases = [
            (
                View::interleaved(n),
                Layout {
                    rows: n,
                    cols: w,
                    line_stride: n,
                    matrix_stride: n as u64 * w as u64,
                    order: Order::ColMajor,
                },
            ),
            (
                View::blocked(n, w as u64),
                Layout {
                    rows: n,
                    cols: w,
                    line_stride: w,
                    matrix_stride: n as u64 * w as u64,
                    order: Order::RowMajor,
                },
            ),
        ];

        let mut ones = 0u64;
        for (v, layout) in cases {
            let mut sink = ViewSink::new(v);
            for (i, p) in parts.iter().enumerate() {
                sink.place(i as u32, p).unwrap();
            }
            let packed = sink.build();
            let m = packed.read_matrix(0, &layout).expect("addressable");
            ones += m.count_ones();

            for i in 0..n {
                let sel = packed.view_select(&v, i);
                for x in 0..w {
                    assert_eq!(
                        sel.contains(x as u64),
                        m.get(i, x),
                        "{v:?} constituent {i} logical {x}"
                    );
                }
                // Row weight is the constituent's cardinality.
                let weight = (0..w).filter(|&x| m.get(i, x)).count() as u64;
                assert_eq!(packed.view_cardinality(&v, i), weight, "{v:?} row {i}");
            }
        }
        assert!(ones > 0, "the fixture must not be all zeros");
    }

    /// The two layouts are transposes, so the same constituents packed both ways
    /// must answer identically. A real cross-check: the two take entirely
    /// different code paths, one a strided filter and one a range window.
    #[test]
    fn interleaved_and_blocked_hold_the_same_contents() {
        use crate::view::ViewSink;
        use crate::OrdSet;

        let parts: Vec<OrdSet> = vec![
            OrdSet::new(),
            OrdSet::from_iter_unsorted([0u64, 1, 2, 9_999]),
            OrdSet::from_iter_unsorted((0..3000u64).map(|i| i * 3)),
            OrdSet::from_iter_unsorted(0..9_000u64),
        ];
        let n = parts.len() as u32;
        let cap = 10_000u64;

        let pack = |v: View| {
            let mut s = ViewSink::new(v);
            for (i, p) in parts.iter().enumerate() {
                s.place(i as u32, p).unwrap();
            }
            s.build()
        };
        let (vi, vb) = (View::interleaved(n), View::blocked(n, cap));
        let (pi, pb) = (pack(vi), pack(vb));

        let mut checked = 0u32;
        for i in 0..n {
            assert_eq!(
                pi.view_cardinality(&vi, i),
                pb.view_cardinality(&vb, i),
                "constituent {i}"
            );
            let (si, sb) = (pi.view_select(&vi, i), pb.view_select(&vb, i));
            assert_eq!(si.len(), sb.len(), "constituent {i}");
            for x in si.iter() {
                assert!(sb.contains(x), "constituent {i} logical {x}");
                assert!(pi.view_contains(&vi, i, x));
                assert!(pb.view_contains(&vb, i, x));
                checked += 1;
            }
        }
        assert!(checked > 1000, "only {checked} ordinals compared");
    }

    /// §15.5 and §11: a generator that never produces a slot crossing a chunk
    /// boundary leaves the seam untested while every property still passes. An
    /// interleaved view whose `sets` does not divide 65 536 straddles by
    /// construction, and this asserts the fixtures actually reach it rather than
    /// hoping they do.
    #[test]
    fn the_fixtures_reach_slots_that_straddle_a_chunk_boundary() {
        // Scanned rather than hand-picked. An earlier version of this test
        // listed a few plausible-looking indices and found only three straddling
        // groups across four widths — the assertion caught its own fixture,
        // which is exactly what a coverage assertion is for. The first straddle
        // for `sets = 3` is at x = 21 845, well past any index one would guess.
        for sets in [3u32, 5, 7, 100] {
            let v = View::interleaved(sets);
            let p = v.packing().expect("interleaved always has one");
            let n = (0..100_000u64)
                .filter(|&x| p.straddles(x) == Some(true))
                .count();
            assert!(n > 0, "sets={sets} never straddles; its seam is untested");
        }
        // And a divisor of 65 536 never straddles, which is the contrast that
        // makes the counts above mean something.
        for sets in [1u32, 2, 4, 16] {
            let p = View::interleaved(sets).packing().unwrap();
            let n = (0..100_000u64)
                .filter(|&x| p.straddles(x) == Some(true))
                .count();
            assert_eq!(n, 0, "sets={sets} divides 65 536 and must never straddle");
        }
    }

    #[test]
    fn stream_chunk_validation_rejects_ordering_and_the_reserved_ordinal() {
        let chunk = Container::from_sorted(&[7]);
        let mut last = None;
        validate_stream_chunk(&mut last, 4, &chunk).unwrap();
        assert!(validate_stream_chunk(&mut last, 4, &chunk).is_err());
        assert!(validate_stream_chunk(&mut last, 3, &chunk).is_err());

        let mut last = None;
        let reserved = Container::from_sorted(&[u16::MAX]);
        assert!(validate_stream_chunk(&mut last, (1u64 << 48) - 1, &reserved).is_err());
    }

    #[test]
    fn compatibility_is_descriptor_equality() {
        assert!(View::interleaved(4).compatible_with(&View::interleaved(4)));
        assert!(!View::interleaved(4).compatible_with(&View::interleaved(5)));
        assert!(!View::interleaved(4).compatible_with(&View::blocked(4, 16)));
    }
}
