//! Leaf streams — the sources a query tree bottoms out in.

use std::sync::Arc;

use super::{Backing, ChunkStream, StreamStats};
use crate::container::Container;
use crate::{split, OrdSet, Prefix48, Result};

/// A stream over a materialized [`OrdSet`].
///
/// Holds an `Arc`, not a borrow, so the stream is `'static + Send`. That costs
/// one atomic per stream construction and zero per chunk — and it is what lets
/// an expression be built on one thread and consumed on another.
pub struct SetStream {
    set: Arc<OrdSet>,
    idx: usize,
}

impl SetStream {
    pub fn new(set: Arc<OrdSet>) -> Self {
        SetStream { set, idx: 0 }
    }
}

impl ChunkStream for SetStream {
    fn next_chunk(&mut self) -> Result<Option<(Prefix48, Container)>> {
        let out = self.set.chunk_at(self.idx).map(|(p, c)| (p, c.clone()));
        if out.is_some() {
            self.idx += 1;
        }
        Ok(out)
    }

    /// The cached length off the borrowed container: no clone at all.
    fn next_cardinality(&mut self) -> Result<Option<(Prefix48, u64)>> {
        let out = self
            .set
            .chunk_at(self.idx)
            .map(|(p, c)| (p, c.len() as u64));
        if out.is_some() {
            self.idx += 1;
        }
        Ok(out)
    }

    fn seek(&mut self, prefix: Prefix48) -> Result<()> {
        // Gallop from the current position: forward-skewed access (the common
        // case in a merge-join) costs O(log delta), not O(log n).
        let mut step = 1usize;
        let start = self.idx;
        while let Some(p) = self.set.prefix_at(start + step) {
            if p >= prefix {
                break;
            }
            step *= 2;
        }
        let lo = start + step / 2;
        let hi = (start + step).min(self.set.chunk_count());
        self.idx = lo + self.set.partition_point_in(lo, hi, prefix);
        Ok(())
    }

    fn peek_prefix(&mut self) -> Result<Option<Prefix48>> {
        Ok(self.set.prefix_at(self.idx))
    }

    fn cardinality_hint(&self) -> (u64, Option<u64>) {
        let n = self.set.len();
        (n, Some(n))
    }

    /// Exact: the prefix array is already in memory, so the remaining chunk
    /// count and span are a subtraction and two indexed reads.
    fn stats(&self) -> StreamStats {
        let n = self.set.chunk_count();
        if self.idx >= n {
            return StreamStats::new(0, None, Backing::Memory);
        }
        let span = self.set.prefix_at(self.idx).zip(self.set.prefix_at(n - 1));
        StreamStats::new((n - self.idx) as u64, span, Backing::Memory)
    }

    /// An untouched stream reports the set's cached total; only a partly
    /// consumed one walks.
    ///
    /// # Why the walk was there at all
    ///
    /// Summing cached container lengths touches no payload, so the loop was
    /// already cheap — about 0.78 ns per chunk, two or three cycles for a bounds
    /// check, two loads and an add. The objection is not that it is slow, it is
    /// that it is **running**: when nothing has been consumed the sum it
    /// computes is exactly [`OrdSet::len`], which is a cached `u64` field. That
    /// makes counting a whole `Set` leaf `O(chunks)` where it is `O(1)`, and the
    /// planner's rewrites produce bare `Set` leaves constantly —
    /// `AndNot(a, b) -> a` when the operands are disjoint, and every part of the
    /// `Concat` a prefix-disjoint union lowers to.
    ///
    /// `idx == 0` is the whole precondition, and it is sufficient because
    /// this stream has no other state: `seek` only moves `idx`, and there is no
    /// window. A clipping wrapper is [`super::Restrict`], which delegates here
    /// only after establishing that its window contains the operand's entire
    /// span — so a delegated call at `idx == 0` really does want the whole set.
    ///
    /// The cursor is driven to the end rather than left at zero, so the
    /// exhausted-afterwards postcondition holds either way.
    fn cardinality_dyn(&mut self) -> Result<u64> {
        if self.idx == 0 {
            self.idx = self.set.chunk_count();
            return Ok(self.set.len());
        }
        let mut n = 0u64;
        while let Some((_, c)) = self.set.chunk_at(self.idx) {
            n += c.len() as u64;
            self.idx += 1;
        }
        Ok(n)
    }
}

/// The empty stream.
pub struct EmptyStream;

impl ChunkStream for EmptyStream {
    fn next_chunk(&mut self) -> Result<Option<(Prefix48, Container)>> {
        Ok(None)
    }
    fn seek(&mut self, _: Prefix48) -> Result<()> {
        Ok(())
    }
    fn peek_prefix(&mut self) -> Result<Option<Prefix48>> {
        Ok(None)
    }
    fn next_cardinality(&mut self) -> Result<Option<(Prefix48, u64)>> {
        Ok(None)
    }
    fn cardinality_hint(&self) -> (u64, Option<u64>) {
        (0, Some(0))
    }
    fn stats(&self) -> StreamStats {
        StreamStats::new(0, None, Backing::Computed)
    }
    fn cardinality_dyn(&mut self) -> Result<u64> {
        Ok(0)
    }
}

/// A stream that fails with the same error on every call.
///
/// # Why this exists rather than a fallible open
///
/// [`ChunkSource::open`](super::ChunkSource::open) returns a stream, not a
/// `Result`, because [`Expr::open_planned`](super::Expr::open_planned) returns
/// one and threading a `Result` through the whole lowering would change the
/// signature of every operator constructor for a case that only a lazy leaf can
/// produce. A source whose open fails hands back one of these instead, so the
/// failure surfaces at the first `next_chunk` — which is a place every caller
/// already handles, because every other read can fail there too.
///
/// **This must never be used to swallow an error into an empty result.** It
/// yields the error; it does not yield nothing. An `EmptyStream` in its place
/// would turn a failed read into a silently short answer, which is the failure
/// mode `Snapshot::key_stream` propagates errors to avoid in the first place.
pub struct ErrStream(crate::error::CodecError);

impl ErrStream {
    pub fn new(e: crate::error::CodecError) -> Self {
        ErrStream(e)
    }
}

impl ChunkStream for ErrStream {
    fn next_chunk(&mut self) -> Result<Option<(Prefix48, Container)>> {
        Err(self.0.clone())
    }
    fn seek(&mut self, _: Prefix48) -> Result<()> {
        Err(self.0.clone())
    }
    fn peek_prefix(&mut self) -> Result<Option<Prefix48>> {
        Err(self.0.clone())
    }
    fn next_cardinality(&mut self) -> Result<Option<(Prefix48, u64)>> {
        Err(self.0.clone())
    }
    /// Deliberately not `(0, Some(0))`: this stream knows nothing, and claiming
    /// an exact zero would let a planner rewrite the expression away.
    fn cardinality_hint(&self) -> (u64, Option<u64>) {
        (0, None)
    }
    fn stats(&self) -> StreamStats {
        StreamStats::unknown()
    }
    fn cardinality_dyn(&mut self) -> Result<u64> {
        Err(self.0.clone())
    }
}

/// A contiguous ordinal range, synthesized without any backing storage.
///
/// Useful as a literal in an expression (`x AND [lo, hi)`) and as a cheap way
/// to build very large dense operands in tests and benchmarks.
pub struct RangeStream {
    lo: u64,
    hi: u64, // exclusive
    cur: Prefix48,
}

impl RangeStream {
    /// `[lo, hi)`. An inverted or empty range yields nothing.
    pub fn new(lo: u64, hi: u64) -> Self {
        let cur = split(lo).0;
        RangeStream {
            lo,
            hi: hi.max(lo),
            cur,
        }
    }

    fn chunk_for(&self, p: Prefix48) -> Option<(Prefix48, Container)> {
        let base = crate::chunk_base(p);
        let chunk_end = base.saturating_add(crate::CHUNK_CARD as u64); // exclusive
        let s = self.lo.max(base);
        let e = self.hi.min(chunk_end);
        if s >= e {
            return None;
        }
        let start = (s - base) as u16;
        let end = (e - 1 - base) as u16;
        Some((
            p,
            Container::Run(crate::container::RunContainer::from_pairs(&[(start, end)])),
        ))
    }

    fn last_prefix(&self) -> Option<Prefix48> {
        (self.hi > self.lo).then(|| split(self.hi - 1).0)
    }
}

impl ChunkStream for RangeStream {
    fn next_chunk(&mut self) -> Result<Option<(Prefix48, Container)>> {
        let Some(last) = self.last_prefix() else {
            return Ok(None);
        };
        while self.cur <= last {
            let p = self.cur;
            self.cur += 1;
            if let Some(c) = self.chunk_for(p) {
                return Ok(Some(c));
            }
        }
        Ok(None)
    }

    /// Arithmetic. `next_chunk` builds a `Run` container to describe an
    /// interval whose width is a subtraction.
    fn next_cardinality(&mut self) -> Result<Option<(Prefix48, u64)>> {
        let Some(last) = self.last_prefix() else {
            return Ok(None);
        };
        while self.cur <= last {
            let p = self.cur;
            self.cur += 1;
            let base = crate::chunk_base(p);
            let chunk_end = base.saturating_add(crate::CHUNK_CARD as u64);
            let (s, e) = (self.lo.max(base), self.hi.min(chunk_end));
            if s < e {
                return Ok(Some((p, e - s)));
            }
        }
        Ok(None)
    }

    fn seek(&mut self, prefix: Prefix48) -> Result<()> {
        self.cur = self.cur.max(prefix);
        Ok(())
    }

    fn peek_prefix(&mut self) -> Result<Option<Prefix48>> {
        let Some(last) = self.last_prefix() else {
            return Ok(None);
        };
        let start = self.cur.max(split(self.lo).0);
        Ok((start <= last).then_some(start))
    }

    fn cardinality_hint(&self) -> (u64, Option<u64>) {
        let n = self.hi - self.lo;
        (n, Some(n))
    }

    /// Exact, and free: a range knows its own extent arithmetically and touches
    /// nothing to say so.
    fn stats(&self) -> StreamStats {
        let Some(last) = self.last_prefix() else {
            return StreamStats::new(0, None, Backing::Computed);
        };
        let first = self.cur.max(crate::split(self.lo).0);
        if first > last {
            return StreamStats::new(0, None, Backing::Computed);
        }
        StreamStats::new(last - first + 1, Some((first, last)), Backing::Computed)
    }

    /// Arithmetic, not a walk.
    ///
    /// This counted by stepping every prefix and building a container for each,
    /// which is `O(range / 65536)` for a number the range already knows: a
    /// universe-wide `Range` took longer than five seconds to report `hi - lo`,
    /// and `cardinality_hint` was returning that same value exactly and for free
    /// one method above. Nothing to do with any operator — the leaf itself was
    /// the slow part.
    fn cardinality_dyn(&mut self) -> Result<u64> {
        let Some(last) = self.last_prefix() else {
            return Ok(0);
        };
        if self.cur > last {
            return Ok(0);
        }
        // Consume the cursor, so a second call reports nothing left — the same
        // one-shot contract the walk had.
        let from = self.lo.max(crate::chunk_base(self.cur));
        self.cur = last + 1;
        Ok(self.hi.saturating_sub(from))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::ChunkStreamExt;

    fn arc(vals: &[u64]) -> Arc<OrdSet> {
        Arc::new(OrdSet::from_iter_unsorted(vals.iter().copied()))
    }

    #[test]
    fn set_stream_yields_all_chunks_in_order() {
        let s = arc(&[1, 2, 70_000, 1 << 40]);
        let got: Vec<u64> = SetStream::new(s.clone())
            .ordinals()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(got, vec![1, 2, 70_000, 1 << 40]);
    }

    #[test]
    fn set_stream_seek_positions_correctly() {
        let s = arc(&[0, 1 << 16, 2 << 16, 3 << 16, 4 << 16]);
        let mut st = SetStream::new(s);
        st.seek(2).unwrap();
        assert_eq!(st.peek_prefix().unwrap(), Some(2));
        // Seeking past the end terminates cleanly.
        st.seek(99).unwrap();
        assert_eq!(st.peek_prefix().unwrap(), None);
    }

    #[test]
    fn seek_is_idempotent_and_never_rewinds() {
        let s = arc(&[0, 1 << 16, 2 << 16, 3 << 16]);
        let mut st = SetStream::new(s);
        st.seek(2).unwrap();
        st.seek(1).unwrap(); // backwards seek must not rewind
        assert_eq!(st.peek_prefix().unwrap(), Some(2));
    }

    #[test]
    fn range_stream_spans_chunk_boundaries() {
        let vals: Vec<u64> = RangeStream::new(65_000, 200_000)
            .ordinals()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(vals.first(), Some(&65_000));
        assert_eq!(vals.last(), Some(&199_999));
        assert_eq!(vals.len(), 135_000);
    }

    #[test]
    fn range_stream_cardinality_matches_iteration() {
        for (lo, hi) in [(0u64, 0u64), (5, 6), (0, 65_536), (100, 300_000)] {
            let n = RangeStream::new(lo, hi).cardinality().unwrap();
            let it = RangeStream::new(lo, hi).ordinals().count() as u64;
            assert_eq!(n, it, "range [{lo}, {hi})");
            assert_eq!(n, hi - lo);
        }
    }

    /// Counting an untouched `Set` leaf must not scale with its chunk count.
    ///
    /// # Why this is timed, when nothing else here is
    ///
    /// The claim is *asymptotic*, and the two implementations are
    /// indistinguishable by every other means available: both return the same
    /// number, neither allocates, and neither touches a payload. There is no
    /// spy to install — the walk is over `OrdSet`'s own arrays, inside the leaf.
    /// So the only observable that separates `O(1)` from `O(chunks)` is cost,
    /// and the honest way to use cost is a **ratio between two sizes** rather
    /// than an absolute threshold.
    ///
    /// The margin is deliberately enormous. A 100x difference in chunk count
    /// gives ~100x under the walk and ~1x under the field read, and the bound is
    /// 10x — an order of magnitude clear of both, which is what makes it
    /// survivable on a machine where timings have swung 2x within one run.
    #[test]
    fn counting_an_untouched_set_does_not_scale_with_its_chunks() {
        fn build(chunks: u64) -> Arc<OrdSet> {
            Arc::new(OrdSet::from_sorted_slice(
                &(0..chunks).map(|p| p << 16).collect::<Vec<_>>(),
            ))
        }
        let (small, large) = (build(500), build(50_000));

        let time = |s: &Arc<OrdSet>| {
            for _ in 0..20 {
                SetStream::new(s.clone()).cardinality_dyn().unwrap();
            }
            let t = std::time::Instant::now();
            for _ in 0..200 {
                SetStream::new(s.clone()).cardinality_dyn().unwrap();
            }
            t.elapsed().as_secs_f64()
        };
        assert_eq!(
            SetStream::new(large.clone()).cardinality_dyn().unwrap(),
            50_000
        );

        // Minimum of three, because noise only ever adds time.
        let ratio = (0..3)
            .map(|_| time(&large) / time(&small))
            .fold(f64::MAX, f64::min);
        assert!(
            ratio < 10.0,
            "counting a 50 000-chunk set cost {ratio:.1}x a 500-chunk one — it is \
             walking the chunks rather than reading `OrdSet::len`"
        );
    }

    #[test]
    fn empty_stream_is_empty() {
        assert!(EmptyStream.is_empty().unwrap());
        assert_eq!(EmptyStream.cardinality().unwrap(), 0);
    }
}
