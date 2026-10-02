//! Streaming k-way union.
//!
//! The eager form is [`crate::ops::nary::union_all`]; this is the same shape as
//! a [`ChunkStream`], so a k-way OR can be composed into an expression and can
//! answer `cardinality()` without materializing a result.
//!
//! # Why `cardinality_dyn` is the point
//!
//! `ChunkStream::cardinality` is a required method with no default body,
//! precisely so a new operator cannot silently inherit the materializing one.
//! For a k-way union that inheritance would be expensive twice over: a result
//! container per prefix, **and** an `optimize()` on each one to pick its
//! encoding — all to then read `len()` off it and drop it. Here a prefix with
//! one contributor is answered from its cached cardinality with no payload
//! access at all, and a prefix with several is popcounted straight out of the
//! accumulator.

use crate::container::Container;
use crate::ops::generic::SetOp;
use crate::ops::nary::Scratch;
use crate::stream::ChunkStream;

/// `(prefix, container)`, matching `stream::ops`.
type Chunk = (Prefix48, Container);
use crate::{CodecError, Prefix48};

type Result<T> = std::result::Result<T, CodecError>;

/// Union of many streams, evaluated one prefix at a time.
pub struct UnionAll {
    streams: Vec<Box<dyn ChunkStream>>,
    pending: Option<Chunk>,
    scratch: Scratch,
    /// Reused across prefixes so a wide k does not allocate per chunk.
    group: Vec<Container>,
}

impl UnionAll {
    pub fn new(streams: Vec<Box<dyn ChunkStream>>) -> Self {
        let n = streams.len();
        UnionAll {
            streams,
            pending: None,
            scratch: Scratch::new(),
            group: Vec::with_capacity(n),
        }
    }

    /// The lowest prefix any stream is sitting on, **how many sit there**, and
    /// the first that does.
    ///
    /// A linear scan over k, not a heap: k is small next to the chunk count,
    /// and a heap would need re-sifting every time a cursor advanced anyway.
    ///
    /// # Why the contributor count comes from here
    ///
    /// A prefix with a single contributor needs no merge, so it needs no
    /// payload — but `gather` cannot know that until it has already fetched
    /// one. Counting is a property of the peeks, and this pass is already
    /// looking at every peek, so it costs nothing to carry out. Doing it in a
    /// second pass would have cost `k` extra peeks per prefix to save one
    /// fetch, which is not obviously a trade at all.
    fn min_at(&mut self) -> Result<Option<(Prefix48, usize, usize)>> {
        let (mut min, mut count, mut first) = (None, 0usize, 0usize);
        for (i, s) in self.streams.iter_mut().enumerate() {
            let Some(p) = s.peek_prefix()? else {
                continue;
            };
            match min {
                Some(m) if p > m => {}
                Some(m) if p == m => count += 1,
                _ => {
                    min = Some(p);
                    count = 1;
                    first = i;
                }
            }
        }
        Ok(min.map(|m| (m, count, first)))
    }

    fn min_prefix(&mut self) -> Result<Option<Prefix48>> {
        Ok(self.min_at()?.map(|(p, _, _)| p))
    }

    /// Take every stream's chunk at `prefix` into `self.group`.
    fn gather(&mut self, prefix: Prefix48) -> Result<()> {
        self.group.clear();
        for s in &mut self.streams {
            if s.peek_prefix()? == Some(prefix) {
                if let Some((_, c)) = s.next_chunk()? {
                    self.group.push(c);
                }
            }
        }
        Ok(())
    }

    fn produce(&mut self) -> Result<Option<Chunk>> {
        loop {
            let Some(prefix) = self.min_prefix()? else {
                return Ok(None);
            };
            self.gather(prefix)?;
            let merged = match self.group.len() {
                // `peek_prefix` is a lower bound, so a contributor can vanish.
                0 => continue,
                1 => Some(self.group[0].clone()),
                2 => crate::ops::apply(SetOp::Or, &self.group[0], &self.group[1]),
                _ => {
                    for c in &self.group {
                        self.scratch.or_in(c);
                    }
                    self.scratch.take()
                }
            };
            match merged {
                Some(c) => return Ok(Some((prefix, c))),
                None => continue,
            }
        }
    }

    fn fill(&mut self) -> Result<()> {
        if self.pending.is_none() {
            self.pending = self.produce()?;
        }
        Ok(())
    }
}

impl ChunkStream for UnionAll {
    fn next_chunk(&mut self) -> Result<Option<Chunk>> {
        self.fill()?;
        Ok(self.pending.take())
    }

    /// A sole-contributor prefix passes through, so its count is its
    /// contributor's and no container need exist.
    ///
    /// Everything else falls through to `next_chunk` deliberately: with two or
    /// more contributors a merged container is unavoidable, and re-deriving the
    /// merge here would be a second implementation of `produce` to keep in step.
    fn next_cardinality(&mut self) -> Result<Option<(Prefix48, u64)>> {
        if self.pending.is_none() {
            while let Some((prefix, contributors, first)) = self.min_at()? {
                if contributors != 1 {
                    break;
                }
                if let Some((p, k)) = self.streams[first].next_cardinality()? {
                    debug_assert_eq!(p, prefix);
                    if k > 0 {
                        return Ok(Some((p, k)));
                    }
                }
            }
        }
        Ok(self.next_chunk()?.map(|(p, c)| (p, c.len() as u64)))
    }

    fn seek(&mut self, prefix: Prefix48) -> Result<()> {
        if self.pending.as_ref().is_some_and(|(p, _)| *p < prefix) {
            self.pending = None;
        }
        if self.pending.is_none() {
            for s in &mut self.streams {
                s.seek(prefix)?;
            }
        }
        Ok(())
    }

    fn peek_prefix(&mut self) -> Result<Option<Prefix48>> {
        self.fill()?;
        Ok(self.pending.as_ref().map(|(p, _)| *p))
    }

    fn cardinality_hint(&self) -> (u64, Option<u64>) {
        // The union is at least the largest input and at most their sum.
        let mut lower = 0u64;
        let mut upper = Some(0u64);
        for s in &self.streams {
            let (l, u) = s.cardinality_hint();
            lower = lower.max(l);
            upper = upper.zip(u).map(|(a, b)| a.saturating_add(b));
        }
        (lower, upper)
    }

    /// Union of many: sum the parts, span the extremes, and inherit the worst
    /// backing among them.
    fn stats(&self) -> super::StreamStats {
        let mut chunks = Some(0u64);
        let mut span: Option<(crate::Prefix48, crate::Prefix48)> = None;
        let mut backing = super::Backing::Computed;
        for s in &self.streams {
            let st = s.stats();
            chunks = chunks.zip(st.chunks).map(|(a, b)| a.saturating_add(b));
            span = match (span, st.prefix_span) {
                (Some((la, ha)), Some((lb, hb))) => Some((la.min(lb), ha.max(hb))),
                (x, y) => x.or(y),
            };
            backing = backing.worse(st.backing);
        }
        super::StreamStats {
            chunks,
            prefix_span: span,
            backing,
        }
    }

    fn cardinality_dyn(&mut self) -> Result<u64> {
        let mut n = self.pending.take().map_or(0, |(_, c)| c.len() as u64);
        while let Some((prefix, contributors, first)) = self.min_at()? {
            // One contributor: nothing to merge, so nothing to fetch. The
            // stream reports its own count and never hands over a container.
            //
            // This arm used to run *after* `gather`, which had already
            // fetched that container — so "no payload touched" was true of the
            // arithmetic and false of the walk. `gather` cannot make this
            // decision because by the time it could, it has already paid.
            if contributors == 1 {
                if let Some((p, k)) = self.streams[first].next_cardinality()? {
                    debug_assert_eq!(
                        p, prefix,
                        "a stream reported prefix {prefix} and then yielded {p}"
                    );
                    n += k;
                }
                continue;
            }
            self.gather(prefix)?;
            n += match self.group.len() {
                0 => continue,
                // Reachable when a peeked contributor vanishes: `peek_prefix`
                // is a lower bound by contract.
                1 => self.group[0].len() as u64,
                // Two: the non-allocating identity, not a materialized union.
                2 => crate::ops::or_cardinality(&self.group[0], &self.group[1]) as u64,
                _ => {
                    for c in &self.group {
                        self.scratch.or_in(c);
                    }
                    self.scratch.take_len() as u64
                }
            };
        }
        Ok(n)
    }
}

/// Ordered concatenation of many prefix-disjoint streams, driven by one cursor.
///
/// # Why this is n-ary and not a chain of binary concatenations
///
/// Folding `k` parts pairwise built `Concat( Concat( .. ), r )`, and that operator forwarded
/// `seek` to **both** children with nothing to compare the target against. A `seek` on the
/// root therefore re-sought every leaf, so one seek cost `O(k)` and a walk that seeks once
/// per prefix cost `O(k · p)`. `And::cardinality_dyn` does exactly that -- it seeks its other
/// side once per candidate prefix -- which is how a 399-range union against 511 selected
/// chunks spent 0.92 ms seeking to perform 0.08 ms of kernels. The cursor here only moves
/// forward, so the whole walk advances it `k` times in total and each seek is `O(1)`
/// amortized.
///
/// **The load-bearing part is `ends`, not the flatness**, and that is worth stating because
/// it is easy to get backwards. A *chain* of pairs that each knew their left subtree's upper
/// bound would skip in `O(log k)` and be perfectly adequate -- measured while checking that
/// the seek-budget test could see the defect at all: nesting this very operator pairwise
/// still met the budget, because each level could skip its left subtree arithmetically.
/// What was quadratic was descending into a subtree to ask it a question its bound already
/// answered. The flat form is preferred because it needs no tree, not because a tree could
/// not be fixed.
///
/// Measured on that shape, 399 ranges and 511 chunks: seeking once per selected prefix went
/// from 0.93 ms to 0.020 ms ( 47x ), and draining every chunk from 0.61 ms to 0.018 ms
/// ( 34x ). At 102 ranges, 11x and 8x -- the gap scales with `k`, as a quadratic term does.
/// End to end, the planned count walk behind Flight's cache-page filter went from 1.86 ms to
/// 0.198 ms, which is now *below* the 0.218 ms a materialized bitmap mask costs for the same
/// answer, and within 2.4x of the 0.083 ms the kernels alone need.
///
/// **At small `k` it is a wash, and that was checked rather than assumed**: against the
/// chain it replaced, `k = 2` measured 0.86x ( a hair slower, at a 100 ns measurement ),
/// `k = 4` parity, `k = 8` 1.5x, `k = 16` 2.4x, `k = 64` 7.9x. Nothing regresses at the
/// sizes where the chain was fine.
///
/// # The precondition, and why it is worth having
///
/// **Every prefix of part `i` must be strictly less than every prefix of part `i + 1`.**
/// Given that, a union needs no merge at all: drain each part in turn. No per-prefix peek
/// across `k` cursors, no comparison, no kernel call -- and `cardinality` is a plain sum, so
/// a part that can count itself arithmetically ( a `RangeStream` ) contributes in `O(1)`
/// instead of being walked.
///
/// That is not a micro-optimization. `Or`'s cardinality loop peeks every side once per
/// prefix, so `Or( small_set, huge_disjoint_range )` costs one step per chunk **of the
/// range** and does not finish. Concatenated, it is instant.
///
/// # `ends` is what makes `seek` cheap, and it must bound from above
///
/// `ends[ i ]` is an **inclusive upper bound** on the prefixes part `i` can yield. `seek`
/// skips a part by comparing against that number rather than by descending into the part, so
/// skipping 300 ranges costs 300 integer comparisons and not 300 stream seeks.
///
/// The bound must be sound in one direction only: it may be *wider* than the part's real
/// extent, never narrower. A bound that is too low would skip a part that still had chunks
/// at or above the target, silently dropping them. The callers take it from the planner's own
/// `prefix_span` walk, which is the same bound their disjointness test already relies on -- a
/// span that did not contain every prefix an operand can produce would make *that* conclusion
/// wrong too, so this adds no new obligation.
///
/// # It is selected, never assumed
///
/// Violating the ordering yields chunks out of order, which every operator above silently
/// mis-merges. So this is not something callers are asked to get right:
/// [`crate::stream::Expr::open`] builds it only where the parts are *shown* prefix-disjoint
/// -- from the segmentation it just computed, or from a `prefix_span` test over the union's
/// parts -- and the `debug_assert` below fails loudly if it is ever constructed by hand over
/// overlapping parts.
///
/// Both tests are on **prefixes**. `{0, 2}` and `{3, 5}` are disjoint and ordered as *sets*
/// and still share chunk 0, so an ordinal-level test would admit them and the concatenation
/// would emit that chunk twice.
pub struct ConcatAll {
    parts: Vec<Box<dyn ChunkStream>>,
    /// Inclusive upper bound on each part's prefixes, parallel to `parts`.
    ends: Vec<Prefix48>,
    /// Parts below this are finished: drained, or skipped by a `seek` past them.
    at: usize,
    #[cfg(debug_assertions)]
    last: Option<Prefix48>,
}

impl ConcatAll {
    /// `parts` in ascending prefix order, with `ends[ i ]` bounding part `i` from above.
    pub fn new(parts: Vec<Box<dyn ChunkStream>>, ends: Vec<Prefix48>) -> Self {
        debug_assert_eq!(
            parts.len(),
            ends.len(),
            "every part needs its own upper bound"
        );
        debug_assert!(
            ends.windows(2).all(|w| w[0] < w[1]),
            "ConcatAll parts are not in strictly ascending prefix order: {ends:?}"
        );
        ConcatAll {
            parts,
            ends,
            at: 0,
            #[cfg(debug_assertions)]
            last: None,
        }
    }

    #[inline]
    fn check(&mut self, p: Prefix48) {
        #[cfg(debug_assertions)]
        {
            debug_assert!(
                self.last.is_none_or(|prev| prev < p),
                "ConcatAll parts are not prefix-disjoint and ordered: {:?} then {p}",
                self.last
            );
            self.last = Some(p);
        }
        let _ = p;
    }

    /// The index of the first part that is not finished, having retired any that are.
    #[inline]
    fn live(&self) -> usize {
        self.at.min(self.parts.len())
    }
}

impl ChunkStream for ConcatAll {
    fn next_chunk(&mut self) -> Result<Option<Chunk>> {
        while self.at < self.parts.len() {
            if let Some(c) = self.parts[self.at].next_chunk()? {
                self.check(c.0);
                return Ok(Some(c));
            }
            self.at += 1;
        }
        Ok(None)
    }

    fn next_cardinality(&mut self) -> Result<Option<(Prefix48, u64)>> {
        while self.at < self.parts.len() {
            if let Some(c) = self.parts[self.at].next_cardinality()? {
                self.check(c.0);
                return Ok(Some(c));
            }
            self.at += 1;
        }
        Ok(None)
    }

    /// Skip whole parts arithmetically, then seek only the one that can hold the target.
    ///
    /// Seeks are monotonic, so the cursor never rewinds and the total advance across a walk
    /// is `k`. This is the whole reason the operator exists.
    fn seek(&mut self, prefix: Prefix48) -> Result<()> {
        while self.at < self.parts.len() && self.ends[self.at] < prefix {
            self.at += 1;
        }
        if self.at < self.parts.len() {
            self.parts[self.at].seek(prefix)?;
        }
        Ok(())
    }

    fn peek_prefix(&mut self) -> Result<Option<Prefix48>> {
        while self.at < self.parts.len() {
            if let Some(p) = self.parts[self.at].peek_prefix()? {
                return Ok(Some(p));
            }
            self.at += 1;
        }
        Ok(None)
    }

    fn cardinality_hint(&self) -> (u64, Option<u64>) {
        let mut lo = 0u64;
        let mut hi = Some(0u64);
        for s in &self.parts[self.live()..] {
            let (a, b) = s.cardinality_hint();
            lo = lo.saturating_add(a);
            hi = hi.zip(b).map(|(x, y)| x.saturating_add(y));
        }
        (lo, hi)
    }

    /// Spans the remaining parts' extremes, which are already ordered.
    fn stats(&self) -> super::StreamStats {
        let mut chunks = Some(0u64);
        let mut span: Option<(Prefix48, Prefix48)> = None;
        let mut backing = super::Backing::Computed;
        for s in &self.parts[self.live()..] {
            let st = s.stats();
            chunks = chunks.zip(st.chunks).map(|(a, b)| a.saturating_add(b));
            span = match (span, st.prefix_span) {
                (Some((la, _)), Some((_, hb))) => Some((la, hb)),
                (x, y) => x.or(y),
            };
            backing = backing.worse(st.backing);
        }
        super::StreamStats {
            chunks,
            prefix_span: span,
            backing,
        }
    }

    /// A sum, not a merge. This is the other half of the point of the operator.
    ///
    /// Parts the cursor has passed contribute nothing: a drained part has nothing left, and
    /// one skipped by `seek` was deliberately stepped over. That matches what a chain did,
    /// where a seeked-past side simply counted zero.
    fn cardinality_dyn(&mut self) -> Result<u64> {
        let mut n = 0u64;
        let from = self.live();
        for s in &mut self.parts[from..] {
            n = n.saturating_add(s.cardinality_dyn()?);
        }
        Ok(n)
    }
}

#[cfg(test)]
mod counting_tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    use super::*;
    use crate::stream::ops::counting_tests::{spread, Spy};
    use crate::OrdSet;

    /// A prefix with one contributor must be counted, not fetched.
    ///
    /// The operands are span-disjoint, so every prefix has exactly one — which
    /// is the arm `cardinality_dyn` always claimed touched no payload, while
    /// `gather` had already fetched the container before the arm was chosen.
    #[test]
    fn a_sole_contributor_prefix_is_counted_not_gathered() {
        const N: u64 = 300;
        let (chunks, counts) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let streams: Vec<Box<dyn ChunkStream>> = (0..3u64)
            .map(|i| {
                Box::new(Spy::new(spread(i * 10_000, N), &chunks, &counts)) as Box<dyn ChunkStream>
            })
            .collect();

        let n = UnionAll::new(streams).cardinality_dyn().unwrap();
        assert_eq!(n, 3 * N * 50, "wrong cardinality");
        assert_eq!(
            chunks.load(Ordering::Relaxed),
            0,
            "UnionAll gathered payloads for prefixes with a single contributor"
        );
        assert!(
            counts.load(Ordering::Relaxed) > 0,
            "watching the wrong path"
        );
    }

    /// Overlapping operands still need real merges — the fast path must not
    /// swallow them, and the answer must not change.
    #[test]
    fn overlapping_contributors_still_merge() {
        const N: u64 = 60;
        let sets: Vec<Arc<OrdSet>> = (0..3u64).map(|i| spread(i * (N / 2), N)).collect();
        let mk = || -> Vec<Box<dyn ChunkStream>> {
            sets.iter()
                .map(|s| Box::new(crate::stream::SetStream::new(s.clone())) as Box<dyn ChunkStream>)
                .collect()
        };
        let counted = UnionAll::new(mk()).cardinality_dyn().unwrap();

        // Oracle: the eager union of the same sets.
        let mut want = OrdSet::new();
        for s in &sets {
            want = want.or(s);
        }
        assert_eq!(
            counted,
            want.len(),
            "k-way union count disagrees with the set"
        );

        // And the streamed chunks must still equal it.
        let mut drained = 0u64;
        let mut u = UnionAll::new(mk());
        while let Some((_, c)) = u.next_chunk().unwrap() {
            drained += c.len() as u64;
        }
        assert_eq!(drained, want.len(), "drained union disagrees with the set");
    }
}
