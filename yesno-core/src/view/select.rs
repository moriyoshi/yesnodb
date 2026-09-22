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
//! [`stream_view_ranks`] records the bounded prefix of every slot in the same
//! pass; whole interleaved chunks reuse the cardinality reducer and only the
//! final physical chunk needs ordinal mapping.
//! For 2/4/8-way bitmap chunks it counts fixed owner masks per word instead of
//! enumerating set bits, using NEON on little-endian AArch64 and scalar masks
//! elsewhere; other shapes keep the generic walk. The blocked
//! direct-intersection counter sums whole containers without reading their
//! payloads. Its bitmap arm batches sibling filters in pairs: one architecture
//! dispatch per container, and each data vector is reused for both
//! AND-popcounts. A lone filter retains the scalar word loop because the
//! earlier per-row SIMD dispatch was a measured loss.
//!
//! Under [`ViewLayout::Blocked`] with a stride that is a multiple of 65 536, a
//! constituent occupies a whole number of chunks and its logical ordinals differ
//! from its physical ones by a multiple of the chunk width. So the low 16 bits
//! are unchanged, **every container is bit-for-bit the answer**, and extracting a
//! constituent is a prefix relabel with the payloads shared by refcount rather
//! than rebuilt. That is [`OrdSet::view_select`]'s specialised arm, and it is
//! `O(chunks)` with no payload access at all.

use super::{View, ViewLayout};
use crate::{
    chunk_base, split, ChunkStream, CodecError, Container, OrdSet, Prefix48, Result, ORDINAL_MAX,
};

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
    /// A device to offer blocked batches to, and the identity prefix its
    /// residency table keys on. See [`ViewIntersectionCounter::with_accelerator`].
    accel: crate::accel::Accel,
    source: u64,
    /// Reused across chunks so the offload path does not allocate the larger
    /// of its two buffers per chunk.
    offload_out: Vec<u32>,
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
            accel: crate::accel::Accel::none(),
            source: 0,
            offload_out: Vec::new(),
        })
    }

    /// Offer blocked batches to `accel`, keyed under `source`.
    ///
    /// # What `source` has to be
    ///
    /// `ChunkId` is `source` mixed with the chunk prefix, so `source` carries
    /// the whole of the identity contract that
    /// [`crate::accel::ChunkId`] states and this type cannot check: equal
    /// payloads must produce equal ids, and **different payloads must produce
    /// different ones**. A posting-list key alone satisfies the first and
    /// fails the second, because a commit rewrites a chunk without changing
    /// its key or its prefix -- so fold in something that moves when the bytes
    /// move, such as the snapshot version.
    ///
    /// Getting this wrong does not produce an error. It produces a stale
    /// device copy answering for new contents.
    ///
    /// Only the blocked layout is offered today; interleaved traversal keeps
    /// its own path. An accelerator is free to decline, and the CPU path
    /// below is both the fallback and the oracle.
    pub fn with_accelerator(mut self, accel: crate::accel::Accel, source: u64) -> Self {
        self.accel = accel;
        self.source = source;
        self
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
        // The final chunk's mathematical end is 2^64. The reserved
        // u64::MAX sentinel is the exclusive end of every legal ordinal.
        let chunk_end = base.saturating_add(crate::CHUNK_CARD as u64);
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

            if try_offload_blocked(
                &self.accel,
                chunk_identity(self.source, base),
                data,
                row_words,
                query_words,
                &mut self.counts,
                first_owner,
                rows,
                &mut self.offload_out,
            ) {
                return;
            }

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

/// Accumulate owner counts from one aligned bitmap payload, if a vector arm applies.
#[inline]
fn count_bitmap_words_simd(words: &[u64], counts: &mut [u64]) -> bool {
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    if std::arch::is_aarch64_feature_detected!("neon") {
        // SAFETY: NEON was detected and bitmap_words lends one complete
        // 1024-word container payload. Each arity uses its own owner masks.
        match counts.len() {
            2 => counts
                .iter_mut()
                .zip(unsafe { neon_count::count::<2>(words) })
                .for_each(|(count, add)| *count += u64::from(add)),
            4 => counts
                .iter_mut()
                .zip(unsafe { neon_count::count::<4>(words) })
                .for_each(|(count, add)| *count += u64::from(add)),
            8 => counts
                .iter_mut()
                .zip(unsafe { neon_count::count::<8>(words) })
                .for_each(|(count, add)| *count += u64::from(add)),
            _ => return false,
        }
        return true;
    }
    #[cfg(all(target_arch = "x86_64", target_endian = "little"))]
    if std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: AVX2 was detected and bitmap_words lends one complete
        // 1024-word container payload. Each arity uses its own owner masks.
        match counts.len() {
            2 => counts
                .iter_mut()
                .zip(unsafe { avx2_count::count::<2>(words) })
                .for_each(|(count, add)| *count += u64::from(add)),
            4 => counts
                .iter_mut()
                .zip(unsafe { avx2_count::count::<4>(words) })
                .for_each(|(count, add)| *count += u64::from(add)),
            8 => counts
                .iter_mut()
                .zip(unsafe { avx2_count::count::<8>(words) })
                .for_each(|(count, add)| *count += u64::from(add)),
            _ => return false,
        }
        return true;
    }
    let _ = (words, counts);
    false
}

/// The x86_64 counterpart of [`neon_count`].
///
/// Structurally the same kernel -- mask per owner, count bytes, accumulate --
/// but the two middle steps have no x86 instruction. Byte population count is
/// synthesized with Mula's `pshufb` nibble table, which `ops::bitmap` already
/// measured at 2.70x over scalar `popcnt` on this project's reference x86
/// machine, and NEON's `vpadalq_u8` accumulate becomes `psadbw`, which sums
/// each 8-byte group straight into a 64-bit lane. That second substitution is
/// what makes the bound trivial rather than tight: NEON needs the B10 argument
/// about u16 lanes because it accumulates in 16 bits, while a 64-bit lane
/// cannot overflow from an 8 KiB input at all.
///
/// AVX2 rather than 128-bit SSE because the popcount must be synthesized
/// either way, which is exactly the condition under which `ops::bitmap`
/// measured the wider register to pay. There is no cross-lane step here --
/// `psadbw` accumulates within its lane and the horizontal sum happens once
/// per container -- so the permute that made AVX2 lose for `ops::array` has no
/// counterpart.
#[cfg(all(target_arch = "x86_64", target_endian = "little"))]
mod avx2_count {
    use crate::BITMAP_WORDS;
    use std::arch::x86_64::*;

    /// B10x: the bitmap has 8192 bytes, visited in 256 complete 32-byte
    /// vectors with no tail. N is 2, 4, or 8; each owner occupies 8/N bits per
    /// byte. Every `_mm256_sad_epu8` result is at most 8 bytes times 8 bits,
    /// and 256 of them sum to at most 16_384 per 64-bit lane, so no
    /// accumulator can wrap.
    ///
    /// # Safety
    ///
    /// Requires AVX2, N in 2/4/8, and exactly BITMAP_WORDS input words.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn count<const N: usize>(words: &[u64]) -> [u32; N] {
        debug_assert!(matches!(N, 2 | 4 | 8), "B10x");
        debug_assert_eq!(words.len(), BITMAP_WORDS, "B10x");
        let bytes = bytemuck::cast_slice::<u64, u8>(words);
        let base_mask = (u8::MAX as u16 / ((1u16 << N) - 1)) as u8;
        // SAFETY: B10x bounds each 32-byte load inside the 8192-byte payload.
        // The nibble table is indexed by values masked to 0..15, so `pshufb`
        // never sees the high bit that would zero a lane.
        unsafe {
            #[rustfmt::skip]
            let lut = _mm256_setr_epi8(
                0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3, 3, 4,
                0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3, 3, 4,
            );
            let low_mask = _mm256_set1_epi8(0x0f);
            let zero = _mm256_setzero_si256();
            // The table is a full 0..15 population count, but only a subset of
            // its lanes is reachable: after the owner mask, every nibble is a
            // submask of that owner's nibble -- {0,1,4,5} at arity 2, {0,1} at
            // arity 4, a single bit at arity 8. Discovered by mutation: forcing
            // the entry for 0b1111 to a wrong value changes no result, because
            // no arity can index it. Keep the full table anyway ( it is a
            // compile-time constant and the general form is the recognisable
            // one ), but do not read an unchanged test as covering a lane the
            // kernel cannot select.
            let masks: [_; N] =
                std::array::from_fn(|owner| _mm256_set1_epi8((base_mask << owner) as i8));
            let mut sums = [zero; N];
            let mut i = 0usize;
            while i < bytes.len() {
                let input = _mm256_loadu_si256(bytes.as_ptr().add(i).cast());
                for owner in 0..N {
                    let masked = _mm256_and_si256(input, masks[owner]);
                    let lo = _mm256_and_si256(masked, low_mask);
                    let hi = _mm256_and_si256(_mm256_srli_epi16(masked, 4), low_mask);
                    let counted =
                        _mm256_add_epi8(_mm256_shuffle_epi8(lut, lo), _mm256_shuffle_epi8(lut, hi));
                    sums[owner] = _mm256_add_epi64(sums[owner], _mm256_sad_epu8(counted, zero));
                }
                i += 32;
            }
            std::array::from_fn(|owner| {
                let mut lanes = [0u64; 4];
                _mm256_storeu_si256(lanes.as_mut_ptr().cast(), sums[owner]);
                (lanes[0] + lanes[1] + lanes[2] + lanes[3]) as u32
            })
        }
    }
}

#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
mod neon_count {
    use crate::BITMAP_WORDS;
    use std::arch::aarch64::*;

    /// B10: the bitmap has 8192 bytes, visited in 512 complete 16-byte
    /// vectors. N is 2, 4, or 8; each owner occupies 8/N bits per byte.
    /// A u16 accumulator lane receives at most 512 * 16/N <= 4096,
    /// and each horizontal owner count is at most 65_536/N <= 32_768.
    ///
    /// # Safety
    ///
    /// Requires NEON, N in 2/4/8, and exactly BITMAP_WORDS input words.
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn count<const N: usize>(words: &[u64]) -> [u32; N] {
        debug_assert!(matches!(N, 2 | 4 | 8), "B10");
        debug_assert_eq!(words.len(), BITMAP_WORDS, "B10");
        let bytes = bytemuck::cast_slice::<u64, u8>(words);
        let base_mask = (u8::MAX as u16 / ((1u16 << N) - 1)) as u8;
        // SAFETY: B10 bounds each 16-byte load. Accumulators and horizontal
        // sums remain below u16::MAX even for an all-one bitmap.
        unsafe {
            let masks: [_; N] = std::array::from_fn(|owner| vdupq_n_u8(base_mask << owner));
            let mut sums = [vdupq_n_u16(0); N];
            let mut i = 0usize;
            while i < bytes.len() {
                let input = vld1q_u8(bytes.as_ptr().add(i));
                for owner in 0..N {
                    sums[owner] = vpadalq_u8(sums[owner], vcntq_u8(vandq_u8(input, masks[owner])));
                }
                i += 16;
            }
            std::array::from_fn(|owner| vaddvq_u16(sums[owner]) as u32)
        }
    }
}

fn add_view_cardinalities(
    view: &View,
    prefix: Prefix48,
    container: &Container,
    counts: &mut [u64],
) {
    if matches!(view.layout(), ViewLayout::Interleaved) && matches!(view.sets(), 2 | 4 | 8) {
        if let Some(words) = crate::unstable_arrow::bitmap_words(container) {
            if count_bitmap_words_simd(words, counts) {
                return;
            }
            // Both the chunk width and a word's 64 bits divide by these
            // arities, so owner i always occupies bit positions i mod sets.
            let owner_mask = u64::MAX / ((1u64 << view.sets()) - 1);
            for &word in words {
                for (owner, count) in counts.iter_mut().enumerate() {
                    *count += (word & (owner_mask << owner)).count_ones() as u64;
                }
            }
            return;
        }
    }

    // bitmap_words may decline a healthy mmap bitmap whose payload is not
    // u64-aligned; it is not a kind check. Every other arity and both layouts
    // use this representation-independent oracle.
    let base = chunk_base(prefix);
    for low in container.iter() {
        if let Some((owner, _)) = view.logical_of(base | u64::from(low)) {
            counts[owner as usize] += 1;
        }
    }
}

/// Count every constituent while consuming a prefix-ordered chunk stream.
///
/// This is the non-materializing counterpart of
/// [`OrdSet::view_cardinalities`]. It uses the same bitmap SIMD helper for
/// aligned 2/4/8-way interleaved chunks and the same ordinal mapping otherwise.
/// Unlike an `OrdSet`, an arbitrary stream is fallible and untrusted, so empty
/// chunks, repeated prefixes, prefixes outside 48 bits, and the reserved
/// `u64::MAX` ordinal are reported as errors.
/// Mix a caller-supplied source with a chunk base into a device identity.
///
/// Not a hash of the payload: hashing 8 KiB to identify a chunk costs more
/// than the intersection being accelerated. See [`crate::accel::ChunkId`] for
/// what the caller must guarantee instead.
fn chunk_identity(source: u64, base: u64) -> crate::accel::ChunkId {
    let mut x = source ^ base.wrapping_mul(0xd1b5_4a32_d192_ed03);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    crate::accel::ChunkId(x ^ (x >> 31))
}

/// Offer one blocked chunk to an accelerator, accumulating on success.
///
/// Returns `false` for every reason -- no device, a batch too small to pay,
/// a chunk not resident and not yet worth filling, a device that simply said
/// no -- and the caller then runs its ordinary loop. Declining is the normal
/// case and costs a comparison.
///
/// A free function rather than a method so the three fields it needs can be
/// borrowed disjointly: `counts` mutably while `query_words` stays shared.
#[allow(clippy::too_many_arguments)]
fn try_offload_blocked(
    accel: &crate::accel::Accel,
    chunk: crate::accel::ChunkId,
    data: &[u64],
    row_words: usize,
    query_words: &[Vec<u64>],
    counts: &mut [Vec<u64>],
    first_owner: usize,
    rows: usize,
    scratch: &mut Vec<u32>,
) -> bool {
    if rows == 0 || !accel.worth_offering(query_words.len()) {
        return false;
    }
    // Only the pointer vector is built per chunk; the wider count buffer is
    // the caller's scratch and is reused.
    let filters: Vec<&[u64]> = query_words.iter().map(|q| q.as_slice()).collect();
    if filters.iter().any(|f| f.len() != row_words) {
        return false;
    }
    scratch.clear();
    scratch.resize(filters.len() * rows, 0);
    if !accel.and_cardinalities(chunk, data, row_words, &filters, scratch) {
        return false;
    }
    // The accelerator returns absolute counts; accumulation across chunks is
    // the caller's, which is what keeps the device free of scan state.
    for (f, owner_counts) in counts.iter_mut().enumerate().take(filters.len()) {
        let row_counts = &scratch[f * rows..(f + 1) * rows];
        for (r, got) in row_counts.iter().enumerate() {
            owner_counts[first_owner + r] += u64::from(*got);
        }
    }
    true
}

pub fn stream_view_cardinalities(stream: &mut dyn ChunkStream, view: &View) -> Result<Vec<u64>> {
    view.check()?;
    let mut counts = vec![0; view.sets() as usize];
    let mut last_prefix = None;
    while let Some((prefix, container)) = stream.next_chunk()? {
        super::validate_stream_chunk(&mut last_prefix, prefix, &container)?;
        add_view_cardinalities(view, prefix, &container, &mut counts);
    }
    Ok(counts)
}

fn add_view_ranks(
    view: &View,
    prefix: Prefix48,
    container: &Container,
    upper: u64,
    counts: &mut [u64],
) {
    if upper == 0 {
        return;
    }
    if matches!(view.layout(), ViewLayout::Interleaved) {
        let physical_end = u128::from(upper) * u128::from(view.sets());
        let chunk_start = u128::from(prefix) << 16;
        if chunk_start >= physical_end {
            return;
        }
        if chunk_start + (1u128 << 16) <= physical_end {
            add_view_cardinalities(view, prefix, container, counts);
            return;
        }
    }

    let base = chunk_base(prefix);
    for low in container.iter() {
        if let Some((owner, logical)) = view.logical_of(base | u64::from(low)) {
            if logical < upper {
                counts[owner as usize] += 1;
            }
        }
    }
}

/// Rank every constituent below one strict logical upper bound while consuming
/// a prefix-ordered chunk stream.
///
/// Whole interleaved chunks below the physical endpoint reuse
/// [`stream_view_cardinalities`]' bitmap SIMD helper. The one chunk straddling
/// the endpoint is clipped by logical ordinal, with `u128` endpoint arithmetic
/// so `upper * sets` cannot wrap. Blocked views remain correct through the
/// representation-independent ordinal mapping.
pub fn stream_view_ranks(
    stream: &mut dyn ChunkStream,
    view: &View,
    upper: u64,
) -> Result<Vec<u64>> {
    view.check()?;
    let mut counts = vec![0; view.sets() as usize];
    let mut last_prefix = None;
    while let Some((prefix, container)) = stream.next_chunk()? {
        super::validate_stream_chunk(&mut last_prefix, prefix, &container)?;
        add_view_ranks(view, prefix, &container, upper, &mut counts);
    }
    Ok(counts)
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
    /// Interleaved constituents share one physical span. For arities 2, 4 and
    /// 8, an aligned bitmap word has a fixed owner-bit mask per constituent,
    /// so it can be counted without expanding its set bits. Array, run and
    /// unaligned bitmap chunks retain the ordinal walk. Other arities use
    /// that generic walk throughout. Blocked constituents keep their scalar
    /// range-count arm, which sums whole containers without payload reads.
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
        for (prefix, container) in self.chunks() {
            add_view_cardinalities(v, prefix, container, &mut counts);
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

    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    #[test]
    fn neon_counts_match_scalar_owner_masks_at_word_seams() {
        assert!(std::arch::is_aarch64_feature_detected!("neon"));
        let mut seam = vec![0u64; crate::BITMAP_WORDS];
        for bit in [0, 3, 4, 7, 8, 63, 64, 127, 128, 65_535] {
            seam[bit / 64] |= 1u64 << (bit % 64);
        }
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let dense: Vec<u64> = (0..crate::BITMAP_WORDS)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                state
            })
            .collect();
        fn check<const N: usize>(words: &[u64]) {
            let base_mask = u64::MAX / ((1u64 << N) - 1);
            let expected = std::array::from_fn(|owner| {
                words
                    .iter()
                    .map(|word| (word & (base_mask << owner)).count_ones())
                    .sum::<u32>()
            });
            // SAFETY: NEON was detected and the input satisfies B10.
            let got = unsafe { neon_count::count::<N>(words) };
            assert_eq!(got, expected, "sets={N}");
        }
        for words in [
            vec![0u64; crate::BITMAP_WORDS],
            vec![u64::MAX; crate::BITMAP_WORDS],
            seam,
            vec![0x1111_1111_1111_1111; crate::BITMAP_WORDS],
            dense,
        ] {
            check::<2>(&words);
            check::<4>(&words);
            check::<8>(&words);
        }
    }

    /// The x86 companion of the NEON count differential, over the same
    /// patterns: empty, full, one-hot bits sitting on byte / word / chunk
    /// seams, a periodic word that puts every owner in a different phase, and
    /// an LCG-dense payload. The scalar owner-mask sum is the oracle.
    #[cfg(all(target_arch = "x86_64", target_endian = "little"))]
    #[test]
    fn avx2_counts_match_scalar_owner_masks_at_word_seams() {
        // AVX2, unlike the SSSE3 the fold arm needs, is not universal on
        // x86_64, so this one genuinely has to skip rather than assert. Note
        // what that costs: on a pre-AVX2 host this test is a silent no-op and
        // the count kernel below is never executed here. Its coverage there is
        // the scalar arm plus the independent `BTreeSet` property, and the
        // dispatch declines to the scalar arm on exactly the same condition.
        if !std::arch::is_x86_feature_detected!("avx2") {
            eprintln!("avx2 absent: count differential skipped, scalar arm covers this host");
            return;
        }
        let mut seam = vec![0u64; crate::BITMAP_WORDS];
        for bit in [0, 3, 4, 7, 8, 63, 64, 127, 128, 255, 256, 65_535] {
            seam[bit / 64] |= 1u64 << (bit % 64);
        }
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let dense: Vec<u64> = (0..crate::BITMAP_WORDS)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                state
            })
            .collect();
        fn check<const N: usize>(words: &[u64]) {
            let base_mask = u64::MAX / ((1u64 << N) - 1);
            let expected = std::array::from_fn(|owner| {
                words
                    .iter()
                    .map(|word| (word & (base_mask << owner)).count_ones())
                    .sum::<u32>()
            });
            // SAFETY: AVX2 was detected and the input satisfies B10x.
            let got = unsafe { avx2_count::count::<N>(words) };
            assert_eq!(got, expected, "sets={N}");
        }
        for words in [
            vec![0u64; crate::BITMAP_WORDS],
            vec![u64::MAX; crate::BITMAP_WORDS],
            seam,
            vec![0x1111_1111_1111_1111; crate::BITMAP_WORDS],
            vec![0x8000_0000_0000_0001; crate::BITMAP_WORDS],
            dense,
        ] {
            check::<2>(&words);
            check::<4>(&words);
            check::<8>(&words);
        }
    }

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
