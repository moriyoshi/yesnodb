//! The three container representations and the rules for choosing between them.

pub mod array;
pub mod bitmap;
pub mod codec;
pub mod run;

pub use array::ArrayContainer;
pub use bitmap::BitmapContainer;
pub use run::RunContainer;

use crate::{
    array_bytes, run_bytes, ARRAY_MAX, BITMAP_BYTES, BITMAP_DEMOTE, CHUNK_CARD, OPT_GAIN_DEN,
    OPT_GAIN_NUM, RUN_MAX_INTERVALS,
};

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ContainerKind {
    Array = 0,
    Bitmap = 1,
    Run = 2,
}

/// A chunk's contents. Clone is O(1) for shared payloads (a refcount bump).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Container {
    Array(ArrayContainer),
    Bitmap(BitmapContainer),
    Run(RunContainer),
}

/// Where [`Container::fill_from`] left off.
///
/// Opaque and cheap to copy. Reset it — `DecodeCursor::default()` — when moving
/// to a new container; reusing one across containers reads from the wrong place
/// rather than failing, which is why it carries no container identity to check
/// against and callers must be disciplined.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecodeCursor {
    /// Array: value index. Bitmap: word index. Run: interval index.
    idx: u32,
    /// Bitmap: the still-unemitted bits of word `idx`. Run: offset within
    /// interval `idx`. Unused by arrays.
    residual: u64,
}

impl DecodeCursor {
    /// Whether anything has been taken yet.
    #[inline]
    pub fn is_start(&self) -> bool {
        self.idx == 0 && self.residual == 0
    }
}

impl Container {
    pub fn new_array() -> Self {
        Container::Array(ArrayContainer::new())
    }

    /// Build the natural representation for a sorted, unique value list.
    ///
    /// Chooses the final kind directly rather than inserting one value at a
    /// time through the promotion path — that is what makes bulk build fast.
    pub fn from_sorted(vals: &[u16]) -> Self {
        if vals.len() > ARRAY_MAX {
            Container::Bitmap(BitmapContainer::from_sorted(vals))
        } else {
            Container::Array(ArrayContainer::from_sorted_vec(vals.to_vec()))
        }
    }

    /// Like [`Self::from_sorted`] but takes ownership, avoiding a copy.
    ///
    /// Kernels already produce an owned `Vec`, so this is the form they should
    /// use — `from_sorted` would clone it straight back.
    pub fn from_sorted_vec(vals: Vec<u16>) -> Self {
        if vals.len() > ARRAY_MAX {
            Container::Bitmap(BitmapContainer::from_sorted(&vals))
        } else {
            Container::Array(ArrayContainer::from_sorted_vec(vals))
        }
    }

    #[inline]
    pub fn kind(&self) -> ContainerKind {
        match self {
            Container::Array(_) => ContainerKind::Array,
            Container::Bitmap(_) => ContainerKind::Bitmap,
            Container::Run(_) => ContainerKind::Run,
        }
    }

    #[inline]
    pub fn len(&self) -> u32 {
        match self {
            Container::Array(a) => a.len(),
            Container::Bitmap(b) => b.len(),
            Container::Run(r) => r.len(),
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A full container short-circuits every kernel: `a ∩ full = a`,
    /// `a ∪ full = full`, `a \ full = ∅`.
    #[inline]
    /// Every value in the chunk, as the one-interval run the format prefers.
    ///
    /// Six bytes rather than a filled 8192-byte bitmap, and it is the shape
    /// `is_full()` and the kernels' full-container fast paths expect. A range
    /// insert that covers a whole chunk produces this directly instead of
    /// setting 65 536 bits and waiting for checkpoint run-optimization to
    /// discover what it just built.
    pub fn full() -> Self {
        Container::Run(RunContainer::from_pairs(&[(0, u16::MAX)]))
    }

    pub fn is_full(&self) -> bool {
        self.len() == CHUNK_CARD
    }

    #[inline]
    pub fn contains(&self, v: u16) -> bool {
        match self {
            Container::Array(a) => a.contains(v),
            Container::Bitmap(b) => b.contains(v),
            Container::Run(r) => r.contains(v),
        }
    }

    #[inline]
    pub fn min(&self) -> Option<u16> {
        match self {
            Container::Array(a) => a.min(),
            Container::Bitmap(b) => b.min(),
            Container::Run(r) => r.min(),
        }
    }

    #[inline]
    pub fn max(&self) -> Option<u16> {
        match self {
            Container::Array(a) => a.max(),
            Container::Bitmap(b) => b.max(),
            Container::Run(r) => r.max(),
        }
    }

    #[inline]
    /// Append up to `want` ordinals to `out`, resuming where `cur` left off.
    ///
    /// Returns how many were appended; fewer than `want` means the container is
    /// exhausted and `cur` should be reset for the next one.
    ///
    /// # Why a cursor and not an iterator
    ///
    /// A `ChunkStream` hands out `Chunk<'_>`, so an iterator borrowed from it
    /// cannot outlive the call that produced it. A batch reader wanting to emit
    /// a fixed number of rows at a time therefore either **materializes the
    /// whole container first** — 512 KiB of `u64` for a full bitmap, to emit
    /// 8 192 rows — or carries a position and re-enters. This is the position,
    /// and it is what the design means by "never fully decode a bitmap before
    /// slicing".
    ///
    /// The bitmap arm extracts word-at-a-time with `trailing_zeros`, keeping the
    /// unconsumed bits of the current word in the cursor, so a resume costs no
    /// re-scan. `set_indices()` would be the safe fallback and is not resumable
    /// across calls, which is exactly the property needed here.
    pub fn fill_from(
        &self,
        cur: &mut DecodeCursor,
        base: u64,
        want: usize,
        out: &mut Vec<u64>,
    ) -> usize {
        if want == 0 {
            return 0;
        }
        let before = out.len();
        match self {
            Container::Array(a) => {
                let vals = a.as_slice();
                let start = (cur.idx as usize).min(vals.len());
                let n = want.min(vals.len() - start);
                out.extend(vals[start..start + n].iter().map(|&v| base | v as u64));
                cur.idx += n as u32;
            }
            Container::Run(r) => {
                // `idx` is the interval, `residual` the offset within it.
                let n = r.nruns();
                while out.len() - before < want && cur.idx < n {
                    let (s, e) = (r.start(cur.idx) as u32, r.end(cur.idx) as u32);
                    let from = s + cur.residual as u32;
                    let room = (want - (out.len() - before)) as u32;
                    let take = room.min(e - from + 1);
                    out.extend((from..from + take).map(|v| base | v as u64));
                    if from + take > e {
                        cur.idx += 1;
                        cur.residual = 0;
                    } else {
                        cur.residual += take as u64;
                    }
                }
            }
            Container::Bitmap(b) => {
                let words = b.words();
                while out.len() - before < want {
                    if cur.residual == 0 {
                        // Advance to the next non-empty word.
                        let mut i = cur.idx as usize;
                        while i < words.len() && words[i] == 0 {
                            i += 1;
                        }
                        if i >= words.len() {
                            cur.idx = words.len() as u32;
                            break;
                        }
                        cur.idx = i as u32;
                        cur.residual = words[i];
                    }
                    // Peel set bits out of the word we are standing on.
                    while cur.residual != 0 && out.len() - before < want {
                        let bit = cur.residual.trailing_zeros();
                        out.push(base | (cur.idx as u64 * 64 + bit as u64));
                        cur.residual &= cur.residual - 1;
                    }
                    if cur.residual == 0 {
                        cur.idx += 1;
                    }
                }
            }
        }
        out.len() - before
    }

    /// Values in the half-open window `[lo, hi)` within this chunk.
    ///
    /// Bounds are `u32` because `hi` may be `CHUNK_CARD` — one past the
    /// largest `u16` — which is how a caller names "to the end of the chunk".
    ///
    /// Non-allocating on every kind: two `rank` probes, and a whole-chunk window
    /// short-circuits to the cached cardinality without a probe at all. That is
    /// what lets `range_summary` cost `O( chunks touched )` with payload access
    /// only at the two chunks a range partially covers.
    pub fn count_in_range(&self, lo: u32, hi: u32) -> u32 {
        debug_assert!(
            lo <= hi && hi <= CHUNK_CARD,
            "window {lo}..{hi} is not a chunk window"
        );
        if lo >= hi {
            return 0;
        }
        if lo == 0 && hi >= CHUNK_CARD {
            return self.len();
        }
        let upper = if hi >= CHUNK_CARD {
            self.len()
        } else {
            self.rank(hi as u16)
        };
        upper - self.rank(lo as u16)
    }

    /// Is the half-open window `[lo, hi)` **free of any value**, stopping at the
    /// first one it finds?
    ///
    /// The weaker question behind [`count_in_range`](Self::count_in_range), and
    /// it obeys the rule `ops::card` states and proves for its own predicates:
    /// **a predicate must never cost more than the count it is weaker than.**
    /// Each arm stops at the first set value instead of totalling them:
    ///
    /// - **Array** — one `partition_point` and one comparison, against
    ///   `count_in_range`'s two.
    /// - **Run** — one binary search for the first interval ending at or after
    ///   `lo`, then one comparison. `RunContainer::rank` is a *linear* walk over
    ///   the intervals below `v` and `count_in_range` calls it twice, so this is
    ///   asymptotically better — but not pointwise: `rank` breaks at the first
    ///   interval starting past `v`, so a window at the **front** of a
    ///   many-interval container costs it two iterations against this search's
    ///   `log nruns`. `OrdSet::range_summary` records the measurement.
    /// - **Bitmap** — `ops::mixed::masked_intersects`, which exits at the first
    ///   word that meets the window. `ops::card` records that a
    ///   short-circuiting word loop can be *slower* than a counting one, because
    ///   LLVM widens the second and cannot widen the first — so the comparison
    ///   that matters is against what it replaces: `count_in_range` calls
    ///   `rank` twice, and `rank(hi)` popcounts every word from zero, so it
    ///   already touches a superset of the words this reads even before the
    ///   early exit. There is no shape here where the predicate scans more.
    ///
    /// Bounds are `u32` for the same reason `count_in_range`'s are: `hi` may
    /// be `CHUNK_CARD`, one past the largest `u16`.
    pub fn is_range_empty(&self, lo: u32, hi: u32) -> bool {
        debug_assert!(
            lo <= hi && hi <= CHUNK_CARD,
            "window {lo}..{hi} is not a chunk window"
        );
        if lo >= hi {
            return true;
        }
        // A stored container is never empty, but one held in hand can be, and
        // the whole-chunk window then answers from the cached cardinality with
        // no payload access at all — the same shortcut `count_in_range` takes.
        if self.is_empty() {
            return true;
        }
        if lo == 0 && hi >= CHUNK_CARD {
            return false;
        }
        // `hi > lo >= 0` and `hi <= CHUNK_CARD`, so both fit `u16` once `hi` is
        // made inclusive.
        let (l, h) = (lo as u16, (hi - 1) as u16);
        match self {
            Container::Array(a) => {
                let vals = a.as_slice();
                // The first value at or above `lo`; it is in the window exactly
                // when it is also at or below `hi - 1`.
                let i = vals.partition_point(|&v| v < l);
                !matches!(vals.get(i), Some(&v) if v <= h)
            }
            Container::Bitmap(b) => !crate::ops::mixed::masked_intersects(&b.words(), l, h),
            Container::Run(r) => {
                // The first interval whose `end` reaches `lo`. Every earlier one
                // ends below the window, and the intervals are ordered and
                // disjoint, so this one is the only candidate: it meets the
                // window exactly when it also *starts* at or below `hi - 1`.
                let n = r.nruns();
                let (mut a, mut b) = (0u32, n);
                while a < b {
                    let mid = a + (b - a) / 2;
                    if r.end(mid) < l {
                        a = mid + 1;
                    } else {
                        b = mid;
                    }
                }
                a >= n || r.start(a) > h
            }
        }
    }

    pub fn rank(&self, v: u16) -> u32 {
        match self {
            Container::Array(a) => a.rank(v),
            Container::Bitmap(b) => b.rank(v),
            Container::Run(r) => r.rank(v),
        }
    }

    #[inline]
    pub fn select(&self, n: u32) -> Option<u16> {
        match self {
            Container::Array(a) => a.select(n),
            Container::Bitmap(b) => b.select(n),
            Container::Run(r) => r.select(n),
        }
    }

    pub fn iter(&self) -> ContainerIter<'_> {
        match self {
            Container::Array(a) => ContainerIter::Array(a.as_slice().iter()),
            Container::Bitmap(b) => ContainerIter::Bitmap(b.iter()),
            Container::Run(r) => ContainerIter::Run(RunIter::new(r)),
        }
    }

    /// Number of maximal runs of consecutive values.
    pub fn run_count(&self) -> u32 {
        match self {
            Container::Array(a) => a.run_count(),
            Container::Bitmap(b) => b.run_count(),
            Container::Run(r) => r.nruns(),
        }
    }

    /// Move the payload into shared, refcounted form so [`Clone`] becomes a
    /// refcount bump instead of a memcpy.
    ///
    /// This is what makes the merge-join's single-sided pass-through free. A
    /// set built by repeated `insert` sits in owned form, where cloning copies —
    /// so freeze at commit/optimize boundaries. Mutating afterwards copies once.
    pub fn freeze(&mut self) {
        match self {
            Container::Array(a) => a.vals.freeze(),
            Container::Bitmap(b) => b.bits.freeze(),
            Container::Run(r) => r.runs.freeze(),
        }
    }

    /// Whether cloning this container is allocation-free.
    pub fn is_shared(&self) -> bool {
        match self {
            Container::Array(a) => a.vals.is_shared(),
            Container::Bitmap(b) => b.bits.is_shared(),
            Container::Run(r) => r.runs.is_shared(),
        }
    }

    /// Serialized payload size in bytes under the Roaring spec encoding.
    pub fn payload_bytes(&self) -> usize {
        match self {
            Container::Array(a) => array_bytes(a.len()),
            Container::Bitmap(_) => BITMAP_BYTES,
            Container::Run(r) => run_bytes(r.nruns()),
        }
    }

    /// Insert one value, promoting array -> bitmap at the capacity bound.
    ///
    /// Promotion is mandatory and inline because it is a capacity bound, not a
    /// heuristic. Demotion is *not* done here — see [`Self::ensure_demoted`].
    pub fn insert(&mut self, v: u16) -> bool {
        match self {
            Container::Array(a) => {
                if a.len() as usize == ARRAY_MAX && !a.contains(v) {
                    let mut b = BitmapContainer::from_sorted(a.as_slice());
                    let changed = b.insert(v);
                    *self = Container::Bitmap(b);
                    return changed;
                }
                a.insert(v)
            }
            Container::Bitmap(b) => b.insert(v),
            Container::Run(r) => {
                if r.contains(v) {
                    return false;
                }
                // v1 simplification: any single-value mutation of a Run leaves the
                // run representation first. This keeps interval split/merge — the
                // trickiest container code — off the hot path entirely.
                let mut c = self.to_array_or_bitmap();
                let changed = c.insert(v);
                *self = c;
                changed
            }
        }
    }

    /// Add every value in `[lo, hi]` inclusive; returns how many were new.
    ///
    /// # Why this is not a loop over `insert`
    ///
    /// `Db::insert_range` on a contiguous span used to walk the span one
    /// ordinal at a time, which for a full chunk is 65 536 calls that each
    /// re-dispatch on the container kind, against one masked pass over 1024
    /// words. `BitmapContainer::insert_range` existed for exactly this and had
    /// no production caller.
    ///
    /// An array that the range would push past `ARRAY_MAX` is promoted **once,
    /// up front**, rather than promoted mid-loop on whichever insert happens to
    /// cross the boundary. A run leaves the run representation first, per the
    /// v1 rule that keeps interval split/merge off the write path — checkpoint
    /// run-optimization is what turns a filled bitmap back into a run.
    pub fn insert_range(&mut self, lo: u16, hi: u16) -> u32 {
        debug_assert!(lo <= hi);
        let span = hi as u32 - lo as u32 + 1;
        match self {
            Container::Bitmap(b) => b.insert_range(lo, hi),
            Container::Array(a) => {
                // Upper bound on the result; promote once if it may not fit.
                if a.len() + span > ARRAY_MAX as u32 {
                    let mut b = BitmapContainer::from_sorted(a.as_slice());
                    let changed = b.insert_range(lo, hi);
                    *self = Container::Bitmap(b);
                    changed
                } else {
                    let mut changed = 0;
                    for v in lo..=hi {
                        changed += a.insert(v) as u32;
                    }
                    changed
                }
            }
            Container::Run(_) => {
                let mut c = self.to_array_or_bitmap();
                let changed = c.insert_range(lo, hi);
                *self = c;
                changed
            }
        }
    }

    /// Drop every value in `[lo, hi]` inclusive; returns how many were present.
    pub fn remove_range(&mut self, lo: u16, hi: u16) -> u32 {
        debug_assert!(lo <= hi);
        match self {
            Container::Bitmap(b) => b.remove_range(lo, hi),
            Container::Array(a) => {
                let mut changed = 0;
                for v in lo..=hi {
                    changed += a.remove(v) as u32;
                }
                changed
            }
            Container::Run(_) => {
                let mut c = self.to_array_or_bitmap();
                let changed = c.remove_range(lo, hi);
                *self = c;
                changed
            }
        }
    }

    pub fn remove(&mut self, v: u16) -> bool {
        match self {
            Container::Array(a) => a.remove(v),
            Container::Bitmap(b) => b.remove(v),
            Container::Run(r) => {
                if !r.contains(v) {
                    return false;
                }
                let mut c = self.to_array_or_bitmap();
                let changed = c.remove(v);
                *self = c;
                changed
            }
        }
    }

    /// Materialize as Array or Bitmap according to cardinality alone.
    fn to_array_or_bitmap(&self) -> Container {
        if self.len() as usize > ARRAY_MAX {
            let mut b = BitmapContainer::zeroed();
            for v in self.iter() {
                b.insert(v);
            }
            Container::Bitmap(b)
        } else {
            Container::Array(ArrayContainer::from_sorted_vec(self.iter().collect()))
        }
    }

    /// Demote bitmap -> array once cardinality falls *below* [`BITMAP_DEMOTE`].
    ///
    /// Deliberately not at 4096: the 512-value hysteresis band means a workload
    /// alternating insert/remove at the boundary cannot thrash. Called at
    /// commit/flush time, never on the write path.
    pub fn ensure_demoted(&mut self) {
        if let Container::Bitmap(b) = self {
            if b.len() < BITMAP_DEMOTE {
                let vals: Vec<u16> = b.iter().collect();
                *self = Container::Array(ArrayContainer::from_sorted_vec(vals));
            }
        }
    }

    /// Re-select the encoding by serialized size, requiring a >= 12.5% saving.
    ///
    /// The gain margin is what stops a container sitting near a crossover from
    /// oscillating on every commit. Run-optimization happens here and nowhere
    /// else: run detection is O(card) and runs are awkward to mutate, so it is a
    /// storage optimization, not a write-path one.
    pub fn optimize(&mut self) {
        self.ensure_demoted();

        let cur = self.payload_bytes();
        let card = self.len();
        let nruns = self.run_count();

        // A run container we would refuse to write must not be chosen.
        let run_ok = nruns <= RUN_MAX_INTERVALS && nruns > 0;
        let run_sz = run_bytes(nruns);
        let alt_sz = if card as usize > ARRAY_MAX {
            BITMAP_BYTES
        } else {
            array_bytes(card)
        };

        let beats = |new: usize, old: usize| new * OPT_GAIN_DEN <= old * OPT_GAIN_NUM;

        match self {
            Container::Run(_) => {
                if beats(alt_sz, cur) {
                    *self = self.to_array_or_bitmap();
                }
            }
            _ => {
                if run_ok && beats(run_sz, cur) {
                    let pairs: Vec<(u16, u16)> = match self {
                        Container::Bitmap(b) => b.runs(),
                        _ => runs_from_sorted(self.iter()),
                    };
                    *self = Container::Run(RunContainer::from_pairs(&pairs));
                }
            }
        }
    }
}

/// Coalesce a sorted value stream into inclusive `(start, end)` pairs.
fn runs_from_sorted(vals: impl Iterator<Item = u16>) -> Vec<(u16, u16)> {
    let mut out: Vec<(u16, u16)> = Vec::new();
    for v in vals {
        match out.last_mut() {
            Some((_, e)) if *e != u16::MAX && *e + 1 == v => *e = v,
            _ => out.push((v, v)),
        }
    }
    out
}

pub struct RunIter<'a> {
    r: &'a RunContainer,
    idx: u32,
    cur: u32,
    end: u32,
}

impl<'a> RunIter<'a> {
    fn new(r: &'a RunContainer) -> Self {
        if r.nruns() == 0 {
            RunIter {
                r,
                idx: 0,
                cur: 1,
                end: 0,
            }
        } else {
            RunIter {
                r,
                idx: 0,
                cur: r.start(0) as u32,
                end: r.end(0) as u32,
            }
        }
    }
}

impl Iterator for RunIter<'_> {
    type Item = u16;

    fn next(&mut self) -> Option<u16> {
        loop {
            if self.cur <= self.end {
                let v = self.cur as u16;
                self.cur += 1;
                return Some(v);
            }
            self.idx += 1;
            if self.idx >= self.r.nruns() {
                return None;
            }
            self.cur = self.r.start(self.idx) as u32;
            self.end = self.r.end(self.idx) as u32;
        }
    }
}

pub enum ContainerIter<'a> {
    Array(std::slice::Iter<'a, u16>),
    Bitmap(bitmap::BitmapIter<'a>),
    Run(RunIter<'a>),
}

impl Iterator for ContainerIter<'_> {
    type Item = u16;

    #[inline]
    fn next(&mut self) -> Option<u16> {
        match self {
            ContainerIter::Array(i) => i.next().copied(),
            ContainerIter::Bitmap(i) => i.next(),
            ContainerIter::Run(i) => i.next(),
        }
    }
}

#[cfg(test)]
mod tests {

    /// `fill_from` must reproduce `iter()` exactly, at every slice width, on
    /// every kind.
    ///
    /// The widths matter more than the values. A cursor is only interesting
    /// where it *resumes*, so a width that happens to divide the container's
    /// cardinality never exercises a mid-word or mid-interval restart — which is
    /// precisely where an off-by-one lives. The primes below guarantee ragged
    /// boundaries against every shape here.
    #[test]
    fn a_decode_cursor_reproduces_iter_at_every_slice_width() {
        let shapes: Vec<(&str, Container)> = vec![
            (
                "array",
                Container::Array(ArrayContainer::from_sorted_vec(
                    (0u16..3000)
                        .map(|i| i.wrapping_mul(7))
                        .collect::<std::collections::BTreeSet<u16>>()
                        .into_iter()
                        .collect::<Vec<u16>>(),
                )),
            ),
            (
                "bitmap",
                Container::Bitmap(BitmapContainer::from_sorted(
                    &(0u32..65_536)
                        .step_by(3)
                        .map(|v| v as u16)
                        .collect::<Vec<u16>>(),
                )),
            ),
            (
                "run",
                Container::Run(RunContainer::from_sorted_values(
                    (0u32..65_536)
                        .filter(|v| (v / 100) % 2 == 0)
                        .map(|v| v as u16),
                )),
            ),
            (
                "full run",
                Container::Run(RunContainer::from_sorted_values(
                    (0u32..65_536).map(|v| v as u16),
                )),
            ),
            (
                "one value",
                Container::Array(ArrayContainer::from_sorted_vec(vec![42u16])),
            ),
        ];

        for (name, c) in shapes {
            let want: Vec<u64> = c.iter().map(|v| 0xABCD_0000u64 | v as u64).collect();
            assert_eq!(
                want.len(),
                c.len() as usize,
                "{name}: iter disagrees with len"
            );

            for width in [1usize, 2, 7, 13, 101, 997, 4096, 65_536] {
                let mut cur = DecodeCursor::default();
                assert!(cur.is_start());
                let mut got: Vec<u64> = Vec::new();
                loop {
                    let n = c.fill_from(&mut cur, 0xABCD_0000, width, &mut got);
                    if n == 0 {
                        break;
                    }
                    assert!(n <= width, "{name}/{width}: filled {n}, more than asked");
                }
                assert_eq!(got, want, "{name} at slice width {width}");
            }
        }
    }

    /// A zero-width request takes nothing and moves nothing.
    #[test]
    fn filling_zero_is_a_no_op() {
        let c = Container::Array(ArrayContainer::from_sorted_vec(vec![1u16, 2, 3]));
        let mut cur = DecodeCursor::default();
        let mut out = Vec::new();
        assert_eq!(c.fill_from(&mut cur, 0, 0, &mut out), 0);
        assert!(out.is_empty() && cur.is_start());
        // And the next real call still starts at the beginning.
        assert_eq!(c.fill_from(&mut cur, 0, 3, &mut out), 3);
        assert_eq!(out, vec![1u64, 2, 3]);
    }
    /// [`Container::is_range_empty`] must agree with
    /// [`Container::count_in_range`] on **every** window, on **every** kind.
    ///
    /// This is a *parallel implementation* of a computation that already
    /// exists — the hazard `tests/allocation.rs` was written for, in its
    /// correctness form. `count_in_range` is the oracle here because it is the
    /// function the predicate is weaker than, and the three arms are separate
    /// code with no shared line between them, so a shape that only one kind can
    /// produce is a shape only that kind's arm is tested by.
    ///
    /// The boundaries are drawn **from each container's own structure** — every
    /// value, one below it, one above it, and both chunk ends — so the windows
    /// that start or stop exactly at a value, exactly in a gap, and exactly at
    /// `0` / `CHUNK_CARD` all occur. An off-by-one in a `partition_point`, a
    /// `<` that should be `<=` in the run binary search, or an inclusive /
    /// exclusive slip in the bitmap mask each survive only if some window
    /// straddles nothing, and none here does.
    #[test]
    fn is_range_empty_agrees_with_count_in_range_on_every_kind() {
        use std::collections::BTreeSet;

        let shapes: Vec<(&str, ContainerKind, Container)> = vec![
            (
                "array",
                ContainerKind::Array,
                Container::from_sorted(&[0u16, 1, 5, 63, 64, 100, 1000, 60_000, 65_534, 65_535]),
            ),
            (
                "array, one value",
                ContainerKind::Array,
                Container::from_sorted(&[3000u16]),
            ),
            (
                // Strided past ARRAY_MAX so it is a bitmap and stays one: the
                // gaps are too short for a run container to win.
                "bitmap",
                ContainerKind::Bitmap,
                Container::from_sorted(
                    &(0u32..5_000).map(|i| (i * 13) as u16).collect::<Vec<u16>>(),
                ),
            ),
            (
                "run",
                ContainerKind::Run,
                Container::Run(RunContainer::from_sorted_values(
                    (0u32..65_536)
                        .filter(|v| (v / 100) % 2 == 0)
                        .map(|v| v as u16),
                )),
            ),
            (
                "run, one interval at the top",
                ContainerKind::Run,
                Container::Run(RunContainer::from_sorted_values(
                    (65_000u32..65_536).map(|v| v as u16),
                )),
            ),
            (
                "full run",
                ContainerKind::Run,
                Container::Run(RunContainer::from_sorted_values(
                    (0u32..65_536).map(|v| v as u16),
                )),
            ),
        ];

        for (name, kind, c) in shapes {
            // A shape that silently changed representation would stop testing
            // the arm it names.
            assert_eq!(c.kind(), kind, "{name}: not the kind this case covers");

            let present: BTreeSet<u32> = c.iter().map(|v| v as u32).collect();

            // Boundaries from the container's own structure, thinned so the
            // pair loop stays small on the dense shapes.
            let mut bounds: BTreeSet<u32> = BTreeSet::from([0, 1, CHUNK_CARD - 1, CHUNK_CARD]);
            let step = (present.len() / 16).max(1);
            for v in present.iter().copied().step_by(step) {
                bounds.insert(v.saturating_sub(1));
                bounds.insert(v);
                bounds.insert(v + 1);
            }
            // And the last value, whichever side of the thinning it fell on.
            if let Some(&v) = present.iter().next_back() {
                bounds.insert(v.saturating_sub(1));
                bounds.insert(v);
                bounds.insert(v + 1);
            }
            let bounds: Vec<u32> = bounds.into_iter().filter(|&b| b <= CHUNK_CARD).collect();

            let (mut saw_empty, mut saw_occupied) = (false, false);
            for &lo in &bounds {
                for &hi in &bounds {
                    if hi < lo {
                        continue;
                    }
                    let want = present.range(lo..hi).next().is_none();
                    assert_eq!(
                        c.is_range_empty(lo, hi),
                        want,
                        "{name}: is_range_empty({lo}, {hi})"
                    );
                    // The relationship that makes the predicate legitimate: it
                    // is the count's own answer, arrived at without counting.
                    assert_eq!(
                        c.is_range_empty(lo, hi),
                        c.count_in_range(lo, hi) == 0,
                        "{name}: disagrees with count_in_range({lo}, {hi})"
                    );
                    saw_empty |= want;
                    saw_occupied |= !want;
                }
            }
            // A shape whose windows are all one verdict proves nothing: a
            // sabotaged arm returning that constant would pass.
            assert!(saw_empty, "{name}: no window was empty");
            assert!(saw_occupied, "{name}: no window was occupied");
        }
    }

    /// The degenerate windows, stated separately because the loop above skips
    /// `hi < lo` and an empty container cannot appear in it.
    #[test]
    fn a_zero_width_window_and_an_empty_container_are_empty() {
        let c = Container::from_sorted(&[7u16]);
        assert!(c.is_range_empty(7, 7), "zero width at a present value");
        assert!(!c.is_range_empty(7, 8));
        assert!(c.is_range_empty(0, 0));
        assert!(!c.is_range_empty(0, CHUNK_CARD));

        let e = Container::new_array();
        assert!(e.is_empty());
        assert!(e.is_range_empty(0, CHUNK_CARD));
    }

    use super::*;

    fn vals(c: &Container) -> Vec<u16> {
        c.iter().collect()
    }

    #[test]
    fn array_promotes_to_bitmap_at_capacity() {
        let mut c = Container::Array(ArrayContainer::from_sorted_vec(
            (0..ARRAY_MAX as u16).collect(),
        ));
        assert_eq!(c.kind(), ContainerKind::Array);
        assert_eq!(c.len(), 4096);

        assert!(c.insert(5000));
        assert_eq!(
            c.kind(),
            ContainerKind::Bitmap,
            "must promote past ARRAY_MAX"
        );
        assert_eq!(c.len(), 4097);
        assert!(c.contains(5000));
        assert!(c.contains(0));
    }

    #[test]
    fn reinserting_at_capacity_does_not_promote() {
        let mut c = Container::Array(ArrayContainer::from_sorted_vec(
            (0..ARRAY_MAX as u16).collect(),
        ));
        assert!(!c.insert(10), "value already present");
        assert_eq!(
            c.kind(),
            ContainerKind::Array,
            "no promotion without growth"
        );
    }

    #[test]
    fn hysteresis_prevents_thrash_at_the_boundary() {
        // A bitmap sitting just above the demote threshold must NOT demote,
        // otherwise alternating insert/remove converts on every operation.
        let mut c = Container::Bitmap(BitmapContainer::from_sorted(
            &(0..4000u16).collect::<Vec<_>>(),
        ));
        c.ensure_demoted();
        assert_eq!(
            c.kind(),
            ContainerKind::Bitmap,
            "4000 >= BITMAP_DEMOTE, stay a bitmap"
        );

        // Only once it falls below 3584 does it demote.
        let mut c = Container::Bitmap(BitmapContainer::from_sorted(
            &(0..3583u16).collect::<Vec<_>>(),
        ));
        c.ensure_demoted();
        assert_eq!(c.kind(), ContainerKind::Array);
    }

    #[test]
    fn promote_demote_cycle_is_lossless() {
        let mut c = Container::Array(ArrayContainer::from_sorted_vec(
            (0..ARRAY_MAX as u16).collect(),
        ));
        c.insert(9000);
        let expect: Vec<u16> = (0..ARRAY_MAX as u16).chain(std::iter::once(9000)).collect();
        assert_eq!(vals(&c), expect);

        c.remove(9000);
        // Drop below BITMAP_DEMOTE (3584), not merely below ARRAY_MAX: at 3600
        // the hysteresis band correctly keeps it a bitmap.
        for v in 3583..ARRAY_MAX as u16 {
            c.remove(v);
        }
        assert_eq!(c.len(), 3583);
        c.ensure_demoted();
        assert_eq!(c.kind(), ContainerKind::Array);
        assert_eq!(vals(&c), (0..3583u16).collect::<Vec<_>>());
    }

    #[test]
    fn optimize_picks_run_for_runny_data() {
        // One long run: run form is 6 bytes vs 2*card for an array.
        let mut c = Container::from_sorted(&(0..4000u16).collect::<Vec<_>>());
        c.optimize();
        assert_eq!(c.kind(), ContainerKind::Run);
        assert_eq!(c.len(), 4000);
        assert_eq!(vals(&c), (0..4000u16).collect::<Vec<_>>());
    }

    #[test]
    fn optimize_leaves_scattered_data_alone() {
        // Every value isolated: runs would cost 4 bytes each vs 2 for an array.
        let scattered: Vec<u16> = (0..1000u16).map(|i| i * 3).collect();
        let mut c = Container::from_sorted(&scattered);
        c.optimize();
        assert_eq!(c.kind(), ContainerKind::Array);
        assert_eq!(vals(&c), scattered);
    }

    #[test]
    fn optimize_respects_the_gain_margin() {
        // Alternating pairs: nruns = card/2, so run bytes = 2 + 4*(card/2) =
        // 2 + 2*card, which is worse than the array's 2*card. Must not convert.
        let pairs: Vec<u16> = (0..500u16).flat_map(|i| [i * 4, i * 4 + 1]).collect();
        let mut c = Container::from_sorted(&pairs);
        c.optimize();
        assert_eq!(c.kind(), ContainerKind::Array);
    }

    #[test]
    fn run_mutation_leaves_run_form() {
        let mut c = Container::Run(RunContainer::from_pairs(&[(10, 20)]));
        assert!(c.insert(50));
        assert_ne!(c.kind(), ContainerKind::Run, "v1 exits Run before mutating");
        assert!(c.contains(50));
        assert!(c.contains(15));
        assert_eq!(c.len(), 12);
    }

    /// A whole-chunk range must land as a one-interval run, not a filled bitmap.
    ///
    /// Both hold the same set, so no correctness test can tell them apart — the
    /// difference is 6 bytes against 8192, and the `is_full()` fast paths.
    #[test]
    fn a_whole_chunk_range_becomes_a_run_not_a_bitmap() {
        let mut c = Container::new_array();
        let added = c.insert_range(0, u16::MAX);
        assert_eq!(added, CHUNK_CARD);
        assert!(c.is_full());
        assert!(
            matches!(Container::full(), Container::Run(_)),
            "a full container must be the one-interval run the kernels expect"
        );
    }

    /// The array arm promotes on an upper bound, so a range that overlaps what
    /// is already there must still report only what it actually added.
    #[test]
    fn a_range_reports_added_values_not_its_span() {
        let mut c = Container::from_sorted(&[10, 11, 12, 13, 14]);
        assert_eq!(c.insert_range(12, 20), 6, "12..=14 were already present");
        assert_eq!(c.len(), 11);
        assert_eq!(c.remove_range(0, 11), 2, "only 10 and 11 were present");
        assert_eq!(c.len(), 9);
    }

    #[test]
    fn full_container_detection() {
        let mut b = BitmapContainer::zeroed();
        b.insert_range(0, 65535);
        let c = Container::Bitmap(b);
        assert!(c.is_full());
        assert_eq!(c.len(), CHUNK_CARD);
    }

    #[test]
    fn iteration_agrees_across_kinds() {
        let src: Vec<u16> = (0..300u16).map(|i| i * 7).collect();
        let arr = Container::Array(ArrayContainer::from_sorted_vec(src.clone()));
        let bm = Container::Bitmap(BitmapContainer::from_sorted(&src));
        let rn = Container::Run(RunContainer::from_sorted_values(src.iter().copied()));
        assert_eq!(vals(&arr), src);
        assert_eq!(vals(&bm), src);
        assert_eq!(vals(&rn), src);
    }
}
