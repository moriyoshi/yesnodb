//! An in-memory set of `u64` ordinals: a sorted sequence of `(Prefix48, Container)`.

use crate::container::{Container, RunContainer};
use crate::ops::{self, SetOp};
use crate::{chunk_base, join, split, Prefix48, CHUNK_CARD};

/// Structure-of-arrays, mirroring the on-disk chunk directory.
///
/// The parallel `Vec`s are deliberate: `seek` and every merge-join binary-search
/// the prefix array, and keeping it dense means 8 entries per cache line with the
/// container payloads never touched during search.
#[derive(Clone, Debug, Default)]
pub struct OrdSet {
    prefixes: Vec<Prefix48>,
    containers: Vec<Container>,
    len: u64,
}

/// What a range looks like from a set's point of view — the answer a scan
/// planner needs before it decides how to read a row group.
///
/// # Why three values and not a count
///
/// The design calls [`OrdSet::range_summary`] and [`OrdSet::len_in_range`] the
/// pair that "carries the entire pushdown story", and the split is the reason:
/// a count tells a planner *how many* rows survive, but only these three tell it
/// **what to do**. `Empty` skips a row group without decompressing a page;
/// `Full` scans it with no selection vector at all, which is the case a count
/// alone cannot distinguish from a merely large `Partial`; `Partial` is the only
/// one that has to pay for a `RowSelection`.
///
/// `#[non_exhaustive]` under policy R5. A fourth answer is conceivable — the
/// design's `range_summary` sketch predates the run-container mapping onto
/// `RowSelector`, and "full except for a short prefix" is the kind of thing that
/// path may want to name — and adding one must not be a breaking change.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RangeSummary {
    /// No ordinal of the set lies in the range.
    Empty,
    /// Every ordinal in the range is in the set.
    Full,
    /// Some, but not all.
    Partial,
}

impl OrdSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cardinality. O(1) — maintained incrementally.
    #[inline]
    pub fn len(&self) -> u64 {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Number of chunks. Useful for cost accounting and tests.
    #[inline]
    pub fn chunk_count(&self) -> usize {
        self.prefixes.len()
    }

    #[inline]
    fn find(&self, p: Prefix48) -> std::result::Result<usize, usize> {
        self.prefixes.binary_search(&p)
    }

    /// # Panics (debug only)
    ///
    /// `ordinal` must be `<= ORDINAL_MAX` (invariant I8). This is a documented
    /// precondition rather than a `Result` because the in-memory API is
    /// deliberately infallible; the fallible boundaries — [`crate::Db`],
    /// [`crate::WriteBatch::commit`] and the roaring import — return
    /// [`crate::CodecError::OrdinalOutOfRange`] instead.
    pub fn insert(&mut self, ordinal: u64) -> bool {
        debug_assert!(
            crate::is_valid_ordinal(ordinal),
            "ordinal {ordinal} exceeds ORDINAL_MAX (2^64-2); u64::MAX is reserved"
        );
        let (p, low) = split(ordinal);
        match self.find(p) {
            Ok(i) => {
                let changed = self.containers[i].insert(low);
                self.len += changed as u64;
                changed
            }
            Err(i) => {
                self.prefixes.insert(i, p);
                self.containers.insert(i, Container::from_sorted(&[low]));
                self.len += 1;
                true
            }
        }
    }

    /// See [`OrdSet::insert`] for the I8 precondition.
    pub fn remove(&mut self, ordinal: u64) -> bool {
        debug_assert!(
            crate::is_valid_ordinal(ordinal),
            "ordinal {ordinal} exceeds ORDINAL_MAX (2^64-2); u64::MAX is reserved"
        );
        let (p, low) = split(ordinal);
        let Ok(i) = self.find(p) else { return false };
        let changed = self.containers[i].remove(low);
        if changed {
            self.len -= 1;
            // An empty container is never stored.
            if self.containers[i].is_empty() {
                self.prefixes.remove(i);
                self.containers.remove(i);
            }
        }
        changed
    }

    #[inline]
    pub fn contains(&self, ordinal: u64) -> bool {
        let (p, low) = split(ordinal);
        matches!(self.find(p), Ok(i) if self.containers[i].contains(low))
    }

    pub fn min(&self) -> Option<u64> {
        let c = self.containers.first()?;
        Some(join(self.prefixes[0], c.min()?))
    }

    pub fn max(&self) -> Option<u64> {
        let i = self.containers.len().checked_sub(1)?;
        Some(join(self.prefixes[i], self.containers[i].max()?))
    }

    /// Number of ordinals strictly less than `ordinal`.
    pub fn rank(&self, ordinal: u64) -> u64 {
        let (p, low) = split(ordinal);
        let mut n = 0u64;
        for (i, &pref) in self.prefixes.iter().enumerate() {
            if pref < p {
                n += self.containers[i].len() as u64;
            } else if pref == p {
                n += self.containers[i].rank(low) as u64;
                break;
            } else {
                break;
            }
        }
        n
    }

    /// Ordinals in the half-open range `[lo, hi)`, **without materializing one**.
    ///
    /// **Half-open**, and deliberately not the other convention: yesno spells
    /// ranges both ways — `Db::insert_range` is inclusive `[lo, hi]` — and this
    /// one is the shape a Parquet row group's `[start, start + count)` already
    /// has, which is what it exists for.
    ///
    /// `O( chunks the range touches )`, with **payload access only at the two
    /// chunks the range partially covers**: a wholly covered chunk answers from
    /// its cached cardinality. That is the property that makes the pushdown
    /// affordable — a 1 M-row row group spans about sixteen chunks, of which at
    /// most two are ever probed.
    pub fn len_in_range(&self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            return 0;
        }
        let (p_lo, _) = split(lo);
        // `hi` is exclusive, so the last chunk touched is the one holding
        // `hi - 1`. Taking `split(hi)` instead would include a chunk the range
        // stops exactly at the start of.
        let (p_hi, _) = split(hi - 1);
        let start = self.prefixes.partition_point(|&p| p < p_lo);
        let mut n = 0u64;
        for i in start..self.prefixes.len() {
            let p = self.prefixes[i];
            if p > p_hi {
                break;
            }
            let Some((l, h)) = crate::chunk_window(p, lo, hi) else {
                continue;
            };
            n += self.containers[i].count_in_range(l, h) as u64;
        }
        n
    }

    /// Whether `[lo, hi)` is wholly outside this set, wholly inside it, or
    /// neither.
    ///
    /// This is the decision a scan planner makes per row group: `Empty` skips it
    /// without reading a byte, `Full` scans it with no selection vector at all,
    /// and only `Partial` needs one built. Answered from container metadata and
    /// popcounts, never from ordinals.
    ///
    /// # `Empty` is decided before anything is counted
    ///
    /// This used to be `match self.len_in_range(lo, hi)` and nothing else, so
    /// "is anything in this range" cost exactly "how many are in it" — the rule
    /// [`ops::card`](crate::ops::card) states and proves twice for its own
    /// predicates, **a predicate must never cost more than the count it is
    /// weaker than**, broken in the one place a planner asks the weak question
    /// most often. [`Container::is_range_empty`] answers it per kind with a
    /// first-hit exit, and `Full` still needs the count because it is a claim
    /// about every ordinal in the range.
    ///
    /// The saving is not a constant factor: the emptiness walk stops at the
    /// first chunk that meets the range, where the count visits every chunk the
    /// range touches.
    ///
    /// # Two orderings were measured, and the naive one was a regression
    ///
    /// Putting the probe in front of the count and nothing else made the two
    /// answers that still need a count pay for both. Measured against
    /// `len_in_range` over the same range ( release, `aarch64-unknown-linux-gnu`,
    /// 200 000 iterations ): a whole-chunk `Partial` went from 3.1 ns to 6.6 ns
    /// ( 2.11x ) and a narrow `Partial` over a 512-interval run container from
    /// 8.0 ns to 12.8 ns ( 1.62x ). Making the weak question cheap by making the
    /// strong one dearer is the same trade this rule exists to forbid.
    ///
    /// The `self.len < width` test below is what repairs it, and it is `O(1)`:
    /// `Full` claims every ordinal of the range, so a set that holds fewer
    /// ordinals *in total* than the range is wide cannot be `Full`, and the
    /// count is then not needed at all. That is the shape a scan planner asks
    /// about — a wide row group against a sparse pushed-down set — so the
    /// common path counts nothing:
    ///
    /// ```text
    ///   range over a set of ...            len_in_range   range_summary   ratio
    ///   256 chunks, hit only in the last       525.4 ns         10.0 ns    0.02
    ///   8 KiB bitmap, empty 25 000 window      138.3 ns         28.7 ns    0.21
    ///   2 000-value array, empty window         18.2 ns         13.2 ns    0.72
    ///   512-interval run, empty window           7.5 ns          4.9 ns    0.65
    ///   8 KiB bitmap, whole chunk                3.1 ns          3.2 ns    1.01
    ///   512-interval run, narrow occupied        7.8 ns         12.6 ns    1.61
    /// ```
    ///
    /// **The last row is the residual and it is not a measurement error.**
    /// It happens when `Full` is still live ( `len >= width` ) *and* the count
    /// on that particular window is already cheap, which is a property of where
    /// the window sits rather than of how big the container is.
    /// `RunContainer::rank` is a **linear walk that breaks at the first interval
    /// starting past `v`**, so a window near the *front* of a 512-interval run
    /// costs it about two iterations while the probe's binary search still pays
    /// its nine — the one shape where an asymptotically better search loses. On
    /// an array, `rank` is a `partition_point`, so the count is two searches and
    /// the probe makes three. Either way it is bounded at ~1.5x of a count that
    /// is already single-digit nanoseconds, and it is the price of the 0.02x row
    /// above and of [`OrdSet::int_is_zero`], whose spans are narrow and whose
    /// sets are not.
    ///
    /// Do not repair it by counting first when the range touches one chunk:
    /// `rank` on a bitmap scans the whole prefix below `hi`, so exactly that
    /// case is the 138 ns row, and it is the case `int_is_zero` asks.
    pub fn range_summary(&self, lo: u64, hi: u64) -> RangeSummary {
        let width = hi.saturating_sub(lo);
        if width == 0 {
            // An empty range is `Empty`, not `Full`. Vacuously it is both;
            // the caller's question is "may I skip this?", and the answer is
            // yes.
            return RangeSummary::Empty;
        }
        if self.range_is_empty(lo, hi) {
            return RangeSummary::Empty;
        }
        // `Full` is refuted in O(1) before the count is even considered: it
        // claims *every* ordinal of the range, so a set holding fewer ordinals
        // in total than the range is wide cannot make it. That test is what
        // keeps this cheaper than the count rather than more expensive — see
        // the note above — because it is the case a planner asks about most:
        // a row group is wide and the sets pushed into it are sparse.
        if self.len < width {
            return RangeSummary::Partial;
        }
        // `Full` is still possible, and only the count separates it from
        // `Partial`.
        if self.len_in_range(lo, hi) == width {
            RangeSummary::Full
        } else {
            RangeSummary::Partial
        }
    }

    /// Does `[lo, hi)` hold no ordinal of this set?
    ///
    /// The short-circuiting counterpart of [`OrdSet::len_in_range`]: same chunk
    /// walk, same `partition_point` entry, but it returns at the first chunk
    /// that meets the range instead of accumulating over all of them, and each
    /// chunk answers with [`Container::is_range_empty`] rather than two `rank`
    /// probes.
    ///
    /// Private on purpose. [`OrdSet::range_summary`] already names this
    /// answer — `RangeSummary::Empty` — and a second public spelling of one
    /// question is API surface that R1 / R6 / R7 turn into a promise for
    /// nothing.
    fn range_is_empty(&self, lo: u64, hi: u64) -> bool {
        if hi <= lo {
            return true;
        }
        let (p_lo, _) = split(lo);
        // Same reasoning as `len_in_range`: `hi` is exclusive, so the last chunk
        // touched holds `hi - 1`.
        let (p_hi, _) = split(hi - 1);
        let start = self.prefixes.partition_point(|&p| p < p_lo);
        for i in start..self.prefixes.len() {
            let p = self.prefixes[i];
            if p > p_hi {
                break;
            }
            let Some((l, h)) = crate::chunk_window(p, lo, hi) else {
                continue;
            };
            if !self.containers[i].is_range_empty(l, h) {
                return false;
            }
        }
        true
    }

    /// The `n`-th smallest ordinal, zero-indexed.
    pub fn select(&self, mut n: u64) -> Option<u64> {
        for (i, &pref) in self.prefixes.iter().enumerate() {
            let c = self.containers[i].len() as u64;
            if n < c {
                return Some(join(pref, self.containers[i].select(n as u32)?));
            }
            n -= c;
        }
        None
    }

    pub fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        self.prefixes
            .iter()
            .zip(&self.containers)
            .flat_map(|(&p, c)| {
                let base = chunk_base(p);
                c.iter().map(move |v| base | v as u64)
            })
    }

    /// Chunk-level view, for the merge-join and for tests.
    pub fn chunks(&self) -> impl Iterator<Item = (Prefix48, &Container)> {
        self.prefixes.iter().copied().zip(self.containers.iter())
    }

    /// Chunk at a positional index, for cursor-style streaming.
    #[inline]
    pub fn chunk_at(&self, i: usize) -> Option<(Prefix48, &Container)> {
        Some((*self.prefixes.get(i)?, &self.containers[i]))
    }

    /// Prefix at a positional index, without touching the payload.
    #[inline]
    pub fn prefix_at(&self, i: usize) -> Option<Prefix48> {
        self.prefixes.get(i).copied()
    }

    /// Number of prefixes in `[lo, hi)` that are `< prefix`. Backs galloping seek.
    #[inline]
    pub fn partition_point_in(&self, lo: usize, hi: usize, prefix: Prefix48) -> usize {
        let hi = hi.min(self.prefixes.len());
        let lo = lo.min(hi);
        self.prefixes[lo..hi].partition_point(|&p| p < prefix)
    }

    /// Assemble directly from ascending, non-empty chunks.
    ///
    /// Used by `ChunkStreamExt::collect_set`, which already produces them in
    /// order — this avoids re-sorting or re-merging what the stream guaranteed.
    /// The prefix and container lists, which are parallel and prefix-ordered.
    pub(crate) fn parts(&self) -> (&[Prefix48], &[Container]) {
        (&self.prefixes, &self.containers)
    }

    pub fn from_chunks(chunks: Vec<(Prefix48, Container)>) -> Self {
        let mut s = OrdSet::new();
        s.prefixes.reserve(chunks.len());
        s.containers.reserve(chunks.len());
        for (p, c) in chunks {
            debug_assert!(!c.is_empty(), "an empty container must never be stored");
            debug_assert!(s.prefixes.last().is_none_or(|&last| last < p));
            s.len += c.len() as u64;
            s.prefixes.push(p);
            s.containers.push(c);
        }
        s
    }

    /// A lazy stream over a shared snapshot of this set.
    pub fn stream(self: &std::sync::Arc<Self>) -> crate::stream::SetStream {
        crate::stream::SetStream::new(self.clone())
    }

    pub fn from_iter_unsorted(it: impl IntoIterator<Item = u64>) -> Self {
        let mut v: Vec<u64> = it.into_iter().collect();
        v.sort_unstable();
        v.dedup();
        Self::from_sorted_slice(&v)
    }

    /// Bulk build from sorted, unique ordinals.
    ///
    /// Builds each container directly in its final representation rather than
    /// inserting one value at a time through the promotion path.
    pub fn from_sorted_slice(vals: &[u64]) -> Self {
        debug_assert!(
            vals.last().is_none_or(|&v| crate::is_valid_ordinal(v)),
            "input contains an ordinal above ORDINAL_MAX (2^64-2); u64::MAX is reserved"
        );
        let mut s = Self::new();
        let mut i = 0usize;
        while i < vals.len() {
            let (p, _) = split(vals[i]);
            let mut j = i;
            let mut lows = Vec::new();
            while j < vals.len() {
                let (pj, low) = split(vals[j]);
                if pj != p {
                    break;
                }
                lows.push(low);
                j += 1;
            }
            let mut c = Container::from_sorted(&lows);
            // Bulk build produces immutable data; freeze so downstream set ops
            // pass chunks through by refcount rather than by copy.
            c.freeze();
            s.prefixes.push(p);
            s.containers.push(c);
            s.len += lows.len() as u64;
            i = j;
        }
        s
    }

    /// Re-select every container's encoding, then freeze.
    ///
    /// Storage optimization, done at commit/compaction time — never on the write
    /// path. Freezing here is what makes subsequent set-op pass-through free;
    /// see [`Self::freeze`].
    pub fn optimize(&mut self) {
        for c in &mut self.containers {
            c.optimize();
            c.freeze();
        }
    }

    /// Move every container into shared form so cloning is a refcount bump.
    ///
    /// A set built by repeated `insert` holds owned payloads, where cloning a
    /// container copies its bytes — which shows up as one allocation per chunk
    /// in every merge-join that passes a chunk through untouched. Freezing at a
    /// commit boundary removes that. Mutating afterwards copies once, per the
    /// usual copy-on-write contract.
    pub fn freeze(&mut self) {
        for c in &mut self.containers {
            c.freeze();
        }
    }

    fn push_chunk(&mut self, p: Prefix48, c: Container) {
        debug_assert!(self.prefixes.last().is_none_or(|&last| last < p));
        self.len += c.len() as u64;
        self.prefixes.push(p);
        self.containers.push(c);
    }

    /// Chunk-aligned merge-join. Chunks present on only one side pass through by
    /// clone, which is a refcount bump for shared payloads rather than a copy.
    ///
    /// # Galloping is per-operation, not universal
    ///
    /// A side may only be galloped where its chunks are being *skipped*:
    ///
    /// - **AND** discards both single-sided cases, so both sides gallop. That is
    ///   what makes intersecting a 3-chunk set with a 15 000-chunk one cost
    ///   O(log n) rather than a full scan.
    /// - **ANDNOT** keeps every left chunk, so only the right side gallops.
    /// - **OR / XOR** keep everything, so neither does — a union must visit every
    ///   prefix, and "optimizing" that would simply drop output.
    fn binary(op: SetOp, a: &OrdSet, b: &OrdSet) -> OrdSet {
        let mut out = OrdSet::new();
        let (mut i, mut j) = (0usize, 0usize);
        let gallop_left = matches!(op, SetOp::And);
        let gallop_right = matches!(op, SetOp::And | SetOp::AndNot);
        while i < a.prefixes.len() || j < b.prefixes.len() {
            let pa = a.prefixes.get(i).copied();
            let pb = b.prefixes.get(j).copied();
            match (pa, pb) {
                (Some(x), Some(y)) if x < y => {
                    if matches!(op, SetOp::Or | SetOp::Xor | SetOp::AndNot) {
                        out.push_chunk(x, a.containers[i].clone());
                        i += 1;
                    } else if gallop_left {
                        i = a.gallop(i, y);
                    } else {
                        i += 1;
                    }
                }
                (Some(x), Some(y)) if y < x => {
                    if matches!(op, SetOp::Or | SetOp::Xor) {
                        out.push_chunk(y, b.containers[j].clone());
                        j += 1;
                    } else if gallop_right {
                        j = b.gallop(j, x);
                    } else {
                        j += 1;
                    }
                }
                (Some(x), Some(_)) => {
                    if let Some(c) = ops::apply(op, &a.containers[i], &b.containers[j]) {
                        out.push_chunk(x, c);
                    }
                    i += 1;
                    j += 1;
                }
                (Some(x), None) => {
                    if matches!(op, SetOp::Or | SetOp::Xor | SetOp::AndNot) {
                        out.push_chunk(x, a.containers[i].clone());
                    }
                    i += 1;
                }
                (None, Some(y)) => {
                    if matches!(op, SetOp::Or | SetOp::Xor) {
                        out.push_chunk(y, b.containers[j].clone());
                    }
                    j += 1;
                }
                (None, None) => break,
            }
        }
        out
    }

    pub fn and(&self, other: &OrdSet) -> OrdSet {
        Self::binary(SetOp::And, self, other)
    }
    /// Union of many sets in one pass, with no intermediate results.
    ///
    /// Folding `or` allocates a complete new set per input, and as the
    /// accumulator grows each copy approaches the size of the final answer — so
    /// a k-way union costs quadratically in k for an answer that does not. This
    /// visits each prefix once and ORs every contributor into one reusable
    /// accumulator. See `ops::nary`.
    pub fn union_all(sets: &[&OrdSet]) -> OrdSet {
        // Below three inputs the cursor machinery costs more than it saves —
        // measured 0.4x against a plain `or` at k=2.
        match sets {
            [] => return OrdSet::new(),
            [a] => return (*a).clone(),
            [a, b] => return a.or(b),
            _ => {}
        }
        let lists: Vec<(&[Prefix48], &[Container])> = sets.iter().map(|s| s.parts()).collect();
        OrdSet::from_chunks(crate::ops::nary::union_all(&lists))
    }

    pub fn or(&self, other: &OrdSet) -> OrdSet {
        Self::binary(SetOp::Or, self, other)
    }
    pub fn xor(&self, other: &OrdSet) -> OrdSet {
        Self::binary(SetOp::Xor, self, other)
    }
    pub fn and_not(&self, other: &OrdSet) -> OrdSet {
        Self::binary(SetOp::AndNot, self, other)
    }

    /// # There is no eager unbounded complement
    ///
    /// Deliberately no `OrdSet::not()`. An eager complement must materialize its
    /// whole answer, and over the full universe that is ~`2^48` chunks — so the
    /// method would take no arguments and abort for *every* input, including a
    /// nearly-full set whose complement is tiny, because the walk is `2^48`
    /// either way. It shipped briefly on 2026-08-26 and was removed; the doc
    /// called it "complete rather than practical", which was a rationalisation
    /// of an API that could not succeed.
    ///
    /// The lazy form has no such problem and is the one to reach for:
    /// [`crate::stream::ChunkStreamExt::not`] and `!expr` yield one chunk at a
    /// time, and their `cardinality()` is `O(chunks of the input)` — so
    /// `(!expr).cardinality()` answers in microseconds over the same universe
    /// this method could not enumerate.
    ///
    /// `not_in_range` itself is still bounded only by what you ask for: a range
    /// spanning `2^40` produces 16.7 million chunks in about a second. That cost
    /// is visible in the arguments, which is the difference.
    ///
    /// Complement of `self` within `[lo, hi)`.
    ///
    /// `[lo, hi)` is half-open, matching [`crate::stream::RangeStream`]. Under
    /// I8 that costs nothing: `u64::MAX` is not an ordinal, so `hi = u64::MAX`
    /// names the entire universe and the exclusive bound never needs the
    /// unrepresentable `2^64`. An inclusive `hi` would make the *empty* range
    /// unrepresentable instead.
    ///
    /// # Cost is the width of the range, not `len()`
    ///
    /// Every prefix in `[lo, hi)` that `self` does not fully cover yields a
    /// chunk, so this is O(range width / 65536) chunks of output regardless of
    /// how small `self` is. Complementing a sparse set over a wide range is
    /// inherently a large answer. For a wide range prefer the lazy
    /// [`crate::stream::ChunkStreamExt::not_in_range`], which holds one chunk at
    /// a time and can answer `cardinality()` without materializing any of it.
    pub fn not_in_range(&self, lo: u64, hi: u64) -> OrdSet {
        let hi = hi.max(lo);
        if hi == lo {
            return OrdSet::new();
        }
        let mut out = OrdSet::new();
        let mut i = 0usize;
        for p in split(lo).0..=split(hi - 1).0 {
            // Chunks are prefix-ordered, so one forward cursor suffices; no
            // search per prefix.
            while i < self.prefixes.len() && self.prefixes[i] < p {
                i += 1;
            }
            let base = chunk_base(p);
            // The top chunk's exclusive end is 2^64, which does not fit. It is
            // only ever compared against `hi`, itself a `u64`, so saturating is
            // exact here rather than merely safe.
            let s = lo.max(base);
            let e = hi.min(base.saturating_add(CHUNK_CARD as u64));
            if s >= e {
                continue;
            }
            let universe = Container::Run(RunContainer::from_pairs(&[(
                (s - base) as u16,
                (e - 1 - base) as u16,
            )]));
            let c = if i < self.prefixes.len() && self.prefixes[i] == p {
                ops::and_not(&universe, &self.containers[i])
            } else {
                Some(universe)
            };
            if let Some(c) = c {
                out.push_chunk(p, c);
            }
        }
        out
    }

    /// Advance `i` to the first index at or after it whose prefix is `>= target`.
    ///
    /// Galloping (exponential probe, then binary search), so a forward-skewed
    /// scan costs O(log delta) rather than O(n). Stepping linearly instead makes
    /// intersecting a 1-chunk set with a 15 000-chunk one cost 15 000 steps —
    /// measured at 13× slower than the seeking path.
    #[inline]
    fn gallop(&self, i: usize, target: Prefix48) -> usize {
        let n = self.prefixes.len();
        if i >= n || self.prefixes[i] >= target {
            return i;
        }
        let mut step = 1usize;
        while i + step < n && self.prefixes[i + step] < target {
            step *= 2;
        }
        let lo = i + step / 2;
        let hi = (i + step + 1).min(n);
        lo + self.prefixes[lo..hi].partition_point(|&p| p < target)
    }

    /// `|A ∩ B|` without materializing anything.
    ///
    /// Chunks present on only one side are skipped entirely — no payload is
    /// touched for them, and galloping skips runs of them at once.
    pub fn and_cardinality(&self, other: &OrdSet) -> u64 {
        let (mut i, mut j) = (0usize, 0usize);
        let mut n = 0u64;
        while i < self.prefixes.len() && j < other.prefixes.len() {
            let (x, y) = (self.prefixes[i], other.prefixes[j]);
            if x < y {
                i = self.gallop(i, y);
            } else if y < x {
                j = other.gallop(j, x);
            } else {
                n += ops::and_cardinality(&self.containers[i], &other.containers[j]) as u64;
                i += 1;
                j += 1;
            }
        }
        n
    }

    /// `|A ∪ B|` via the identity — no container is built.
    #[inline]
    pub fn or_cardinality(&self, other: &OrdSet) -> u64 {
        self.len + other.len - self.and_cardinality(other)
    }

    #[inline]
    pub fn xor_cardinality(&self, other: &OrdSet) -> u64 {
        self.len + other.len - 2 * self.and_cardinality(other)
    }

    #[inline]
    pub fn andnot_cardinality(&self, other: &OrdSet) -> u64 {
        self.len - self.and_cardinality(other)
    }

    pub fn is_disjoint(&self, other: &OrdSet) -> bool {
        let (mut i, mut j) = (0usize, 0usize);
        while i < self.prefixes.len() && j < other.prefixes.len() {
            let (x, y) = (self.prefixes[i], other.prefixes[j]);
            if x < y {
                i = self.gallop(i, y);
            } else if y < x {
                j = other.gallop(j, x);
            } else {
                if !ops::is_disjoint(&self.containers[i], &other.containers[j]) {
                    return false;
                }
                i += 1;
                j += 1;
            }
        }
        true
    }

    /// `other ⊆ self`.
    pub fn is_superset(&self, other: &OrdSet) -> bool {
        if other.len > self.len {
            return false;
        }
        let mut i = 0usize;
        for (p, c) in other.chunks() {
            while i < self.prefixes.len() && self.prefixes[i] < p {
                i += 1;
            }
            if i >= self.prefixes.len() || self.prefixes[i] != p {
                return false;
            }
            if !ops::contains_all(&self.containers[i], c) {
                return false;
            }
        }
        true
    }

    #[inline]
    pub fn is_subset(&self, other: &OrdSet) -> bool {
        other.is_superset(self)
    }
}

impl PartialEq for OrdSet {
    fn eq(&self, other: &Self) -> bool {
        // Compare by contents, not representation: the same set may legitimately
        // be encoded as an array on one side and a run on the other.
        self.len == other.len && self.prefixes == other.prefixes && self.iter().eq(other.iter())
    }
}
impl Eq for OrdSet {}

impl FromIterator<u64> for OrdSet {
    fn from_iter<I: IntoIterator<Item = u64>>(it: I) -> Self {
        Self::from_iter_unsorted(it)
    }
}

#[cfg(test)]
mod tests {

    /// All three verdicts, by construction rather than by hoping a generator
    /// finds them.
    ///
    /// `Full` is the one worth pinning deliberately. Over a sparse set a
    /// randomly drawn range is almost always `Partial`, so a property test alone
    /// can exercise the two cheap answers thousands of times and the expensive
    /// one never — and `Full` is precisely the verdict that saves a planner from
    /// building a selection vector.
    #[test]
    fn range_summary_names_all_three_answers() {
        // A contiguous stretch spanning three chunks, so there is an interior
        // chunk answered from its cached cardinality and two partial edges.
        let base = 5 * CHUNK_CARD as u64;
        let s =
            OrdSet::from_sorted_slice(&(base..base + 3 * CHUNK_CARD as u64).collect::<Vec<u64>>());

        assert_eq!(s.range_summary(0, base), RangeSummary::Empty, "below");
        assert_eq!(
            s.range_summary(base + 3 * CHUNK_CARD as u64, u64::MAX),
            RangeSummary::Empty,
            "above"
        );
        assert_eq!(
            s.range_summary(base, base + 3 * CHUNK_CARD as u64),
            RangeSummary::Full,
            "exactly the stretch"
        );
        assert_eq!(
            s.range_summary(base + 100, base + 200),
            RangeSummary::Full,
            "wholly inside one chunk"
        );
        assert_eq!(
            s.range_summary(base + CHUNK_CARD as u64 - 5, base + CHUNK_CARD as u64 + 5),
            RangeSummary::Full,
            "straddling a chunk boundary, both sides present"
        );
        assert_eq!(
            s.range_summary(base - 1, base + 10),
            RangeSummary::Partial,
            "one ordinal short at the bottom"
        );
        assert_eq!(
            s.range_summary(0, u64::MAX),
            RangeSummary::Partial,
            "universe"
        );
        // An empty range may be skipped, which is the caller's question.
        assert_eq!(
            s.range_summary(base, base),
            RangeSummary::Empty,
            "degenerate"
        );

        // And the counts underneath.
        assert_eq!(
            s.len_in_range(base, base + 3 * CHUNK_CARD as u64),
            3 * CHUNK_CARD as u64
        );
        assert_eq!(s.len_in_range(base + 100, base + 200), 100);
        assert_eq!(s.len_in_range(base - 50, base + 50), 50);
        assert_eq!(s.len_in_range(0, u64::MAX), 3 * CHUNK_CARD as u64);
        assert_eq!(s.len_in_range(10, 10), 0);
        assert_eq!(
            s.len_in_range(100, 10),
            0,
            "an inverted range is empty, not a panic"
        );
    }

    /// A gap makes an otherwise-full range `Partial`, and the count sees it.
    #[test]
    fn a_single_hole_is_visible_to_both() {
        let base = 3 * CHUNK_CARD as u64;
        let mut vals: Vec<u64> = (base..base + 2 * CHUNK_CARD as u64).collect();
        vals.retain(|&v| v != base + CHUNK_CARD as u64 + 7);
        let s = OrdSet::from_sorted_slice(&vals);
        let (lo, hi) = (base, base + 2 * CHUNK_CARD as u64);
        assert_eq!(s.range_summary(lo, hi), RangeSummary::Partial);
        assert_eq!(s.len_in_range(lo, hi), 2 * CHUNK_CARD as u64 - 1);
        // The chunk without the hole is still full on its own.
        assert_eq!(
            s.range_summary(base, base + CHUNK_CARD as u64),
            RangeSummary::Full
        );
    }
    use super::*;
    use std::collections::BTreeSet;

    fn set(vals: &[u64]) -> OrdSet {
        OrdSet::from_iter_unsorted(vals.iter().copied())
    }

    #[test]
    fn insert_remove_contains_len() {
        let mut s = OrdSet::new();
        assert!(s.insert(5));
        assert!(!s.insert(5));
        assert!(s.insert(1 << 40));
        assert_eq!(s.len(), 2);
        assert!(s.contains(5));
        assert!(s.contains(1 << 40));
        assert!(!s.contains(6));
        assert!(s.remove(5));
        assert!(!s.remove(5));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn emptying_a_chunk_drops_it() {
        let mut s = set(&[100, 200]);
        assert_eq!(s.chunk_count(), 1);
        s.remove(100);
        s.remove(200);
        assert_eq!(s.chunk_count(), 0, "empty containers are never stored");
        assert!(s.is_empty());
    }

    #[test]
    fn spans_multiple_chunks_and_iterates_ascending() {
        // `ORDINAL_MAX`, not `u64::MAX`: I8 reserves the top value, and the
        // largest *storable* ordinal is what this test is about.
        let vals: Vec<u64> = vec![0, 1, 65535, 65536, 65537, 1 << 20, crate::ORDINAL_MAX];
        let s = set(&vals);
        assert_eq!(s.iter().collect::<Vec<_>>(), vals);
        assert_eq!(s.len(), vals.len() as u64);
        assert_eq!(s.min(), Some(0));
        assert_eq!(s.max(), Some(crate::ORDINAL_MAX));
    }

    #[test]
    fn set_ops_match_btreeset_oracle() {
        let av: Vec<u64> = (0..2000u64).map(|i| i * 37).collect();
        let bv: Vec<u64> = (0..2000u64).map(|i| i * 53).collect();
        let (a, b) = (set(&av), set(&bv));
        let (sa, sb): (BTreeSet<u64>, BTreeSet<u64>) =
            (av.iter().copied().collect(), bv.iter().copied().collect());

        let got = |s: &OrdSet| s.iter().collect::<Vec<_>>();
        assert_eq!(
            got(&a.and(&b)),
            sa.intersection(&sb).copied().collect::<Vec<_>>()
        );
        assert_eq!(got(&a.or(&b)), sa.union(&sb).copied().collect::<Vec<_>>());
        assert_eq!(
            got(&a.xor(&b)),
            sa.symmetric_difference(&sb).copied().collect::<Vec<_>>()
        );
        assert_eq!(
            got(&a.and_not(&b)),
            sa.difference(&sb).copied().collect::<Vec<_>>()
        );
    }

    #[test]
    fn cardinality_identities_never_materialize_but_still_agree() {
        let a = set(&(0..5000u64).map(|i| i * 11).collect::<Vec<_>>());
        let b = set(&(0..5000u64).map(|i| i * 13).collect::<Vec<_>>());
        assert_eq!(a.and_cardinality(&b), a.and(&b).len());
        assert_eq!(a.or_cardinality(&b), a.or(&b).len());
        assert_eq!(a.xor_cardinality(&b), a.xor(&b).len());
        assert_eq!(a.andnot_cardinality(&b), a.and_not(&b).len());
    }

    #[test]
    fn disjoint_and_subset() {
        let a = set(&[1, 2, 3, 1 << 30]);
        let b = set(&[2, 3]);
        let c = set(&[99, 1 << 40]);
        assert!(a.is_superset(&b));
        assert!(b.is_subset(&a));
        assert!(!a.is_superset(&c));
        assert!(a.is_disjoint(&c));
        assert!(!a.is_disjoint(&b));
    }

    #[test]
    fn rank_and_select_are_inverse() {
        let vals: Vec<u64> = (0..1000u64).map(|i| i * 1234567).collect();
        let s = set(&vals);
        for (i, &v) in vals.iter().enumerate() {
            assert_eq!(s.select(i as u64), Some(v));
            assert_eq!(s.rank(v), i as u64);
        }
        assert_eq!(s.select(vals.len() as u64), None);
    }

    #[test]
    fn optimize_preserves_contents_across_encodings() {
        let vals: Vec<u64> = (0..20000u64).collect();
        let mut s = set(&vals);
        let before = s.len();
        s.optimize();
        assert_eq!(s.len(), before);
        assert_eq!(s.iter().collect::<Vec<_>>(), vals);
    }

    #[test]
    fn bulk_build_matches_incremental_insert() {
        let vals: Vec<u64> = (0..10000u64).map(|i| i * 7 + 3).collect();
        let bulk = OrdSet::from_sorted_slice(&vals);
        let mut incremental = OrdSet::new();
        for &v in &vals {
            incremental.insert(v);
        }
        assert_eq!(bulk.len(), incremental.len());
        assert_eq!(
            bulk.iter().collect::<Vec<_>>(),
            incremental.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn not_in_range_over_an_empty_or_inverted_range_is_empty() {
        let s = set(&[1, 2, 3]);
        assert_eq!(s.not_in_range(5, 5).len(), 0);
        // Inverted, not a panic and not a wraparound: `hi.max(lo)` clamps it to
        // empty, matching `RangeStream`.
        assert_eq!(s.not_in_range(9, 4).len(), 0);
    }

    #[test]
    fn not_in_range_of_the_empty_set_is_the_whole_range() {
        let got = OrdSet::new().not_in_range(10, 20);
        assert_eq!(got.iter().collect::<Vec<_>>(), (10..20).collect::<Vec<_>>());
    }

    #[test]
    fn not_in_range_spans_prefixes_the_set_never_touches() {
        // The set lives entirely in chunk 0; the range reaches into chunk 2. The
        // absent prefixes must come back full, which is the arm a set-driven
        // implementation would skip entirely.
        let s = set(&[0, 1, 2]);
        let got = s.not_in_range(0, (2 << 16) + 5);
        assert_eq!(got.len(), (2 << 16) + 5 - 3);
        assert!(!got.contains(0) && !got.contains(2));
        assert!(got.contains(3) && got.contains(1 << 16) && got.contains((2 << 16) + 4));
    }

    #[test]
    fn not_in_range_at_the_u64_ceiling_does_not_overflow() {
        // The top chunk's exclusive end is 2^64, which no `u64` holds. A debug
        // build panics on the add if it is computed rather than saturated —
        // the same shape as the `Memtable` range-walk overflow.
        let s = set(&[u64::MAX - 1]);
        let got = s.not_in_range(u64::MAX - 4, u64::MAX);
        assert_eq!(
            got.iter().collect::<Vec<_>>(),
            vec![u64::MAX - 4, u64::MAX - 3, u64::MAX - 2]
        );
        // `hi` is exclusive, so `u64::MAX` itself is unreachable by construction.
        assert!(!got.contains(u64::MAX));
    }

    #[test]
    fn not_in_range_of_a_full_chunk_yields_no_chunk_at_all() {
        // Complementing a chunk the set fully covers must drop the chunk rather
        // than store an empty container — the invariant every kernel relies on.
        let s = set(&(0..65_536u64).collect::<Vec<_>>());
        let got = s.not_in_range(0, 1 << 16);
        assert_eq!(got.len(), 0);
        assert_eq!(got.chunk_count(), 0, "an empty container reached the set");
    }

    #[test]
    fn not_in_range_clips_to_the_range_rather_than_the_chunk() {
        // A range starting and ending mid-chunk: the universe slice must be the
        // range's intersection with the chunk, not the whole chunk.
        let s = set(&[100, 200]);
        let got = s.not_in_range(150, 250);
        assert_eq!(got.min(), Some(150));
        assert_eq!(got.max(), Some(249));
        assert_eq!(got.len(), 100 - 1);
    }
}
