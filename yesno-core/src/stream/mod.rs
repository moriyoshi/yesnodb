//! Lazy, chunk-aligned set algebra.
//!
//! A [`ChunkStream`] yields `(Prefix48, Container)` in ascending prefix order.
//! Operators compose over streams, so `a AND (b OR c)` never materializes an
//! intermediate set.
//!
//! # No lifetime parameter
//!
//! Because [`Container`] is `'static + Clone + Send` (it is backed by a
//! refcounted `arrow_buffer::Buffer`), this trait needs no lifetime and no GAT.
//! Streams are therefore `Box`-able, storable in a struct, and sendable across
//! threads — which is what a query engine holding a plan node requires. A
//! borrowed `Container<'a>` design would make that unrepresentable.
//!
//! # Fallibility
//!
//! `next_chunk` returns [`Result`] even though the in-memory leaves cannot fail.
//! Once leaves decode from an mmap'd page store, they can — and retrofitting
//! fallibility through every operator later would be a breaking change.

pub mod dynamic;
pub mod leaf;
pub mod nary;
pub mod ops;
pub mod plan;
pub mod sketch;

pub use dynamic::{BoxedStream, ChunkSource, Expr};
pub use leaf::{EmptyStream, ErrStream, RangeStream, SetStream};
pub use ops::{And, AndNot, Concat, Not, Or, Restrict, Xor};

use crate::container::Container;
use crate::{Prefix48, Result};

/// What is behind a stream, and therefore what one chunk costs to obtain.
///
/// The numbers are ordinal, not absolute: they exist so that a plan can prefer
/// touching one operand over another, and only their ratios matter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Backing {
    /// Produced arithmetically, touching nothing. A `RangeStream` chunk.
    Computed,
    /// A materialized `OrdSet` already in memory.
    Memory,
    /// Decoded from an mmap'd page, so a miss is a page fault — and under a
    /// 2 MiB folio, faulting in one 8 KiB container reads 256x what it needs.
    ///
    /// **[`KeyStream`](crate::KeyStream) reports this**, and until 2026-09-13
    /// nothing did — `Snapshot::load` materialized an `OrdSet` before anything
    /// could stream it, so every leaf bottomed out in [`Backing::Memory`] no
    /// matter what stood behind it. The variant was written ahead of its
    /// producer because it is the reason this channel exists at all: a plan that
    /// counted chunks without asking what backed them would prefer exactly the
    /// wrong operand. `stats_drive_the_plan_not_just_chunk_counts` builds a
    /// synthetic stream that reports it, and kept the path exercised rather than
    /// aspirational in the meantime; it is still the test that pins the
    /// planner's reaction, independently of the database.
    Paged,
    /// Cost unknown — an operator over children that did not report.
    Unknown,
}

impl Backing {
    /// Relative cost of obtaining one chunk.
    #[inline]
    pub fn chunk_cost(self) -> u64 {
        match self {
            Backing::Computed => 1,
            Backing::Memory => 2,
            Backing::Paged => 64,
            Backing::Unknown => 8,
        }
    }

    /// The costlier of two backings, for an operator over both.
    #[inline]
    pub fn worse(self, other: Backing) -> Backing {
        if other.chunk_cost() > self.chunk_cost() {
            other
        } else {
            self
        }
    }
}

/// What a stream knows about itself at runtime.
///
/// # Why this is on the stream and not on `Expr`
///
/// [`crate::stream::plan`] rewrites an `Expr` using statistics read off its
/// leaves. **That used to work only because every leaf was a materialized
/// `OrdSet`**, which the planner could measure directly; it still cannot see
/// behind a `Box<dyn ChunkStream>` handed in from elsewhere, and it treats a
/// chunk as a chunk whatever produced it.
///
/// Since 2026-09-13 a leaf may also be [`Expr::Source`](crate::Expr), which is
/// lazy. That is why [`ChunkSource`](crate::stream::ChunkSource) carries
/// `chunk_count`, `prefix_span` and `cardinality`: they are the three things the
/// planner used to read off the set, asked of the leaf instead, and answerable
/// without decoding anything. A source that returns `None` from them is
/// correct and merely opaque -- it plans as an operand of unknown size, charged
/// `plan::OPAQUE_CHUNKS` so it never wins a comparison against one that
/// reported.
///
/// This is the dynamic half: each stream reports its own size and backing, so a
/// decision can be made once the operands are actually open and it is known what
/// stands behind them. Where the two disagree, this one is authoritative,
/// because static rewriting is guessing about the thing this measures.
#[derive(Clone, Copy, Debug)]
pub struct StreamStats {
    /// Chunks the stream will yield, when known exactly.
    pub chunks: Option<u64>,
    /// Inclusive prefix span, when known.
    pub prefix_span: Option<(Prefix48, Prefix48)>,
    pub backing: Backing,
}

impl StreamStats {
    pub fn unknown() -> Self {
        StreamStats {
            chunks: None,
            prefix_span: None,
            backing: Backing::Unknown,
        }
    }

    pub fn new(chunks: u64, span: Option<(Prefix48, Prefix48)>, backing: Backing) -> Self {
        StreamStats {
            chunks: Some(chunks),
            prefix_span: span,
            backing,
        }
    }

    /// Estimated work to drain this stream: chunks weighted by what backs them.
    ///
    /// `None` chunk counts are charged a deliberately pessimistic constant. A
    /// stream that will not say how large it is should not win a comparison
    /// against one that did.
    pub fn drain_cost(&self) -> u64 {
        const UNKNOWN_CHUNKS: u64 = 1 << 20;
        self.chunks
            .unwrap_or(UNKNOWN_CHUNKS)
            .saturating_mul(self.backing.chunk_cost())
    }
}

/// A lazily-evaluated, prefix-ordered sequence of chunks.
///
/// Implementations must yield strictly ascending prefixes and never yield an
/// empty container.
pub trait ChunkStream: Send {
    /// Next chunk at or after the current position.
    fn next_chunk(&mut self) -> Result<Option<(Prefix48, Container)>>;

    /// Position so the next [`Self::next_chunk`] returns the first chunk with
    /// prefix >= `prefix`. Implementations should gallop; a correct-but-slow
    /// scan is permitted.
    fn seek(&mut self, prefix: Prefix48) -> Result<()>;

    /// Prefix of the next chunk *without* materializing its payload.
    ///
    /// Deliberately separate from `next_chunk`: seek-driven AND compares
    /// prefixes constantly and must not pay a refcount bump for a chunk it is
    /// about to skip.
    ///
    /// **This is a lower bound, not a promise.** XOR and ANDNOT may report a
    /// prefix whose chunk then cancels to empty and is skipped. Use it for
    /// ordering decisions only; anything needing an exact answer (emptiness,
    /// cardinality) must go through `next_chunk` or `cardinality_dyn`.
    fn peek_prefix(&mut self) -> Result<Option<Prefix48>>;

    /// `(lower, upper)` bound on remaining cardinality, if cheaply known.
    fn cardinality_hint(&self) -> (u64, Option<u64>) {
        (0, None)
    }

    /// Size and backing, for plans made once the operands are open.
    ///
    /// Distinct from [`Self::cardinality_hint`], which bounds *ordinals*. This
    /// reports *chunks* and what produces them, which is what decides how much
    /// work touching this stream costs. The default says "unknown" so that
    /// implementing it stays optional — but an operator that does not report
    /// makes every plan above it guess.
    fn stats(&self) -> StreamStats {
        StreamStats::unknown()
    }

    /// Advance one chunk and report **its cardinality**, without materializing
    /// its payload.
    ///
    /// # Why this exists
    ///
    /// `next_chunk` is the only way to advance, and it hands back a `Container`.
    /// So every operator that needed a length and not a payload was fetching one
    /// to read an integer the index already holds — five sites did, and removing
    /// it took `AndNot(disjoint)` from 2.09 ms to 0.67 ms over 100 000 chunks,
    /// which is most of the 26x that operator's disjointness rewrite had been
    /// credited with. The capability was never missing:
    /// `SetStream::cardinality_dyn` sums cached lengths and touches no payload.
    /// Only the exposure was.
    ///
    /// **It is not a payload copy, and saying so was wrong.** Containers are
    /// frozen ( [`Container::freeze`](crate::Container::freeze) ), so cloning is
    /// a refcount bump. The cost is two atomics and an enum move per chunk —
    /// about 14 ns — and the original measurement already ruled a copy out:
    /// 20.8 ns per chunk is two orders of magnitude too fast for an 8 KiB memcpy.
    ///
    /// The single-sided arms of `Or` / `Xor`, `Restrict`'s window walk,
    /// `AndNot`'s skip branch and `Not`'s interior are all this shape: they know
    /// the chunk passes through unchanged and want only its count.
    ///
    /// # The default is correct and materializing
    ///
    /// Unlike [`Self::cardinality_dyn`], inheriting the default here is not a
    /// silent asymptotic regression — it costs exactly what the caller paid
    /// before. It is still the slow path, and **an override is the entire point
    /// of the method**.
    ///
    /// `tests/allocation.rs` **cannot** pin this, and an allocation test
    /// written for it passed with the overrides removed. Frozen containers make
    /// the clone allocation-free, so there is nothing for a counting allocator
    /// to see. The pin is behavioural instead —
    /// `counting_a_pass_through_chunk_asks_for_a_count_not_a_chunk` in
    /// `stream::ops` watches *which question the operator asked*.
    ///
    /// Same contract as `next_chunk` otherwise: strictly ascending prefixes,
    /// and never a zero count.
    fn next_cardinality(&mut self) -> Result<Option<(Prefix48, u64)>> {
        Ok(self.next_chunk()?.map(|(p, c)| (p, c.len() as u64)))
    }

    /// Total cardinality, consuming the stream.
    ///
    /// The default materializes a container per chunk. **Every operator must
    /// override this** with a non-allocating walk — otherwise the
    /// `Box<dyn ChunkStream>` path silently falls back to the slow version and
    /// the whole point of the cardinality identities is lost. The override is
    /// what `ChunkStreamExt::cardinality` dispatches to.
    fn cardinality_dyn(&mut self) -> Result<u64> {
        let mut n = 0u64;
        while let Some((_, c)) = self.next_chunk()? {
            n += c.len() as u64;
        }
        Ok(n)
    }
}

/// Combinators. Kept on an extension trait so `ChunkStream` stays object-safe.
pub trait ChunkStreamExt: ChunkStream + Sized {
    fn and<R: ChunkStream>(self, rhs: R) -> And<Self, R> {
        And::new(self, rhs)
    }
    fn or<R: ChunkStream>(self, rhs: R) -> Or<Self, R> {
        Or::new(self, rhs)
    }
    fn xor<R: ChunkStream>(self, rhs: R) -> Xor<Self, R> {
        Xor::new(self, rhs)
    }
    fn and_not<R: ChunkStream>(self, rhs: R) -> AndNot<Self, R> {
        AndNot::new(self, rhs)
    }

    /// Complement over the whole universe `[0, ORDINAL_MAX]` — the unary NOT.
    ///
    /// Well-defined under invariant I8; see [`crate::ORDINAL_MAX`]. There is
    /// deliberately no eager equivalent — see [`crate::set::OrdSet::not_in_range`]
    /// for why a no-argument complement cannot succeed.
    ///
    /// # Which consumers are cheap
    ///
    /// The complement spans nearly `2^48` chunks, so what you do with it decides
    /// the cost. `O(chunks of the input)`, measured in microseconds over the full
    /// universe: [`ChunkStreamExt::cardinality`], [`ChunkStreamExt::contains`],
    /// [`ChunkStreamExt::min`], [`ChunkStreamExt::is_empty`], and taking the
    /// first chunks. `O(chunks of the range)`, i.e. do not: [`ChunkStreamExt::max`],
    /// [`ChunkStreamExt::collect_set`], and draining [`ChunkStreamExt::ordinals`]
    /// — each of those has to reach the end of a stream that is effectively
    /// endless. That asymmetry is inherent to a complement, not a defect.
    fn not(self) -> Not<Self> {
        self.not_in_range(0, u64::MAX)
    }

    /// Complement within `[lo, hi)`.
    ///
    /// Unlike the eager version this never holds
    /// more than one chunk, so complementing over a wide range is fine as long
    /// as the consumer is a fold.
    ///
    /// `cardinality()` applies `(hi - lo) - |self ∩ [lo, hi)|`, so it walks
    /// **this stream's** chunks rather than the range's and allocates nothing at
    /// all — measured at exactly zero allocations for a complement of ~131
    /// million ordinals. `tests/allocation.rs` holds the line.
    fn not_in_range(self, lo: u64, hi: u64) -> Not<Self> {
        Not::new(self, lo, hi)
    }

    /// Cardinality without materializing a result. Dispatches to the operator's
    /// [`ChunkStream::cardinality_dyn`] override.
    fn cardinality(mut self) -> Result<u64> {
        self.cardinality_dyn()
    }

    /// Whether the stream yields nothing.
    ///
    /// Must use `next_chunk`, not `peek_prefix`: for XOR and ANDNOT a prefix can
    /// be present on both sides yet cancel to an empty chunk, so `peek_prefix`
    /// is only a lower bound for ordering. `next_chunk` skips cancelled chunks
    /// and is authoritative.
    // Consumes `self` like every other terminal combinator here — a stream is a
    // one-shot cursor, and answering this question advances it.
    #[allow(clippy::wrong_self_convention)]
    fn is_empty(mut self) -> Result<bool> {
        Ok(self.next_chunk()?.is_none())
    }

    /// Does the stream contain `ordinal`? Seeks, then probes one container.
    fn contains(mut self, ordinal: u64) -> Result<bool> {
        let (p, low) = crate::split(ordinal);
        self.seek(p)?;
        match self.next_chunk()? {
            Some((cp, c)) if cp == p => Ok(c.contains(low)),
            _ => Ok(false),
        }
    }

    fn min(mut self) -> Result<Option<u64>> {
        Ok(self
            .next_chunk()?
            .and_then(|(p, c)| c.min().map(|v| crate::join(p, v))))
    }

    fn max(mut self) -> Result<Option<u64>> {
        let mut last = None;
        while let Some((p, c)) = self.next_chunk()? {
            last = c.max().map(|v| crate::join(p, v));
        }
        Ok(last)
    }

    /// Every ordinal, ascending.
    fn ordinals(self) -> OrdinalIter<Self> {
        OrdinalIter {
            s: self,
            cur: None,
            base: 0,
        }
    }

    /// Materialize. This is the eager path — one set of kernels, one set of tests.
    fn collect_set(mut self) -> Result<crate::OrdSet> {
        let mut chunks = Vec::new();
        while let Some((p, c)) = self.next_chunk()? {
            chunks.push((p, c));
        }
        Ok(crate::OrdSet::from_chunks(chunks))
    }

    fn boxed(self) -> BoxedStream
    where
        Self: 'static,
    {
        Box::new(self)
    }
}

impl<T: ChunkStream + Sized> ChunkStreamExt for T {}

/// Flattens a chunk stream into ordinals.
pub struct OrdinalIter<S> {
    s: S,
    cur: Option<std::vec::IntoIter<u16>>,
    base: u64,
}

impl<S: ChunkStream> Iterator for OrdinalIter<S> {
    type Item = Result<u64>;

    fn next(&mut self) -> Option<Result<u64>> {
        loop {
            if let Some(it) = &mut self.cur {
                if let Some(v) = it.next() {
                    return Some(Ok(self.base | v as u64));
                }
                self.cur = None;
            }
            match self.s.next_chunk() {
                Ok(Some((p, c))) => {
                    self.base = crate::chunk_base(p);
                    self.cur = Some(c.iter().collect::<Vec<_>>().into_iter());
                }
                Ok(None) => return None,
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

#[cfg(test)]
mod stats_tests {
    use super::*;
    use crate::{Expr, OrdSet};
    use std::sync::Arc;

    fn set(vals: &[u64]) -> Arc<OrdSet> {
        Arc::new(OrdSet::from_sorted_slice(vals))
    }
    /// One ordinal per chunk, so chunk count is `n`.
    fn chunks(n: u64) -> Arc<OrdSet> {
        set(&(0..n).map(|i| i << 16).collect::<Vec<_>>())
    }

    /// A leaf that claims a costly backing, to prove the plan reacts to what
    /// stands behind a stream rather than only to how many chunks it has.
    struct PagedLeaf(SetStream, u64);
    impl ChunkStream for PagedLeaf {
        fn next_chunk(&mut self) -> Result<Option<(Prefix48, Container)>> {
            self.0.next_chunk()
        }
        fn seek(&mut self, p: Prefix48) -> Result<()> {
            self.0.seek(p)
        }
        fn peek_prefix(&mut self) -> Result<Option<Prefix48>> {
            self.0.peek_prefix()
        }
        fn cardinality_dyn(&mut self) -> Result<u64> {
            self.0.cardinality_dyn()
        }
        fn stats(&self) -> StreamStats {
            StreamStats::new(self.1, None, Backing::Paged)
        }
    }

    #[test]
    fn leaves_report_exact_chunk_counts_and_spans() {
        let s = SetStream::new(chunks(5));
        let st = s.stats();
        assert_eq!(st.chunks, Some(5));
        assert_eq!(st.prefix_span, Some((0, 4)));
        assert_eq!(st.backing, Backing::Memory);

        // A range is computed, not stored, and says so.
        let r = RangeStream::new(0, 3 << 16);
        assert_eq!(r.stats().chunks, Some(3));
        assert_eq!(r.stats().backing, Backing::Computed);

        assert_eq!(EmptyStream.stats().chunks, Some(0));
    }

    /// Stats are a *runtime* property: they describe what is left, not what the
    /// stream was built from. A plan made after partial consumption must see the
    /// remainder, which is the whole reason this lives on the stream.
    #[test]
    fn stats_describe_the_remainder_not_the_original() {
        let mut s = SetStream::new(chunks(5));
        assert_eq!(s.stats().chunks, Some(5));
        s.next_chunk().unwrap();
        s.next_chunk().unwrap();
        assert_eq!(s.stats().chunks, Some(3));
        assert_eq!(s.stats().prefix_span, Some((2, 4)));
    }

    /// Boxing must not erase the channel. It did for `cardinality_dyn` once, and
    /// every stream a planner touches is boxed.
    #[test]
    fn stats_survive_type_erasure() {
        let boxed: BoxedStream = Box::new(SetStream::new(chunks(7)));
        assert_eq!(boxed.stats().chunks, Some(7));
        assert_eq!(boxed.stats().backing, Backing::Memory);
    }

    #[test]
    fn operators_compose_their_childrens_stats() {
        // Intersection: bounded by the sparser side and by the span overlap.
        let a = SetStream::new(chunks(100));
        let b = SetStream::new(set(&(50..250u64).map(|i| i << 16).collect::<Vec<_>>()));
        let and = a.and(b);
        assert_eq!(and.stats().chunks, Some(100));
        assert_eq!(and.stats().prefix_span, Some((50, 99)));

        // Provably disjoint spans give an empty intersection from stats alone.
        let lo = SetStream::new(set(&[0u64, 1 << 16]));
        let hi = SetStream::new(set(&[500u64 << 16, 501 << 16]));
        assert_eq!(lo.and(hi).stats().chunks, Some(0));

        // A merge yields the union and inherits the worse backing.
        let paged = PagedLeaf(SetStream::new(chunks(4)), 4);
        let mem = SetStream::new(chunks(6));
        let or = paged.or(mem);
        assert_eq!(or.stats().chunks, Some(10));
        assert_eq!(or.stats().backing, Backing::Paged);
    }

    /// The point of `Backing`: identical chunk counts, different cost, because
    /// different things stand behind them.
    #[test]
    fn stats_drive_the_plan_not_just_chunk_counts() {
        let mem = SetStream::new(chunks(100));
        let paged = PagedLeaf(SetStream::new(chunks(100)), 100);
        assert_eq!(mem.stats().chunks, paged.stats().chunks);
        assert!(
            paged.stats().drain_cost() > mem.stats().drain_cost() * 8,
            "a paged chunk must not cost the same as one already in memory"
        );

        // A stream that declines to report is charged pessimistically, so it
        // cannot win a comparison against one that answered.
        assert!(StreamStats::unknown().drain_cost() > mem.stats().drain_cost());
    }

    /// The n-ary union decision is made from the opened streams, so the same
    /// three-way `Or` routes differently depending on what is in it.
    #[test]
    fn the_nary_union_decision_follows_the_data_not_the_leaf_count() {
        let tiny = |n: u64| Expr::set(chunks(n));
        // Three leaves, but almost nothing flowing: the accumulator's setup is
        // not worth it. The old rule was `parts.len() >= 3` and would have taken
        // it regardless.
        let small = tiny(1).or(tiny(1)).or(tiny(1));
        let opened: Vec<BoxedStream> = {
            let mut v = Vec::new();
            let mut parts = Vec::new();
            small.flatten_or_for_test(&mut parts);
            for e in parts {
                v.push(e.open_planned());
            }
            v
        };
        assert!(!super::dynamic::use_nary_union_for_test(&opened));

        // Same shape, real volume: now it is worth it.
        let big = Expr::set(chunks(500))
            .or(Expr::set(chunks(500)))
            .or(Expr::set(chunks(500)));
        let opened: Vec<BoxedStream> = {
            let mut v = Vec::new();
            let mut parts = Vec::new();
            big.flatten_or_for_test(&mut parts);
            for e in parts {
                v.push(e.open_planned());
            }
            v
        };
        assert!(super::dynamic::use_nary_union_for_test(&opened));
    }
}
