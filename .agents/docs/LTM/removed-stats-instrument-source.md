# Removed source: `yesno-core/src/stats.rs`

This document exists so that deleting a file did not delete the work. It carries
the **complete source** of the `( m, r )` container histogram and its compressed
binary trie cost models, as the file stood when it was removed from the tree on
2026-08-28.

## Provenance

- **Why it was removed.** Its purpose was to gate adding a fourth container kind
  ( "do not add a container kind before this exists" ). The question was asked,
  measured, and answered "no" on 2026-08-28, and the instrument had no caller
  anywhere in the workspace.
- **What is committed.** The 978-line pre-trie implementation is at
  `git show 7a641fb:yesno-core/src/stats.rs`. The trie modelling below —
  `TrieShape`, `trie_shape`, `cbt_bits_plain`, `cbt_bits_pruned`, and the four
  report columns — was **never committed** and exists only here.
- **Restoring it.** Copy the block below to `yesno-core/src/stats.rs`, add
  `pub mod stats;` to `yesno-core/src/lib.rs`, and restore the `stats.rs` line to
  ARCHITECTURE's module diagram — `scripts/check-layout.py` verifies that diagram
  against the tree in both directions, so all three are required.
- 1608 lines, `sha256[:16] = bdff9e74a8d8f3dd`.

## What it is worth reading for, not just restoring

Four decisions in here were each arrived at by correcting a wrong first attempt,
and are cheaper to read than to rediscover. The distilled versions are in
[Compression Models and Space Economics](./compression-models-and-space-economics.md)
and [Measurement and Investigation Methodology](./measurement-and-investigation-methodology.md);
the reasoning in full is in the module comment below.

1. **Two axes, not one.** An `m`-only histogram samples a single column of the
   `( m, r )` plane, because the cardinality-only bound implicitly pins `r` to its
   uniform expectation while the real ratio moves ~2.5x across `r`.
2. **Order cells by absolute waste, never ratio.** A full chunk has a bound of
   exactly zero and therefore infinite ratio against six bytes of real waste.
3. **Inline chunks are counted but never binned.** At or below `INLINE_MAX` a
   non-bitmap chunk owns no payload extent at all.
4. **Run compression is not uniformly cheaper than the plain trie**, so the model
   takes the better of the two per chunk — and prices a trie as a fourth *option*
   rather than a replacement.

## Source

````rust
//! Byte-weighted `( m, r )` histogram of stored container encodings.
//!
//! **This measures container payload, not total storage, and the difference
//! is not a rounding error in the sparse regime — it is everything.** Every
//! chunk also costs an index entry, and `INLINE_MAX = 3` means a chunk of at
//! most three ordinals lives *entirely* inside its 8-byte `ChunkRef` and
//! allocates no payload extent at all. For such chunks the payload column is not
//! approximate, it is **empty**: 100% of their stored bytes are index.
//!
//! Above `INLINE_MAX` the index share falls off fast but not instantly. It is
//! quoted as a **range**, because the per-chunk index cost is itself only
//! bracketed: [`INDEX_ENTRY_FLOOR`] = 10 bytes at the narrowest key suffix,
//! against 22 for an untruncated `ChunkKey` plus `ChunkRef`. Suffix truncation
//! is real and working, so the floor is the better guide, but neither end is
//! the measured cost:
//!
//! ```text
//!   m      payload    index share ( floor .. untruncated )
//!   <= 3     0 B          100%  ..  100%     exact at both ends
//!   4        8 B         55.6%  ..  73.3%
//!   16      32 B         23.8%  ..  40.7%
//!   256    512 B          1.9%  ..   4.1%
//! ```
//!
//! The `m <= 3` row is the load-bearing one and is the only row that does not
//! depend on which bracket you take.
//!
//! Two consequences are built into the accounting rather than left to the
//! reader:
//!
//! * **Inline chunks are counted but never binned** ( [`Report::inline_chunks`] ).
//!   They have no container encoding, so no container encoding change can help
//!   them, and binning them at a stored size of zero would drag every ratio and
//!   every waste ordering with it. `Container::payload_bytes` would happily
//!   report `2m` for one — bytes that are not stored anywhere.
//! * **Index cost is reported as an explicit floor**, [`INDEX_ENTRY_FLOOR`] per
//!   chunk, in [`Report::index_floor_bytes`]. It is not added into
//!   `stored_bytes` and the two must not be summed into a "total storage"
//!   figure without saying that the index term is a lower bound.
//!
//! **Do not compare `stored_bytes + index_floor_bytes` against the counting
//! bound below.** That bound prices *storing a container*; the index bytes buy
//! something else entirely — `cardinality()` and the `Full` test in `O(1)`
//! without touching a payload, which is what makes the non-materializing
//! counting paths and the planner's metadata-only decisions possible. No bound
//! on the former charges for the latter, and mixing them is the cross-setup
//! error this file exists to avoid making.
//!
//! For every stored container this records its cardinality `m`, its
//! maximal-run count `r`, and the bytes its Roaring payload actually occupies,
//! against the counting bound for an encoder told both:
//!
//! ```text
//! N( m, r ) = C( m-1, r-1 ) * C( n-m+1, r )        n = CHUNK_CARD = 65536
//! ```
//!
//! # Why both axes
//!
//! The obvious instrument is a histogram over `m` alone, compared against
//! `log2 C( n, m )`. That bound implicitly pins the run count to its
//! expectation under a *uniformly distributed* set, `E[r] = m(n-m+1)/n`, and
//! the uniform model fixes `r` within a few percent of that expectation — so an
//! `m`-only histogram samples **one column** of the `( m, r )` plane. At fixed
//! `m` the Roaring-versus-bound ratio moves by about 2.5x across `r`, which is
//! wider than the peak such a histogram exists to locate. Real data is
//! precisely where the uniform assumption fails, so the second axis is not a
//! refinement; without it the instrument cannot size the prize it is measuring.
//!
//! # Why this is offline
//!
//! `m` is free — it is `card_m1` in the leaf entry ( see
//! [`crate::store::extent`] ), so a histogram over `m` alone is an index range
//! scan that faults **no payload extent at all**. `r` is free only for run
//! containers, where [`Container::run_count`] reads the stored `nruns` prefix;
//! for arrays it scans up to 4096 `u16`, and for bitmaps it walks 1024 words.
//! Adding the second axis therefore turns an index scan into a full-heap read.
//!
//! That is affordable for a run-once measurement and is **not** affordable
//! anywhere else. Do not consult this from the planner, and do not fold it
//! into [`crate::stream::sketch::ChunkProfile`], whose own comment records that
//! it classifies chunks by a cached-length comparison and touches no payload.
//! `Snapshot::cardinality` has already decayed from the index-only path into a
//! materializing one once; no correctness test could see it, because both
//! return the same number.
//!
//! # The CBT column: what a compressed binary trie would cost
//!
//! `docs/formal-model.md` §13.6 records the one published structure that beats
//! Roaring on space *and* time at once — the compressed binary trie ( rTrie ) of
//! Arroyuelo and Castillo, at `2( trie(S) - n + 1 ) + o( trie(S) )` bits. §13.9
//! then states what decides whether it is worth building here:
//!
//! ```text
//! saving = SUM over bands ( byte fraction in band ) * ( 1 - new cost / Roaring cost )
//! ```
//!
//! The second factor is analytic; the first is a property of a corpus and is
//! exactly what this histogram measures. So each cell now also carries what a
//! trie would have cost for the containers that landed in it.
//!
//! Two models, because the difference between them is itself the finding:
//!
//! * **plain** — the depth-16 trie with every element a leaf at full depth, at
//!   [`cbt_bits_plain`]. This is [35]'s stated bound verbatim.
//! * **run-compressed** — every entirely-present dyadic subtree collapsed to a
//!   single leaf, at [`cbt_bits_pruned`]. This is their §5, the step §13.6
//!   describes as "recognizing a trie node whose subtree is full", and it is
//!   `ChunkClass::Full` applied at all 16 scales instead of one.
//!
//! **Both omit the `o( trie(S) )` rank/select term, so both are LOWER BOUNDS
//! on any real implementation.** That is deliberate and it is what makes this a
//! *screen*: a cell where the optimistic trie does not beat what is stored today
//! cannot be rescued by implementing one. Do not quote either column as a
//! predicted size.
//!
//! **The two models price leaves differently on purpose.** Plain subtracts
//! the leaf level ( `- n + 1` ) because every leaf sits at known depth 16, so
//! its absent children carry no information. A pruned leaf sits at *whatever*
//! depth its subtree became full, so its child bits are real signal and the
//! uniform two-bits-per-node stands. The asymmetry is in the structures, not in
//! the accounting.
//!
//! **So run compression is not uniformly cheaper, and assuming it is was an
//! error this module made once.** Pruning removes nodes but forfeits the
//! known-depth leaf discount, so where nothing is full it costs `2( m - 1 )`
//! bits *more* than the plain trie. `cbt_best_bytes` is therefore the better of
//! the two taken **per chunk** — which is what any real design would do — and it
//! is the column the saving is computed from. Taking the minimum of the two
//! *totals* would be wrong: different chunks favour different variants. ( The
//! one discriminator bit per chunk that a real encoding would need to say which
//! variant it used is omitted along with the rest of the `o( . )` term. )
//!
//! **The diagnostic to read first is `plain` against `run-compressed`, not
//! either against `stored`.** Where the gap is large the trie's win is coming
//! from run compression — and this crate already has a run container. A trie
//! that only beats Roaring where `Run` was available anyway is reinventing
//! [`crate::container::RunContainer`] with sixteen levels of indirection. The
//! interesting cells are the ones where the trie wins and `r` is *high*.
//!
//! Note what this column does **not** price: [35]'s result is
//! intersection-only and static, so a cell where the trie wins on bytes has said
//! nothing about `∪`, `⊕`, `\`, `¬`, or about what a fourth kind costs in kernel
//! arms.
//!
//! **The fourth kind was measured and declined ( 2026-08-28 ).** 6.6% as a
//! fourth kind, an upper bound, against §13.9's own ~6% "not worth a format
//! change" threshold — and a wider chunk does not rescue it. These columns are
//! kept as the *evidence* for that decision, not as groundwork for it.
//!
//! **And this column is bytes only — it must not be read as a time argument,
//! in either direction.** §13.8 already records that the space crossover is not
//! the time crossover, and there is a sharper version of that here: a sorted-`u16`
//! array intersection is a merge over contiguous memory and **vectorizes**, while
//! a trie descent is dependent loads and bit manipulation and does not. So a trie
//! that wins on bytes can still lose on time, and — the trap — comparing one
//! against a *scalar* array kernel would flatter it by whatever SIMD was left on
//! the table. Any time comparison must be against the fastest array kernel this
//! crate can field, not the one it happens to ship. That is the same
//! cross-setup error the note above warns about for `stored_bytes` versus the
//! index floor, one axis over.
//!
//! # Bins are a presentation device, not the measurement
//!
//! The bound is evaluated **per chunk at its exact `( m, r )`** and accumulated
//! before binning, so [`Report::bound_bytes`] and [`Report::waste_bytes`] are
//! exact and independent of where the bin edges fall. Binning affects only how
//! the mass is displayed. Reading a binned table as if it were the function it
//! samples is the error this module is downstream of.
//!
//! # Two caveats to carry with any reading
//!
//! * `N( m, r )` is the bound for an encoder **told `r`**. Where Roaring picks
//!   a bitmap it is not using `r` at all, so those cells pit a structure-blind
//!   encoding against a structure-aware bound. The extreme is the perfectly
//!   alternating chunk, `m = r = 32768`: 8192 stored bytes against 1.875, a
//!   ratio of 4369. There are 32 769 such sets out of `C( 65536, 32768 )`, so
//!   the cell is measure-negligible in any real corpus — but nothing here
//!   excludes it, and it would dominate a table sorted by ratio.
//! * A **full** chunk is the reductio: told `m = 65536` and `r = 1` there is
//!   exactly one such set, so the bound is 0 bytes and the ratio is infinite,
//!   against 6 bytes stored — the whole 6 being waste. It is the top of the
//!   `r = 1` band, where waste runs 4 B at `m = 1` to 6 B at a full chunk.
//!
//! Both are why [`Report::cells`] is ordered by **absolute waste**, not by
//! ratio. Away from degenerate cells the ratio is diffuse — a narrow band
//! across the whole plane — so the size of any prize is set by where the bytes
//! are, not by where the ratio is worst.

use std::fmt;
use std::sync::OnceLock;

use crate::container::Container;
use crate::CHUNK_CARD;

/// `log2( k! )` for `k` in `0..=CHUNK_CARD + 1`.
///
/// Built once with compensated summation. The largest entry is ~1.02e6 and the
/// bound subtracts two such entries, so naive accumulation would leave ~1e-5
/// bits of cancellation error — negligible against a byte-granular answer, but
/// the compensation costs three lines and removes the question.
fn log2_fact() -> &'static [f64] {
    static TABLE: OnceLock<Vec<f64>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let top = CHUNK_CARD as usize + 1;
        let mut v = Vec::with_capacity(top + 1);
        v.push(0.0);
        let (mut sum, mut comp) = (0.0f64, 0.0f64);
        for k in 1..=top {
            let y = (k as f64).log2() - comp;
            let t = sum + y;
            comp = (t - sum) - y;
            sum = t;
            v.push(sum);
        }
        v
    })
}

/// `log2 C( a, b )`, for `b <= a <= CHUNK_CARD + 1`.
fn log2_choose(a: u32, b: u32) -> f64 {
    if b == 0 || b == a {
        return 0.0;
    }
    let lf = log2_fact();
    lf[a as usize] - lf[b as usize] - lf[(a - b) as usize]
}

/// Bits needed to name one of the `m`-element subsets of `0..n` that form
/// exactly `r` maximal runs, or `None` if no such subset exists.
fn bound_bits_n(n: u32, m: u32, r: u32) -> Option<f64> {
    if m > n {
        return None;
    }
    if m == 0 {
        return (r == 0).then_some(0.0);
    }
    // A run needs an element, and each run past the first needs a gap.
    if r == 0 || r > m || r > n - m + 1 {
        return None;
    }
    Some(log2_choose(m - 1, r - 1) + log2_choose(n - m + 1, r))
}

/// Bits needed to name one chunk-sized set of cardinality `m` in exactly `r`
/// maximal runs, or `None` if the pair is unrealizable.
///
/// This is the fair comparison for a run container, which stores the count and
/// the intervals — and, per the module note, a *generous* one for a bitmap,
/// which does not use `r` at all.
pub fn bound_bits(m: u32, r: u32) -> Option<f64> {
    bound_bits_n(CHUNK_CARD, m, r)
}

/// Depth of the binary trie over one chunk's value space.
///
/// Derived from [`CHUNK_CARD`] rather than written down, so it cannot drift from
/// the chunking if that ever moves.
const TRIE_DEPTH: u32 = CHUNK_CARD.trailing_zeros();

/// Node counts for the binary trie over one chunk's values.
///
/// Both are structural facts about the set, independent of any encoding — which
/// is what makes them checkable. The cost models that consume them
/// ( [`cbt_bits_plain`], [`cbt_bits_pruned`] ) are where the assumptions live.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrieShape {
    /// Nodes of the plain depth-[`TRIE_DEPTH`] trie, root included, every
    /// element a leaf at full depth.
    pub nodes: u64,
    /// Nodes left once every entirely-present dyadic subtree collapses to a
    /// single leaf — the run compression of the published structure.
    pub pruned_nodes: u64,
}

/// Nodes of the plain binary trie over `vals`, which must be sorted, distinct
/// and below [`CHUNK_CARD`].
///
/// Level `k` holds one node per distinct `v >> ( TRIE_DEPTH - k )`, and two
/// adjacent sorted values share that prefix exactly when their XOR is below
/// `2^( TRIE_DEPTH - k )`. So each adjacent pair removes one node per level it
/// agrees on, and that count is its XOR's leading-zero count within the
/// `TRIE_DEPTH`-bit universe. One `O( m )` pass, no allocation, no descent.
fn plain_nodes(vals: &[u32]) -> u64 {
    let m = vals.len() as u64;
    if m == 0 {
        return 0;
    }
    let mut nodes = 1 + m * TRIE_DEPTH as u64;
    for w in vals.windows(2) {
        let x = w[1] ^ w[0];
        debug_assert!(
            w[1] > w[0] && x < CHUNK_CARD,
            "values must be sorted, distinct and in range"
        );
        nodes -= (TRIE_DEPTH - 1 - x.ilog2()) as u64;
    }
    nodes
}

/// Nodes of the same trie once every entirely-present dyadic subtree collapses
/// to one leaf. `vals` must be sorted, distinct, and inside
/// `[lo, lo + 2^depth)`.
fn pruned_nodes(vals: &[u32], lo: u32, depth: u32) -> u64 {
    if vals.is_empty() {
        return 0;
    }
    // The whole of the run compression: a subtree that is entirely present is
    // one leaf whatever its width.
    if vals.len() as u64 == 1u64 << depth {
        return 1;
    }
    // Unreachable while the values really are distinct — a width-1 range holding
    // any value is full and returned above — but a recursion guard costs one
    // branch and removes the question.
    if depth == 0 {
        return 1;
    }
    let mid = lo + (1u32 << (depth - 1));
    let split = vals.partition_point(|&v| v < mid);
    1 + pruned_nodes(&vals[..split], lo, depth - 1) + pruned_nodes(&vals[split..], mid, depth - 1)
}

fn trie_shape_of_sorted(vals: &[u32]) -> TrieShape {
    TrieShape {
        nodes: plain_nodes(vals),
        pruned_nodes: pruned_nodes(vals, 0, TRIE_DEPTH),
    }
}

/// Both node counts for one container.
///
/// Walks every value, so it costs `O( m )` on top of the payload walk the
/// module note already charges for. Offline only.
pub fn trie_shape(c: &Container) -> TrieShape {
    let vals: Vec<u32> = c.iter().map(u32::from).collect();
    trie_shape_of_sorted(&vals)
}

/// Bits for the plain trie: `2( trie(S) - m + 1 )`.
///
/// The published bound verbatim, leading term only. The `- m + 1` is the leaf
/// level, which carries no information because every leaf sits at known depth
/// [`TRIE_DEPTH`].
///
/// Omits `o( trie(S) )`, so this is a **lower bound**, not a size.
pub fn cbt_bits_plain(m: u32, nodes: u64) -> f64 {
    if m == 0 {
        return 0.0;
    }
    2.0 * (nodes as f64 - m as f64 + 1.0)
}

/// Bits for the run-compressed trie: two per surviving node.
///
/// Deliberately does **not** subtract a leaf level. A pruned leaf sits at
/// whatever depth its subtree became full, so unlike the plain trie's leaves its
/// child bits are real signal. See the module note on why the two models price
/// leaves differently.
///
/// Omits `o( trie(S) )`, so this is a **lower bound**, not a size.
pub fn cbt_bits_pruned(pruned_nodes: u64) -> f64 {
    2.0 * pruned_nodes as f64
}

/// Lower edges of the cardinality bins.
///
/// `4096` and `65536` each get a bin to themselves: `ARRAY_MAX` is where the
/// middle-band peak sits, and the full chunk is the degenerate cell above.
const M_LO: [u32; 17] = [
    1, 4, 16, 64, 256, 1024, 3584, 4096, 4097, 8192, 16384, 32768, 49152, 61440, 64512, 65535,
    65536,
];

/// Lower edges of the run-count bins.
///
/// `r = 1` is isolated because the contiguous chunk is common in real data
/// ( sequential ids, time ranges, anything appended in order ) and behaves
/// unlike everything above it. `2033` is the first count past
/// [`crate::RUN_MAX_INTERVALS`], so the bin below it is "still writable as a
/// run" and the one above is not.
const R_LO: [u32; 16] = [
    1, 2, 3, 5, 9, 17, 33, 65, 129, 257, 513, 1025, 2033, 4097, 8193, 16385,
];

const N_CELLS: usize = M_LO.len() * R_LO.len();

fn bin_of(edges: &[u32], v: u32) -> usize {
    edges.partition_point(|&lo| lo <= v).saturating_sub(1)
}

fn span_of(edges: &[u32], i: usize, top: u32) -> (u32, u32) {
    (
        edges[i],
        if i + 1 < edges.len() {
            edges[i + 1] - 1
        } else {
            top
        },
    )
}

#[derive(Clone, Copy, Default, Debug)]
struct Cell {
    chunks: u64,
    stored: u64,
    bound_bits: f64,
    cbt_plain_bits: f64,
    cbt_pruned_bits: f64,
    /// The better of the two **per chunk**, which is what a real design would
    /// pick. Not `min` of the two totals — different chunks favour different
    /// variants, so taking the minimum after summing would understate it.
    cbt_best_bits: f64,
    /// What the corpus would cost if a trie were a **fourth option** rather than
    /// a replacement: per chunk, the better of the trie and the encoding the
    /// crate already chose. This is the decision-relevant column — a fourth
    /// kind is only ever selected where it wins.
    cbt_selective_bits: f64,
}

impl Cell {
    const ZERO: Cell = Cell {
        chunks: 0,
        stored: 0,
        bound_bits: 0.0,
        cbt_plain_bits: 0.0,
        cbt_pruned_bits: 0.0,
        cbt_best_bits: 0.0,
        cbt_selective_bits: 0.0,
    };

    fn add(&mut self, stored: u64, bits: f64, cbt: (f64, f64)) {
        self.chunks += 1;
        self.stored += stored;
        self.bound_bits += bits;
        self.cbt_plain_bits += cbt.0;
        self.cbt_pruned_bits += cbt.1;
        let best = cbt.0.min(cbt.1);
        self.cbt_best_bits += best;
        self.cbt_selective_bits += best.min(stored as f64 * 8.0);
    }

    fn absorb(&mut self, o: &Cell) {
        self.chunks += o.chunks;
        self.stored += o.stored;
        self.bound_bits += o.bound_bits;
        self.cbt_plain_bits += o.cbt_plain_bits;
        self.cbt_pruned_bits += o.cbt_pruned_bits;
        self.cbt_best_bits += o.cbt_best_bits;
        self.cbt_selective_bits += o.cbt_selective_bits;
    }
}

/// Bytes one index leaf entry occupies, at the **narrowest** key suffix.
///
/// `leaf_entry_size( ksuf_len ) = ksuf_len + 8` and `KSUF_WIDTHS[0] = 2`, so
/// this is 10 — derived here rather than written down, so it cannot drift from
/// the format.
///
/// **A floor, never an estimate.** It deliberately models none of: the actual
/// `ksuf_len` a leaf chooses ( which depends on how many keys share that leaf ),
/// node fill factor, or internal-node overhead. All three can only push the real
/// figure **up**. Pricing them would need three unmeasured assumptions, and a
/// number built on those is worse than a bound that survives all of them.
pub const INDEX_ENTRY_FLOOR: u64 =
    crate::index::node::leaf_entry_size(crate::index::node::KSUF_WIDTHS[0]) as u64;

/// Would this container live inline in its `ChunkRef` rather than an extent?
///
/// Mirrors `checkpoint::inline_values`: at most [`INLINE_MAX`] ordinals **and**
/// not a bitmap. Kept in step with that function deliberately — if the two
/// disagree, this instrument reports payload bytes for chunks that have none.
fn inline_eligible(c: &Container) -> bool {
    c.len() as usize <= crate::store::extent::INLINE_MAX && c.kind() != crate::ContainerKind::Bitmap
}

/// Byte-weighted distribution of stored containers over `( m, r )`.
///
/// Feed it every container you want accounted for, then [`Self::report`].
#[derive(Clone)]
pub struct MrHistogram {
    cells: Vec<Cell>,
    total: Cell,
    kinds: [Cell; 3],
    /// Chunks seen, binned or not — the denominator for the index floor.
    all_chunks: u64,
    /// Chunks living inline in their index entry, owning no payload extent.
    inline_chunks: u64,
    /// Reused across [`MrHistogram::observe`] so a full bitmap does not cost a
    /// fresh 256 KiB allocation per chunk.
    scratch: Vec<u32>,
}

impl Default for MrHistogram {
    fn default() -> Self {
        Self::new()
    }
}

impl MrHistogram {
    pub fn new() -> Self {
        MrHistogram {
            cells: vec![Cell::ZERO; N_CELLS],
            total: Cell::ZERO,
            kinds: [Cell::ZERO; 3],
            all_chunks: 0,
            inline_chunks: 0,
            scratch: Vec::new(),
        }
    }

    /// Account for one stored container.
    ///
    /// Costs a payload walk for arrays and bitmaps — see the module note.
    /// Empty containers are ignored; the invariant is that they are never
    /// stored, and they have no realizable `( m, r )`.
    ///
    /// **A chunk that lives inline in its index entry is counted but not
    /// binned.** See [`MrHistogram::inline_chunks`].
    pub fn observe(&mut self, c: &Container) {
        let m = c.len();
        if m == 0 {
            return;
        }
        // Every chunk costs an index entry, inline or not.
        self.all_chunks += 1;

        // `INLINE_MAX = 3`: a chunk this small lives entirely inside the
        // 8-byte `ChunkRef` and allocates **no payload extent at all**. It has
        // no container encoding, so it is not a candidate for a container
        // encoding change and binning it would distort every statistic this
        // instrument produces. `Container::payload_bytes` would report `2m`
        // here — bytes that are not stored anywhere.
        if inline_eligible(c) {
            self.inline_chunks += 1;
            return;
        }
        let r = c.run_count();
        let stored = c.payload_bytes() as u64;
        let bits = match bound_bits(m, r) {
            Some(b) => b,
            None => {
                debug_assert!(
                    false,
                    "unrealizable ( m, r ) = ( {m}, {r} ) from a stored container"
                );
                0.0
            }
        };

        // What a compressed binary trie would have cost for this container. Two
        // models, both lower bounds — see the module note; the gap between them
        // is how much of the trie's win is really run compression.
        self.scratch.clear();
        self.scratch.extend(c.iter().map(u32::from));
        let shape = trie_shape_of_sorted(&self.scratch);
        let cbt = (
            cbt_bits_plain(m, shape.nodes),
            cbt_bits_pruned(shape.pruned_nodes),
        );

        let i = bin_of(&M_LO, m) * R_LO.len() + bin_of(&R_LO, r);
        self.cells[i].add(stored, bits, cbt);
        self.total.add(stored, bits, cbt);
        self.kinds[match c {
            Container::Array(_) => 0,
            Container::Bitmap(_) => 1,
            Container::Run(_) => 2,
        }]
        .add(stored, bits, cbt);
    }

    /// Account for every chunk of a materialized set.
    pub fn observe_set(&mut self, set: &crate::OrdSet) {
        for i in 0..set.chunk_count() {
            let Some((_, c)) = set.chunk_at(i) else { break };
            self.observe(c);
        }
    }

    /// Fold another histogram in, so a walk can be sharded.
    pub fn merge(&mut self, other: &MrHistogram) {
        for (a, b) in self.cells.iter_mut().zip(other.cells.iter()) {
            a.absorb(b);
        }
        self.total.absorb(&other.total);
        self.all_chunks += other.all_chunks;
        self.inline_chunks += other.inline_chunks;
        for (a, b) in self.kinds.iter_mut().zip(other.kinds.iter()) {
            a.absorb(b);
        }
    }

    pub fn chunks(&self) -> u64 {
        self.total.chunks
    }

    /// Summarize, with cells ordered by absolute waste.
    pub fn report(&self) -> Report {
        let mut cells: Vec<CellReport> = self
            .cells
            .iter()
            .enumerate()
            .filter(|(_, c)| c.chunks > 0)
            .map(|(i, c)| {
                let (mi, ri) = (i / R_LO.len(), i % R_LO.len());
                CellReport::new(
                    span_of(&M_LO, mi, CHUNK_CARD),
                    span_of(&R_LO, ri, CHUNK_CARD / 2),
                    c,
                    self.total.stored,
                )
            })
            .collect();
        cells.sort_by(|a, b| {
            b.waste_bytes
                .partial_cmp(&a.waste_bytes)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // The `r = 1` rollup, kept separate because it is the one band where a
        // large mass means the *least*: a contiguous chunk wastes four bytes
        // whatever its cardinality, so it can top a ratio table while being the
        // smallest prize in the plane.
        let mut contiguous = Cell::ZERO;
        for mi in 0..M_LO.len() {
            contiguous.absorb(&self.cells[mi * R_LO.len()]);
        }

        Report {
            chunks: self.total.chunks,
            inline_chunks: self.inline_chunks,
            all_chunks: self.all_chunks,
            index_floor_bytes: self.all_chunks * INDEX_ENTRY_FLOOR,
            stored_bytes: self.total.stored,
            bound_bytes: self.total.bound_bits / 8.0,
            waste_bytes: self.total.stored as f64 - self.total.bound_bits / 8.0,
            ratio: ratio(self.total.stored, self.total.bound_bits),
            cbt_plain_bytes: self.total.cbt_plain_bits / 8.0,
            cbt_pruned_bytes: self.total.cbt_pruned_bits / 8.0,
            cbt_best_bytes: self.total.cbt_best_bits / 8.0,
            cbt_selective_bytes: self.total.cbt_selective_bits / 8.0,
            cbt_selective_frac: if self.total.stored == 0 {
                0.0
            } else {
                1.0 - (self.total.cbt_selective_bits / 8.0) / self.total.stored as f64
            },
            cbt_saving_bytes: self.total.stored as f64 - self.total.cbt_best_bits / 8.0,
            cbt_saving_frac: if self.total.stored == 0 {
                0.0
            } else {
                (self.total.stored as f64 - self.total.cbt_best_bits / 8.0)
                    / self.total.stored as f64
            },
            cells,
            contiguous: Aggregate::new(&contiguous, self.total.stored),
            kinds: [
                Aggregate::new(&self.kinds[0], self.total.stored),
                Aggregate::new(&self.kinds[1], self.total.stored),
                Aggregate::new(&self.kinds[2], self.total.stored),
            ],
        }
    }
}

fn ratio(stored: u64, bound_bits: f64) -> f64 {
    if bound_bits <= 0.0 {
        f64::INFINITY
    } else {
        stored as f64 * 8.0 / bound_bits
    }
}

/// Totals for a group of cells.
#[derive(Clone, Copy, Debug)]
pub struct Aggregate {
    pub chunks: u64,
    pub stored_bytes: u64,
    pub bound_bytes: f64,
    pub waste_bytes: f64,
    /// Share of all stored bytes, in `0.0..=1.0`.
    pub byte_share: f64,
    /// Stored over bound; infinite where the bound is zero.
    pub ratio: f64,
    /// What a plain binary trie would have cost. Leading term, a lower bound.
    pub cbt_plain_bytes: f64,
    /// What a run-compressed binary trie would have cost. Leading term, a lower
    /// bound — see the module note before quoting it.
    pub cbt_pruned_bytes: f64,
    /// The better of the two variants, chosen **per chunk**. This is the column
    /// the saving is computed from, and the one to read as "what a trie costs".
    pub cbt_best_bytes: f64,
    /// `stored_bytes - cbt_best_bytes`. **Signed**: negative where a trie would
    /// be larger than what is stored today, which is a real outcome and must
    /// not be clamped away.
    pub cbt_saving_bytes: f64,
    /// What this group would cost with a trie available as a **fourth kind**,
    /// picked per chunk only where it wins. Never above `stored_bytes`.
    pub cbt_selective_bytes: f64,
    /// `stored_bytes - cbt_selective_bytes`. Never negative, by construction.
    pub cbt_selective_saving_bytes: f64,
    /// This group's term in §13.9's sum — its saving as a fraction of **all**
    /// stored bytes, so the terms add up to the overall saving.
    pub cbt_saving_share: f64,
}

impl Aggregate {
    fn new(c: &Cell, total_stored: u64) -> Aggregate {
        let cbt_best_bytes = c.cbt_best_bits / 8.0;
        let cbt_saving_bytes = c.stored as f64 - cbt_best_bytes;
        let cbt_selective_bytes = c.cbt_selective_bits / 8.0;
        Aggregate {
            chunks: c.chunks,
            stored_bytes: c.stored,
            bound_bytes: c.bound_bits / 8.0,
            waste_bytes: c.stored as f64 - c.bound_bits / 8.0,
            byte_share: if total_stored == 0 {
                0.0
            } else {
                c.stored as f64 / total_stored as f64
            },
            ratio: ratio(c.stored, c.bound_bits),
            cbt_plain_bytes: c.cbt_plain_bits / 8.0,
            cbt_pruned_bytes: c.cbt_pruned_bits / 8.0,
            cbt_best_bytes,
            cbt_saving_bytes,
            cbt_selective_bytes,
            cbt_selective_saving_bytes: c.stored as f64 - cbt_selective_bytes,
            cbt_saving_share: if total_stored == 0 {
                0.0
            } else {
                cbt_saving_bytes / total_stored as f64
            },
        }
    }
}

/// One occupied `( m, r )` bin.
#[derive(Clone, Copy, Debug)]
pub struct CellReport {
    /// Inclusive cardinality span of the bin.
    pub m: (u32, u32),
    /// Inclusive run-count span of the bin.
    pub r: (u32, u32),
    pub agg: Aggregate,
    /// Convenience mirror of `agg.waste_bytes`, which orders [`Report::cells`].
    pub waste_bytes: f64,
}

impl CellReport {
    fn new(m: (u32, u32), r: (u32, u32), c: &Cell, total_stored: u64) -> CellReport {
        let agg = Aggregate::new(c, total_stored);
        CellReport {
            m,
            r,
            agg,
            waste_bytes: agg.waste_bytes,
        }
    }
}

/// A summarized histogram.
///
/// The totals are exact sums over per-chunk bounds; [`Self::cells`] only
/// distributes them for display.
#[derive(Clone, Debug)]
pub struct Report {
    /// Chunks that own a payload extent, and so appear in [`Report::cells`].
    pub chunks: u64,
    /// Chunks living inline in their index entry, owning no payload. **Not**
    /// binned: they have no container encoding to account for.
    pub inline_chunks: u64,
    /// `chunks + inline_chunks`.
    pub all_chunks: u64,
    /// A **floor** on index bytes: [`INDEX_ENTRY_FLOOR`] per chunk. Not an
    /// estimate — see that constant for what it deliberately does not model.
    pub index_floor_bytes: u64,
    /// Payload bytes only. Does **not** include [`Report::index_floor_bytes`],
    /// and the two must not be added into a "total storage" figure without
    /// saying that the index term is a lower bound.
    pub stored_bytes: u64,
    pub bound_bytes: f64,
    pub waste_bytes: f64,
    pub ratio: f64,
    /// What a plain binary trie would have cost over the same containers.
    /// Leading term only, so a lower bound.
    pub cbt_plain_bytes: f64,
    /// What a run-compressed binary trie would have cost. Leading term only, so
    /// a lower bound — not a predicted size.
    pub cbt_pruned_bytes: f64,
    /// The better of the two variants, chosen per chunk then summed.
    pub cbt_best_bytes: f64,
    /// `stored_bytes - cbt_best_bytes`, signed.
    pub cbt_saving_bytes: f64,
    /// What the corpus would cost with a trie available as a **fourth kind**,
    /// selected per chunk only where it beats the encoding already chosen.
    /// **This is the number the format decision turns on**, not
    /// [`Self::cbt_saving_frac`], which prices a wholesale replacement.
    pub cbt_selective_bytes: f64,
    /// `1 - cbt_selective_bytes / stored_bytes`. Never negative.
    pub cbt_selective_frac: f64,
    /// The whole of §13.9's sum: the saving as a fraction of stored bytes.
    /// Negative where the trie would cost more than what is stored today.
    pub cbt_saving_frac: f64,
    /// Occupied bins, most absolute waste first.
    pub cells: Vec<CellReport>,
    /// Everything at `r = 1`, rolled up.
    pub contiguous: Aggregate,
    /// Array, bitmap, run.
    pub kinds: [Aggregate; 3],
}

impl Report {
    /// How many of the leading cells it takes to cover `frac` of all waste.
    ///
    /// This is the number the instrument exists to produce: if a handful of
    /// cells hold most of the achievable saving, a targeted encoding is worth
    /// costing; if the waste is spread thin, no format change pays.
    pub fn cells_covering(&self, frac: f64) -> usize {
        if self.waste_bytes <= 0.0 {
            return 0;
        }
        let target = self.waste_bytes * frac;
        let mut acc = 0.0;
        for (i, c) in self.cells.iter().enumerate() {
            acc += c.waste_bytes;
            if acc >= target {
                return i + 1;
            }
        }
        self.cells.len()
    }

    /// The same cells, ordered by how much a run-compressed trie would save —
    /// largest saving first, largest *loss* last.
    ///
    /// A **second** ordering, deliberately not the default. [`Self::cells`]
    /// stays ordered by absolute waste against the counting bound, which is the
    /// encoding-independent question; this one answers a question about one
    /// candidate encoding and would be the wrong default the moment a different
    /// candidate is costed. Neither is ordered by ratio, for the reason the
    /// module note gives.
    pub fn cells_by_cbt_saving(&self) -> Vec<CellReport> {
        let mut v = self.cells.clone();
        v.sort_by(|a, b| {
            b.agg
                .cbt_saving_bytes
                .partial_cmp(&a.agg.cbt_saving_bytes)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        v
    }
}

fn kib(b: f64) -> f64 {
    b / 1024.0
}

fn show_ratio(r: f64) -> String {
    if r.is_finite() {
        format!("{r:.2}x")
    } else {
        "  inf".to_string()
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{} chunks, {:.1} KiB stored, {:.1} KiB bound, {:.1} KiB waste, {} overall",
            self.chunks,
            kib(self.stored_bytes as f64),
            kib(self.bound_bytes),
            kib(self.waste_bytes),
            show_ratio(self.ratio),
        )?;
        writeln!(
            f,
            "array {:.1}% / bitmap {:.1}% / run {:.1}% of stored bytes",
            self.kinds[0].byte_share * 100.0,
            self.kinds[1].byte_share * 100.0,
            self.kinds[2].byte_share * 100.0,
        )?;
        writeln!(
            f,
            "r = 1 holds {:.1}% of stored bytes and {:.1} KiB of waste \
             ( 4-6 B/chunk; a large mass here is the smallest prize, not the largest )",
            self.contiguous.byte_share * 100.0,
            kib(self.contiguous.waste_bytes),
        )?;
        writeln!(
            f,
            "{} of {} occupied cells hold 80% of the waste",
            self.cells_covering(0.80),
            self.cells.len(),
        )?;
        writeln!(
            f,
            "\n{:>13}  {:>13}  {:>9}  {:>10}  {:>7}  {:>10}  {:>7}",
            "m", "r", "chunks", "stored KiB", "ratio", "waste KiB", "% bytes"
        )?;
        for c in &self.cells {
            writeln!(
                f,
                "{:>6}..{:<5}  {:>6}..{:<5}  {:>9}  {:>10.1}  {:>7}  {:>10.1}  {:>6.1}%",
                c.m.0,
                c.m.1,
                c.r.0,
                c.r.1,
                c.agg.chunks,
                kib(c.agg.stored_bytes as f64),
                show_ratio(c.agg.ratio),
                kib(c.waste_bytes),
                c.agg.byte_share * 100.0,
            )?;
        }
        writeln!(
            f,
            "\nCBT ( leading term, a lower bound — not a predicted size ): \
             best-per-chunk {:.1} KiB against {:.1} KiB stored, {:+.1}% overall; \
             plain {:.1} KiB, run-compressed {:.1} KiB",
            kib(self.cbt_best_bytes),
            kib(self.stored_bytes as f64),
            self.cbt_saving_frac * 100.0,
            kib(self.cbt_plain_bytes),
            kib(self.cbt_pruned_bytes),
        )?;
        writeln!(
            f,
            "as a FOURTH KIND ( chosen per chunk only where it wins ): \
             {:.1} KiB, {:.1}% saved — this is the figure the format decision \
             turns on",
            kib(self.cbt_selective_bytes),
            self.cbt_selective_frac * 100.0,
        )?;
        writeln!(
            f,
            "{:>13}  {:>13}  {:>9}  {:>10}  {:>10}  {:>11}  {:>8}",
            "m", "r", "chunks", "stored KiB", "cbt KiB", "saving KiB", "% of all"
        )?;
        for c in &self.cells_by_cbt_saving() {
            writeln!(
                f,
                "{:>6}..{:<5}  {:>6}..{:<5}  {:>9}  {:>10.1}  {:>10.1}  {:>11.1}  {:>+7.1}%",
                c.m.0,
                c.m.1,
                c.r.0,
                c.r.1,
                c.agg.chunks,
                kib(c.agg.stored_bytes as f64),
                kib(c.agg.cbt_best_bytes),
                kib(c.agg.cbt_saving_bytes),
                c.agg.cbt_saving_share * 100.0,
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::{ArrayContainer, BitmapContainer, RunContainer};

    /// `sum_r N( m, r ) = C( n, m )` — Vandermonde. If the conditioned count is
    /// wrong, every ratio derived from it is wrong by an unknown factor, and
    /// nothing downstream would notice.
    #[test]
    fn conditioned_counts_sum_to_the_unconditioned_one() {
        for n in [4u32, 7, 12] {
            for m in 0..=n {
                let mut sum = 0.0f64;
                for r in 0..=n {
                    if let Some(bits) = bound_bits_n(n, m, r) {
                        sum += bits.exp2();
                    }
                }
                let want = log2_choose(n, m).exp2();
                assert!(
                    (sum - want).abs() < 1e-6 * want.max(1.0),
                    "n={n} m={m}: sum_r N = {sum}, C(n,m) = {want}"
                );
            }
        }
    }

    #[test]
    fn unrealizable_pairs_have_no_bound() {
        assert_eq!(bound_bits(0, 1), None); // no elements, but a run
        assert_eq!(bound_bits(5, 0), None); // elements, but no run
        assert_eq!(bound_bits(5, 6), None); // more runs than elements
                                            // 65535 elements cannot be split into two runs: only one gap exists.
        assert_eq!(bound_bits(CHUNK_CARD - 1, 3), None);
        assert!(bound_bits(CHUNK_CARD - 1, 2).is_some());
    }

    /// A single-element chunk is the one place Roaring is exactly optimal: two
    /// stored bytes against `log2 65536 = 16` bits. If the bound were off by a
    /// constant factor this is where it would be visible without arithmetic.
    #[test]
    fn a_single_element_chunk_sits_exactly_on_the_bound() {
        let bits = bound_bits(1, 1).unwrap();
        assert!((bits - 16.0).abs() < 1e-9, "{bits}");
        assert_eq!(crate::array_bytes(1), 2);
    }

    /// The contiguous-interval corner: 6 stored bytes against `log2( n-m+1 )`.
    /// Worse than the middle-band peak at every density, and worth exactly four
    /// bytes.
    #[test]
    fn the_contiguous_corner_reproduces() {
        for (m, want) in [(256u32, 3.0f64), (32768, 3.2), (61440, 4.0)] {
            let bits = bound_bits(m, 1).unwrap();
            let got = crate::run_bytes(1) as f64 * 8.0 / bits;
            assert!((got - want).abs() < 0.05, "m={m}: {got} vs {want}");
            // Four to six bytes, never more: the bound at `r = 1` is
            // `log2( n-m+1 )`, which is 16 bits at `m = 1` and falls to 0 at a
            // full chunk. "Four bytes" is only the sparse end of that.
            let waste = crate::run_bytes(1) as f64 - bits / 8.0;
            assert!((4.0..=6.0).contains(&waste), "m={m}: waste {waste} B");
        }
    }

    /// The tail cell the module warns about, pinned so nobody rediscovers it
    /// and reports it as a finding.
    #[test]
    fn the_alternating_chunk_is_the_absurd_cell_and_is_measure_negligible() {
        let half = CHUNK_CARD / 2;
        let bits = bound_bits(half, half).unwrap();
        // Told both, there are exactly C( 32769, 32768 ) = 32769 such sets.
        assert!((bits - 32769f64.log2()).abs() < 1e-6, "{bits}");
        let r = crate::BITMAP_BYTES as f64 * 8.0 / bits;
        assert!((r - 4369.0).abs() < 1.0, "{r}");
    }

    /// A full chunk has a zero bound, so its ratio is infinite and all six of
    /// its stored bytes are waste. The report must survive the division, which
    /// is the concrete reason cells are ordered by waste rather than ratio.
    #[test]
    fn a_full_chunk_has_an_infinite_ratio_and_is_entirely_waste() {
        assert_eq!(bound_bits(CHUNK_CARD, 1), Some(0.0));
        let mut h = MrHistogram::new();
        h.observe(&Container::Run(RunContainer::from_pairs(&[(0, 65535)])));
        let rep = h.report();
        assert!(rep.ratio.is_infinite());
        assert!((rep.waste_bytes - 6.0).abs() < 1e-9, "{}", rep.waste_bytes);
        assert_eq!(rep.cells.len(), 1);
        assert!(format!("{rep}").contains("inf"));
    }

    #[test]
    fn array_max_and_the_full_chunk_each_get_their_own_bin() {
        let lo = |m| span_of(&M_LO, bin_of(&M_LO, m), CHUNK_CARD);
        assert_eq!(lo(crate::ARRAY_MAX as u32), (4096, 4096));
        assert_ne!(lo(4097), (4096, 4096));
        assert_eq!(lo(CHUNK_CARD), (65536, 65536));
        assert_ne!(lo(CHUNK_CARD - 1), (65536, 65536));
        assert_eq!(span_of(&R_LO, bin_of(&R_LO, 1), CHUNK_CARD / 2), (1, 1));
        assert_ne!(span_of(&R_LO, bin_of(&R_LO, 2), CHUNK_CARD / 2), (1, 1));
    }

    /// The totals must be the sum of per-chunk bounds, not of bin
    /// representatives. Two containers land in the same bin with different
    /// `( m, r )`; if binning happened before the bound was evaluated, the
    /// total would take one of them twice.
    #[test]
    fn the_totals_are_exact_and_do_not_depend_on_bin_edges() {
        let a = Container::Array(ArrayContainer::from_sorted_vec((0..300u16).collect()));
        let b = Container::Array(ArrayContainer::from_sorted_vec(
            (0..600u16).step_by(2).collect(),
        ));
        assert_eq!(a.len(), 300);
        assert_eq!(b.len(), 300);
        assert_eq!(a.run_count(), 1);
        assert_eq!(b.run_count(), 300);
        assert_eq!(bin_of(&M_LO, a.len()), bin_of(&M_LO, b.len()));

        let mut h = MrHistogram::new();
        h.observe(&a);
        h.observe(&b);
        let want = (bound_bits(300, 1).unwrap() + bound_bits(300, 300).unwrap()) / 8.0;
        assert!(
            (h.report().bound_bytes - want).abs() < 1e-9,
            "{} vs {want}",
            h.report().bound_bytes
        );
    }

    #[test]
    fn cells_are_ordered_by_absolute_waste_not_by_ratio() {
        let mut h = MrHistogram::new();
        // One bitmap: 8192 B stored, a few hundred bytes of bound. Large waste,
        // modest ratio.
        let mut bm = BitmapContainer::zeroed();
        for v in (0..65536u32).step_by(7) {
            bm.insert(v as u16);
        }
        h.observe(&Container::Bitmap(bm));
        // Many contiguous chunks: tiny waste each, ratio 3-4x.
        for _ in 0..64 {
            h.observe(&Container::Run(RunContainer::from_pairs(&[(0, 255)])));
        }
        let rep = h.report();
        assert!(rep.cells.len() >= 2);
        assert!(
            rep.cells[0].waste_bytes >= rep.cells[1].waste_bytes,
            "not waste-ordered"
        );
        assert!(
            rep.cells[0].agg.ratio < rep.cells.last().unwrap().agg.ratio,
            "the widest-ratio cell must not lead"
        );
        assert_eq!(rep.contiguous.chunks, 64);
    }

    /// The rendered table must be in the units its header claims.
    ///
    /// Every other test here asserts on [`Report`]'s fields, so none of them
    /// can see the rendering — and the first run of the three-shape corpus
    /// printed the waste column in bytes under a `KiB` heading. With exactly
    /// one occupied cell the header total and the cell's figure are the same
    /// quantity, so a unit slip in either makes them disagree.
    #[test]
    fn the_rendered_table_is_in_the_units_its_header_claims() {
        let mut bm = BitmapContainer::zeroed();
        for v in (0..65536u32).step_by(3) {
            bm.insert(v as u16);
        }
        let mut h = MrHistogram::new();
        h.observe(&Container::Bitmap(bm));
        let rep = h.report();
        assert_eq!(rep.cells.len(), 1);

        let text = format!("{rep}");
        let shown = |line: &str, want: f64| {
            let hit = format!("{want:.1}");
            assert!(text.contains(&hit), "{line}: {hit} missing from\n{text}");
        };
        shown("header waste", kib(rep.waste_bytes));
        shown("cell waste", kib(rep.cells[0].waste_bytes));
        shown("stored", kib(rep.stored_bytes as f64));
        // The byte figure must not appear where a KiB figure was promised.
        //
        // **The negative check is the load-bearing half, and it has to cover
        // every column.** The positive `contains` above runs over the whole
        // rendering, so with one occupied cell it is satisfied by the header
        // even when the *row* prints raw bytes, and vice versa — sabotaging the
        // row's `stored` column passed until `stored` was added here. Redundant
        // columns are exactly what makes a whole-text `contains` vacuous; see
        // `the_cbt_table_is_in_the_units_its_header_claims`, which pays for the
        // same lesson at greater length.
        for (what, raw) in [
            ("waste", rep.waste_bytes),
            ("stored", rep.stored_bytes as f64),
            ("cell waste", rep.cells[0].waste_bytes),
        ] {
            assert!(
                !text.contains(&format!("{raw:.1}")),
                "{what} rendered in bytes under a KiB heading:\n{text}"
            );
        }
    }

    #[test]
    fn merging_shards_equals_observing_them_together() {
        let mk = |off: u16| {
            Container::Array(ArrayContainer::from_sorted_vec(
                (0..100u16).map(|v| v * 3 + off).collect(),
            ))
        };
        let (mut one, mut a, mut b) = (MrHistogram::new(), MrHistogram::new(), MrHistogram::new());
        one.observe(&mk(0));
        one.observe(&mk(1));
        a.observe(&mk(0));
        b.observe(&mk(1));
        a.merge(&b);
        let (x, y) = (one.report(), a.report());
        assert_eq!(x.chunks, y.chunks);
        assert_eq!(x.stored_bytes, y.stored_bytes);
        assert!((x.bound_bytes - y.bound_bytes).abs() < 1e-9);
        assert!((x.cbt_plain_bytes - y.cbt_plain_bytes).abs() < 1e-9);
        assert!((x.cbt_pruned_bytes - y.cbt_pruned_bytes).abs() < 1e-9);
        assert!((x.cbt_best_bytes - y.cbt_best_bytes).abs() < 1e-9);
    }

    #[test]
    fn an_empty_container_is_not_accounted_for() {
        let mut h = MrHistogram::new();
        h.observe(&Container::Array(ArrayContainer::new()));
        assert_eq!(h.chunks(), 0);
        assert_eq!(h.report().cells.len(), 0);
        assert_eq!(h.report().cells_covering(0.8), 0);
    }

    /// Every container the crate can store must land in a bin with a realizable
    /// bound. The `debug_assert` in `observe` would fire otherwise, so this is
    /// the boundary sweep that gives it teeth.
    #[test]
    fn every_storable_cardinality_has_a_bound() {
        for m in [
            1u32, 2, 3, 3583, 3584, 4095, 4096, 4097, 32768, 65535, CHUNK_CARD,
        ] {
            for r in [1u32, 2, m / 2, m] {
                if r == 0 || r > m || r > CHUNK_CARD - m + 1 {
                    continue;
                }
                assert!(bound_bits(m, r).is_some(), "m={m} r={r}");
            }
        }
    }

    /// A three-shape corpus — appended ranges, scattered ids, dense chunks —
    /// run end to end through `optimize()` so the encodings are the ones that
    /// would actually be stored.
    ///
    /// The assertions are the two readings the instrument exists to support,
    /// and they point opposite ways: the contiguous band holds a large share of
    /// *chunks* and almost none of the *waste*, while the dense band is the
    /// reverse. A ratio table would rank them the other way round.
    ///
    /// `cargo test -p yesno-core --lib stats::tests::a_three_shape -- --nocapture`
    /// prints the report.
    #[test]
    fn a_three_shape_corpus_separates_where_the_bytes_are_from_where_the_ratio_is() {
        let mut vals: Vec<u64> = Vec::new();
        // Appended ranges: one contiguous interval per chunk.
        for c in 0..200u64 {
            vals.extend((c << 16)..((c << 16) + 4_000));
        }
        // Scattered ids: sparse arrays, one run per element.
        for c in 200..400u64 {
            vals.extend((0..600u64).map(|i| (c << 16) | (i * 101)));
        }
        // Dense chunks with structure too fine for runs: bitmaps.
        for c in 400..430u64 {
            vals.extend((0..65536u64).step_by(3).map(|v| (c << 16) | v));
        }
        let mut set = crate::OrdSet::from_sorted_slice(&vals);
        set.optimize();

        let mut h = MrHistogram::new();
        h.observe_set(&set);
        let rep = h.report();
        println!("{rep}");

        assert_eq!(rep.chunks, 430);
        // Contiguous chunks are 200 of 430 and carry essentially no waste.
        assert_eq!(rep.contiguous.chunks, 200);
        assert!(
            rep.contiguous.waste_bytes / rep.waste_bytes < 0.01,
            "r = 1 holds {:.1}% of the waste",
            100.0 * rep.contiguous.waste_bytes / rep.waste_bytes
        );
        // ... yet its ratio is the widest finite one in the table.
        let finite_max = rep
            .cells
            .iter()
            .filter(|c| c.agg.ratio.is_finite())
            .fold(0.0f64, |a, c| a.max(c.agg.ratio));
        assert!(
            rep.contiguous.ratio >= finite_max - 1e-9,
            "contiguous ratio {} is not the widest ({finite_max})",
            rep.contiguous.ratio
        );
        // ... and the widest-ratio band does not lead the table. Which cell
        // *does* lead is a property of the corpus, not of the structure: here
        // the sparse arrays edge out the bitmaps ( 114 KiB of waste against
        // 80 KiB ) despite the narrower ratio, purely on chunk count.
        assert_ne!(rep.cells[0].r, (1, 1), "the widest-ratio cell leads");
        assert!(rep.cells_covering(0.80) <= 2);
    }

    #[test]
    fn observing_a_set_walks_every_chunk() {
        let mut set = crate::OrdSet::new();
        for c in 0..5u64 {
            for v in 0..10u64 {
                set.insert((c << 16) | v);
            }
        }
        let mut h = MrHistogram::new();
        h.observe_set(&set);
        assert_eq!(h.chunks(), 5);
        assert_eq!(h.report().chunks, 5);
    }

    /// The index floor is derived from the format, not written down.
    #[test]
    fn the_index_entry_floor_is_the_narrowest_leaf_entry() {
        assert_eq!(
            INDEX_ENTRY_FLOOR, 10,
            "2-byte suffix plus an 8-byte ChunkRef"
        );
        assert_eq!(crate::index::node::KSUF_WIDTHS[0], 2);
    }

    /// A chunk small enough to live in its index entry owns **no payload**, and
    /// must be counted without being binned.
    ///
    /// This is the case the whole module header is about. Before it was
    /// handled, `observe` took `Container::payload_bytes` at face value and
    /// credited such a chunk `2m` bytes — bytes stored nowhere — while the
    /// 8-byte `ChunkRef` that really holds it went unmentioned. A corpus of
    /// 1-ordinal chunks is the extreme: **every** stored byte is index, and a
    /// payload-only instrument that binned them would report a tidy ratio
    /// against a bound for an encoding that does not exist.
    #[test]
    fn inline_chunks_are_counted_but_never_binned() {
        let mut h = MrHistogram::new();
        for m in 1..=crate::store::extent::INLINE_MAX as u16 {
            let vals: Vec<u16> = (0..m).collect();
            h.observe(&Container::from_sorted(&vals));
        }
        let r = h.report();
        assert_eq!(r.inline_chunks, 3);
        assert_eq!(r.all_chunks, 3);
        assert_eq!(r.chunks, 0, "an inline chunk has no container encoding");
        assert_eq!(r.stored_bytes, 0, "and stores no payload byte");
        assert!(r.cells.is_empty(), "so it must not occupy a cell");
        assert_eq!(r.index_floor_bytes, 3 * INDEX_ENTRY_FLOOR);

        // One ordinal past the boundary and it is a real extent again.
        let vals: Vec<u16> = (0..=crate::store::extent::INLINE_MAX as u16).collect();
        let c = Container::from_sorted(&vals);
        assert!(!inline_eligible(&c), "m = 4 owns an extent");
        h.observe(&c);
        let r = h.report();
        assert_eq!(r.chunks, 1);
        assert_eq!(r.all_chunks, 4);
        assert_eq!(r.stored_bytes, crate::array_bytes(4) as u64);
        assert_eq!(r.index_floor_bytes, 4 * INDEX_ENTRY_FLOOR);
    }

    /// `inline_eligible` must agree with the checkpointer, which is the thing
    /// that actually decides. A bitmap is never inline however small it is.
    #[test]
    fn inline_eligibility_matches_the_checkpointer() {
        let small = Container::from_sorted(&[1u16, 2]);
        assert!(inline_eligible(&small));

        let mut bm = crate::container::BitmapContainer::from_sorted(&[1u16, 2]);
        bm.insert(3);
        bm.remove(3);
        let bm = Container::Bitmap(bm);
        assert_eq!(bm.len(), 2, "small enough by cardinality");
        assert!(
            !inline_eligible(&bm),
            "but a bitmap is never inline: checkpoint::inline_values excludes it"
        );
    }

    /// The `O( m )` XOR identity in [`plain_nodes`], against a direct count of
    /// the distinct prefixes at every level.
    ///
    /// **This is the only test that can see an error in that identity.**
    /// Everything downstream consumes the node count without being able to
    /// check it, so a wrong count would move the CBT column by a
    /// plausible-looking amount and nothing else in the suite would disagree —
    /// the same shape as a missing kernel arm, where a slow arm and a fast arm
    /// return the same value.
    #[test]
    fn the_trie_node_identity_matches_a_direct_level_walk() {
        fn direct(vals: &[u32]) -> u64 {
            let mut total = 1u64;
            for k in 1..=TRIE_DEPTH {
                let shift = TRIE_DEPTH - k;
                let mut seen = std::collections::BTreeSet::new();
                for &v in vals {
                    seen.insert(v >> shift);
                }
                total += seen.len() as u64;
            }
            total
        }
        let shapes: Vec<Vec<u32>> = vec![
            vec![0],
            vec![CHUNK_CARD - 1],
            vec![0, 1],
            vec![0, CHUNK_CARD - 1],
            (0..4096u32).collect(),
            (1000u32..1777).collect(),
            (0..CHUNK_CARD).step_by(2).collect(),
            (0..CHUNK_CARD).step_by(577).collect(),
            vec![0, 1, 2, 32768, 32769, CHUNK_CARD - 1],
        ];
        for vals in shapes {
            assert_eq!(
                plain_nodes(&vals),
                direct(&vals),
                "m = {}, first = {:?}",
                vals.len(),
                vals.first()
            );
        }
    }

    /// A full chunk is both extremes at once: the plain trie is the complete
    /// binary tree over the whole universe, and run compression takes it to a
    /// single node.
    #[test]
    fn a_full_chunk_is_a_complete_tree_plain_and_one_node_pruned() {
        let vals: Vec<u32> = (0..CHUNK_CARD).collect();
        let s = trie_shape_of_sorted(&vals);
        assert_eq!(s.nodes, (1u64 << (TRIE_DEPTH + 1)) - 1);
        assert_eq!(s.pruned_nodes, 1);
        assert_eq!(cbt_bits_pruned(s.pruned_nodes), 2.0);
        assert!(cbt_bits_plain(CHUNK_CARD, s.nodes) > 100_000.0);
    }

    /// A width-1 range is full, so a lone element prunes at its own leaf and
    /// nowhere above it.
    #[test]
    fn a_singleton_reaches_full_depth_and_prunes_to_nothing() {
        let s = trie_shape_of_sorted(&[12345]);
        assert_eq!(s.nodes, 1 + TRIE_DEPTH as u64);
        assert_eq!(s.pruned_nodes, s.nodes);
    }

    /// The smallest interesting pruning: root, one full child, nothing else.
    /// Shifting the same run off its dyadic alignment destroys that.
    #[test]
    fn an_aligned_dyadic_run_prunes_to_its_cover() {
        let aligned: Vec<u32> = (0..CHUNK_CARD / 2).collect();
        assert_eq!(trie_shape_of_sorted(&aligned).pruned_nodes, 2);
        let off: Vec<u32> = (1..=CHUNK_CARD / 2).collect();
        assert!(trie_shape_of_sorted(&off).pruned_nodes > 2);
    }

    /// The diagnostic the module note asks readers to check first.
    ///
    /// Where the two models separate, the trie's saving is coming from run
    /// compression — which [`crate::container::RunContainer`] already does — so
    /// a large gap here is a reason for suspicion, not celebration.
    #[test]
    fn the_two_models_separate_only_where_a_dyadic_subtree_is_full() {
        // Every other value: no range of width 2 or more is full, so nothing
        // prunes and the node counts agree exactly.
        let sparse: Vec<u32> = (0..CHUNK_CARD).step_by(2).collect();
        let s = trie_shape_of_sorted(&sparse);
        assert_eq!(s.pruned_nodes, s.nodes);

        // The *bits* still differ, by exactly the leaf level that the plain
        // model discounts and the pruned model deliberately does not.
        let m = sparse.len() as u32;
        let gap = cbt_bits_pruned(s.pruned_nodes) - cbt_bits_plain(m, s.nodes);
        assert!((gap - 2.0 * (m as f64 - 1.0)).abs() < 1e-9, "{gap}");

        // A contiguous run of the same cardinality prunes by three orders.
        let run: Vec<u32> = (0..m).collect();
        let r = trie_shape_of_sorted(&run);
        assert!(
            r.pruned_nodes * 1000 < s.pruned_nodes,
            "{} vs {}",
            r.pruned_nodes,
            s.pruned_nodes
        );
    }

    /// The per-cell savings are a decomposition of the reported total, so they
    /// must add back up — that is what makes §13.9's per-band sum meaningful
    /// rather than decorative.
    #[test]
    fn the_cell_savings_decompose_the_reported_total() {
        let mut h = MrHistogram::new();
        for k in 0..8u32 {
            h.observe(&Container::Array(ArrayContainer::from_sorted_vec(
                (0..300u32).map(|v| (v * 7 + k) as u16).collect(),
            )));
        }
        h.observe(&Container::Run(RunContainer::from_pairs(&[(0, 4095)])));
        let mut bm = BitmapContainer::zeroed();
        for v in (0..CHUNK_CARD).step_by(3) {
            bm.insert(v as u16);
        }
        h.observe(&Container::Bitmap(bm));
        let rep = h.report();

        let sum: f64 = rep.cells.iter().map(|c| c.agg.cbt_saving_bytes).sum();
        assert!(
            (sum - rep.cbt_saving_bytes).abs() < 1e-6,
            "cells {sum} vs total {}",
            rep.cbt_saving_bytes
        );
        let shares: f64 = rep.cells.iter().map(|c| c.agg.cbt_saving_share).sum();
        assert!((shares - rep.cbt_saving_frac).abs() < 1e-9, "{shares}");

        // A trie offered as a fourth *option* can never lose, because it is
        // only selected where it wins — so the selective column is bracketed by
        // the wholesale one below and by `stored` above. The two answer
        // different questions and the gap between them is large enough to
        // reverse the verdict, which is why both are reported.
        // Picking per chunk beats committing to either strategy wholesale:
        // `sum of min <= min of sums`, in both directions at once.
        assert!(rep.cbt_selective_bytes <= rep.stored_bytes as f64);
        assert!(rep.cbt_selective_bytes <= rep.cbt_best_bytes + 1e-9);
        assert!(rep.cbt_selective_frac >= 0.0);

        // The best-per-chunk column is a lower bound on both variants by
        // construction. It is *not* true that pruning is always cheaper:
        // pruning removes nodes but forfeits the plain trie's known-depth leaf
        // discount, so on a corpus with nothing to prune the run-compressed
        // model costs `2( m - 1 )` bits more. That is the whole reason this
        // column exists.
        assert!(rep.cbt_best_bytes <= rep.cbt_plain_bytes);
        assert!(rep.cbt_best_bytes <= rep.cbt_pruned_bytes);

        // The second ordering is by saving, descending — largest loss last.
        let by_saving = rep.cells_by_cbt_saving();
        assert_eq!(by_saving.len(), rep.cells.len());
        for w in by_saving.windows(2) {
            assert!(w[0].agg.cbt_saving_bytes >= w[1].agg.cbt_saving_bytes);
        }
    }

    /// The CBT table must be in the units its own header claims, for the reason
    /// [`the_rendered_table_is_in_the_units_its_header_claims`] gives: every
    /// other test here asserts on [`Report`]'s fields and none can see the
    /// rendering. One occupied cell makes the header total and the row the same
    /// quantity, so a unit slip in either makes them disagree.
    #[test]
    fn the_cbt_table_is_in_the_units_its_header_claims() {
        // **Two chunks, chosen so the three CBT totals are all different.**
        // With one chunk `cbt_best` is necessarily *equal* to whichever of
        // plain/pruned won, so the correct figure still appears in the header
        // via the neighbouring column and a `contains` check cannot fail. That
        // is not a hypothetical: the one-chunk version of this test passed
        // under sabotage twice. A sparse bitmap has nothing to prune so plain
        // wins it; a contiguous run prunes to five nodes so pruned wins that;
        // the per-chunk minimum is then strictly below both totals.
        let mut bm = BitmapContainer::zeroed();
        for v in (0..CHUNK_CARD).step_by(3) {
            bm.insert(v as u16);
        }
        let mut h = MrHistogram::new();
        h.observe(&Container::Bitmap(bm));
        h.observe(&Container::Run(RunContainer::from_pairs(&[(0, 4095)])));
        let rep = h.report();
        assert_eq!(rep.cells.len(), 2);
        assert!(
            rep.cbt_best_bytes < rep.cbt_plain_bytes && rep.cbt_best_bytes < rep.cbt_pruned_bytes,
            "best {} must be strictly below plain {} and pruned {}",
            rep.cbt_best_bytes,
            rep.cbt_plain_bytes,
            rep.cbt_pruned_bytes
        );

        let text = format!("{rep}");
        // Assert against the *specific* lines, never the whole rendering.
        // A `contains` over the full text passes when the header prints raw
        // bytes, because the identical KiB figure still appears in the one data
        // row below it — verified by sabotage, which is how this test was
        // found to be vacuous in its first form.
        let header = text
            .lines()
            .find(|l| l.starts_with("CBT ("))
            .expect("no CBT header line");
        for want in [
            format!("{:.1}", kib(rep.cbt_best_bytes)),
            format!("{:.1}", kib(rep.cbt_pruned_bytes)),
            format!("{:.1}", kib(rep.cbt_plain_bytes)),
            format!("{:+.1}%", rep.cbt_saving_frac * 100.0),
        ] {
            assert!(
                header.contains(&want),
                "{want} missing from header\n{header}"
            );
        }
        // The complement of the check above, and the one that survives columns
        // being redundant with each other: the *unconverted* figure must not
        // appear at all.
        for raw in [
            rep.cbt_best_bytes,
            rep.cbt_plain_bytes,
            rep.cbt_pruned_bytes,
        ] {
            let slip = format!("{raw:.1}");
            assert!(
                !header.contains(&slip),
                "header renders {slip} — bytes under a KiB heading\n{header}"
            );
        }

        // Rows are the trailing `cells.len()` lines, in saving order.
        let mut rows: Vec<&str> = text.lines().rev().take(rep.cells.len()).collect();
        rows.reverse();
        for (row, cell) in rows.iter().zip(rep.cells_by_cbt_saving()) {
            assert!(row.contains(".."), "not a data row: {row}");
            for want in [
                format!("{:.1}", kib(cell.agg.cbt_best_bytes)),
                format!("{:.1}", kib(cell.agg.cbt_saving_bytes)),
            ] {
                assert!(row.contains(&want), "{want} missing from row\n{row}");
            }
            let slip = format!("{:.1}", cell.agg.cbt_saving_bytes);
            assert!(
                !row.contains(&slip) || cell.agg.cbt_saving_bytes.abs() < 1024.0,
                "row renders {slip} — bytes under a KiB heading\n{row}"
            );
        }
        // And the sign is carried: this corpus is a bitmap of every third
        // ordinal, where a trie has nothing to prune and must lose.
        assert!(rep.cbt_saving_bytes < 0.0, "{}", rep.cbt_saving_bytes);
        assert!(
            text.contains('-'),
            "a loss must render as negative:\n{text}"
        );
    }
}
````
