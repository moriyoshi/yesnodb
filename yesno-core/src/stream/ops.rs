//! Streaming set operators.
//!
//! # Why operators buffer one chunk
//!
//! An operator's `peek_prefix` cannot be answered from its children's prefixes
//! alone: XOR of two equal chunks, or ANDNOT of a chunk by itself, cancels to
//! empty and must be skipped. So an operator that reported a prefix and then
//! yielded nothing would break every parent that peeked it.
//!
//! Each operator therefore keeps a **one-slot lookahead**, filled by `produce`,
//! which makes its `peek_prefix` exact. Leaves do *not* buffer — their peek
//! reads the prefix array without touching a payload, which is what keeps
//! seek-driven AND cheap where it actually matters.
//!
//! # Cardinality overrides
//!
//! Each operator overrides [`ChunkStream::cardinality_dyn`] with a walk that
//! never builds a result container. Without those overrides the object-safe
//! path would silently fall back to materializing, and the identities in
//! [`crate::ops::card`] would buy nothing.
//!
//! **That does not generalize to [`ChunkStream::next_cardinality`], and the
//! generalization is tempting enough to have been tried.** It has the same
//! materializing default body, and no operator here overrides it — which reads
//! like the same gap. It is not. Overriding it is load-bearing on *leaves*
//! ( `SetStream` reads the cached length off a borrowed container and never
//! clones ) and is **dead weight on operators**, because every parent that walks
//! calls `peek_prefix` on a child before `next_cardinality` — it has to, to
//! decide which side contributes — and `peek_prefix` has already filled the
//! one-slot lookahead via `produce`. By the time `next_cardinality` runs there is
//! no allocation left to save. Measured on `Or( And, And )` over 400 chunks:
//! 469 allocations with the four overrides added and 469 without. See JOURNAL,
//! 2026-08-26.
//!
//! The cost that *does* show up there is `produce` itself running the
//! materializing kernel to make `peek_prefix` exact, and that is the deliberate
//! property described above rather than a defect: a parent counting `|A ∩ B|`
//! needs the operands' actual chunk contents, so there is nothing to elide.

use super::{Backing, ChunkStream, StreamStats};
use crate::container::Container;
use crate::ops as kern;
use crate::{Prefix48, Result};

type Chunk = (Prefix48, Container);

/// Intersection. Strictly seek-driven: the denser operand is never scanned.
pub struct And<L, R> {
    l: L,
    r: R,
    pending: Option<Chunk>,
}

impl<L: ChunkStream, R: ChunkStream> And<L, R> {
    pub fn new(l: L, r: R) -> Self {
        And {
            l,
            r,
            pending: None,
        }
    }

    /// Leapfrog to the next shared prefix and intersect there.
    ///
    /// Seeks the lagging side to the other's prefix rather than stepping, so
    /// intersecting a 12-chunk set with a 15 000-chunk one touches ~12 chunks of
    /// the latter: O(min(n, m) · log max(n, m)).
    fn produce(&mut self) -> Result<Option<Chunk>> {
        loop {
            let Some(pa) = self.l.peek_prefix()? else {
                return Ok(None);
            };
            self.r.seek(pa)?;
            let Some(pb) = self.r.peek_prefix()? else {
                return Ok(None);
            };
            if pb > pa {
                self.l.seek(pb)?;
                // `seek` is monotonic and pb > pa, so the next peek is strictly
                // greater than pa: the loop makes progress.
                continue;
            }
            let (Some((_, ca)), Some((_, cb))) = (self.l.next_chunk()?, self.r.next_chunk()?)
            else {
                return Ok(None);
            };
            if let Some(c) = kern::and(&ca, &cb) {
                return Ok(Some((pa, c)));
            }
            // Empty intersection here: keep going rather than yield an empty chunk.
        }
    }

    fn fill(&mut self) -> Result<()> {
        if self.pending.is_none() {
            self.pending = self.produce()?;
        }
        Ok(())
    }
}

impl<L: ChunkStream, R: ChunkStream> ChunkStream for And<L, R> {
    fn next_chunk(&mut self) -> Result<Option<Chunk>> {
        self.fill()?;
        Ok(self.pending.take())
    }

    fn seek(&mut self, prefix: Prefix48) -> Result<()> {
        if self.pending.as_ref().is_some_and(|(p, _)| *p < prefix) {
            self.pending = None;
        }
        if self.pending.is_none() {
            self.l.seek(prefix)?;
            self.r.seek(prefix)?;
        }
        Ok(())
    }

    fn peek_prefix(&mut self) -> Result<Option<Prefix48>> {
        self.fill()?;
        Ok(self.pending.as_ref().map(|(p, _)| *p))
    }

    fn cardinality_hint(&self) -> (u64, Option<u64>) {
        let (_, ua) = self.l.cardinality_hint();
        let (_, ub) = self.r.cardinality_hint();
        let upper = match (ua, ub) {
            (Some(x), Some(y)) => Some(x.min(y)),
            (a, b) => a.or(b),
        };
        (0, upper)
    }

    /// An intersection yields at most the sparser side, and cannot escape the
    /// overlap of the two prefix spans — which is often much tighter.
    fn stats(&self) -> StreamStats {
        let (a, b) = (self.l.stats(), self.r.stats());
        let span = match (a.prefix_span, b.prefix_span) {
            (Some((la, ha)), Some((lb, hb))) => {
                let (lo, hi) = (la.max(lb), ha.min(hb));
                (lo <= hi).then_some((lo, hi))
            }
            (x, y) => x.or(y),
        };
        let chunks = match (a.chunks, b.chunks) {
            (Some(x), Some(y)) => Some(x.min(y)),
            (x, y) => x.or(y),
        };
        // Disjoint spans mean an empty result, whatever the counts said.
        let chunks = if span.is_none() && a.prefix_span.is_some() && b.prefix_span.is_some() {
            Some(0)
        } else {
            chunks
        };
        StreamStats {
            chunks,
            prefix_span: span,
            backing: a.backing.worse(b.backing),
        }
    }

    fn cardinality_dyn(&mut self) -> Result<u64> {
        let mut n = self.pending.take().map_or(0, |(_, c)| c.len() as u64);
        while let Some(pa) = self.l.peek_prefix()? {
            self.r.seek(pa)?;
            let Some(pb) = self.r.peek_prefix()? else {
                break;
            };
            if pb > pa {
                self.l.seek(pb)?;
                continue;
            }
            let (Some((_, ca)), Some((_, cb))) = (self.l.next_chunk()?, self.r.next_chunk()?)
            else {
                break;
            };
            n += kern::and_cardinality(&ca, &cb) as u64;
        }
        Ok(n)
    }
}

/// OR and XOR share a two-way merge; they differ only in the pairwise kernel
/// and in whether a single-sided chunk survives (it always does for both, but
/// the kernel differs, and XOR can cancel).
macro_rules! merge_operator {
    ($name:ident, $kern:path, $card:path) => {
        pub struct $name<L, R> {
            l: L,
            r: R,
            pending: Option<Chunk>,
        }

        impl<L: ChunkStream, R: ChunkStream> $name<L, R> {
            pub fn new(l: L, r: R) -> Self {
                $name {
                    l,
                    r,
                    pending: None,
                }
            }

            fn produce(&mut self) -> Result<Option<Chunk>> {
                loop {
                    let pa = self.l.peek_prefix()?;
                    let pb = self.r.peek_prefix()?;
                    match (pa, pb) {
                        (None, None) => return Ok(None),
                        // Single-sided chunks pass through by clone: a refcount
                        // bump, not a copy.
                        (Some(_), None) => return self.l.next_chunk(),
                        (None, Some(_)) => return self.r.next_chunk(),
                        (Some(x), Some(y)) if x < y => return self.l.next_chunk(),
                        (Some(x), Some(y)) if y < x => return self.r.next_chunk(),
                        (Some(x), Some(_)) => {
                            let (Some((_, ca)), Some((_, cb))) =
                                (self.l.next_chunk()?, self.r.next_chunk()?)
                            else {
                                return Ok(None);
                            };
                            match $kern(&ca, &cb) {
                                Some(c) => return Ok(Some((x, c))),
                                None => continue, // cancelled: skip, do not yield empty
                            }
                        }
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

        impl<L: ChunkStream, R: ChunkStream> ChunkStream for $name<L, R> {
            fn next_chunk(&mut self) -> Result<Option<Chunk>> {
                self.fill()?;
                Ok(self.pending.take())
            }

            fn seek(&mut self, prefix: Prefix48) -> Result<()> {
                if self.pending.as_ref().is_some_and(|(p, _)| *p < prefix) {
                    self.pending = None;
                }
                if self.pending.is_none() {
                    self.l.seek(prefix)?;
                    self.r.seek(prefix)?;
                }
                Ok(())
            }

            fn peek_prefix(&mut self) -> Result<Option<Prefix48>> {
                self.fill()?;
                Ok(self.pending.as_ref().map(|(p, _)| *p))
            }

            fn cardinality_hint(&self) -> (u64, Option<u64>) {
                let (_, ua) = self.l.cardinality_hint();
                let (_, ub) = self.r.cardinality_hint();
                // Saturating: a universe-wide range legitimately reports
                // `u64::MAX`, and two of them overflow. `cardinality_hint` is
                // public and delegated through `BoxedStream`, so this panics in
                // a debug build and wraps in release.
                (0, ua.zip(ub).map(|(x, y)| x.saturating_add(y)))
            }

            /// A merge visits both sides and yields their union, so the chunk
            /// count is a sum bounded below by the larger side — the overlap is
            /// not knowable from stats alone, which is what
            /// `stream::sketch` exists to estimate at plan time.
            fn stats(&self) -> StreamStats {
                let (a, b) = (self.l.stats(), self.r.stats());
                let span = match (a.prefix_span, b.prefix_span) {
                    (Some((la, ha)), Some((lb, hb))) => Some((la.min(lb), ha.max(hb))),
                    (x, y) => x.or(y),
                };
                StreamStats {
                    chunks: a.chunks.zip(b.chunks).map(|(x, y)| x.saturating_add(y)),
                    prefix_span: span,
                    backing: a.backing.worse(b.backing),
                }
            }

            fn cardinality_dyn(&mut self) -> Result<u64> {
                let mut n = self.pending.take().map_or(0, |(_, c)| c.len() as u64);
                loop {
                    let pa = self.l.peek_prefix()?;
                    let pb = self.r.peek_prefix()?;
                    // Which side (or both) contributes at the next prefix.
                    enum Take {
                        Left,
                        Right,
                        Both,
                        Done,
                    }
                    let take = match (pa, pb) {
                        (None, None) => Take::Done,
                        (Some(_), None) => Take::Left,
                        (None, Some(_)) => Take::Right,
                        (Some(x), Some(y)) if x < y => Take::Left,
                        (Some(x), Some(y)) if y < x => Take::Right,
                        (Some(_), Some(_)) => Take::Both,
                    };
                    match take {
                        Take::Done => break,
                        // Single-sided: the cached container length, no payload
                        // work and no result container.
                        //
                        // This said exactly that before it was true. It read
                        // the length via `next_chunk`, which hands back a cloned
                        // `Container` — a refcount bump on a frozen payload, so
                        // no copy and no allocation, but two atomics and an enum
                        // move per chunk for a number already in the index.
                        Take::Left => match self.l.next_cardinality()? {
                            Some((_, k)) => n += k,
                            None => break,
                        },
                        Take::Right => match self.r.next_cardinality()? {
                            Some((_, k)) => n += k,
                            None => break,
                        },
                        Take::Both => {
                            let (Some((_, ca)), Some((_, cb))) =
                                (self.l.next_chunk()?, self.r.next_chunk()?)
                            else {
                                break;
                            };
                            // Identity-based: no result container is built.
                            n += $card(&ca, &cb) as u64;
                        }
                    }
                }
                Ok(n)
            }
        }
    };
}

merge_operator!(Or, kern::or, kern::or_cardinality);
merge_operator!(Xor, kern::xor, kern::xor_cardinality);

/// Difference. Iterates the left side and *seeks* the right; right-only
/// prefixes are never visited.
/// A stream clipped to the prefix window `[lo, hi]`.
///
/// The other half of split/concatenate: segmentation cuts the prefix domain at
/// the points where the set of contributing operands changes, and this is what
/// confines an operand to one segment. Concatenating the segments reproduces the
/// original stream exactly, because the windows are ordered and disjoint.
///
/// Clipping is a `seek` plus a bound, so it costs nothing per chunk — no
/// container is touched, and an operand that can seek cheaply skips the segments
/// it does not appear in.
pub struct Restrict<S> {
    inner: S,
    hi: Prefix48,
    started: bool,
    lo: Prefix48,
}

impl<S: ChunkStream> Restrict<S> {
    /// Inclusive on both ends.
    pub fn new(inner: S, lo: Prefix48, hi: Prefix48) -> Self {
        Restrict {
            inner,
            hi,
            started: false,
            lo,
        }
    }

    fn ensure_started(&mut self) -> Result<()> {
        if !self.started {
            self.started = true;
            self.inner.seek(self.lo)?;
        }
        Ok(())
    }
}

impl<S: ChunkStream> ChunkStream for Restrict<S> {
    fn next_chunk(&mut self) -> Result<Option<Chunk>> {
        self.ensure_started()?;
        match self.inner.peek_prefix()? {
            Some(p) if p <= self.hi => self.inner.next_chunk(),
            _ => Ok(None),
        }
    }

    fn seek(&mut self, prefix: Prefix48) -> Result<()> {
        self.lo = self.lo.max(prefix);
        if self.started {
            self.inner.seek(prefix)?;
        }
        Ok(())
    }

    fn peek_prefix(&mut self) -> Result<Option<Prefix48>> {
        self.ensure_started()?;
        Ok(self.inner.peek_prefix()?.filter(|p| *p <= self.hi))
    }

    fn cardinality_hint(&self) -> (u64, Option<u64>) {
        (0, self.inner.cardinality_hint().1)
    }

    fn stats(&self) -> StreamStats {
        let inner = self.inner.stats();
        let span = inner
            .prefix_span
            .map(|(a, b)| (a.max(self.lo), b.min(self.hi)))
            .filter(|(a, b)| a <= b);
        StreamStats {
            // The window can only shrink what the inner stream would yield.
            chunks: inner
                .chunks
                .map(|n| n.min(self.hi.saturating_sub(self.lo).saturating_add(1))),
            prefix_span: span,
            backing: inner.backing,
        }
    }

    /// Delegates when the window does not actually clip anything.
    ///
    /// Walking chunk by chunk defeats the purpose: a segment that contains a
    /// whole operand must report that operand's own count, and for a range that
    /// is a subtraction rather than `2^48` steps. Without this the segmentation
    /// in `Expr::open` still enumerated the universe — it built the right plan
    /// and then executed it the slow way.
    fn cardinality_dyn(&mut self) -> Result<u64> {
        self.ensure_started()?;
        if let Some((a, b)) = self.inner.stats().prefix_span {
            if a >= self.lo && b <= self.hi {
                return self.inner.cardinality_dyn();
            }
        }
        let mut n = 0u64;
        while let Some(p) = self.inner.peek_prefix()? {
            if p > self.hi {
                break;
            }
            match self.inner.next_cardinality()? {
                Some((_, k)) => n += k,
                None => break,
            }
        }
        Ok(n)
    }

    /// A window that does not clip a chunk passes its count straight through.
    fn next_cardinality(&mut self) -> Result<Option<(Prefix48, u64)>> {
        self.ensure_started()?;
        match self.inner.peek_prefix()? {
            Some(p) if p <= self.hi => self.inner.next_cardinality(),
            _ => Ok(None),
        }
    }
}

/// Ordered concatenation of two prefix-disjoint streams.
///
/// # The precondition, and why it is worth having
///
/// **Every prefix of `l` must be strictly less than every prefix of `r`.** Given
/// that, a union needs no merge at all: drain the left, then the right. No
/// per-prefix `peek` on both sides, no comparison, no kernel call — and
/// `cardinality` is a plain sum, so a side that can count itself arithmetically
/// ( a `RangeStream` ) contributes in `O(1)` instead of being walked.
///
/// That is not a micro-optimization. `Or`'s cardinality loop peeks both sides
/// once per prefix, so `Or( small_set, huge_disjoint_range )` costs one step per
/// chunk **of the range** and does not finish. As `Concat` it is instant.
///
/// # It is selected, never assumed
///
/// Violating the precondition yields chunks out of order, which every operator
/// above silently mis-merges. So this is not something callers are asked to get
/// right: [`crate::stream::Expr::open`] builds it only where the operands are
/// *shown* prefix-disjoint — from the segmentation it just computed, or from a
/// `prefix_span` test over the union's parts — and the `debug_assert` below
/// fails loudly if it is ever constructed by hand over overlapping operands.
///
/// Both tests are on **prefixes**. `{0, 2}` and `{3, 5}` are disjoint and
/// ordered as *sets* and still share chunk 0, so an ordinal-level test would
/// admit them and the concatenation would emit that chunk twice.
pub struct Concat<L, R> {
    l: L,
    r: R,
    l_done: bool,
    #[cfg(debug_assertions)]
    last: Option<Prefix48>,
}

impl<L: ChunkStream, R: ChunkStream> Concat<L, R> {
    pub fn new(l: L, r: R) -> Self {
        Concat {
            l,
            r,
            l_done: false,
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
                "Concat operands are not prefix-disjoint and ordered: {:?} then {p}",
                self.last
            );
            self.last = Some(p);
        }
        let _ = p;
    }
}

impl<L: ChunkStream, R: ChunkStream> ChunkStream for Concat<L, R> {
    fn next_chunk(&mut self) -> Result<Option<Chunk>> {
        if !self.l_done {
            if let Some(c) = self.l.next_chunk()? {
                self.check(c.0);
                return Ok(Some(c));
            }
            self.l_done = true;
        }
        let out = self.r.next_chunk()?;
        if let Some((p, _)) = &out {
            self.check(*p);
        }
        Ok(out)
    }

    fn seek(&mut self, prefix: Prefix48) -> Result<()> {
        if !self.l_done {
            self.l.seek(prefix)?;
        }
        // Seeks are monotonic, so advancing the right side early cannot rewind
        // it, and every prefix it holds is above the left's anyway.
        self.r.seek(prefix)
    }

    fn peek_prefix(&mut self) -> Result<Option<Prefix48>> {
        if !self.l_done {
            if let Some(p) = self.l.peek_prefix()? {
                return Ok(Some(p));
            }
        }
        self.r.peek_prefix()
    }

    fn cardinality_hint(&self) -> (u64, Option<u64>) {
        let (la, lb) = self.l.cardinality_hint();
        let (ra, rb) = self.r.cardinality_hint();
        (
            la.saturating_add(ra),
            lb.zip(rb).map(|(x, y)| x.saturating_add(y)),
        )
    }

    fn stats(&self) -> StreamStats {
        let (a, b) = (self.l.stats(), self.r.stats());
        let span = match (a.prefix_span, b.prefix_span) {
            (Some((la, _)), Some((_, hb))) => Some((la, hb)),
            (x, y) => x.or(y),
        };
        StreamStats {
            chunks: a.chunks.zip(b.chunks).map(|(x, y)| x.saturating_add(y)),
            prefix_span: span,
            backing: a.backing.worse(b.backing),
        }
    }

    fn next_cardinality(&mut self) -> Result<Option<(Prefix48, u64)>> {
        if !self.l_done {
            if let Some(c) = self.l.next_cardinality()? {
                self.check(c.0);
                return Ok(Some(c));
            }
            self.l_done = true;
        }
        let out = self.r.next_cardinality()?;
        if let Some((p, _)) = &out {
            self.check(*p);
        }
        Ok(out)
    }

    /// A sum, not a merge. This is the whole point of the operator.
    fn cardinality_dyn(&mut self) -> Result<u64> {
        let l = if self.l_done {
            0
        } else {
            self.l.cardinality_dyn()?
        };
        Ok(l.saturating_add(self.r.cardinality_dyn()?))
    }
}

/// Complement within a range: yields `[lo, hi) \ S`.
///
/// # Why this is an operator and not `AndNot<RangeStream, S>`
///
/// It was a type alias for exactly that, and the identity is still exact — but
/// the alias is asymptotically wrong for the case invariant I8 made possible.
///
/// `AndNot::cardinality_dyn` drives its loop from the **left** operand, one
/// prefix at a time. With `RangeStream` on the left that is a step per chunk
/// *of the range*, which is fine for a narrow range and catastrophic for the
/// whole universe: `x.not()` spans `2^48` prefixes, so counting it would take
/// ~10^14 iterations to return a number obtainable in one subtraction.
///
/// This operator keeps the same streaming shape ( the universe slice minus the
/// input, one chunk at a time ) but overrides cardinality with the identity
///
/// ```text
/// |[lo, hi) \ S|  ==  (hi - lo) - |S ∩ [lo, hi)|
/// ```
///
/// which walks **`S`'s** chunks rather than the range's. For a sparse set that
/// is O(1) work per stored chunk and independent of how wide the complement is.
///
/// The kernel is still the shared `and_not` — only the loop's driver changed.
/// The bitmap-complement specialization ( word-wise `!w` ) remains untaken; see
/// QG §4, and prefer specializing `and_not( run, bitmap )` since that pays here
/// *and* on every existing ANDNOT.
pub struct Not<S> {
    inner: S,
    lo: u64,
    /// Exclusive. Under I8 `u64::MAX` here names the whole universe.
    hi: u64,
    /// Next prefix to consider.
    cur: Prefix48,
    pending: Option<Chunk>,
}

impl<S: ChunkStream> Not<S> {
    /// Complement of `inner` within `[lo, hi)`. An inverted range yields nothing.
    pub fn new(inner: S, lo: u64, hi: u64) -> Self {
        Not {
            inner,
            lo,
            hi: hi.max(lo),
            cur: crate::split(lo).0,
            pending: None,
        }
    }

    /// Last prefix the range touches, or `None` when the range is empty.
    fn last_prefix(&self) -> Option<Prefix48> {
        (self.hi > self.lo).then(|| crate::split(self.hi - 1).0)
    }

    /// The range's slice of chunk `p`, as a container.
    fn universe_at(&self, p: Prefix48) -> Option<Container> {
        let base = crate::chunk_base(p);
        // The top chunk's exclusive end is 2^64, which no u64 holds. It is only
        // ever compared against `hi`, so saturating is exact rather than merely
        // safe — and under I8 `u64::MAX` is precisely the universe's end.
        let s = self.lo.max(base);
        let e = self.hi.min(base.saturating_add(crate::CHUNK_CARD as u64));
        (s < e).then(|| {
            Container::Run(crate::container::RunContainer::from_pairs(&[(
                (s - base) as u16,
                (e - 1 - base) as u16,
            )]))
        })
    }

    /// Values of `c` at prefix `p` that fall inside `[from, self.hi)`.
    ///
    /// Only the range's first and last chunk can be partial, so the whole-chunk
    /// case is answered from the cached `len()` without touching the payload.
    fn clipped_len(&self, p: Prefix48, c: &Container, from: u64) -> u64 {
        let base = crate::chunk_base(p);
        let end = base.saturating_add(crate::CHUNK_CARD as u64);
        if from <= base && self.hi >= end {
            return c.len() as u64;
        }
        c.iter()
            .map(|v| crate::join(p, v))
            .filter(|&o| o >= from && o < self.hi)
            .count() as u64
    }

    fn produce(&mut self) -> Result<Option<Chunk>> {
        let Some(last) = self.last_prefix() else {
            return Ok(None);
        };
        while self.cur <= last {
            let p = self.cur;
            self.cur += 1;
            let Some(universe) = self.universe_at(p) else {
                continue;
            };
            self.inner.seek(p)?;
            let c = if self.inner.peek_prefix()? == Some(p) {
                match self.inner.next_chunk()? {
                    Some((_, cb)) => kern::and_not(&universe, &cb),
                    None => Some(universe),
                }
            } else {
                Some(universe)
            };
            // Fully covered by the input: the complement is empty here, skip.
            if let Some(c) = c {
                return Ok(Some((p, c)));
            }
        }
        Ok(None)
    }

    fn fill(&mut self) -> Result<()> {
        if self.pending.is_none() {
            self.pending = self.produce()?;
        }
        Ok(())
    }
}

impl<S: ChunkStream> ChunkStream for Not<S> {
    fn next_chunk(&mut self) -> Result<Option<Chunk>> {
        self.fill()?;
        Ok(self.pending.take())
    }

    fn seek(&mut self, prefix: Prefix48) -> Result<()> {
        if self.pending.as_ref().is_some_and(|(p, _)| *p < prefix) {
            self.pending = None;
        }
        if self.pending.is_none() {
            self.cur = self.cur.max(prefix);
        }
        Ok(())
    }

    fn peek_prefix(&mut self) -> Result<Option<Prefix48>> {
        self.fill()?;
        Ok(self.pending.as_ref().map(|(p, _)| *p))
    }

    fn cardinality_hint(&self) -> (u64, Option<u64>) {
        (0, Some(self.hi - self.lo))
    }

    /// A complement is dense over its range, so it *yields* the range — but the
    /// chunks are synthesized, not fetched, which is why `cardinality` can be
    /// answered from the input instead.
    fn stats(&self) -> StreamStats {
        let Some(last) = self.last_prefix() else {
            return StreamStats::new(0, None, Backing::Computed);
        };
        let first = self.cur.max(crate::split(self.lo).0);
        if first > last {
            return StreamStats::new(0, None, Backing::Computed);
        }
        StreamStats::new(
            last - first + 1,
            Some((first, last)),
            self.inner.stats().backing.worse(Backing::Computed),
        )
    }

    /// `(hi - lo) - |S ∩ [lo, hi)|`, walking `S` rather than the range.
    ///
    /// This is the whole reason `Not` is an operator; see the type's docs.
    fn cardinality_dyn(&mut self) -> Result<u64> {
        let mut n = self.pending.take().map_or(0, |(_, c)| c.len() as u64);
        let Some(last) = self.last_prefix() else {
            return Ok(n);
        };
        if self.cur > last {
            return Ok(n);
        }

        // Everything still ahead of the cursor, counted as a width rather than
        // chunk by chunk — this is the step that makes an unbounded complement
        // countable at all.
        let from = self.lo.max(crate::chunk_base(self.cur));
        if from >= self.hi {
            return Ok(n);
        }
        let remaining = self.hi - from;

        let mut inside = 0u64;
        self.inner.seek(self.cur)?;
        while let Some(p) = self.inner.peek_prefix()? {
            if p > last {
                break;
            }
            // `clipped_len` only needs the payload when the window cuts *into*
            // this chunk, and that is decidable from the prefix alone — so every
            // interior chunk, which is all but at most two, is a count.
            let base = crate::chunk_base(p);
            let end = base.saturating_add(crate::CHUNK_CARD as u64);
            if from <= base && self.hi >= end {
                match self.inner.next_cardinality()? {
                    Some((_, k)) => inside += k,
                    None => break,
                }
            } else {
                match self.inner.next_chunk()? {
                    Some((cp, c)) => inside += self.clipped_len(cp, &c, from),
                    None => break,
                }
            }
        }
        // `inside` counts only ordinals within `[from, hi)`, so it can never
        // exceed `remaining`; the subtraction cannot wrap.
        debug_assert!(inside <= remaining);
        n += remaining - inside;
        Ok(n)
    }
}

/// Complement of `s` within `[lo, hi)`.
pub fn not_in_range<S: ChunkStream>(s: S, lo: u64, hi: u64) -> Not<S> {
    Not::new(s, lo, hi)
}

pub struct AndNot<L, R> {
    l: L,
    r: R,
    pending: Option<Chunk>,
}

impl<L: ChunkStream, R: ChunkStream> AndNot<L, R> {
    pub fn new(l: L, r: R) -> Self {
        AndNot {
            l,
            r,
            pending: None,
        }
    }

    fn produce(&mut self) -> Result<Option<Chunk>> {
        loop {
            let Some(pa) = self.l.peek_prefix()? else {
                return Ok(None);
            };
            self.r.seek(pa)?;
            if self.r.peek_prefix()? == Some(pa) {
                let (Some((_, ca)), Some((_, cb))) = (self.l.next_chunk()?, self.r.next_chunk()?)
                else {
                    return Ok(None);
                };
                if let Some(c) = kern::and_not(&ca, &cb) {
                    return Ok(Some((pa, c)));
                }
                // Fully subtracted: skip.
            } else {
                // Left-only prefix: pass through, no kernel call.
                return self.l.next_chunk();
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

impl<L: ChunkStream, R: ChunkStream> ChunkStream for AndNot<L, R> {
    fn next_chunk(&mut self) -> Result<Option<Chunk>> {
        self.fill()?;
        Ok(self.pending.take())
    }

    fn seek(&mut self, prefix: Prefix48) -> Result<()> {
        if self.pending.as_ref().is_some_and(|(p, _)| *p < prefix) {
            self.pending = None;
        }
        if self.pending.is_none() {
            self.l.seek(prefix)?;
        }
        Ok(())
    }

    fn peek_prefix(&mut self) -> Result<Option<Prefix48>> {
        self.fill()?;
        Ok(self.pending.as_ref().map(|(p, _)| *p))
    }

    fn cardinality_hint(&self) -> (u64, Option<u64>) {
        (0, self.l.cardinality_hint().1)
    }

    /// `a \\ b` is bounded by `a` in both count and span, and the right side is
    /// only ever *sought* — so its backing does not enter the cost the way the
    /// left's does.
    fn stats(&self) -> StreamStats {
        self.l.stats()
    }

    fn cardinality_dyn(&mut self) -> Result<u64> {
        let mut n = self.pending.take().map_or(0, |(_, c)| c.len() as u64);
        while let Some(pa) = self.l.peek_prefix()? {
            self.r.seek(pa)?;
            if self.r.peek_prefix()? == Some(pa) {
                let (Some((_, ca)), Some((_, cb))) = (self.l.next_chunk()?, self.r.next_chunk()?)
                else {
                    break;
                };
                n += kern::andnot_cardinality(&ca, &cb) as u64;
            } else {
                // `r` has nothing here, so this chunk of `l` passes through
                // unchanged and only its count is wanted.
                //
                // Reading that count via `next_chunk` measured as most of the
                // 26x this operator's disjointness rewrite was credited with:
                // 2.09 ms -> 0.67 ms over 100 000 chunks, so the rewrite is
                // worth ~8.6x rather than 26x. Not a payload copy — containers
                // are frozen, so the clone is a refcount bump — but two atomics
                // and an enum move per chunk is 14 ns that buys nothing.
                match self.l.next_cardinality()? {
                    Some((_, k)) => n += k,
                    None => break,
                }
            }
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::stream::{ChunkStreamExt, SetStream};
    use crate::OrdSet;

    fn stream(vals: &[u64]) -> SetStream {
        SetStream::new(Arc::new(OrdSet::from_iter_unsorted(vals.iter().copied())))
    }

    fn vals(s: impl ChunkStream) -> Vec<u64> {
        s.ordinals().collect::<Result<Vec<_>>>().unwrap()
    }

    #[test]
    fn operators_match_eager_results() {
        let a: Vec<u64> = (0..3000u64).map(|i| i * 37).collect();
        let b: Vec<u64> = (0..3000u64).map(|i| i * 53).collect();
        let (ea, eb) = (
            OrdSet::from_iter_unsorted(a.iter().copied()),
            OrdSet::from_iter_unsorted(b.iter().copied()),
        );

        assert_eq!(
            vals(stream(&a).and(stream(&b))),
            ea.and(&eb).iter().collect::<Vec<_>>()
        );
        assert_eq!(
            vals(stream(&a).or(stream(&b))),
            ea.or(&eb).iter().collect::<Vec<_>>()
        );
        assert_eq!(
            vals(stream(&a).xor(stream(&b))),
            ea.xor(&eb).iter().collect::<Vec<_>>()
        );
        assert_eq!(
            vals(stream(&a).and_not(stream(&b))),
            ea.and_not(&eb).iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn cardinality_overrides_agree_with_materialization() {
        let a: Vec<u64> = (0..2000u64).map(|i| i * 11).collect();
        let b: Vec<u64> = (0..2000u64).map(|i| i * 13).collect();

        let cases: Vec<(u64, usize)> = vec![
            (
                stream(&a).and(stream(&b)).cardinality().unwrap(),
                vals(stream(&a).and(stream(&b))).len(),
            ),
            (
                stream(&a).or(stream(&b)).cardinality().unwrap(),
                vals(stream(&a).or(stream(&b))).len(),
            ),
            (
                stream(&a).xor(stream(&b)).cardinality().unwrap(),
                vals(stream(&a).xor(stream(&b))).len(),
            ),
            (
                stream(&a).and_not(stream(&b)).cardinality().unwrap(),
                vals(stream(&a).and_not(stream(&b))).len(),
            ),
        ];
        for (counted, materialized) in cases {
            assert_eq!(counted, materialized as u64);
        }
    }

    /// The exact shape proptest shrank to when `peek_prefix` was assumed exact.
    #[test]
    fn cancelling_subexpression_does_not_break_its_parent() {
        let a: Vec<u64> = (0..500u64).map(|i| i * 3).collect();
        let c: Vec<u64> = (0..500u64).map(|i| i * 7).collect();

        // Xor(c, c) is entirely empty; AndNot must handle a right side that
        // peeks a prefix but then yields nothing.
        let got = vals(stream(&a).and_not(stream(&c).xor(stream(&c))));
        assert_eq!(got, a);

        assert_eq!(
            stream(&a)
                .and_not(stream(&c).xor(stream(&c)))
                .cardinality()
                .unwrap(),
            a.len() as u64
        );
        // And the same on the AND path.
        assert_eq!(
            vals(stream(&a).and(stream(&c).xor(stream(&c)))),
            Vec::<u64>::new()
        );
        assert!(stream(&a)
            .and(stream(&c).xor(stream(&c)))
            .is_empty()
            .unwrap());
    }

    #[test]
    fn nested_expression_composes() {
        let a: Vec<u64> = (0..500u64).map(|i| i * 3).collect();
        let b: Vec<u64> = (0..500u64).map(|i| i * 5).collect();
        let c: Vec<u64> = (0..500u64).map(|i| i * 7).collect();

        let got = vals(stream(&a).and(stream(&b).or(stream(&c))));
        let (ea, eb, ec) = (
            OrdSet::from_iter_unsorted(a.iter().copied()),
            OrdSet::from_iter_unsorted(b.iter().copied()),
            OrdSet::from_iter_unsorted(c.iter().copied()),
        );
        assert_eq!(got, ea.and(&eb.or(&ec)).iter().collect::<Vec<_>>());
        assert_eq!(
            stream(&a)
                .and(stream(&b).or(stream(&c)))
                .cardinality()
                .unwrap(),
            got.len() as u64
        );
    }

    #[test]
    fn and_skips_the_dense_operand() {
        let dense: Vec<u64> = (0..2000u64).map(|i| i << 16).collect();
        let sparse: Vec<u64> = vec![5, 1 << 30, 3 << 30];
        assert_eq!(vals(stream(&sparse).and(stream(&dense))), Vec::<u64>::new());

        let overlapping: Vec<u64> = vec![0, 1 << 16, 5 << 16];
        assert_eq!(vals(stream(&overlapping).and(stream(&dense))), overlapping);
    }

    #[test]
    fn disjoint_prefixes_pass_through_untouched() {
        let a: Vec<u64> = vec![1, 2];
        let b: Vec<u64> = vec![1 << 40, (1 << 40) + 1];
        assert_eq!(vals(stream(&a).and(stream(&b))), Vec::<u64>::new());
        assert_eq!(
            vals(stream(&a).or(stream(&b))),
            vec![1, 2, 1 << 40, (1 << 40) + 1]
        );
        assert_eq!(vals(stream(&a).and_not(stream(&b))), vec![1, 2]);
    }

    #[test]
    fn xor_of_identical_streams_is_empty() {
        let a: Vec<u64> = (0..1000u64).collect();
        assert_eq!(vals(stream(&a).xor(stream(&a))), Vec::<u64>::new());
        assert_eq!(stream(&a).xor(stream(&a)).cardinality().unwrap(), 0);
        assert!(stream(&a).xor(stream(&a)).is_empty().unwrap());
        assert_eq!(stream(&a).xor(stream(&a)).peek_prefix().unwrap(), None);
    }

    #[test]
    fn seek_discards_a_stale_buffered_chunk() {
        let a: Vec<u64> = (0..10u64).map(|i| i << 16).collect();
        let b: Vec<u64> = (0..10u64).map(|i| i << 16).collect();
        let mut s = stream(&a).and(stream(&b));
        assert_eq!(s.peek_prefix().unwrap(), Some(0)); // buffers chunk 0
        s.seek(5).unwrap();
        assert_eq!(
            s.peek_prefix().unwrap(),
            Some(5),
            "stale buffer must be dropped"
        );
    }

    #[test]
    fn contains_min_max_on_a_lazy_expression() {
        let a: Vec<u64> = (0..1000u64).map(|i| i * 4).collect();
        let b: Vec<u64> = (0..1000u64).map(|i| i * 6).collect();
        assert!(stream(&a).and(stream(&b)).contains(24).unwrap());
        assert!(!stream(&a).and(stream(&b)).contains(4).unwrap());
        assert_eq!(stream(&a).and(stream(&b)).min().unwrap(), Some(0));
        let expected_max = vals(stream(&a).and(stream(&b))).into_iter().max();
        assert_eq!(stream(&a).and(stream(&b)).max().unwrap(), expected_max);
    }

    #[test]
    fn streams_are_send_and_static() {
        fn assert_send<T: Send + 'static>(_: T) {}
        assert_send(stream(&[1, 2, 3]).and(stream(&[2, 3, 4])));
    }
}

#[cfg(test)]
pub(crate) mod counting_tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    use super::*;
    use crate::stream::{ChunkStreamExt, SetStream};
    use crate::OrdSet;

    /// A leaf that records which question it was asked.
    ///
    /// # Why this and not an allocation count
    ///
    /// The obvious instrument for "counting must not materialize" is
    /// `tests/allocation.rs`, and it **cannot see this**. Containers are frozen
    /// at construction ( [`Container::freeze`] ), so `Container::clone` is a
    /// refcount bump and allocates nothing at all — an allocation-counting test
    /// of this property passes whether the override exists or not, which is
    /// worse than having no test.
    ///
    /// What `next_chunk` actually costs on this path is two atomics and moving
    /// an enum, which no counter in the crate can observe. So the property is
    /// pinned **behaviourally** instead: the operator must ask for a *count*,
    /// not for a chunk. That is exact, deterministic, and fails the moment an
    /// override or a call site is lost.
    pub(crate) struct Spy {
        inner: SetStream,
        chunks: Arc<AtomicU64>,
        counts: Arc<AtomicU64>,
    }

    impl Spy {
        pub(crate) fn new(
            set: Arc<OrdSet>,
            chunks: &Arc<AtomicU64>,
            counts: &Arc<AtomicU64>,
        ) -> Spy {
            Spy {
                inner: SetStream::new(set),
                chunks: chunks.clone(),
                counts: counts.clone(),
            }
        }
    }

    impl ChunkStream for Spy {
        fn next_chunk(&mut self) -> Result<Option<Chunk>> {
            self.chunks.fetch_add(1, Ordering::Relaxed);
            self.inner.next_chunk()
        }
        fn next_cardinality(&mut self) -> Result<Option<(Prefix48, u64)>> {
            self.counts.fetch_add(1, Ordering::Relaxed);
            self.inner.next_cardinality()
        }
        fn seek(&mut self, p: Prefix48) -> Result<()> {
            self.inner.seek(p)
        }
        fn peek_prefix(&mut self) -> Result<Option<Prefix48>> {
            self.inner.peek_prefix()
        }
        fn cardinality_hint(&self) -> (u64, Option<u64>) {
            self.inner.cardinality_hint()
        }
        fn stats(&self) -> StreamStats {
            self.inner.stats()
        }
        fn cardinality_dyn(&mut self) -> Result<u64> {
            self.inner.cardinality_dyn()
        }
    }

    pub(crate) fn spread(base: u64, chunks: u64) -> Arc<OrdSet> {
        let vals: Vec<u64> = (0..chunks)
            .flat_map(|c| (0..50u64).map(move |i| ((base + c) << 16) | (i * 3)))
            .collect();
        Arc::new(OrdSet::from_sorted_slice(&vals))
    }

    /// A chunk that passes through unchanged must be *counted*, not fetched.
    ///
    /// The operands are span-disjoint, so every chunk takes a pass-through arm:
    /// `Or` / `Xor`'s single-sided merge and `AndNot`'s skip branch. None of
    /// them needs a payload, and reading one via `next_chunk` measured as most
    /// of the `AndNot(disjoint)` rewrite's supposed value.
    #[test]
    fn counting_a_pass_through_chunk_asks_for_a_count_not_a_chunk() {
        const N: u64 = 500;
        let (lo, hi) = (spread(0, N), spread(10_000, N));
        let expect_or = 2 * N * 50;

        for (name, expect) in [("or", expect_or), ("xor", expect_or), ("andnot", N * 50)] {
            let (chunks, counts) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
            let l = Spy::new(lo.clone(), &chunks, &counts);
            let r = Spy::new(hi.clone(), &chunks, &counts);
            let n = match name {
                "or" => l.or(r).cardinality_dyn(),
                "xor" => l.xor(r).cardinality_dyn(),
                _ => l.and_not(r).cardinality_dyn(),
            }
            .unwrap();

            assert_eq!(n, expect, "{name}: wrong cardinality");
            assert_eq!(
                chunks.load(Ordering::Relaxed),
                0,
                "{name} fetched payloads for chunks it only had to count — an \
                 override of `next_cardinality`, or a call site using it, was lost"
            );
            assert!(
                counts.load(Ordering::Relaxed) > 0,
                "{name} counted nothing, so this test is watching the wrong path"
            );
        }
    }

    /// `next_cardinality` is a parallel implementation of `next_chunk`, so the
    /// only thing keeping them in agreement is a test that runs both.
    #[test]
    fn next_cardinality_agrees_with_next_chunk() {
        /// Builds a fresh stream, so the two walks start from the same place.
        type Build = Box<dyn Fn() -> Box<dyn ChunkStream>>;
        let cases: Vec<(&str, Build)> = vec![
            ("set", Box::new(|| Box::new(SetStream::new(spread(0, 40))))),
            (
                "range",
                Box::new(|| Box::new(crate::stream::RangeStream::new(70_000, 500_000))),
            ),
            (
                "range/aligned",
                Box::new(|| Box::new(crate::stream::RangeStream::new(0, 3 << 16))),
            ),
            ("empty", Box::new(|| Box::new(crate::stream::EmptyStream))),
            (
                "concat",
                Box::new(|| {
                    Box::new(Concat::new(
                        SetStream::new(spread(0, 10)),
                        SetStream::new(spread(100, 10)),
                    ))
                }),
            ),
            (
                "restrict",
                Box::new(|| Box::new(Restrict::new(SetStream::new(spread(0, 40)), 5, 25))),
            ),
        ];
        for (name, build) in cases {
            let mut a = build();
            let mut b = build();
            loop {
                let x = a.next_chunk().unwrap().map(|(p, c)| (p, c.len() as u64));
                let y = b.next_cardinality().unwrap();
                assert_eq!(x, y, "{name}: the two walks disagree");
                if x.is_none() {
                    break;
                }
            }
        }
    }
}
