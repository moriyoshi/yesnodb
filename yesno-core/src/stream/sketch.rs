//! Chunk-level prefix statistics, for estimating how much two operands overlap.
//!
//! # What the planner could not see without this
//!
//! [`crate::stream::plan`] costs `And( a, b )` as `min( yield_a, yield_b )` and
//! decides disjointness from `bounds()`, which is an **interval**. Two sets can
//! share an interval completely and still be disjoint chunk for chunk:
//!
//! ```text
//!   a = prefixes 0, 2, 4, ... 998      bounds = [0, 998·65536)
//!   b = prefixes 1, 3, 5, ... 999      bounds = [65536, 999·65536)
//! ```
//!
//! Those bounds overlap almost entirely, so `bounds()` reports "unknown" and the
//! planner keeps a full intersection that is provably empty. An interval cannot
//! express "occupies alternating chunks"; a sketch of the actual prefixes can.
//!
//! # Exact where it is affordable, estimated only where it is not
//!
//! The first version of this module used a **Bloom-style bitset** to *prove*
//! disjointness, reasoning that equal prefixes hash equal so a zero AND implies
//! no shared prefix. That implication is true and the design is still useless:
//! by the birthday bound, 500 prefixes in a 4096-bit filter collide on ~61 bits,
//! so the AND is essentially never zero and disjointness is essentially never
//! proved. Sizing it properly needs bits proportional to `n²`. The unit test
//! caught it immediately, which is the only reason it is not in the planner.
//!
//! What replaced it:
//!
//! - **`Set` against `Set` — exact.** Both operands carry a sorted prefix array,
//!   so a galloping merge answers exactly, with no false anything. It is
//!   strictly cheaper than executing the intersection it is costing: `u64`
//!   compares over the dense prefix array, no payload access, no container
//!   kernels. See [`exact_shared_prefixes`].
//! - **Anything composite — a bottom-`K` sketch.** A sub-expression has no
//!   materialized prefix array, so its statistics have to be summarized.
//!   `splitmix64` is a bijection on `u64`, so distinct prefixes never share a
//!   hash — which makes the sketch **exact**, not approximate, whenever it holds
//!   every prefix ( `n <= K` ). Above that it degrades to the usual KMV
//!   estimate.
//!
//! Disjointness is only ever *claimed* from an exact source. An estimate is used
//! to size work, never to prove a set empty.
//!
//! # Cost
//!
//! One pass over an operand's prefix array, which is `O(chunk_count)` of hashing
//! and no payload access whatsoever — the prefix array is the dense, cache-
//! friendly half of `OrdSet` precisely so that scans like this are cheap.
//! **Planning does not stay small relative to execution, and this is why.**
//! An earlier version of this paragraph claimed `plan` "memoizes per `plan()`
//! call". It does not — there is no cache anywhere in `plan.rs`, and every
//! fixpoint pass rebuilds these statistics at every node. Measured: `a AND b`
//! over two 1 000 000-chunk operands spends **42 ms planning a 64 ms query that
//! it does not rewrite at all**, and the cost falls off a cliff to 128 ns the
//! moment an operand crosses [`K`] — so a bigger query can
//! plan 300 000x faster than a smaller one.
//!
//! **Both figures predate [`STATS_MAX_CHUNKS`]. Re-measured 2026-09-14**, with
//! node count held at two leaves so only the chunk axis moves: plan cost is
//! linear below the cap at ~24 ns/chunk, reaching **99 us at 4096 chunks**, and
//! **flat at ~95 ns above it** out to 524 288 chunks. The transition is **one
//! chunk wide** — 99 325 ns at 4096, 95 ns at 4097, a **1045x** step.
//!
//! So the cap did not change the shape, it bounded where the shape stops. The
//! worst plan/exec ratio is **0.699**, at the cap, against the 0.66 the 42 ms /
//! 64 ms pair above represents — essentially unchanged. What fell is the
//! absolute worst case, 42 ms to 99 us, because the peak can no longer occur
//! past 4096. Planning never exceeds execution at any size measured.
//!
//! The old figures are kept for the shape they show, not as current numbers.
//! What remains open is the planner's own tree walk rather than these
//! statistics, plus the 1045x discontinuity itself; see
//! `planner-cost-is-o-chunks` in TODO.md.

/// Retained hashes in the bottom-`K` sketch. At or below this many prefixes the
/// sketch is exact, so the common case of a modest operand is not estimated at
/// all.
pub const K: usize = 256;

/// Operands larger than this get **no** `O(n)` statistics at all — not a sketch,
/// not an occupancy map, not a profile, not an exact prefix merge. The planner
/// falls back to `bounds()`, which is `O(1)`.
///
/// # Why there is a cut-off, and why this value
///
/// Every statistic in this module is `O(chunks)`, and the planner consults them
/// at every node on every fixpoint pass. Without a bound that is not a constant
/// factor — it is a tax proportional to the data, charged whether or not any
/// rewrite fires. Measured at the old value of `1 << 20`:
///
/// ```text
///   case                          plan @ 2^20   plan @ 4096
///   and / 1 000 000 chunks           42.7 ms        160 ns
///   16-leaf depth-4 tree              2.37 ms       3.46 µs
///   and / 500 000 chunks             20.4 ms        144 ns
///   and / 2 000 000 chunks            144 ns        144 ns
/// ```
///
/// The last two rows are the reason the old value was not merely too high but
/// the wrong *shape*: cost rose with size right up to the threshold and then
/// collapsed, so enlarging a query made it plan 140 000x faster. A threshold
/// only bounds cost if it is low enough to bind on real operands.
///
/// 4096 is where the sweep flattens. Below it nothing measured improves further
/// ( 1024 gave the same figures ); above it the deep-tree case returns, because
/// a threshold above the leaves' own size never fires.
///
/// This bounds cost **per operand**, not per query: a wide tree still pays
/// `nodes x passes` consults. Memoization is the complementary fix and is not
/// implemented; tracked as `planner-cost-is-o-chunks` in TODO.md.
pub const STATS_MAX_CHUNKS: u64 = 4096;

/// SplitMix64. A **bijection** on `u64`, which is the property that matters
/// here: distinct prefixes cannot collide, so a complete bottom-K sketch gives
/// exact answers rather than estimates.
#[inline]
pub fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut x = z;
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Exact count of prefixes present in both sets, by galloping merge.
///
/// Cheaper than the intersection it is costing — `u64` compares over the dense
/// prefix array, no payload access — and exact, so the planner may act on a zero
/// by rewriting to `Expr::Empty`.
pub fn exact_shared_prefixes(a: &crate::OrdSet, b: &crate::OrdSet) -> u64 {
    let (small, large) = if a.chunk_count() <= b.chunk_count() {
        (a, b)
    } else {
        (b, a)
    };
    let (mut shared, mut j) = (0u64, 0usize);
    let n = large.chunk_count();
    for i in 0..small.chunk_count() {
        let Some(p) = small.prefix_at(i) else {
            break;
        };
        // Gallop the larger side forward to the first prefix >= p.
        //
        // `partition_point_in` counts matches **within `[lo, hi)`**, so it
        // returns an offset relative to `j`, not an absolute index. Assigning it
        // directly makes the cursor drift backwards and silently undercount —
        // it reported 200 shared prefixes where the answer was 400.
        j += large.partition_point_in(j, n, p);
        if j >= n {
            break;
        }
        if large.prefix_at(j) == Some(p) {
            shared += 1;
            j += 1;
        }
    }
    shared
}

/// A summary of which `Prefix48`s a stream occupies.
#[derive(Clone, Debug)]
pub struct PrefixSketch {
    /// The `K` smallest hashes seen, ascending.
    kmv: Vec<u64>,
    /// `kmv` holds *every* prefix, so intersections computed from it are exact.
    complete: bool,
    /// Exact number of prefixes.
    n: u64,
}

impl PrefixSketch {
    pub fn build(prefixes: impl Iterator<Item = u64>) -> Self {
        // Collect, sort, truncate — `O(n log n)`.
        //
        // This was a sorted `Vec::insert` capped at `K`, which is `O(n·K)`:
        // 256 000 element moves for a 1 000-chunk operand. Combined with the
        // planner calling it once per cost evaluation per fixpoint pass, that
        // made **planning 192 ms for a query that executes in 37 µs**. A sketch
        // exists to make planning cheap; building it must not be the expensive
        // part of the plan.
        let mut hashes: Vec<u64> = prefixes.map(mix).collect();
        hashes.sort_unstable();
        hashes.dedup();
        let n = hashes.len() as u64;
        let complete = hashes.len() <= K;
        hashes.truncate(K);
        PrefixSketch {
            kmv: hashes,
            complete,
            n,
        }
    }

    /// Do the two provably share **no** prefix?
    ///
    /// Only ever answered from an exact source: an empty side, or two *complete*
    /// sketches ( `n <= K` each ), where the bijectivity of `mix` makes the hash
    /// comparison exact. A saturated sketch returns `false` meaning "unknown",
    /// never a guess — the planner turns this into `Expr::Empty`, so a false
    /// positive would silently delete results.
    pub fn provably_disjoint(&self, other: &PrefixSketch) -> bool {
        if self.n == 0 || other.n == 0 {
            return true;
        }
        if !(self.complete && other.complete) {
            return false;
        }
        self.kmv.iter().all(|h| other.kmv.binary_search(h).is_err())
    }

    /// Estimated number of prefixes present in both.
    ///
    /// Exact when both sketches are complete, because `mix` is a bijection.
    /// Otherwise the standard KMV estimate: intersect within the bottom-`K` of
    /// the union, and scale by the estimated union size.
    pub fn estimate_shared(&self, other: &PrefixSketch) -> u64 {
        if self.n == 0 || other.n == 0 {
            return 0;
        }
        if self.complete && other.complete {
            let mut shared = 0u64;
            let (mut i, mut j) = (0usize, 0usize);
            while i < self.kmv.len() && j < other.kmv.len() {
                match self.kmv[i].cmp(&other.kmv[j]) {
                    std::cmp::Ordering::Equal => {
                        shared += 1;
                        i += 1;
                        j += 1;
                    }
                    std::cmp::Ordering::Less => i += 1,
                    std::cmp::Ordering::Greater => j += 1,
                }
            }
            return shared;
        }

        // Bottom-K of the union, and how many of those are in both.
        let mut union: Vec<u64> = self.kmv.iter().chain(other.kmv.iter()).copied().collect();
        union.sort_unstable();
        union.dedup();
        let k = union.len().min(K);
        if k == 0 {
            return 0;
        }
        let sample = &union[..k];
        let both = sample
            .iter()
            .filter(|h| self.kmv.binary_search(h).is_ok() && other.kmv.binary_search(h).is_ok())
            .count() as u64;
        let jaccard = both as f64 / k as f64;
        // |A ∩ B| = J * |A ∪ B|, and |A ∪ B| = |A| + |B| - |A ∩ B|, so
        // |A ∩ B| = J * (|A| + |B|) / (1 + J).
        let est = jaccard * (self.n + other.n) as f64 / (1.0 + jaccard);
        (est.round() as u64).min(self.n.min(other.n))
    }

    /// Estimated prefixes present in either.
    pub fn estimate_union(&self, other: &PrefixSketch) -> u64 {
        (self.n + other.n).saturating_sub(self.estimate_shared(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sk(v: &[u64]) -> PrefixSketch {
        PrefixSketch::build(v.iter().copied())
    }

    #[test]
    fn mix_is_injective_on_the_values_we_hash() {
        // The exactness claim for complete sketches rests entirely on this.
        let mut seen = std::collections::HashSet::new();
        for p in (0..20_000u64).chain((1u64 << 47)..(1u64 << 47) + 20_000) {
            assert!(seen.insert(mix(p)), "mix collided at {p}");
        }
    }

    /// Complete sketches prove disjointness exactly; saturated ones must not
    /// claim it at all.
    #[test]
    fn disjointness_is_claimed_only_from_a_complete_sketch() {
        // The case bounds cannot see: interleaved prefixes, overlapping
        // intervals. Within K, this is exact.
        let a: Vec<u64> = (0..100).map(|i| i * 2).collect();
        let b: Vec<u64> = (0..100).map(|i| i * 2 + 1).collect();
        assert!(sk(&a).provably_disjoint(&sk(&b)));
        assert_eq!(sk(&a).estimate_shared(&sk(&b)), 0);

        // Overlapping, still within K: must not be called disjoint.
        let c: Vec<u64> = (0..100).collect();
        let d: Vec<u64> = (99..200).collect();
        assert!(!sk(&c).provably_disjoint(&sk(&d)));

        // Past K the sketch is a sample, so it must decline to prove anything,
        // even when the operands really are disjoint. Declining costs an
        // optimization; claiming would delete results.
        let big_a: Vec<u64> = (0..5000).map(|i| i * 2).collect();
        let big_b: Vec<u64> = (0..5000).map(|i| i * 2 + 1).collect();
        assert!(!sk(&big_a).provably_disjoint(&sk(&big_b)));
    }

    /// The exact path, which is what the planner uses for two `Set` leaves.
    #[test]
    fn exact_shared_prefixes_sees_what_bounds_cannot() {
        use crate::OrdSet;
        let mk = |it: Vec<u64>| OrdSet::from_sorted_slice(&it);
        // Interleaved chunks, deliberately far past K so no sketch could help.
        let a = mk((0..5000u64).map(|i| i * 2 * 65_536).collect());
        let b = mk((0..5000u64).map(|i| (i * 2 + 1) * 65_536).collect());
        assert_eq!(exact_shared_prefixes(&a, &b), 0);
        // Their intervals overlap almost entirely, which is the whole point.
        assert!(a.min().unwrap() < b.max().unwrap() && b.min().unwrap() < a.max().unwrap());

        let c = mk((0..1000u64).map(|i| i * 65_536).collect());
        let d = mk((600..1600u64).map(|i| i * 65_536).collect());
        assert_eq!(exact_shared_prefixes(&c, &d), 400);
        assert_eq!(exact_shared_prefixes(&d, &c), 400);
    }

    #[test]
    fn a_complete_sketch_is_exact() {
        let a: Vec<u64> = (0..100).collect();
        let b: Vec<u64> = (60..200).collect();
        assert_eq!(sk(&a).estimate_shared(&sk(&b)), 40);
        assert_eq!(sk(&a).estimate_union(&sk(&b)), 200);
    }

    /// Past `K` the sketch estimates. It must stay in the right neighbourhood,
    /// and it must never claim more overlap than the smaller side can hold.
    #[test]
    fn a_saturated_sketch_estimates_within_tolerance() {
        for (an, bn, shared) in [
            (4000u64, 4000u64, 2000u64),
            (10_000, 500, 500),
            (8000, 8000, 0),
        ] {
            let a: Vec<u64> = (0..an).collect();
            let b: Vec<u64> = (an - shared..an - shared + bn).collect();
            let (sa, sb) = (sk(&a), sk(&b));
            let est = sa.estimate_shared(&sb);
            assert!(est <= an.min(bn), "estimate {est} exceeds the smaller side");
            let err = (est as f64 - shared as f64).abs() / (shared.max(1) as f64);
            assert!(
                err < 0.5 || est.abs_diff(shared) < 200,
                "estimate {est} too far from {shared}"
            );
        }
    }

    #[test]
    fn an_empty_operand_is_disjoint_from_everything() {
        assert!(sk(&[]).provably_disjoint(&sk(&[1, 2, 3])));
        assert_eq!(sk(&[]).estimate_shared(&sk(&[1, 2, 3])), 0);
    }
}

/// Buckets in a [`PrefixOccupancy`]. Also the bound on how many segments
/// occupancy-driven refinement can produce, which is the point: see the type.
pub const OCCUPANCY_BUCKETS: u64 = 256;

/// Which coarse regions of the prefix domain an operand actually occupies.
///
/// # Why the bottom-K sketch cannot do this
///
/// [`PrefixSketch`] answers *how much* two operands overlap. Segmentation needs
/// *where*, and a bottom-K sketch cannot say: it holds hashes, and `mix` being a
/// bijection is exactly what destroys the ordering the question depends on.
/// Positional information needs positional buckets.
///
/// # Exact at its resolution, and safe below it
///
/// A bucket is `(prefix - base) >> shift`, so occupancy is **exact**: a bucket is
/// marked iff the operand really has a chunk in it. There are no hash collisions
/// to worry about, and therefore no false positives.
///
/// The resolution loss is one-directional in the safe way. Excluding an operand
/// from a segment requires that *no* overlapping bucket is marked, which means it
/// genuinely has no chunk there. Including one that contributes nothing merely
/// merges unnecessarily. So a coarse bucketing costs optimization, never
/// correctness — unlike disjointness, where a wrong answer deletes rows.
///
/// # The bucket count is a cost bound, not a precision knob
///
/// Refining to single-prefix resolution would separate two operands on
/// alternating chunks perfectly — into `2n` segments, each of which re-opens its
/// operands. That is far worse than the merge it replaces. Capping at
/// [`OCCUPANCY_BUCKETS`] bounds the segment count by construction, and it means
/// truly interleaved operands are reported as "both everywhere" and left alone,
/// which is the right answer for them. What this finds instead is **gaps**: an
/// operand whose span is wide but whose chunks are clustered, so that span-based
/// cutting credits it with regions it does not occupy at all.
#[derive(Clone, Debug)]
pub struct PrefixOccupancy {
    base: u64,
    shift: u32,
    bits: Vec<u64>,
}

/// A shared bucketing, so two operands' occupancies are comparable.
#[derive(Clone, Copy, Debug)]
pub struct Bucketing {
    pub base: u64,
    pub shift: u32,
}

impl Bucketing {
    /// Cover `[lo, hi]` in at most [`OCCUPANCY_BUCKETS`] buckets.
    pub fn covering(lo: u64, hi: u64) -> Bucketing {
        let width = hi.saturating_sub(lo).saturating_add(1);
        let mut shift = 0u32;
        while (width >> shift) > OCCUPANCY_BUCKETS {
            shift += 1;
        }
        Bucketing { base: lo, shift }
    }

    #[inline]
    pub fn bucket(&self, prefix: u64) -> u64 {
        prefix.saturating_sub(self.base) >> self.shift
    }
}

impl PrefixOccupancy {
    fn empty(b: Bucketing) -> Self {
        PrefixOccupancy {
            base: b.base,
            shift: b.shift,
            bits: vec![0u64; (OCCUPANCY_BUCKETS as usize + 2).div_ceil(64)],
        }
    }

    #[inline]
    fn set(&mut self, bucket: u64) {
        let i = bucket as usize;
        if i / 64 < self.bits.len() {
            self.bits[i / 64] |= 1u64 << (i % 64);
        }
    }

    pub fn from_prefixes(b: Bucketing, prefixes: impl Iterator<Item = u64>) -> Self {
        let mut o = Self::empty(b);
        for p in prefixes {
            let bk = b.bucket(p);
            o.set(bk);
        }
        o
    }

    /// Every bucket touching `[lo, hi]`, computed rather than enumerated — a
    /// range operand may span `2^48` prefixes.
    pub fn from_range(b: Bucketing, lo: u64, hi: u64) -> Self {
        let mut o = Self::empty(b);
        if hi < lo {
            return o;
        }
        let (first, last) = (b.bucket(lo), b.bucket(hi));
        for bk in first..=last.min(OCCUPANCY_BUCKETS + 1) {
            o.set(bk);
        }
        o
    }

    /// Does the operand have a chunk anywhere in `[lo, hi]`?
    ///
    /// Conservative upward: `true` may be a bucket that overlaps the window
    /// while the actual chunk sits outside it. `false` is exact, and `false` is
    /// the answer that drops a contributor.
    pub fn any_in(&self, lo: u64, hi: u64) -> bool {
        if hi < lo {
            return false;
        }
        let b = Bucketing {
            base: self.base,
            shift: self.shift,
        };
        let (first, last) = (b.bucket(lo), b.bucket(hi).min(OCCUPANCY_BUCKETS + 1));
        (first..=last).any(|i| {
            let i = i as usize;
            i / 64 < self.bits.len() && self.bits[i / 64] & (1u64 << (i % 64)) != 0
        })
    }

    /// Bucket indices where occupancy starts or stops, as prefix cut points.
    pub fn transitions(&self) -> Vec<u64> {
        let b = Bucketing {
            base: self.base,
            shift: self.shift,
        };
        let mut out = Vec::new();
        let mut prev = false;
        for i in 0..=OCCUPANCY_BUCKETS + 1 {
            let idx = i as usize;
            let cur = idx / 64 < self.bits.len() && self.bits[idx / 64] & (1u64 << (idx % 64)) != 0;
            if cur != prev {
                out.push(b.base.saturating_add(i << b.shift));
                prev = cur;
            }
        }
        out
    }
}

#[cfg(test)]
mod occupancy_tests {
    use super::*;

    #[test]
    fn occupancy_is_exact_at_bucket_resolution() {
        let b = Bucketing::covering(0, 1023);
        // 1024 prefixes over 256 buckets: 4 prefixes each.
        assert_eq!(b.shift, 2);
        let o = PrefixOccupancy::from_prefixes(b, [0u64, 5, 900].into_iter());
        assert!(o.any_in(0, 3));
        assert!(o.any_in(4, 7));
        // Nothing between; `false` here is the answer that drops a contributor,
        // so it must be exact at this resolution.
        assert!(!o.any_in(8, 800));
        assert!(o.any_in(900, 903));
    }

    #[test]
    fn a_range_fills_its_buckets_without_enumerating_them() {
        let b = Bucketing::covering(0, u64::MAX >> 16);
        let o = PrefixOccupancy::from_range(b, 0, 1 << 40);
        assert!(o.any_in(0, 0));
        assert!(!o.any_in(1u64 << 47, 1u64 << 47));
    }

    /// The gap case: a wide span whose chunks are clustered at both ends.
    #[test]
    fn occupancy_reveals_the_gap_a_span_hides() {
        let b = Bucketing::covering(0, 1000);
        let clustered = (0..100u64).chain(900..1000);
        let o = PrefixOccupancy::from_prefixes(b, clustered);
        assert!(o.any_in(0, 50));
        assert!(o.any_in(950, 1000));
        // The span says (0, 999); occupancy says nothing lives in the middle.
        assert!(!o.any_in(400, 500));
        assert!(!o.transitions().is_empty());
    }
}

/// What a run of chunks *is*, not merely whether it exists.
///
/// Occupancy answers "are there chunks here". That is not enough, because the
/// algebra depends on their character: a **1-filled** run is an identity for
/// `∩` and an annihilator for `∪`, and a **0-filled** run is the reverse. An
/// operand that is 1-filled over a region is a `Range` literal over that region,
/// and every rule that recognizes a range should recognize it too.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChunkClass {
    /// No chunks at all.
    Empty,
    /// Every chunk present and `is_full()`.
    Full,
    /// Chunks present, not all full. Claims nothing, so it is always the safe
    /// answer — coarsening merges into this.
    Present,
}

/// Run-length description of an operand's prefix domain by [`ChunkClass`].
///
/// Distinct from [`PrefixOccupancy`], which is bucketed and coarse. A profile is
/// **exact** and its size is the number of runs rather than a fixed budget — a
/// `Range` is three runs whatever its width ( partial, full, partial ), and a
/// uniformly sparse set is one. Runs are capped, and overflow coarsens to
/// [`ChunkClass::Present`], which loses optimization and never correctness.
#[derive(Clone, Debug)]
pub struct ChunkProfile {
    runs: Vec<(u64, u64, ChunkClass)>,
}

/// Beyond this many runs a profile stops being cheaper than the work it saves.
pub const MAX_PROFILE_RUNS: usize = 512;

impl ChunkProfile {
    /// Classify a materialized set, chunk by chunk.
    ///
    /// `is_full()` is a cached-length comparison, so this touches no payload.
    pub fn of_set(set: &crate::OrdSet) -> ChunkProfile {
        let mut runs: Vec<(u64, u64, ChunkClass)> = Vec::new();
        let mut push = |lo: u64, hi: u64, c: ChunkClass| match runs.last_mut() {
            Some(prev) if prev.2 == c && prev.1 + 1 == lo => prev.1 = hi,
            _ => runs.push((lo, hi, c)),
        };
        let mut prev_prefix: Option<u64> = None;
        for i in 0..set.chunk_count() {
            let (Some(p), Some((_, c))) = (set.prefix_at(i), set.chunk_at(i)) else {
                break;
            };
            if let Some(pp) = prev_prefix {
                if p > pp + 1 {
                    push(pp + 1, p - 1, ChunkClass::Empty);
                }
            }
            push(
                p,
                p,
                if c.is_full() {
                    ChunkClass::Full
                } else {
                    ChunkClass::Present
                },
            );
            prev_prefix = Some(p);
        }
        if runs.len() > MAX_PROFILE_RUNS {
            // Coarsen to a single conservative run rather than carry a profile
            // that costs more to consult than it saves.
            let (lo, hi) = (runs[0].0, runs[runs.len() - 1].1);
            runs = vec![(lo, hi, ChunkClass::Present)];
        }
        ChunkProfile { runs }
    }

    /// A range is full everywhere except possibly its two end chunks.
    ///
    /// Derived from the definition — chunk `p` is full iff the range covers
    /// all of `[p << 16, (p+1) << 16)` — rather than from "first and last are
    /// partial, the middle is full". That shortcut marked chunk 0 of
    /// `Range(0, 40)` as `Full`, because the head looked aligned and the tail
    /// arithmetic underflowed onto the same chunk. `covers` would then have
    /// claimed a 40-ordinal range contained a whole 65 536-ordinal chunk, which
    /// is an *unsound* absorption, not a missed one.
    pub fn of_range(lo: u64, hi: u64) -> ChunkProfile {
        // `[lo, hi)` in ordinals.
        if hi <= lo {
            return ChunkProfile { runs: Vec::new() };
        }
        let bits = crate::CHUNK_BITS;
        let (first, last) = (lo >> bits, (hi - 1) >> bits);
        // Smallest chunk whose start is at or after `lo`.
        // Saturating: `lo` may sit in the top chunk, where the round-up
        // overflows. Saturating to `u64::MAX` yields prefix `2^48 - 1`, which is
        // the correct ceiling — that chunk cannot be full anyway, since
        // `u64::MAX` is not an ordinal ( I8 ).
        let first_full = lo.saturating_add(crate::CHUNK_CARD as u64 - 1) >> bits;
        // Largest chunk whose end is at or before `hi`.
        let last_full = (hi >> bits).checked_sub(1);

        let mut runs = Vec::new();
        if first < first_full {
            runs.push((first, first, ChunkClass::Present));
        }
        if let Some(lf) = last_full {
            if first_full <= lf {
                runs.push((first_full, lf, ChunkClass::Full));
            }
        }
        let covered_to = runs.last().map(|(_, b, _)| *b);
        if covered_to.is_none_or(|b| b < last) {
            runs.push((last, last, ChunkClass::Present));
        }
        ChunkProfile { runs }
    }

    /// Is every chunk in `[lo, hi]` present and full?
    ///
    /// Conservative downward: `false` may mean "not known", and the caller then
    /// takes the general path. `true` is exact.
    pub fn is_full_over(&self, lo: u64, hi: u64) -> bool {
        if hi < lo {
            return true;
        }
        let mut at = lo;
        for (a, b, c) in &self.runs {
            if *b < at {
                continue;
            }
            if *a > at || *c != ChunkClass::Full {
                return false;
            }
            if *b >= hi {
                return true;
            }
            at = *b + 1;
        }
        false
    }

    /// Prefix boundaries where the class changes.
    pub fn transitions(&self) -> Vec<u64> {
        let mut out = Vec::with_capacity(self.runs.len() * 2);
        for (a, b, _) in &self.runs {
            out.push(*a);
            out.push(b.saturating_add(1));
        }
        out
    }

    pub fn runs(&self) -> &[(u64, u64, ChunkClass)] {
        &self.runs
    }
}

#[cfg(test)]
mod profile_tests {
    use super::*;
    use crate::OrdSet;

    fn full_chunks(lo: u64, hi: u64) -> OrdSet {
        let mut v = Vec::new();
        for p in lo..=hi {
            v.extend((p << 16)..((p + 1) << 16));
        }
        OrdSet::from_sorted_slice(&v)
    }

    /// Operand A from the design discussion: `[0,99]` 0-filled, `[100,199]`
    /// 1-filled.
    #[test]
    fn a_zero_filled_then_one_filled_operand_is_described_exactly() {
        let a = full_chunks(100, 199);
        let p = ChunkProfile::of_set(&a);
        assert_eq!(p.runs(), &[(100, 199, ChunkClass::Full)]);
        assert!(p.is_full_over(100, 199));
        assert!(p.is_full_over(150, 160));
        // The 0-filled region is not full, and neither is a straddling window.
        assert!(!p.is_full_over(0, 99));
        assert!(!p.is_full_over(99, 150));
        assert!(!p.is_full_over(150, 250));
    }

    #[test]
    fn a_mixed_operand_separates_full_from_merely_present() {
        // [0,9] full, [10,19] sparse, [20,29] full.
        let mut v = Vec::new();
        for p in 0..=9u64 {
            v.extend((p << 16)..((p + 1) << 16));
        }
        v.extend((10..=19u64).map(|p| p << 16));
        for p in 20..=29u64 {
            v.extend((p << 16)..((p + 1) << 16));
        }
        v.sort_unstable();
        v.dedup();
        let p = ChunkProfile::of_set(&OrdSet::from_sorted_slice(&v));
        assert_eq!(
            p.runs(),
            &[
                (0, 9, ChunkClass::Full),
                (10, 19, ChunkClass::Present),
                (20, 29, ChunkClass::Full),
            ]
        );
        assert!(p.is_full_over(0, 9));
        assert!(!p.is_full_over(0, 10));
        assert!(p.is_full_over(20, 29));
    }

    /// A range inside a single chunk contains no full chunk at all.
    ///
    /// The regression: this reported `[(0, 0, Full)]`, which would let `covers`
    /// conclude that 40 ordinals contain a whole chunk.
    #[test]
    fn a_sub_chunk_range_is_never_full() {
        for (lo, hi) in [(0u64, 40u64), (5, 40), (0, 65_535), (1, 65_536)] {
            let p = ChunkProfile::of_range(lo, hi);
            assert!(
                !p.is_full_over(0, 0),
                "Range({lo}, {hi}) claimed chunk 0 is full: {:?}",
                p.runs()
            );
        }
        // Exactly one whole chunk is full.
        assert!(ChunkProfile::of_range(0, 65_536).is_full_over(0, 0));
    }

    #[test]
    fn a_range_is_full_except_at_partial_ends() {
        // Chunk-aligned: entirely full.
        let p = ChunkProfile::of_range(0, 10 << 16);
        assert_eq!(p.runs(), &[(0, 9, ChunkClass::Full)]);
        assert!(p.is_full_over(0, 9));

        // Ragged both ends: partial, full, partial.
        let p = ChunkProfile::of_range((1 << 16) + 5, (4 << 16) + 5);
        assert_eq!(
            p.runs(),
            &[
                (1, 1, ChunkClass::Present),
                (2, 3, ChunkClass::Full),
                (4, 4, ChunkClass::Present),
            ]
        );
        assert!(!p.is_full_over(1, 4));
        assert!(p.is_full_over(2, 3));
    }

    #[test]
    fn an_empty_gap_is_never_full() {
        let mut v: Vec<u64> = ((0u64 << 16)..(1u64 << 16)).collect();
        v.extend((5u64 << 16)..(6u64 << 16));
        let p = ChunkProfile::of_set(&OrdSet::from_sorted_slice(&v));
        assert_eq!(
            p.runs(),
            &[
                (0, 0, ChunkClass::Full),
                (1, 4, ChunkClass::Empty),
                (5, 5, ChunkClass::Full),
            ]
        );
        assert!(!p.is_full_over(0, 5));
    }
}
