//! Extracting and querying one constituent of a [`View`].
//!
//! # The generic walk is the oracle
//!
//! [`OrdSet::view_select`], [`OrdSet::view_cardinality`], and
//! [`OrdSet::view_intersection_cardinalities`] have specialised arms and a
//! generic one. The generic one maps ordinals through [`View::logical_of`]; it
//! is correct for every descriptor and is never deleted when an arm is faster —
//! same contract as [`ops::generic`](crate::ops::generic).
//!
//! # Why `range_summary` is still not used here
//!
//! **The reason changed on 2026-09-06 and the conclusion did not.** It used
//! to be a cost argument: [`OrdSet::range_summary`](crate::OrdSet::range_summary)
//! counted the whole range and then compared, with no short-circuit, so "is
//! anything in this slot" cost "how many are in it". That half is now false —
//! `range_summary` answers `Empty` from
//! [`Container::is_range_empty`](crate::container::Container::is_range_empty),
//! which stops at the first set value, and on a bitmap it reads only the words
//! the window covers rather than every word below `hi`.
//!
//! What survives is the **shape** of the question, and it is the load-bearing
//! half. None of these paths asks whether a slot is empty:
//! [`OrdSet::view_select`] needs every ordinal of the constituent and
//! [`OrdSet::view_cardinality`] needs a count, and no emptiness predicate,
//! however cheap, answers either. Reaching for one would mean *one call per
//! slot*, and per-slot probing is **quadratic** because the slots sweep forward
//! — every call re-enters at `partition_point` over the chunk directory, and a
//! predicate that is `O(window / 64)` still sums to `O(slots × chunk)` when the
//! windows tile the chunk. That is the same mistake `matrix/read.rs` records as
//! `O(lines × container size)` — 5.5 ms to move 8 KiB. The scalar operations
//! walk once for one requested slot; [`OrdSet::view_cardinalities`] records the
//! whole interleaved per-slot shape in one pass when every count is requested.
//! containers without reading their payloads. Their bitmap arm batches sibling
//! filters in pairs: one architecture dispatch per container, and each data
//! vector is reused for both AND-popcounts. A lone filter retains the scalar
//! word loop because the earlier per-row SIMD dispatch was a measured loss.
//!
//! Under [`ViewLayout::Blocked`] with a stride that is a multiple of 65 536, a
//! constituent occupies a whole number of chunks and its logical ordinals differ
//! from its physical ones by a multiple of the chunk width. So the low 16 bits
//! are unchanged, **every container is bit-for-bit the answer**, and extracting a
//! constituent is a prefix relabel with the payloads shared by refcount rather
//! than rebuilt. That is [`OrdSet::view_select`]'s specialised arm, and it is
//! `O(chunks)` with no payload access at all.

use super::{View, ViewLayout};
use crate::{chunk_base, split, CodecError, Container, OrdSet, Prefix48, Result, ORDINAL_MAX};

/// How an interleaved intersection counter consumes physical chunks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntersectionCountStrategy {
    /// Visit every stored ordinal and test its logical value against each query.
    FullScan,
    /// Visit only the physical row intervals named by the union of the queries.
    Selective,
}

#[derive(Debug)]
struct SelectedLogical {
    filter_start: usize,
    filter_end: usize,
    physical_lo: u64,
    physical_hi: u64,
}

/// Checked chunk accumulator for several view intersection-count vectors.
///
/// Resident sets and persisted streams feed the same operation. Chunks must be
/// supplied in strictly ascending prefix order; rejecting duplicates makes it
/// impossible for overlapping bounded windows to silently double-count one
/// payload. `Selective` is valid only for interleaved views and exposes the
/// coalesced prefix windows that a persisted caller should read.
pub struct ViewIntersectionCounter<'a> {
    view: View,
    filters: Vec<&'a OrdSet>,
    selected: Vec<SelectedLogical>,
    selected_filters: Vec<usize>,
    windows: Vec<(Prefix48, Prefix48)>,
    query_words: Option<Vec<Vec<u64>>>,
    counts: Vec<Vec<u64>>,
    strategy: IntersectionCountStrategy,
    last_prefix: Option<Prefix48>,
    last_logical: Option<u64>,
    last_selected: Vec<bool>,
}

impl<'a> ViewIntersectionCounter<'a> {
    /// Prepare query membership and the required output vectors once.
    pub fn new(
        view: View,
        filters: impl IntoIterator<Item = &'a OrdSet>,
        strategy: IntersectionCountStrategy,
    ) -> Result<Self> {
        view.check()?;
        if strategy == IntersectionCountStrategy::Selective
            && !matches!(view.layout(), ViewLayout::Interleaved)
        {
            return Err(CodecError::Invariant(
                "selective view intersection counting requires an interleaved view",
            ));
        }
        let filters: Vec<_> = filters.into_iter().collect();
        let (selected, selected_filters) = if strategy == IntersectionCountStrategy::Selective {
            selected_logicals(&view, &filters)
        } else {
            (Vec::new(), Vec::new())
        };
        let windows = selected_windows(&selected);
        let query_words = match view.layout() {
            ViewLayout::Blocked { stride } => blocked_query_words_batch(&filters, stride),
            ViewLayout::Interleaved => None,
        };
        let counts = vec![vec![0; view.sets() as usize]; filters.len()];
        let last_selected = vec![false; filters.len()];
        Ok(Self {
            view,
            filters,
            selected,
            selected_filters,
            windows,
            query_words,
            counts,
            strategy,
            last_prefix: None,
            last_logical: None,
            last_selected,
        })
    }

    /// Coalesced half-open chunk-prefix windows for selective traversal.
    pub fn prefix_windows(&self) -> &[(Prefix48, Prefix48)] {
        &self.windows
    }

    /// Choose selective traversal only when the query union covers less than
    /// half the packed logical extent. This is a work ratio, not an arity or
    /// machine-specific timing threshold.
    pub fn recommended_strategy(&self, logical_end: u64) -> IntersectionCountStrategy {
        if matches!(self.view.layout(), ViewLayout::Interleaved)
            && (self.selected.len() as u128) * 2 < u128::from(logical_end)
        {
            IntersectionCountStrategy::Selective
        } else {
            IntersectionCountStrategy::FullScan
        }
    }

    /// Consume one non-empty physical chunk.
    pub fn push(&mut self, prefix: Prefix48, container: &Container) -> Result<()> {
        if self.last_prefix.is_some_and(|last| prefix <= last) {
            return Err(CodecError::Invariant(
                "view intersection chunks are not strictly ascending",
            ));
        }
        self.last_prefix = Some(prefix);
        let base = chunk_base(prefix);
        match (self.view.layout(), self.strategy) {
            (ViewLayout::Interleaved, IntersectionCountStrategy::Selective) => {
                self.count_selected_interleaved(container, base)
            }
            (ViewLayout::Interleaved, IntersectionCountStrategy::FullScan) => {
                self.count_generic(container, base)
            }
            (ViewLayout::Blocked { stride }, _) => self.count_blocked(container, base, stride),
        }
        Ok(())
    }

    /// Complete filter-major vectors in the same order passed to [`Self::new`].
    pub fn finish(self) -> Vec<Vec<u64>> {
        self.counts
    }

    fn count_generic(&mut self, container: &Container, base: u64) {
        for low in container.iter() {
            let physical = base | u64::from(low);
            let Some((owner, logical)) = self.view.logical_of(physical) else {
                continue;
            };
            if self.last_logical != Some(logical) {
                for (selected, filter) in self.last_selected.iter_mut().zip(&self.filters) {
                    *selected = filter.contains(logical);
                }
                self.last_logical = Some(logical);
            }
            for (selected, counts) in self.last_selected.iter().zip(&mut self.counts) {
                if *selected {
                    counts[owner as usize] += 1;
                }
            }
        }
    }

    fn count_selected_interleaved(&mut self, container: &Container, base: u64) {
        let chunk_end = base + crate::CHUNK_CARD as u64;
        let first = self.selected.partition_point(|row| row.physical_hi <= base);
        for row in &self.selected[first..] {
            if row.physical_lo >= chunk_end {
                break;
            }
            let lo = row.physical_lo.max(base);
            let hi = row.physical_hi.min(chunk_end);
            visit_container_range(container, (lo - base) as u32, (hi - base) as u32, |low| {
                let owner = (base + u64::from(low) - row.physical_lo) as usize;
                for &filter in &self.selected_filters[row.filter_start..row.filter_end] {
                    self.counts[filter][owner] += 1;
                }
            });
        }
    }

    fn count_blocked(&mut self, container: &Container, base: u64, stride: u64) {
        if let (Container::Bitmap(bitmap), Some(query_words)) = (container, &self.query_words) {
            let row_words = (stride / 64) as usize;
            let first_owner = base / stride;
            if first_owner >= self.view.sets() as u64 {
                return;
            }
            let rows = (bitmap.words().len() / row_words)
                .min((self.view.sets() as u64 - first_owner) as usize);
            let first_owner = first_owner as usize;
            let last_owner = first_owner + rows;
            let data = &bitmap.words()[..rows * row_words];

            let mut filter = 0usize;
            while filter + 1 < query_words.len() {
                let (left, right) = self.counts.split_at_mut(filter + 1);
                crate::ops::bitmap::words_and_cardinality_rows2(
                    data,
                    &query_words[filter],
                    &query_words[filter + 1],
                    row_words,
                    &mut left[filter][first_owner..last_owner],
                    &mut right[0][first_owner..last_owner],
                );
                filter += 2;
            }
            if filter < query_words.len() {
                let query = &query_words[filter];
                let counts = &mut self.counts[filter];
                for (row, words) in data.chunks_exact(row_words).enumerate() {
                    counts[first_owner + row] += and_popcount(words, query);
                }
            }
            return;
        }

        if let Container::Run(runs) = container {
            let physical_end = (self.view.sets() as u64)
                .saturating_mul(stride)
                .min(ORDINAL_MAX.saturating_add(1));
            for i in 0..runs.nruns() {
                let mut lo = base + u64::from(runs.start(i));
                let end = (base + u64::from(runs.end(i)) + 1).min(physical_end);
                while lo < end {
                    let owner = lo / stride;
                    if owner >= self.view.sets() as u64 {
                        break;
                    }
                    let row_base = owner * stride;
                    let hi = end.min(row_base.saturating_add(stride));
                    for (filter, counts) in self.filters.iter().zip(&mut self.counts) {
                        counts[owner as usize] += filter.len_in_range(lo - row_base, hi - row_base);
                    }
                    lo = hi;
                }
            }
            return;
        }

        self.count_generic(container, base);
    }
}

/// The half-open physical range constituent `set` can occupy.
///
/// `None` when the constituent is not addressable at all. Under
/// [`ViewLayout::Interleaved`] this is the **whole universe** — a constituent's
/// ordinals are scattered at stride `sets`, not confined to a region — so it
/// bounds the walk only for `Blocked`.
fn physical_span(v: &View, set: u32) -> Option<(u64, u64)> {
    match v.layout() {
        ViewLayout::Interleaved => Some((set as u64, u64::MAX)),
        ViewLayout::Blocked { stride } => {
            let base = (set as u64).checked_mul(stride)?;
            (base <= ORDINAL_MAX).then(|| (base, base.saturating_add(stride)))
        }
    }
}

impl OrdSet {
    /// Visit every logical ordinal of constituent `set`, ascending.
    ///
    /// The generic path behind both public entry points.
    fn for_each_logical(&self, v: &View, set: u32, mut f: impl FnMut(u64)) {
        let Some((lo, hi)) = physical_span(v, set) else {
            return;
        };
        if hi <= lo {
            return;
        }
        let last = hi - 1;
        let (p_lo, _) = split(lo);
        let (p_hi, _) = split(last);

        let n = self.chunk_count();
        let mut i = self.partition_point_in(0, n, p_lo);
        while i < n {
            let Some((p, c)) = self.chunk_at(i) else {
                break;
            };
            if p > p_hi {
                break;
            }
            let cb = chunk_base(p);
            for val in c.iter() {
                let o = cb | val as u64;
                // Only the first chunk can hold values below `lo`; the container
                // iterates ascending, so the upper bound can break outright.
                if o < lo {
                    continue;
                }
                if o > last {
                    break;
                }
                if let Some((owner, x)) = v.logical_of(o) {
                    if owner == set {
                        f(x);
                    }
                }
            }
            i += 1;
        }
    }

    /// Extract constituent `set` as a set over its own logical ordinals.
    ///
    /// An empty set for a descriptor that does not [`View::check`] or a `set`
    /// that is out of range — absence, not an error, because a constituent that
    /// was never written is legitimately empty and the caller cannot tell the two
    /// apart from the data anyway.
    pub fn view_select(&self, v: &View, set: u32) -> OrdSet {
        if v.check().is_err() || set >= v.sets() {
            return OrdSet::new();
        }
        if let Some(out) = self.select_blocked_aligned(v, set) {
            return out;
        }
        let mut xs = Vec::new();
        self.for_each_logical(v, set, |x| xs.push(x));
        let mut out = OrdSet::from_iter_unsorted(xs);
        out.optimize();
        out
    }

    /// The chunk-aligned blocked arm: a prefix relabel, payloads shared.
    ///
    /// `None` when the layout is not blocked or the stride is not a whole number
    /// of chunks, in which case the generic walk answers.
    ///
    /// The containers are **cloned, which is a refcount bump** rather than a
    /// copy ( `Container::freeze` ), so this arm moves no bits at all. It is
    /// correct precisely because a multiple-of-65 536 shift leaves the low 16
    /// bits — the container's own value space — untouched.
    fn select_blocked_aligned(&self, v: &View, set: u32) -> Option<OrdSet> {
        let ViewLayout::Blocked { stride } = v.layout() else {
            return None;
        };
        if !stride.is_multiple_of(crate::CHUNK_CARD as u64) {
            return None;
        }
        let (lo, hi) = physical_span(v, set)?;
        let shift = lo >> crate::CHUNK_BITS;
        let p_end = hi >> crate::CHUNK_BITS;

        let n = self.chunk_count();
        let mut i = self.partition_point_in(0, n, lo >> crate::CHUNK_BITS);
        let mut chunks = Vec::new();
        while i < n {
            let Some((p, c)) = self.chunk_at(i) else {
                break;
            };
            if p >= p_end {
                break;
            }
            chunks.push((p - shift, c.clone()));
            i += 1;
        }
        Some(OrdSet::from_chunks(chunks))
    }

    /// Does constituent `set` contain logical ordinal `x`?
    ///
    /// One address computation and one membership test, so it costs what
    /// [`OrdSet::contains`] costs and never materialises the constituent. This is
    /// the one question both layouts answer equally cheaply.
    pub fn view_contains(&self, v: &View, set: u32, x: u64) -> bool {
        v.ordinal_of(set, x).is_some_and(|o| self.contains(o))
    }

    /// How many logical ordinals constituent `set` holds.
    ///
    /// **The two layouts differ by more than a constant here**, which is the
    /// whole reason [`ViewLayout`] is a parameter. Under `Blocked` a constituent
    /// is one contiguous range, so this is
    /// [`OrdSet::len_in_range`](crate::OrdSet::len_in_range) — `O(chunks
    /// touched)` with payload access at no more than two of them. Under
    /// `Interleaved` the constituent's ordinals are scattered at stride `sets`,
    /// so there is nothing to do but walk, and it is `O(nnz)`.
    pub fn view_cardinality(&self, v: &View, set: u32) -> u64 {
        if v.check().is_err() || set >= v.sets() {
            return 0;
        }
        if let ViewLayout::Blocked { .. } = v.layout() {
            let Some((lo, hi)) = physical_span(v, set) else {
                return 0;
            };
            return self.len_in_range(lo, hi);
        }
        let mut n = 0u64;
        self.for_each_logical(v, set, |_| n += 1);
        n
    }

    /// Cardinality of every constituent in one batch.
    ///
    /// Interleaved constituents share one physical span, so asking the scalar
    /// operation once per constituent would walk the same packed set `sets`
    /// times. This method assigns each physical ordinal to its owner in one
    /// pass. Blocked constituents retain the scalar range-count arm, which can
    /// sum whole containers without reading their payloads.
    pub fn view_cardinalities(&self, v: &View) -> Vec<u64> {
        let mut counts = vec![0; v.sets() as usize];
        if v.check().is_err() {
            return counts;
        }
        if let ViewLayout::Blocked { .. } = v.layout() {
            for set in 0..v.sets() {
                counts[set as usize] = self.view_cardinality(v, set);
            }
            return counts;
        }
        for ordinal in self.iter() {
            if let Some((owner, _)) = v.logical_of(ordinal) {
                counts[owner as usize] += 1;
            }
        }
        counts
    }

    /// Cardinality of every constituent after intersecting with `filter`.
    ///
    /// This is the count-only form of selecting every constituent and applying
    /// [`OrdSet::and_cardinality`], but it never constructs those constituent
    /// sets. The generic path is the oracle for every descriptor. A blocked
    /// view whose row width divides one chunk and is a whole number of words
    /// instead counts bitmap rows with word-wise AND-popcount. Batched filters
    /// are paired by the whole-container SIMD arm; a lone filter stays on the
    /// scalar loop. Array, run, mixed, partial, and unaligned payloads remain
    /// exact through the generic path or the bitmap container's alignment-safe
    /// word accessor.
    pub fn view_intersection_cardinalities(&self, v: &View, filter: &OrdSet) -> Vec<u64> {
        self.view_intersection_cardinalities_batch(v, &[filter])
            .pop()
            .unwrap_or_else(|| vec![0; v.sets() as usize])
    }

    /// Cardinalities for several filters while visiting each source chunk once.
    ///
    /// Results are filter-major and preserve the order of `filters`. Interleaved
    /// views select between a query-driven bounded walk and one full scan from
    /// the query union's coverage of the stored logical extent. Blocked views
    /// always scan the packed source once and use native bitmap and run arms.
    pub fn view_intersection_cardinalities_batch(
        &self,
        v: &View,
        filters: &[&OrdSet],
    ) -> Vec<Vec<u64>> {
        let empty = || vec![vec![0; v.sets() as usize]; filters.len()];
        if v.check().is_err()
            || filters.is_empty()
            || filters.iter().all(|filter| filter.is_empty())
        {
            return empty();
        }

        let logical_end = self
            .max()
            .and_then(|physical| v.logical_of(physical))
            .and_then(|(_, logical)| logical.checked_add(1))
            .unwrap_or(0);
        let (strategy, mut counter) = match v.layout() {
            ViewLayout::Interleaved => {
                let support_upper = filters
                    .iter()
                    .fold(0u128, |sum, filter| sum + u128::from(filter.len()));
                let strategy = if support_upper * 2 < u128::from(logical_end) {
                    IntersectionCountStrategy::Selective
                } else {
                    IntersectionCountStrategy::FullScan
                };
                let Ok(counter) =
                    ViewIntersectionCounter::new(*v, filters.iter().copied(), strategy)
                else {
                    return empty();
                };
                (strategy, counter)
            }
            ViewLayout::Blocked { .. } => {
                let Ok(counter) = ViewIntersectionCounter::new(
                    *v,
                    filters.iter().copied(),
                    IntersectionCountStrategy::FullScan,
                ) else {
                    return empty();
                };
                (IntersectionCountStrategy::FullScan, counter)
            }
        };

        if strategy == IntersectionCountStrategy::Selective {
            let windows = counter.prefix_windows().to_vec();
            for (lo, hi) in windows {
                let mut i = self.partition_point_in(0, self.chunk_count(), lo);
                while let Some((prefix, container)) = self.chunk_at(i) {
                    if prefix >= hi {
                        break;
                    }
                    if counter.push(prefix, container).is_err() {
                        return empty();
                    }
                    i += 1;
                }
            }
        } else {
            for (prefix, container) in self.chunks() {
                if counter.push(prefix, container).is_err() {
                    return empty();
                }
            }
        }
        counter.finish()
    }
}

fn blocked_query_words_batch(filters: &[&OrdSet], stride: u64) -> Option<Vec<Vec<u64>>> {
    let chunk = crate::CHUNK_CARD as u64;
    if !stride.is_multiple_of(64) || !chunk.is_multiple_of(stride) {
        return None;
    }
    Some(
        filters
            .iter()
            .map(|filter| {
                let mut words = vec![0u64; (stride / 64) as usize];
                for logical in filter.iter() {
                    if logical >= stride {
                        break;
                    }
                    words[logical as usize / 64] |= 1u64 << (logical % 64);
                }
                words
            })
            .collect(),
    )
}

fn selected_logicals(view: &View, filters: &[&OrdSet]) -> (Vec<SelectedLogical>, Vec<usize>) {
    let mut logicals = Vec::new();
    for (filter, set) in filters.iter().enumerate() {
        for logical in set.iter() {
            if matches!(view.layout(), ViewLayout::Blocked { stride } if logical >= stride) {
                break;
            }
            logicals.push((logical, filter));
        }
    }
    logicals.sort_unstable();

    let mut selected = Vec::new();
    let mut selected_filters = Vec::new();
    let mut i = 0;
    while i < logicals.len() {
        let logical = logicals[i].0;
        let filter_start = selected_filters.len();
        while i < logicals.len() && logicals[i].0 == logical {
            let filter = logicals[i].1;
            if selected_filters.len() == filter_start || selected_filters.last() != Some(&filter) {
                selected_filters.push(filter);
            }
            i += 1;
        }
        let addressable = view.addressable_sets(logical);
        if addressable == 0 {
            selected_filters.truncate(filter_start);
            continue;
        }
        let Some(physical_lo) = view.ordinal_of(0, logical) else {
            selected_filters.truncate(filter_start);
            continue;
        };
        let physical_hi = physical_lo.saturating_add(u64::from(addressable));
        selected.push(SelectedLogical {
            filter_start,
            filter_end: selected_filters.len(),
            physical_lo,
            physical_hi,
        });
    }
    (selected, selected_filters)
}

fn selected_windows(selected: &[SelectedLogical]) -> Vec<(Prefix48, Prefix48)> {
    let mut windows = Vec::new();
    for row in selected {
        let lo = row.physical_lo >> crate::CHUNK_BITS;
        let hi = ((row.physical_hi - 1) >> crate::CHUNK_BITS) + 1;
        if let Some((_, last_hi)) = windows.last_mut() {
            if lo <= *last_hi {
                *last_hi = (*last_hi).max(hi);
                continue;
            }
        }
        windows.push((lo, hi));
    }
    windows
}

fn visit_container_range(container: &Container, lo: u32, hi: u32, mut f: impl FnMut(u16)) {
    if lo >= hi {
        return;
    }
    match container {
        Container::Array(array) => {
            let xs = array.as_slice();
            let start = xs.partition_point(|&x| u32::from(x) < lo);
            let end = xs.partition_point(|&x| u32::from(x) < hi);
            for &x in &xs[start..end] {
                f(x);
            }
        }
        Container::Run(runs) => {
            let mut left = 0;
            let mut right = runs.nruns();
            while left < right {
                let mid = left + (right - left) / 2;
                if u32::from(runs.end(mid)) < lo {
                    left = mid + 1;
                } else {
                    right = mid;
                }
            }
            for i in left..runs.nruns() {
                if u32::from(runs.start(i)) >= hi {
                    break;
                }
                let start = u32::from(runs.start(i)).max(lo);
                let end = (u32::from(runs.end(i)) + 1).min(hi);
                for x in start..end {
                    f(x as u16);
                }
            }
        }
        Container::Bitmap(bitmap) => {
            let words = bitmap.words();
            let first = lo as usize / 64;
            let last = (hi as usize - 1) / 64;
            for (word_index, &word) in words[first..=last].iter().enumerate() {
                let absolute = first + word_index;
                let mut masked = word;
                if absolute == first {
                    masked &= u64::MAX << (lo % 64);
                }
                if absolute == last && !hi.is_multiple_of(64) {
                    masked &= (1u64 << (hi % 64)) - 1;
                }
                while masked != 0 {
                    let bit = masked.trailing_zeros();
                    f((absolute as u32 * 64 + bit) as u16);
                    masked &= masked - 1;
                }
            }
        }
    }
}

fn and_popcount(data: &[u64], query: &[u64]) -> u64 {
    data.iter()
        .zip(query)
        .map(|(data, query)| (data & query).count_ones() as u64)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn set_of(xs: &[u64]) -> OrdSet {
        OrdSet::from_iter_unsorted(xs.iter().copied())
    }

    /// The independent recomputation every arm is checked against.
    fn oracle_select(packed: &OrdSet, v: &View, set: u32) -> BTreeSet<u64> {
        let mut out = BTreeSet::new();
        for (p, c) in packed.chunks() {
            for val in c.iter() {
                let o = chunk_base(p) | val as u64;
                if let Some((owner, x)) = v.logical_of(o) {
                    if owner == set {
                        out.insert(x);
                    }
                }
            }
        }
        out
    }

    fn as_btree(s: &OrdSet) -> BTreeSet<u64> {
        let mut out = BTreeSet::new();
        for (p, c) in s.chunks() {
            for val in c.iter() {
                out.insert(chunk_base(p) | val as u64);
            }
        }
        out
    }

    #[test]
    fn interleaved_select_picks_out_its_own_slots() {
        // Constituent 1 of 3 holds logical 0 and 2 -> physical 1 and 7.
        let packed = set_of(&[1, 7]);
        let v = View::interleaved(3);
        assert_eq!(as_btree(&packed.view_select(&v, 1)), BTreeSet::from([0, 2]));
        assert!(packed.view_select(&v, 0).is_empty());
        assert!(packed.view_select(&v, 2).is_empty());
    }

    #[test]
    fn blocked_select_shifts_by_the_region_base() {
        let v = View::blocked(3, 100);
        // Constituent 2 owns [200, 300); logical 5 and 99.
        let packed = set_of(&[205, 299]);
        assert_eq!(
            as_btree(&packed.view_select(&v, 2)),
            BTreeSet::from([5, 99])
        );
        assert!(packed.view_select(&v, 0).is_empty());
    }

    #[test]
    fn contains_and_cardinality_agree_with_select() {
        for v in [
            View::interleaved(3),
            View::blocked(3, 100),
            View::blocked(3, 65_536),
            View::blocked(3, 131_072),
        ] {
            let packed = OrdSet::from_iter_unsorted((0..4000u64).map(|i| i * 37));
            let cardinalities = packed.view_cardinalities(&v);
            for set in 0..3u32 {
                let sel = packed.view_select(&v, set);
                assert_eq!(packed.view_cardinality(&v, set), sel.len(), "{v:?} {set}");
                assert_eq!(cardinalities[set as usize], sel.len(), "{v:?} {set}");
                for x in [0u64, 1, 5, 99, 100, 1000] {
                    assert_eq!(
                        packed.view_contains(&v, set, x),
                        sel.contains(x),
                        "{v:?} set={set} x={x}"
                    );
                }
            }
        }
    }

    /// The specialised arm and the generic walk are two total functions over
    /// the same domain, and this is the diff that keeps them honest. A stride of
    /// 65 536 and 131 072 reaches the arm; 100 000 does not, because it is not a
    /// whole number of chunks.
    #[test]
    fn the_aligned_arm_agrees_with_the_generic_walk() {
        let sources: Vec<OrdSet> = vec![
            OrdSet::new(),
            set_of(&[0, 1, 65_535, 65_536, 131_071, 131_072, 262_143]),
            OrdSet::from_iter_unsorted((0..300_000u64).filter(|i| i.is_multiple_of(3))),
            OrdSet::from_iter_unsorted(0..200_000u64),
            OrdSet::from_iter_unsorted((0..900u64).map(|i| i * 701)),
        ];
        let mut reached = 0u32;
        let mut nonempty = 0u32;
        for src in &sources {
            for stride in [65_536u64, 131_072, 100_000, 65_535] {
                let v = View::blocked(4, stride);
                for set in 0..4u32 {
                    let mut xs = Vec::new();
                    src.for_each_logical(&v, set, |x| xs.push(x));
                    let mut want = OrdSet::from_iter_unsorted(xs);
                    want.optimize();

                    if let Some(got) = src.select_blocked_aligned(&v, set) {
                        reached += 1;
                        assert_eq!(as_btree(&got), as_btree(&want), "stride={stride} set={set}");
                        nonempty += u32::from(!got.is_empty());
                    }
                    // And the public entry point agrees either way.
                    assert_eq!(
                        as_btree(&src.view_select(&v, set)),
                        as_btree(&want),
                        "stride={stride} set={set}"
                    );
                }
            }
        }
        // Without these the test passes on an arm that never fires, or on
        // pairs of empty sets.
        assert!(reached > 20, "the aligned arm fired only {reached} times");
        assert!(nonempty > 5, "only {nonempty} non-empty aligned results");
    }

    #[test]
    fn select_agrees_with_the_oracle_on_both_layouts() {
        let sources: Vec<OrdSet> = vec![
            OrdSet::new(),
            set_of(&[0, 1, 2, 65_535, 65_536, ORDINAL_MAX]),
            OrdSet::from_iter_unsorted((0..5000u64).map(|i| i * 13)),
            OrdSet::from_iter_unsorted(0..70_000u64),
        ];
        let mut compared = 0u32;
        let mut nonempty = 0u32;
        for src in &sources {
            for v in [
                View::interleaved(1),
                View::interleaved(2),
                View::interleaved(7),
                View::blocked(3, 1),
                View::blocked(3, 100),
                View::blocked(2, 65_536),
            ] {
                for set in 0..v.sets() {
                    let got = src.view_select(&v, set);
                    let want = oracle_select(src, &v, set);
                    assert_eq!(as_btree(&got), want, "{v:?} set={set}");
                    assert_eq!(src.view_cardinality(&v, set), want.len() as u64, "{v:?}");
                    compared += 1;
                    nonempty += u32::from(!want.is_empty());
                }
            }
        }
        assert!(compared > 50, "only {compared} comparisons");
        assert!(
            nonempty > 20,
            "only {nonempty} of {compared} were non-empty"
        );
    }

    /// A constituent whose region starts past the ordinal ceiling holds nothing,
    /// rather than wrapping into another constituent's ordinals.
    ///
    /// Constituent 4 is the one that matters: `4 * 2^62` is `2^64`, which
    /// overflows a `u64` outright, so `ordinal_of` must decline rather than wrap
    /// to zero and hand back constituent 0's contents. Constituent 3 at
    /// `3 * 2^62` is still addressable and is included so the test distinguishes
    /// "out of range" from "merely large".
    #[test]
    fn an_unaddressable_constituent_is_empty_not_wrapped() {
        let v = View::blocked(5, 1 << 62);
        let packed = set_of(&[0, 1 << 62, 3 * (1u64 << 62)]);
        assert_eq!(packed.view_select(&v, 0).len(), 1);
        assert_eq!(packed.view_select(&v, 1).len(), 1);
        assert_eq!(packed.view_select(&v, 3).len(), 1, "still addressable");

        assert_eq!(v.ordinal_of(4, 0), None, "4 * 2^62 overflows a u64");
        assert!(packed.view_select(&v, 4).is_empty());
        assert_eq!(packed.view_cardinality(&v, 4), 0);
    }

    fn count_with_strategy(
        packed: &OrdSet,
        view: View,
        filters: &[&OrdSet],
        strategy: IntersectionCountStrategy,
    ) -> Vec<Vec<u64>> {
        let mut counter = ViewIntersectionCounter::new(view, filters.iter().copied(), strategy)
            .expect("valid counter");
        for (prefix, container) in packed.chunks() {
            counter.push(prefix, container).expect("ascending chunks");
        }
        counter.finish()
    }

    #[test]
    fn interleaved_selective_and_full_scan_agree_across_container_kinds() {
        let view = View::interleaved(17);
        let filters = [
            set_of(&[0, 1, 3_855, 3_856, 7_710, 11_565]),
            OrdSet::from_iter_unsorted((0..12_000).filter(|x| x % 3 != 0)),
        ];
        let filter_refs = [&filters[0], &filters[1]];
        let mut sources = vec![
            OrdSet::from_iter_unsorted(
                (0..12_000u64)
                    .step_by(37)
                    .flat_map(|x| [x * 17, x * 17 + 8]),
            ),
            OrdSet::from_iter_unsorted(
                (0..12_000u64).flat_map(|x| (0..17).map(move |owner| x * 17 + owner)),
            ),
            OrdSet::from_iter_unsorted(0..12_000u64 * 17),
        ];
        sources[2].optimize();

        let mut saw_array = false;
        let mut saw_bitmap = false;
        let mut saw_run = false;
        for packed in &sources {
            for (_, container) in packed.chunks() {
                match container.kind() {
                    crate::ContainerKind::Array => saw_array = true,
                    crate::ContainerKind::Bitmap => saw_bitmap = true,
                    crate::ContainerKind::Run => saw_run = true,
                }
            }
            let selective = count_with_strategy(
                packed,
                view,
                &filter_refs,
                IntersectionCountStrategy::Selective,
            );
            let full = count_with_strategy(
                packed,
                view,
                &filter_refs,
                IntersectionCountStrategy::FullScan,
            );
            assert_eq!(selective, full);
            assert_eq!(
                packed.view_intersection_cardinalities_batch(&view, &filter_refs),
                full
            );
        }
        assert!(saw_array);
        assert!(saw_bitmap);
        assert!(saw_run);
    }

    #[test]
    fn selective_windows_coalesce_and_reject_duplicate_chunks() {
        let view = View::interleaved(4_096);
        let filter = set_of(&[15, 16, 31, 32]);
        let packed = OrdSet::from_iter_unsorted([
            15 * 4_096,
            16 * 4_096 + 1,
            31 * 4_096 + 2,
            32 * 4_096 + 3,
        ]);
        let mut counter =
            ViewIntersectionCounter::new(view, [&filter], IntersectionCountStrategy::Selective)
                .unwrap();
        assert_eq!(counter.prefix_windows(), &[(0, 3)]);
        let (prefix, container) = packed.chunks().next().unwrap();
        counter.push(prefix, container).unwrap();
        assert!(counter.push(prefix, container).is_err());
    }

    #[test]
    fn blocked_run_batch_uses_logical_row_ranges() {
        const STRIDE: u64 = 4_096;
        let view = View::blocked(17, STRIDE);
        let mut packed = OrdSet::from_iter_unsorted(0..17 * STRIDE);
        packed.optimize();
        assert!(packed
            .chunks()
            .all(|(_, container)| container.kind() == crate::ContainerKind::Run));
        let odds = OrdSet::from_iter_unsorted((0..STRIDE).filter(|x| x % 2 == 1));
        let edges = set_of(&[0, STRIDE - 1]);
        let got = packed.view_intersection_cardinalities_batch(&view, &[&odds, &edges]);
        assert_eq!(got[0], vec![STRIDE / 2; 17]);
        assert_eq!(got[1], vec![2; 17]);
    }
}
